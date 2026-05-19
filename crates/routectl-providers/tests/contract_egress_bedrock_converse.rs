//! Contract tests for the Bedrock-Converse egress.
//!
//! Mirrors `contract_egress.rs`: feeds the shared canonical
//! `ChatRequest` fixtures from `common::scenarios` into
//! `BedrockProvider::normalize_request` with `api_shape =
//! BedrockApiShape::Converse` and snapshots the upstream wire body.
//!
//! Bedrock-Converse is AWS's vendor-neutral envelope; the body shape is
//! fundamentally different from the Anthropic Messages shape used by
//! `Invoke`: `messages` carry typed content blocks, `system` is an
//! array of typed blocks, inference parameters live in
//! `inferenceConfig` (camelCase), tools live under
//! `toolConfig.{tools, toolChoice}`, and Anthropic-specific knobs
//! (`anthropic_beta`, `cache_control`, `thinking`) land in
//! `additionalModelRequestFields`. Cache breakpoints become inline
//! `{cachePoint}` blocks rather than per-block `cache_control` markers.
//!
//! Snapshots land under `tests/snapshots/bedrock_converse/`. Review with
//! `cargo insta review` after first run or after intentional behavior
//! changes.

#![cfg(feature = "bedrock")]

mod common;

use routectl_core::Provider;
use routectl_providers::bedrock::{
    auth::ResolvedCreds, BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider,
};

use common::scenarios;

// ---------------------------------------------------------------------
// Provider builder
// ---------------------------------------------------------------------

/// Construct a `BedrockProvider` wired for the Converse shape with a
/// bearer-key auth handle. The bearer-key path skips SigV4 entirely;
/// `normalize_request` does not touch the resolved creds (signing
/// happens in `signing::apply_auth` later in the pipeline), so the
/// dummy key here is purely structural -- it never crosses any wire in
/// these tests.
///
/// `allowed_betas` and `allowed_body_fields` are intentionally empty
/// (pass-through, the "discovery default" per `BedrockConfig` doc) so
/// the scenarios exercise the unfiltered translator. The filter
/// surfaces have dedicated coverage in
/// `bedrock/converse/request_tests*.rs` and
/// `bedrock/{betas,body_fields}.rs::tests`.
fn bedrock_converse_provider() -> BedrockProvider {
    let cfg = BedrockConfig {
        id: "bedrock-converse-test".into(),
        region: "us-east-1".into(),
        model_id: "us.anthropic.claude-3-opus-20240229-v1:0".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey {
            key: "test-key".into(),
        },
        user_agent: None,
        extra_headers: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: Vec::new(),
        allowed_body_fields: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = ResolvedCreds::Bearer {
        key: "test-key".into(),
    };
    BedrockProvider::new(cfg, resolved)
}

// =====================================================================
// Scenario 1: system_handling
// =====================================================================
//
// Bedrock-Converse: `system` becomes an array of typed blocks --
// `[{"text": "..."}]` -- even when the canonical input is a flat
// `SystemContent::Text`. The single user turn becomes a `messages`
// entry whose `content` is a typed-block array `[{"text": "Hello!"}]`.
// `max_tokens` migrates from the top level into the camelCase
// `inferenceConfig.maxTokens`.

mod scenario_1_system_handling {
    use super::*;

    #[test]
    fn bedrock_converse_egress() {
        let req = scenarios::scenario_1_system_handling();
        let body = bedrock_converse_provider()
            .normalize_request(&req)
            .expect("bedrock_converse normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_converse"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 2: tool_choice_translations
// =====================================================================
//
// Bedrock-Converse: tools live under `toolConfig.tools` as `[{toolSpec:
// {name, description, inputSchema: {json: ...}}}]`. `tool_choice` maps
// onto AWS's tagged union -- `{auto: {}}` / `{any: {}}` / `{tool:
// {name}}`. The Converse translator accepts the bare-string OpenAI
// shape ("auto"), the Anthropic-object shape (`{"type": "auto"}`), and
// the OpenAI-object shape (`{"type": "function", "function": {"name":
// "..."}}`) on the canonical side and produces the AWS-shape on the
// wire. AWS rejects every other form.

mod scenario_2_tool_choice_auto {
    use super::*;

    #[test]
    fn bedrock_converse_egress() {
        let req = scenarios::scenario_2_tool_choice_auto();
        let body = bedrock_converse_provider()
            .normalize_request(&req)
            .expect("bedrock_converse normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_converse"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

mod scenario_2_tool_choice_auto_anthropic_shape {
    use super::*;

    #[test]
    fn bedrock_converse_egress() {
        let req = scenarios::scenario_2_tool_choice_auto_anthropic_shape();
        let body = bedrock_converse_provider()
            .normalize_request(&req)
            .expect("bedrock_converse normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_converse"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

mod scenario_2_tool_choice_named_function {
    use super::*;

    #[test]
    fn bedrock_converse_egress() {
        let req = scenarios::scenario_2_tool_choice_named_function();
        let body = bedrock_converse_provider()
            .normalize_request(&req)
            .expect("bedrock_converse normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_converse"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 3: multi_turn_with_tool_result
// =====================================================================
//
// History serialization round-trip. Bedrock-Converse: the assistant's
// `ToolUse` part becomes a `{toolUse: {toolUseId, name, input}}`
// content block; the canonical `Role::Tool` turn becomes a synthetic
// user-role message carrying a `{toolResult: {toolUseId, content: [{text}],
// status?}}` block (AWS does not have a `tool` role -- tool results
// always ride inside a user turn).

mod scenario_3_multi_turn_with_tool_result {
    use super::*;

    #[test]
    fn bedrock_converse_egress() {
        let req = scenarios::scenario_3_multi_turn_with_tool_result();
        let body = bedrock_converse_provider()
            .normalize_request(&req)
            .expect("bedrock_converse normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_converse"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 4: stop_reason_round_trip
// =====================================================================
//
// Response-side scenario. The Anthropic ingress's `render_response` is
// the response-side analog of egress `normalize_request`, and that
// surface is covered in the cli crate's `contract_ingress` tests. The
// Converse egress translates REQUESTS, not responses, so there is no
// egress-side action to snapshot here. Skipped by design.

// =====================================================================
// Scenario 5: cache_control_positions
// =====================================================================
//
// Bedrock-Converse uses a fundamentally different cache-marker wire
// shape than Anthropic Messages: instead of per-block `cache_control`
// fields, AWS uses inline `{cachePoint: {type: "default", ttl?}}`
// blocks that follow the cached block. Expect:
//
//   - System: a `[{text}, {cachePoint}]` pair in the top-level
//     `system` array.
//   - Tools: a `[{toolSpec}, {cachePoint}]` pair in `toolConfig.tools`.
//   - Messages: each user / assistant content block whose canonical
//     part carried a `cache_control` marker is followed immediately by
//     a sibling `{cachePoint}` block.
//   - Top-level: `cache_control` lands in
//     `additionalModelRequestFields.cache_control` verbatim so AWS can
//     forward it to Anthropic-on-Bedrock.
//
// No paranoia string check here (unlike the openai-compat egress);
// Converse legitimately emits `cache_control` inside the
// `additionalModelRequestFields` bag and a substring assertion would
// false-positive against that intentional payload.

mod scenario_5_cache_control_positions {
    use super::*;

    #[test]
    fn bedrock_converse_egress() {
        let req = scenarios::scenario_5_cache_control_positions();
        let body = bedrock_converse_provider()
            .normalize_request(&req)
            .expect("bedrock_converse normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_converse"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}
