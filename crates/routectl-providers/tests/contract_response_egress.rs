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
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
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
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
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

// =====================================================================
// Scenario 11: matched_stop_sequence_round_trip
// =====================================================================
//
// Anthropic-shape upstreams surface the matched stop sequence as the
// wire field `stop_sequence` alongside `stop_reason:"stop_sequence"`.
// The egress's `normalize_response` MUST lift that into
// `Choice.matched_stop_sequence` so the Anthropic ingress can render
// `stop_reason:"stop_sequence"` + `stop_sequence:"<value>"` on the
// way out. Without the lift, the canonical `finish_reason:"stop"`
// collapses to wire `end_turn` and claude-code structured-output
// flows fail with synthetic `is_error: true` envelopes.
//
// OpenAI-compat upstreams don't carry the matched sequence on the
// wire (the OpenAI spec has no such field; most hosts also strip the
// marker from response content). The egress applies a heuristic:
// suffix-match the response content against `req.stop` and, when
// exactly one stop sequence was configured, fall back to that single
// sequence even on a stripped-content response (the common
// structured-output single-fence pattern).

mod scenario_11_normalize_response_matched_stop_sequence {
    use super::*;
    use routectl_providers::openai_compat::response::apply_stop_sequence_heuristic;

    #[test]
    fn anthropic_api_egress_lifts_native_stop_sequence() {
        let raw = json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "content": [{"type": "text", "text": "Here is the structured answer."}],
            "stop_reason": "stop_sequence",
            "stop_sequence": "</answer>",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });

        let resp = anthropic_api_provider()
            .normalize_response(raw)
            .expect("anthropic_api normalize_response");

        assert_eq!(resp.choices.len(), 1);
        assert_eq!(
            resp.choices[0].finish_reason.as_deref(),
            Some("stop"),
            "stop_sequence still maps to canonical `stop` for OpenAI-compat consumers",
        );
        assert_eq!(
            resp.choices[0].matched_stop_sequence.as_deref(),
            Some("</answer>"),
            "anthropic_api egress must lift wire `stop_sequence` into canonical \
             `Choice.matched_stop_sequence`",
        );
    }

    #[test]
    fn anthropic_api_ignores_stop_sequence_when_reason_is_end_turn() {
        // A stray `stop_sequence` field on a non-stop_sequence response
        // must be ignored so the Anthropic ingress doesn't mis-render
        // a `stop_reason:"stop_sequence"` over an `end_turn` upstream.
        let raw = json!({
            "id": "msg_02",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "stop_sequence": "</leak>",
            "usage": {"input_tokens": 5, "output_tokens": 1}
        });

        let resp = anthropic_api_provider()
            .normalize_response(raw)
            .expect("anthropic_api normalize_response");

        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert!(
            resp.choices[0].matched_stop_sequence.is_none(),
            "stop_sequence only meaningful when stop_reason == \"stop_sequence\"; \
             a stray field on end_turn must NOT propagate",
        );
    }

    #[test]
    fn openai_compat_heuristic_suffix_match() {
        // Upstream left the marker in the content; suffix match recovers
        // it precisely.
        let raw = json!({
            "id": "chatcmpl-2",
            "model": "deepseek-v4-pro",
            "created": 0,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Here is the answer.</answer>"},
                "finish_reason": "stop"
            }]
        });

        let mut resp = openai_compat_provider()
            .normalize_response(raw)
            .expect("openai_compat normalize_response");
        // Heuristic runs in `complete()`; call directly here since the
        // contract harness bypasses the HTTP-driven path.
        let stops = vec!["</answer>".to_string()];
        apply_stop_sequence_heuristic(&mut resp, Some(stops.as_slice()));

        assert_eq!(
            resp.choices[0].matched_stop_sequence.as_deref(),
            Some("</answer>"),
            "openai-compat heuristic must suffix-match a stop sequence still present in content",
        );
    }

    #[test]
    fn openai_compat_heuristic_single_stop_fallback() {
        // Common structured-output case: host strips the matched
        // marker from content, single stop sequence was configured.
        // Heuristic falls back to the sole stop as the best-guess so
        // the Anthropic ingress emits `stop_reason:"stop_sequence"`
        // instead of `end_turn`. Captures the deepseek-v4-pro
        // reviewer-flow failure (2026-05-19).
        let raw = json!({
            "id": "chatcmpl-3",
            "model": "deepseek-v4-pro",
            "created": 0,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Here is the answer."},
                "finish_reason": "stop"
            }]
        });

        let mut resp = openai_compat_provider()
            .normalize_response(raw)
            .expect("openai_compat normalize_response");
        let stops = vec!["</answer>".to_string()];
        apply_stop_sequence_heuristic(&mut resp, Some(stops.as_slice()));

        assert_eq!(
            resp.choices[0].matched_stop_sequence.as_deref(),
            Some("</answer>"),
            "single-stop fallback must kick in when the host strips the matched marker",
        );
    }

    #[test]
    fn openai_compat_heuristic_skips_non_stop_finish_reason() {
        // length / tool_calls etc. must NOT trigger the heuristic.
        let raw = json!({
            "id": "chatcmpl-4",
            "model": "deepseek-v4-pro",
            "created": 0,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "truncated"},
                "finish_reason": "length"
            }]
        });

        let mut resp = openai_compat_provider()
            .normalize_response(raw)
            .expect("openai_compat normalize_response");
        let stops = vec!["</answer>".to_string()];
        apply_stop_sequence_heuristic(&mut resp, Some(stops.as_slice()));

        assert!(
            resp.choices[0].matched_stop_sequence.is_none(),
            "heuristic must only fire on finish_reason == \"stop\"",
        );
    }

    #[test]
    fn openai_compat_heuristic_skips_when_content_is_null() {
        // Regression: when the response has no content to evidence the
        // match (Null content, empty Parts), the single-stop fallback
        // must NOT fire -- otherwise we over-claim `stop_sequence` on
        // turns where the model never emitted one. Bug class flagged
        // by code review of the initial issue #8 fix.
        let raw = json!({
            "id": "chatcmpl-null",
            "model": "deepseek-v4-pro",
            "created": 0,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": null},
                "finish_reason": "stop"
            }]
        });

        let mut resp = openai_compat_provider()
            .normalize_response(raw)
            .expect("openai_compat normalize_response");
        let stops = vec!["</answer>".to_string()];
        apply_stop_sequence_heuristic(&mut resp, Some(stops.as_slice()));

        assert!(
            resp.choices[0].matched_stop_sequence.is_none(),
            "null-content responses must not trigger the single-stop fallback",
        );
    }

    #[test]
    fn openai_compat_heuristic_skips_when_content_is_whitespace() {
        // Regression: whitespace-only content trims to empty, so the
        // single-stop fallback must NOT fire (same reason as the
        // null-content case -- no actual content to evidence the
        // match).
        let raw = json!({
            "id": "chatcmpl-ws",
            "model": "deepseek-v4-pro",
            "created": 0,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "   \n  "},
                "finish_reason": "stop"
            }]
        });

        let mut resp = openai_compat_provider()
            .normalize_response(raw)
            .expect("openai_compat normalize_response");
        let stops = vec!["</answer>".to_string()];
        apply_stop_sequence_heuristic(&mut resp, Some(stops.as_slice()));

        assert!(
            resp.choices[0].matched_stop_sequence.is_none(),
            "whitespace-only content must not trigger the single-stop fallback",
        );
    }

    #[test]
    fn openai_compat_heuristic_filters_empty_string_stop_entries() {
        // Operator-misconfigured `stop` list with empty strings must
        // not poison the heuristic. Empty entries are filtered before
        // matching; a list of only empty strings collapses to no
        // effective stops, and a mixed list pins the heuristic to the
        // non-empty entries only.
        let raw = json!({
            "id": "chatcmpl-empty-stops",
            "model": "deepseek-v4-pro",
            "created": 0,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Here is the answer.</answer>"},
                "finish_reason": "stop"
            }]
        });

        // Only empty strings -> no effective stops -> stays None.
        let mut resp_only_empty = openai_compat_provider()
            .normalize_response(raw.clone())
            .expect("openai_compat normalize_response");
        apply_stop_sequence_heuristic(
            &mut resp_only_empty,
            Some(vec![String::new(), String::new()].as_slice()),
        );
        assert!(
            resp_only_empty.choices[0].matched_stop_sequence.is_none(),
            "all-empty stop list must collapse to no effective stops",
        );

        // Mixed list -> non-empty entries used for matching.
        let mut resp_mixed = openai_compat_provider()
            .normalize_response(raw)
            .expect("openai_compat normalize_response");
        apply_stop_sequence_heuristic(
            &mut resp_mixed,
            Some(vec![String::new(), "</answer>".to_string()].as_slice()),
        );
        assert_eq!(
            resp_mixed.choices[0].matched_stop_sequence.as_deref(),
            Some("</answer>"),
            "mixed list must filter empties and suffix-match the real entry",
        );
    }

    #[test]
    fn openai_compat_heuristic_ambiguous_multi_stop_stays_none() {
        // Multiple stop sequences configured AND no suffix hit in
        // content -> we can't pick a winner; leave None rather than
        // over-claim.
        let raw = json!({
            "id": "chatcmpl-5",
            "model": "deepseek-v4-pro",
            "created": 0,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "neutral content"},
                "finish_reason": "stop"
            }]
        });

        let mut resp = openai_compat_provider()
            .normalize_response(raw)
            .expect("openai_compat normalize_response");
        let stops = vec!["</answer>".to_string(), "</next>".to_string()];
        apply_stop_sequence_heuristic(&mut resp, Some(stops.as_slice()));

        assert!(
            resp.choices[0].matched_stop_sequence.is_none(),
            "ambiguous multi-stop without a suffix hit must stay None",
        );
    }
}

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
                assert_eq!(t, "The answer is 42.");
            }
            other => panic!("expected Text content, got {other:?}"),
        }
    }
}
