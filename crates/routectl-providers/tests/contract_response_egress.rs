//! Contract tests for the response-side egress layer.
//!
//! Each scenario takes a canned upstream wire body (Anthropic
//! Messages-shape or OpenAI chat-completions-shape) and asserts that
//! the provider's `normalize_response` produces the expected canonical
//! `ChatResponse` shape. This is the wire-to-canonical analog of the
//! request-side `contract_egress` tests in the sibling file.
//!
//! See the sibling `contract_response_ingress` tests in `routectl-cli`
//! for the canonical-to-wire half. Scenario builders are mirrored in
//! both crates' `common/mod.rs` files; see the mirror-sync note
//! there.
//!
//! Scope: only `anthropic_api` and `openai_compat` providers are
//! covered. Bedrock (Invoke + Converse) and `openai_responses`
//! response-side coverage land in a follow-up PR.

#![cfg(all(feature = "anthropic-api", feature = "openai-compat"))]

mod common;

use routectl_core::Provider;
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use serde_json::json;

// ---------------------------------------------------------------------
// Provider builders
// ---------------------------------------------------------------------
//
// Duplicated from `contract_egress.rs`: the shared `common/mod.rs`
// only carries canonical-shape scenario builders, not provider
// constructors. Keeping the two-line constructors local avoids a
// `pub mod providers` carve-out for two callers.

fn anthropic_api_provider() -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "anthropic-test".into(),
        api_key: "test-key".into(),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        extra_headers: Vec::new(),
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
        extra_headers: vec![],
        default_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
    })
}

// =====================================================================
// Scenario 4: stop_reason_round_trip
// =====================================================================
//
// Wire upstream stop_reason / finish_reason must lift into the
// canonical `Choice.finish_reason` shape that the ingress's
// `render_response` can round-trip back out. Anthropic-API:
// `stop_reason:"end_turn"` -> canonical `"stop"` (OpenAI overlap);
// `stop_reason:"pause_turn"` -> canonical `"pause_turn"` (passthrough
// of the Anthropic-only value, the bug class flagged in the
// `stop_reason round-trip` runbook section). OpenAI-compat:
// `finish_reason:"stop"` round-trips as canonical `"stop"`.

mod scenario_4_normalize_response_stop_reason_end_turn {
    use super::*;

    #[test]
    fn anthropic_api_egress() {
        let raw = json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "content": [{"type": "text", "text": "Hello!"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });

        let resp = anthropic_api_provider()
            .normalize_response(raw)
            .expect("anthropic_api normalize_response");

        assert_eq!(resp.id, "msg_01");
        assert_eq!(resp.model, "claude-3-opus");
        assert_eq!(resp.choices.len(), 1);
        // OpenAI-overlap mapping: end_turn -> stop. The ingress's
        // `render_response` reverse-maps stop -> end_turn so the
        // round-trip closes cleanly.
        assert_eq!(
            resp.choices[0].finish_reason.as_deref(),
            Some("stop"),
            "anthropic end_turn must normalize to canonical `stop`"
        );
    }

    #[test]
    fn openai_compat_egress() {
        let raw = json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "created": 0,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }]
        });

        let resp = openai_compat_provider()
            .normalize_response(raw)
            .expect("openai_compat normalize_response");

        assert_eq!(resp.id, "chatcmpl-1");
        assert_eq!(resp.model, "gpt-4o");
        assert_eq!(resp.choices.len(), 1);
        // OpenAI-compat is canonical-shape passthrough on
        // finish_reason; `stop` round-trips verbatim.
        assert_eq!(
            resp.choices[0].finish_reason.as_deref(),
            Some("stop"),
            "openai-compat finish_reason must passthrough as canonical `stop`"
        );
    }
}

mod scenario_4_normalize_response_stop_reason_pause_turn {
    use super::*;

    #[test]
    fn anthropic_api_egress() {
        let raw = json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "content": [{"type": "text", "text": "Hello!"}],
            "stop_reason": "pause_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });

        let resp = anthropic_api_provider()
            .normalize_response(raw)
            .expect("anthropic_api normalize_response");

        assert_eq!(resp.choices.len(), 1);
        // Anthropic-only stop reasons must passthrough verbatim on
        // the egress normalize half (the bug class: pre-fix
        // `map_stop_reason` clobbered unknown values to `end_turn`,
        // breaking claude-code's per-stop-reason error handling).
        // The ingress's `render_response` reverse mapping then
        // passes `pause_turn` back out unchanged.
        assert_eq!(
            resp.choices[0].finish_reason.as_deref(),
            Some("pause_turn"),
            "anthropic pause_turn must passthrough verbatim, not clobber to stop/end_turn"
        );
    }

    // openai_compat intentionally skipped: the OpenAI chat-completions
    // wire shape has no `pause_turn` equivalent, so there is no
    // matching wire body to feed through normalize_response. The
    // anthropic_api sub-scenario above is the only one that exercises
    // the bug class.
}

// =====================================================================
// Scenario 9: null_content_with_reasoning
// =====================================================================
//
// NIM-style upstreams (e.g. meta/llama-3.3-70b-instruct, Clarifai-
// hosted models on OpenRouter) emit BOTH `reasoning: null` and
// `reasoning_content: "..."` on the same message. The openai-compat
// normalize_response's `coalesce_reasoning_content_in_response`
// preprocessor merges these into the canonical `Message.reasoning`
// field, preferring the non-null value. Without the coalesce the
// canonical message ends up with `reasoning: None` and the
// reasoning chain-of-thought is silently dropped. Bug class:
// `null content alongside non-null reasoning` (see CLAUDE.md gotcha
// section).
//
// Anthropic API does not emit `reasoning_content` (it carries
// thinking as a typed content block), so this scenario is
// openai_compat-only.

mod scenario_9_null_content_with_reasoning {
    use super::*;

    #[test]
    fn openai_compat_egress() {
        // NIM-style raw upstream: `reasoning` is explicitly null AND
        // `reasoning_content` carries the real value. The coalescer
        // must drop the null and lift `reasoning_content` into
        // canonical `Message.reasoning`.
        let raw = json!({
            "id": "chatcmpl-nim-01",
            "model": "meta/llama-3.3-70b-instruct",
            "created": 0,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "The answer is 42.",
                    "reasoning": null,
                    "reasoning_content": "Let me think... 6 * 7 = 42."
                },
                "finish_reason": "stop"
            }]
        });

        let resp = openai_compat_provider()
            .normalize_response(raw)
            .expect("openai_compat normalize_response");

        assert_eq!(resp.choices.len(), 1);
        // Legacy `Message.reasoning` field MUST carry the lifted
        // reasoning_content. The coalesce-from-null path is the bug
        // class; without it the field stays None and downstream
        // ingress render drops the chain-of-thought.
        assert_eq!(
            resp.choices[0].message.reasoning.as_deref(),
            Some("Let me think... 6 * 7 = 42."),
            "openai-compat normalize_response must lift reasoning_content into Message.reasoning when reasoning is null"
        );
        // Content text must NOT get clobbered by the coalesce.
        match &resp.choices[0].message.content {
            routectl_core::MessageContent::Text(t) => {
                assert_eq!(t, "The answer is 42.")
            }
            other => panic!("expected Text content, got {other:?}"),
        }
    }
}
