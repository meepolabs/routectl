//! Contract tests for the OpenAI Responses egress.
//!
//! Mirrors the per-scenario structure of `contract_egress.rs` for the
//! `openai_responses` provider. Each scenario takes a canonical
//! `ChatRequest` from `common::scenarios` and snapshots the upstream
//! wire body that `OpenAiResponsesProvider::normalize_request`
//! produces. The snapshot files live under
//! `tests/snapshots/openai_responses/` and are reviewed with
//! `cargo insta review`.
//!
//! Three invariants these snapshots must hold (verified by inspection
//! of every snapshot landed by this file plus the paranoia check in
//! scenario 5):
//!
//!   - `instructions` field MUST appear in every snapshot (possibly as
//!     `""`). The chatgpt-oauth backend 400s with
//!     `{"detail":"Instructions are required"}` when the field is
//!     absent; a snapshot lacking the key is a regression.
//!   - Tools entries MUST use the flat Responses shape
//!     (`{type, name, description, parameters}`) NOT the nested
//!     chat-completions shape (`{type, function:{name,...}}`). The
//!     nested shape 400s with
//!     `"Missing required parameter: 'tools[0].name'"`.
//!   - `tool_choice` named-function entries MUST use the flat
//!     `{type:"function", name:"X"}` shape NOT the nested
//!     `{type:"function", function:{name:"X"}}` shape. The nested
//!     shape 400s with `"Unknown parameter: 'tool_choice.function'"`.

#![cfg(feature = "openai-responses")]

mod common;

use routectl_core::Provider;
use routectl_providers::openai_responses::{
    AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};

use common::scenarios;

// ---------------------------------------------------------------------
// Provider builder
// ---------------------------------------------------------------------

fn openai_responses_provider() -> OpenAiResponsesProvider {
    // Defaults from `OpenAiResponsesConfig::new`:
    //   - auth_kind = ChatgptOauth
    //   - base_url  = https://chatgpt.com/backend-api/codex
    //   - account_id = None (only required by the auth layer at request
    //     time; normalize_request does not consume it).
    // The id + api_key are placeholders; neither leaks into a snapshot
    // because the request body never carries auth material.
    OpenAiResponsesProvider::new(OpenAiResponsesConfig::new(
        "openai-responses-test",
        "test-key",
    ))
}

/// The api-key lane, which differs from the codex-oauth default above in
/// the fields the upstream accepts. Its own snapshot exists so a future
/// convergence of the two lanes fails a recorded wire body, not only a
/// unit test.
fn openai_responses_provider_api_key() -> OpenAiResponsesProvider {
    let mut cfg = OpenAiResponsesConfig::new("openai-responses-test", "test-key");
    cfg.auth_kind = AuthKind::ApiKey;
    OpenAiResponsesProvider::new(cfg)
}

/// The bedrock-mantle lane. It shares this egress with the two lanes above
/// and diverges only through auth-kind conditionals, so its own recorded
/// wire body is the only WHOLE-BODY contract snapshot for the lane -- unit
/// and runtime tests cover the individual conditionals, but nothing else
/// pins the complete shape against a conditional written correctly for
/// ApiKey and wrongly for Mantle.
///
/// No `cfg` gate: `AuthKind::BedrockMantle` is feature-unconditional and
/// `translate()` never reads the (bedrock-gated) `cfg.mantle` field.
fn openai_responses_provider_bedrock_mantle() -> OpenAiResponsesProvider {
    let mut cfg = OpenAiResponsesConfig::new("openai-responses-test", "test-key");
    cfg.auth_kind = AuthKind::BedrockMantle;
    OpenAiResponsesProvider::new(cfg)
}

// =====================================================================
// Scenario 1: system_handling
// =====================================================================
//
// Responses egress: the top-level canonical `system` text becomes the
// `instructions` string. Even when no system prompt is supplied the
// `instructions` field is serialized as `""` (the chatgpt-oauth
// backend 400s when the field is absent).

mod scenario_1_system_handling {
    use super::*;

    #[test]
    fn openai_responses_egress() {
        let req = scenarios::scenario_1_system_handling();
        let body = openai_responses_provider()
            .normalize_request(&req)
            .expect("openai_responses normalize");

        // Final path:
        //   snapshots/openai_responses/contract_egress_openai_responses__scenario_1_system_handling__request_body.snap
        insta::with_settings!({snapshot_path => "snapshots/openai_responses"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 2: tool_choice_translations
// =====================================================================
//
// Responses egress quirks (smoke 2026-05-12):
//   - tools entries are flat: {type, name, description, parameters, strict?}
//     NOT nested under a `function` object.
//   - tool_choice named-function is flat: {type:"function", name:"X"}
//     NOT nested {type:"function", function:{name:"X"}}.
//   - tool_choice bare strings ("auto" / "required" / "none") pass
//     through verbatim.
//   - tool_choice Anthropic-shape object `{type:"auto"}` collapses to
//     the bare string `"auto"`.

mod scenario_2_tool_choice_auto {
    use super::*;

    #[test]
    fn openai_responses_egress() {
        let req = scenarios::scenario_2_tool_choice_auto();
        let body = openai_responses_provider()
            .normalize_request(&req)
            .expect("openai_responses normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_responses"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

mod scenario_2_tool_choice_auto_anthropic_shape {
    use super::*;

    #[test]
    fn openai_responses_egress() {
        let req = scenarios::scenario_2_tool_choice_auto_anthropic_shape();
        let body = openai_responses_provider()
            .normalize_request(&req)
            .expect("openai_responses normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_responses"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

mod scenario_2_tool_choice_named_function {
    use super::*;

    #[test]
    fn openai_responses_egress() {
        let req = scenarios::scenario_2_tool_choice_named_function();
        let body = openai_responses_provider()
            .normalize_request(&req)
            .expect("openai_responses normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_responses"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 3: multi_turn_with_tool_result
// =====================================================================
//
// History serialization round-trip. Responses egress lowers each
// canonical message to one entry in the `input` array:
//   - Role::User text  -> {type:"message", role:"user",  content:[{type:"input_text", ...}]}
//   - Role::Assistant ToolUse part -> {type:"function_call", call_id, name, arguments}
//     (arguments is a JSON-encoded string, not an object -- Responses API quirk).
//   - Role::Tool       -> {type:"function_call_output", call_id, output}
// Tools serialize in the flat Responses shape (no nested `function` object).

mod scenario_3_multi_turn_with_tool_result {
    use super::*;

    #[test]
    fn openai_responses_egress() {
        let req = scenarios::scenario_3_multi_turn_with_tool_result();
        let body = openai_responses_provider()
            .normalize_request(&req)
            .expect("openai_responses normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_responses"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 4: stop_reason_round_trip
// =====================================================================
//
// Response-side scenario lives entirely in the cli crate's
// `contract_ingress` tests (mirroring the openai_compat skip in
// `contract_egress.rs`). No egress-side action runs here because
// `normalize_request` translates REQUESTS, not responses, and the
// Responses egress has no canonical-to-wire response-rendering surface.

// =====================================================================
// Scenario 5: cache_control_positions
// =====================================================================
//
// Responses egress: cache_control is Anthropic-only on the wire. Every
// position (top-level, system block, tool, message content block) must
// be silently dropped. Any leak would 400 on the chatgpt-oauth backend
// and break OpenAI-shape callers that pass cache_control through.
// The paranoia string check catches a regression that emits any
// `cache_control` field anywhere in the body.

mod scenario_5_cache_control_positions {
    use super::*;

    #[test]
    fn openai_responses_egress() {
        let req = scenarios::scenario_5_cache_control_positions();
        let body = openai_responses_provider()
            .normalize_request(&req)
            .expect("openai_responses normalize");

        // Paranoia: cache_control is Anthropic-only on the wire. The
        // Responses egress must drop every position silently; any leak
        // into the wire body is a regression. Check before snapshotting
        // so the failure mode is a clear assertion message rather than
        // a noisy snapshot diff.
        let body_str = body.to_string();
        assert!(
            !body_str.contains("cache_control"),
            "openai-responses egress must NOT emit `cache_control` anywhere; body: {body_str}"
        );

        insta::with_settings!({snapshot_path => "snapshots/openai_responses"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 6: max_output_tokens_api_key_lane
// =====================================================================
//
// The api-key lane accepts `max_output_tokens` as a documented top-level
// field, so the caller's ceiling MUST reach the wire body there. The
// codex-oauth snapshots elsewhere in this file correctly omit it (codex's
// request shape has no such member). Snapshotting the api-key body pins
// the retained field so a future cross-lane convergence -- one that
// suppressed it here too -- fails a recorded wire body, not only a unit
// test. The reused fixture carries `max_tokens: Some(1024)`.

mod scenario_6_max_output_tokens_api_key_lane {
    use super::*;

    #[test]
    fn openai_responses_egress() {
        let req = scenarios::scenario_1_system_handling();
        let body = openai_responses_provider_api_key()
            .normalize_request(&req)
            .expect("openai_responses normalize");

        // Pin: the caller's ceiling survives to the api-key wire body.
        assert_eq!(
            body.get("max_output_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(1024),
            "api-key lane must retain max_output_tokens; body: {body}"
        );

        insta::with_settings!({snapshot_path => "snapshots/openai_responses"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 7: store_lock_bedrock_mantle_lane
// =====================================================================
//
// The mantle Responses lane must never persist: `store` is forced false
// regardless of an operator- or model-supplied override, and because
// `store` stays false the encrypted-reasoning `include` carrier is forced
// on so a later reasoning replay has a blob to send. (The carrier is
// forced only when the caller supplies no explicit `include`; an explicit
// one is honored verbatim. This fixture supplies none.)
//
// The fixture requests `store: true` deliberately. Over a bare scenario-1
// request nothing in this body would depend on the auth kind at all, so
// the recording could not fail for a Mantle-specific reason. The override
// puts both lane conditionals in play: dropping the lane from the store
// lock flips `store` to true and drops `include`, and excluding the lane
// from the token passthrough drops `max_output_tokens` -- each a snapshot
// diff.
//
// The recorded body is byte-identical to the api-key scenario above, and
// that is the point rather than a defect: the api-key fixture carries no
// override, so its honored-`store` path and this lane's forced-`store`
// path coincide. The bytes match; the reasons do not, and only this
// recording pins the forced one.

mod scenario_7_store_lock_bedrock_mantle_lane {
    use super::*;

    #[test]
    fn openai_responses_egress() {
        let mut req = scenarios::scenario_1_system_handling();
        req.provider_extras = Some(serde_json::json!({"store": true}));

        let body = openai_responses_provider_bedrock_mantle()
            .normalize_request(&req)
            .expect("openai_responses normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_responses"}, {
            insta::assert_json_snapshot!("request_body", body);
        });

        // Pins run AFTER the snapshot so a perturbation of the lane
        // conditionals proves the recorded body fails, not merely a
        // neighbouring assertion. They also still fail if a wrong
        // snapshot is ever accepted.

        // Pin: the caller's ceiling survives to the mantle wire body.
        assert_eq!(
            body.get("max_output_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(1024),
            "mantle lane must retain max_output_tokens; body: {body}"
        );

        // Pin: the requested store override is inert on this lane.
        assert_eq!(
            body.get("store").and_then(serde_json::Value::as_bool),
            Some(false),
            "mantle lane must force store=false despite provider_extras.store=true; body: {body}"
        );

        // Pin: store=false keeps the encrypted-reasoning carrier on the wire.
        assert!(
            body.get("include")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|a| a
                    .iter()
                    .any(|v| v.as_str() == Some("reasoning.encrypted_content"))),
            "mantle lane must include the encrypted-reasoning carrier; body: {body}"
        );
    }
}
