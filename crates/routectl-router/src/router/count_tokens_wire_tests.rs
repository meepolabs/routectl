//! The wire side of a token count: each provider call gets a bare tap when
//! the request carries the client's body, a rejection is read against the
//! bytes the egress deposited there, and a rejection laid to an operator
//! payload extra is answered by the revert. The count body is an allowlist
//! projection of the client's, so nothing about it is compared or counted.
//!
//! The revert is driven through the real anthropic-api egress against a
//! wiremock upstream, so the asserted bodies are the bytes the upstream
//! received.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::wire_check::{IngressDialect, IngressWire, WireTap};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Message, MessageContent, Provider, Result, Role,
    TokenCount,
};
use routectl_testkit::CapturedEvent;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::config::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy};
use crate::factory::{BuildOptions, build_resolved_models};
use crate::resolved::ResolvedModel;
use crate::router::{Router, RouterOptions};

const ALIAS: &str = "a";
const EXTRA_KEY: &str = "mcp_servers";
const EDIT_REJECTED: &str = "routectl_edit_rejected";
const COUNT_PATH: &str = "/v1/messages/count_tokens";
const MESSAGES_PATH: &str = "/v1/messages";
const COUNTED: u32 = 42;

/// The operator's provider-level payload extras: one MCP server, a key the
/// count allowlist forwards and the client never sent.
fn operator_extras() -> Value {
    json!({EXTRA_KEY: [{"type": "url", "name": "s1", "url": "https://mcp.example.com/sse"}]})
}

/// An anthropic-api 400 refusing the operator's extra as the upstream words it.
fn extra_refused() -> Value {
    json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!("{EXTRA_KEY}: Extra inputs are not permitted"),
        }
    })
}

fn carries_extra(request: &Request) -> bool {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|body| body.get(EXTRA_KEY).cloned())
        .is_some()
}

/// An upstream that refuses any body carrying the extra on both the count
/// and the messages endpoint, and serves any body without it.
async fn refusing_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(COUNT_PATH))
        .respond_with(|request: &Request| {
            if carries_extra(request) {
                ResponseTemplate::new(400).set_body_json(extra_refused())
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"input_tokens": COUNTED}))
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(MESSAGES_PATH))
        .respond_with(|request: &Request| {
            if carries_extra(request) {
                ResponseTemplate::new(400).set_body_json(extra_refused())
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": "msg_ok",
                    "type": "message",
                    "role": "assistant",
                    "model": "upstream-model",
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                    "content": [{"type": "text", "text": "ok"}]
                }))
            }
        })
        .mount(&server)
        .await;
    server
}

/// A router whose alias is one real anthropic-api target at `base_url`,
/// carrying the operator's payload extras.
async fn wire_router(base_url: &str) -> Router {
    let entry = ProviderEntry::anthropic_api(crate::test_secret::file_ref("test-key"))
        .with_base_url(base_url)
        .with_payload_extras(operator_extras());
    let mut cfg = Config {
        providers: BTreeMap::from([("p0".to_owned(), entry)]),
        models: BTreeMap::from([("m0".to_owned(), ModelEntry::new("p0", "upstream-model"))]),
        aliases: BTreeMap::from([(ALIAS.to_owned(), AliasValue::Single("m0".to_owned()))]),
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            backoff_multiplier: 1.0,
            ..RetryPolicy::default()
        },
        ..Config::default()
    };
    cfg.capability.enabled = true;
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "provider build failures: {failed:?}");
    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(resolved);
    router
}

/// A request whose client body, captured as the Anthropic ingress does,
/// carries no extras.
fn client_request() -> ChatRequest {
    let client = json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]});
    let mut req = bare_request();
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect: IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&client).expect("serialize").into(),
    });
    req
}

/// The same request with no captured client body.
fn bare_request() -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(16),
        ..Default::default()
    }
}

async fn bodies(server: &MockServer, endpoint: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .filter(|request| request.url.path() == endpoint)
        .map(|request| serde_json::from_slice(&request.body).expect("JSON body"))
        .collect()
}

/// `(lane, path, edit_site)` of every `routectl_edit_rejected` warning.
fn edit_rejected_warnings(events: &[CapturedEvent]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter(|event| event.level == tracing::Level::WARN && event.message == EDIT_REJECTED)
        .map(|event| {
            let field = |name| event.field(name).unwrap_or_default().to_owned();
            (field("lane"), field("path"), field("edit_site"))
        })
        .collect()
}

#[tokio::test]
async fn a_declared_operator_extra_rejected_on_count_tokens_is_reverted() {
    let server = refusing_upstream().await;
    let router = wire_router(&server.uri()).await;

    let (counted, events) = Box::pin(routectl_testkit::with_capture(
        router.count_tokens_with_meta(client_request()),
    ))
    .await;

    let sent = bodies(&server, COUNT_PATH).await;
    assert_eq!(sent.len(), 2, "the refused count is re-sent once");
    assert_eq!(
        sent[0].get(EXTRA_KEY),
        Some(&operator_extras()[EXTRA_KEY]),
        "the first count carries the operator's extra"
    );
    assert!(
        sent[1].get(EXTRA_KEY).is_none(),
        "the re-sent count lacks the extra"
    );
    let count = counted.result.expect("the re-sent count is served");
    assert_eq!(count.input_tokens, COUNTED);
    assert!(
        counted.meta.learned_capabilities.is_empty(),
        "nothing is learned"
    );
    assert!(
        counted.meta.cleared_capabilities.is_empty(),
        "nothing is cleared"
    );
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "the learned registry stays empty"
    );
    assert_eq!(
        edit_rejected_warnings(&events),
        [(
            "anthropic->anthropic-api".to_owned(),
            format!("/{EXTRA_KEY}"),
            "operator_payload_extras".to_owned(),
        )],
        "one warning names the operator extra"
    );
}

#[tokio::test]
async fn count_traffic_leaves_wire_check_counters_unchanged() {
    let server = refusing_upstream().await;
    let router = wire_router(&server.uri()).await;
    let before = router.wire_check_snapshot();

    let counted = router.count_tokens_with_meta(client_request()).await;

    assert_eq!(bodies(&server, COUNT_PATH).await.len(), 2, "count refused");
    assert!(counted.result.is_ok(), "the count is served");
    assert_eq!(
        router.wire_check_snapshot(),
        before,
        "a count records no check, loss or finding"
    );

    // Positive control: the same rejection on a message dispatch through
    // the same router does move the wire check.
    let completed =
        Box::pin(router.complete_with_options(client_request(), RouterOptions::new())).await;

    assert!(completed.result.is_ok(), "the reverted message is served");
    let after = router.wire_check_snapshot();
    assert!(
        after
            .findings
            .iter()
            .any(|finding| finding.provenance.is_some()),
        "a message rejection records a rejection finding"
    );
    assert!(
        !after.checks.is_empty(),
        "a message success records a check"
    );
}

/// A count-capable provider recording whether each call carried a tap.
struct TapProbe {
    taps: Mutex<Vec<bool>>,
}

#[async_trait]
impl Provider for TapProbe {
    fn id(&self) -> &'static str {
        "tap-probe"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("tap-probe", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::normalize_response("tap-probe", "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("tap-probe", "unused"))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.taps
            .lock()
            .expect("taps lock")
            .push(req.routectl_internal.wire_tap.is_some());
        Ok(TokenCount {
            input_tokens: COUNTED,
            extras: Default::default(),
        })
    }
}

fn probe_router(probe: &Arc<TapProbe>) -> Router {
    let toml_text = format!(
        "version = 5\n[providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n\
         [aliases]\n{ALIAS} = [\"m0\"]\n"
    );
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let provider = Arc::clone(probe) as Arc<dyn Provider>;
    let model = ResolvedModel::new("m0", "p0", provider, "upstream");
    router.install_resolved_models(BTreeMap::from([("m0".to_owned(), Arc::new(model))]));
    router
}

#[tokio::test]
async fn a_count_without_ingress_wire_installs_no_tap() {
    for (name, request, tapped) in [
        ("no client body", bare_request(), false),
        ("client body captured", client_request(), true),
    ] {
        let probe = Arc::new(TapProbe {
            taps: Mutex::new(Vec::new()),
        });
        let router = probe_router(&probe);

        let count = router.count_tokens(request).await;

        assert!(count.is_ok(), "{name}: counted");
        assert_eq!(
            *probe.taps.lock().expect("taps lock"),
            [tapped],
            "{name}: tap on the one call"
        );
    }
}

#[test]
fn a_count_without_ingress_wire_clears_a_tap_it_carried_in() {
    // Arrange
    let mut attempt_req = bare_request();
    attempt_req.routectl_internal.wire_tap = Some(WireTap::default());

    // Act
    let tap = Router::install_count_wire_tap(&mut attempt_req);

    // Assert
    assert!(tap.is_none(), "no tap for the router to read");
    assert!(
        attempt_req.routectl_internal.wire_tap.is_none(),
        "no tap left for the egress to deposit into"
    );
}
