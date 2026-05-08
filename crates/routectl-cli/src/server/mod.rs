use std::sync::Arc;

use axum::Router as AxumRouter;
use routectl_auth::{MemoryStore, SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{Config, Router};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

use crate::handlers;

pub mod auth;

use auth::TokenSet;

pub struct AppState {
    pub router: Arc<Router>,
    pub openai_aliases: std::collections::BTreeMap<String, String>,
    pub anthropic_aliases: std::collections::BTreeMap<String, String>,
    pub strict_translation: bool,
}

/// Validate that `host` is loopback or that `unsafe_public` has been set.
/// Returns an error (without binding) if the check fails.
pub fn check_bind_safety(host: &str, unsafe_public: bool) -> Result<()> {
    if unsafe_public {
        if !is_loopback(host) {
            tracing::warn!(
                "WARNING: routectl bound to {host}, exposing your local LLM credentials \
                 to anyone reachable on this address"
            );
        }
        return Ok(());
    }
    if !is_loopback(host) {
        return Err(Error::Config(format!(
            "refusing to bind to non-loopback address `{host}`; \
             pass --unsafe-public to override"
        )));
    }
    Ok(())
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// Bind a TCP listener, then serve. Exposes the bound address for tests.
pub async fn serve(config: Arc<Config>, host: &str, port: u16, unsafe_public: bool) -> Result<()> {
    check_bind_safety(host, unsafe_public)?;

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| Error::Config(format!("bind {addr}: {e}")))?;

    serve_on_listener(config, listener).await
}

/// Serve on an already-bound listener. Used by tests so the OS-assigned port
/// can be read back with `listener.local_addr()` before handing it over.
pub async fn serve_on_listener(config: Arc<Config>, listener: TcpListener) -> Result<()> {
    let router = build_router_from_config(config.clone()).await?;

    let bound = listener
        .local_addr()
        .map_err(|e| Error::Config(format!("local_addr: {e}")))?;

    let alias_list: Vec<&str> = config.aliases.keys().map(String::as_str).collect();
    tracing::info!(
        addr = %bound,
        aliases = ?alias_list,
        "routectl listening on http://{bound}"
    );

    let token_set = resolve_listener_tokens(&config).await?;

    let state = Arc::new(AppState {
        router: Arc::new(router),
        openai_aliases: config.ingress.openai.aliases.clone(),
        anthropic_aliases: config.ingress.anthropic.aliases.clone(),
        strict_translation: config.server.strict_translation,
    });

    let app = build_axum_router(state, token_set);

    axum::serve(listener, app)
        .await
        .map_err(|e| Error::Config(format!("serve: {e}")))?;

    Ok(())
}

async fn resolve_listener_tokens(config: &Config) -> Result<Arc<TokenSet>> {
    let Some(auth) = config.server.auth.as_ref() else {
        return Ok(Arc::new(TokenSet::default()));
    };
    if auth.tokens.is_empty() {
        return Ok(Arc::new(TokenSet::default()));
    }

    let store = MemoryStore::new();
    let mut resolved: Vec<String> = Vec::with_capacity(auth.tokens.len());
    for uri in &auth.tokens {
        let secret_ref = SecretRef::parse(uri)
            .map_err(|e| Error::Config(format!("[server.auth].tokens entry `{uri}`: {e}")))?;
        let value = store
            .get(&secret_ref)
            .await
            .map_err(|e| Error::Config(format!("[server.auth].tokens entry `{uri}`: {e}")))?;
        resolved.push(value);
    }
    tracing::info!(
        token_count = resolved.len(),
        "listener auth enabled (x-api-key or Authorization: Bearer required)"
    );
    Ok(Arc::new(TokenSet::new(resolved)))
}

async fn build_router_from_config(config: Arc<Config>) -> Result<Router> {
    let secrets = MemoryStore::new();
    let mut router = Router::new(config.clone());

    let opts = routectl_router::BuildOptions::new()
        .with_strict_translation(config.server.strict_translation);

    for (name, entry) in &config.providers {
        match routectl_router::build_provider_with_options(name, entry, &secrets, opts).await {
            Ok(provider) => {
                router.register(name.clone(), provider);
            }
            Err(e) => {
                tracing::warn!(provider = %name, error = ?e, "skipping provider (build failed)");
            }
        }
    }

    Ok(router)
}

/// Maximum incoming JSON body size for `/v1/chat/completions` and
/// `/v1/messages`. 4 MiB easily fits the largest legitimate
/// Anthropic Messages request (long system prompt + tool defs +
/// long history) while preventing trivial OOM-DoS via a multi-GB
/// POST.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

fn build_axum_router(state: Arc<AppState>, token_set: Arc<TokenSet>) -> AxumRouter {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::{get, post};

    // Public routes: /health is intentionally outside the auth layer
    // so external liveness probes work in --unsafe-public deployments.
    let public = AxumRouter::new().route("/health", get(handlers::health::health));

    // Authenticated routes: /v1/models lists configured aliases (low
    // sensitivity but still gated when auth is on); /v1/chat/completions
    // and /v1/messages carry the body of every request and forward
    // upstream.
    let mut authed = AxumRouter::new()
        .route("/v1/models", get(handlers::models::list_models))
        .route(
            "/v1/chat/completions",
            post(handlers::chat_completions::chat_completions),
        )
        .route("/v1/messages", post(handlers::messages::messages))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));

    // Mount the auth middleware only when tokens are configured.
    // Loopback dev (empty token list) gets the historical zero-auth
    // behavior with no per-request overhead.
    if !token_set.is_empty() {
        authed = authed.layer(axum::middleware::from_fn_with_state(
            token_set,
            auth::auth_layer,
        ));
    }

    public
        .merge(authed)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
