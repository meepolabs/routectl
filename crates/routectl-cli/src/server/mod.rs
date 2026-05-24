use std::sync::Arc;

use axum::Router as AxumRouter;
use routectl_auth::{MemoryStore, SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{Config, Router};
use tokio::net::TcpListener;

use crate::handlers;

pub mod auth;
pub mod request_id;
pub mod secrets;

use auth::TokenSet;
pub use secrets::CompositeStore;

pub struct AppState {
    pub router: Arc<Router>,
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
    use std::net::IpAddr;
    use std::str::FromStr;
    match IpAddr::from_str(host) {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
        Err(_) => host == "localhost",
    }
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

    // Resolve the redaction env var once at server boot so the
    // `info`-level confirmation lands in the log before any TRACE
    // request fires. Without this, the OnceLock initializes at the
    // first traced body, which means an operator who set the var
    // after launching routectl would silently get unredacted traces.
    routectl_core::log_redaction_status();
    // Same shape for ROUTECTL_TRACE_BODY_BYTES: announce the
    // resolved cap once at boot so an operator capturing live-
    // traffic fixtures can see whether the override took effect
    // before any traced body fires.
    routectl_core::log_trace_body_cap_status();

    let token_set = resolve_listener_tokens(&config).await?;

    // Cross-check: a public bind (post-`--unsafe-public`) without
    // any configured listener tokens is a configuration mistake --
    // the operator's intent to "expose this address" only makes
    // sense alongside auth. Fail fast here rather than running an
    // open server. Loopback binds are exempt because the local-dev
    // workflow relies on token-less loopback access. Use IpAddr
    // semantics on the actually-bound address (covers 127.x.x.x,
    // ::1, ::ffff:127.0.0.1 etc.) instead of re-parsing the host
    // string.
    if !bound.ip().is_loopback() && token_set.is_empty() {
        return Err(Error::Config(format!(
            "refusing to serve on public bind `{bound}` without [server.auth].tokens; \
             configure listener auth before exposing routectl on a non-loopback address"
        )));
    }

    let state = Arc::new(AppState {
        router: Arc::new(router),
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
    // Composite resolver: oauth:// refs flow through OAuthStore (the
    // routectl-managed credentials.json), everything else through
    // MemoryStore. Built once per startup; cheap to clone
    // (Arc-shared). Wrapped in `Arc<dyn SecretStore>` so the factory
    // can share the same store handle into the per-provider
    // `ManagedToken` for `oauth://` refs (lives across the whole
    // server lifetime; refresh + 401 retry land in a prior change).
    let secrets: Arc<dyn routectl_auth::SecretStore> =
        Arc::new(CompositeStore::open_default().await?);
    let mut router = Router::new(config.clone());

    // Surface incoherent `[bedrock]` config (e.g. populated
    // `allowed_body_fields` missing routectl-mandatory keys) at
    // startup instead of at first-request 400. Empty lists are
    // pass-through and accepted; see `validate_bedrock_global_config`.
    routectl_router::validate_bedrock_global_config(&config)?;

    // Reject empty-string `thinking = ""` on any provider before
    // building, so the operator gets a clean error rather than
    // silently emitting `effort: ""` on every routed request.
    routectl_router::validate_reasoning_defaults(&config)?;

    // Reject `[aliases]` chains that reference unknown OR disabled
    // `[models.X]` nicknames. Without this, dispatching against a
    // typo'd alias chain returns `UnknownAlias` at request time with
    // no breadcrumb back to the misconfiguration; failing here gives
    // the operator the offending alias + nickname pair upfront.
    routectl_router::validate_alias_chain_targets(&config)?;

    let opts = routectl_router::BuildOptions::new()
        .with_strict_translation(config.server.strict_translation)
        .with_bedrock_allowed_betas(config.bedrock.allowed_betas.clone())
        .with_bedrock_allowed_body_fields(config.bedrock.allowed_body_fields.clone());

    // v0.6.0: walk `[models]` once, building one provider per unique
    // non-Bedrock provider entry (cached) and one provider per Bedrock
    // model. Failures are collected and only fatal when an `[aliases]`
    // chain references a model whose provider failed to build.
    let (resolved_models, failed) =
        routectl_router::build_resolved_models(&config, secrets, opts).await?;
    router.install_resolved_models(resolved_models);

    // Provider build failures are normally non-fatal (an operator
    // may have an unused-but-declared model whose provider creds
    // aren't set in the current environment). But a failed model
    // that an `[aliases.*]` entry actually references is a real
    // misconfiguration -- without this guard, the server starts
    // "healthy" and the first real request hits `Error::UnknownAlias`
    // at dispatch time, with no configuration-error breadcrumb to
    // follow. Fail loudly here so operators see the issue at startup,
    // not at first traffic.
    if !failed.is_empty() {
        let failed_models: std::collections::HashSet<&str> =
            failed.iter().map(|(n, _)| n.as_str()).collect();
        let mut blocking: Vec<String> = Vec::new();
        for (alias, entry) in &config.aliases {
            for nick in entry.nicknames() {
                if failed_models.contains(nick) {
                    blocking.push(format!("alias `{alias}` -> model `{nick}`"));
                }
            }
        }
        if !blocking.is_empty() {
            let detail = failed
                .iter()
                .map(|(n, e)| format!("  - {n}: {e}"))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(Error::Config(format!(
                "{} model(s) failed to build AND are referenced by routes:\n{}\n\
                 affected routes:\n  {}",
                failed.len(),
                detail,
                blocking.join("\n  "),
            )));
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
    // upstream. /v1/messages/count_tokens is a probe call claude-code
    // uses for context-budget display.
    let mut authed = AxumRouter::new()
        .route("/v1/models", get(handlers::models::list_models))
        .route(
            "/v1/chat/completions",
            post(handlers::chat_completions::chat_completions),
        )
        .route("/v1/messages", post(handlers::messages::messages))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::messages_count_tokens::count_tokens),
        )
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
        .layer(axum::middleware::from_fn(request_id::middleware))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::is_loopback;

    #[test]
    fn is_loopback_covers_full_127_range() {
        // Arrange + Act + Assert
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("127.0.0.2"));
        assert!(is_loopback("127.255.255.254"));
        assert!(is_loopback("::1"));
        assert!(is_loopback("localhost"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("192.168.1.1"));
        assert!(!is_loopback("not-an-address"));
    }

    #[test]
    fn is_loopback_handles_ipv4_mapped_ipv6() {
        // Arrange + Act + Assert: IPv4-mapped IPv6 addresses
        // (::ffff:127.x.x.x) must be treated as loopback; non-loopback
        // IPv4-mapped addresses must not be.
        assert!(is_loopback("::ffff:127.0.0.1"));
        assert!(!is_loopback("::ffff:192.168.1.1"));
    }
}
