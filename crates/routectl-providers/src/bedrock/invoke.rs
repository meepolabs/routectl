//! InvokeModel adapter -- vendor-specific request body, with Anthropic
//! Messages JSON for Claude models.
//!
//! For Anthropic on Bedrock, the body shape is identical to the
//! Anthropic Messages API except:
//!   - `anthropic_version` is `"bedrock-2023-05-31"` (Bedrock-specific
//!     version string) and lives in the BODY, not in a header.
//!   - No auth headers (replaced by SigV4 signing on the HTTP layer).
//!   - Beta flags travel in the body's `anthropic_beta` array.
//!
//! We delegate body construction to `anthropic_api::request::normalize`
//! so the reasoning / cache_control / tool-use logic stays in one place,
//! then patch the result for Bedrock-specific fields.
//!
//! Response parsing is a direct call to `anthropic_api::response::normalize`
//! -- the Bedrock InvokeModel response shape for Anthropic models is
//! identical to the Anthropic Messages API response shape (which is
//! the whole point of the Invoke shape: pure passthrough).

use serde_json::Value;

use routectl_core::wire_check::{EditKind, EditSite};
use routectl_core::{ChatRequest, ChatResponse, Error, Result, sanitize_for_log};

use crate::anthropic_api::wire_edits;
use crate::managed_keys::{Dialect, is_managed_key};

use super::BedrockConfig;
use super::betas::{operator_floor, withhold_betas};

/// The Bedrock-required `anthropic_version` body field. Distinct from
/// the Anthropic API's `anthropic-version: 2023-06-01` header.
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Provider-kind discriminator string for this Bedrock shape, read back by
/// `BedrockApiShape::provider_kind_str`.
pub(crate) const PROVIDER_KIND: &str = routectl_core::provider_kind::trace::BEDROCK_INVOKE;

/// The lane spelling every translation counter call on this egress uses.
/// Bound to `PROVIDER_KIND` so the telemetry lane and the `provider_kind=`
/// tracing field can never drift apart. A lane of its own, never the
/// anthropic lane: the body is assembled by the shared Anthropic normalizer,
/// but crediting that lane would mix Bedrock's volume into its rates.
pub(crate) const LANE: &str = PROVIDER_KIND;

/// Build the InvokeModel request body from a routectl `ChatRequest`.
///
/// For Anthropic Claude models the body is Anthropic Messages JSON
/// patched with Bedrock-specific fields. For non-Anthropic Bedrock-hosted
/// vendors, prefer Converse (vendor-neutral) -- the InvokeModel adapter
/// here does not currently shape Mistral/Llama/Cohere bodies.
pub fn normalize_request(cfg: &BedrockConfig, req: &ChatRequest) -> Result<Value> {
    // The one point every Invoke request passes exactly once, on both the Ok
    // and the Err arm, ahead of the first fallible step: this lane's
    // request-volume DENOMINATOR. Exactly one call site per lane.
    crate::translation_drop_metrics::record_translation_lane_seen(LANE);
    let mut response_format =
        crate::anthropic_api::request::UnrecognizedResponseFormatTally::default();
    let mut foreign = crate::anthropic_api::request::ForeignDialectExtraTally::default();
    let assembled = crate::anthropic_api::request::normalize_deferring_format_key_warn(
        &cfg.id,
        req,
        cfg.adaptive_thinking.unwrap_or(false),
        // Bedrock does not emulate context_management beta; no cache.
        false,
        None,
        // Bedrock Invoke is never the genuine Anthropic host: this lane
        // egresses to a Bedrock endpoint, so a routectl reasoning
        // envelope in a `redacted_thinking` block rides through
        // byte-for-byte. Stated EXPLICITLY here rather than inherited, so
        // the passthrough cannot be flipped by a default changing
        // elsewhere.
        false,
        // Bedrock InvokeModel is not the Anthropic Messages API: support for
        // a mid-conversation `role: "system"` turn is not established on this
        // lane, so system turns stay lift-consumed here. Stated EXPLICITLY
        // for the same reason as the flag above -- a default changing
        // elsewhere must not start shipping the shape to Bedrock.
        false,
        // The fingerprint strip inside the shared assembly is real here too,
        // and this lane does not count it yet: the Invoke seam strips the
        // fingerprint again on the assembled body below, so a per-request
        // count needs those surfaces sharing one tally. Recording it against
        // the anthropic lane instead would credit that lane with this one's
        // volume, so the tally is driven and dropped.
        &mut crate::anthropic_api::request::ClientFingerprintStripTally::default(),
        // Same reason: the hosted-MCP tool withhold still happens on this
        // lane's body and is not counted on it yet.
        &mut crate::anthropic_api::request::HostedMcpToolWithholdTally::default(),
        &mut response_format,
        &mut foreign,
    );
    // Flushed on BOTH arms of the assembly, so a request that dropped its
    // directive and then failed is still in the numerator its denominator
    // already counted.
    flush_response_format_tally(&response_format);
    flush_foreign_dialect_tally(&cfg.id, &foreign);
    let (mut body, deferred) = assembled?;
    let obj = body.as_object_mut().ok_or_else(|| {
        Error::Internal(format!(
            "provider `{}`: anthropic_api::request assembly returned non-object",
            cfg.id
        ))
    })?;

    // Bedrock requires the body-side anthropic_version. Override
    // anything the upstream normalizer may have added.
    obj.insert(
        "anthropic_version".into(),
        Value::String(BEDROCK_ANTHROPIC_VERSION.into()),
    );
    declare_anthropic_version(req);

    // Merge the configured beta flags ahead of the client's, keeping only
    // the first occurrence of each flag. Runs even with no configured
    // flags, so a client list carrying a repeat is collapsed too. A flag
    // in both lists keeps its configured position, and the withhold below
    // spares every floor flag, so it is never removed.
    let merged = merge_floor_and_client_betas(&cfg.anthropic_beta, obj.get("anthropic_beta"));
    if !merged.is_empty() {
        obj.insert("anthropic_beta".into(), Value::Array(merged));
    }

    // Drop the request's withheld client flags (the router fills that set
    // from its seed and learned beta verdicts); every other flag reaches the
    // wire verbatim, and the operator floor is never withheld.
    withhold_client_betas(cfg, req, obj);

    // Merge any additional model request fields at the top level
    // with the same routectl-managed-keys allow-list as the Anthropic
    // and openai-compat egresses. The Bedrock-Invoke body is in
    // Anthropic shape (today); without this filter, a config-file
    // `additional_model_request_fields = { "messages" = [...] }`
    // would silently replace the assembled history. Filter here
    // (rather than at config load) so the WARN line carries the
    // provider id at request time and matches the operator's
    // existing log workflow.
    if let Some(extras) = cfg.additional_model_request_fields.as_ref()
        && let Some(extras_obj) = extras.as_object()
    {
        for (k, v) in extras_obj {
            if is_managed_key(Dialect::BedrockInvokeOperator, k) {
                tracing::warn!(
                    provider = %cfg.id,
                    key = %sanitize_for_log(k),
                    "additional_model_request_fields attempted to override \
                     routectl-managed key; dropped"
                );
                continue;
            }
            obj.insert(k.clone(), v.clone());
        }
    }

    // Stream flag is decided at HTTP level (different endpoint suffix),
    // not via a body field, so strip any leftover.
    let tap = wire_edits::tap_of(req);
    obj.remove("stream");
    wire_edits::declare_stream_strip(tap, req.stream);

    // Drop the Claude Code billing/attribution system block before the
    // body hits AWS. The body is in Anthropic wire shape (the shared
    // anthropic_api normalizer forwards `system` verbatim, which is
    // correct for the all-Anthropic path), but Bedrock is a third-party
    // upstream, so the client fingerprint the block carries must not
    // leave routectl here.
    strip_billing_system_field(&cfg.id, obj);

    // Drop the Anthropic `metadata` block before the body hits AWS. It
    // carries the client fingerprint (`user_id`, `account_uuid`) lifted
    // from the inbound request via the Anthropic ingress forward-compat
    // sweep (provider_extras -> body, merged inside the shared
    // normalizer above). Bedrock is always a third-party upstream, so
    // the CLIENT-path strip is unconditional. An operator who
    // deliberately set `metadata` via `additional_model_request_fields`
    // keeps it -- that is the operator's choice, not a client
    // fingerprint. Shared key with the Converse seam via
    // `super::CLIENT_FINGERPRINT_METADATA_KEY`.
    strip_client_metadata(obj, cfg.additional_model_request_fields.as_ref());

    // AWS Bedrock InvokeModel REJECTS a top-level `cache_control` body
    // field with HTTP 400 ("Extra inputs are not permitted") -- proven by
    // a live probe. The shared anthropic_api normalizer emits the
    // canonical `req.cache_control` as a top-level marker (correct for
    // Anthropic-direct, which honors it). Bedrock InvokeModel for
    // Anthropic instead honors a PER-BLOCK marker inside a content block,
    // where a single marker caches all prefixes up to ~20 blocks before
    // it. Lower the top-level marker to the last eligible block.
    let lowered = lower_top_level_cache_control_to_per_block(&cfg.id, obj)?;
    declare_cache_marker_lowering(tap, lowered);

    // Bedrock's InvokeModel takes the model identifier in the URL path
    // (`/model/{model_id}/invoke`), not the body. The Anthropic API
    // path requires it in the body, which `anthropic_api::request::
    // normalize` honors -- but Bedrock's strict-schema validator
    // rejects a body-side `model` with "Extra inputs are not
    // permitted". Strip it here on the Bedrock-Invoke seam.
    if obj.remove("model").is_some() {
        wire_edits::declare(tap, EditSite::UrlFieldRelocate, "/model", EditKind::Remove);
    }

    // Anthropic (and Invoke, carrying its body) rejects a tool_choice with
    // nothing to select. Remove an orphaned tool_choice on the body that
    // actually ships.
    super::body_fields::drop_orphan_tool_choice(
        &cfg.id,
        obj,
        super::body_fields::FilterContext::InvokeBody,
    );

    // Re-run both `output_config` passes the shared assembly already ran, on
    // the body that ACTUALLY ships. The `additional_model_request_fields`
    // merge above is a post-normalize write path that
    // the Invoke operator arm of `managed_keys::is_managed_key` does not cover
    // for `output_config`, so
    // an operator-supplied object can both reintroduce the `format` keys
    // Anthropic cannot represent and replace the whole repaired schema with
    // an unrepaired one (missing the mandatory `additionalProperties: false`).
    // Each diagnostic folds into ONE WARN: the shared pass deliberately
    // deferred its emission so this lane does not warn twice for a single
    // request.
    deferred.rescanning(&cfg.id, obj)?.warn(&cfg.id);

    // Capability union, LAST so it reads the body that actually ships: a
    // body carrying `output_config.format` gains the structured-outputs beta
    // in `anthropic_beta`. Retained as belt-and-braces, NOT a proven hard
    // requirement -- a 2026-08-11 live capture on api.anthropic.com (one
    // lane, one seat, one model) accepted the field both WITH and WITHOUT
    // the beta, refuting the older "rejected unless the flag rides along"
    // claim on that surface. Whether AWS rejects an ungated body is
    // UNMEASURED, and an older account/model tier may still gate the field,
    // so the union stays; revisit it if Anthropic retires the flag (whether
    // an unknown beta string is itself rejected is also unmeasured).
    // Deliberately AFTER the withhold, because the flag is a routectl-derived
    // capability signal implied by the shipped body, not a client-opted beta,
    // so a withheld entry naming it must not drop it.
    // Feature-triggered and idempotent: no `output_config.format` means no
    // flag, and an already-present flag is neither duplicated nor reordered.
    if let Some(obj) = body.as_object_mut() {
        super::betas::union_and_deposit_betas(cfg, req, super::BedrockApiShape::Invoke, obj);
        super::body_fields::drop_unrepresentable_body_fields(
            &cfg.id,
            obj,
            super::body_fields::FilterContext::InvokeBody,
        );
    }

    Ok(body)
}

/// Declare the body `anthropic_version` this seam writes: an Add when the
/// client sent none, a Rewrite when it sent another value.
fn declare_anthropic_version(req: &ChatRequest) {
    let client = req
        .provider_extras
        .as_ref()
        .and_then(|extras| extras.get("anthropic_version"));
    let kind = match client {
        None => EditKind::Add,
        Some(v) if v.as_str() == Some(BEDROCK_ANTHROPIC_VERSION) => return,
        Some(_) => EditKind::Rewrite,
    };
    wire_edits::declare(
        wire_edits::tap_of(req),
        EditSite::RequiredFieldDefault,
        "/anthropic_version",
        kind,
    );
}

/// Count this request's unrecognized `response_format` on the Invoke lane.
fn flush_response_format_tally(
    tally: &crate::anthropic_api::request::UnrecognizedResponseFormatTally,
) {
    if tally.dropped() {
        crate::translation_drop_metrics::record_translation_drop(
            LANE,
            "response_format_unrepresentable",
        );
    }
}

/// Count this request's foreign-dialect client extras on the Invoke lane.
/// The shared Anthropic assembly withheld them: the Invoke body is Anthropic
/// Messages JSON, which refuses another dialect's keys as extra inputs.
/// TRANSLATION-DROP: lane=bedrock-invoke class=foreign_dialect_extra test=foreign_dialect_extras_are_absent_from_every_anthropic_shape_egress_and_counted_once
fn flush_foreign_dialect_tally(
    provider_id: &str,
    tally: &crate::anthropic_api::request::ForeignDialectExtraTally,
) {
    if tally.dropped() {
        crate::translation_drop_metrics::record_translation_drop(LANE, "foreign_dialect_extra");
        tracing::debug!(
            provider = %provider_id,
            keys = %tally.keys_for_log(),
            "bedrock-invoke egress: client extras from another ingress dialect dropped",
        );
    }
}

/// Apply the shared Bedrock beta withhold to the body and count a withheld
/// client flag once per request, not once per flag.
fn withhold_client_betas(
    cfg: &BedrockConfig,
    req: &ChatRequest,
    obj: &mut serde_json::Map<String, Value>,
) {
    // The router put each withheld flag in the set because its seed or a
    // learned verdict says this lane's upstream rejects it, so the upstream
    // compels the loss.
    // TRANSLATION-DROP: lane=bedrock-invoke class=anthropic_beta_rejected_by_bedrock test=invoke_rejected_beta_withhold_bumps_the_drop_counter_once
    if withhold_betas(
        &cfg.id,
        obj,
        &req.routectl_internal.withheld_betas,
        &operator_floor(cfg, req),
    ) {
        crate::translation_drop_metrics::record_translation_drop(
            LANE,
            "anthropic_beta_rejected_by_bedrock",
        );
    }
}

/// Drop the Claude Code billing/attribution block from the assembled
/// Anthropic-shape `system` body field. Handles both wire shapes: a flat
/// string `system` and an array of `{type:"text", text}` blocks. Removes
/// the `system` key entirely when nothing survives. Reuses the canonical
/// predicate in `crate::system_filter` for the prefix match.
fn strip_billing_system_field(id: &str, obj: &mut serde_json::Map<String, Value>) {
    let Some(system) = obj.get("system") else {
        return;
    };
    // `Some(v)` -> replace `system` with `v`; `None` -> remove `system`.
    // We only reach the assignment when a billing block was found, so the
    // event fires exactly when the body actually changes.
    let replacement: Option<Value> = match system {
        Value::String(s) if crate::system_filter::is_billing_attribution_block(s) => None,
        Value::Array(blocks) => {
            let kept: Vec<Value> = blocks
                .iter()
                .filter(|b| {
                    let text = b.get("text").and_then(Value::as_str).unwrap_or("");
                    !crate::system_filter::is_billing_attribution_block(text)
                })
                .cloned()
                .collect();
            if kept.len() == blocks.len() {
                return;
            }
            if kept.is_empty() {
                None
            } else {
                Some(Value::Array(kept))
            }
        }
        // A normal string system, or a non-string/non-array shape: nothing
        // to strip.
        _ => return,
    };
    tracing::debug!(
        provider = id,
        "bedrock-invoke egress: Claude Code billing/attribution system block dropped",
    );
    match replacement {
        Some(v) => {
            obj.insert("system".into(), v);
        }
        None => {
            obj.remove("system");
        }
    }
}

/// Drop the client-fingerprint `metadata` block from the assembled
/// Bedrock-Invoke body. The block (carrying `user_id` / `account_uuid`)
/// rides in via the Anthropic ingress forward-compat sweep, so it is
/// always client-derived. An operator who deliberately set `metadata`
/// via `additional_model_request_fields` keeps it -- that config value
/// is restored after the strip. Shares the key name with the Converse
/// seam via `super::CLIENT_FINGERPRINT_METADATA_KEY`.
fn strip_client_metadata(
    obj: &mut serde_json::Map<String, Value>,
    operator_extras: Option<&Value>,
) {
    let key = super::CLIENT_FINGERPRINT_METADATA_KEY;
    let operator_metadata = operator_extras
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(key))
        .cloned();
    obj.remove(key);
    if let Some(v) = operator_metadata {
        obj.insert(key.to_string(), v);
    }
}

/// Lower a TOP-LEVEL `cache_control` body field to a PER-BLOCK marker on
/// the last cache_control-eligible content block. Bedrock Invoke rejects
/// a top-level `cache_control` with a 400, so the field never ships; the
/// per-block re-placement preserves the caller's caching intent.
///
/// The re-placement is computed on a CLONE of the messages array, the
/// resulting full breakpoint sequence is re-validated against the
/// canonical invariants, and only a valid arrangement is committed. The
/// pre-lowering validation in `anthropic_api::request::normalize` checked
/// a DIFFERENT arrangement (with the top-level marker, no per-block
/// insertion), so the lowered shape -- which can interact with an
/// already-marked forward-compat `Other` block the insertion scan skips
/// -- must be re-checked here before it ships.
///
/// Outcomes:
/// - `Ok(None)`: there was no top-level marker; `obj` is untouched.
/// - `Ok(Some(Lowered::Placed(_)))`: the top-level marker was removed and
///   lowered onto a block (or the last eligible block already carried one).
/// - `Ok(Some(Lowered::Dropped))`: the top-level marker was removed and
///   nothing could host it (no messages array, no eligible block). The
///   top-level form is broken on this wire, so re-emitting it is never
///   correct.
/// - `Err`: the lowered arrangement breaks a breakpoint rule. The caller's
///   top-level marker is restored first, so `obj` is left unmutated, and
///   the validator's error is returned rather than trimming the marker.
fn lower_top_level_cache_control_to_per_block(
    provider_id: &str,
    obj: &mut serde_json::Map<String, Value>,
) -> Result<Option<Lowered>> {
    // Removed up front; the refusal path below puts it back.
    let Some(cc) = obj.remove("cache_control") else {
        return Ok(None);
    };
    // `cc` stays in scope for the whole function, so borrow the ttl for
    // logging rather than allocating a String.
    let ttl = cc.get("ttl").and_then(Value::as_str).unwrap_or("");

    let Some(messages) = obj.get("messages").and_then(Value::as_array) else {
        tracing::warn!(
            provider = provider_id,
            "bedrock-invoke egress: top-level cache_control dropped (no messages array to host a per-block marker)"
        );
        return Ok(Some(Lowered::Dropped));
    };

    // Compute the insertion on a CANDIDATE clone; do NOT mutate the live
    // `obj["messages"]` until the result is validated.
    let mut candidate = messages.clone();
    let Some(placement) = insert_marker_on_candidate(&mut candidate, &cc) else {
        tracing::warn!(
            provider = provider_id,
            "bedrock-invoke egress: top-level cache_control dropped (no eligible content block to host a per-block marker)"
        );
        return Ok(Some(Lowered::Dropped));
    };

    // Re-validate the FULL breakpoint sequence across tools -> system ->
    // (candidate) messages before committing. This catches arrangements
    // the pre-lowering validator never saw -- e.g. inserting a 5m marker
    // ahead of an already-marked 1h forward-compat `Other` block.
    if let Err(e) = validate_lowered_breakpoints(obj, &candidate) {
        obj.insert("cache_control".into(), cc);
        return Err(e);
    }

    // Valid -> commit the candidate.
    obj.insert("messages".into(), Value::Array(candidate));
    match placement.kind {
        InsertKind::NewTextBlock => tracing::debug!(
            provider = provider_id,
            ttl = %sanitize_for_log(ttl),
            "bedrock-invoke egress: lowered top-level cache_control to a new trailing text block"
        ),
        InsertKind::ExistingBlock => tracing::debug!(
            provider = provider_id,
            ttl = %sanitize_for_log(ttl),
            "bedrock-invoke egress: lowered top-level cache_control to the last eligible content block"
        ),
        InsertKind::AlreadyMarked => tracing::debug!(
            provider = provider_id,
            ttl = %sanitize_for_log(ttl),
            "bedrock-invoke egress: top-level cache_control removed; last eligible block already marked"
        ),
    }
    Ok(Some(Lowered::Placed(placement)))
}

/// The outcome of a lowering that found a top-level marker.
enum Lowered {
    /// The marker was removed and placed nowhere.
    Dropped,
    /// The marker was removed and placed on a block.
    Placed(MarkerPlacement),
}

/// Where the lowering put the marker: the message, and the block within it.
#[derive(Clone, Copy)]
struct MarkerPlacement {
    kind: InsertKind,
    message: usize,
    block: usize,
}

/// Declare the lowering's edits: the top-level marker removed and, when it
/// landed, the per-block marker added (or the trailing string turn
/// rewritten into the block that carries it).
fn declare_cache_marker_lowering(
    tap: Option<&routectl_core::wire_check::WireTap>,
    lowered: Option<Lowered>,
) {
    let Some(lowered) = lowered else {
        return;
    };
    wire_edits::declare(
        tap,
        EditSite::CacheMarkerLowering,
        "/cache_control",
        EditKind::Remove,
    );
    let Lowered::Placed(placement) = lowered else {
        return;
    };
    let message = placement.message;
    match placement.kind {
        InsertKind::NewTextBlock => wire_edits::declare(
            tap,
            EditSite::CacheMarkerLowering,
            format!("/messages/{message}/content"),
            EditKind::Rewrite,
        ),
        InsertKind::ExistingBlock => wire_edits::declare(
            tap,
            EditSite::CacheMarkerLowering,
            format!(
                "/messages/{message}/content/{}/cache_control",
                placement.block
            ),
            EditKind::Add,
        ),
        InsertKind::AlreadyMarked => {}
    }
}

/// How the candidate placed the marker; drives the commit-time debug log.
#[derive(Clone, Copy)]
enum InsertKind {
    /// A trailing string-content message was converted to a one-element
    /// text block carrying the marker.
    NewTextBlock,
    /// The marker was inserted onto an existing eligible block.
    ExistingBlock,
    /// The last eligible block already carried a marker; nothing was added.
    AlreadyMarked,
}

/// Apply the per-block lowering to a CANDIDATE messages array in place.
/// Scans messages last -> first; within each, blocks last -> first. The
/// first eligible block found is the target. A trailing string-content
/// message converts to a single text block carrying the marker.
///
/// Returns `Some(placement)` describing where the marker went, or `None` if
/// there was no eligible target (caller drops the marker).
fn insert_marker_on_candidate(messages: &mut [Value], cc: &Value) -> Option<MarkerPlacement> {
    for (message, msg) in messages.iter_mut().enumerate().rev() {
        let Some(content) = msg.get_mut("content") else {
            continue;
        };

        if let Some(s) = content.as_str() {
            // Convert the trailing string turn to a one-element text block
            // carrying the marker. Realistic traffic (and the live probe)
            // sends a flat-string system + a string user message, so this
            // is the ONLY way to place a marker that caches the stable
            // system prefix; drop-only would cache nothing. The cacheable
            // prefix (tools / system / earlier messages) stays
            // byte-identical -- only this trailing turn is rewritten, and
            // deterministically so within a request.
            let text = s.to_owned();
            *content = Value::Array(vec![serde_json::json!({
                "type": "text",
                "text": text,
                "cache_control": cc.clone(),
            })]);
            return Some(MarkerPlacement {
                kind: InsertKind::NewTextBlock,
                message,
                block: 0,
            });
        }

        let Some(blocks) = content.as_array_mut() else {
            continue;
        };
        for (block, value) in blocks.iter_mut().enumerate().rev() {
            let Some(map) = value.as_object_mut() else {
                continue;
            };
            let is_eligible = map
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(is_cache_control_eligible_block_type);
            if !is_eligible {
                continue;
            }
            if map.contains_key("cache_control") {
                // Already caches at/after this point; adding would
                // duplicate and risk the breakpoint cap. Top-level
                // removal alone is the fix here.
                return Some(MarkerPlacement {
                    kind: InsertKind::AlreadyMarked,
                    message,
                    block,
                });
            }
            map.insert("cache_control".into(), cc.clone());
            return Some(MarkerPlacement {
                kind: InsertKind::ExistingBlock,
                message,
                block,
            });
        }
    }
    None
}

/// Re-validate the post-lowering breakpoint sequence against the canonical
/// invariants. Walks the `cache_control` markers in cache-prefix order
/// across tools -> system -> (candidate) messages, builds
/// `routectl_core::cache_control::Breakpoint` values, and delegates to
/// `routectl_core::cache_control::validate` -- the single source of truth
/// for the count cap (<= MAX_BREAKPOINTS) and the TTL-ordering rule (1h
/// before 5m). Markers are pulled from the assembled JSON body directly
/// because the lowering operates on JSON, not the typed request.
fn validate_lowered_breakpoints(
    obj: &serde_json::Map<String, Value>,
    candidate_messages: &[Value],
) -> Result<()> {
    use routectl_core::cache_control::{Breakpoint, BreakpointPosition, CacheControl, validate};

    fn parse_marker(v: &Value) -> Option<CacheControl> {
        serde_json::from_value::<CacheControl>(v.clone()).ok()
    }

    let mut owned: Vec<(BreakpointPosition, CacheControl)> = Vec::new();

    // Tools first in the cache prefix.
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        for t in tools {
            if let Some(cc) = t.get("cache_control").and_then(parse_marker) {
                owned.push((BreakpointPosition::Tools, cc));
            }
        }
    }

    // Then system blocks (array shape only carries per-block markers).
    if let Some(blocks) = obj.get("system").and_then(Value::as_array) {
        for b in blocks {
            if let Some(cc) = b.get("cache_control").and_then(parse_marker) {
                owned.push((BreakpointPosition::System, cc));
            }
        }
    }

    // Then the candidate messages, in order.
    for msg in candidate_messages {
        if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
            for b in blocks {
                if let Some(cc) = b.get("cache_control").and_then(parse_marker) {
                    owned.push((BreakpointPosition::Messages, cc));
                }
            }
        }
    }

    let bps: Vec<Breakpoint<'_>> = owned
        .iter()
        .map(|(position, control)| Breakpoint {
            position: *position,
            control,
        })
        .collect();
    validate(&bps)
}

/// The INSERTION-TARGET allow-list: the content block `type`s the
/// Bedrock-Invoke lowering will place a `cache_control` marker onto.
/// Forward-compat `Other` (unknown `type`) blocks and
/// `thinking` / `redacted_thinking` blocks are intentionally NEVER
/// insertion targets -- `Other` because its shape is opaque and
/// `thinking` / `redacted_thinking` because they are not valid cache
/// breakpoint targets. This predicate is deliberately NOT a mirror of the
/// sibling `anthropic_api::request::content_block_cache_control` walk
/// (which counts `Other` via a catch-all and does not list
/// `search_result` separately); that walk enumerates existing markers,
/// this list selects where to ADD one. Ordering safety against an
/// already-marked `Other` block is provided by the
/// clone -> validate -> refuse gate in
/// `lower_top_level_cache_control_to_per_block`, not by this predicate.
fn is_cache_control_eligible_block_type(block_type: &str) -> bool {
    matches!(
        block_type,
        "text" | "image" | "document" | "tool_use" | "tool_result" | "search_result"
    )
}

/// The configured `floor` flags, then each client flag from `client` (the
/// body's `anthropic_beta` array, if any) not already present, so every flag
/// appears once in floor-then-client order. Non-string client entries are
/// kept verbatim so the upstream reports them rather than routectl dropping
/// them silently.
fn merge_floor_and_client_betas(floor: &[String], client: Option<&Value>) -> Vec<Value> {
    let client = client
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    crate::beta_withhold::merge_unique(
        floor
            .iter()
            .map(|flag| Value::String(flag.clone()))
            .chain(client.iter().cloned()),
        Value::as_str,
    )
}

/// Parse the Bedrock InvokeModel response body into a `ChatResponse`.
///
/// For Anthropic Claude models this is exactly the Anthropic Messages
/// API response shape, so we delegate.
pub fn normalize_response(provider_id: &str, raw: Value) -> Result<ChatResponse> {
    crate::anthropic_api::response::normalize(provider_id, raw)
}

// A `tool_choice` with no tools on the wire never ships.
#[cfg(test)]
#[path = "invoke_orphan_tool_choice_tests.rs"]
mod orphan_tool_choice_tests;

// The floor and client beta lists merge with each flag sent once.
#[cfg(test)]
#[path = "invoke_beta_merge_tests.rs"]
mod beta_merge_tests;

// This lane's translation telemetry: its denominator and its drop counts.
#[cfg(test)]
#[path = "invoke_drop_counter_tests.rs"]
mod drop_counter_tests;

// Each body edit this seam makes is declared on the attempt's wire tap.
#[cfg(test)]
#[path = "invoke_wire_edits_tests.rs"]
mod wire_edits_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, RefusalReason, Role};
    use serde_json::json;

    use crate::bedrock::{BedrockApiShape, BedrockCreds};

    fn fake_cfg() -> BedrockConfig {
        BedrockConfig {
            id: "bedrock:test".into(),
            region: "us-west-2".into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShape::Invoke,
            creds: BedrockCreds::BearerKey { key: "test".into() },
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: vec!["context-1m-2025-08-07".into()],
            // `top_p` is a routectl-managed key the merge drops; `top_k`
            // is a long-tail Anthropic-only knob it forwards.
            additional_model_request_fields: Some(json!({"top_k": 40})),
            adaptive_thinking: None,
        }
    }

    fn user_req() -> ChatRequest {
        ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            max_tokens: Some(64),
            stream: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn body_includes_bedrock_anthropic_version_and_beta_flags() {
        let cfg = fake_cfg();
        let req = user_req();
        let body = normalize_request(&cfg, &req).unwrap();
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
        assert_eq!(body["anthropic_beta"], json!(["context-1m-2025-08-07"]),);
        assert_eq!(body["top_k"], json!(40));
        assert!(body.get("stream").is_none(), "stream should be stripped");
        assert!(
            body.get("model").is_none(),
            "model must be stripped: Bedrock takes it in the URL, not the body"
        );
    }

    /// Lane pin: Bedrock InvokeModel never ships a mid-conversation
    /// `role: "system"` turn. This lane passes `forward_system_turns:
    /// false`, so a canonical system present ALONGSIDE `Role::System`
    /// messages still leaves the messages array free of the wire role --
    /// a default flip on the anthropic-api side cannot leak the shape
    /// here.
    #[test]
    fn system_role_turns_stay_absent_from_the_invoke_body() {
        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.system = Some(routectl_core::SystemContent::Text("be brief".into()));
        req.messages = vec![
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::System,
                content: MessageContent::Text("mid-conversation note".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into();

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert_eq!(body["system"], json!("be brief"));
        let msgs = body["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 2, "got: {body}");
        assert!(
            msgs.iter().all(|m| m["role"] != json!("system")),
            "the Invoke lane must not ship a system-role turn: {body}"
        );
    }

    #[test]
    fn additional_model_request_fields_cannot_override_managed_keys() {
        // A misconfigured
        // `additional_model_request_fields = { "messages" = [...] }`
        // would silently replace the assembled history. Now blocked
        // with a WARN, matching the openai-compat / anthropic_api
        // egress filters.
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            // managed -- MUST be dropped:
            "messages": [{"role": "user", "content": "INJECTED"}],
            "system": "INJECTED",
            "anthropic_version": "evil-version",
            "max_tokens": 1,
            // long-tail -- MUST pass through:
            "metadata": {"user_id": "u-1"},
        }));
        let req = user_req();
        let body = normalize_request(&cfg, &req).unwrap();
        // Anthropic-version stays as the Bedrock-required value.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
        // Messages stays the user_req() text, NOT "INJECTED".
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_ne!(messages[0]["content"], "INJECTED");
        // System didn't get stomped.
        assert!(body.get("system").is_none() || body["system"] != "INJECTED");
        // max_tokens stays the request's value, not the override.
        assert_ne!(body["max_tokens"], 1);
        // Long-tail extras land verbatim.
        assert_eq!(body["metadata"]["user_id"], "u-1");
    }

    #[test]
    fn cache_control_on_user_text_round_trips_to_bedrock_invoke_body() {
        // Bedrock InvokeModel REJECTS a top-level cache_control body field
        // (HTTP 400), so the Bedrock-Invoke egress LOWERS the canonical
        // top-level marker to a per-block marker. Per-block markers the
        // caller already placed (here: the 1h system block and the 5m user
        // text block) are preserved byte-for-byte; the body must carry NO
        // top-level cache_control key after normalize.
        use routectl_core::{
            KnownContentPart, SystemBlock, cache_control::CacheControl, content_part::ContentPart,
            system_content::SystemContent,
        };

        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "be helpful".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            citations: None,
        }]));
        std::sync::Arc::make_mut(&mut req.messages)[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                citations: None,
                cache_control: Some(CacheControl::ephemeral_5m()),
            })]);

        let body = normalize_request(&cfg, &req).unwrap();

        // Top-level cache_control is lowered away (Bedrock 400s on it).
        assert!(
            body.get("cache_control").is_none(),
            "top-level cache_control must be removed for Bedrock-Invoke: {body}"
        );
        // System block preserved with its original cache_control.
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[0]["cache_control"]["ttl"], "1h");
        // User text block already carried its own 5m marker; it is the
        // last eligible block and already marked, so it stays 5m.
        let blk = &body["messages"][0]["content"][0];
        assert_eq!(blk["cache_control"]["ttl"], "5m");
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }

    /// Top-level 5m marker + a trailing Blocks-array user message whose
    /// last block carries no cache_control: the marker lowers onto that
    /// last block, and the top-level key is gone.
    #[test]
    fn top_level_cache_control_lowers_to_last_unmarked_block() {
        use routectl_core::{
            KnownContentPart, cache_control::CacheControl, content_part::ContentPart,
        };

        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        std::sync::Arc::make_mut(&mut req.messages)[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                citations: None,
                cache_control: None,
            })]);

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("cache_control").is_none(),
            "top-level cache_control must be removed: {body}"
        );
        let blk = &body["messages"][0]["content"][0];
        assert_eq!(blk["cache_control"]["type"], "ephemeral");
        assert_eq!(blk["cache_control"]["ttl"], "5m");
    }

    /// Top-level 5m marker + last user message content is a STRING: the
    /// string converts to a one-element text-block array carrying the 5m
    /// marker; the top-level key is gone.
    #[test]
    fn top_level_cache_control_lowers_onto_stringified_content() {
        use routectl_core::cache_control::CacheControl;

        // Arrange: user_req() builds MessageContent::Text("hello"), which
        // the shared normalizer emits as a JSON string `content`.
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("cache_control").is_none(),
            "top-level cache_control must be removed: {body}"
        );
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("string content converted to a block array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hello");
        assert_eq!(content[0]["cache_control"]["ttl"], "5m");
    }

    /// Top-level marker + the last eligible block already carries its own
    /// marker: the top-level key is removed, and the existing block marker
    /// is left UNCHANGED (no duplicate, no overwrite). The block's TTL is
    /// 1h and the top-level is 5m so the shared normalizer's prefix-order
    /// validation (longer TTLs before shorter) is satisfied before lowering
    /// runs.
    #[test]
    fn top_level_cache_control_does_not_override_existing_block_marker() {
        use routectl_core::{
            KnownContentPart, cache_control::CacheControl, content_part::ContentPart,
        };

        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        std::sync::Arc::make_mut(&mut req.messages)[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                citations: None,
                cache_control: Some(CacheControl::ephemeral_1h()),
            })]);

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("cache_control").is_none(),
            "top-level cache_control must be removed: {body}"
        );
        let blk = &body["messages"][0]["content"][0];
        assert_eq!(
            blk["cache_control"]["ttl"], "1h",
            "existing block marker must be left unchanged, not overwritten: {body}"
        );
    }

    /// No top-level cache_control, but a per-block marker on a system
    /// block: the body is unchanged -- the per-block marker survives and
    /// no top-level key is introduced.
    #[test]
    fn no_top_level_cache_control_leaves_per_block_marker_untouched() {
        use routectl_core::{
            SystemBlock, cache_control::CacheControl, system_content::SystemContent,
        };

        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = None;
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "be helpful".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            citations: None,
        }]));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("cache_control").is_none(),
            "no top-level cache_control must be introduced: {body}"
        );
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[0]["cache_control"]["ttl"], "1h");
    }

    /// Determinism: lowering is a pure function of the input body, so two
    /// identical requests must serialize to byte-identical bodies.
    #[test]
    fn lowering_is_deterministic_across_identical_requests() {
        use routectl_core::{
            KnownContentPart, SystemBlock, cache_control::CacheControl, content_part::ContentPart,
            system_content::SystemContent,
        };

        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "be helpful".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            citations: None,
        }]));
        std::sync::Arc::make_mut(&mut req.messages)[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                citations: None,
                cache_control: None,
            })]);

        // Act
        let first = normalize_request(&cfg, &req).unwrap().to_string();
        let second = normalize_request(&cfg, &req).unwrap().to_string();

        // Assert
        assert_eq!(first, second, "lowering must be byte-identical");
    }

    /// A lowering that would break a breakpoint rule refuses the request
    /// with the validator's reason and leaves the body as it came in. Rows:
    /// a top-level 5m lowered onto a text block ahead of an already-marked
    /// 1h forward-compat block (ttl order), and a top-level marker lowered
    /// onto the one unmarked block beside four marked ones (cap).
    #[test]
    fn lowering_that_breaks_a_breakpoint_rule_refuses_and_leaves_the_body_unmutated() {
        let mk = |ttl: &str| json!({"type": "ephemeral", "ttl": ttl});
        let rows = [
            (
                "5m lowered ahead of a 1h block",
                RefusalReason::CacheTtlOrder,
                json!([
                    {"type": "text", "text": "hi"},
                    {"type": "some_future_block", "cache_control": mk("1h")}
                ]),
            ),
            (
                "lowered marker is the fifth",
                RefusalReason::CacheBreakpointCap,
                json!([
                    {"type": "text", "text": "a", "cache_control": mk("5m")},
                    {"type": "text", "text": "b", "cache_control": mk("5m")},
                    {"type": "text", "text": "c", "cache_control": mk("5m")},
                    {"type": "text", "text": "d", "cache_control": mk("5m")},
                    {"type": "text", "text": "e"}
                ]),
            ),
        ];
        for (name, reason, content) in rows {
            // Arrange
            let mut obj = serde_json::Map::new();
            obj.insert("cache_control".into(), mk("5m"));
            obj.insert(
                "messages".into(),
                json!([{"role": "user", "content": content}]),
            );
            let before = obj.clone();

            // Act
            let result = lower_top_level_cache_control_to_per_block("bedrock:test", &mut obj);

            // Assert
            let err = result
                .err()
                .unwrap_or_else(|| panic!("{name}: must refuse"));
            assert_eq!(err.refusal_reason(), Some(reason), "{name}: {err}");
            assert_eq!(obj, before, "{name}: body must be left as it came in");
        }
    }

    /// No `messages` array to host a per-block marker: the top-level marker
    /// is dropped (drop-only via the no-messages warn path).
    #[test]
    fn top_level_cache_control_dropped_when_no_messages_array() {
        // Arrange
        let mut obj = serde_json::Map::new();
        obj.insert(
            "cache_control".into(),
            json!({"type": "ephemeral", "ttl": "5m"}),
        );

        // Act
        let lowered = lower_top_level_cache_control_to_per_block("bedrock:test", &mut obj);

        // Assert
        assert!(
            matches!(lowered, Ok(Some(Lowered::Dropped))),
            "the marker is dropped, not refused"
        );

        // Assert
        assert!(
            obj.get("cache_control").is_none(),
            "top-level cache_control must be removed even with no messages: {obj:?}"
        );
    }

    /// The only block is a `thinking` block (not an insertion target): the
    /// top-level marker is dropped and the thinking block is untouched.
    #[test]
    fn top_level_cache_control_dropped_when_no_eligible_block() {
        // Arrange
        let mut obj = serde_json::Map::new();
        obj.insert(
            "cache_control".into(),
            json!({"type": "ephemeral", "ttl": "5m"}),
        );
        obj.insert(
            "messages".into(),
            json!([{
                "role": "assistant",
                "content": [{"type": "thinking", "thinking": "hmm"}]
            }]),
        );

        // Act
        let lowered = lower_top_level_cache_control_to_per_block("bedrock:test", &mut obj);

        // Assert
        assert!(
            matches!(lowered, Ok(Some(Lowered::Dropped))),
            "the marker is dropped, not refused"
        );

        // Assert
        assert!(
            obj.get("cache_control").is_none(),
            "top-level cache_control must be removed: {obj:?}"
        );
        let block = &obj["messages"][0]["content"][0];
        assert!(
            block.get("cache_control").is_none(),
            "ineligible thinking block must not receive a marker: {obj:?}"
        );
    }

    /// Direct unit test of the post-lowering validator: a 1h-before-5m
    /// sequence is VALID, a 5m-before-1h sequence is INVALID, and a
    /// 5-marker sequence is INVALID (exceeds MAX_BREAKPOINTS).
    #[test]
    fn validate_lowered_breakpoints_enforces_count_and_ttl_order() {
        let mk = |ttl: &str| json!({"type": "ephemeral", "ttl": ttl});

        // VALID: 1h (system) then 5m (messages).
        let mut valid = serde_json::Map::new();
        valid.insert(
            "system".into(),
            json!([{"type": "text", "text": "s", "cache_control": mk("1h")}]),
        );
        let valid_msgs = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "u", "cache_control": mk("5m")}]
        })];
        assert!(validate_lowered_breakpoints(&valid, &valid_msgs).is_ok());

        // INVALID: 5m (system) then 1h (messages).
        let mut bad_order = serde_json::Map::new();
        bad_order.insert(
            "system".into(),
            json!([{"type": "text", "text": "s", "cache_control": mk("5m")}]),
        );
        let bad_order_msgs = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "u", "cache_control": mk("1h")}]
        })];
        assert!(validate_lowered_breakpoints(&bad_order, &bad_order_msgs).is_err());

        // INVALID: 5 markers exceeds MAX_BREAKPOINTS (4).
        let five = serde_json::Map::new();
        let five_msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "a", "cache_control": mk("5m")},
                {"type": "text", "text": "b", "cache_control": mk("5m")},
                {"type": "text", "text": "c", "cache_control": mk("5m")},
                {"type": "text", "text": "d", "cache_control": mk("5m")},
                {"type": "text", "text": "e", "cache_control": mk("5m")}
            ]
        })];
        assert!(validate_lowered_breakpoints(&five, &five_msgs).is_err());
    }
    /// through `normalize_request` -> `anthropic_api::request::normalize`
    /// and produces the Opus 4.7+ wire shape (no `budget_tokens`,
    /// top-level `output_config.effort`). This is the integration
    /// point that lets a Bedrock provider opt into adaptive thinking
    /// without touching the shared anthropic_api normalizer's
    /// signature beyond the `bool` flag.
    #[test]
    fn adaptive_thinking_propagates_from_bedrock_cfg() {
        use routectl_core::ReasoningConfig;
        let mut cfg = fake_cfg();
        cfg.adaptive_thinking = Some(true);

        let mut req = user_req();
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: Some(2000),
            exclude: None,
            enabled: Some(true),
        });
        let body = normalize_request(&cfg, &req).unwrap();

        // thinking is the adaptive shape (no budget_tokens) and the
        // effort moves to top-level output_config.
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }

    /// Structured output (`output_config.format`) flows through
    /// provider_extras -> Bedrock-Invoke body verbatim. Bedrock-Invoke
    /// for Claude is pure Anthropic-shape passthrough (only the
    /// `anthropic_version` body field differs from api.anthropic.com),
    /// so when the canonical request carries `output_config` in
    /// provider_extras (placed there by the Anthropic ingress, see
    /// `routectl_cli::ingress::anthropic::translate_request`), the
    /// Bedrock egress must emit the field unchanged. Bedrock-Invoke for
    /// Claude is pure Anthropic-shape passthrough; structured output
    /// (output_config.format) needs no extra translation on this seam.
    #[test]
    fn structured_output_format_passes_through_to_bedrock_invoke_body() {
        let cfg = fake_cfg();
        let mut req = user_req();
        req.provider_extras = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "properties": {"x": {"type": "integer"}}}
                }
            }
        }));
        let body = normalize_request(&cfg, &req).unwrap();
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
        // The gating beta rides along with the field it gates.
        let betas = body["anthropic_beta"]
            .as_array()
            .expect("a structured-output body must carry anthropic_beta");
        assert!(
            betas
                .iter()
                .any(|b| b.as_str()
                    == Some(routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA)),
            "output_config.format must never ship without its gating beta; got: {betas:?}"
        );
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }

    /// REGRESSION: a withheld entry naming the structured-outputs flag must
    /// not strip it off a body that carries `output_config.format`. The flag
    /// is a routectl-derived server requirement implied by the shipped field,
    /// so the union runs AFTER the withhold -- running it earlier lets the
    /// withhold drop the flag and ship the gated field ungated.
    #[test]
    fn structured_outputs_beta_survives_a_withheld_entry_naming_it() {
        let flag = routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA;
        let cfg = fake_cfg();
        assert!(
            !cfg.anthropic_beta.iter().any(|b| b == flag),
            "precondition: the operator floor must not supply the flag"
        );

        let mut req = user_req();
        req.anthropic_beta = vec![flag.into()];
        req.routectl_internal.withheld_betas = std::iter::once(flag.to_string()).collect();
        req.response_format = Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "widget", "schema": {"type": "object"}},
        }));

        let body = normalize_request(&cfg, &req).unwrap();
        assert!(
            body["output_config"].get("format").is_some(),
            "precondition: the structured-output directive must reach the body; got: {body}"
        );
        let betas: Vec<&str> = body["anthropic_beta"]
            .as_array()
            .expect("the gating beta must be on the final body")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(
            betas.iter().filter(|b| **b == flag).count(),
            1,
            "the flag must survive the withhold exactly once; got: {betas:?}"
        );
    }

    /// Client flags that no operator list or withheld set names reach the
    /// Invoke body verbatim and in order, after the operator floor.
    #[test]
    fn client_betas_outside_every_list_reach_the_invoke_body_verbatim() {
        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.anthropic_beta = vec![
            "oauth-2025-04-20".into(),
            "made-up-flag".into(),
            "interleaved-thinking-2025-05-14".into(),
        ];

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert_eq!(
            body["anthropic_beta"],
            json!([
                "context-1m-2025-08-07",
                "oauth-2025-04-20",
                "made-up-flag",
                "interleaved-thinking-2025-05-14"
            ])
        );
    }

    /// The Claude Code billing/attribution block must be dropped from the
    /// assembled Bedrock-Invoke body (Anthropic wire shape, Blocks form):
    /// AWS is a third-party upstream and must not receive the fingerprint.
    /// A normal sibling block survives.
    #[test]
    fn billing_block_dropped_from_invoke_body_keeps_normal_block() {
        use routectl_core::{SystemBlock, SystemContent};
        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "x-anthropic-billing-header: v=1; fp=secret".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "you are helpful".into(),
                cache_control: None,
                citations: None,
            },
        ]));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let sys = body["system"].as_array().expect("system survives as array");
        assert_eq!(sys.len(), 1, "only the normal block survives, got: {body}");
        assert_eq!(sys[0]["text"], "you are helpful");
    }

    /// A mid-string occurrence of the billing prefix in the Invoke body is
    /// a normal prompt and must be preserved.
    #[test]
    fn invoke_body_preserves_mid_string_billing_prefix() {
        use routectl_core::{SystemBlock, SystemContent};
        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "intro x-anthropic-billing-header: not at start".into(),
            cache_control: None,
            citations: None,
        }]));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let sys = body["system"].as_array().expect("system survives as array");
        assert_eq!(sys.len(), 1);
        assert_eq!(
            sys[0]["text"],
            "intro x-anthropic-billing-header: not at start"
        );
    }

    /// The Anthropic `metadata` block carries client identity
    /// (`user_id`, `account_uuid`) and must NOT reach AWS -- a
    /// third-party upstream. It arrives via `provider_extras` (the
    /// Anthropic ingress forward-compat sweep), merges into the body
    /// inside `anthropic_api::request::normalize`, and is then stripped
    /// unconditionally on the Bedrock-Invoke seam.
    #[test]
    fn client_metadata_fingerprint_stripped_from_invoke_body() {
        use serde_json::json;
        // Arrange: client supplies a metadata fingerprint via
        // provider_extras and sets req.user (the canonical mirror).
        let cfg = fake_cfg();
        let mut req = user_req();
        req.user = Some("u-1".into());
        req.provider_extras = Some(json!({
            "metadata": {"user_id": "u-1", "account_uuid": "a-2"}
        }));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: no metadata key, and no fingerprint substring anywhere.
        assert!(
            body.get("metadata").is_none(),
            "client metadata fingerprint leaked to Bedrock-Invoke body: {body}"
        );
        let serialized = body.to_string();
        assert!(
            !serialized.contains("u-1"),
            "user_id fingerprint leaked into Invoke body: {serialized}"
        );
        assert!(
            !serialized.contains("a-2"),
            "account_uuid fingerprint leaked into Invoke body: {serialized}"
        );
    }

    /// Mirror of the Converse `operator_metadata_survives_in_converse_bag`:
    /// operator-config `metadata` (set via
    /// `additional_model_request_fields`) SURVIVES the strip while a
    /// client `metadata` fingerprint (via `provider_extras`) is removed.
    /// Pins that `strip_client_metadata` restores from the OPERATOR
    /// CONFIG, not from the assembled body -- the security-critical
    /// property is that operator intent is sourced from cfg, never from
    /// the client-derived obj.
    #[test]
    fn operator_metadata_survives_while_client_metadata_stripped_from_invoke_body() {
        use serde_json::json;
        // Arrange: operator deliberately sets metadata via config; client
        // supplies its own metadata fingerprint via provider_extras.
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({"metadata": {"trace": "operator-set"}}));
        let mut req = user_req();
        req.user = Some("u-1".into());
        req.provider_extras = Some(json!({
            "metadata": {"user_id": "u-1", "account_uuid": "a-2"}
        }));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: operator metadata survives, client fingerprint is gone.
        assert_eq!(
            body["metadata"]["trace"], "operator-set",
            "operator-deliberate metadata must survive: {body}"
        );
        assert!(
            body["metadata"].get("user_id").is_none(),
            "client metadata fingerprint must not survive: {body}"
        );
        let serialized = body.to_string();
        assert!(
            !serialized.contains("u-1"),
            "user_id fingerprint leaked into Invoke body: {serialized}"
        );
        assert!(
            !serialized.contains("a-2"),
            "account_uuid fingerprint leaked into Invoke body: {serialized}"
        );
    }

    #[test]
    fn response_format_inherited_from_anthropic_normalize_reaches_invoke_body() {
        // Bedrock Invoke delegates body construction to the Anthropic-API
        // normalizer, so honoring req.response_format there means the
        // output_config.format field rides through onto the Invoke body
        // (it survives the anthropic_beta withhold).
        // `name` is NOT carried: Anthropic's format object accepts only
        // `type` and `schema`, and AWS forwards the bag verbatim to the same
        // validator.
        let cfg = fake_cfg();
        let req = ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            max_tokens: Some(1024),
            response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {"name": "widget", "schema": {"type": "object"}}
            })),
            ..Default::default()
        };

        let body = normalize_request(&cfg, &req).unwrap();

        assert_eq!(
            body["output_config"]["format"]["type"], "json_schema",
            "response_format must reach the Invoke body: {body}"
        );
        let fmt = body["output_config"]["format"]
            .as_object()
            .expect("format must be an object");
        let mut keys: Vec<&str> = fmt.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["schema", "type"],
            "Invoke output_config.format must carry exactly {{type, schema}}; got: {body}"
        );
    }

    /// An operator `additional_model_request_fields` entry is merged onto the
    /// assembled Invoke body AFTER the shared normalizer ran, and
    /// `output_config` is not on the Invoke operator arm of
    /// `managed_keys::is_managed_key`, so that
    /// merge is a second write path for the rejected keys. The Invoke-side
    /// scrub closes it.
    #[test]
    fn operator_supplied_output_config_format_loses_name_and_strict() {
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "operator-widget",
                    "schema": {"type": "object"},
                    "strict": true
                }
            }
        }));
        let req = user_req();

        let body = normalize_request(&cfg, &req).unwrap();

        let fmt = body["output_config"]["format"]
            .as_object()
            .expect("format must be an object");
        assert!(
            fmt.get("name").is_none() && fmt.get("strict").is_none(),
            "operator-supplied name/strict must not reach the Invoke wire: {body}"
        );
        assert!(
            !body.to_string().contains("operator-widget"),
            "the operator's schema name must not reach the wire: {body}"
        );
    }

    /// The Invoke lane delegates body construction to the anthropic-api
    /// normalizer, so the shared sampling leak-guard fires here too --
    /// attributed to the Bedrock provider id.
    #[test]
    fn sampling_fields_emit_one_debug_event_naming_dropped_fields() {
        use crate::sampling_drop_guard::test_support::sole_sampling_drop;

        let cfg = fake_cfg();
        let mut req = user_req();
        req.seed = Some(42);
        req.top_logprobs = Some(5);

        let mut body = None;
        let events = routectl_testkit::capture_events(|| {
            body = Some(normalize_request(&cfg, &req).unwrap())
        });
        let body = body.expect("normalize ran inside the capture");

        assert!(body.get("seed").is_none(), "got: {body}");
        let warn = sole_sampling_drop(&events, &cfg.id);
        assert_eq!(
            warn.field("dropped_fields"),
            Some(r#"["seed", "top_logprobs"]"#)
        );
        assert_eq!(warn.field("dropped_count"), Some("2"));
    }

    #[test]
    fn no_sampling_warn_when_no_sampling_field_set() {
        use crate::sampling_drop_guard::test_support::{sampling_drops, sole_sampling_drop};

        let cfg = fake_cfg();
        let req = user_req();
        let control_cfg = BedrockConfig {
            id: "bedrock:sampling-control".into(),
            ..fake_cfg()
        };
        let mut control = user_req();
        control.n = Some(3);

        let events = routectl_testkit::capture_events(|| {
            let _ = normalize_request(&cfg, &req).unwrap();
            let _ = normalize_request(&control_cfg, &control).unwrap();
        });

        // The control's dropped `n` proves the capture saw the callsite.
        sole_sampling_drop(&events, &control_cfg.id);
        assert!(
            sampling_drops(&events, &cfg.id).is_empty(),
            "no sampling field must mean no sampling drop event; captured {events:?}"
        );
    }

    /// Both sources of the unrepresentable `output_config.format` keys are
    /// live on this lane at once: the canonical `response_format` (scrubbed by
    /// the shared assembly) and an operator `additional_model_request_fields`
    /// object merged AFTER it (scrubbed here). The two records fold into ONE
    /// WARN -- emitting at both sites would report a single request twice, and
    /// an operator counting these lines would double-count every such request.
    #[test]
    #[tracing_test::traced_test]
    fn dropped_format_keys_warn_once_across_both_write_paths() {
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "operator-widget",
                    "schema": {"type": "object"},
                    "strict": true
                }
            }
        }));
        let mut req = user_req();
        req.response_format = Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "caller-widget",
                "schema": {"type": "object"},
                "strict": true
            }
        }));

        let body = normalize_request(&cfg, &req).unwrap();

        let fmt = &body["output_config"]["format"];
        assert!(
            fmt.get("name").is_none() && fmt.get("strict").is_none(),
            "neither key may reach the wire from either path: {body}"
        );
        logs_assert(|lines: &[&str]| {
            let warns = lines
                .iter()
                .filter(|l| {
                    l.contains(crate::anthropic_api::request::OUTPUT_FORMAT_KEY_DROP_EVENT)
                        && l.contains("DEBUG")
                })
                .count();
            if warns == 1 {
                return Ok(());
            }
            Err(format!(
                "one request must produce exactly one dropped-format-key DEBUG line; got {warns}"
            ))
        });
    }

    /// An operator `additional_model_request_fields` object replaces the whole
    /// `output_config` the shared assembly repaired, so its schema arrives on
    /// this seam never having seen the `additionalProperties: false` pass.
    /// Anthropic (and AWS, which forwards to the same validator) rejects an
    /// object schema omitting the key, so the re-run is what makes the
    /// operator's schema shippable.
    #[test]
    fn operator_supplied_output_schema_is_repaired_on_the_invoke_body() {
        // Arrange
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "address": {
                                "type": "object",
                                "properties": {"street": {"type": "string"}}
                            }
                        }
                    }
                }
            }
        }));
        let req = user_req();

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let schema = &body["output_config"]["format"]["schema"];
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "the merge-supplied root schema must ship repaired: {body}"
        );
        assert_eq!(
            schema["properties"]["address"]["additionalProperties"],
            json!(false),
            "a nested object omitting the key 400s even when the root carries it: {body}"
        );
    }

    /// The repair runs twice on this lane -- once inside the shared assembly,
    /// once on the merged body -- and a non-`false` value present on both
    /// passes must still yield ONE WARN. An operator counting these lines
    /// would otherwise double-count every such request.
    #[test]
    fn additional_properties_forward_warns_once_across_both_repair_passes() {
        // Arrange
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "additionalProperties": true}
                }
            }
        }));
        let mut req = user_req();
        req.response_format = Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "caller-widget",
                "schema": {"type": "object", "additionalProperties": true}
            }
        }));

        // Act
        let mut body = serde_json::Value::Null;
        let events = routectl_testkit::capture_events(|| {
            body = normalize_request(&cfg, &req).unwrap();
        });

        // Assert
        assert_eq!(
            body["output_config"]["format"]["schema"]["additionalProperties"],
            json!(true),
            "a present non-false value is forwarded verbatim, never overwritten: {body}"
        );
        let warns: Vec<&routectl_testkit::CapturedEvent> = events
            .iter()
            .filter(|e| e.field("event") == Some("output_schema_additional_properties_not_false"))
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "one request must produce exactly one additionalProperties WARN; got {events:?}"
        );
        assert_eq!(warns[0].level, tracing::Level::WARN);
    }

    /// This lane egresses to a Bedrock endpoint, never to the genuine
    /// Anthropic host, so a routectl reasoning envelope inside a
    /// `redacted_thinking` block rides through byte-for-byte. Pins the
    /// EXPLICIT passthrough this call site selects, so a later edit that
    /// flips the shared normalizer's terminal-host argument here shows up
    /// as a failing test rather than as a silent wire change.
    #[test]
    fn reasoning_envelope_passes_through_verbatim_on_bedrock_invoke() {
        // Arrange
        use routectl_core::{ContentPart, KnownContentPart, reasoning_envelope};
        let envelope = reasoning_envelope::wrap(
            routectl_core::OPENAI_RESPONSES_V1,
            Some("rs_42"),
            "rsn_abc123-payload",
        );
        let cfg = fake_cfg();
        let mut req = user_req();
        req.messages = vec![
            req.messages[0].clone(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::RedactedThinking {
                        data: envelope.clone(),
                    }),
                    ContentPart::Known(KnownContentPart::Text {
                        text: "answer".into(),
                        citations: None,
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into();

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let data: Vec<&str> = body["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter(|b| b["type"] == "redacted_thinking")
            .filter_map(|b| b["data"].as_str())
            .collect();
        assert_eq!(
            data,
            vec![envelope.as_str()],
            "Bedrock Invoke must keep the envelope verbatim: {body}"
        );
    }
}
