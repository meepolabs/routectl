//! The streaming metadata fix must not strip complete-response tool IDs.
use routectl_core::{ChatRequest, Provider};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn req() -> ChatRequest {
    serde_json::from_value(
        json!({"model":"test-model", "messages":[{"role":"user","content":"hi"}]}),
    )
    .unwrap()
}

async fn server(body: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

fn assert_tools(response: routectl_core::ChatResponse, ids: &[&str]) {
    let calls = response.choices[0].message.tool_calls.as_ref().unwrap();
    assert_eq!(calls.len(), ids.len());
    for (call, id) in calls.iter().zip(ids) {
        assert_eq!(call["id"], *id);
        assert!(!call["function"]["name"].as_str().unwrap().is_empty());
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
            json!({"x":1})
        );
    }
}

#[cfg(feature = "anthropic-api")]
#[tokio::test]
async fn anthropic_complete_preserves_multiple_tool_ids() {
    use crate::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
    let server = server(
        json!({"id":"msg", "type":"message", "role":"assistant", "model":"test-model",
        "stop_reason":"tool_use", "content":[
            {"type":"tool_use","id":"tool_a","name":"alpha","input":{"x":1}},
            {"type":"tool_use","id":"tool_b","name":"beta","input":{"x":1}}],
        "usage":{"input_tokens":1,"output_tokens":1}}),
    )
    .await;
    let mut cfg = AnthropicApiConfig::new("anthropic:audit", "synthetic-test-key");
    cfg.base_url = server.uri();
    assert_tools(
        AnthropicApiProvider::new(cfg)
            .complete(req())
            .await
            .unwrap(),
        &["tool_a", "tool_b"],
    );
}

#[cfg(feature = "gemini")]
#[tokio::test]
async fn gemini_complete_preserves_synthesized_tool_ids() {
    use crate::gemini::{GeminiConfig, GeminiProvider};
    let server = server(
        json!({"candidates":[{"finishReason":"STOP", "content":{"parts":[
        {"functionCall":{"name":"alpha","args":{"x":1}}},
        {"functionCall":{"name":"beta","args":{"x":1}}}]}}]}),
    )
    .await;
    let mut cfg = GeminiConfig::new("gemini:audit", "synthetic-test-key");
    cfg.base_url = server.uri();
    let response = GeminiProvider::new(cfg).complete(req()).await.unwrap();
    let calls = response.choices[0].message.tool_calls.as_ref().unwrap();
    let ids: Vec<&str> = calls.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert!(ids.iter().all(|id| id.starts_with("call_") && id.len() > 5));
    let owned_ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    let borrowed_ids: Vec<&str> = owned_ids.iter().map(String::as_str).collect();
    assert_tools(response, &borrowed_ids);
}

#[cfg(all(feature = "openai-responses", feature = "bedrock"))]
#[tokio::test]
async fn responses_complete_force_stream_preserves_multiple_tool_ids() {
    use crate::openai_responses::{AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider};
    let server = MockServer::start().await;
    let response = json!({"id":"resp", "model":"test-model", "status":"completed", "output":[
        {"type":"function_call", "id":"fc_a", "call_id":"tool_a", "name":"alpha", "arguments":"{\"x\":1}"},
        {"type":"function_call", "id":"fc_b", "call_id":"tool_b", "name":"beta", "arguments":"{\"x\":1}"}]});
    let body = format!(
        "data: {}\n\n",
        json!({"type":"response.completed", "response":response})
    );
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let mut cfg = OpenAiResponsesConfig::new("responses:audit", "unused-synthetic-key");
    cfg.auth_kind = AuthKind::BedrockMantle;
    cfg.base_url = server.uri();
    // This lane avoids reading/writing the first-party persistent cookie jar.
    cfg.mantle = Some(crate::mantle::MantleAuth {
        region: "us-east-1".into(),
        creds: crate::bedrock::auth::ResolvedCreds::Bearer {
            key: "synthetic-test-key".into(),
        },
    });
    assert_tools(
        OpenAiResponsesProvider::new(cfg)
            .complete(req())
            .await
            .unwrap(),
        &["tool_a", "tool_b"],
    );
}

#[cfg(feature = "bedrock")]
#[test]
fn bedrock_complete_response_normalizers_preserve_tool_ids_in_both_shapes() {
    use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider};
    for shape in [BedrockApiShape::Invoke, BedrockApiShape::Converse] {
        let cfg = BedrockConfig {
            id: "bedrock:audit".into(),
            region: "us-east-1".into(),
            model_id: "m".into(),
            api_shape: shape,
            creds: BedrockCreds::BearerKey {
                key: "synthetic-test-key".into(),
            },
            user_agent: None,
            header_extras: vec![],
            anthropic_beta: vec![],
            allowed_betas: vec![],
            allowed_body_fields: vec![],
            additional_model_request_fields: None,
            adaptive_thinking: None,
        };
        let provider = BedrockProvider::new(
            cfg,
            crate::bedrock::auth::ResolvedCreds::Bearer {
                key: "synthetic-test-key".into(),
            },
        )
        .unwrap();
        let body = match shape {
            BedrockApiShape::Invoke => {
                json!({"id":"msg", "type":"message", "role":"assistant", "model":"m",
                "stop_reason":"tool_use", "content":[{"type":"tool_use","id":"tool_a","name":"alpha","input":{"x":1}}],
                "usage":{"input_tokens":1,"output_tokens":1}})
            }
            BedrockApiShape::Converse => json!({"output":{"message":{"role":"assistant","content":[
                {"toolUse":{"toolUseId":"tool_a","name":"alpha","input":{"x":1}}}]}},
                "stopReason":"tool_use", "usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}),
        };
        assert_tools(provider.normalize_response(body).unwrap(), &["tool_a"]);
    }
}

#[cfg(feature = "openai-compat")]
#[tokio::test]
async fn compat_complete_preserves_multiple_upstream_tool_ids() {
    use crate::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
    let server = server(
        json!({"id":"chat", "model":"test-model", "choices":[{"index":0,
        "finish_reason":"tool_calls", "message":{"role":"assistant", "tool_calls":[
            {"id":"tool_a", "type":"function", "function":{"name":"alpha","arguments":"{\"x\":1}"}},
            {"id":"call_1", "type":"function", "function":{"name":"beta","arguments":"{\"x\":1}"}}
        ]}}]}),
    )
    .await;
    let cfg = OpenAiCompatConfig {
        id: "compat:audit".into(),
        base_url: server.uri(),
        api_key: "synthetic-test-key".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: Default::default(),
        history_reasoning: Default::default(),
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    assert_tools(
        OpenAiCompatProvider::new(cfg)
            .complete(req())
            .await
            .unwrap(),
        &["tool_a", "call_1"],
    );
}
