//! Top-level request normalization. Walks a routectl `ChatRequest` into
//! the JSON body sent upstream:
//!
//!   1. Direct serde serialization of `ChatRequest`.
//!   2. Drop routectl-internal fields the upstream never wants.
//!   3. Apply outgoing-history reasoning policy (strip vs preserve).
//!   4. Hand off dialect-specific shaping to `Dialect::apply_request`.
//!   5. Merge `default_extras` then `provider_extras` last (callers win).
//!
//! Per-dialect logic lives in `dialects/*.rs`; this module is a thin
//! envelope around that dispatch.
//!
//! System content is projected before serialization: the filtered top-level
//! `system` lowers to a leading `role: "system"` message and every
//! `Role::System` turn stays in place, with the Claude Code
//! billing/attribution block withheld from both surfaces. The withhold counts
//! once per request on the policy-action counter, flushed ahead of the first
//! fallible step so a request that later fails translation is still counted.

use serde_json::Value;
use tracing::{debug, warn};

use routectl_core::wire_check::{EditKind, EditSite, WireTap};
use routectl_core::{
    ChatRequest, Error, Message, RefusalReason, RequestProvenance, Result, ToolDef,
    sanitize_for_log,
};

use super::HistoryReasoning;
use super::dialect::ReasoningDialect;
use super::dialects::util::strip_history_reasoning;
use crate::managed_keys::{
    ClientExtras, Dialect, MatchingIngress, client_extras_for, declare_foreign_keys, is_managed_key,
};
use crate::translation_drop_metrics::ClientFingerprintStripTally;

/// `source` value passed to [`merge_extras`] for operator-config-supplied
/// extras (`[providers.X] payload_extras = {...}` -- renamed from the
/// pre-v0.6.0 `default_extras`). Drop here is adversarial -- the
/// operator asked routectl to send a managed key.
const DEFAULT_EXTRAS_SOURCE: &str = "payload_extras";

/// `source` value passed to [`merge_extras`] for `req.provider_extras`: the
/// Chat ingress's forward-compat sweep, a library caller's own extras, or
/// the operator's `payload_extras` the router merged in. Another ingress
/// dialect's swept keys never reach the merge (see [`client_extras_for`]).
const PROVIDER_EXTRAS_SOURCE: &str = "provider_extras";

/// Extras keys this egress consumes into another field before the merge: the
/// response_format lift reads `output_config.format`. Withheld from the merge
/// whatever the provenance, and never counted as a foreign-dialect extra.
const TRANSLATED_EXTRAS_KEYS: [&str; 1] = ["output_config"];

/// Top-level extras key carrying the Anthropic client identity block
/// (`user_id`, `account_uuid`).
const CLIENT_METADATA_KEY: &str = "metadata";

/// Canonical top-level keys this egress removes from the serialized body
/// before the dialect runs, with the edit site each removal declares.
/// `system` (lowered into `messages`) and `provider_extras` (merged into the
/// top level) are carriers whose content stays on the wire, so they declare
/// nothing here.
const STRIPPED_TOP_LEVEL_KEYS: [(&str, EditSite); 4] = [
    ("reasoning", EditSite::DialectFieldStrip),
    ("chat_template_kwargs", EditSite::DialectFieldStrip),
    ("cache_control", EditSite::CacheMarkerStrip),
    ("anthropic_beta", EditSite::AnthropicBetaStrip),
];

/// Translate a canonical [`ChatRequest`] into an openai-compat wire body,
/// applying the reasoning dialect, history-reasoning policy, and merged
/// payload extras.
pub fn normalize(
    id: &str,
    req: &ChatRequest,
    dialect: ReasoningDialect,
    history_reasoning: HistoryReasoning,
    payload_extras: Option<&Value>,
    strict_translation: bool,
) -> Result<Value> {
    let _observation = crate::translation_drop_metrics::ObservationScope::enter(req);
    // The one point every openai-compat egress request passes exactly
    // once, on both the Ok and the Err arm: it precedes the first
    // fallible step. This is the DENOMINATOR for this lane's
    // translation-drop counters -- a raw drop count with no
    // request-volume figure behind it cannot tell a lane that drops on
    // every request from one that dropped once all week. Exactly one call
    // site per lane; a second anywhere would understate the rate for the
    // whole lane.
    crate::translation_drop_metrics::record_translation_lane_seen(super::LANE);
    count_unmodeled_thinking_type(req);

    // The system projection is infallible and runs first, so its tally is
    // flushed before any `?` below: a request that withheld the fingerprint
    // and then failed translation still counts against the denominator that
    // already counted it.
    let mut fingerprint = ClientFingerprintStripTally::default();
    let system = project_system(id, req, &mut fingerprint);
    let client_extras = select_client_extras(id, req, &mut fingerprint);
    flush_fingerprint_tally(&fingerprint);

    // Seams the OpenAI-compat wire can't represent: Anthropic-only
    // fields dropped from the body, plus unmodeled content blocks that
    // are forwarded verbatim. Default mode warns + continues; strict
    // mode 400s on either, having dropped nothing. The lenient tally is
    // flushed here, ahead of every later fallible step, so a request that
    // dropped a field and then failed translation still counts.
    check_dropped_anthropic_fields(id, req, strict_translation)?.flush();

    let mut body = serde_json::to_value(req)
        .map_err(|e| Error::Internal(format!("provider `{id}`: serialize request: {e}")))?;

    let obj = body.as_object_mut().ok_or_else(|| {
        Error::Internal(format!(
            "provider `{id}`: serialized request is not an object"
        ))
    })?;

    let tap = crate::wire_declare::tap(req);
    declare_fingerprint_strip(tap, &system);
    apply_system_projection(id, obj, system)?;

    // Dialects that need `chat_template_kwargs` re-inject it themselves.
    obj.remove("system");
    obj.remove("provider_extras");
    let stripped: Vec<(&str, EditSite)> = STRIPPED_TOP_LEVEL_KEYS
        .into_iter()
        .filter(|(key, _)| obj.remove(*key).is_some())
        .collect();

    let dyn_dialect = dialect.as_dyn();
    let dialect_strips = dyn_dialect.strip_history_reasoning();
    let resolved_strip = match history_reasoning {
        HistoryReasoning::Auto => dialect_strips,
        HistoryReasoning::Strip => true,
        HistoryReasoning::Preserve => false,
    };
    if resolved_strip && request_carries_reasoning(req) {
        warn!(
            provider = id,
            mode = ?history_reasoning,
            "openai-compat egress: assistant reasoning_content stripped from outgoing history. \
             Set `history_reasoning = \"preserve\"` on the provider if your upstream requires \
             echo-back (DeepSeek v4+, recent vLLM)."
        );
    }

    dyn_dialect.apply_request(id, obj, req)?;

    // Lift Anthropic-shape request fields to OpenAI-wire shape. Runs
    // after dialect apply so dialect shaping is visible, and before
    // extras merge so operator-supplied provider_extras cannot clobber
    // the lift (managed-key allow-list already blocks "tools" et al,
    // but belt-and-suspenders ordering matters for clarity).
    //
    // history_reasoning processing MUST run AFTER wire_lift, not
    // before. The `thinking` step of wire_lift extracts Anthropic
    // `thinking` / `redacted_thinking` content blocks from assistant
    // messages into the message-envelope `reasoning_details` array.
    // The strip path needs to see those extracted entries to remove
    // them; the preserve path needs them to lower into
    // `reasoning_content` (deepseek/vllm) or keep as typed details
    // (openrouter). Running history_reasoning before wire_lift would
    // make both modes blind to anything cc echoed as content blocks.
    super::wire_lift::lift_all(id, obj, req, strict_translation)?;

    match history_reasoning {
        HistoryReasoning::Auto => {
            if dialect_strips {
                strip_history_reasoning(id, obj, tap)?;
            }
        }
        HistoryReasoning::Strip => {
            strip_history_reasoning(id, obj, tap)?;
        }
        HistoryReasoning::Preserve => {
            dyn_dialect.preserve_history_reasoning(id, obj)?;
        }
    }

    // payload_extras (provider-level) then req.provider_extras
    // (canonical, post-dispatch merge of provider + model). Both
    // gated by the managed-key allow-list -- without this, a request
    // body of `provider_extras = {"messages":[...]}` could replace
    // the assembled messages.
    let mut withheld = ExtrasWithholdTally::default();
    if let Some(extras) = payload_extras {
        merge_extras(id, obj, extras, DEFAULT_EXTRAS_SOURCE, &mut withheld, tap);
    }
    declare_foreign_keys(tap, &client_extras.foreign_keys);
    if let Some(extras) = client_extras.forwardable.as_deref() {
        merge_extras(id, obj, extras, PROVIDER_EXTRAS_SOURCE, &mut withheld, tap);
    }
    withheld.flush();
    declare_stripped_keys(tap, obj, &stripped);
    declare_effort_withheld(tap, obj, req);
    drop_credential_bearing_fields(id, obj, tap);

    Ok(body)
}

/// Body fields never sent to an openai-compat host, whichever writer put
/// them there: `mcp_servers` entries can carry a connector credential issued
/// for a remote MCP server, never for this upstream.
const CREDENTIAL_BEARING_BODY_FIELDS: &[&str] = &["mcp_servers"];

/// Remove every [`CREDENTIAL_BEARING_BODY_FIELDS`] key from the finished
/// body, client and operator values alike. Runs as the LAST mutation of
/// [`normalize`] so no later writer can reintroduce a key. Logs the key name
/// only: a value may hold a credential.
/// TRANSLATION-DROP: policy-action class=credential_bearing_field_withheld test=a_chat_ingress_mcp_servers_is_absent_from_the_openai_compat_wire_and_counted_once
fn drop_credential_bearing_fields(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    tap: Option<&WireTap>,
) {
    let mut withheld = false;
    for &key in CREDENTIAL_BEARING_BODY_FIELDS {
        if obj.remove(key).is_some() {
            withheld = true;
            declare_member(tap, EditSite::ManagedExtrasStrip, key, EditKind::Remove);
            debug!(
                provider = id,
                field = key,
                "openai-compat egress: credential-bearing body field dropped before egress",
            );
        }
    }
    if withheld {
        crate::translation_drop_metrics::record_translation_policy_action(
            super::LANE,
            "credential_bearing_field_withheld",
        );
    }
}

/// Canonical system content projected onto this lane's wire, with the Claude
/// Code billing/attribution block withheld from both canonical surfaces:
/// openai-compat is a third-party upstream and must not receive the client
/// fingerprint the block carries.
struct SystemProjection {
    /// The filtered top-level `system`, flattened for the leading
    /// `role: "system"` message. `None` when absent, blank, or wholly
    /// withheld.
    lowered: Option<String>,
    /// `messages` with the block withheld from every `system` or `developer` turn.
    /// `None` when no turn carried it, so the serialized array stands.
    messages: Option<Vec<Message>>,
    /// True when the block was withheld from the top-level `system`.
    system_withheld: bool,
}

fn project_system(
    id: &str,
    req: &ChatRequest,
    fingerprint: &mut ClientFingerprintStripTally,
) -> SystemProjection {
    // A blank canonical system reads as "no canonical system supplied" (same
    // as None): it lowers no meaningless empty system message.
    let mut system_withheld = false;
    let lowered = req
        .system
        .as_ref()
        .filter(|s| !s.is_blank())
        .and_then(|s| crate::system_filter::strip_billing_attribution(s, &mut system_withheld))
        .map(|s| s.flatten())
        .filter(|text| !text.trim().is_empty());
    // Withheld by routectl, not by the wire: the lowered `role: "system"`
    // message would carry the block's text fine. One class and one tally
    // across both surfaces, so a request stripping on both is still one
    // action. The pin names the SINGLE-SOURCE test: a both-sources request
    // sets the shared tally from the sibling site too, so only a fingerprint
    // on this surface alone fails when this record is deleted.
    // TRANSLATION-DROP: policy-action class=client_fingerprint_stripped test=a_fingerprint_only_in_the_top_level_system_is_withheld_from_openai_compat_and_counted
    if system_withheld {
        fingerprint.record();
        debug!(
            provider = id,
            "openai-compat egress: Claude Code billing/attribution system block dropped",
        );
    }

    let messages = crate::system_filter::strip_system_role_messages(&req.messages);
    // The second site of the one policy action above, sharing its tally. A
    // system-role turn reaches this egress unlifted from the Anthropic ingress
    // and from library callers, with or without a top-level system.
    // TRANSLATION-DROP: policy-action class=client_fingerprint_stripped test=a_fingerprint_only_in_a_system_role_message_is_withheld_from_openai_compat_and_counted
    if messages.is_some() {
        fingerprint.record();
        debug!(
            provider = id,
            "openai-compat egress: Claude Code billing/attribution block dropped from a \
             system-role message",
        );
    }

    SystemProjection {
        lowered,
        messages,
        system_withheld,
    }
}

/// Count this request's fingerprint withhold, if any, once.
fn flush_fingerprint_tally(fingerprint: &ClientFingerprintStripTally) {
    if fingerprint.stripped() {
        crate::translation_drop_metrics::record_translation_policy_action(
            super::LANE,
            "client_fingerprint_stripped",
        );
    }
}

/// Declare the withhold from the top-level `system`, which reaches this wire
/// as the leading system-role message: its content is rewritten, or the
/// message is gone when nothing but the block was there.
fn declare_fingerprint_strip(tap: Option<&WireTap>, system: &SystemProjection) {
    let Some(tap) = tap.filter(|_| system.system_withheld) else {
        return;
    };
    let site = EditSite::ClientFingerprintStrip;
    if system.lowered.is_some() {
        crate::wire_declare::declare(tap, site, "/messages/0/content", EditKind::Rewrite);
    } else {
        crate::wire_declare::declare(tap, site, "/messages/0", EditKind::Remove);
    }
}

/// Write the projection into the serialized body. The top-level `system` is
/// lowered into a leading `role: "system"` message because strict
/// OpenAI-compat hosts (NIM) reject the top-level field with
/// `400 Validation: Unsupported parameter(s): system`. Every `Role::System`
/// turn the caller sent keeps its position after it: equal content is not
/// provenance, so nothing is deduplicated.
fn apply_system_projection(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    system: SystemProjection,
) -> Result<()> {
    if let Some(messages) = system.messages {
        let messages = serde_json::to_value(messages)
            .map_err(|e| Error::Internal(format!("provider `{id}`: serialize messages: {e}")))?;
        obj.insert("messages".into(), messages);
    }
    if let Some(text) = system.lowered {
        obj.entry("messages")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| {
                Error::Internal(format!(
                    "provider `{id}`: serialized messages is not an array"
                ))
            })?
            .insert(0, serde_json::json!({"role": "system", "content": text}));
    }
    Ok(())
}

/// Per-request record of the canonical fields this lane's wire has no slot
/// for. A FLAG per class rather than a count: the `(lane, class)` counters are
/// per REQUEST, so four cache breakpoints dropped from four surfaces of one
/// request are one drop event. Strict mode rejects instead of dropping, so its
/// tally is discarded with the error and nothing is counted.
#[must_use = "a canonical-field drop counts nothing until the tally is flushed"]
#[derive(Debug, Default)]
struct CanonicalFieldDropTally {
    cache_control: bool,
    anthropic_beta: bool,
}

impl CanonicalFieldDropTally {
    fn flush(&self) {
        if self.cache_control {
            crate::translation_drop_metrics::record_translation_drop(
                super::LANE,
                "cache_control_unsupported",
            );
        }
        if self.anthropic_beta {
            crate::translation_drop_metrics::record_translation_drop(
                super::LANE,
                "anthropic_beta_unsupported",
            );
        }
    }
}

/// Count the client thinking choice this wire loses when an Anthropic client
/// sent a `thinking.type` canonical reasoning cannot express: the body is
/// built from the canonical config alone, which holds the default for it.
/// Runs ahead of every fallible step, so a request that then fails still
/// counts against the denominator that already counted it.
/// TRANSLATION-DROP: lane=openai-compat class=thinking_type_unrepresentable test=an_unmodeled_thinking_type_counts_one_openai_compat_drop
fn count_unmodeled_thinking_type(req: &ChatRequest) {
    if crate::translation_drop_metrics::carries_unmodeled_thinking_type(req) {
        crate::translation_drop_metrics::record_translation_drop(
            super::LANE,
            "thinking_type_unrepresentable",
        );
    }
}

/// Per-request record of the extras entries [`merge_extras`] kept off the
/// wire, across both of its calls, so a request withholding several keys
/// through either source is still one event per class.
#[must_use = "an extras withhold counts nothing until the tally is flushed"]
#[derive(Debug, Default)]
struct ExtrasWithholdTally {
    provider_managed_key: bool,
    operator_managed_key: bool,
}

impl ExtrasWithholdTally {
    fn flush(&self) {
        if self.provider_managed_key {
            crate::translation_drop_metrics::record_translation_policy_action(
                super::LANE,
                "provider_extra_managed_key_conflict",
            );
        }
        if self.operator_managed_key {
            crate::translation_drop_metrics::record_translation_policy_action(
                super::LANE,
                "operator_extra_managed_key_conflict",
            );
        }
    }
}

/// Shallow-merge `extras` into `obj` with a routectl-managed-keys
/// allow-list. Drop when an extras entry tries to override a key
/// routectl owns (model, messages, stream, tools, etc.). The
/// `source` arg names where the override came from and selects the
/// log level + wording:
///
///   - `DEFAULT_EXTRAS_SOURCE` -> WARN with the existing adversarial
///     phrasing. Operator asked routectl to send a managed key, so
///     the drop deserves their attention.
///   - `PROVIDER_EXTRAS_SOURCE` -> DEBUG with neutral phrasing. A
///     canonical key in a client's extras bag is a client shape, not an
///     operator misconfiguration.
fn merge_extras(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    extras: &Value,
    source: &'static str,
    withheld: &mut ExtrasWithholdTally,
    tap: Option<&WireTap>,
) {
    let Some(extra_obj) = extras.as_object() else {
        return;
    };
    for (k, v) in extra_obj {
        if withholds_extras_key(k) {
            if source == PROVIDER_EXTRAS_SOURCE {
                if TRANSLATED_EXTRAS_KEYS.contains(&k.as_str()) {
                    // TRANSLATION-DROP: structural -- the response_format lift reads
                    // `output_config.format` off the request before this merge and
                    // counts its own loss when the format has no equivalent here,
                    // so withholding the leftover object loses no format twice.
                } else {
                    // A canonical key arriving through the extras bag would
                    // replace a field routectl assembled; the wire would carry
                    // it, and routectl refuses it to keep its own value
                    // authoritative.
                    // TRANSLATION-DROP: policy-action class=provider_extra_managed_key_conflict test=a_provider_extra_naming_a_canonical_key_is_refused_and_counted
                    withheld.provider_managed_key = true;
                }
                tracing::debug!(
                    provider = id,
                    source = source,
                    key = %sanitize_for_log(k),
                    "extras entry names a key routectl writes itself; dropped"
                );
                declare_member(tap, EditSite::ManagedExtrasStrip, k, EditKind::Remove);
            } else {
                // The operator's configured value would ride fine; routectl
                // refuses it so its own assembled field stays authoritative.
                // TRANSLATION-DROP: policy-action class=operator_extra_managed_key_conflict test=an_operator_extra_naming_a_managed_key_is_refused_and_counted
                withheld.operator_managed_key = true;
                tracing::warn!(
                    provider = id,
                    source = source,
                    key = %sanitize_for_log(k),
                    "extras attempted to override routectl-managed key; dropped"
                );
            }
            continue;
        }
        if source == DEFAULT_EXTRAS_SOURCE {
            let kind = if obj.contains_key(k) {
                EditKind::Rewrite
            } else {
                EditKind::Add
            };
            declare_member(tap, EditSite::OperatorPayloadExtras, k, kind);
        }
        obj.insert(k.clone(), v.clone());
    }
}

/// True when an extras entry under `key` never reaches this wire: a key
/// routectl writes itself, or one this egress consumes into another field.
fn withholds_extras_key(key: &str) -> bool {
    is_managed_key(Dialect::OpenAiCompat, key) || TRANSLATED_EXTRAS_KEYS.contains(&key)
}

/// The client extras this request may merge onto the Chat body. Only the
/// Chat ingress (and a library caller) speaks this wire's vocabulary; another
/// ingress's swept keys are withheld, and only the operator's own
/// `payload_extras` forward. Runs ahead of every fallible step so the
/// request's counts are flushed with the fingerprint tally.
fn select_client_extras<'a>(
    id: &str,
    req: &'a ChatRequest,
    fingerprint: &mut ClientFingerprintStripTally,
) -> ClientExtras<'a> {
    let client_extras = client_extras_for(
        req,
        Dialect::OpenAiCompat,
        MatchingIngress::Http(RequestProvenance::OpenaiIngress),
        &TRANSLATED_EXTRAS_KEYS,
    );
    let (metadata, foreign): (Vec<&str>, Vec<&str>) = client_extras
        .foreign_keys
        .iter()
        .partition(|k| **k == CLIENT_METADATA_KEY);
    // Another dialect's `metadata` is the Anthropic client identity block.
    // It shares the request's fingerprint tally with the system strips, so a
    // request withholding it there too is still one action, and it is never
    // counted again as a foreign-dialect extra. Logs the key only.
    // TRANSLATION-DROP: policy-action class=client_fingerprint_stripped test=an_anthropic_ingress_metadata_object_is_absent_from_the_openai_compat_wire_and_counted_once
    if !metadata.is_empty() {
        fingerprint.record();
        debug!(
            provider = id,
            "openai-compat egress: client metadata from another ingress dialect dropped",
        );
    }
    // A key another dialect's client sent is spelled for that dialect's
    // upstream; strict hosts answer `Unsupported parameter`, and routectl
    // does not guess a Chat equivalent.
    // TRANSLATION-DROP: lane=openai-compat class=foreign_dialect_extra test=foreign_dialect_extras_are_absent_from_the_openai_compat_wire_and_counted_once
    if !foreign.is_empty() {
        crate::translation_drop_metrics::record_translation_drop(
            super::LANE,
            "foreign_dialect_extra",
        );
        debug!(
            provider = id,
            keys = %sanitize_for_log(&foreign.join(",")),
            "openai-compat egress: client extras from another ingress dialect dropped",
        );
    }
    client_extras
}

/// Declare the removal of each stripped top-level key the finished body
/// still lacks. A dialect that re-injects a key (vLLM's
/// `chat_template_kwargs`, OpenRouter's `reasoning`) leaves nothing removed
/// to declare.
fn declare_stripped_keys(
    tap: Option<&WireTap>,
    obj: &serde_json::Map<String, Value>,
    stripped: &[(&str, EditSite)],
) {
    for &(key, site) in stripped {
        if !obj.contains_key(key) {
            declare_member(tap, site, key, EditKind::Remove);
        }
    }
}

/// Force `stream_options.include_usage: true` onto a finished streaming
/// body: without it many hosts end the stream on a bare `[DONE]`, with no
/// terminal usage chunk or `finish_reason` for the stream summary. Runs
/// after the extras merge, so it sees whatever value the client sent. Only
/// the operator's own `payload_extras` (the router's per-request record, or
/// the config slot of a provider built without the router) may set the flag;
/// when it does, the body is left as merged.
pub(super) fn force_stream_include_usage(
    id: &str,
    body: &mut Value,
    req: &ChatRequest,
    config_payload_extras: Option<&Value>,
) {
    let operator_sets_flag = [
        req.routectl_internal.operator_payload_extras.as_deref(),
        config_payload_extras,
    ]
    .into_iter()
    .flatten()
    .any(|extras| extras.pointer(INCLUDE_USAGE_POINTER).is_some());
    if operator_sets_flag {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let tap = crate::wire_declare::tap(req);
    let (path, kind) = match obj.get_mut("stream_options") {
        None => {
            obj.insert(
                "stream_options".to_owned(),
                serde_json::json!({ "include_usage": true }),
            );
            ("/stream_options", EditKind::Add)
        }
        Some(Value::Object(options)) => match options.get("include_usage") {
            Some(Value::Bool(true)) => return,
            None => {
                options.insert("include_usage".to_owned(), Value::Bool(true));
                (INCLUDE_USAGE_POINTER, EditKind::Add)
            }
            Some(_) => {
                options.insert("include_usage".to_owned(), Value::Bool(true));
                (INCLUDE_USAGE_POINTER, EditKind::Rewrite)
            }
        },
        Some(other) => {
            *other = serde_json::json!({ "include_usage": true });
            ("/stream_options", EditKind::Rewrite)
        }
    };
    if let Some(tap) = tap {
        crate::wire_declare::declare(tap, EditSite::StreamOptionsInjection, path, kind);
    }
    if kind == EditKind::Rewrite {
        // The client's value would ride the wire fine; routectl replaces it
        // because the stream summary and usage accounting need the usage chunk.
        // TRANSLATION-DROP: policy-action class=client_stream_include_usage_overridden test=a_client_include_usage_false_is_forced_true_declared_and_counted_once
        crate::translation_drop_metrics::record_translation_policy_action(
            super::LANE,
            "client_stream_include_usage_overridden",
        );
        debug!(
            provider = id,
            path = path,
            "openai-compat egress: client stream_options value overridden to request usage",
        );
    }
}

/// JSON pointer of the stream usage flag, in the body and in an operator's
/// `payload_extras` alike.
const INCLUDE_USAGE_POINTER: &str = "/stream_options/include_usage";

/// The canonical effort reaches this wire only as a dialect's flat
/// `reasoning_effort`. When the finished body carries none (a dialect with
/// no effort field, reasoning disabled, or `effort: "none"`), declare the
/// field a Chat client spells it as removed.
fn declare_effort_withheld(
    tap: Option<&WireTap>,
    obj: &serde_json::Map<String, Value>,
    req: &ChatRequest,
) {
    let carried = req.reasoning.as_ref().is_some_and(|r| r.effort.is_some());
    if carried && !obj.contains_key("reasoning_effort") {
        declare_member(
            tap,
            EditSite::DialectFieldStrip,
            "reasoning_effort",
            EditKind::Remove,
        );
    }
}

/// Declare an edit at top-level member `key`, when a tap is listening and
/// the key can be named by a pointer.
fn declare_member(tap: Option<&WireTap>, site: EditSite, key: &str, kind: EditKind) {
    if let Some(tap) = tap
        && let Some(path) = crate::wire_declare::member_pointer("", key)
    {
        crate::wire_declare::declare(tap, site, path, kind);
    }
}

/// Log each canonical field or block the openai-compat wire cannot
/// represent: the designed Anthropic-only drops at DEBUG, unmodeled
/// forward-compat blocks at WARN. Two kinds of finding:
///
/// - Anthropic-only fields (`cache_control` on the request, system,
///   tools, or content blocks; `anthropic_beta`) that are dropped
///   from the outbound body.
/// - `ContentPart::Other` blocks routectl does not model. These are
///   NOT dropped: default mode forwards them verbatim for the
///   upstream to accept or reject.
///
/// Default mode logs and returns the request's drop tally for the caller to
/// flush once; strict mode collects both kinds of finding and returns an
/// `Error::Validation` (HTTP 400) so the operator sees them before the
/// upstream does.
fn check_dropped_anthropic_fields(
    id: &str,
    req: &ChatRequest,
    strict: bool,
) -> Result<CanonicalFieldDropTally> {
    let mut findings: Vec<String> = Vec::new();
    let mut tally = CanonicalFieldDropTally::default();
    let mut record = |msg: String| {
        if strict {
            findings.push(msg);
        }
    };

    // The four cache-breakpoint arms share one class: no surface of this
    // wire carries a breakpoint, so each is the same loss, counted once per
    // request however many surfaces carried one.
    // TRANSLATION-DROP: lane=openai-compat class=cache_control_unsupported test=a_top_level_cache_control_is_absent_from_the_openai_compat_wire_and_counted
    if req.cache_control.is_some() {
        tally.cache_control = true;
        debug!(
            provider = id,
            "openai-compat egress: top-level cache_control dropped (prompt caching not supported)",
        );
        record("top-level cache_control".into());
    }
    // TRANSLATION-DROP: lane=openai-compat class=anthropic_beta_unsupported test=anthropic_beta_flags_are_absent_from_the_openai_compat_wire_and_counted
    if !req.anthropic_beta.is_empty() {
        tally.anthropic_beta = true;
        debug!(
            provider = id,
            beta_flags = ?req.anthropic_beta,
            "openai-compat egress: anthropic_beta flags dropped (Anthropic-only)",
        );
        record(format!("anthropic_beta {:?}", req.anthropic_beta));
    }
    if let Some(routectl_core::SystemContent::Blocks(blocks)) = &req.system {
        let any_cc = blocks.iter().any(|b| b.cache_control.is_some());
        // TRANSLATION-DROP: lane=openai-compat class=cache_control_unsupported test=a_system_block_cache_control_is_absent_from_the_openai_compat_wire_and_counted
        if any_cc {
            tally.cache_control = true;
            debug!(
                provider = id,
                "openai-compat egress: per-block cache_control on system dropped",
            );
            record("per-block cache_control on system".into());
        }
    }
    if let Some(tools) = &req.tools {
        for t in tools {
            if let ToolDef::Other(v) = t {
                // OpenAI function-shape tools (`{type:"function",
                // function:{...}}`) land here today (the OpenAI ingress
                // does not lift them, so canonical preserves the wire
                // shape). They serialize verbatim through the
                // openai-compat egress and reach the upstream
                // unchanged -- no warn needed. Only Anthropic builtin
                // / unknown shapes are genuinely dropped at the
                // upstream's deserialization step.
                if routectl_core::CustomTool::from_openai_function(v).is_some() {
                    continue;
                }
                // wire_lift::tools::lift is the canonical write point for
                // the Anthropic-builtin warn; no second warn here.
            } else if let ToolDef::Custom(c) = t
                && c.cache_control.is_some()
            {
                // TRANSLATION-DROP: lane=openai-compat class=cache_control_unsupported test=a_tool_cache_control_is_absent_from_the_openai_compat_wire_and_counted
                tally.cache_control = true;
                debug!(
                    provider = id,
                    tool = %sanitize_for_log(&c.name),
                    "openai-compat egress: tool cache_control dropped (Anthropic-only)",
                );
                record(format!("tool `{}` cache_control", c.name));
            }
        }
    }
    for (i, m) in req.messages.iter().enumerate() {
        if let routectl_core::MessageContent::Parts(parts) = &m.content {
            for p in parts {
                match p {
                    routectl_core::ContentPart::Other { type_tag, .. } => {
                        warn!(
                            provider = id,
                            message_index = i,
                            block_type = %sanitize_for_log(type_tag),
                            "openai-compat egress: forward-compat content block is unmodeled",
                        );
                        record(format!("forward-compat block `{type_tag}` on message {i}"));
                    }
                    routectl_core::ContentPart::Known(k) => {
                        // TRANSLATION-DROP: lane=openai-compat class=cache_control_unsupported test=a_message_part_cache_control_is_absent_from_the_openai_compat_wire_and_counted
                        if k.cache_control().is_some() {
                            tally.cache_control = true;
                            debug!(
                                provider = id,
                                message_index = i,
                                block_type = k.type_tag(),
                                "openai-compat egress: per-block cache_control dropped",
                            );
                            record(format!(
                                "per-block cache_control on message {i} ({})",
                                k.type_tag()
                            ));
                        }
                    }
                }
            }
        }
    }

    if strict && !findings.is_empty() {
        return Err(Error::Validation(
            format!(
                "strict_translation: {} canonical-only field(s) cannot be carried by the openai-compat egress: {}",
                findings.len(),
                findings.join("; ")
            ),
            RefusalReason::StrictTranslation,
        ));
    }
    Ok(tally)
}

/// True when the canonical request carries assistant reasoning that
/// the strip path would silently drop. Drives the operator-visibility
/// warn so a DeepSeek-v4 / vLLM operator can see why their upstream
/// 400s without enabling debug logs.
///
/// Checks three shapes:
///   1. Flat `m.reasoning` string (DeepSeek / vLLM echo-back slot).
///   2. Typed `m.reasoning_details` array (lifted details).
///   3. `ContentPart::Known(KnownContentPart::Thinking)` content block --
///      the common Anthropic-ingress shape where the thinking trace rides
///      inside the parts array instead of the dedicated reasoning slots.
fn request_carries_reasoning(req: &ChatRequest) -> bool {
    use routectl_core::{ContentPart, KnownContentPart, MessageContent};
    req.messages.iter().any(|m| {
        matches!(m.role, routectl_core::Role::Assistant)
            && (m.reasoning.as_deref().is_some_and(|s| !s.is_empty())
                || !m.reasoning_details.is_empty()
                || matches!(&m.content, MessageContent::Parts(parts)
                    if parts.iter().any(|p| matches!(
                        p,
                        ContentPart::Known(KnownContentPart::Thinking { .. })
                    ))
                ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{
        ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, ReasoningConfig, Role,
    };
    use serde_json::json;

    fn push_msg(req: &mut ChatRequest, m: Message) {
        let mut msgs = req.messages.to_vec();
        msgs.push(m);
        req.messages = msgs.into();
    }

    fn insert_msg(req: &mut ChatRequest, idx: usize, m: Message) {
        let mut msgs = req.messages.to_vec();
        msgs.insert(idx, m);
        req.messages = msgs.into();
    }

    fn simple_req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
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
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(512),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.1),
            ..Default::default()
        }
    }

    #[test]
    fn openai_passthrough_normal_model() {
        let req = simple_req("llama-3.3-70b");
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        // temperature preserved for ids outside the modern chat shape
        assert!(body.get("temperature").is_some());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("provider_extras").is_none());
    }

    /// v0.8: the openai-compat egress MUST NOT inject `max_tokens` when
    /// the caller omitted it. The good-translator principle: only
    /// inject where the upstream demands it (Anthropic-shape egresses).
    /// Pins the resolution-policy guarantee for #35.
    #[test]
    fn openai_compat_does_not_inject_max_tokens_when_caller_omitted() {
        let mut req = simple_req("gpt-4o");
        req.max_tokens = None;
        // Also set the router carrier to a non-zero value; the egress
        // must STILL leave it out of the wire body. routectl_internal
        // is Anthropic-shape territory only.
        req.routectl_internal.max_output_tokens = 8000;
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(
            body.get("max_tokens").is_none()
                || body.get("max_tokens") == Some(&serde_json::Value::Null),
            "openai-compat must leave max_tokens absent on omitted-by-caller requests; got: {body}"
        );
    }

    #[test]
    fn openai_drops_sampling_for_o_series() {
        let req = simple_req("o3-mini");
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("presence_penalty").is_none());
    }

    /// Reasoning models (drops_sampling_params=true) reject `max_tokens`
    /// on real OpenAI; the OpenAI ingress renamed the inbound
    /// `max_completion_tokens` to canonical `max_tokens`, so the egress
    /// must rename it back for these models.
    #[test]
    fn openai_renames_max_tokens_to_max_completion_tokens_for_reasoning_model() {
        let req = simple_req("o3-mini"); // simple_req sets max_tokens = 512
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(
            body.get("max_tokens").is_none(),
            "reasoning model must not carry max_tokens; got: {body}"
        );
        assert_eq!(
            body["max_completion_tokens"], 512,
            "reasoning model must carry max_completion_tokens"
        );
    }

    /// Bedrock Mantle names OpenAI models with an `openai.` vendor segment;
    /// the reasoning-model shaping still applies.
    #[test]
    fn openai_shapes_vendor_prefixed_reasoning_model_like_its_bare_id() {
        let req = simple_req("openai.gpt-5.6-terra");
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["max_completion_tokens"], 512, "got: {body}");
        assert!(body.get("max_tokens").is_none(), "got: {body}");
        assert!(body.get("top_p").is_none(), "got: {body}");
    }

    /// Every served gpt-6 id refuses `max_tokens` and `top_p`.
    #[test]
    fn openai_shapes_vendor_prefixed_gpt6_as_a_reasoning_model() {
        let req = simple_req("openai.gpt-6-sol");
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["max_completion_tokens"], 512, "got: {body}");
        assert!(body.get("max_tokens").is_none(), "got: {body}");
        assert!(body.get("top_p").is_none(), "got: {body}");
        assert!(body.get("temperature").is_none(), "got: {body}");
        assert!(body.get("presence_penalty").is_none(), "got: {body}");
        assert!(body.get("frequency_penalty").is_none(), "got: {body}");
    }

    /// An id outside the modern chat shape keeps `max_tokens` unchanged and
    /// must not gain a `max_completion_tokens` key.
    #[test]
    fn openai_keeps_max_tokens_outside_the_modern_chat_shape() {
        let req = simple_req("llama-3.3-70b"); // max_tokens = 512
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 512);
        assert!(
            body.get("max_completion_tokens").is_none(),
            "an id outside the modern shape must not gain max_completion_tokens; got: {body}"
        );
    }

    #[test]
    fn openai_maps_reasoning_effort() {
        let mut req = simple_req("o3");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        });
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    /// OpenRouter's native request shape IS the canonical `reasoning`
    /// object, so the egress must re-emit it on the wire (effort +
    /// budget survive the round-trip). Without this the request envelope's
    /// unconditional `reasoning` strip drops all reasoning control for
    /// OpenRouter with no signal.
    #[test]
    fn openrouter_emits_canonical_reasoning_object() {
        let mut req = simple_req("anthropic/claude-sonnet-4-5");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            exclude: Some(true),
            ..Default::default()
        });
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenRouter,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        let reasoning = body
            .get("reasoning")
            .and_then(|v| v.as_object())
            .expect("OpenRouter must re-emit the reasoning object");
        assert_eq!(reasoning["effort"], "high");
        assert_eq!(reasoning["exclude"], true);
        // The `reasoning_effort` scalar is the OpenAI-dialect shape, not
        // OpenRouter's; it must NOT leak onto the OpenRouter wire.
        assert!(
            body.get("reasoning_effort").is_none(),
            "OpenRouter must not emit the OpenAI-shape reasoning_effort scalar, got: {body}"
        );
    }

    /// An Anthropic-ingress `thinking.display: "updates"` reaches this
    /// egress as `exclude: Some(true)` plus an internal carrier string.
    /// OpenRouter re-emits the whole canonical object, so the semantic
    /// `exclude` must survive while no `display` key appears on its wire
    /// -- this is the negative that catches a future canonical `display`
    /// field leaking through `serde_json::to_value`.
    #[test]
    fn openrouter_updates_display_emits_exclude_without_display_key() {
        // Arrange
        let mut req = simple_req("anthropic/claude-sonnet-4-5");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            exclude: Some(true),
            ..Default::default()
        });
        req.routectl_internal.anthropic_thinking_display = Some("updates".into());

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenRouter,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();

        // Assert
        let reasoning = body
            .get("reasoning")
            .and_then(|v| v.as_object())
            .expect("OpenRouter must re-emit the reasoning object");
        assert_eq!(reasoning["exclude"], true);
        assert!(
            reasoning.get("display").is_none(),
            "no display key may reach the OpenRouter wire; got: {body}"
        );
    }

    /// A non-OpenRouter openai-compat dialect (plain Chat Completions)
    /// still strips the canonical `reasoning` object -- these hosts 400
    /// on the unknown key.
    #[test]
    fn passthrough_strips_canonical_reasoning_object() {
        let mut req = simple_req("some-model");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        });
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(
            body.get("reasoning").is_none(),
            "non-OpenRouter dialects must strip the reasoning object, got: {body}"
        );
    }

    /// An OpenRouter request with no reasoning config must not gain an
    /// empty `reasoning: {}` key -- an all-None config is a no-op.
    #[test]
    fn openrouter_omits_reasoning_when_config_absent() {
        let req = simple_req("anthropic/claude-sonnet-4-5");
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenRouter,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(
            body.get("reasoning").is_none(),
            "absent reasoning config must not produce an empty reasoning key, got: {body}"
        );
    }

    #[test]
    fn deepseek_drops_sampling_for_reasoner() {
        let req = simple_req("deepseek-reasoner");
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::DeepSeek,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn deepseek_strips_reasoning_content_from_history() {
        let mut req = simple_req("deepseek-reasoner");
        push_msg(
            &mut req,
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("I thought about it".into()),
                reasoning: Some("hidden chain".into()),
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::DeepSeek,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        for m in msgs {
            assert!(m.get("reasoning_content").is_none());
            assert!(m.get("reasoning").is_none());
            assert!(m.get("reasoning_details").is_none());
        }
    }

    /// DeepSeek v4 + history_reasoning="preserve" must echo the
    /// canonical `reasoning` string back to the wire as
    /// `reasoning_content`. Without this, multi-turn 400s with
    /// "reasoning_content in the thinking mode must be passed back
    /// to the API". This is the bug that motivated the whole branch.
    #[test]
    fn deepseek_preserve_renames_reasoning_to_reasoning_content() {
        let mut req = simple_req("deepseek-reasoner");
        push_msg(
            &mut req,
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("I thought about it".into()),
                reasoning: Some("hidden chain".into()),
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::DeepSeek,
            HistoryReasoning::Preserve,
            None,
            false,
        )
        .unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert_eq!(
            assistant["reasoning_content"], "hidden chain",
            "preserve mode must rename reasoning -> reasoning_content"
        );
        assert!(
            assistant.get("reasoning").is_none(),
            "preserve mode must drop the legacy `reasoning` slot",
        );
    }

    /// Counterpart: explicit `Strip` must drop reasoning even on a
    /// dialect whose default would preserve. Future-proofing in case
    /// a dialect default flips.
    #[test]
    fn explicit_strip_overrides_dialect_default() {
        let mut req = simple_req("anything");
        push_msg(
            &mut req,
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("a".into()),
                reasoning: Some("zap me".into()),
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenRouter, // default = passthrough/no-op
            HistoryReasoning::Strip,
            None,
            false,
        )
        .unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert!(assistant.get("reasoning").is_none());
        assert!(assistant.get("reasoning_content").is_none());
    }

    /// OpenRouter + preserve emits the typed `reasoning_details`
    /// array (Anthropic-aligned shape).
    #[test]
    fn openrouter_preserve_lifts_reasoning_to_typed_details_array() {
        let mut req = simple_req("anthropic/claude-haiku-4-5");
        push_msg(
            &mut req,
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: Some("trace".into()),
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenRouter,
            HistoryReasoning::Preserve,
            None,
            false,
        )
        .unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        let details = assistant["reasoning_details"].as_array().unwrap();
        assert_eq!(details[0]["text"], "trace");
        assert_eq!(details[0]["type"], "reasoning.text");
        assert!(assistant.get("reasoning").is_none());
    }

    #[test]
    fn vllm_injects_enable_thinking() {
        let mut req = simple_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[test]
    fn provider_extras_merged_last() {
        let mut req = simple_req("gpt-4o");
        req.provider_extras = Some(json!({"custom_key": "custom_val"}));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["custom_key"], "custom_val");
        assert!(body.get("provider_extras").is_none());
    }

    #[test]
    fn responses_reasoning_remainder_dropped() {
        // A Responses-ingress request carrying reasoning summary/context/mode
        // routed to an openai-compat egress must NOT emit those keys on the
        // wire. The context/mode fidelity WARN is emitted router-side, per
        // dispatched target.
        let mut req = simple_req("gpt-4o");
        req.provider_extras = Some(json!({
            "reasoning": {"summary": "concise", "context": "all_turns", "mode": "pro"}
        }));
        let body = normalize(
            "openrouter:test",
            &req,
            ReasoningDialect::OpenRouter,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        // The reasoning remainder is dropped, not smuggled onto the wire.
        assert!(body.get("reasoning").is_none());
        assert!(body.get("summary").is_none());
        assert!(body.get("context").is_none());
        assert!(body.get("mode").is_none());
    }

    #[test]
    fn responses_reasoning_summary_only_remainder_dropped() {
        let mut req = simple_req("gpt-4o");
        req.provider_extras = Some(json!({"reasoning": {"summary": "concise"}}));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn provider_extras_cannot_override_routectl_managed_keys() {
        // Stray routectl-managed keys in
        // `provider_extras = {"messages": [...], "model": "..."}`
        // could replace the assembled messages or model before the
        // body went upstream. The Anthropic egress had an allow-list;
        // the openai-compat egress did not. Verify routectl-managed
        // keys are dropped here, while long-tail
        // provider knobs (`top_k`, `seed`, dialect-specific) still
        // pass through.
        let mut req = simple_req("gpt-4o");
        req.seed = Some(7);
        req.provider_extras = Some(json!({
            // canonical fields -- all MUST be dropped:
            "model": "evil-model",
            "messages": [{"role": "user", "content": "INJECTED"}],
            "stream": true,
            "tools": [],
            "max_tokens": 1,
            "seed": 99,
            "presence_penalty": 1.5,
            "frequency_penalty": 1.5,
            "response_format": {"type": "json_object"},
            // long-tail provider knobs -- MUST pass through:
            "top_k": 40,
            "service_tier": "premium",
            "safety_settings": {"hate": "high"},
        }));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        // Canonical fields preserved from the request, NOT overridden.
        assert_eq!(body["model"], "gpt-4o");
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 1);
        assert_ne!(messages[0]["content"], "INJECTED");
        assert_ne!(body["max_tokens"], 1);
        // `seed` is canonical; the request had `seed = 7`, extras
        // tried to set it to 99 -- canonical wins.
        assert_eq!(body["seed"], 7);
        assert!(body.get("presence_penalty").is_none() || body["presence_penalty"] != 1.5);
        assert!(body.get("response_format").is_none());
        // Long-tail extras land verbatim.
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["service_tier"], "premium");
        assert_eq!(body["safety_settings"]["hate"], "high");
    }

    /// Drop-semantics parity: a managed-key collision from
    /// `default_extras` (operator-config source) is dropped EXACTLY
    /// the way the provider_extras (forward-compat sweep source)
    /// drop is. The source-branching change in `merge_extras` only
    /// toggles the log level/wording; the dropped-key set is
    /// identical. This pins that the two source branches converge on
    /// the same body shape (the WARN vs DEBUG choice is reviewable in
    /// source; the drop is asserted here).
    #[test]
    fn default_extras_cannot_override_routectl_managed_keys() {
        let mut req = simple_req("gpt-4o");
        req.seed = Some(7);
        let defaults = json!({
            // canonical fields -- MUST be dropped:
            "model": "evil-model",
            "messages": [{"role": "user", "content": "INJECTED"}],
            "stream": true,
            "tools": [],
            "max_tokens": 1,
            "seed": 99,
            // long-tail provider knobs -- MUST pass through:
            "top_k": 40,
            "service_tier": "premium",
        });
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            Some(&defaults),
            false,
        )
        .unwrap();
        // Canonical fields preserved from the request, NOT overridden.
        assert_eq!(body["model"], "gpt-4o");
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 1);
        assert_ne!(messages[0]["content"], "INJECTED");
        assert_ne!(body["max_tokens"], 1);
        assert_eq!(body["seed"], 7);
        // Long-tail extras land verbatim.
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["service_tier"], "premium");
    }

    #[test]
    fn anthropic_ingress_metadata_is_dropped_from_openai_compat_body() {
        // The Anthropic ingress sweeps the inbound `metadata` object into
        // provider_extras. Strict openai-compat hosts (NIM, vLLM-strict,
        // DeepSeek-direct) 400 with `Unsupported parameter(s): metadata`,
        // so the egress must strip it from the upstream body.
        let mut req = simple_req("gpt-4o");
        req.provider_extras = Some(json!({"metadata": {"user_id": "abc123"}}));
        req.routectl_internal.provenance = RequestProvenance::AnthropicIngress;
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(
            body.get("metadata").is_none(),
            "Anthropic metadata must not reach the openai-compat wire, got: {body}"
        );
    }

    #[test]
    fn extras_cannot_re_inject_top_level_system() {
        // Regression: the `system` lowering above strips the top-level
        // `system` key. If extras (default or provider) could
        // reintroduce it, strict hosts (NIM) would 400 again. The
        // managed-key allow-list must include `system`.
        use routectl_core::SystemContent;
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Text("real system prompt".into()));
        req.provider_extras = Some(json!({
            "system": "INJECTED via provider_extras",
        }));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            Some(&json!({"system": "INJECTED via default_extras"})),
            false,
        )
        .unwrap();
        assert!(
            body.get("system").is_none(),
            "top-level `system` must remain stripped, got: {body}"
        );
        // The lowered system message is at messages[0].
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "real system prompt");
    }

    #[test]
    fn default_extras_overridden_by_provider_extras() {
        let req_mut = {
            let mut r = simple_req("gpt-4o");
            r.provider_extras = Some(json!({"key": "from_request"}));
            r
        };
        let defaults = json!({"key": "from_defaults"});
        let body = normalize(
            "test",
            &req_mut,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            Some(&defaults),
            false,
        )
        .unwrap();
        assert_eq!(body["key"], "from_request");
    }

    #[test]
    fn system_text_lowered_to_role_system_message() {
        // Canonical `system: "you are a helpful bot"` MUST be lowered
        // into a synthetic `role: "system"` message at the start of
        // the messages array, and the top-level `system` field MUST
        // be removed. NIM strict-rejects unknown top-level fields with
        // `400 Validation: Unsupported parameter(s): system`.
        use routectl_core::SystemContent;
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Text("you are a helpful bot".into()));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(
            body.get("system").is_none(),
            "top-level `system` must be stripped, body: {body}"
        );
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "you are a helpful bot");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn system_blocks_flattened_to_role_system_message() {
        // SystemContent::Blocks (Anthropic-shape per-block array)
        // collapses to a single newline-joined string for the
        // synthetic role:"system" message, since OpenAI-compat has no
        // wire shape for typed system blocks.
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "first block".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "second block".into(),
                cache_control: None,
                citations: None,
            },
        ]));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(body.get("system").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "first block\nsecond block");
    }

    #[test]
    fn empty_system_does_not_inject_message() {
        // SystemContent::Text("") flattens to "" -- skip injection,
        // don't waste a wire slot on a noop system prompt.
        use routectl_core::SystemContent;
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Text(String::new()));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(body.get("system").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user", "no system message injected");
    }

    /// Pin: every blank canonical system shape -- empty string,
    /// whitespace-only, and blocks whose every text is blank -- lowers to no
    /// system message at all, and the top-level field never reaches the wire.
    #[test]
    fn blank_system_shapes_inject_no_system_message() {
        use routectl_core::{SystemBlock, SystemContent};

        let blank = |text: &str| SystemBlock {
            kind: "text".into(),
            text: text.into(),
            cache_control: None,
            citations: None,
        };
        for system in [
            SystemContent::Text(String::new()),
            SystemContent::Text("   \n\t ".into()),
            SystemContent::Blocks(vec![blank(""), blank("  \n")]),
        ] {
            // Arrange
            let mut req = simple_req("test-model");
            req.system = Some(system);

            // Act
            let body = normalize(
                "test",
                &req,
                ReasoningDialect::Passthrough,
                HistoryReasoning::Auto,
                None,
                false,
            )
            .unwrap();

            // Assert
            assert!(
                body.get("system").is_none(),
                "the top-level system field must never reach the wire: {body}"
            );
            let messages = body["messages"].as_array().unwrap();
            assert!(
                messages.iter().all(|m| m["role"] != "system"),
                "no system message may be injected for a blank system: {body}"
            );
        }
    }

    /// A blank canonical system lowers nothing, and a direct caller's
    /// Role::System message stays first on the wire.
    #[test]
    fn blank_canonical_system_preserves_role_system_message() {
        use routectl_core::SystemContent;

        // Arrange
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Text("   ".into()));
        insert_msg(
            &mut req,
            0,
            Message {
                refusal: None,
                role: Role::System,
                content: MessageContent::Text("you are helpful".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();

        // Assert
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "you are helpful");
    }

    /// `request_carries_reasoning` must return `true` when an assistant
    /// message has a `Thinking` content part (the Anthropic-ingress shape
    /// where the thinking trace rides in the parts array). Before this fix
    /// the check only looked at `m.reasoning` and `m.reasoning_details`,
    /// so the strip WARN was never emitted for this shape even though the
    /// downstream strip would remove the block.
    #[test]
    fn request_carries_reasoning_detects_thinking_content_part() {
        let mut req = simple_req("any-model");
        push_msg(
            &mut req,
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "my trace".into(),
                        signature: None,
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
        );
        assert!(
            request_carries_reasoning(&req),
            "Thinking content part must be detected as carrying reasoning"
        );
    }

    /// Counterpart: a request with NO reasoning in any form must return `false`.
    #[test]
    fn request_carries_reasoning_false_when_no_reasoning() {
        let req = simple_req("any-model");
        assert!(!request_carries_reasoning(&req));
    }

    /// The Claude Code billing/attribution block (a system block whose
    /// text starts with `x-anthropic-billing-header:`) must be dropped
    /// before the openai-compat egress lowers `system` to a role:system
    /// message; a normal sibling block must survive and reach the wire.
    #[test]
    fn openai_compat_drops_billing_block_keeps_normal_block() {
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req("gpt-4o");
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
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(
            messages[0]["content"], "you are helpful",
            "billing block must be dropped; only the normal block survives, got: {body}"
        );
    }

    /// A block whose text contains the billing prefix MID-string (not at
    /// the start) is a normal prompt and must be preserved.
    #[test]
    fn openai_compat_preserves_mid_string_billing_prefix() {
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req("gpt-4o");
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "intro x-anthropic-billing-header: not at start".into(),
            cache_control: None,
            citations: None,
        }]));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(
            messages[0]["content"], "intro x-anthropic-billing-header: not at start",
            "a mid-string occurrence must NOT be treated as the billing block"
        );
    }

    /// Leading whitespace before the billing prefix still matches and the
    /// block is dropped (mirrors the reference trim-then-prefix check).
    #[test]
    fn openai_compat_drops_billing_block_with_leading_whitespace() {
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req("gpt-4o");
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "  \n\tx-anthropic-billing-header: v=1".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "real prompt".into(),
                cache_control: None,
                citations: None,
            },
        ]));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[0]["content"], "real prompt",
            "leading-whitespace billing block must still be dropped, got: {body}"
        );
    }

    /// This lane's request-volume denominator, read through the registry's own
    /// accessor, so it reads correctly even before any drop class on this lane
    /// has fired.
    fn lane_seen_count() -> u64 {
        crate::translation_drop_metrics::translation_lane_seen(super::super::PROVIDER_KIND)
    }

    /// The `(openai-compat, client_fingerprint_stripped)` policy-action
    /// counter, read through the public snapshot. Zero before its first bump.
    fn fingerprint_strip_count() -> u64 {
        crate::translation_drop_metrics::translation_policy_action_snapshot()
            .into_iter()
            .find(|e| e.lane == "openai-compat" && e.policy_class == "client_fingerprint_stripped")
            .map_or(0, |e| e.action_count)
    }

    /// Build a request whose `system` carries the supplied blocks.
    fn req_with_system_blocks(texts: &[&str]) -> ChatRequest {
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req("gpt-4o");
        req.system = Some(SystemContent::Blocks(
            texts
                .iter()
                .map(|text| SystemBlock {
                    kind: "text".into(),
                    text: (*text).into(),
                    cache_control: None,
                    citations: None,
                })
                .collect(),
        ));
        req
    }

    fn lenient_normalize(req: &ChatRequest) -> serde_json::Value {
        normalize(
            "test",
            req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .expect("lenient normalize must succeed")
    }

    /// The counted half of the strip: one request whose system carries the
    /// block bumps the policy-action counter exactly once, however many blocks
    /// it stripped, and the withheld text is absent from the lowered message
    /// while its unmarked sibling survives.
    #[test]
    fn the_billing_strip_counts_one_openai_compat_policy_action_for_the_request() {
        // Arrange -- TWO billing blocks in one request. The count is per
        // REQUEST, so two stripped blocks are still one action; a
        // per-occurrence bump would read 2 here.
        let req = req_with_system_blocks(&[
            "x-anthropic-billing-header: v=1; fp=secret",
            "x-anthropic-billing-header: v=2; fp=other",
            "you are helpful",
        ]);

        // Act
        let before = fingerprint_strip_count();
        let body = lenient_normalize(&req);
        let after = fingerprint_strip_count();

        // Assert
        assert_eq!(
            after - before,
            1,
            "two stripped blocks in one request are one policy action"
        );
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["content"], "you are helpful");
        assert!(
            !body.to_string().contains("fp="),
            "the withheld fingerprint must not reach the wire body: {body}"
        );
    }

    /// The record sits ahead of the message lowering, so the request whose
    /// whole system IS the block -- the one shape that flattens to empty and
    /// lowers no system message at all -- still reaches the counter. A record
    /// placed past that skip would miss exactly the requests that strip the
    /// most while the lane denominator counted them, reading the action rate
    /// low for the case that withholds most often.
    #[test]
    fn an_all_billing_system_still_counts_the_openai_compat_policy_action() {
        // Arrange
        let req = req_with_system_blocks(&["x-anthropic-billing-header: v=1; fp=secret"]);

        // Act
        let before = fingerprint_strip_count();
        let body = lenient_normalize(&req);
        let after = fingerprint_strip_count();

        // Assert -- nothing was lowered, and the strip still counted.
        let messages = body["messages"].as_array().expect("messages array");
        assert!(
            messages.iter().all(|m| m["role"] != "system"),
            "an all-billing system must lower no system message, got: {body}"
        );
        assert_eq!(
            after - before,
            1,
            "a system that collapsed to nothing still stripped a block"
        );
    }

    /// The positive control on the counter: a system carrying no billing block
    /// leaves the key untouched, so the delta assertions above cannot pass on a
    /// counter that bumps for every request.
    #[test]
    fn a_system_with_no_billing_block_records_no_openai_compat_policy_action() {
        // Arrange
        let req = req_with_system_blocks(&["you are helpful"]);

        // Act
        let before = fingerprint_strip_count();
        let body = lenient_normalize(&req);
        let after = fingerprint_strip_count();

        // Assert
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["content"], "you are helpful");
        assert_eq!(after, before, "nothing was stripped, so nothing is counted");
    }

    /// A request carrying one Anthropic-shape document block: the content
    /// lift drops it (creating a row on this lane) and strict mode rejects
    /// it, so one fixture drives both of `normalize`'s arms.
    fn req_with_dropped_document() -> ChatRequest {
        let mut req = simple_req("m");
        push_msg(
            &mut req,
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::Document {
                        source: json!({
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": "AA=="
                        }),
                        title: None,
                        citations: None,
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );
        req
    }

    /// `normalize` bumps this lane's request-volume denominator on BOTH the
    /// Ok and the Err arm. Without the Err arm, strict-rejected requests
    /// vanish from the denominator and every drop rate on the lane reads
    /// high. Each arm must count exactly once; the at-most-one-call-site half
    /// is welded by the census test.
    #[test]
    fn normalize_counts_the_request_on_both_arms() {
        // Arrange
        let req = req_with_dropped_document();

        // Act + Assert -- the lenient arm returns Ok and is counted.
        let before = lane_seen_count();
        normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .expect("lenient normalize must succeed");
        assert_eq!(
            lane_seen_count() - before,
            1,
            "the Ok arm must count the request in the lane denominator once"
        );

        // Act + Assert -- the strict arm returns Err and is still counted.
        let before = lane_seen_count();
        let res = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            true,
        );
        assert!(
            res.is_err(),
            "strict mode must reject the unrepresentable document block"
        );
        assert_eq!(
            lane_seen_count() - before,
            1,
            "the Err arm must count the request in the lane denominator once too"
        );
    }

    /// A request whose user turn carries one block type no canonical variant
    /// models, so it deserializes to `ContentPart::Other`.
    fn req_with_unknown_block() -> ChatRequest {
        let part: ContentPart = serde_json::from_value(json!({
            "type": "future_widget_block",
            "widget_id": "wdg_01",
            "payload": {"nested": [1, 2, 3]}
        }))
        .expect("an unknown block type deserializes");
        assert!(
            matches!(&part, ContentPart::Other { .. }),
            "fixture must exercise the forward-compat variant, got {part:?}"
        );
        let mut req = simple_req("m");
        push_msg(
            &mut req,
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(vec![part]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );
        req
    }

    #[tracing_test::traced_test]
    #[test]
    fn lenient_mode_warns_that_an_unknown_content_block_is_unmodeled_without_claiming_a_drop() {
        // Arrange
        let req = req_with_unknown_block();

        // Act
        let _ = lenient_normalize(&req);

        // Assert
        logs_assert(|lines: &[&str]| {
            let warned: Vec<&&str> = lines
                .iter()
                .filter(|line| line.contains("future_widget_block"))
                .collect();
            match warned.as_slice() {
                [line]
                    if line.contains("forward-compat content block is unmodeled")
                        && !line.contains("dropped") =>
                {
                    Ok(())
                }
                _ => Err(format!(
                    "expected one warning calling the block unmodeled and not dropped, since \
                     lenient mode forwards it: {warned:?}"
                )),
            }
        });
    }

    #[test]
    fn lenient_mode_forwards_an_unknown_content_block_verbatim() {
        // Arrange
        let req = req_with_unknown_block();

        // Act
        let body = lenient_normalize(&req);

        // Assert
        let parts = body["messages"][1]["content"]
            .as_array()
            .expect("the unknown block's turn keeps its parts array");
        assert_eq!(
            parts,
            &vec![json!({
                "type": "future_widget_block",
                "widget_id": "wdg_01",
                "payload": {"nested": [1, 2, 3]}
            })],
            "the unknown block must reach the wire with its type and extras intact"
        );
    }

    #[test]
    fn strict_mode_rejects_an_unknown_content_block() {
        // Arrange
        let req = req_with_unknown_block();

        // Act
        let res = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            true,
        );

        // Assert
        assert_eq!(
            res.as_ref().err().and_then(Error::refusal_reason),
            Some(routectl_core::RefusalReason::StrictTranslation)
        );
        match res {
            Err(Error::Validation(msg, _)) => assert!(
                msg.contains("forward-compat block `future_widget_block` on message 1"),
                "the rejection must name the unknown block, got: {msg}"
            ),
            other => panic!("strict mode must reject the unknown block, got {other:?}"),
        }
    }

    include!("request_system_tests.rs");
    include!("request_drop_level_tests.rs");
    include!("request_drop_counter_tests.rs");
    include!("request_stream_usage_tests.rs");
}
