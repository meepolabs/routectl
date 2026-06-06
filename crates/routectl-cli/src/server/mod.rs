use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router as AxumRouter;
use routectl_auth::{MemoryStore, SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{Config, Router};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::handlers;

pub mod auth;
pub mod file_watch;
pub mod request_id;
pub mod secrets;

use auth::TokenSet;
use file_watch::{ReloadRequest, WatchTarget};
pub use secrets::CompositeStore;

/// Shared state every axum handler reads from. The `Router` lives
/// behind an `ArcSwap` so the file-watch / SIGHUP coordinator can
/// hot-swap a freshly-built routing surface without per-request
/// locking; handlers `load_full()` once at entry and use that
/// snapshot for the lifetime of the request.
///
/// The two pre-v0.7 fields `strict_translation` and `max_body_bytes`
/// were folded back into the `Router.config` because both are
/// duplicates of `config.server.*` knobs and only `max_body_bytes`
/// has any read site outside the handler path (in `build_axum_router`,
/// where it is now read directly from the live config at the same
/// wiring step).
pub struct AppState {
    pub router: Arc<ArcSwap<Router>>,
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
///
/// `config_path`, when `Some`, is the resolved on-disk path of the
/// config file. The file-watch coordinator uses it to pick up edits
/// without a restart. `None` disables the config half of the watcher
/// (tests that build a `Config` in-memory have no path to watch).
pub async fn serve(
    config: Arc<Config>,
    host: &str,
    port: u16,
    unsafe_public: bool,
    config_path: Option<PathBuf>,
) -> Result<()> {
    check_bind_safety(host, unsafe_public)?;

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| Error::Config(format!("bind {addr}: {e}")))?;

    serve_on_listener(config, listener, config_path).await
}

/// Serve on an already-bound listener. Used by tests so the OS-assigned port
/// can be read back with `listener.local_addr()` before handing it over.
///
/// `config_path` follows the same semantics as `serve`: `Some(path)`
/// installs the file-watch + SIGHUP coordinator; `None` skips the
/// config-half of the watcher (the credentials half still wires when
/// the OAuth arm is available).
pub async fn serve_on_listener(
    config: Arc<Config>,
    listener: TcpListener,
    config_path: Option<PathBuf>,
) -> Result<()> {
    // Composite resolver: oauth:// refs flow through OAuthStore (the
    // routectl-managed credentials.json), everything else through
    // MemoryStore. Built ONCE up here so the same `Arc<dyn
    // SecretStore>` survives every Router rebuild on hot-reload --
    // hot-swapping the Router would otherwise re-construct the
    // OAuthStore (losing its in-memory cache + per-provider
    // single-flight refresh mutexes) on every config change.
    let composite = CompositeStore::open_default().await?;
    let oauth_store = composite.oauth_store();
    let secrets: Arc<dyn SecretStore> = Arc::new(composite);

    let router = build_router_from_config(config.clone(), secrets.clone()).await?;

    let bound = listener
        .local_addr()
        .map_err(|e| Error::Config(format!("local_addr: {e}")))?;

    let alias_list: Vec<&str> = config.aliases.keys().map(String::as_str).collect();
    tracing::info!(
        addr = %bound,
        aliases = ?alias_list,
        "routectl listening on http://{bound}"
    );

    // Resolve the three runtime log knobs (redact_prompts,
    // trace_body_bytes, trace_headers) once at server boot so the
    // matching `info` confirmation lines land before any TRACE
    // request fires. Without this, each reader's OnceLock initializes
    // at the first call from a body/header trace helper, which means
    // an operator who set the env var or `[log]` config after launch
    // would silently get the wrong policy.
    //
    // Per-knob resolution: env wins when set; otherwise the matching
    // `[log]` config field (when `Some(_)`); otherwise the hardcoded
    // default. The single `init_log_overrides` entrypoint seeds the
    // config-side fallbacks AND fires the three status emitters in
    // one atomic step, closing the seed-then-status ordering window
    // by structure (one public seeder, no second-seeder path).
    routectl_core::init_log_overrides(
        config.log.trace_headers,
        config.log.trace_body_bytes,
        config.log.redact_prompts,
    );

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

    let max_body_bytes = compute_max_body_bytes(&config);
    let router_swap = Arc::new(ArcSwap::from_pointee(router));
    let state = Arc::new(AppState {
        router: router_swap.clone(),
    });

    // Wire the file-watch + SIGHUP reload coordinator. Shutdown is
    // shared across the watcher, the SIGHUP listener, and the
    // coordinator task; they all observe the same `watch::Sender`
    // closing when `axum::serve` returns.
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let reload_handles = spawn_reload_pipeline(
        config.clone(),
        config_path.clone(),
        oauth_store,
        secrets.clone(),
        router_swap.clone(),
        shutdown_rx,
    );

    let app = build_axum_router(state, token_set, max_body_bytes);

    let serve_result = axum::serve(listener, app)
        .await
        .map_err(|e| Error::Config(format!("serve: {e}")));

    // Signal every reload-side task to shut down. Drop the handles
    // last so a hung task does not block server return; tokio
    // detaches them with the runtime.
    let _ = shutdown_tx.send(());
    drop(reload_handles);

    serve_result
}

/// Compute the effective `DefaultBodyLimit` value. Mirrors the legacy
/// behavior: zero in the config means fall through to the library
/// default; a non-zero value is honored.
fn compute_max_body_bytes(config: &Config) -> usize {
    let raw = usize::try_from(config.server.max_body_bytes).unwrap_or(usize::MAX);
    if raw == 0 {
        DEFAULT_MAX_BODY_BYTES
    } else {
        raw
    }
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

/// Build a `Router` from the parsed config + a shared
/// `Arc<dyn SecretStore>`. The store is hoisted out of this function
/// (the caller passes it in) so a hot-reload config change that
/// triggers a Router rebuild reuses the SAME store handle, preserving
/// the OAuthStore in-memory token cache and the per-provider
/// single-flight refresh mutex across rebuilds.
pub(crate) async fn build_router_from_config(
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
) -> Result<Router> {
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

    // Reject `[retry]` blocks that set both `retry_allowlist` and
    // `retry_denylist`. The two are mutually exclusive predicates;
    // failing here surfaces the conflict at startup rather than
    // letting the silently-ignored denylist mask operator intent.
    routectl_router::validate_retry_policy(&config)?;

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
/// `/v1/messages`. Operator-configurable via `[server] max_body_bytes`
/// (default 32 MiB; see `routectl_router::ServerConfig`). Used by
/// `compute_max_body_bytes` as the fallback when the operator-supplied
/// value is zero.
const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// `max_body_bytes` MUST already be the resolved effective value
/// (zero -> default mapped by `compute_max_body_bytes`). Private to
/// this module; the only call site is `serve_on_listener`.
fn build_axum_router(
    state: Arc<AppState>,
    token_set: Arc<TokenSet>,
    max_body_bytes: usize,
) -> AxumRouter {
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
        .layer(DefaultBodyLimit::max(max_body_bytes));

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

/// Spawn the file-watch task, the SIGHUP listener (cfg(unix)), and
/// the reload coordinator. The returned vector keeps the
/// `JoinHandle`s alive until `serve_on_listener` returns; dropping
/// them only detaches because each task observes the shared
/// `shutdown_rx` and exits cleanly.
fn spawn_reload_pipeline(
    initial_config: Arc<Config>,
    config_path: Option<PathBuf>,
    oauth_store: Option<Arc<routectl_auth::OAuthStore>>,
    secrets: Arc<dyn SecretStore>,
    router_swap: Arc<ArcSwap<Router>>,
    shutdown_rx: watch::Receiver<()>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    let mut targets: Vec<WatchTarget> = Vec::new();
    if let Some(path) = config_path.as_ref() {
        targets.push(WatchTarget::Config(path.clone()));
    }
    if let Some(store) = oauth_store.as_ref() {
        targets.push(WatchTarget::Credentials(store.path().to_path_buf()));
    }

    let (reload_tx, reload_rx) = mpsc::channel::<ReloadRequest>(16);

    match file_watch::spawn_watcher(targets, reload_tx.clone(), shutdown_rx.clone()) {
        Ok(handle) => handles.push(handle),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "file-watch init failed; SIGHUP remains as the reload escape hatch",
            );
        }
    }

    #[cfg(unix)]
    {
        let sighup_tx = reload_tx.clone();
        let sighup_shutdown = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            run_sighup_listener(sighup_tx, sighup_shutdown).await;
        }));
    }
    // Drop the original sender so the coordinator's `recv()` returns
    // None when every clone is closed. (cfg(unix) clones above keep
    // the channel open under the SIGHUP listener; the coordinator
    // exits via `shutdown.changed()` instead.)
    drop(reload_tx);

    let coordinator_handle = tokio::spawn(run_reload_coordinator(
        initial_config,
        config_path,
        oauth_store,
        secrets,
        router_swap,
        reload_rx,
        shutdown_rx,
    ));
    handles.push(coordinator_handle);

    handles
}

/// Listen for `SIGHUP` and fan each delivery into the reload channel
/// as a paired (Config + Credentials) full-rescan request. Sends are
/// best-effort: a closed coordinator (post-shutdown) silently drops.
#[cfg(unix)]
async fn run_sighup_listener(tx: mpsc::Sender<ReloadRequest>, mut shutdown: watch::Receiver<()>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sig = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to install SIGHUP handler; reload-via-signal disabled",
            );
            return;
        }
    };

    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            received = sig.recv() => {
                if received.is_none() {
                    return;
                }
                tracing::info!("SIGHUP received; triggering full config + credentials rescan");
                let _ = tx.send(ReloadRequest::Config).await;
                let _ = tx.send(ReloadRequest::Credentials).await;
            }
        }
    }
}

/// Drain `ReloadRequest`s and apply them. Each request is processed
/// to completion before the next is read so a Router swap and a
/// credentials reload do not interleave.
async fn run_reload_coordinator(
    mut current_config: Arc<Config>,
    config_path: Option<PathBuf>,
    oauth_store: Option<Arc<routectl_auth::OAuthStore>>,
    secrets: Arc<dyn SecretStore>,
    router_swap: Arc<ArcSwap<Router>>,
    mut reload_rx: mpsc::Receiver<ReloadRequest>,
    mut shutdown: watch::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            req = reload_rx.recv() => {
                let Some(req) = req else { return; };
                match req {
                    ReloadRequest::Credentials => {
                        handle_credentials_reload(&oauth_store).await;
                    }
                    ReloadRequest::Config => {
                        if let Some(new_config) = handle_config_reload(
                            config_path.as_ref(),
                            &current_config,
                            secrets.clone(),
                            &router_swap,
                        ).await {
                            current_config = new_config;
                        }
                    }
                }
            }
        }
    }
}

/// Apply a credentials reload. Successful reload emits one info
/// line; a parse / IO failure emits a warn and leaves the in-memory
/// cache untouched (the disk-first ordering invariant in
/// `OAuthStore::reload_from_disk`).
async fn handle_credentials_reload(oauth_store: &Option<Arc<routectl_auth::OAuthStore>>) {
    let Some(store) = oauth_store else {
        tracing::debug!(
            "credentials reload requested but OAuth store unavailable (no HOME/XDG); ignoring",
        );
        return;
    };
    match store.reload_from_disk().await {
        Ok(()) => {
            tracing::info!(
                path = %store.path().display(),
                "credentials reloaded from disk",
            );
        }
        Err(e) => {
            tracing::warn!(
                path = %store.path().display(),
                error = %e,
                "credentials reload failed; keeping previous in-memory cache",
            );
        }
    }
}

/// Apply a config reload. On any pre-build failure (read, parse,
/// validate, build) the function emits a warn and returns `None`
/// (the existing Router stays installed). On success it swaps the
/// new Router into `router_swap` atomically and returns the new
/// `Arc<Config>` so the coordinator can diff future reloads against
/// the live config rather than the original-startup config.
async fn handle_config_reload(
    config_path: Option<&PathBuf>,
    current_config: &Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    router_swap: &Arc<ArcSwap<Router>>,
) -> Option<Arc<Config>> {
    let Some(path) = config_path else {
        tracing::debug!("config reload requested but no config path was registered; ignoring",);
        return None;
    };

    let new_config = read_parse_validate_config(path).await?;

    let mut new_router = match build_router_from_config(new_config.clone(), secrets).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "config reload failed: router rebuild error; keeping previous router",
            );
            return None;
        }
    };

    // Carry over per-nickname runtime state (circuit-breaker counters,
    // RPM token buckets) from the outgoing Router so a hot-reload does
    // not reset gates that took time to build up.
    new_router.carry_over_runtime_state_from(&router_swap.load_full());

    router_swap.store(Arc::new(new_router));
    tracing::info!(
        path = %path.display(),
        "config reloaded; router rebuilt and swapped",
    );

    let restart_required = collect_restart_required_changes(current_config, &new_config);
    if !restart_required.is_empty() {
        tracing::warn!(
            restart_required = ?restart_required,
            "config reload swapped routing state, but the listed fields require a daemon restart to take effect",
        );
    }

    Some(new_config)
}

/// Read, parse, and validate the config at `path`. Returns `None` and
/// emits a warn on any failure so the coordinator can keep the previous
/// config installed. Pulled out of `handle_config_reload` to keep that
/// function focused on the swap + diff phases.
async fn read_parse_validate_config(path: &PathBuf) -> Option<Arc<Config>> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config reload failed: read error; keeping previous config",
            );
            return None;
        }
    };

    let new_config: Config = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config reload failed: parse error; keeping previous config",
            );
            return None;
        }
    };
    let new_config = Arc::new(new_config);

    // Unix-only: WARN when the config file is group/world-readable
    // and carries sensitive values. Non-fatal so dev setups with
    // literal: secrets still start; the operator is informed and can
    // restrict permissions when it matters.
    #[cfg(unix)]
    warn_if_config_world_readable(path, &new_config, &text);

    if let Err(e) = routectl_router::validate_bedrock_global_config(&new_config) {
        tracing::warn!(error = %e, "config reload rejected by validate_bedrock_global_config; keeping previous config");
        return None;
    }
    if let Err(e) = routectl_router::validate_reasoning_defaults(&new_config) {
        tracing::warn!(error = %e, "config reload rejected by validate_reasoning_defaults; keeping previous config");
        return None;
    }
    if let Err(e) = routectl_router::validate_alias_chain_targets(&new_config) {
        tracing::warn!(error = %e, "config reload rejected by validate_alias_chain_targets; keeping previous config");
        return None;
    }
    if let Err(e) = routectl_router::validate_retry_policy(&new_config) {
        tracing::warn!(error = %e, "config reload rejected by validate_retry_policy; keeping previous config");
        return None;
    }

    Some(new_config)
}

/// Emit a one-time WARN when `path` is group/world-readable AND the
/// config text carries listener auth tokens or `literal:` secrets.
/// Non-fatal: the caller keeps the config regardless so dev setups
/// that store credentials in plain TOML still start. Operators running
/// in shared environments should restrict the file to `0600`.
#[cfg(unix)]
fn warn_if_config_world_readable(path: &PathBuf, config: &Config, raw_text: &str) {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    let mode = meta.mode();
    // Group-read (0o040) or world-read (0o004).
    if (mode & 0o044) == 0 {
        return;
    }
    let has_server_tokens = config
        .server
        .auth
        .as_ref()
        .is_some_and(|a| !a.tokens.is_empty());
    let has_literal_secret = raw_text.contains("literal:");
    if has_server_tokens || has_literal_secret {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{:04o}", mode & 0o777),
            "config file is group/world-readable and carries secrets \
             ([server.auth].tokens or literal: values); restrict to 0600 \
             to prevent credential exposure",
        );
    }
}

/// Diff the previous config against the new one and return the
/// names of fields whose change requires a daemon restart to take
/// effect. Per the architect-validated decision: bind, listener
/// auth, the `DefaultBodyLimit` axum layer, and the three `[log]`
/// knobs (deliberately frozen behind `OnceLock` in
/// `routectl-core/src/log_safe.rs`) all stay restart-required.
fn collect_restart_required_changes(prev: &Config, next: &Config) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();

    if prev.server.host != next.server.host {
        out.push("server.host");
    }
    if prev.server.port != next.server.port {
        out.push("server.port");
    }
    let prev_tokens: &[String] = prev
        .server
        .auth
        .as_ref()
        .map(|a| a.tokens.as_slice())
        .unwrap_or(&[]);
    let next_tokens: &[String] = next
        .server
        .auth
        .as_ref()
        .map(|a| a.tokens.as_slice())
        .unwrap_or(&[]);
    if prev_tokens != next_tokens {
        out.push("server.auth.tokens");
    }
    if prev.server.max_body_bytes != next.server.max_body_bytes {
        out.push("server.max_body_bytes");
    }

    if prev.log.trace_headers != next.log.trace_headers {
        out.push("log.trace_headers");
    }
    if prev.log.trace_body_bytes != next.log.trace_body_bytes {
        out.push("log.trace_body_bytes");
    }
    if prev.log.redact_prompts != next.log.redact_prompts {
        out.push("log.redact_prompts");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn collect_restart_required_changes_flags_bind_and_log() {
        use routectl_router::{Config, ServerAuth, ServerConfig};

        let mut prev = Config::default();
        let mut next = Config::default();

        // Baseline: identical configs -> empty list.
        assert!(collect_restart_required_changes(&prev, &next).is_empty());

        // Host change -> server.host.
        next.server = ServerConfig {
            host: "0.0.0.0".into(),
            ..ServerConfig::default()
        };
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"server.host"), "got {changes:?}");

        // Token change -> server.auth.tokens.
        prev.server = ServerConfig::default();
        next.server = ServerConfig {
            auth: Some(ServerAuth {
                tokens: vec!["literal:tok-1".into()],
            }),
            ..ServerConfig::default()
        };
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"server.auth.tokens"), "got {changes:?}");

        // Log knob change -> log.redact_prompts.
        prev = Config::default();
        next = Config::default();
        next.log.redact_prompts = Some(true);
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"log.redact_prompts"), "got {changes:?}");
    }

    #[tokio::test]
    async fn build_router_twice_from_same_secrets_handle_succeeds() {
        // Hot-reload smoke test: rebuild a Router twice from the
        // same `Arc<dyn SecretStore>` handle. Pinning the no-panic
        // contract -- a regression that drops the per-provider
        // single-flight refresh mutex on rebuild would either error
        // on the second build or surface a dangling Arc here.
        use routectl_auth::MemoryStore;
        use routectl_router::Config;
        use std::sync::Arc;

        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let config = Arc::new(Config::default());

        let r1 = build_router_from_config(config.clone(), secrets.clone())
            .await
            .expect("first router build");
        let r2 = build_router_from_config(config.clone(), secrets.clone())
            .await
            .expect("second router build");

        // Sanity: each call returns a fresh Router but they share
        // the same Config (Arc-shared at construction time).
        assert!(Arc::ptr_eq(&r1.config, &config));
        assert!(Arc::ptr_eq(&r2.config, &config));
    }

    /// SIGHUP-only delivery: drive `run_sighup_listener` directly with no
    /// filesystem watcher in the picture. Sending SIGHUP to ourselves
    /// must produce exactly one `Config` followed by one `Credentials`
    /// `ReloadRequest` on the channel. A regression that breaks the
    /// signal -> mpsc fan-out (registration error, dropped sender, fused
    /// recv) cannot pass this test because no other path can emit those
    /// requests in this fixture. Pairs with the integration-level
    /// combined-path test in `tests/hot_reload.rs`.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sighup_listener_emits_paired_reload_requests_in_isolation() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        use std::time::Duration;

        // Arrange: pure SIGHUP -> channel rig. No file watcher, no
        // server, no reload coordinator.
        let (tx, mut rx) = mpsc::channel::<file_watch::ReloadRequest>(8);
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        tokio::spawn(run_sighup_listener(tx, shutdown_rx));

        // Yield until the spawned listener has installed its handler.
        // tokio::signal::unix::signal registers synchronously, but the
        // task needs to enter its select! loop before a signal landing
        // in the same instant is observable.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Act: deliver SIGHUP to ourselves.
        kill(Pid::from_raw(std::process::id() as i32), Signal::SIGHUP)
            .expect("kill(SIGHUP) to self");

        // Assert: one Config then one Credentials, in that order. A
        // 2s timeout is generous for the OS notify -> tokio signal
        // futex hop on a loaded CI runner.
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first reload request must arrive within 2s")
            .expect("channel must not close while listener is alive");
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("second reload request must arrive within 2s")
            .expect("channel must not close while listener is alive");

        assert_eq!(first, file_watch::ReloadRequest::Config);
        assert_eq!(second, file_watch::ReloadRequest::Credentials);
    }
}
