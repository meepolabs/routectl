//! Contract tests for the egress layer.
//!
//! Each scenario takes a canonical `ChatRequest` from
//! `common::scenarios` and snapshots the upstream wire body that each
//! provider's `normalize_request` produces. The snapshot files live
//! under `tests/snapshots/` and are reviewed with `cargo insta
//! review`.
//!
//! Per-provider snapshot subdirectories keep `cargo insta review`
//! filterable as more providers and scenarios land. See the sibling
//! `contract_ingress` tests in `routectl-cli` for the
//! wire-body-to-canonical half.

#![cfg(all(feature = "anthropic-api", feature = "openai-compat"))]

mod common;

use routectl_core::Provider;
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};

use common::scenarios;

// ---------------------------------------------------------------------
// Provider builders
// ---------------------------------------------------------------------

fn anthropic_api_provider() -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "anthropic-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        adaptive_thinking: None,
        allowed_betas: Vec::new(),
    })
}

fn openai_compat_provider() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "openai-compat-test".into(),
        base_url: "https://api.openai.com/v1".into(),
        api_key: "test-key".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
    })
}

// =====================================================================
// Scenario 1: system_handling
// =====================================================================
//
// Anthropic egress: emit top-level `system: "..."` and a single user
// message. OpenAI egress: prepend a `role: "system"` message; no
// top-level `system` field (strict openai-compat hosts 400 on it).

mod scenario_1_system_handling {
    use super::*;

    #[test]
    fn anthropic_api_egress() {
        let req = scenarios::scenario_1_system_handling();
        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        // Snapshot name `request_body` is stable across scenarios; the
        // per-provider subdirectory disambiguates and the scenario is
        // already in the enclosing mod name. Final path:
        //   snapshots/anthropic_api/contract_egress__scenario_1_system_handling__request_body.snap
        insta::with_settings!({snapshot_path => "snapshots/anthropic_api"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }

    #[test]
    fn openai_compat_egress() {
        let req = scenarios::scenario_1_system_handling();
        let body = openai_compat_provider()
            .normalize_request(&req)
            .expect("openai_compat normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_compat"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 2: tool_choice_translations
// =====================================================================
//
// Anthropic egress: `translate_tool_choice` rewrites bare-string
// "auto" to `{"type":"auto"}` (Bedrock 400s otherwise) and the
// OpenAI-shape `{"type":"function","function":{"name":X}}` to
// `{"type":"tool","name":X}`. OpenAI-compat egress: passthrough for
// the bare-string form; `wire_lift::tool_choice` preserves the
// OpenAI-shape function pointer as-is.

mod scenario_2_tool_choice_auto {
    use super::*;

    #[test]
    fn anthropic_api_egress() {
        let req = scenarios::scenario_2_tool_choice_auto();
        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        insta::with_settings!({snapshot_path => "snapshots/anthropic_api"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }

    #[test]
    fn openai_compat_egress() {
        let req = scenarios::scenario_2_tool_choice_auto();
        let body = openai_compat_provider()
            .normalize_request(&req)
            .expect("openai_compat normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_compat"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

mod scenario_2_tool_choice_auto_anthropic_shape {
    use super::*;

    #[test]
    fn anthropic_api_egress() {
        let req = scenarios::scenario_2_tool_choice_auto_anthropic_shape();
        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        insta::with_settings!({snapshot_path => "snapshots/anthropic_api"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }

    #[test]
    fn openai_compat_egress() {
        let req = scenarios::scenario_2_tool_choice_auto_anthropic_shape();
        let body = openai_compat_provider()
            .normalize_request(&req)
            .expect("openai_compat normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_compat"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

mod scenario_2_tool_choice_named_function {
    use super::*;

    #[test]
    fn anthropic_api_egress() {
        let req = scenarios::scenario_2_tool_choice_named_function();
        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        insta::with_settings!({snapshot_path => "snapshots/anthropic_api"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }

    #[test]
    fn openai_compat_egress() {
        let req = scenarios::scenario_2_tool_choice_named_function();
        let body = openai_compat_provider()
            .normalize_request(&req)
            .expect("openai_compat normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_compat"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 3: multi_turn_with_tool_result
// =====================================================================
//
// History serialization round-trip. Anthropic egress preserves typed
// blocks (tool_use + tool_result) on the wire; openai-compat lowers
// `Role::Tool` to a `role:"tool"` message and exposes the assistant's
// ToolUse content part as the canonical `tool_calls` shape.

mod scenario_3_multi_turn_with_tool_result {
    use super::*;

    #[test]
    fn anthropic_api_egress() {
        let req = scenarios::scenario_3_multi_turn_with_tool_result();
        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        insta::with_settings!({snapshot_path => "snapshots/anthropic_api"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }

    #[test]
    fn openai_compat_egress() {
        let req = scenarios::scenario_3_multi_turn_with_tool_result();
        let body = openai_compat_provider()
            .normalize_request(&req)
            .expect("openai_compat normalize");

        insta::with_settings!({snapshot_path => "snapshots/openai_compat"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 4: stop_reason_round_trip
// =====================================================================
//
// Response-side scenario lives entirely in the cli crate's
// `contract_ingress` tests (the Anthropic ingress's
// `render_response` is the response-side analog of egress
// `normalize_request`). No egress-side action runs here because the
// egress translates REQUESTS, not responses, and there is no
// canonical-to-Anthropic response-rendering surface on the egress.
//
// openai-compat is skipped end-to-end for scenario 4 because OpenAI
// has no `pause_turn` equivalent.

// =====================================================================
// Scenario 5: cache_control_positions
// =====================================================================
//
// Anthropic egress: cache_control survives on every position (top
// level, system block, tool, message content block). OpenAI-compat
// egress: every cache_control field is silently dropped (prompt
// caching not supported on the wire). The paranoia string check
// catches a regression that leaks any cache_control field into the
// openai-compat body.

// =====================================================================
// Scenario 10: reasoning_details_signature_replay
// =====================================================================
//
// Multi-turn history carries an assistant turn with Anthropic-shape
// reasoning_details (format `anthropic-claude-v1`, payload `{text,
// signature}`). The Anthropic egress MUST emit the assistant turn
// with a `thinking` content block carrying both the text and the
// signature -- Anthropic 400s on thinking blocks missing the
// signature field. The openai-compat egress drops reasoning_details
// (the wire shape has no thinking-block equivalent; reasoning travels
// on `reasoning_content` or `reasoning_details` on the message and
// the egress's history_reasoning policy decides whether to strip).

mod scenario_10_reasoning_details_signature_replay {
    use super::*;

    #[test]
    fn anthropic_api_egress() {
        let req = scenarios::scenario_10_reasoning_details_signature_replay();
        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        // Sanity-pin the assistant turn carries a `thinking` block
        // with a non-empty `signature` BEFORE the snapshot so a
        // regression that drops the signature fails with a clear
        // message rather than a generic snapshot diff. Bug class:
        // see CLAUDE.md "Anthropic streaming reasoning replay
        // residual" -- Anthropic 400s on Thinking blocks missing the
        // signature field, so the egress must preserve it on replay.
        let assistant = body["messages"][1].as_object().expect("assistant turn");
        assert_eq!(assistant["role"], "assistant");
        let parts = assistant["content"]
            .as_array()
            .expect("assistant content must be a typed-block array on replay");
        let thinking = parts
            .iter()
            .find(|p| p["type"] == "thinking")
            .expect("assistant turn must carry a `thinking` content block on replay");
        let signature = thinking["signature"]
            .as_str()
            .expect("thinking block must carry `signature` as a string");
        assert!(
            !signature.is_empty(),
            "thinking block signature must be non-empty (Anthropic 400s on empty/missing); thinking: {thinking}"
        );

        insta::with_settings!({snapshot_path => "snapshots/anthropic_api"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

mod scenario_5_cache_control_positions {
    use super::*;

    #[test]
    fn anthropic_api_egress() {
        let req = scenarios::scenario_5_cache_control_positions();
        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        insta::with_settings!({snapshot_path => "snapshots/anthropic_api"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }

    #[test]
    fn openai_compat_egress() {
        let req = scenarios::scenario_5_cache_control_positions();
        let body = openai_compat_provider()
            .normalize_request(&req)
            .expect("openai_compat normalize");

        // Paranoia: cache_control is Anthropic-only on the wire. The
        // openai-compat egress must drop every position silently;
        // any leak into the wire body would 400 strict hosts. Check
        // before snapshotting so a regression fails with a clear
        // message rather than a noisy snapshot diff.
        let body_str = body.to_string();
        assert!(
            !body_str.contains("cache_control"),
            "openai-compat egress must NOT emit `cache_control` anywhere; body: {body_str}"
        );

        insta::with_settings!({snapshot_path => "snapshots/openai_compat"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}
