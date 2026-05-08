//! Tests for the Anthropic API provider.
//!
//! Covers:
//!   - Request normalization (system lift, reasoning, tools, multi-turn signature)
//!   - Response normalization (thinking blocks, text, tool_use, stop_reason mapping)
//!   - SSE state machine (full event sequence with signature_delta)
//!   - wiremock integration for complete and stream paths

#[cfg(feature = "anthropic-api")]
mod tests {
    use pretty_assertions::assert_eq;
    use routectl_core::Provider;
    use routectl_core::{
        ChatRequest, Message, MessageContent, ReasoningConfig, ReasoningDetail,
        ReasoningDetailKind, Role, ToolDef,
    };
    use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
    use serde_json::{json, Value};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_provider(base_url: &str) -> AnthropicApiProvider {
        let cfg = AnthropicApiConfig {
            id: "test-anthropic".into(),
            api_key: "test-key".into(),
            base_url: base_url.to_string(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            extra_headers: Vec::new(),
            user_agent: None,
        };
        AnthropicApiProvider::new(cfg)
    }

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn system_msg(text: &str) -> Message {
        Message {
            role: Role::System,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn base_req(model: &str, msgs: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: msgs,
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // Request normalization tests
    // -----------------------------------------------------------------------

    #[test]
    fn system_message_lifted_to_top_level() {
        let provider = make_provider("https://api.anthropic.com");
        let req = base_req(
            "claude-3-opus",
            vec![
                system_msg("You are a helpful assistant."),
                user_msg("Hello!"),
            ],
        );
        let body = provider.normalize_request(&req).unwrap();

        // top-level system field must be present
        assert_eq!(body["system"], "You are a helpful assistant.");

        // messages array must contain only the user message
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");

        // no system role in messages
        for m in msgs {
            assert_ne!(m["role"], "system");
        }
    }

    #[test]
    fn reasoning_max_tokens_maps_to_budget_tokens() {
        let provider = make_provider("https://api.anthropic.com");
        let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
        req.reasoning = Some(ReasoningConfig {
            max_tokens: Some(5000),
            ..Default::default()
        });
        let body = provider.normalize_request(&req).unwrap();

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 5000);
    }

    #[test]
    fn reasoning_effort_high_maps_to_80_percent() {
        let provider = make_provider("https://api.anthropic.com");
        let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
        req.max_tokens = Some(10000);
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        });
        let body = provider.normalize_request(&req).unwrap();

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8000u64);
    }

    #[test]
    fn reasoning_effort_none_disables_thinking() {
        let provider = make_provider("https://api.anthropic.com");
        let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
        req.reasoning = Some(ReasoningConfig {
            effort: Some("none".into()),
            ..Default::default()
        });
        let body = provider.normalize_request(&req).unwrap();

        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn tools_translated_to_anthropic_shape() {
        let provider = make_provider("https://api.anthropic.com");
        let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
        req.tools = Some(vec![ToolDef::Other(json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get the current weather",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                }
            }
        }))]);
        let body = provider.normalize_request(&req).unwrap();

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        // Anthropic shape: name, description, input_schema (no 'type' or 'function' wrapper)
        assert_eq!(tool["name"], "get_weather");
        assert_eq!(tool["description"], "Get the current weather");
        assert!(tool.get("input_schema").is_some());
        assert_eq!(
            tool["input_schema"]["properties"]["location"]["type"],
            "string"
        );
        // No 'parameters' key in Anthropic shape
        assert!(tool.get("parameters").is_none());
        assert!(tool.get("function").is_none());
    }

    #[test]
    fn multi_turn_signature_preserved_in_assistant_message() {
        let provider = make_provider("https://api.anthropic.com");

        // Simulate an assistant turn that came back with a thinking block.
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: Some("rd_01".into()),
            format: Some("anthropic-claude-v1".into()),
            index: Some(0),
            payload: json!({"text": "I reasoned about it", "signature": "sig_preserve_me"}),
        };
        let assistant_msg = Message {
            role: Role::Assistant,
            content: MessageContent::Text("Sure!".into()),
            reasoning: None,
            reasoning_details: vec![detail],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        let req = base_req(
            "claude-3-opus",
            vec![
                user_msg("Think about X"),
                assistant_msg,
                user_msg("Continue"),
            ],
        );
        let body = provider.normalize_request(&req).unwrap();
        let msgs = body["messages"].as_array().unwrap();

        // Find the assistant message
        let asst = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        let content = asst["content"].as_array().unwrap();

        // Must have a thinking block with the signature
        let thinking_block = content
            .iter()
            .find(|b| b["type"] == "thinking")
            .expect("thinking block missing");
        assert_eq!(thinking_block["signature"], "sig_preserve_me");
        assert_eq!(thinking_block["thinking"], "I reasoned about it");
    }

    #[test]
    fn max_tokens_defaults_to_4096_when_not_set() {
        let provider = make_provider("https://api.anthropic.com");
        let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
        req.max_tokens = None;
        let body = provider.normalize_request(&req).unwrap();
        assert_eq!(body["max_tokens"], 4096u64);
    }

    #[test]
    fn stop_translated_to_stop_sequences() {
        let provider = make_provider("https://api.anthropic.com");
        let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
        req.stop = Some(vec!["STOP".into(), "END".into()]);
        let body = provider.normalize_request(&req).unwrap();
        let seqs = body["stop_sequences"].as_array().unwrap();
        assert_eq!(seqs[0], "STOP");
        assert_eq!(seqs[1], "END");
    }

    #[test]
    fn reasoning_enabled_sets_temperature_to_one() {
        let provider = make_provider("https://api.anthropic.com");
        let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
        req.temperature = Some(0.5);
        req.reasoning = Some(ReasoningConfig {
            max_tokens: Some(1000),
            ..Default::default()
        });
        let body = provider.normalize_request(&req).unwrap();
        // When thinking is enabled, Anthropic requires temperature=1.
        assert_eq!(body["temperature"], 1.0f64);
    }

    // -----------------------------------------------------------------------
    // Response normalization tests
    // -----------------------------------------------------------------------

    #[test]
    fn response_thinking_blocks_normalized() {
        let provider = make_provider("https://api.anthropic.com");
        let raw = json!({
            "id": "msg_01abc",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-5-20251101",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 120, "output_tokens": 80},
            "content": [
                {"type": "thinking", "thinking": "Let me think...", "signature": "sig_abc123"},
                {"type": "text", "text": "The answer is 42."}
            ]
        });

        let resp = provider.normalize_response(raw).unwrap();
        assert_eq!(resp.choices.len(), 1);
        let msg = &resp.choices[0].message;

        // Text content
        match &msg.content {
            MessageContent::Text(t) => assert_eq!(t, "The answer is 42."),
            other => panic!("expected Text content, got {:?}", other),
        }

        // Reasoning details
        assert_eq!(msg.reasoning_details.len(), 1);
        let detail = &msg.reasoning_details[0];
        assert!(matches!(detail.kind, ReasoningDetailKind::Text));
        assert_eq!(detail.format.as_deref(), Some("anthropic-claude-v1"));
        assert_eq!(detail.payload["text"], "Let me think...");
        assert_eq!(detail.payload["signature"], "sig_abc123");

        assert_eq!(resp.choices[0].finish_reason, Some("stop".into()));
    }

    #[test]
    fn response_tool_use_maps_to_tool_calls() {
        let provider = make_provider("https://api.anthropic.com");
        let raw = json!({
            "id": "msg_02tool",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-5-20251101",
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 50, "output_tokens": 30},
            "content": [
                {"type": "tool_use", "id": "toolu_01", "name": "get_weather",
                 "input": {"location": "London"}}
            ]
        });

        let resp = provider.normalize_response(raw).unwrap();
        let msg = &resp.choices[0].message;

        let tool_calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "toolu_01");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");

        // arguments should be JSON string
        let args: Value =
            serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["location"], "London");

        assert_eq!(resp.choices[0].finish_reason, Some("tool_calls".into()));
    }

    #[test]
    fn stop_reason_mapping() {
        use routectl_providers::anthropic_api::response::map_stop_reason;
        assert_eq!(map_stop_reason(Some("end_turn")), Some("stop".into()));
        assert_eq!(map_stop_reason(Some("max_tokens")), Some("length".into()));
        assert_eq!(map_stop_reason(Some("stop_sequence")), Some("stop".into()));
        assert_eq!(map_stop_reason(Some("tool_use")), Some("tool_calls".into()));
        assert_eq!(map_stop_reason(None), None);
    }

    #[test]
    fn usage_mapped_correctly() {
        let provider = make_provider("https://api.anthropic.com");
        let raw = json!({
            "id": "msg_03",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_read_input_tokens": 20
            },
            "content": [{"type": "text", "text": "Hi"}]
        });

        let resp = provider.normalize_response(raw).unwrap();
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
    }

    #[test]
    fn redacted_thinking_maps_to_encrypted_kind() {
        let provider = make_provider("https://api.anthropic.com");
        let raw = json!({
            "id": "msg_04",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5},
            "content": [
                {"type": "redacted_thinking", "data": "base64encryptedblob"},
                {"type": "text", "text": "Here is my answer."}
            ]
        });

        let resp = provider.normalize_response(raw).unwrap();
        let detail = &resp.choices[0].message.reasoning_details[0];
        assert!(matches!(detail.kind, ReasoningDetailKind::Encrypted));
        assert_eq!(detail.payload["data"], "base64encryptedblob");
    }

    // -----------------------------------------------------------------------
    // SSE state machine tests
    // -----------------------------------------------------------------------

    #[test]
    fn sse_full_stream_sequence() {
        use routectl_providers::anthropic_api::sse::SseState;

        let mut state = SseState::default();
        let pid = "test";
        let mut chunks = Vec::new();

        let events = vec![
            r#"{"type":"message_start","message":{"id":"msg_sse01","model":"claude-3-opus","usage":{"input_tokens":100,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_xyz789"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello world!"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":20}}"#,
            r#"{"type":"message_stop"}"#,
        ];

        for event_data in &events {
            if let Some(chunk) = state.parse_event(pid, event_data).unwrap() {
                chunks.push(chunk);
            }
        }

        // Should have: thinking_delta chunk, signature chunk, text_delta chunk, finish_reason chunk
        assert!(
            chunks.len() >= 4,
            "expected at least 4 chunks, got {}",
            chunks.len()
        );

        // First chunk: thinking_delta with reasoning and reasoning_details
        let thinking_chunk = &chunks[0];
        let delta = &thinking_chunk.choices[0].delta;
        assert!(delta.reasoning.is_some());
        assert!(!delta.reasoning_details.is_empty());
        assert_eq!(
            delta.reasoning_details[0].payload["text"],
            "Let me think..."
        );
        // No signature yet in thinking_delta
        assert!(delta.reasoning_details[0]
            .payload
            .get("signature")
            .is_none());

        // Second chunk: signature_delta with only signature in reasoning_details
        let sig_chunk = &chunks[1];
        let sig_delta = &sig_chunk.choices[0].delta;
        assert!(
            sig_delta.reasoning.is_none(),
            "signature chunk should have no reasoning text"
        );
        assert!(!sig_delta.reasoning_details.is_empty());
        assert_eq!(
            sig_delta.reasoning_details[0].payload["signature"],
            "sig_xyz789"
        );

        // Third chunk: text_delta
        let text_chunk = &chunks[2];
        assert_eq!(
            text_chunk.choices[0].delta.content.as_deref(),
            Some("Hello world!")
        );

        // Last chunk: finish_reason
        let finish_chunk = chunks.last().unwrap();
        assert_eq!(
            finish_chunk.choices[0].finish_reason.as_deref(),
            Some("stop")
        );

        // State captures id and model from message_start
        assert_eq!(state.id, "msg_sse01");
        assert_eq!(state.model, "claude-3-opus");
    }

    #[test]
    fn sse_tool_use_delta_emits_tool_calls() {
        use routectl_providers::anthropic_api::sse::SseState;

        let mut state = SseState::default();
        let pid = "test";
        let mut chunks = Vec::new();

        let events = vec![
            r#"{"type":"message_start","message":{"id":"msg_tool","model":"claude-3-opus","usage":{"input_tokens":20,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"search"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"rust\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}"#,
        ];

        for event_data in &events {
            if let Some(chunk) = state.parse_event(pid, event_data).unwrap() {
                chunks.push(chunk);
            }
        }

        // Tool delta chunk
        let tool_chunk = &chunks[0];
        let tool_calls = tool_chunk.choices[0].delta.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0]["function"]["name"], "search");
        assert_eq!(tool_calls[0]["function"]["arguments"], "{\"q\":\"rust\"}");

        // Finish reason chunk
        let finish = chunks.last().unwrap();
        assert_eq!(
            finish.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
    }

    #[test]
    fn sse_unknown_events_return_none() {
        use routectl_providers::anthropic_api::sse::SseState;

        let mut state = SseState::default();
        let pid = "test";

        // Ping event
        let result = state.parse_event(pid, r#"{"type":"ping"}"#).unwrap();
        assert!(result.is_none());

        // message_stop
        let result = state
            .parse_event(pid, r#"{"type":"message_stop"}"#)
            .unwrap();
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // wiremock integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn integration_complete() {
        let mock_server = MockServer::start().await;

        let response_body = json!({
            "id": "msg_int01",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 20},
            "content": [{"type": "text", "text": "Integration test response."}]
        });

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&mock_server)
            .await;

        let provider = make_provider(&mock_server.uri());
        let req = base_req("claude-3-opus", vec![user_msg("Hi from integration test")]);

        let resp = provider.complete(req).await.unwrap();
        assert_eq!(resp.id, "msg_int01");
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "Integration test response."),
            other => panic!("expected Text content, got {:?}", other),
        }
        assert_eq!(resp.routectl_provider.as_deref(), Some("test-anthropic"));
    }

    #[tokio::test]
    async fn integration_complete_upstream_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "type": "error",
                "error": {"type": "authentication_error", "message": "invalid api key"}
            })))
            .mount(&mock_server)
            .await;

        let provider = make_provider(&mock_server.uri());
        let req = base_req("claude-3-opus", vec![user_msg("hi")]);

        let err = provider.complete(req).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("401") || msg.contains("invalid api key"),
            "unexpected: {msg}"
        );
    }

    #[tokio::test]
    async fn integration_stream() {
        let mock_server = MockServer::start().await;

        // Build a minimal valid SSE body.
        let sse_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_st01\",\"model\":\"claude-3-opus\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi!\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .append_header("content-type", "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider = make_provider(&mock_server.uri());
        let mut req = base_req("claude-3-opus", vec![user_msg("stream test")]);
        req.stream = Some(true);

        use futures::StreamExt;
        let mut stream = provider.stream(req).await.unwrap();
        let mut text_chunks: Vec<String> = Vec::new();
        let mut finish_reasons: Vec<String> = Vec::new();

        while let Some(result) = stream.next().await {
            let chunk = result.unwrap();
            for choice in &chunk.choices {
                if let Some(ref text) = choice.delta.content {
                    text_chunks.push(text.clone());
                }
                if let Some(ref fr) = choice.finish_reason {
                    finish_reasons.push(fr.clone());
                }
            }
        }

        assert!(
            text_chunks.contains(&"Hi!".to_string()),
            "expected 'Hi!' in {:?}",
            text_chunks
        );
        assert!(
            finish_reasons.contains(&"stop".to_string()),
            "expected 'stop' in {:?}",
            finish_reasons
        );
    }

    // -----------------------------------------------------------------------
    // M1.2: decoupled anthropic-beta from auth_kind
    // -----------------------------------------------------------------------

    fn make_response_body() -> Value {
        json!({
            "id": "msg_check",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 3},
            "content": [{"type": "text", "text": "ok"}]
        })
    }

    #[tokio::test]
    async fn oauth_bearer_does_not_auto_inject_beta_gate() {
        let mock_server = MockServer::start().await;

        // Set up a mock that will ONLY match if anthropic-beta equals
        // the value we explicitly put in extra_headers (NOT the old
        // auto-injected oauth-2025-04-20).
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", "Bearer test-key"))
            .and(header("anthropic-beta", "context-1m-2025-08-07"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response_body()))
            .mount(&mock_server)
            .await;

        // No wiremock match will succeed if anthropic-beta is missing
        // or set to oauth-2025-04-20 -- the provider should hit timeout.
        let cfg = AnthropicApiConfig {
            id: "oauth-test".into(),
            api_key: "test-key".into(),
            base_url: mock_server.uri(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::OauthBearer,
            extra_headers: vec![("anthropic-beta".into(), "context-1m-2025-08-07".into())],
            user_agent: None,
        };
        let provider = AnthropicApiProvider::new(cfg);
        let req = base_req("claude-3-opus", vec![user_msg("hi")]);
        let resp = provider.complete(req).await.unwrap();
        assert_eq!(resp.id, "msg_check");
    }

    #[tokio::test]
    async fn api_key_auth_can_set_beta_via_extra_headers() {
        let mock_server = MockServer::start().await;

        // Match the anthropic-beta extra header. Use a single flag
        // (no comma) because wiremock's `header(name, value)` matcher
        // compares against parsed comma-split values; a comma-joined
        // flag list is exposed as multiple values, not one.
        let expected_beta = "context-1m-2025-08-07";
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-beta", expected_beta))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response_body()))
            .mount(&mock_server)
            .await;

        let cfg = AnthropicApiConfig {
            id: "apikey-test".into(),
            api_key: "test-key".into(),
            base_url: mock_server.uri(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            extra_headers: vec![("anthropic-beta".into(), expected_beta.into())],
            user_agent: None,
        };
        let provider = AnthropicApiProvider::new(cfg);
        let req = base_req("claude-3-opus", vec![user_msg("hi")]);
        let resp = provider.complete(req).await.unwrap();
        assert_eq!(resp.id, "msg_check");
    }

    #[tokio::test]
    async fn user_agent_override_reaches_outbound() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("user-agent", "claude-code/1.2.3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response_body()))
            .mount(&mock_server)
            .await;

        let cfg = AnthropicApiConfig {
            id: "ua-test".into(),
            api_key: "test-key".into(),
            base_url: mock_server.uri(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            extra_headers: Vec::new(),
            user_agent: Some("claude-code/1.2.3".into()),
        };
        let provider = AnthropicApiProvider::new(cfg);
        let req = base_req("claude-3-opus", vec![user_msg("hi")]);
        let resp = provider.complete(req).await.unwrap();
        assert_eq!(resp.id, "msg_check");
    }
}
