use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use routectl_core::wire_check::IngressDialect;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, Provider,
    RequestProvenance, Role,
};
use routectl_router::{AliasValue, Config, ProviderEntry, ResolvedModel, Router};
use serde_json::{Value, json};

use super::{capture_ingress_wire, client_version};
use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::server::AppState;

fn user_agent(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::USER_AGENT, HeaderValue::from_str(value).unwrap());
    headers
}

#[test]
fn client_version_prefers_the_claude_cli_version_then_a_safe_product_token() {
    let long_token = format!("client/{}", "9".repeat(80));
    let cases: [(&str, Option<&str>); 6] = [
        ("claude-cli/2.1.246 (external, sdk-cli)", Some("2.1.246")),
        (
            "claude-cli/2.1.246.1e8 (external, cli)",
            Some("claude-cli/2.1.246.1e8"),
        ),
        ("Mozilla compat OpenAI/Python 1.40.0", Some("OpenAI/Python")),
        ("codex_cli_rs/0.40.0 (Linux)", Some("codex_cli_rs/0.40.0")),
        ("no product token here", None),
        (&long_token, None),
    ];

    for (ua, expected) in cases {
        assert_eq!(client_version(&user_agent(ua)).as_deref(), expected, "{ua}");
    }
}

#[test]
fn client_version_is_none_without_a_user_agent() {
    assert_eq!(client_version(&HeaderMap::new()), None);
}

#[test]
fn a_request_with_no_ingress_provenance_gets_no_ingress_wire() {
    let mut req = ChatRequest::default();

    capture_ingress_wire(&HeaderMap::new(), &Bytes::from_static(b"{}"), &mut req);

    assert!(req.routectl_internal.ingress_wire.is_none());
}

/// Serves every request and keeps each canonical request it was handed.
struct CapturingProvider {
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn id(&self) -> &'static str {
        "capturing"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value, Error> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse, Error> {
        Err(Error::normalize_response("capturing", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, Error> {
        self.seen.lock().unwrap().push(req);
        Ok(ChatResponse {
            id: "ok".into(),
            model: "m".into(),
            created: 1,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: Vec::new(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: None,
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk, Error>>, Error> {
        Err(Error::upstream("capturing", 501, "unused"))
    }
}

fn capturing_state() -> (Arc<AppState>, Arc<Mutex<Vec<ChatRequest>>>) {
    let mut providers = BTreeMap::new();
    providers.insert("p".to_string(), ProviderEntry::anthropic_api("literal:k"));
    let mut aliases = BTreeMap::new();
    aliases.insert("a".to_string(), AliasValue::Single("m".to_string()));
    let config = Arc::new(Config {
        providers,
        aliases,
        ..Default::default()
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingProvider {
        seen: Arc::clone(&seen),
    };
    let mut router = Router::new(config);
    let mut models = BTreeMap::new();
    models.insert(
        "m".to_string(),
        Arc::new(ResolvedModel::new("m", "p", Arc::new(provider), "upstream")),
    );
    router.install_resolved_models(models);
    let state = AppState::for_test(Arc::new(arc_swap::ArcSwap::from_pointee(router)));
    (state, seen)
}

/// Post `body` through the real ingress driver and return the canonical
/// request the provider received.
async fn posted_through<A: IngressAdapter + 'static>(
    adapter: A,
    body: &'static str,
) -> ChatRequest {
    let (state, seen) = capturing_state();
    let mut headers = user_agent("claude-cli/2.1.246 (external, cli)");
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let resp = ingress_handle(
        state,
        headers,
        None,
        Ok(Bytes::from_static(body.as_bytes())),
        adapter,
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let seen = seen.lock().unwrap();
    seen.first().expect("the provider was called").clone()
}

#[tokio::test]
async fn the_ingress_wire_carries_the_posted_body_dialect_and_version() {
    const ANTHROPIC: &str =
        r#"{"model":"a","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"x":1}"#;
    const CHAT: &str = r#"{"model":"a","messages":[{"role":"user","content":"hi"}],"x":1}"#;
    const RESPONSES: &str = r#"{"model":"a","input":"hi","x":1}"#;

    let cases = [
        (
            "anthropic",
            posted_through(AnthropicIngress, ANTHROPIC).await,
            ANTHROPIC,
            IngressDialect::Anthropic,
            RequestProvenance::AnthropicIngress,
        ),
        (
            "chat",
            posted_through(OpenAiIngress, CHAT).await,
            CHAT,
            IngressDialect::Chat,
            RequestProvenance::OpenaiIngress,
        ),
        (
            "responses",
            posted_through(ResponsesIngress, RESPONSES).await,
            RESPONSES,
            IngressDialect::Responses,
            RequestProvenance::ResponsesIngress,
        ),
    ];

    for (name, req, posted, dialect, provenance) in cases {
        let wire = req
            .routectl_internal
            .ingress_wire
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: no ingress wire"));
        assert_eq!(wire.bytes.as_ref(), posted.as_bytes(), "{name}");
        assert_eq!(wire.dialect, dialect, "{name}");
        assert_eq!(wire.client_version.as_deref(), Some("2.1.246"), "{name}");
        assert_eq!(req.routectl_internal.provenance, provenance, "{name}");
    }
}
