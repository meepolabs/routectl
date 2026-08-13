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
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
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
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
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
        #[cfg(feature = "bedrock")]
        mantle: None,
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

// =====================================================================
// Forward-compat regression pins (NO production change)
// =====================================================================
//
// These tests pin the existing forward-compat seams that let routectl
// ship new Anthropic features without code edits:
//
//   - the 3-source `anthropic-beta` union (ingress lift +
//     provider header_extras + model header_extras) lands a single
//     comma-joined wire header.
//   - unknown top-level body fields lifted from `provider_extras` (the
//     escape hatch the Anthropic ingress writes into) round-trip
//     verbatim through the egress, so a new wire field like
//     `context_management` ships without a routectl release.
//   - unknown content blocks captured into `ContentPart::Other` round-
//     trip verbatim through the per-block translator, so a new content
//     block type like `tool_reference` ships without a code edit.
//
// Failure of any of these would mean an architectural regression that
// breaks the hub-and-spoke contract documented in CLAUDE.md.

mod forward_compat_pins {
    use super::*;
    use routectl_core::{ChatRequest, ContentPart, Message, MessageContent, Role};
    use serde_json::{Map, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Anthropic-shape success body for wiremock matches in this module.
    /// Provider's `complete()` must succeed so we get a clean look at
    /// the captured outbound request without triggering the 4xx error
    /// path.
    fn ok_response_body() -> serde_json::Value {
        json!({
            "id": "msg_pin",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "content": [{"type": "text", "text": "ok"}]
        })
    }

    /// Local provider builder that points at a wiremock URI; reuses the
    /// rest of the AnthropicApiConfig defaults from the file-level
    /// `anthropic_api_provider()` with one operator-beta merged into
    /// `header_extras["anthropic-beta"]`. The test below combines that
    /// provider source with the `req.anthropic_beta` ingress source.
    fn anthropic_api_provider_with_mock(
        base_url: String,
        beta_in_header_extras: &str,
    ) -> AnthropicApiProvider {
        AnthropicApiProvider::new(AnthropicApiConfig {
            id: "anthropic-test-pins".into(),
            auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
            base_url,
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: vec![("anthropic-beta".into(), beta_in_header_extras.into())],
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,

            #[cfg(feature = "bedrock")]
            mantle: None,
        })
    }

    /// Pin: the `anthropic-beta` HTTP header on the wire is the union of
    /// `req.anthropic_beta` (ingress lift; the dispatch layer also folds
    /// per-model values in here, hence "three sources") AND
    /// `cfg.header_extras["anthropic-beta"]` (provider-config). The
    /// per-model `routectl_internal::header_extras` source is composed
    /// upstream of this egress and lands on `req.anthropic_beta`; its
    /// own contract is pinned in the router-side merge tests.
    ///
    /// Containment-only assertion: the merge dedupes via a `BTreeSet`
    /// and joins with `,`, so the order on the wire is
    /// implementation-defined. We assert presence of every flag without
    /// pinning order.
    #[tokio::test]
    async fn anthropic_beta_three_source_union_reaches_outbound_header() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_body()))
            .mount(&mock_server)
            .await;

        let provider = anthropic_api_provider_with_mock(mock_server.uri(), "operator-beta-1");

        let req = ChatRequest {
            model: "claude-3-opus".into(),
            messages: vec![common::user_msg("hi")].into(),
            max_tokens: Some(1024),
            anthropic_beta: vec![
                "claude-code-20250219".to_string(),
                "context-management-2025-06-27".to_string(),
            ],
            ..Default::default()
        };

        provider.complete(req).await.expect("complete must succeed");

        let received = mock_server
            .received_requests()
            .await
            .expect("wiremock captured requests");
        assert_eq!(
            received.len(),
            1,
            "expected exactly one outbound request, got {}",
            received.len()
        );

        let beta_header = received[0]
            .headers
            .get("anthropic-beta")
            .expect("anthropic-beta header must be set on the outbound request");
        let beta_value = beta_header
            .to_str()
            .expect("anthropic-beta header must be valid utf-8");

        for expected in [
            "claude-code-20250219",
            "context-management-2025-06-27",
            "operator-beta-1",
        ] {
            assert!(
                beta_value.contains(expected),
                "anthropic-beta header missing `{expected}`; got `{beta_value}`",
            );
        }
    }

    /// Pin: an unknown top-level body field provided via
    /// `req.provider_extras` (the destination of the Anthropic
    /// ingress's forward-compat sweep) lands on the outbound body
    /// verbatim. `context_management` is the v0.7 example -- routectl
    /// has no typed field for it, but the egress must still ship the
    /// wire body Anthropic expects.
    ///
    /// The egress's `merge_provider_extras` blocks routectl-managed
    /// keys (`messages`, `thinking`, etc.) so a stray override can't
    /// replace assembled body fields; `context_management` is not on
    /// that block-list and so flows through. Pin both halves with one
    /// equality assertion on the merged value.
    #[test]
    fn context_management_body_field_round_trips_byte_for_byte() {
        let input = json!({
            "applied_edits": [{"type": "clear_tool_uses_20250919"}]
        });

        let req = ChatRequest {
            model: "claude-3-opus".into(),
            messages: vec![common::user_msg("hi")].into(),
            max_tokens: Some(1024),
            provider_extras: Some(json!({
                "context_management": input
            })),
            ..Default::default()
        };

        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        assert_eq!(
            body.get("context_management"),
            Some(&input),
            "context_management must round-trip verbatim into the outbound body; got body: {body}",
        );
    }

    /// Pin: an unknown content block (e.g. Anthropic ships
    /// `tool_reference` in a future API version) captured by the
    /// `ContentPart::Other` catchall on the canonical surface must
    /// round-trip verbatim into the outbound message content. The
    /// egress's per-part translator emits `ContentBlock::Other`, whose
    /// serializer rebuilds the original wire JSON object from
    /// (type_tag, extras).
    ///
    /// Without this seam, a new block type would require a code edit
    /// in `routectl-core::content_part` AND a release before
    /// claude-code could speak it through routectl. Architect
    /// confirmation: production code in `routectl-core/src/content_part.rs`
    /// already handles this; the test is a regression pin.
    #[test]
    fn tool_reference_block_round_trips_via_other_catchall() {
        let mut extras: Map<String, serde_json::Value> = Map::new();
        extras.insert("tool_use_id".into(), json!("toolu_abc"));

        let req = ChatRequest {
            model: "claude-3-opus".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(vec![ContentPart::Other {
                    type_tag: "tool_reference".into(),
                    cache_control: None,
                    extras,
                }]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            max_tokens: Some(1024),
            ..Default::default()
        };

        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        let block = body
            .pointer("/messages/0/content/0")
            .expect("first content block must be present in body");

        assert_eq!(
            *block,
            json!({
                "type": "tool_reference",
                "tool_use_id": "toolu_abc"
            }),
            "tool_reference block must round-trip via ContentPart::Other; got block: {block}",
        );
    }
}

// =====================================================================
// Structured-output format shape (request side)
// =====================================================================

mod structured_output_format_shape {
    use super::*;
    use routectl_core::ChatRequest;
    use serde_json::json;

    /// Pin: the CONVENTIONAL OpenAI-shape structured-output request --
    /// `{name, schema, strict: true}`, which every OpenAI-compatible client
    /// emits -- reaches the Anthropic wire as `{type, schema}` only.
    /// Anthropic's `output_config.format` accepts no other members and 400s a
    /// body carrying either sibling key, so this is the request shape the
    /// egress must reshape rather than forward.
    ///
    /// Member-set assertion, deliberately not a snapshot: an absent-key check
    /// fails when the keys come back, whereas a snapshot would simply record
    /// whatever ships and be re-accepted through review.
    #[test]
    fn conventional_json_schema_response_format_emits_type_and_schema_only() {
        let schema = json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"]
        });

        let req = ChatRequest {
            model: "claude-3-opus".into(),
            messages: vec![common::user_msg("hi")].into(),
            max_tokens: Some(1024),
            response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "answer_schema",
                    "schema": schema.clone(),
                    "strict": true
                }
            })),
            ..Default::default()
        };

        let body = anthropic_api_provider()
            .normalize_request(&req)
            .expect("anthropic_api normalize");

        let format = body
            .pointer("/output_config/format")
            .and_then(serde_json::Value::as_object)
            .expect("output_config.format must be present on the wire body");

        let mut keys: Vec<&str> = format.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["schema", "type"],
            "output_config.format must carry exactly {{type, schema}}; got body: {body}"
        );
        // Every caller keyword survives; the ONE addition is
        // `additionalProperties: false`, which Anthropic requires explicitly
        // on every object and whose only accepted value is `false`. Supplying
        // a mandatory key contradicts no caller intent, unlike dropping a
        // constraint the caller wrote.
        let mut expected_schema = schema;
        expected_schema["additionalProperties"] = json!(false);
        assert_eq!(
            format.get("schema"),
            Some(&expected_schema),
            "the caller's schema must ship with only the mandatory \
             additionalProperties added; got body: {body}"
        );
        assert!(
            !body.to_string().contains("answer_schema"),
            "the caller's schema name must not appear anywhere on the wire body: {body}"
        );
    }
}
