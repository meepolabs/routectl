use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde_json::{Value, json};

use crate::proxy::forward::{
    DEFAULT_MAX_CONCURRENT_STREAMS, ForwardBody, ForwardRequest, ForwardState, STREAM_IDLE_WINDOW,
    build_client, forward,
};
use crate::proxy::metrics::{Leg, PathClass, ProxyMetrics};
use crate::server::AppState;

/// `GET /v1/models` -- discovery endpoint that lists every routable
/// identifier the server accepts on the `model` field.
///
/// Sources walked in order, deduplicated:
///   1. `[aliases]` keys     (wire model -> nickname/chain)
///   2. `[models]` keys      (wire model -> direct nickname)
///
/// Two classes of alias keys are intentionally omitted because they
/// are not selectable identifiers on their own:
///   - the literal `default` catch-all key, used as the fallback
///     when no other route matches
///   - any glob-pattern key (containing `*`, e.g. `claude-opus-*`),
///     used by the router for prefix matching at dispatch time but
///     not a model the client may select. This matters for clients
///     like claude-code 2.1.129+ with
///     `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` that surface
///     `/v1/models` ids as a picker -- listing `claude-opus-*`
///     would put a non-selectable pattern in front of the user.
///
/// Models with `selectable = false` are also omitted -- they exist
/// in the config (operator may be staging an entry) but the router
/// refuses to route to them.
///
/// On the forwarded (pure-proxy) lane, this local list is a fallback,
/// not the primary answer: when a `credential_source = "forwarded"`
/// provider is configured AND this request arrived via the MITM
/// reinject leg carrying a captured client bearer, the handler proxies
/// through to Anthropic's real `/v1/models` list instead (see
/// `forwarded_proxy_target`) and returns THAT response verbatim.
/// Every other case -- no forwarded provider, or a forwarded provider
/// configured but this particular request carries no captured bearer
/// (a direct call to the main listener, never routed through the MITM
/// seam) -- falls through to the local list below unchanged.
pub async fn list_models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Snapshot the live Router once so a hot-swap mid-request does
    // not mix old + new alias state in the response payload.
    let router = state.router.load_full();

    if let Some((outbound_headers, base_url)) =
        forwarded_proxy_target(&headers, &router, &state.mitm_seam_nonce)
        && let Some(resp) = proxy_models_through(&base_url, outbound_headers).await
    {
        return resp;
    }

    Json(local_models_list(&router)).into_response()
}

/// One entry of the local `/v1/models` payload. `context_length` is an
/// OpenRouter-compatible extension: an ABSENT key means routectl has no
/// confirmed window for that id, which the `skip_serializing_if` makes
/// structural -- no code path can emit `null` or `0` for it.
#[derive(serde::Serialize)]
struct ModelListEntry {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_length: Option<u32>,
}

/// Build the local alias/model discovery payload from `router`. No routing
/// side effects -- the window read goes through
/// `Router::context_window_for`, never `dispatch_chain`: the
/// forwarded-lane fallback path in [`list_models`] and every existing
/// local-discovery caller share this one implementation.
fn local_models_list(router: &routectl_router::Router) -> Value {
    let config = &router.config;
    let now = Utc::now().timestamp();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut entries: Vec<ModelListEntry> = Vec::new();
    let emit = |id: &str, entries: &mut Vec<ModelListEntry>, seen: &mut BTreeSet<String>| {
        if !seen.insert(id.to_string()) {
            return;
        }
        entries.push(ModelListEntry {
            id: id.to_string(),
            object: "model",
            created: now,
            owned_by: "routectl",
            context_length: router.context_window_for(id),
        });
    };

    for alias in config.aliases.keys() {
        // Skip the `default` catch-all key; it isn't a routable
        // identifier on its own.
        if alias == "default" {
            continue;
        }
        // Skip suffix-glob pattern keys (e.g. `claude-opus-*`) --
        // they're routing patterns the dispatcher matches against,
        // not selectable model ids. Surfacing them as picker entries
        // (claude-code 2.1.129+ gateway model discovery) would let a
        // user click a pattern and produce a request the server
        // can't dispatch.
        if alias.contains('*') {
            continue;
        }
        emit(alias, &mut entries, &mut seen);
    }
    // Skip `selectable = false` model nicknames -- they exist in the
    // config (operator may be staging an entry) but the router refuses
    // to route to them.
    for (nickname, model) in &config.models {
        if !model.selectable {
            continue;
        }
        emit(nickname, &mut entries, &mut seen);
    }

    json!({
        "object": "list",
        "data": entries,
    })
}

/// Decide, without touching the network, whether `/v1/models` should
/// proxy through to Anthropic's real list on the forwarded lane -- and
/// if so, the exact outbound headers + pinned origin to use. Pure:
/// safe to unit test without a live host (the WIRE gate's Anthropic
/// host pin cannot be driven through wiremock -- see the router
/// learnings this task inherited).
///
/// `None` on every fall-soft trigger, all of which the caller reads as
/// "stay on the local list, never a 5xx":
///   - no `credential_source = "forwarded"` provider is configured
///     (`Router::has_forwarded_provider`);
///   - the two-key capture gate the ingress path already uses
///     (`handlers::ingress_handle::forwarded_capture_armed`) is not
///     armed -- most commonly because this request never carried the
///     nonce-matching MITM seam header, i.e. it did not arrive via the
///     reinject leg. A direct `GET /v1/models` against the main
///     listener is exactly this case: capture is seam-gated, so it
///     never carries a captured bearer regardless of `[providers]`;
///   - no inbound `Authorization: Bearer` to relay -- routectl never
///     mints or substitutes a credential for this call;
///   - the configured forwarded provider's `base_url` is not (still,
///     defense-in-depth) pinned to `api.anthropic.com`. Config
///     validation already enforces this at load time; this is a
///     second checkpoint, mirroring the WIRE gate, so the full-scope
///     token is never sent anywhere else regardless.
///
/// When armed, the returned headers are the inbound set minus the
/// internal `x-routectl-mitm-proxied` seam marker, which must never
/// leak to a real upstream.
fn forwarded_proxy_target(
    headers: &HeaderMap,
    router: &routectl_router::Router,
    seam_nonce: &crate::ingress::MitmSeamNonce,
) -> Option<(HeaderMap, String)> {
    if !router.has_forwarded_provider() {
        return None;
    }
    if !crate::handlers::ingress_handle::forwarded_capture_armed(headers, router, seam_nonce) {
        return None;
    }
    crate::handlers::ingress_handle::extract_authorization_bearer(headers)?;
    let base_url = forwarded_provider_base_url(router)?;
    if !routectl_core::identity::anthropic::is_anthropic_api_host(&base_url) {
        return None;
    }

    let mut outbound_headers = headers.clone();
    outbound_headers.remove(crate::ingress::MITM_PROXIED_HEADER);
    Some((outbound_headers, base_url))
}

/// The `base_url` of the configured `credential_source = "forwarded"`
/// `anthropic-api` provider, if any. Config validation already
/// requires this be unique-enough for the pin to matter (only an
/// `anthropic-api` variant may set `credential_source = "forwarded"`
/// at all, and it must be pinned to `api.anthropic.com`) -- the first
/// match is authoritative.
fn forwarded_provider_base_url(router: &routectl_router::Router) -> Option<String> {
    router
        .config
        .providers
        .values()
        .find_map(|entry| entry.forwarded_base_url().map(str::to_owned))
}

/// Proxy `/v1/models` through to `base_url` using
/// `crate::proxy::forward`'s shared byte-forwarder -- the SAME
/// machinery both MITM split legs reuse -- rather than building a
/// bespoke client for this one call site. The lazily-built, process-wide
/// [`ForwardState`] (see [`forward_state`]) is constructed once and
/// reused across calls, exactly like the MITM proxy's own `ForwardState`.
///
/// `base_url` is the CALLER-validated, already-pinned origin (see
/// [`forwarded_proxy_target`]) -- this function does not re-validate
/// the host, only issues the request against whatever origin it is
/// given. Kept separate from the host pin so the forwarding mechanics
/// themselves stay testable against an injected test origin: the real
/// `api.anthropic.com` host cannot be driven through wiremock.
///
/// Fail-soft: a malformed `base_url`, an unreachable upstream, or any
/// non-2xx response returns `None` so the caller degrades to the local
/// model list rather than surfacing a proxy-side failure as a 5xx that
/// would block discovery.
async fn proxy_models_through(base_url: &str, headers: HeaderMap) -> Option<Response> {
    let state = forward_state()?;
    let upstream_base = reqwest::Url::parse(base_url).ok()?;

    let metrics = Arc::new(ProxyMetrics::new());
    let request = ForwardRequest {
        method: Method::GET,
        raw_path_and_query: "/v1/models".to_string(),
        headers,
        body: reqwest::Body::from(Vec::new()),
    };
    let response = forward(
        state,
        &metrics,
        &upstream_base,
        request,
        Leg::ControlPlane,
        PathClass::ControlPlane,
    )
    .await;

    if !response.status().is_success() {
        return None;
    }
    Some(into_axum_response(response))
}

/// Rebuild `response`'s status/headers/body as an axum [`Response`].
/// [`ForwardBody`] already satisfies axum's body bounds (`Send` +
/// `Data = Bytes` + an error convertible to `axum::BoxError`), so this
/// is a bare re-wrap, not a buffering copy.
fn into_axum_response(response: http::Response<ForwardBody>) -> Response {
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, axum::body::Body::new(body))
}

/// The process-wide [`ForwardState`] backing [`proxy_models_through`],
/// built once on first use and reused for the life of the process --
/// mirrors the MITM proxy's own `ForwardState` lifecycle without
/// sharing its instance (that one lives inside the proxy listener task,
/// which may never start at all when `[mitm]` is absent; this handler
/// needs its own regardless of whether the front-proxy is running).
/// `None` only if the shared [`build_client`] constructor itself fails
/// (no working TLS backend) -- logged once, then every call degrades to
/// the local list rather than retrying a build that will not succeed.
fn forward_state() -> Option<&'static ForwardState> {
    static STATE: std::sync::OnceLock<Option<ForwardState>> = std::sync::OnceLock::new();
    STATE
        .get_or_init(|| match build_client() {
            Ok(client) => Some(ForwardState::new(
                client,
                DEFAULT_MAX_CONCURRENT_STREAMS,
                STREAM_IDLE_WINDOW,
            )),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "failed to build the forwarded /v1/models proxy client; \
                     falling back to the local model list"
                );
                None
            }
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use axum::http::HeaderValue;
    use axum::http::header::AUTHORIZATION;
    use http_body_util::BodyExt;
    use routectl_router::{
        AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, Router, ServerConfig,
    };
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::ingress::MitmSeamNonce;

    async fn json_body_of(resp: Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn list_models_skips_glob_keys_and_default() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "p".into(),
            ProviderEntry::openai_compat("http://127.0.0.1:1", "literal:test-key"),
        );

        let mut models = BTreeMap::new();
        models.insert("haiku".into(), ModelEntry::new("p", "claude-haiku-x"));
        models.insert("opus".into(), ModelEntry::new("p", "claude-opus-x"));

        let mut aliases = BTreeMap::new();
        aliases.insert("claude-opus-*".into(), AliasValue::Single("opus".into()));
        aliases.insert("default".into(), AliasValue::Single("haiku".into()));
        aliases.insert("claude-3".into(), AliasValue::Single("haiku".into()));

        let config = Arc::new(Config {
            server: ServerConfig::default(),
            providers,
            aliases,
            retry: RetryPolicy::default(),
            models,
            ..Default::default()
        });
        let router = Router::new(config);
        let (state, _usage_dir) =
            AppState::for_test(Arc::new(arc_swap::ArcSwap::from_pointee(router)));

        let body = json_body_of(list_models(State(state), HeaderMap::new()).await).await;
        assert_eq!(body["object"], "list");

        let mut ids: Vec<String> = body["data"]
            .as_array()
            .expect("data must be an array")
            .iter()
            .map(|e| e["id"].as_str().expect("id must be a string").to_string())
            .collect();
        ids.sort();

        assert_eq!(
            ids,
            vec![
                "claude-3".to_string(),
                "haiku".to_string(),
                "opus".to_string(),
            ],
            "expected exactly [claude-3, haiku, opus] (sorted); got: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.contains('*')),
            "no entry may contain '*'; glob alias keys must be filtered: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id == "default"),
            "literal `default` catch-all must be filtered: {ids:?}"
        );
    }

    fn router_with_forwarded_provider() -> Arc<Router> {
        let mut providers = BTreeMap::new();
        providers.insert(
            "forwarded".to_string(),
            ProviderEntry::anthropic_api("")
                .with_credential_source(routectl_router::config::CredentialSource::Forwarded),
        );
        let config = Arc::new(Config {
            providers,
            ..Default::default()
        });
        Arc::new(Router::new(config))
    }

    fn router_without_forwarded_provider() -> Arc<Router> {
        Arc::new(Router::new(Arc::new(Config::default())))
    }

    fn headers_with(nonce: Option<&MitmSeamNonce>, bearer: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(nonce) = nonce {
            headers.insert(
                axum::http::HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER),
                nonce.header_value(),
            );
        }
        if let Some(bearer) = bearer {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {bearer}")).unwrap(),
            );
        }
        headers
    }

    // ---- forwarded_proxy_target: pure predicate, no network ----

    #[test]
    fn forwarded_proxy_target_is_none_without_a_forwarded_provider() {
        let router = router_without_forwarded_provider();
        let nonce = MitmSeamNonce::generate();
        let headers = headers_with(Some(&nonce), Some("sk-ant-oat01-tok"));

        assert!(forwarded_proxy_target(&headers, &router, &nonce).is_none());
    }

    #[test]
    fn forwarded_proxy_target_is_none_without_the_seam_header() {
        let router = router_with_forwarded_provider();
        let nonce = MitmSeamNonce::generate();
        // No seam header at all -- e.g. a direct GET /v1/models against
        // the main listener, never routed through the MITM reinject leg.
        let headers = headers_with(None, Some("sk-ant-oat01-tok"));

        assert!(
            forwarded_proxy_target(&headers, &router, &nonce).is_none(),
            "capture is seam-gated; a direct call must never proxy through"
        );
    }

    #[test]
    fn forwarded_proxy_target_is_none_without_a_bearer() {
        let router = router_with_forwarded_provider();
        let nonce = MitmSeamNonce::generate();
        let headers = headers_with(Some(&nonce), None);

        assert!(
            forwarded_proxy_target(&headers, &router, &nonce).is_none(),
            "no captured credential to relay -- routectl must never mint one"
        );
    }

    #[test]
    fn forwarded_proxy_target_is_armed_and_strips_the_seam_header() {
        let router = router_with_forwarded_provider();
        let nonce = MitmSeamNonce::generate();
        let headers = headers_with(Some(&nonce), Some("sk-ant-oat01-tok"));

        let (outbound, base_url) = forwarded_proxy_target(&headers, &router, &nonce)
            .expect("armed: forwarded provider configured, seam matches, bearer present");

        assert_eq!(base_url, "https://api.anthropic.com");
        assert!(
            outbound.get(crate::ingress::MITM_PROXIED_HEADER).is_none(),
            "the internal seam marker must never reach a real upstream"
        );
        assert_eq!(
            outbound.get(AUTHORIZATION).unwrap(),
            "Bearer sk-ant-oat01-tok",
            "the client's Authorization header must reach the outbound request untouched"
        );
    }

    // ---- proxy_models_through: forward mechanics, injected origin ----

    #[tokio::test]
    async fn proxy_models_through_passes_through_anthropics_response_on_success() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer sk-ant-oat01-tok",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"object": "list", "data": [{"id": "claude-x"}]})),
            )
            .mount(&mock)
            .await;

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer sk-ant-oat01-tok"),
        );

        let resp = proxy_models_through(&mock.uri(), headers)
            .await
            .expect("a successful upstream response must proxy through");
        assert_eq!(resp.status(), 200);
        let body = json_body_of(resp).await;
        assert_eq!(body["data"][0]["id"], "claude-x");
    }

    #[tokio::test]
    async fn proxy_models_through_fails_soft_on_a_non_success_upstream_status() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "bad token"})))
            .mount(&mock)
            .await;

        let resp = proxy_models_through(&mock.uri(), HeaderMap::new()).await;
        assert!(
            resp.is_none(),
            "a non-2xx upstream response must degrade to the local list, never surface directly"
        );
    }

    #[tokio::test]
    async fn proxy_models_through_fails_soft_when_upstream_is_unreachable() {
        // A closed loopback port: `forward` returns a synthetic 502
        // (BAD_GATEWAY), which must also degrade to the local list.
        let resp = proxy_models_through("http://127.0.0.1:1", HeaderMap::new()).await;
        assert!(resp.is_none());
    }

    // ---- list_models end-to-end: fallback paths never 5xx or hang on a live host ----

    #[tokio::test]
    async fn list_models_falls_back_to_local_list_when_no_bearer_was_captured() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "forwarded".to_string(),
            ProviderEntry::anthropic_api("")
                .with_credential_source(routectl_router::config::CredentialSource::Forwarded),
        );
        let mut aliases = BTreeMap::new();
        aliases.insert("heavy".to_string(), AliasValue::Single("haiku".into()));
        let config = Arc::new(Config {
            providers,
            aliases,
            ..Default::default()
        });
        let router = Router::new(config);
        let (state, _usage_dir) =
            AppState::for_test(Arc::new(arc_swap::ArcSwap::from_pointee(router)));

        // Seam header present but no captured bearer -- must never attempt
        // a network call, and must return the local list unchanged.
        let nonce_headers = headers_with(Some(&state.mitm_seam_nonce), None);
        let body = json_body_of(list_models(State(state.clone()), nonce_headers).await).await;
        assert_eq!(body["object"], "list");
        let ids: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"heavy"), "expected local alias list: {ids:?}");

        // No seam header at all -- same fallback.
        let body = json_body_of(list_models(State(state), HeaderMap::new()).await).await;
        assert_eq!(body["object"], "list");
    }
}
