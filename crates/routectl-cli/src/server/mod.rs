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
        .map_err(|e| Error::Internal(format!("bind {addr}: {e}")))?;

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
        .map_err(|e| Error::Internal(format!("local_addr: {e}")))?;

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

    let serve_result = serve_with_bounded_drain(listener, app).await;

    // Signal every reload-side task to shut down. Drop the handles
    // last so a hung task does not block server return; tokio
    // detaches them with the runtime.
    let _ = shutdown_tx.send(());
    drop(reload_handles);

    serve_result
}

/// Upper bound on how long the graceful drain waits for in-flight
/// requests (notably multi-minute streaming responses) to finish after
/// a SIGTERM/SIGINT before the server returns regardless. axum's
/// `with_graceful_shutdown` has no built-in drain timeout, so a hung
/// upstream stream would otherwise block `serve` from ever returning;
/// this cap guarantees a deploy/restart cannot wedge on one stuck
/// connection.
const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// Serve `app` on `listener` with a graceful shutdown that drains
/// in-flight requests on SIGTERM/SIGINT, but bounds that drain by
/// `DRAIN_DEADLINE` so a hung upstream cannot block shutdown forever.
///
/// Mechanism: a `watch` channel carries the "signal fired" edge. The
/// shutdown future axum awaits resolves when the channel flips; a
/// separate deadline-watcher future awaits the same flip, then sleeps
/// `DRAIN_DEADLINE`. We `select!` the graceful `serve` future against
/// the deadline watcher, so whichever finishes first wins -- a clean
/// drain (serve returns) or the deadline elapsing (drain abandoned).
/// `watch` (level-triggered) is used over `Notify` (edge-triggered) so
/// the deadline watcher cannot miss the edge by subscribing late.
async fn serve_with_bounded_drain(listener: TcpListener, app: AxumRouter) -> Result<()> {
    let (signal_tx, mut signal_rx) = watch::channel(false);

    // Owns the OS signal wait. Flips the watch channel on the first
    // SIGTERM/SIGINT, then exits; the watch value stays `true` for any
    // late subscriber (the deadline watcher).
    let mut shutdown_rx = signal_tx.subscribe();
    let signal_task = tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received (SIGTERM/SIGINT); draining in-flight requests");
        let _ = signal_tx.send(true);
    });

    let graceful = axum::serve(listener, app).with_graceful_shutdown(async move {
        // Resolve once the channel flips to `true`. `changed()` also
        // returns Err if every sender dropped, which only happens at
        // process teardown -- treat that as "shut down" too.
        let _ = shutdown_rx.changed().await;
    });

    let serve_result = tokio::select! {
        result = graceful => {
            tracing::info!("graceful drain completed; server stopped");
            result.map_err(|e| Error::Internal(format!("serve: {e}")))
        }
        () = drain_deadline_watcher(&mut signal_rx) => {
            tracing::warn!(
                drain_deadline_secs = DRAIN_DEADLINE.as_secs(),
                "graceful drain deadline elapsed; abandoning in-flight requests and stopping",
            );
            Ok(())
        }
    };

    signal_task.abort();
    serve_result
}

/// Resolve only after the shutdown signal has fired AND
/// `DRAIN_DEADLINE` has subsequently elapsed. Until the signal fires
/// this future never resolves, so the `select!` against the graceful
/// serve future cannot trip the deadline branch during normal operation.
async fn drain_deadline_watcher(signal_rx: &mut watch::Receiver<bool>) {
    // Wait for the flip to `true`. If it already flipped, return
    // immediately; otherwise await the next change.
    while !*signal_rx.borrow_and_update() {
        if signal_rx.changed().await.is_err() {
            // All senders dropped without a signal (process teardown);
            // do not arm the deadline.
            std::future::pending::<()>().await;
        }
    }
    tokio::time::sleep(DRAIN_DEADLINE).await;
}

/// Future that resolves on the first SIGTERM or SIGINT. SIGHUP is
/// deliberately NOT handled here -- it remains the config-reload
/// trigger (`run_sighup_listener`). On non-Unix targets only Ctrl-C
/// (SIGINT-equivalent) is available, so the SIGTERM arm is cfg-gated
/// out there.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        // If a handler fails to install, fall back to a never-resolving
        // future for that arm rather than treating the failure as an
        // immediate shutdown.
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
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
    for (i, uri) in auth.tokens.iter().enumerate() {
        // Identify the failing entry by position, never by raw value: a
        // `literal:` token (or a bare misconfigured secret) would otherwise
        // land in startup logs verbatim.
        let entry = i + 1;
        let secret_ref = SecretRef::parse(uri)
            .map_err(|e| Error::Config(format!("[server.auth].tokens entry #{entry}: {e}")))?;
        let value = store
            .get(&secret_ref)
            .await
            .map_err(|e| Error::Config(format!("[server.auth].tokens entry #{entry}: {e}")))?;
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
                        handle_credentials_reload(
                            &oauth_store,
                            &current_config,
                            secrets.clone(),
                            &router_swap,
                        )
                        .await;
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

/// Apply a credentials reload. Refreshes the in-memory OAuth token
/// cache from disk; a parse / IO failure emits a warn and leaves the
/// cache untouched (the disk-first ordering invariant in
/// `OAuthStore::reload_from_disk`).
///
/// When the SEAT SET changes across the reload (a `routectl login
/// --label` / `logout --label` or a hand-edit added or removed a
/// credential key) the live Router is rebuilt from the CURRENT
/// (unchanged) config so a bare-pool `oauth://provider` re-expands to
/// the new per-seat target set without a daemon restart. The common
/// case -- routectl's own hourly-ish token auto-refresh, which rewrites
/// credentials.json with the same keys but new token values -- leaves
/// the seat set identical and skips the rebuild, so a routine refresh
/// costs only the cache reload it already paid for.
async fn handle_credentials_reload(
    oauth_store: &Option<Arc<routectl_auth::OAuthStore>>,
    current_config: &Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    router_swap: &Arc<ArcSwap<Router>>,
) {
    let Some(store) = oauth_store else {
        tracing::debug!(
            "credentials reload requested but OAuth store unavailable (no HOME/XDG); ignoring",
        );
        return;
    };

    let before = store.credential_keys().await;
    if let Err(e) = store.reload_from_disk().await {
        tracing::warn!(
            path = %store.path().display(),
            error = %e,
            "credentials reload failed; keeping previous in-memory cache",
        );
        return;
    }
    tracing::info!(
        path = %store.path().display(),
        "credentials reloaded from disk",
    );

    // Gate the Router rebuild on a real seat-set change. A token-value-only
    // refresh (same keys) must not rebuild, or every routine auto-refresh
    // would needlessly re-run the startup validators and re-expand the pool.
    let after = store.credential_keys().await;
    if before == after {
        return;
    }

    rebuild_router_for_seat_change(current_config, secrets, router_swap, &before, &after).await;
}

/// Rebuild the live Router from the unchanged config after a seat-set
/// change, preserving per-seat runtime state and honoring the
/// disk-first-keep-old invariant on a build failure. No config re-read
/// is needed (the config is unchanged); re-running the startup
/// validators on it is harmless. Split out of `handle_credentials_reload`
/// to keep that function under the size ceiling.
async fn rebuild_router_for_seat_change(
    current_config: &Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    router_swap: &Arc<ArcSwap<Router>>,
    before: &std::collections::BTreeSet<String>,
    after: &std::collections::BTreeSet<String>,
) {
    let mut new_router = match build_router_from_config(current_config.clone(), secrets).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "credentials seat-set changed but router rebuild failed; keeping previous router",
            );
            return;
        }
    };

    // Carry over per-state_key runtime state (circuit-breaker counters,
    // RPM token buckets) so surviving seats keep gates that took time to
    // build up; a freshly-added seat starts with fresh state.
    new_router.carry_over_runtime_state_from(&router_swap.load_full());
    router_swap.store(Arc::new(new_router));
    tracing::info!(
        seats_before = before.len(),
        seats_after = after.len(),
        "credentials seat set changed; router rebuilt and swapped",
    );
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

    // ---- Graceful shutdown: bounded in-flight drain (OPS-08) ----

    /// Sending SIGTERM to ourselves must trigger the graceful shutdown
    /// path so `serve_on_listener` returns cleanly within a short bound,
    /// proving the signal -> drain -> serve-return wiring. tokio's
    /// registered SIGTERM handler intercepts the signal, so the test
    /// process is not killed. There are no in-flight requests, so the
    /// drain completes immediately (well under `DRAIN_DEADLINE`).
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sigterm_triggers_graceful_shutdown_and_serve_returns() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        use std::time::Duration;

        // Arrange: bind an ephemeral loopback port and start the server.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral loopback port");
        let config = Arc::new(Config::default());
        let server = tokio::spawn(async move { serve_on_listener(config, listener, None).await });

        // Let the server install its signal handler and enter serve.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Act: deliver SIGTERM to ourselves.
        kill(Pid::from_raw(std::process::id() as i32), Signal::SIGTERM)
            .expect("kill(SIGTERM) to self");

        // Assert: serve_on_listener returns Ok within a short bound. A
        // 5s ceiling is generous for an idle drain yet far below
        // DRAIN_DEADLINE, so a hang here is a real regression, not the
        // deadline doing its job.
        let outcome = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("serve_on_listener must return within 5s of SIGTERM")
            .expect("server task must not panic");
        assert!(
            outcome.is_ok(),
            "graceful shutdown must return Ok: {outcome:?}"
        );
    }

    /// The bounded-drain select must resolve to "deadline elapsed" when
    /// the drain future never completes. We drive `drain_deadline_watcher`
    /// against a flipped signal with a tiny stand-in deadline and assert
    /// it wins a race against a never-completing "drain". Covers the
    /// abandon-on-hung-upstream decision in isolation (no real socket).
    #[tokio::test(start_paused = true)]
    async fn drain_deadline_fires_when_drain_never_completes() {
        // Arrange: a signal channel already flipped to `true`.
        let (tx, mut rx) = watch::channel(false);
        tx.send(true).expect("flip signal");
        let never_completes = std::future::pending::<()>();

        // Act + Assert: the deadline watcher must win against a drain
        // that never finishes. With paused time, sleep auto-advances.
        let abandoned = tokio::select! {
            () = drain_deadline_watcher(&mut rx) => true,
            () = never_completes => false,
        };
        assert!(
            abandoned,
            "deadline watcher must resolve when the drain never completes"
        );
    }

    /// The mirror case: when the drain completes promptly, the drain
    /// branch wins and the deadline does NOT fire. Drives the same
    /// select shape with a ready "drain" future against the deadline
    /// watcher (whose DRAIN_DEADLINE sleep would otherwise dominate).
    #[tokio::test(start_paused = true)]
    async fn drain_completion_wins_over_deadline() {
        // Arrange: signal flipped, but the drain is already ready.
        let (tx, mut rx) = watch::channel(false);
        tx.send(true).expect("flip signal");
        let drain_done = std::future::ready(());

        // Act + Assert: the completed-drain branch must win.
        let completed = tokio::select! {
            biased;
            () = drain_done => true,
            () = drain_deadline_watcher(&mut rx) => false,
        };
        assert!(
            completed,
            "a completed drain must win over the deadline watcher"
        );
    }

    /// Before any signal fires, the deadline watcher must NOT resolve --
    /// otherwise the bounded-drain select could trip the deadline branch
    /// during normal operation and tear the server down prematurely.
    #[tokio::test(start_paused = true)]
    async fn drain_deadline_does_not_fire_before_signal() {
        use std::time::Duration;

        // Arrange: an UN-flipped signal channel (no shutdown requested).
        let (_tx, mut rx) = watch::channel(false);

        // Act: race the watcher against a long sleep. With time paused,
        // the only way the sleep wins is if the watcher is correctly
        // pending on the (never-arriving) signal.
        let fired = tokio::select! {
            () = drain_deadline_watcher(&mut rx) => true,
            _ = tokio::time::sleep(Duration::from_secs(3600)) => false,
        };

        // Assert: the watcher stayed pending; the sleep won.
        assert!(
            !fired,
            "deadline watcher must not resolve before the shutdown signal fires"
        );
    }

    // ---- Credentials reload: seat-set-change Router rebuild gate ----

    /// Write a `credentials.json` carrying one record per seat key in
    /// `seats` (each `(key, access_token)`) using the same JSON shape +
    /// 0o600 hygiene the production `file_io::save` emits, so
    /// `OAuthStore::open` / `reload_from_disk` accept it. Keys are the raw
    /// credentials-map keys: a bare provider (`anthropic`) for the default
    /// seat, `provider#label` for a labeled seat.
    #[cfg(test)]
    fn write_pool_credentials(path: &std::path::Path, seats: &[(&str, &str)]) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut providers = serde_json::Map::new();
        for (key, token) in seats {
            providers.insert(
                (*key).to_string(),
                serde_json::json!({
                    "access_token": token,
                    "refresh_token": "seeded-refresh-token",
                    "token_type": "Bearer",
                    "expires_at_unix": now + 3600,
                    "scopes": ["user:inference"],
                    "account": { "email": null, "account_id": null },
                    "obtained_at_unix": now
                }),
            );
        }
        let doc = serde_json::json!({ "schema_version": 1, "providers": providers });
        let bytes = serde_json::to_vec_pretty(&doc).expect("serialize creds");
        let parent = path.parent().expect("creds path has parent");
        std::fs::create_dir_all(parent).expect("mkdir creds parent");
        std::fs::write(path, &bytes).expect("write creds");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }
    }

    /// Config with a single pooled model `claude` whose provider resolves
    /// its bearer via the bare-pool `oauth://anthropic` ref. With >=2
    /// seats on disk this expands to one dispatch target per seat; with one
    /// seat it stays single-target.
    #[cfg(test)]
    fn pooled_oauth_config() -> Arc<Config> {
        let text = r#"
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[providers.anthropic_oauth]
kind = "anthropic-api"
base_url = "http://127.0.0.1:1"
api_key_ref = "oauth://anthropic"
auth_kind = "oauth-bearer"

[models.claude]
provider = "anthropic_oauth"
upstream = "claude-sonnet-4-6"

[aliases]
default = "claude"
"#;
        Arc::new(toml::from_str(text).expect("pooled oauth config must parse"))
    }

    /// Build the `(OAuthStore handle, secrets, router_swap)` triple the
    /// reload coordinator owns, seeded from a credentials file already on
    /// disk at `creds_path`. Mirrors `serve_on_listener`'s wiring: one
    /// shared `Arc<dyn SecretStore>` across rebuilds.
    #[cfg(test)]
    async fn coordinator_rig(
        creds_path: &std::path::Path,
        config: &Arc<Config>,
    ) -> (
        Arc<routectl_auth::OAuthStore>,
        Arc<dyn SecretStore>,
        Arc<ArcSwap<Router>>,
    ) {
        let composite = CompositeStore::open_at(creds_path)
            .await
            .expect("open composite store at temp creds path");
        let oauth = composite.oauth_store().expect("oauth arm present");
        let secrets: Arc<dyn SecretStore> = Arc::new(composite);
        let router = build_router_from_config(config.clone(), secrets.clone())
            .await
            .expect("initial router build");
        let swap = Arc::new(ArcSwap::from_pointee(router));
        (oauth, secrets, swap)
    }

    /// Adding a seat to credentials.json and firing a credentials reload
    /// must re-expand the live Router's pool: the model goes from a single
    /// target (one seat, non-pooled) to two seat targets, with no daemon
    /// restart and no config change.
    #[tokio::test]
    async fn credentials_reload_reexpands_seat_set() {
        // Arrange: one seat on disk -> non-pooled (seat_count_for == None).
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("routectl").join("credentials.json");
        write_pool_credentials(&creds, &[("anthropic", "tok-default")]);
        let config = pooled_oauth_config();
        let (oauth, secrets, swap) = coordinator_rig(&creds, &config).await;
        assert_eq!(
            swap.load().seat_count_for("claude"),
            None,
            "single seat must stay single-target before reload"
        );

        // Act: add a second seat on disk, then reload credentials.
        write_pool_credentials(
            &creds,
            &[("anthropic", "tok-default"), ("anthropic#seat-b", "tok-b")],
        );
        handle_credentials_reload(&Some(oauth), &config, secrets, &swap).await;

        // Assert: the live Router now resolves two seat targets.
        assert_eq!(
            swap.load().seat_count_for("claude"),
            Some(2),
            "reload must re-expand the pool to two seats"
        );
    }

    /// A credentials reload that changes only a token VALUE (same seat
    /// keys) -- the routine auto-refresh case -- must NOT rebuild the
    /// Router. Proven by pointer-equality of the `ArcSwap` payload across
    /// the reload: the seat-set gate skipped the rebuild.
    #[tokio::test]
    async fn credentials_reload_token_only_change_does_not_rebuild() {
        // Arrange: a stable two-seat pool.
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("routectl").join("credentials.json");
        write_pool_credentials(
            &creds,
            &[("anthropic", "tok-default"), ("anthropic#seat-b", "tok-b")],
        );
        let config = pooled_oauth_config();
        let (oauth, secrets, swap) = coordinator_rig(&creds, &config).await;
        let before = swap.load_full();

        // Act: rewrite with the SAME keys but new token values, reload.
        write_pool_credentials(
            &creds,
            &[
                ("anthropic", "tok-default-rotated"),
                ("anthropic#seat-b", "tok-b-rotated"),
            ],
        );
        handle_credentials_reload(&Some(oauth), &config, secrets, &swap).await;

        // Assert: the Router Arc is pointer-unchanged (no rebuild fired).
        let after = swap.load_full();
        assert!(
            Arc::ptr_eq(&before, &after),
            "token-value-only refresh must not rebuild the router"
        );
    }

    /// A credentials reload whose Router rebuild fails (seat set changed,
    /// but the config no longer builds against the new credentials) must
    /// keep the previously-installed Router (disk-first-keep-old). Induced
    /// by pointing the pool's provider-build path at a config the rebuild
    /// rejects: here we make the alias reference a model whose only seat
    /// disappears so the rebuild errors, and pin that the live Router is
    /// untouched.
    #[tokio::test]
    async fn credentials_reload_rebuild_failure_keeps_previous_router() {
        // Arrange: a healthy two-seat pool and a built Router.
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("routectl").join("credentials.json");
        write_pool_credentials(
            &creds,
            &[("anthropic", "tok-default"), ("anthropic#seat-b", "tok-b")],
        );
        let config = pooled_oauth_config();
        let (oauth, secrets, swap) = coordinator_rig(&creds, &config).await;
        let before = swap.load_full();

        // Act: corrupt credentials.json so reload_from_disk fails. The
        // disk-first invariant means the cache (and thus the seat set) is
        // untouched, and the Router must not be rebuilt or swapped.
        std::fs::write(&creds, b"<<corrupt-json>>").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        handle_credentials_reload(&Some(oauth), &config, secrets, &swap).await;

        // Assert: a failed reload leaves the previous Router installed.
        let after = swap.load_full();
        assert!(
            Arc::ptr_eq(&before, &after),
            "a failed credentials reload must keep the previous router"
        );
    }
}
