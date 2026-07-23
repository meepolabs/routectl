//! Listener bind, axum router build, serve loop, and bounded drain.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router as AxumRouter;
use routectl_auth::{MemoryStore, SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{ActivationState, CatalogOverlay, Config, check_drift_and_persist_state};
use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::handlers;

use super::auth::{self, TokenSet};
use super::config_load::compute_max_body_bytes;
use super::reload::{
    ActivationTrigger, apply_activation, await_reload_tasks, spawn_reload_pipeline,
};
use super::router_build::build_router_from_config_with_overlay;
use super::{AppState, CompositeStore, check_bind_safety, k_rebuild, request_id, status_gate};

/// Bind a TCP listener, then serve. Exposes the bound address for tests.
///
/// `config_path`, when `Some`, is the resolved on-disk path of the
/// config file. The file-watch coordinator uses it to pick up edits
/// without a restart. `None` disables the config half of the watcher
/// (tests that build a `Config` in-memory have no path to watch).
///
/// `catalog_overlay` is the overlay [`load_effective_config`](super::config_load::load_effective_config) loaded
/// alongside `config` at the SAME cold-start read -- `main.rs` threads it
/// straight through rather than this function re-reading the overlay file
/// itself, so a config-path reload (which DOES re-read both) and the
/// cold-start boot never diverge on how the overlay was obtained. `serve`
/// has exactly one caller (`main.rs`'s `Cmd::Serve`), so its signature
/// carries the overlay directly; [`serve_on_listener`] stays a thin
/// empty-overlay wrapper over [`serve_on_listener_with_overlay`] because
/// it is ALSO the test seam a dozen integration tests call with an
/// in-memory `Config` and no loader involved at all.
pub async fn serve(
    config: Arc<Config>,
    catalog_overlay: Arc<CatalogOverlay>,
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

    serve_on_listener_with_overlay(config, catalog_overlay, listener, config_path).await
}

/// Format the one-line startup cache-policy banner from the two policy
/// switches. Pure so it can be unit-tested without booting a server.
pub(super) fn cache_policy_banner(auto_emit_top_level: bool, reduction: bool) -> String {
    format!(
        "cache policy: auto-emit top-level breakpoint {}, context reduction {}",
        if auto_emit_top_level {
            "enabled"
        } else {
            "disabled"
        },
        if reduction { "enabled" } else { "disabled" },
    )
}

/// Serve on an already-bound listener, with an EMPTY catalog overlay. Used
/// by the many integration tests that build a `Config` in-memory and never
/// go through the shared loader at all -- an empty overlay is what every
/// target already resolved to before the overlay existed, so this keeps
/// those tests behavior-unchanged. Kept as the STABLE public signature
/// (unlike `serve`, this one has a dozen external callers); production
/// boot goes through [`serve`] -> [`serve_on_listener_with_overlay`]
/// instead.
pub async fn serve_on_listener(
    config: Arc<Config>,
    listener: TcpListener,
    config_path: Option<PathBuf>,
) -> Result<()> {
    serve_on_listener_with_overlay(
        config,
        Arc::new(CatalogOverlay::default()),
        listener,
        config_path,
    )
    .await
}

/// Every `(provider_kind, upstream)` pair this boot's SELECTABLE
/// `[models]` entries reference. Mirrors
/// `routectl_router::apply_catalog_overlay`'s own per-model derivation
/// (`config.models` -> provider -> `ProviderEntry::kind_str`) so the
/// drift log's "in use" set matches what the router actually resolves,
/// without needing the built `Router` itself -- the pair is a pure
/// function of `config`.
fn in_use_catalog_selectors(config: &Config) -> Vec<(String, String)> {
    config
        .models
        .values()
        .filter(|entry| entry.selectable)
        .filter_map(|entry| {
            let provider_kind = config.providers.get(&entry.provider)?.kind_str();
            Some((provider_kind.to_string(), entry.upstream.clone()))
        })
        .collect()
}

/// The real `serve_on_listener` body. `config_path` follows the same
/// semantics as `serve`: `Some(path)` installs the file-watch + SIGHUP
/// coordinator; `None` skips the config-half of the watcher (the
/// credentials half still wires when the OAuth arm is available).
pub async fn serve_on_listener_with_overlay(
    config: Arc<Config>,
    catalog_overlay: Arc<CatalogOverlay>,
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

    let router =
        build_router_from_config_with_overlay(config.clone(), &catalog_overlay, secrets.clone())
            .await?;

    // Cross-version catalog drift observability: AFTER the router
    // build, so the in-use selectors below are the same set
    // `apply_catalog_overlay` just resolved against. Startup-only (not
    // wired into the config-reload path) -- `catalog_state.json` is a
    // separate, rebuildable file the overlay never touches, and this
    // call is fully self-contained: it never fails serve, no matter
    // what state its own file is in.
    //
    // Colocated with `config_path`'s directory rather than the
    // hardcoded `routectl_config_dir()`, and skipped entirely when
    // `config_path` is `None`: the many in-memory-`Config` test
    // helpers that call `serve_on_listener` (never `serve` itself)
    // pass `None` here precisely because they have no real on-disk
    // config to anchor a reload watch to either -- writing this file
    // to a hardcoded real path regardless would mean every `cargo
    // test` run mutates the developer's ACTUAL `~/.config/routectl/`
    // directory. When `config_path` IS `Some`, its parent directory is
    // `routectl_config_dir()` in every real deployment (see
    // `main.rs::resolve_config_path`) and is the test's own tempdir
    // when a test explicitly points `config_path` at one (e.g.
    // `hot_reload.rs`) -- either way this lands next to the config
    // that actually produced this boot's router.
    if let Some(dir) = config_path.as_deref().and_then(Path::parent) {
        check_drift_and_persist_state(
            &in_use_catalog_selectors(&config),
            &catalog_overlay,
            &dir.join("catalog_state.json"),
        );
    }

    // One-shot warm of the K-estimator session store from the usage
    // ledger, on the owned `router` BEFORE it is wrapped in the ArcSwap.
    // Best-effort: a missing / unreadable DB skips the warm and leaves the
    // store cold. Runs ONLY here at the initial bootstrap -- NOT on a
    // hot-reload, where `carry_over_k_store_from` preserves the live store
    // (re-warming there would clobber fresher live samples with older
    // ledger history).
    k_rebuild::warm_k_store_from_ledger(&config.usage.db_path, &router.k_session_store);

    let bound = listener
        .local_addr()
        .map_err(|e| Error::Internal(format!("local_addr: {e}")))?;

    let alias_list: Vec<&str> = config.aliases.keys().map(String::as_str).collect();
    tracing::info!(
        addr = %bound,
        aliases = ?alias_list,
        "routectl listening on http://{bound}"
    );

    let auto_emit = config.cache.auto_emit_top_level_breakpoint;
    let reduction = config.reduction.enabled;
    tracing::info!(
        auto_emit_top_level = auto_emit,
        reduction = reduction,
        "{}",
        cache_policy_banner(auto_emit, reduction)
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

    // HARD REFUSE, independent of --unsafe-public and independent of
    // listener auth tokens: a non-loopback bind with [mitm] enabled
    // would make the MITM front-proxy an open relay forwarding
    // arbitrary reachable callers' full-scope upstream credentials
    // (e.g. a pooled Anthropic OAuth token) to the real upstream
    // origin. That is a strictly higher stake than the "expose the
    // ingress API without auth" case the check below guards, so this
    // has no override -- unlike --unsafe-public, there is no flag that
    // makes running an MITM open relay a supported configuration.
    if config.mitm.is_some() && !bound.ip().is_loopback() {
        return Err(Error::Config(format!(
            "refusing to start with [mitm] enabled while bound to non-loopback address \
             `{bound}`; the MITM front-proxy would relay arbitrary reachable callers' \
             upstream credentials -- this refusal cannot be overridden with --unsafe-public"
        )));
    }

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

    // Compute the initial activation inventory and SEED its ArcSwap before
    // the reload coordinator spawns, so the first reload-triggered recompute
    // diffs against a populated baseline (not empty) and the startup
    // inventory is observable the moment the listener accepts traffic. The
    // startup diff runs against `ActivationState::default()` (empty), so the
    // initially-activated set surfaces as `trigger=startup` activation
    // events. Infallible: a broken/absent store yields Unresolved entries,
    // never a startup abort.
    let activation_swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));
    apply_activation(
        &oauth_store,
        &config,
        &activation_swap,
        ActivationTrigger::Startup,
    )
    .await;

    // Start the usage writer BEFORE building AppState. The writer opens
    // the DB once here and owns it for the daemon's lifetime; the
    // returned `UsageHandle` goes onto AppState (outside the ArcSwap, so
    // a Router hot-swap never disturbs it) while the owning `UsageWriter`
    // stays in this scope as the shutdown handle. The writer is started
    // unconditionally -- even when `usage.enabled == false` -- so the
    // runtime gate can flip live on reload without a restart.
    let (usage_handle, usage_writer) = build_usage_writer(&config);

    // Generated ONCE per process, regardless of whether `[mitm]` is
    // configured: shared with the ingress admission/capture gates via
    // `AppState` below and, only when the MITM proxy actually starts, with
    // its `MitmCtx` (the sole legitimate stamper of the seam header this
    // nonce guards). See `crate::ingress::MitmSeamNonce`.
    let mitm_seam_nonce = Arc::new(crate::ingress::MitmSeamNonce::generate());

    let state = Arc::new(AppState {
        router: router_swap.clone(),
        usage: usage_handle.clone(),
        activation: activation_swap.clone(),
        mitm_seam_nonce: mitm_seam_nonce.clone(),
    });

    // Wire the file-watch + SIGHUP reload coordinator. Shutdown is
    // shared across the watcher, the SIGHUP listener, and the
    // coordinator task; they all observe the same `watch::Sender`
    // closing when `axum::serve` returns.
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    // The MITM front-proxy listener (when `[mitm]` is configured) joins
    // the SAME shutdown channel as the reload pipeline below, and its
    // `JoinHandle` is folded into `reload_handles` so
    // `await_reload_tasks` waits on it too during graceful shutdown. A
    // startup failure here (cert/CA generation, or the proxy's own port
    // already in use) is logged loudly and does NOT fail `serve_on_listener`
    // -- routectl's own HTTP listener still starts and serves normally,
    // just without the MITM front (a degraded, not down, RC). The
    // earlier non-loopback hard-refuse above is the one MITM startup
    // condition that DOES fail the whole server, because it is a
    // security invariant rather than a resource-availability one.
    let mitm_proxy_handle = match config.mitm.as_ref() {
        Some(mitm) => {
            match start_mitm_proxy(mitm, bound.port(), shutdown_rx.clone(), mitm_seam_nonce).await {
                Ok(handle) => Some(handle),
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        "MITM proxy failed to start; routectl continues to serve without it",
                    );
                    None
                }
            }
        }
        None => None,
    };

    let mut reload_handles = spawn_reload_pipeline(
        config.clone(),
        catalog_overlay.clone(),
        config_path.clone(),
        oauth_store,
        secrets.clone(),
        router_swap.clone(),
        activation_swap,
        usage_handle,
        shutdown_rx,
    );
    if let Some(handle) = mitm_proxy_handle {
        reload_handles.push(handle);
    }

    let app = build_axum_router(state, token_set, max_body_bytes, config_path.clone(), bound);

    let serve_result = serve_with_bounded_drain(listener, app).await;

    // Graceful-shutdown ordering matters for a clean usage drain. The
    // writer thread exits only when its mpsc channel closes, i.e. when
    // EVERY `UsageHandle` clone (each holding a sender) is dropped and
    // the writer's own sender is closed. Two clones outlive the server
    // loop: the one inside `AppState` (already gone -- `app` consumed it
    // above) and the one the reload coordinator task carries. So:
    //   1. server stopped accepting (above),
    //   2. signal the reload-side tasks to stop,
    //   3. AWAIT them so the coordinator's `UsageHandle` clone is dropped,
    //   4. THEN drain -- at which point the only remaining sender is the
    //      one inside `usage_writer`, so closing it lets the thread
    //      drain-and-exit well within its bounded deadline.
    let _ = shutdown_tx.send(());
    await_reload_tasks(reload_handles).await;

    // Drain queued usage rows after the server stops accepting and every
    // producer-side handle is gone. The blocking 5s drain MUST run off a
    // runtime worker, so it is dispatched via spawn_blocking; a JoinError
    // is logged, never panicked on. The writer's own bounded deadline
    // keeps a wedged DB from hanging here.
    drain_usage_writer(usage_writer).await;

    serve_result
}

/// Build and spawn the MITM front-proxy listener from the operator's
/// `[mitm]` config block. `reinject_port` is the routectl HTTP
/// listener's OWN bound port (resolved from `bound.port()` in the
/// caller, never a config value) -- the proxy's re-inject leg targets
/// it. Returns the spawned task's `JoinHandle` on success; every
/// failure mode is a [`crate::proxy::listener::ProxyStartError`] the
/// caller logs and treats as non-fatal (see the call site's comment for
/// why a degraded MITM proxy never crashes the main server).
async fn start_mitm_proxy(
    mitm: &routectl_router::MitmConfig,
    reinject_port: u16,
    shutdown: watch::Receiver<()>,
    seam_nonce: Arc<crate::ingress::MitmSeamNonce>,
) -> std::result::Result<tokio::task::JoinHandle<()>, crate::proxy::listener::ProxyStartError> {
    let proxy_config = crate::proxy::listener::ProxyListenerConfig {
        listen_port: mitm.listen_port,
        cert_dir: mitm.cert_dir.clone(),
        mitm_host: mitm.mitm_host.clone(),
        upstream_origin: mitm.upstream_origin.clone(),
        reinject_port,
        tested_cc_version: mitm.tested_cc_version.clone(),
        seam_nonce,
    };
    let (listener, acceptor, ctx) = crate::proxy::listener::build_and_bind(proxy_config).await?;
    let listen_addr = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_default();
    tracing::info!(
        addr = %listen_addr,
        mitm_host = %mitm.mitm_host,
        "MITM front-proxy listening",
    );
    Ok(crate::proxy::listener::spawn(
        listener,
        acceptor,
        ctx,
        mitm.mitm_host.clone(),
        shutdown,
    ))
}

/// Construct the usage writer from `config.usage`. Always starts the
/// writer (even when disabled) so the runtime enabled-gate can flip
/// without a restart; construction never hard-fails (a DB open error
/// degrades the writer to a no-op drain loop internally).
pub(super) fn build_usage_writer(config: &Config) -> (UsageHandle, UsageWriter) {
    UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        config.usage.retention_days,
        config.usage.enabled,
    )
}

/// Flush queued usage rows on graceful shutdown. `UsageWriter::shutdown`
/// blocks up to ~5s draining and joining the writer thread, so it must
/// not run on a runtime worker -- dispatch it via `spawn_blocking`. A
/// `JoinError` (the blocking task panicked or was cancelled) is logged,
/// never propagated as a panic.
pub(super) async fn drain_usage_writer(usage_writer: UsageWriter) {
    tracing::info!("draining usage writer before shutdown");
    match tokio::task::spawn_blocking(move || usage_writer.shutdown()).await {
        Ok(()) => tracing::info!("usage writer drained"),
        Err(e) => tracing::error!(error = %e, "usage writer drain task failed"),
    }
}

/// Upper bound on how long the graceful drain waits for in-flight
/// requests (notably multi-minute streaming responses) to finish after
/// a SIGTERM/SIGINT before the server returns regardless. axum's
/// `with_graceful_shutdown` has no built-in drain timeout, so a hung
/// upstream stream would otherwise block `serve` from ever returning;
/// this cap guarantees a deploy/restart cannot wedge on one stuck
/// connection.
pub(super) const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

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
pub(super) async fn drain_deadline_watcher(signal_rx: &mut watch::Receiver<bool>) {
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
        use tokio::signal::unix::{SignalKind, signal};

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

pub(super) async fn resolve_listener_tokens(config: &Config) -> Result<Arc<TokenSet>> {
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
        // An empty (or all-whitespace) resolved token would silently
        // disable listener authentication: TokenSet would hold a value
        // that matches an absent / empty inbound credential. The
        // literal: parse guard cannot see this -- an env:// / file://
        // source can resolve to "" at use-time -- so reject it here by
        // position, never echoing the raw value.
        if value.trim().is_empty() {
            return Err(Error::Config(format!(
                "[server.auth].tokens entry #{entry}: resolved to an empty token; an empty listener token would disable authentication"
            )));
        }
        resolved.push(value);
    }
    tracing::info!(
        token_count = resolved.len(),
        "listener auth enabled (x-api-key or Authorization: Bearer required)"
    );
    Ok(Arc::new(TokenSet::new(resolved)))
}

/// Whether the read-only `/status*` subtree (and the `GET /` dashboard)
/// must sit behind the listener auth layer for this bind: true whenever
/// tokens are configured OR the bind is non-loopback. One decision point,
/// fail-closed by construction -- a no-tokens + non-loopback bind (which
/// the startup invariant at `serve_on_listener` already refuses) still
/// returns true, so status is never served auth-exempt on a public
/// address even if that invariant ever drifts.
pub(super) fn status_requires_auth(token_set: &TokenSet, bound: std::net::SocketAddr) -> bool {
    !token_set.is_empty() || !bound.ip().is_loopback()
}

/// `max_body_bytes` MUST already be the resolved effective value
/// (zero -> default mapped by `compute_max_body_bytes`). Private to
/// this module; the only call site is `serve_on_listener`.
///
/// `proxy::split::ANTHROPIC_INFERENCE_PATHS` is the source of truth for
/// which of these routes the MITM front-proxy classifies as
/// Anthropic-dialect inference traffic (re-injected here over loopback
/// rather than forwarded to the real Anthropic origin). Adding a NEW
/// Anthropic-dialect inference route below must also add its path to
/// that const. An integration test in `tests/server.rs` pins the const
/// itself: a change to `ANTHROPIC_INFERENCE_PATHS`'s literal set, or a
/// const path that stops being served here, shows up as a failing test.
/// It does NOT catch the reverse -- a new inference route added below
/// that forgets to also update the const -- so that direction of drift
/// relies on review, not CI; this is an accepted, deliberate tradeoff, not an oversight.
fn build_axum_router(
    state: Arc<AppState>,
    token_set: Arc<TokenSet>,
    max_body_bytes: usize,
    config_path: Option<PathBuf>,
    bound: std::net::SocketAddr,
) -> AxumRouter {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::{get, post};

    // Public routes: /health is intentionally outside the auth layer
    // so external liveness probes work in --unsafe-public deployments.
    let public = AxumRouter::new().route("/health", get(handlers::health::health));

    // Authenticated routes: /v1/models lists configured aliases (low
    // sensitivity but still gated when auth is on); /v1/chat/completions,
    // /v1/messages, and /v1/responses carry the body of every request and
    // forward upstream. /v1/messages/count_tokens is a probe call
    // claude-code uses for context-budget display.
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
        .route("/v1/responses", post(handlers::responses::responses))
        .layer(DefaultBodyLimit::max(max_body_bytes));

    // Mount the auth middleware only when tokens are configured.
    // Loopback dev (empty token list) gets the historical zero-auth
    // behavior with no per-request overhead.
    //
    // Clone the `Arc` for the status gate BEFORE `token_set` is moved
    // into the /v1 layer below; the status subtree reuses the same
    // `TokenSet` under its own auth layer.
    let status_token_set = Arc::clone(&token_set);
    if !token_set.is_empty() {
        authed = authed.layer(axum::middleware::from_fn_with_state(
            token_set,
            auth::auth_layer,
        ));
    }

    // The `/v1/*` + `/health` surface, with its own `AppState` erased away.
    let versioned = public.merge(authed).with_state(state.clone());

    // The read-only `/status*` subtree. It carries its OWN state and
    // subtree-only layers that `/v1/*` never inherits:
    //   1. a `Host` allowlist (anti-DNS-rebinding), applied OUTERMOST so a
    //      rejected host is turned away before it can consume a shed permit
    //      or reach the auth check;
    //   2. the listener auth layer, applied beneath the host guard whenever
    //      `status_requires_auth` holds (tokens configured OR non-loopback
    //      bind) -- so status/usage/health/config/doctor and the dashboard
    //      shell are gated exactly like `/v1/*`. Token-less loopback keeps
    //      the historical zero-auth dev path;
    //   3. a bounded-concurrency load-shed (see `status_gate`) on the JSON
    //      routes only, so a poller burst sheds as a 503 instead of
    //      contending with the proxy.
    // `with_state` erases `Router<Arc<StatusState>>` to `Router<()>` so it
    // merges into the state-erased `versioned` router below.
    //
    // The dashboard page (`GET /`, stateless `Router<()>`) is merged in
    // ALONGSIDE the JSON routes but OUTSIDE `apply_overload_layers`: it shares
    // the host guard + auth gate, yet does NOT consume the JSON shed
    // budget. A zero-I/O `&'static str` response cannot stall or hold a permit,
    // and keeping it off that budget means an overload sheds status DATA while
    // the operator's incident window (the shell) still loads.
    let status_state = crate::handlers::status::StatusState::from_app(&state, config_path);
    let status_allowlist = status_gate::StatusHostAllowlist::new(bound);
    let status_json = status_gate::apply_overload_layers(
        crate::handlers::status::status_router().with_state(Arc::new(status_state)),
    );
    let status_page = crate::handlers::status::page_router();
    let mut status_authed = status_json.merge(status_page);
    if status_requires_auth(&status_token_set, bound) {
        status_authed = status_authed.layer(axum::middleware::from_fn_with_state(
            status_token_set,
            auth::auth_layer,
        ));
    }
    let status = status_authed.layer(axum::middleware::from_fn_with_state(
        status_allowlist,
        status_gate::host_guard,
    ));

    versioned
        .merge(status)
        .layer(axum::middleware::from_fn(request_id::middleware))
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod serve_tests;
