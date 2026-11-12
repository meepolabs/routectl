//! Reasoning controls + `provider_extras` allowlist for the Responses
//! API egress.
//!
//! Reasoning translation:
//! - `req.reasoning.effort` -> `reasoning.effort`.
//! - `reasoning.summary` defaults to `"auto"` (so the server emits
//!   reasoning_summary deltas back on stream) UNLESS the caller supplied
//!   one via `provider_extras["reasoning"].summary`.
//! - `reasoning.context` / `reasoning.mode` / any future Responses-dialect
//!   sub-key ride through `provider_extras["reasoning"]` onto the wire.
//! - `req.reasoning.max_tokens` -> mapped to the nearest `effort` band
//!   via the effort<->budget table when no explicit effort is set. The
//!   Responses reasoning surface has no budget knob; an explicit effort
//!   still wins.
//!
//! provider_extras allowlist (8 keys): `prompt_cache_key`,
//! `prompt_cache_options`, `prompt_cache_retention`, `service_tier`,
//! `text`, `include`, `store`, `client_metadata`.
//!
//! Any other key, on a request whose extras are spelled for this wire
//! (`forwards_unknowns`: the Responses ingress or a library caller): the
//! api-key surface forwards it at the top level unless routectl writes that
//! name itself (a `ResponsesRequest` member or a canonical request key); the
//! ChatGPT-OAuth and Bedrock Mantle surfaces keep the allowlist. Every key a
//! surface does not forward is withheld, declared as a Remove, and the
//! request counts once as `unknown_member_withheld`. A key from another
//! ingress dialect was spelled for another wire and stays unforwarded
//! ([`foreign_top_level_keys`]); the request-level foreign withhold declares
//! and counts it.
//!
//! Prompt caching: `prompt_cache_options` and `prompt_cache_retention`
//! forward only for a request the Responses ingress produced, and only on
//! the auth surfaces that accept them (see [`PromptCacheField`]); a
//! withheld one is counted once per request through
//! [`PromptCacheWithheldTally`]. When neither the client nor the operator
//! set `prompt_cache_key`, the egress sends the hex SHA-256 of the inbound
//! session key: OpenAI refuses keys over 64 characters, and the raw client
//! session id stays off another vendor's wire. Bedrock Mantle gets no such
//! derived key, since nothing records that it accepts the field.
//!
//! ChatGPT-OAuth lock: `store` is always written to `false` when the
//! provider authenticates via `AuthKind::ChatgptOauth`, regardless of
//! any operator-supplied `provider_extras["store"]`. Codex sends
//! `store: false` on every ChatGPT subscription request and routectl
//! preserves that behavior to avoid the upstream rejecting the request
//! for a policy mismatch.
//!
//! ChatGPT-OAuth `client_metadata`: the resolved per-installation id is
//! stamped into the body's `client_metadata` object under
//! `x-codex-installation-id`, matching where codex carries it on the
//! streaming `/responses` call. Operator-supplied values win.

use std::fmt::Write as _;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use routectl_core::wire_check::{EditKind, EditSite, WireTap, forwards_unknowns};
use routectl_core::{ChatRequest, ReasoningConfig, RequestProvenance, is_canonical_request_key};

use super::AuthKind;
use super::types::{RESPONSES_REQUEST_MEMBERS, ResponsesReasoning, ResponsesRequest, TextControls};
use crate::effort::{level_from_budget, wire_effort};
use crate::translation_drop_metrics::record_translation_drop;
use crate::wire_declare::declare;

/// Set `request.reasoning` from `req.reasoning` plus the Responses-dialect
/// remainder the ingress stashed under `provider_extras["reasoning"]`.
///
/// `effort` comes from the computed canonical value; `summary` defaults to
/// `"auto"` ONLY when the caller supplied none (a caller value wins).
/// `context` / `mode` / any future sub-key ride through the overlay onto
/// the wire object. summary / context / mode are independently meaningful:
/// a summary-only, context-only, or mode-only request still emits a
/// reasoning object. An explicit canonical `enabled: false` WINS
/// unconditionally and omits reasoning entirely -- regardless of any
/// computed effort, budget, or overlay sub-key. An explicit
/// `effort: "none"` is reasoning-OFF and omits reasoning the same way.
pub(super) fn apply_reasoning(
    request: &mut ResponsesRequest,
    req: &ChatRequest,
    tap: Option<&WireTap>,
) {
    let overlay = responses_reasoning_overlay(req);

    // An explicit `effort: "none"` is a reasoning-OFF request; treat it like
    // `enabled: false` and omit reasoning entirely rather than emit a
    // reasoning object (which would leave thinking ON via a budget or a
    // summary/context overlay).
    let effort_disabled = req
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.as_deref())
        .is_some_and(ReasoningConfig::is_off_effort);
    if effort_disabled {
        if let Some(tap) = tap {
            declare(
                tap,
                EditSite::DialectFieldStrip,
                "/reasoning",
                EditKind::Remove,
            );
        }
        return;
    }

    let (effort, enabled, budget) = match req.reasoning.as_ref() {
        Some(r) => {
            // Explicit effort wins. When no effort is set but a budget is,
            // map the budget to the nearest effort band (the Responses API
            // takes effort, not a budget) rather than dropping it. A
            // reasoning-OFF level (`None`) leaves effort unset.
            let effort = match r.effort.as_deref() {
                Some(e) => wire_effort(e).map(str::to_string),
                None => r
                    .max_tokens
                    .and_then(|budget| wire_effort(level_from_budget(budget)))
                    .map(str::to_string),
            };
            (effort, r.enabled, r.max_tokens)
        }
        None => (None, None, None),
    };

    // Explicit disable wins unconditionally: `enabled: false` is the caller
    // turning reasoning off, so it beats a computed effort, a budget-derived
    // effort, and any provider_extras["reasoning"] overlay sub-key.
    if enabled == Some(false) {
        return;
    }
    // Nothing to emit: no effort, no enable flag, no budget, no caller
    // sub-key. A summary-only / context-only / mode-only request DOES emit
    // because `overlay` is `Some`.
    if effort.is_none() && enabled.is_none() && budget.is_none() && overlay.is_none() {
        return;
    }

    let mut extra = overlay.unwrap_or_default();
    // `effort` is owned by the typed field / computed canonical value; drop
    // any overlay copy so it can never override it through the flatten.
    extra.remove("effort");
    // summary: a caller value wins; default to "auto" only when unset.
    let summary = match extra.remove("summary") {
        Some(Value::String(s)) => Some(s),
        Some(other) => {
            extra.insert("summary".into(), other);
            None
        }
        None => {
            if let Some(tap) = tap {
                declare(
                    tap,
                    EditSite::ReasoningSummaryDefault,
                    "/reasoning/summary",
                    EditKind::Add,
                );
            }
            Some("auto".into())
        }
    };

    request.reasoning = Some(ResponsesReasoning {
        effort,
        summary,
        extra,
    });
}

/// The Responses-dialect reasoning remainder the ingress stashed under
/// `provider_extras["reasoning"]` (summary/context/mode/future). Returns
/// `None` when absent or empty so it never forces an otherwise-omitted
/// reasoning object into existence.
fn responses_reasoning_overlay(req: &ChatRequest) -> Option<Map<String, Value>> {
    req.provider_extras
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("reasoning"))
        .and_then(|v| v.as_object())
        .filter(|m| !m.is_empty())
        .cloned()
}

/// Layer canonical `req.provider_extras` into the Responses request.
/// The 8 allowed keys are honored on every surface; any other key goes
/// through [`TopLevelCarry`] (see the module doc). The `store` flag is
/// special-cased: for `ChatgptOauth`, it stays hardcoded to `false`
/// regardless of any `provider_extras["store"]` value. The two prompt-cache
/// fields pass the gate in [`apply_prompt_cache_field`], which records each
/// one it withholds in `withheld`.
pub(super) fn merge_provider_extras(
    request: &mut ResponsesRequest,
    req: &ChatRequest,
    auth_kind: AuthKind,
    tap: Option<&WireTap>,
    withheld: &mut PromptCacheWithheldTally,
    member_withheld: &mut UnknownMemberWithheldTally,
) {
    let Some(extras) = req.provider_extras.as_ref().and_then(|v| v.as_object()) else {
        return;
    };
    let carry = TopLevelCarry::of(req.routectl_internal.provenance, auth_kind);
    let mut unforwarded = UnforwardedMembers::default();

    for (k, v) in extras {
        match k.as_str() {
            "prompt_cache_key" => {
                if let Some(s) = v.as_str() {
                    request.prompt_cache_key = Some(s.to_string());
                }
            }
            "prompt_cache_options" => {
                let field = PromptCacheField::Options;
                apply_prompt_cache_field(request, req, auth_kind, field, v, tap, withheld);
            }
            "prompt_cache_retention" => {
                let field = PromptCacheField::Retention;
                apply_prompt_cache_field(request, req, auth_kind, field, v, tap, withheld);
            }
            "service_tier" => {
                if let Some(s) = v.as_str() {
                    request.service_tier = Some(s.to_string());
                }
            }
            "text" => {
                request.text = Some(TextControls { inner: v.clone() });
            }
            "include" => {
                if let Some(arr) = v.as_array() {
                    request.include = arr
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect();
                }
            }
            "store" => apply_store_override(request, v, auth_kind),
            "client_metadata" => {
                request.client_metadata = Some(v.clone());
            }
            // Read by `apply_reasoning` and `resolve_parallel_tool_calls`,
            // which build the typed fields from them.
            key if CONSUMED_ELSEWHERE.contains(&key) => {}
            key => carry.apply(request, key, v, &mut unforwarded),
        }
    }
    unforwarded.flush(tap, member_withheld);
}

/// Extras keys another step of this egress reads into a typed field.
const CONSUMED_ELSEWHERE: &[&str] = &["reasoning", "parallel_tool_calls"];

/// Allowlisted extras keys the merge applies whatever the request's ingress
/// dialect. The two prompt-cache fields are not among them: they forward
/// only for a Responses client.
const APPLIED_FROM_ANY_INGRESS: &[&str] = &[
    "prompt_cache_key",
    "service_tier",
    "text",
    "include",
    "store",
    "client_metadata",
];

/// The top-level extras keys of `req` that no surface of `auth_kind`
/// forwards because a client of another ingress dialect spelled them for
/// its own wire: every key outside [`APPLIED_FROM_ANY_INGRESS`] and
/// [`CONSUMED_ELSEWHERE`]. Empty for a request whose extras are spelled for
/// this wire. [`merge_provider_extras`] leaves exactly these unapplied.
pub(super) fn foreign_top_level_keys(req: &ChatRequest, auth_kind: AuthKind) -> Vec<&str> {
    if TopLevelCarry::of(req.routectl_internal.provenance, auth_kind) != TopLevelCarry::Foreign {
        return Vec::new();
    }
    req.provider_extras
        .as_ref()
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(Map::keys)
        .map(String::as_str)
        .filter(|key| !APPLIED_FROM_ANY_INGRESS.contains(key) && !CONSUMED_ELSEWHERE.contains(key))
        .collect()
}

/// What the egress does with a top-level extras key outside the allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopLevelCarry {
    /// The api-key surface: forward it unless routectl writes the name.
    Open,
    /// The ChatGPT-OAuth and Bedrock Mantle surfaces: the allowlist is the
    /// whole forwarded set (the OAuth backend refuses fields its codex
    /// client never sends; nothing records what Mantle accepts).
    Allowlist,
    /// Extras spelled for another wire: never forwarded.
    Foreign,
}

impl TopLevelCarry {
    fn of(provenance: RequestProvenance, auth_kind: AuthKind) -> Self {
        if !forwards_unknowns(provenance, auth_kind.egress_kind()) {
            return Self::Foreign;
        }
        match auth_kind {
            AuthKind::ApiKey => Self::Open,
            AuthKind::ChatgptOauth | AuthKind::BedrockMantle => Self::Allowlist,
        }
    }

    fn apply(
        self,
        request: &mut ResponsesRequest,
        key: &str,
        value: &Value,
        unforwarded: &mut UnforwardedMembers,
    ) {
        match self {
            Self::Open if !is_written_by_egress(key) => {
                request.carried.insert(key.to_owned(), value.clone());
            }
            Self::Open => unforwarded.record(key, EditSite::ManagedExtrasStrip),
            Self::Allowlist => unforwarded.record(key, EditSite::AuthSurfaceGate),
            // A key a client of another dialect sent names a member of that
            // dialect's wire, which no Responses surface defines.
            // TRANSLATION-DROP: structural -- declared and counted once per request by the request-level foreign withhold, from foreign_top_level_keys
            Self::Foreign => {}
        }
    }
}

/// Whether routectl writes the top-level member `key` itself, so an extras
/// value under that name must never reach the wire beside or over it.
fn is_written_by_egress(key: &str) -> bool {
    RESPONSES_REQUEST_MEMBERS.contains(&key) || is_canonical_request_key(key)
}

/// The top-level extras keys one request's surface did not forward, each
/// with the edit site that withheld it.
#[derive(Debug, Default)]
struct UnforwardedMembers {
    keys: Vec<(String, EditSite)>,
}

impl UnforwardedMembers {
    fn record(&mut self, key: &str, site: EditSite) {
        self.keys.push((key.to_owned(), site));
    }

    /// Declare each withheld key as a top-level Remove and record the
    /// request on `member_withheld`. A key that is not token-shaped is left
    /// undeclared, so it still surfaces as a finding.
    fn flush(&self, tap: Option<&WireTap>, member_withheld: &mut UnknownMemberWithheldTally) {
        if self.keys.is_empty() {
            return;
        }
        member_withheld.record();
        tracing::debug!(
            withheld = self.keys.len(),
            "openai-responses egress: top-level extras outside this surface's forwarded set withheld"
        );
        let Some(tap) = tap else {
            return;
        };
        for (key, site) in &self.keys {
            if let Some(path) = crate::wire_declare::member_pointer("", key) {
                declare(tap, *site, path, EditKind::Remove);
            }
        }
    }
}

/// Per-request record that this egress withheld at least one client member
/// it does not model: a top-level extras key outside the surface's
/// forwarded set, or on Bedrock Mantle an unreviewed input item, tool,
/// namespace container or carried member. One tally per request shared by
/// every such site, flushed once outside every fallible step, so a request
/// withholding several counts once.
#[derive(Debug, Default)]
#[must_use = "a tally records nothing until flush() runs"]
pub(super) struct UnknownMemberWithheldTally {
    withheld: bool,
}

impl UnknownMemberWithheldTally {
    /// Note one more withheld member. Idempotent.
    pub(super) const fn record(&mut self) {
        self.withheld = true;
    }

    /// Count the request's withhold, if any, once.
    /// TRANSLATION-DROP: lane=openai-responses class=unknown_member_withheld test=top_level_members_outside_the_forwarded_set_are_withheld_declared_and_counted_once
    pub(super) fn flush(&self) {
        if self.withheld {
            record_translation_drop(super::LANE, "unknown_member_withheld");
        }
    }
}

/// A Responses prompt-cache field the egress forwards per auth surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PromptCacheField {
    Options,
    Retention,
    /// A per-block `prompt_cache_breakpoint`.
    Breakpoint,
    /// The `prompt_cache_key` routectl derives from the inbound session key
    /// when neither the client nor the operator set one.
    SessionKeyFallback,
}

impl PromptCacheField {
    /// The wire member name: top-level, or on a content block for
    /// `Breakpoint`.
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Options => "prompt_cache_options",
            Self::Retention => "prompt_cache_retention",
            Self::Breakpoint => "prompt_cache_breakpoint",
            Self::SessionKeyFallback => "prompt_cache_key",
        }
    }

    /// Whether `auth_kind` accepts the field. The ChatGPT-OAuth backend
    /// refuses fields its codex client never sends: codex sends
    /// `prompt_cache_options` and `prompt_cache_key` but neither
    /// `prompt_cache_retention` nor a breakpoint. Nothing records that the
    /// Mantle surface accepts any of them.
    pub(super) const fn forwards_on(self, auth_kind: AuthKind) -> bool {
        match auth_kind {
            AuthKind::ApiKey => true,
            AuthKind::ChatgptOauth => matches!(self, Self::Options | Self::SessionKeyFallback),
            AuthKind::BedrockMantle => false,
        }
    }
}

/// Per-request record that the egress withheld at least one client
/// prompt-cache field. One tally per request shared by every withhold site
/// (top-level fields and per-block markers alike), so a request withholding
/// several counts once. Flush it outside every fallible step of the
/// request's assembly, or on the error path too, so a withhold followed by a
/// failure still counts.
#[must_use = "a prompt-cache withhold tally counts nothing until it is flushed"]
#[derive(Debug, Default)]
pub(super) struct PromptCacheWithheldTally {
    withheld: bool,
}

impl PromptCacheWithheldTally {
    /// Note one more withheld prompt-cache field. Idempotent.
    pub(super) const fn record(&mut self) {
        self.withheld = true;
    }

    /// Count the request's withhold, if any, once.
    /// TRANSLATION-DROP: lane=openai-responses class=prompt_cache_field_withheld test=withheld_cache_fields_count_once_per_request
    pub(super) fn flush(&self) {
        if self.withheld {
            record_translation_drop(super::LANE, "prompt_cache_field_withheld");
        }
    }
}

/// Forward one prompt-cache field when the Responses ingress produced the
/// request and the auth surface accepts it. A Responses-ingress value the
/// surface refuses is withheld, recorded in `withheld` and declared as an
/// auth-surface Remove. A request from any other ingress spelled the field
/// for another dialect, so it stays unforwarded like any unlisted key.
fn apply_prompt_cache_field(
    request: &mut ResponsesRequest,
    req: &ChatRequest,
    auth_kind: AuthKind,
    field: PromptCacheField,
    value: &Value,
    tap: Option<&WireTap>,
    withheld: &mut PromptCacheWithheldTally,
) {
    if req.routectl_internal.provenance != RequestProvenance::ResponsesIngress {
        return;
    }
    if !field.forwards_on(auth_kind) {
        withheld.record();
        if let Some(tap) = tap {
            declare(
                tap,
                EditSite::AuthSurfaceGate,
                format!("/{}", field.key()),
                EditKind::Remove,
            );
        }
        return;
    }
    match field {
        PromptCacheField::Options => request.prompt_cache_options = Some(value.clone()),
        PromptCacheField::Retention => request.prompt_cache_retention = Some(value.clone()),
        PromptCacheField::Breakpoint | PromptCacheField::SessionKeyFallback => {}
    }
}

/// When neither the client nor the operator set `prompt_cache_key`, key
/// the upstream prompt cache on the lowercase hex SHA-256 of the inbound
/// session key (always 64 characters, whatever the session key's length),
/// on the surfaces [`PromptCacheField::SessionKeyFallback`] forwards on.
/// Nothing is withheld where it does not: no client field is involved.
/// Must run AFTER `merge_provider_extras` so their key wins.
pub(super) fn apply_session_cache_key(
    request: &mut ResponsesRequest,
    req: &ChatRequest,
    auth_kind: AuthKind,
    tap: Option<&WireTap>,
) {
    if request.prompt_cache_key.is_some()
        || !PromptCacheField::SessionKeyFallback.forwards_on(auth_kind)
    {
        return;
    }
    let Some(session) = req.routectl_internal.inbound_session_key.as_deref() else {
        return;
    };
    if let Some(tap) = tap {
        let carried = req
            .provider_extras
            .as_ref()
            .and_then(|v| v.get("prompt_cache_key"))
            .is_some();
        let kind = if carried {
            EditKind::Rewrite
        } else {
            EditKind::Add
        };
        declare(tap, EditSite::CachePlacement, "/prompt_cache_key", kind);
    }
    request.prompt_cache_key = Some(sha256_hex(session));
}

/// Digest of a content block's text, as recorded beside a Responses
/// `prompt_cache_breakpoint`: the ingress computes it when it records the
/// breakpoint and the egress recomputes it to confirm the block it is about
/// to mark still holds the same text. Hex SHA-256.
pub fn breakpoint_text_digest(text: &str) -> String {
    sha256_hex(text)
}

fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Body key codex carries the per-installation id under, inside the
/// top-level `client_metadata` object. Same spelling as the HTTP header
/// name -- codex reuses the header spelling as the body key.
const INSTALLATION_ID_METADATA_KEY: &str = "x-codex-installation-id";

/// Stamp the resolved per-installation id into the request body's
/// `client_metadata` object, creating that object when absent.
///
/// Gated to `ChatgptOauth`: the codex backend is the only surface that
/// reads this key, and the ApiKey / BedrockMantle lanes must carry no
/// codex fingerprint at all.
///
/// Must run AFTER `merge_provider_extras`, and the RESOLVED id wins
/// unconditionally over whatever `client_metadata` carries at that point.
/// That object is client-reachable: an inbound body's top-level
/// `client_metadata` is swept into `provider_extras` and copied onto the
/// request, so honoring a pre-existing value would let request bytes spoof
/// (or, when non-object, suppress) the fingerprint routectl presents
/// upstream. A genuine operator override belongs in a config surface such
/// as `header_extras`, never in request bytes -- hence a non-object
/// `client_metadata` is replaced rather than left in place.
pub(super) fn apply_installation_id(
    request: &mut ResponsesRequest,
    auth_kind: AuthKind,
    installation_id: Option<&str>,
    tap: Option<&WireTap>,
) {
    if !auth_kind.is_codex_lane() {
        return;
    }
    let Some(iid) = installation_id else {
        return;
    };
    if let Some(tap) = tap {
        declare_installation_id(tap, request.client_metadata.as_ref());
    }
    let stamped = Value::String(iid.to_string());
    match request
        .client_metadata
        .as_mut()
        .and_then(Value::as_object_mut)
    {
        Some(obj) => {
            obj.insert(INSTALLATION_ID_METADATA_KEY.into(), stamped);
        }
        None => {
            let mut obj = Map::new();
            obj.insert(INSTALLATION_ID_METADATA_KEY.into(), stamped);
            request.client_metadata = Some(Value::Object(obj));
        }
    }
}

/// Declare the installation-id stamp against the `client_metadata` it lands
/// on: a new key in an object the request carried, otherwise a whole new
/// object (or a rewrite of a non-object value). Keys under `client_metadata`
/// render masked, so the stamped member is named by the wildcard: a `*`
/// pattern segment matches a masked key, while a concrete key segment only
/// ever matches a verbatim one, so `/client_metadata/<key>` could never match
/// the observed path. Naming the key concretely would need a masking
/// exemption for routectl-written keys.
fn declare_installation_id(tap: &WireTap, client_metadata: Option<&Value>) {
    let site = EditSite::InstallationIdStamp;
    match client_metadata {
        Some(Value::Object(obj)) => {
            let path = "/client_metadata/*";
            let kind = if obj.contains_key(INSTALLATION_ID_METADATA_KEY) {
                EditKind::Rewrite
            } else {
                EditKind::Add
            };
            declare(tap, site, path, kind);
        }
        Some(_) => declare(tap, site, "/client_metadata", EditKind::Rewrite),
        None => declare(tap, site, "/client_metadata", EditKind::Add),
    }
}

/// Honor the canonical structured-output directive by mapping
/// `req.response_format` (OpenAI Chat-Completions shape) onto the Responses
/// API `text.format` field. This closes the same-protocol round-trip: the
/// Responses ingress parses inbound `text.format` INTO `req.response_format`
/// (saving the remainder of `text`, e.g. `verbosity`, into
/// `provider_extras["text"]`), so the egress must re-emit it or strict JSON
/// decode fails.
///
///   `{type:json_schema, json_schema:{schema, name?, ...}}`
///       -> `text.format = {type:json_schema, name, schema, ...}`
///   `{type:json_object}` -> `text.format = {type:json_object}`
///
/// Runs AFTER `merge_provider_extras`, so a `verbosity` sibling lifted into
/// `provider_extras["text"]` survives and the format is merged alongside it.
/// A caller-supplied `text.format` (already present) is left untouched.
pub(super) fn apply_response_format(
    request: &mut ResponsesRequest,
    req: &ChatRequest,
    tap: Option<&WireTap>,
) {
    let Some(rf) = req.response_format.as_ref() else {
        return;
    };
    if let Some(tap) = tap
        && json_schema_name_defaults(rf)
        && request
            .text
            .as_ref()
            .is_none_or(|tc| tc.inner.get("format").is_none())
    {
        declare(
            tap,
            EditSite::RequiredFieldDefault,
            "/text/format/name",
            EditKind::Add,
        );
    }
    // The tally is created and flushed inside this same infallible function,
    // so the flush cannot be stranded behind a `?`: every request that can
    // record a loss here also reaches the flush.
    let mut tally = ResponseFormatDropTally::default();
    let format = responses_text_format(rf, &mut tally);
    tally.flush();
    let Some(format) = format else {
        return;
    };
    match request.text.as_mut() {
        Some(tc) => {
            if let Some(obj) = tc.inner.as_object_mut() {
                obj.entry("format").or_insert(format);
            }
        }
        None => {
            request.text = Some(TextControls {
                inner: serde_json::json!({ "format": format }),
            });
        }
    }
}

/// True when `response_format` is a usable `json_schema` directive whose
/// `name` [`responses_text_format`] fills with the default.
fn json_schema_name_defaults(response_format: &Value) -> bool {
    response_format.get("type").and_then(Value::as_str) == Some("json_schema")
        && response_format
            .get("json_schema")
            .and_then(Value::as_object)
            .is_some_and(|js| {
                js.contains_key("schema") && !js.get("name").is_some_and(Value::is_string)
            })
}

/// Per-REQUEST tally for the structured-output directive's drops on this
/// lane.
///
/// Each field is a per-request FLAG, not an occurrence count: a request
/// carries exactly one `response_format`, so its four failing arms are one
/// request's worth of loss for whichever class fired -- never four events.
/// The two classes are the two distinct operator problems (a directive whose
/// envelope cannot be read, and a `json_schema` entry carrying no schema).
/// An unrecognized type token is not a class here: this lane forwards it.
///
/// The denominator is NOT touched here: `request::translate` owns the
/// single `record_translation_lane_seen` site for this lane, and a second
/// would understate the rate for the whole lane.
#[derive(Default)]
#[must_use = "a tally records nothing until flush() runs"]
struct ResponseFormatDropTally {
    shape_unrepresentable: bool,
    schema_missing: bool,
}

impl ResponseFormatDropTally {
    /// Record a directive whose envelope this egress cannot read at all --
    /// not an object, or carrying no string `type`.
    const fn record_shape_unrepresentable(&mut self) {
        self.shape_unrepresentable = true;
    }

    /// Record a `json_schema` directive carrying no usable schema.
    const fn record_schema_missing(&mut self) {
        self.schema_missing = true;
    }

    fn flush(self) {
        if self.shape_unrepresentable {
            record_translation_drop(super::LANE, "response_format_shape_unrepresentable");
        }
        if self.schema_missing {
            record_translation_drop(super::LANE, "response_format_schema_missing");
        }
    }
}

/// Convert the canonical OpenAI Chat-shape `response_format` into the
/// Responses API `text.format` object (flattened: the whole `json_schema`
/// member's keys at the top level, not nested). Returns `None` for an
/// unusable shape. The Responses API requires `name` on a
/// json_schema format, so a missing name defaults to `"response"` (matching
/// the openai-compat wire-lift default).
///
/// The flatten carries the WHOLE member out rather than a named
/// `name`/`schema`/`strict` triple, making it the exact inverse of the
/// Responses ingress nesting (`ingress/openai_responses/parse.rs`). Exact
/// inverses is what makes the same-dialect round trip closed for every key
/// rather than closed for three of them.
///
/// Every `None` exit loses the caller's structured-output request outright:
/// the model answers in free-form prose while the client parses for JSON. So
/// each warns here, where the offending shape is in hand, and records its
/// class on the caller's per-request tally.
fn responses_text_format(
    response_format: &Value,
    tally: &mut ResponseFormatDropTally,
) -> Option<Value> {
    // A `response_format` that is not an object carries no `type` and no
    // schema, so the Responses `text.format` member -- an object tagged with
    // a `type` -- has no shape it could become, and the upstream rejects a
    // bare scalar there. Lane: openai-responses, construction-time
    // translation. Baked seed verdict: it stands until this lane's own wire
    // evidence contradicts it, and is not eligible for deletion until then.
    // TRANSLATION-DROP: lane=openai-responses class=response_format_shape_unrepresentable test=responses_non_object_response_format_drops_and_counts_once
    let Some(obj) = response_format.as_object() else {
        tally.record_shape_unrepresentable();
        tracing::warn!(
            "response_format is not an object; dropping structured-output \
             directive on Responses egress"
        );
        return None;
    };
    // Same class, second arm: an object with no string `type` names no format
    // member, and `text.format` is a tagged union the upstream rejects
    // untagged.
    // TRANSLATION-DROP: lane=openai-responses class=response_format_shape_unrepresentable test=responses_response_format_without_a_type_token_drops_and_counts_once
    let Some(kind) = obj.get("type").and_then(Value::as_str) else {
        tally.record_shape_unrepresentable();
        tracing::warn!(
            "response_format carries no string type token; dropping \
             structured-output directive on Responses egress"
        );
        return None;
    };
    match kind {
        "json_schema" => {
            // A `json_schema` directive with no `json_schema` member has no
            // schema to carry, and the Responses `text.format` json_schema
            // member requires one -- emitting the envelope without it is
            // rejected upstream. Lane: openai-responses, construction-time
            // translation. Baked seed verdict: it stands until this lane's
            // own wire evidence contradicts it, and is not eligible for
            // deletion until then.
            // TRANSLATION-DROP: lane=openai-responses class=response_format_schema_missing test=responses_json_schema_without_a_schema_drops_and_counts_once
            let Some(js) = obj.get("json_schema").and_then(Value::as_object) else {
                tally.record_schema_missing();
                tracing::warn!(
                    "response_format json_schema is absent or not an object; \
                     dropping structured-output directive on Responses egress"
                );
                return None;
            };
            // Same class, second arm: the member is present but carries no
            // `schema`, so there is still nothing the upstream would accept.
            // TRANSLATION-DROP: lane=openai-responses class=response_format_schema_missing test=responses_json_schema_member_without_a_schema_drops_and_counts_once
            if !js.contains_key("schema") {
                tally.record_schema_missing();
                tracing::warn!(
                    "response_format json_schema carries no json_schema.schema; \
                     dropping structured-output directive on Responses egress"
                );
                return None;
            }
            let mut fmt = js.clone();
            fmt.insert("type".into(), Value::from("json_schema"));
            // Repairs a non-string `name` as well as an absent one: the
            // upstream requires a string here, and `entry()` alone would
            // forward a null or a number unchanged.
            if !fmt.get("name").is_some_and(Value::is_string) {
                fmt.insert("name".into(), Value::from("response"));
            }
            Some(Value::Object(fmt))
        }
        "json_object" => Some(serde_json::json!({"type": "json_object"})),
        // A DOCUMENTED Responses `text.format` member, and one this lane's own
        // ingress lifts verbatim -- so it arrives on a same-dialect request.
        // Re-emitted rather than dropped: it was previously read as an unknown
        // tag, which warned and counted a loss that never happened. A false
        // numerator entry is worse than a missing one, because it discredits the
        // metric for the drops that ARE real.
        "text" => Some(serde_json::json!({"type": "text"})),

        // A tag outside the vocabulary above -- most plausibly a newer
        // Responses `text.format` member this lane's own ingress lifted
        // verbatim. The upstream is the authority on its own union, so the
        // whole object is forwarded unchanged: a rejection surfaces as the
        // upstream's own error, whereas dropping it would silently return
        // prose to a client parsing for structured output.
        _ => Some(Value::Object(obj.clone())),
    }
}

/// The `include` entry that carries the encrypted reasoning blob back
/// on the wire. Required whenever `store == false`, otherwise the
/// upstream returns empty `encrypted_content` and a later reasoning
/// replay by item id is a no-op (chatgpt-oauth) or a 404 (api.openai.com).
const REASONING_ENCRYPTED_INCLUDE: &str = "reasoning.encrypted_content";

/// Ensure the request asks the server to echo back the encrypted
/// reasoning carrier when the response is not persisted.
///
/// When `store == false` the server only returns a usable
/// `encrypted_content` if `include` carries
/// `"reasoning.encrypted_content"`. We force it in UNLESS the operator
/// supplied an explicit `include` via `provider_extras` (their value is
/// then respected verbatim). When `store == true` the server retains
/// reasoning, so no `include` is forced.
///
/// Runs after `merge_provider_extras` so it reflects a provider_extras
/// override of `store`.
pub(super) fn finalize_reasoning_include(
    request: &mut ResponsesRequest,
    req: &ChatRequest,
    tap: Option<&WireTap>,
) {
    if request.store {
        return;
    }
    if operator_set_include(req) {
        return;
    }
    if request
        .include
        .iter()
        .any(|s| s == REASONING_ENCRYPTED_INCLUDE)
    {
        return;
    }
    if let Some(tap) = tap {
        declare(
            tap,
            EditSite::ReasoningIncludeInjection,
            "/include",
            EditKind::Add,
        );
    }
    request
        .include
        .push(REASONING_ENCRYPTED_INCLUDE.to_string());
}

/// Whether the operator explicitly supplied `include` via
/// `provider_extras` (an array value under the `include` key). An
/// explicit value -- even an empty array -- is honored as-is.
fn operator_set_include(req: &ChatRequest) -> bool {
    req.provider_extras
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("include"))
        .is_some_and(serde_json::Value::is_array)
}

/// Apply an operator-supplied `store` override. For `ChatgptOauth` (codex
/// parity) and `BedrockMantle` (the mantle Responses lane, which must never
/// persist) the value is IGNORED and `store` stays `false`; for other
/// auth_kinds the boolean is honored verbatim.
///
/// `req.provider_extras` is the FINAL merged value at dispatch (the router
/// deep-merges provider-level and model-level `payload_extras` into it), so
/// forcing `store` here catches a model-level `store = true` the
/// config-time provider-level reject cannot see. Combined with the `false`
/// default in `request.rs`, no origin of `store = true` survives on the
/// mantle lane.
///
/// Baked seed verdict: this provider kind sits outside field repair, and a
/// `store` refusal on `ChatgptOauth` can arrive as a status-0
/// `response.failed` after the stream has started, too late to retry
/// invisibly.
/// SEED-VERDICT: acceptance-seed -- the refusal can arrive mid-stream outside
/// field repair, so the forced value is a baked prior nothing can relearn.
fn apply_store_override(request: &mut ResponsesRequest, v: &Value, auth_kind: AuthKind) {
    if matches!(auth_kind, AuthKind::ChatgptOauth | AuthKind::BedrockMantle) {
        tracing::debug!(
            requested = ?v,
            ?auth_kind,
            "openai-responses: ignoring provider_extras.store (lane forces store=false)"
        );
        return;
    }
    if let Some(b) = v.as_bool() {
        request.store = b;
    }
}

#[cfg(test)]
mod response_format_drop_tests {
    use super::super::request::translate;
    use super::super::{AuthKind, OpenAiResponsesConfig};
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::{Value, json};

    fn cfg() -> OpenAiResponsesConfig {
        let mut c = OpenAiResponsesConfig::new("openai-responses:test", "literal:test");
        c.auth_kind = AuthKind::ChatgptOauth;
        c
    }

    /// A minimal request carrying the given structured-output directive plus
    /// one representable sibling field. The sibling is the positive control's
    /// survivor: `text.format` is what these arms lose, so a surviving
    /// `text.verbosity` proves the whole `text` object did not simply vanish.
    fn req_with(response_format: Option<Value>) -> ChatRequest {
        ChatRequest {
            model: "gpt-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("marker_user_turn_survives".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extras: serde_json::Map::new(),
            }]
            .into(),
            response_format,
            provider_extras: Some(json!({"text": {"verbosity": "low"}})),
            ..Default::default()
        }
    }

    fn drop_count(class: &str) -> u64 {
        crate::translation_drop_metrics::translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == "openai-responses" && e.drop_class == class)
            .map_or(0, |e| e.drop_count)
    }

    fn shape_count() -> u64 {
        drop_count("response_format_shape_unrepresentable")
    }

    fn schema_count() -> u64 {
        drop_count("response_format_schema_missing")
    }

    /// Translate under log capture and return the EMITTED WIRE BODY plus every
    /// captured event, so absence is asserted against the serialized request
    /// rather than the typed struct.
    fn emitted(response_format: Option<Value>) -> (Value, Vec<routectl_testkit::CapturedEvent>) {
        let request = req_with(response_format);
        let mut wire = Value::Null;
        let events = routectl_testkit::capture_events(|| {
            let translated = translate(&cfg(), &request).expect("translation ok");
            wire = serde_json::to_value(&translated).expect("body serializes");
        });
        (wire, events)
    }

    /// Assert the emitted body carries no `format` under `text`, and that the
    /// representable sibling survived beside the loss.
    fn assert_format_absent_sibling_survives(wire: &Value) {
        assert!(
            wire["text"].get("format").is_none(),
            "no format may be invented for an unusable directive; emitted: {wire}"
        );
        assert_eq!(
            wire["text"]["verbosity"], "low",
            "the representable text sibling must survive the format's loss; emitted: {wire}"
        );
        assert!(
            wire.to_string().contains("marker_user_turn_survives"),
            "the rest of the request must survive; emitted: {wire}"
        );
    }

    /// NEGATIVE CONTROL: a non-object directive drops, warns, and counts once
    /// on the shape class.
    #[test]
    fn responses_non_object_response_format_drops_and_counts_once() {
        // Arrange
        let before = shape_count();

        // Act
        let (wire, events) = emitted(Some(json!("json_object")));
        let after = shape_count();

        // Assert
        assert!(
            events.iter().any(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("response_format is not an object")
            }),
            "the drop must warn; got: {events:?}"
        );
        assert_format_absent_sibling_survives(&wire);
        assert_eq!(after - before, 1);
    }

    /// NEGATIVE CONTROL: an object with no string `type` token names no format
    /// member -- the same class as the non-object arm, so ONE request hitting
    /// it is one drop event on that class.
    #[test]
    fn responses_response_format_without_a_type_token_drops_and_counts_once() {
        // Arrange
        let before = shape_count();

        // Act
        let (wire, events) = emitted(Some(json!({"json_schema": {"schema": {}}})));
        let after = shape_count();

        // Assert
        assert!(
            events.iter().any(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("carries no string type token")
            }),
            "the drop must warn; got: {events:?}"
        );
        assert_format_absent_sibling_survives(&wire);
        assert_eq!(after - before, 1);
    }

    /// An unrecognized `type` token is forwarded as the WHOLE format object,
    /// sibling keys included, with no warning and no counted drop: the tag is
    /// the upstream's to judge, not this lane's to censor.
    #[test]
    fn responses_unrecognized_response_format_type_survives_verbatim() {
        // Arrange
        let directive = json!({
            "type": "marker_future_format_tag",
            "marker_future_member": {"nested": [1, "two"]},
            "strict": true
        });
        let before = (shape_count(), schema_count());

        // Act
        let (wire, events) = emitted(Some(directive.clone()));

        // Assert
        assert_eq!(
            wire["text"]["format"], directive,
            "the unknown tag's whole object must reach the wire; emitted: {wire}"
        );
        assert_eq!(
            wire["text"]["verbosity"], "low",
            "the text sibling must survive beside the forwarded format; emitted: {wire}"
        );
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "a forwarded tag is no loss and must not warn; got: {events:?}"
        );
        assert_eq!(
            (shape_count(), schema_count()),
            before,
            "a forwarded tag must count no drop"
        );
    }

    /// NEGATIVE CONTROL: a `json_schema` directive with no `json_schema`
    /// member drops and counts on the schema class.
    #[test]
    fn responses_json_schema_without_a_schema_drops_and_counts_once() {
        // Arrange
        let before = schema_count();

        // Act
        let (wire, events) = emitted(Some(json!({"type": "json_schema"})));
        let after = schema_count();

        // Assert
        assert!(
            events.iter().any(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("json_schema is absent or not an object")
            }),
            "the drop must warn; got: {events:?}"
        );
        assert_format_absent_sibling_survives(&wire);
        assert_eq!(after - before, 1);
    }

    /// NEGATIVE CONTROL: the member is present but carries no `schema` -- the
    /// same class, so this request is one drop event on it too.
    #[test]
    fn responses_json_schema_member_without_a_schema_drops_and_counts_once() {
        // Arrange
        let before = schema_count();

        // Act
        let (wire, events) = emitted(Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "marker_orphan_schema_name"}
        })));
        let after = schema_count();

        // Assert
        assert!(
            events.iter().any(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("carries no json_schema.schema")
            }),
            "the drop must warn; got: {events:?}"
        );
        assert_format_absent_sibling_survives(&wire);
        assert!(
            !wire.to_string().contains("marker_orphan_schema_name"),
            "no partial envelope may reach the wire; emitted: {wire}"
        );
        assert_eq!(after - before, 1);
    }

    /// POSITIVE CONTROL for every fixture above: a representable directive of
    /// each recognized type reaches the emitted body, warns not at all, and
    /// advances NONE of the counters. Without it the absence assertions
    /// would pass against an egress that dropped every directive.
    #[test]
    fn representable_response_formats_survive_and_advance_no_counter() {
        for directive in [
            json!({"type": "json_object"}),
            // A documented Responses `text.format` member the lane's own ingress
            // lifts verbatim. It was read as an unknown tag and counted as a
            // drop, so this fixture is the control that keeps it representable.
            json!({"type": "text"}),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "marker_representable_schema",
                    "schema": {"type": "object", "properties": {}}
                }
            }),
        ] {
            // Arrange
            let before = (shape_count(), schema_count());

            // Act
            let (wire, events) = emitted(Some(directive.clone()));

            // Assert
            assert!(
                !events.iter().any(|e| e.level == tracing::Level::WARN),
                "{directive} is representable and must not warn; got: {events:?}"
            );
            assert_eq!(
                wire["text"]["format"]["type"], directive["type"],
                "{directive} must reach the wire; emitted: {wire}"
            );
            assert_eq!(
                (shape_count(), schema_count()),
                before,
                "{directive} counted a drop"
            );
        }
    }
}
