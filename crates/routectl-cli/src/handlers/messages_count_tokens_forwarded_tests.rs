//! Handler-level forwarded count regression. No real Anthropic transport is
//! used: the fake provider inspects the canonical request at dispatch.

use super::*;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, TokenCount};
use routectl_router::config::CredentialSource;
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, Router};
use std::sync::Mutex;

struct CountingProvider {
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait::async_trait]
impl Provider for CountingProvider {
    fn id(&self) -> &'static str {
        "count-fake"
    }
    fn normalize_request(&self, _: &ChatRequest) -> routectl_core::Result<Value> {
        unreachable!()
    }
    fn normalize_response(&self, _: Value) -> routectl_core::Result<ChatResponse> {
        unreachable!()
    }
    async fn complete(&self, _: ChatRequest) -> routectl_core::Result<ChatResponse> {
        unreachable!()
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> routectl_core::Result<futures::stream::BoxStream<'static, routectl_core::Result<ChatChunk>>>
    {
        unreachable!()
    }
    async fn count_tokens(&self, req: ChatRequest) -> routectl_core::Result<TokenCount> {
        self.seen.lock().unwrap().push(req);
        Ok(TokenCount {
            input_tokens: 42,
            ..Default::default()
        })
    }
}

fn counting_state() -> (
    Arc<AppState>,
    tempfile::TempDir,
    Arc<Mutex<Vec<ChatRequest>>>,
) {
    let mut config = Config::default();
    config.providers.insert(
        "forwarded".into(),
        ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded),
    );
    config
        .providers
        .insert("own".into(), ProviderEntry::anthropic_api(""));
    for name in ["forwarded", "own"] {
        config.models.insert(
            format!("{name}-model"),
            ModelEntry::new(name, "claude-sonnet-4-5"),
        );
        config
            .aliases
            .insert(name.into(), AliasValue::Single(format!("{name}-model")));
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(CountingProvider {
        seen: Arc::clone(&seen),
    });
    let mut router = Router::new(Arc::new(config));
    let models = ["forwarded", "own"]
        .into_iter()
        .map(|name| {
            let nickname = format!("{name}-model");
            let model = Arc::new(routectl_router::ResolvedModel::new(
                nickname.clone(),
                name,
                provider.clone() as Arc<dyn Provider>,
                "claude-sonnet-4-5",
            ));
            (nickname, model)
        })
        .collect();
    router.install_resolved_models(models);
    let (state, dir) = AppState::for_test(Arc::new(arc_swap::ArcSwap::from(Arc::new(router))));
    (state, dir, seen)
}

fn count_req(model: &str) -> Request<Body> {
    let mut req = post_req(
        Some("application/json"),
        json!({
            "model": model, "messages": [{"role":"user", "content":"hi"}]
        })
        .to_string(),
    );
    req.headers_mut().insert(
        "authorization",
        "Bearer synthetic-count-token".parse().unwrap(),
    );
    req.headers_mut().insert(
        "x-claude-code-session-id",
        "synthetic-session".parse().unwrap(),
    );
    req.headers_mut()
        .insert("x-stainless-lang", "typescript".parse().unwrap());
    req
}

#[tokio::test]
async fn trusted_count_seam_captures_bearer_and_stainless_at_dispatch() {
    let (state, _dir, seen) = counting_state();
    let mut req = count_req("forwarded");
    req.headers_mut().insert(
        crate::ingress::MITM_PROXIED_HEADER,
        state.mitm_seam_nonce.header_value(),
    );
    let (status, body) = drive(state, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["input_tokens"], 42);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0]
            .routectl_internal
            .forwarded_bearer
            .as_ref()
            .unwrap()
            .expose(),
        "synthetic-count-token"
    );
    assert!(
        seen[0]
            .routectl_internal
            .stainless_headers
            .contains(&("x-stainless-lang".into(), "typescript".into()))
    );
}

#[tokio::test]
async fn direct_count_abuse_cannot_supply_forwarded_credential_and_own_coexists() {
    let (state, _dir, seen) = counting_state();
    for seam in [None, Some("spoofed")] {
        let mut req = count_req("forwarded");
        if let Some(seam) = seam {
            req.headers_mut()
                .insert(crate::ingress::MITM_PROXIED_HEADER, seam.parse().unwrap());
        }
        let (status, body) = drive(Arc::clone(&state), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missing_forwarded_bearer")
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "invalid seam must never call the forwarded seat"
        );
    }
    let (status, body) = drive(state, count_req("own")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].routectl_internal.forwarded_bearer.is_none());
    assert!(seen[0].routectl_internal.stainless_headers.is_empty());
}

#[tokio::test]
async fn trusted_count_seam_requires_bearer_and_identity_before_parse() {
    let (state, _dir, seen) = counting_state();
    for (missing, expected) in [
        ("authorization", StatusCode::UNAUTHORIZED),
        ("x-claude-code-session-id", StatusCode::BAD_REQUEST),
    ] {
        let mut req = count_req("forwarded");
        req.headers_mut().remove(missing);
        req.headers_mut().insert(
            crate::ingress::MITM_PROXIED_HEADER,
            state.mitm_seam_nonce.header_value(),
        );
        // Invalid body proves admission takes precedence over parse failure.
        *req.body_mut() = Body::from("not json");
        let (status, _) = drive(Arc::clone(&state), req).await;
        assert_eq!(status, expected);
    }
    assert!(seen.lock().unwrap().is_empty());
}
