//! Contract tests for the Bedrock-Invoke egress.
//!
//! Mirrors `contract_egress.rs` (anthropic_api + openai_compat) for the
//! Bedrock-Invoke shape. Bedrock-Invoke for Claude reuses
//! `anthropic_api::request::normalize` under the hood, then patches the
//! body with `anthropic_version: "bedrock-2023-05-31"` and any
//! configured beta flags / additional model request fields. The
//! resulting snapshots should be structurally very close to the
//! `snapshots/anthropic_api/` baselines, with the Bedrock-specific
//! `anthropic_version` body field swapped in and the `model` key
//! stripped (Bedrock takes the model id in the URL, not the body).
//!
//! Per-provider snapshot subdirectory keeps `cargo insta review`
//! filterable as more providers and scenarios land.

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

fn bedrock_invoke_provider() -> BedrockProvider {
    let cfg = BedrockConfig {
        id: "bedrock-invoke-test".into(),
        region: "us-east-1".into(),
        model_id: "anthropic.claude-3-opus-20240229-v1:0".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey {
            key: "test-key".into(),
        },
        user_agent: None,
        header_extras: Vec::new(),
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
// Bedrock-Invoke for Claude is Anthropic-shape passthrough with the
// `anthropic_version: "bedrock-2023-05-31"` body field and the `model`
// field stripped (Bedrock carries the model id in the URL).

mod scenario_1_system_handling {
    use super::*;

    #[test]
    fn bedrock_invoke_egress() {
        let req = scenarios::scenario_1_system_handling();
        let body = bedrock_invoke_provider()
            .normalize_request(&req)
            .expect("bedrock_invoke normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_invoke"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 2: tool_choice_translations
// =====================================================================
//
// Bedrock-Invoke inherits `translate_tool_choice` from the anthropic_api
// egress: bare-string "auto" becomes `{"type":"auto"}` (Bedrock 400s on
// the OpenAI bare-string form) and the OpenAI-shape
// `{"type":"function","function":{"name":X}}` becomes
// `{"type":"tool","name":X}`. The Anthropic-shape `{"type":"auto"}` form
// passes through unchanged.

mod scenario_2_tool_choice_auto {
    use super::*;

    #[test]
    fn bedrock_invoke_egress() {
        let req = scenarios::scenario_2_tool_choice_auto();
        let body = bedrock_invoke_provider()
            .normalize_request(&req)
            .expect("bedrock_invoke normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_invoke"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

mod scenario_2_tool_choice_auto_anthropic_shape {
    use super::*;

    #[test]
    fn bedrock_invoke_egress() {
        let req = scenarios::scenario_2_tool_choice_auto_anthropic_shape();
        let body = bedrock_invoke_provider()
            .normalize_request(&req)
            .expect("bedrock_invoke normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_invoke"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

mod scenario_2_tool_choice_named_function {
    use super::*;

    #[test]
    fn bedrock_invoke_egress() {
        let req = scenarios::scenario_2_tool_choice_named_function();
        let body = bedrock_invoke_provider()
            .normalize_request(&req)
            .expect("bedrock_invoke normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_invoke"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 3: multi_turn_with_tool_result
// =====================================================================
//
// History serialization round-trip. Bedrock-Invoke for Claude preserves
// the Anthropic typed blocks (tool_use + tool_result) verbatim because
// body construction is delegated to `anthropic_api::request::normalize`.

mod scenario_3_multi_turn_with_tool_result {
    use super::*;

    #[test]
    fn bedrock_invoke_egress() {
        let req = scenarios::scenario_3_multi_turn_with_tool_result();
        let body = bedrock_invoke_provider()
            .normalize_request(&req)
            .expect("bedrock_invoke normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_invoke"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario 4: stop_reason_round_trip
// =====================================================================
//
// Response-side only; lives in the cli crate's `contract_ingress`
// tests. No egress-side action because the egress translates REQUESTS,
// not responses.

// =====================================================================
// Scenario 5: cache_control_positions
// =====================================================================
//
// Bedrock-Invoke uses the Anthropic-shape body, but AWS InvokeModel
// REJECTS a top-level `cache_control` body field (HTTP 400). The egress
// lowers the top-level marker onto the last eligible content block, so the
// body carries the system / tool / message-block markers but NO top-level
// `cache_control`. The trailing message block already carries its own 5m
// marker here, so it is left unchanged by the lowering.

mod scenario_5_cache_control_positions {
    use super::*;

    #[test]
    fn bedrock_invoke_egress() {
        let req = scenarios::scenario_5_cache_control_positions();
        let body = bedrock_invoke_provider()
            .normalize_request(&req)
            .expect("bedrock_invoke normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_invoke"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}
