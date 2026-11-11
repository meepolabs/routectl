//! An openai-compat provider built by the factory reads its bearer from the
//! credential store per request for an `oauth://` ref, resolves an `env://`
//! ref once at build, and rotates a managed login on an upstream 401 before
//! the router's single retry.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use routectl_auth::{SecretRef, SecretStore};
use routectl_core::{ChatRequest, Message, MessageContent, Provider, Role};
use routectl_router::{
    AliasValue, BuildOptions, Config, Dispatched, ProviderEntry, ResolvedModel, Router,
    RouterOptions, build_provider_with_options,
};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const MANAGED_REF: &str = "oauth://test-loopback";
const STATIC_REF: &str = "env://ROUTECTL_COMPAT_TOKEN_SOURCE_UNUSED";
const STATIC_VALUE: &str = "static-compat-key";

/// A credential store whose managed token is `token-<generation>`; a 401
/// notification advances the generation. Counts every read and every
/// notification.
#[derive(Default)]
struct RotatingStore {
    generation: AtomicUsize,
    gets: AtomicUsize,
    auth_failures: AtomicUsize,
}

impl RotatingStore {
    fn rotate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
    fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }
    fn auth_failures(&self) -> usize {
        self.auth_failures.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SecretStore for RotatingStore {
    async fn get(&self, secret_ref: &SecretRef) -> routectl_core::Result<String> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        match secret_ref {
            SecretRef::OAuth { .. } => {
                Ok(format!("token-{}", self.generation.load(Ordering::SeqCst)))
            }
            SecretRef::Env(_) => Ok(STATIC_VALUE.to_string()),
            _ => Err(routectl_core::Error::Auth("unexpected ref scheme".into())),
        }
    }
    async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn on_auth_failure(&self, _: &SecretRef) -> routectl_core::Result<()> {
        self.auth_failures.fetch_add(1, Ordering::SeqCst);
        self.rotate();
        Ok(())
    }
}

/// Answers 401 to any bearer other than `accepted`, and a chat completion to
/// `accepted`. `None` accepts every bearer.
struct BearerGate {
    accepted: Option<&'static str>,
}

impl Respond for BearerGate {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let bearer = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if let Some(accepted) = self.accepted
            && bearer != format!("Bearer {accepted}")
        {
            return ResponseTemplate::new(401).set_body_json(json!({
                "error": {"message": "unauthorized", "type": "authentication_error"}
            }));
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-token-source",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
    }
}

async fn upstream(accepted: Option<&'static str>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(BearerGate { accepted })
        .mount(&server)
        .await;
    server
}

/// The `Authorization` header of every request the upstream received.
async fn bearers(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("requests recorded")
        .iter()
        .map(|r| {
            r.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        max_tokens: Some(8),
        ..Default::default()
    }
}

async fn build(entry: &ProviderEntry, store: &Arc<RotatingStore>) -> Arc<dyn Provider> {
    let secrets: Arc<dyn SecretStore> = store.clone();
    build_provider_with_options("compat", entry, secrets, BuildOptions::default())
        .await
        .expect("openai-compat provider builds")
}

/// A router whose alias `fast` is the single model `m` on `provider`.
fn router(entry: ProviderEntry, provider: Arc<dyn Provider>) -> Router {
    let mut config = Config::default();
    config.providers.insert("compat".into(), entry);
    config
        .aliases
        .insert("fast".into(), AliasValue::Chain(vec!["m".into()]));
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        "m".to_string(),
        Arc::new(ResolvedModel::new("m", "compat", provider, "gpt-4o")),
    )]));
    router
}

#[tokio::test]
async fn a_managed_ref_reads_the_current_token_on_every_request() {
    // Arrange
    let server = upstream(None).await;
    let store = Arc::new(RotatingStore::default());
    let entry = ProviderEntry::openai_compat(server.uri(), MANAGED_REF);
    let provider = build(&entry, &store).await;

    // Act: one request, a rotation in the store, a second request.
    provider.complete(request("gpt-4o")).await.expect("first");
    store.rotate();
    provider.complete(request("gpt-4o")).await.expect("second");

    // Assert
    assert_eq!(
        bearers(&server).await,
        vec!["Bearer token-0".to_string(), "Bearer token-1".to_string()],
    );
}

#[tokio::test]
async fn a_static_ref_is_resolved_once_at_build() {
    // Arrange
    let server = upstream(None).await;
    let store = Arc::new(RotatingStore::default());
    let entry = ProviderEntry::openai_compat(server.uri(), STATIC_REF);
    let provider = build(&entry, &store).await;
    let reads_at_build = store.gets();

    // Act
    provider.complete(request("gpt-4o")).await.expect("first");
    provider.complete(request("gpt-4o")).await.expect("second");

    // Assert
    assert_eq!(reads_at_build, 1, "the static ref is read at build");
    assert_eq!(store.gets(), 1, "requests never re-read a static ref");
    let expected = format!("Bearer {STATIC_VALUE}");
    assert_eq!(bearers(&server).await, vec![expected.clone(), expected]);
}

#[tokio::test]
async fn an_upstream_401_refreshes_the_managed_login_and_retries_once() {
    // Arrange: the upstream accepts only the token minted by one refresh.
    let server = upstream(Some("token-1")).await;
    let store = Arc::new(RotatingStore::default());
    let entry = ProviderEntry::openai_compat(server.uri(), MANAGED_REF);
    let provider = build(&entry, &store).await;
    let router = router(entry, provider);

    // Act
    let Dispatched { result, .. } = router
        .complete_with_options(request("fast"), RouterOptions::new())
        .await;

    // Assert
    result.expect("the retry with the refreshed token is served");
    assert_eq!(store.auth_failures(), 1, "one refresh");
    assert_eq!(
        bearers(&server).await,
        vec!["Bearer token-0".to_string(), "Bearer token-1".to_string()],
        "one rejected dial, one retry with the refreshed token",
    );
}
