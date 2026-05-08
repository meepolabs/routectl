use std::sync::Arc;

use axum::Router as AxumRouter;
use routectl_auth::MemoryStore;
use routectl_core::{Error, Result};
use routectl_router::{Config, Router};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

use crate::handlers;

pub struct AppState {
    pub router: Arc<Router>,
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

    let state = Arc::new(AppState {
        router: Arc::new(router),
    });

    let app = build_axum_router(state);

    axum::serve(listener, app)
        .await
        .map_err(|e| Error::Config(format!("serve: {e}")))?;

    Ok(())
}

async fn build_router_from_config(config: Arc<Config>) -> Result<Router> {
    let secrets = MemoryStore::new();
    let mut router = Router::new(config.clone());

    for (name, entry) in &config.providers {
        match routectl_router::build_provider(name, entry, &secrets).await {
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

fn build_axum_router(state: Arc<AppState>) -> AxumRouter {
    use axum::routing::{get, post};

    AxumRouter::new()
        .route("/health", get(handlers::health::health))
        .route("/v1/models", get(handlers::models::list_models))
        .route(
            "/v1/chat/completions",
            post(handlers::chat_completions::chat_completions),
        )
        .route("/v1/messages", post(handlers::messages::messages))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
