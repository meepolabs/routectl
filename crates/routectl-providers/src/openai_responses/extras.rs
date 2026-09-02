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
//! provider_extras allowlist (6 keys): `prompt_cache_key`,
//! `service_tier`, `text`, `include`, `store`, `client_metadata`.
//! Anything else stays unforwarded -- matches the discipline in
//! `routectl-core::is_canonical_request_key`.
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

use serde_json::{Map, Value};

use routectl_core::ChatRequest;

use super::AuthKind;
use super::types::{ResponsesReasoning, ResponsesRequest, TextControls};
use crate::effort::{clamp_effort_to_supported, level_from_budget};
use crate::translation_drop_metrics::record_translation_drop;

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
pub(super) fn apply_reasoning(request: &mut ResponsesRequest, req: &ChatRequest) {
    let overlay = responses_reasoning_overlay(req);

    // An explicit `effort: "none"` is a reasoning-OFF request; treat it like
    // `enabled: false` and omit reasoning entirely rather than emit a
    // reasoning object (which would leave thinking ON via a budget or a
    // summary/context overlay).
    let effort_disabled = req
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.as_deref())
        .is_some_and(|e| e == "none");
    if effort_disabled {
        return;
    }

    let (effort, enabled, budget) = match req.reasoning.as_ref() {
        Some(r) => {
            // Explicit effort wins. When no effort is set but a budget is,
            // map the budget to the nearest effort band (the Responses API
            // takes effort, not a budget) rather than dropping it. A clamp
            // that returns `None` (reasoning-OFF) leaves effort unset.
            let effort = match r.effort.as_deref() {
                Some(e) => clamp_effort_to_supported(e, &req.routectl_internal.effort_levels)
                    .map(std::borrow::Cow::into_owned),
                None => r.max_tokens.and_then(|budget| {
                    let level = level_from_budget(budget);
                    clamp_effort_to_supported(level, &req.routectl_internal.effort_levels)
                        .map(std::borrow::Cow::into_owned)
                }),
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
        None => Some("auto".into()),
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
/// Only the 6 allowed keys are honored; everything else is left
/// unforwarded so an operator-supplied long-tail field doesn't slip
/// through unaudited. The `store` flag is special-cased: for
/// `ChatgptOauth`, it stays hardcoded to `false` regardless of any
/// `provider_extras["store"]` value.
pub(super) fn merge_provider_extras(
    request: &mut ResponsesRequest,
    req: &ChatRequest,
    auth_kind: AuthKind,
) {
    let Some(extras) = req.provider_extras.as_ref().and_then(|v| v.as_object()) else {
        return;
    };

    for (k, v) in extras {
        match k.as_str() {
            "prompt_cache_key" => {
                if let Some(s) = v.as_str() {
                    request.prompt_cache_key = Some(s.to_string());
                }
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
            // Any other key stays unforwarded. The discipline matches
            // routectl-core::is_canonical_request_key -- if the
            // Responses API grows a new top-level field, we add it
            // here explicitly rather than silently passing through.
            //
            // No caller content is lost on this arm for the same-dialect
            // pairing. The Responses ingress sweeps every unhandled
            // top-level body field into `provider_extras`, so an inbound
            // key outside this allowlist was outside the Responses API's
            // own field set too -- forwarding it would put an unaudited
            // operator- or client-supplied key on the wire, which is the
            // failure this allowlist exists to prevent, not a fidelity
            // loss it causes. A recognized field the API grows later is a
            // code edit here, and the pinned allowlist is what makes that
            // edit a review moment rather than a silent passthrough.
            // TRANSLATION-DROP: structural -- the allowlist is the audited forwarding surface; an unlisted key was never a Responses API field
            _ => {}
        }
    }
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
) {
    if auth_kind != AuthKind::ChatgptOauth {
        return;
    }
    let Some(iid) = installation_id else {
        return;
    };
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

/// Honor the canonical structured-output directive by mapping
/// `req.response_format` (OpenAI Chat-Completions shape) onto the Responses
/// API `text.format` field. This closes the same-protocol round-trip: the
/// Responses ingress parses inbound `text.format` INTO `req.response_format`
/// (saving the remainder of `text`, e.g. `verbosity`, into
/// `provider_extras["text"]`), so the egress must re-emit it or strict JSON
/// decode fails.
///
///   `{type:json_schema, json_schema:{schema, name?, strict?}}`
///       -> `text.format = {type:json_schema, name, schema, strict?}`
///   `{type:json_object}` -> `text.format = {type:json_object}`
///
/// Runs AFTER `merge_provider_extras`, so a `verbosity` sibling lifted into
/// `provider_extras["text"]` survives and the format is merged alongside it.
/// A caller-supplied `text.format` (already present) is left untouched.
pub(super) fn apply_response_format(request: &mut ResponsesRequest, req: &ChatRequest) {
    let Some(rf) = req.response_format.as_ref() else {
        return;
    };
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

/// Per-REQUEST tally for the structured-output directive's drops on this
/// lane.
///
/// Each field is a per-request FLAG, not an occurrence count: a request
/// carries exactly one `response_format`, so its five failing arms are one
/// request's worth of loss for whichever class fired -- never five events.
/// The three classes are the three distinct operator problems (a directive
/// whose envelope cannot be read, a type token with no Responses spelling,
/// and a `json_schema` entry carrying no schema), which is also how the
/// openai-compat lift splits the same surface.
///
/// The denominator is NOT touched here: `request::translate` owns the
/// single `record_translation_lane_seen` site for this lane, and a second
/// would understate the rate for the whole lane.
#[derive(Default)]
#[must_use = "a tally records nothing until flush() runs"]
struct ResponseFormatDropTally {
    shape_unrepresentable: bool,
    type_unrepresentable: bool,
    schema_missing: bool,
}

impl ResponseFormatDropTally {
    /// Record a directive whose envelope this egress cannot read at all --
    /// not an object, or carrying no string `type`.
    const fn record_shape_unrepresentable(&mut self) {
        self.shape_unrepresentable = true;
    }

    /// Record a directive whose `type` token has no Responses `text.format`
    /// spelling.
    const fn record_type_unrepresentable(&mut self) {
        self.type_unrepresentable = true;
    }

    /// Record a `json_schema` directive carrying no usable schema.
    const fn record_schema_missing(&mut self) {
        self.schema_missing = true;
    }

    fn flush(self) {
        if self.shape_unrepresentable {
            record_translation_drop("openai-responses", "response_format_shape_unrepresentable");
        }
        if self.type_unrepresentable {
            record_translation_drop("openai-responses", "response_format_type_unrepresentable");
        }
        if self.schema_missing {
            record_translation_drop("openai-responses", "response_format_schema_missing");
        }
    }
}

/// Convert the canonical OpenAI Chat-shape `response_format` into the
/// Responses API `text.format` object (flattened: `name`/`schema`/`strict`
/// at the top level, not nested under `json_schema`). Returns `None` for an
/// absent or unrecognized shape. The Responses API requires `name` on a
/// json_schema format, so a missing name defaults to `"response"` (matching
/// the openai-compat wire-lift default).
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
            let Some(schema) = js.get("schema").cloned() else {
                tally.record_schema_missing();
                tracing::warn!(
                    "response_format json_schema carries no json_schema.schema; \
                     dropping structured-output directive on Responses egress"
                );
                return None;
            };
            let name = js.get("name").and_then(Value::as_str).unwrap_or("response");
            let mut fmt = serde_json::Map::new();
            fmt.insert("type".into(), Value::from("json_schema"));
            fmt.insert("name".into(), Value::from(name));
            fmt.insert("schema".into(), schema);
            if js.get("strict").and_then(Value::as_bool) == Some(true) {
                fmt.insert("strict".into(), Value::Bool(true));
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

        // A type token from neither dialect's known vocabulary (a future
        // OpenAI format, or a client typo). The Responses `text.format` union
        // admits only the tags handled above, so routectl cannot know which
        // member an unknown tag was meant to become and the upstream rejects
        // it -- inventing a member would silently constrain the model's
        // output shape. Lane: openai-responses, construction-time
        // translation. Baked seed verdict: it stands until this lane's own
        // wire evidence contradicts it, and is not eligible for deletion
        // until then.
        // TRANSLATION-DROP: lane=openai-responses class=response_format_type_unrepresentable test=responses_unrecognized_response_format_type_drops_and_counts_once
        other => {
            tally.record_type_unrepresentable();
            tracing::warn!(
                response_format_type = other,
                "unrecognized response_format shape; dropping structured-output \
                 directive on Responses egress"
            );
            None
        }
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
pub(super) fn finalize_reasoning_include(request: &mut ResponsesRequest, req: &ChatRequest) {
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

    fn type_count() -> u64 {
        drop_count("response_format_type_unrepresentable")
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
    #[serial_test::serial(openai_responses_response_format_shape_unrepresentable)]
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
    #[serial_test::serial(openai_responses_response_format_shape_unrepresentable)]
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

    /// NEGATIVE CONTROL: an unrecognized `type` token drops, warns naming the
    /// token, and counts once on its own class.
    #[test]
    #[serial_test::serial(openai_responses_response_format_type_unrepresentable)]
    fn responses_unrecognized_response_format_type_drops_and_counts_once() {
        // Arrange
        let before = type_count();

        // Act
        let (wire, events) = emitted(Some(json!({"type": "marker_future_format_tag"})));
        let after = type_count();

        // Assert
        let warn = events
            .iter()
            .find(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("unrecognized response_format shape")
            })
            .unwrap_or_else(|| panic!("the drop must warn; got: {events:?}"));
        assert_eq!(
            warn.field("response_format_type"),
            Some("marker_future_format_tag")
        );
        assert_format_absent_sibling_survives(&wire);
        assert!(
            !wire.to_string().contains("marker_future_format_tag"),
            "no trace of the dropped tag may reach the wire; emitted: {wire}"
        );
        assert_eq!(after - before, 1);
    }

    /// NEGATIVE CONTROL: a `json_schema` directive with no `json_schema`
    /// member drops and counts on the schema class.
    #[test]
    #[serial_test::serial(openai_responses_response_format_schema_missing)]
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
    #[serial_test::serial(openai_responses_response_format_schema_missing)]
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
    /// advances NONE of the three counters. Without it the absence assertions
    /// would pass against an egress that dropped every directive.
    #[test]
    #[serial_test::serial(
        openai_responses_response_format_schema_missing,
        openai_responses_response_format_shape_unrepresentable,
        openai_responses_response_format_type_unrepresentable
    )]
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
            let before = (shape_count(), type_count(), schema_count());

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
                (shape_count(), type_count(), schema_count()),
                before,
                "{directive} counted a drop"
            );
        }
    }
}
