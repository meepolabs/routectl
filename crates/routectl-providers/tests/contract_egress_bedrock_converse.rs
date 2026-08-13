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
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth::ResolvedCreds,
};

use common::scenarios;

// ---------------------------------------------------------------------
// Provider builder
// ---------------------------------------------------------------------

/// Construct a `BedrockProvider` wired for the Converse shape with a
/// bearer-key auth handle. The bearer-key path skips SigV4 entirely;
/// `normalize_request` does not touch the resolved creds (signing
/// happens in `signing::apply` later in the pipeline), so the
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

// =====================================================================
// Scenario: document tool_result -- canonical Parts path
// =====================================================================
//
// A `Role::Tool` turn whose content is canonical `Parts` reaches
// `translate_part_for_tool_result` -> `document_to_tool_result`, which
// emits `toolResult.content[].document` through the shared
// `tool_result_document_value` assembler.
//
// Two documents ride in one turn so the optional `citations` member is
// pinned present AND absent in the same recording: the first carries
// `citations: {enabled: true}` and lifts to `{"enabled": true}` on the
// wire; the second omits citations entirely and the member must be
// absent from the emitted document (not `false`, not `null`).
//
// The first document's source is `text`, so `source.bytes` records the
// base64 encoding the wire requires; the second is already `base64` and
// passes through verbatim. `title` maps to `document.name` through
// `sanitize_document_name`, so the emitted names record that scrub
// (disallowed characters become `-`) rather than the raw titles.
//
// KEY ORDER IS PART OF THE CONTRACT. The document value is assembled
// with `serde_json::json!` and this workspace builds `serde_json`
// WITHOUT `preserve_order`, so `Value::Object` is a `BTreeMap` and the
// members serialize ALPHABETICALLY (`citations`, `format`, `name`,
// `source`). Recording that order is deliberate: were this value ever
// replaced by a typed struct, serde would emit declaration order and
// this snapshot would fail loudly, forcing a deliberate re-review of
// the bytes rather than an unnoticed reshuffle.

mod scenario_document_tool_result_parts_path {
    use super::*;
    use routectl_core::{
        ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, Role,
    };
    use serde_json::json;

    #[test]
    fn bedrock_converse_egress() {
        let req = ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: vec![
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![ContentPart::Known(
                        KnownContentPart::ToolUse {
                            id: "toolu_doc_1".into(),
                            name: "fetch_report".into(),
                            input: json!({"quarter": "Q3"}),
                            cache_control: None,
                        },
                    )]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                Message {
                    refusal: None,
                    role: Role::Tool,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Document {
                            source: json!({
                                "type": "text",
                                "media_type": "text/plain",
                                "data": "quarterly revenue summary",
                            }),
                            title: Some("Q3 Report (final)".into()),
                            citations: Some(json!({"enabled": true})),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::Document {
                            source: json!({
                                "type": "base64",
                                "media_type": "application/pdf",
                                "data": "JVBERi0xLjQK",
                            }),
                            title: Some("appendix_a".into()),
                            citations: None,
                            cache_control: None,
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: Some("toolu_doc_1".into()),
                    tool_calls: None,
                },
            ]
            .into(),
            max_tokens: Some(1024),
            ..Default::default()
        };
        let body = bedrock_converse_provider()
            .normalize_request(&req)
            .expect("bedrock_converse normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_converse"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario: document tool_result -- raw Anthropic-shape array path
// =====================================================================
//
// The OTHER carrier: a canonical `KnownContentPart::ToolResult` whose
// `content` is an opaque Anthropic-shape array reaches
// `translate_tool_result_array_element`, whose `"document"` arm
// delegates to the same `document_to_tool_result`.
//
// Pinned independently of the Parts path above because THIS is the path
// that drifted: it once silently dropped `citations`, which is why the
// shared `tool_result_document_value` assembler exists. Two recordings
// of one shared assembler are not redundant -- a re-divergence would
// change only one of them.
//
// Same citations-present / citations-absent pairing as the Parts path,
// and the same alphabetical member order for the reason stated there.

mod scenario_document_tool_result_raw_anthropic_shape {
    use super::*;
    use routectl_core::{
        ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, Role,
    };
    use serde_json::json;

    #[test]
    fn bedrock_converse_egress() {
        let req = ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolResult {
                        tool_use_id: "toolu_doc_2".into(),
                        content: json!([
                            {
                                "type": "document",
                                "source": {
                                    "type": "text",
                                    "media_type": "text/markdown",
                                    "data": "# Notes",
                                },
                                "title": "Meeting Notes [2026]",
                                "citations": {"enabled": true},
                            },
                            {
                                "type": "document",
                                "source": {
                                    "type": "base64",
                                    "media_type": "text/csv",
                                    "data": "YSxiLGMK",
                                },
                                "title": "rows.csv",
                            },
                        ]),
                        is_error: None,
                        cache_control: None,
                    },
                )]),
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
        let body = bedrock_converse_provider()
            .normalize_request(&req)
            .expect("bedrock_converse normalize");

        insta::with_settings!({snapshot_path => "snapshots/bedrock_converse"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Scenario: unmodeled-block passthrough round-trip
// =====================================================================
//
// An unmodeled Converse response block (a single-key AWS union such as
// `{video: {...}}`) decodes to a canonical `ContentPart::Other` and must
// re-emit byte-identically when that history is replayed on the next
// request. This closes the asymmetry where the response side preserved
// the block but the request side silently deleted it, breaking multi-turn
// replay of forward-compat block types.

mod scenario_other_passthrough_round_trip {
    use super::*;
    use routectl_core::{ChatRequest, ContentPart, Message, MessageContent, Role};
    use serde_json::json;

    #[test]
    fn unmodeled_converse_block_round_trips_byte_identical() {
        let provider = bedrock_converse_provider();

        // A Converse response carrying an unmodeled single-key union block.
        let video = json!({"format": "mp4", "source": {"bytes": "AAAA"}});
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"video": video}]
                }
            },
            "stopReason": "end_turn"
        });

        // Decode: the unknown block preserves as a canonical Other part.
        let resp = provider
            .normalize_response(raw)
            .expect("bedrock_converse normalize_response");
        let parts = match &resp.choices[0].message.content {
            MessageContent::Parts(p) => p.clone(),
            other => panic!("expected Parts carrying the Other block, got {other:?}"),
        };
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, ContentPart::Other { type_tag, .. } if type_tag == "video")),
            "response decode must yield a canonical Other for the unknown block, got {parts:?}"
        );

        // Replay: the preserved part re-emits as the same single-key union.
        let req = ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(parts),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            ..Default::default()
        };
        let body = provider
            .normalize_request(&req)
            .expect("bedrock_converse normalize_request");

        let emitted = &body["messages"][0]["content"][0];
        assert_eq!(*emitted, json!({"video": video}), "got {body}");
    }
}
