use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router as AxumRouter;
use routectl_auth::{LocalProbe, MemoryStore, SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{
    ActivationDelta, ActivationState, CatalogOverlay, Config, Router,
    check_drift_and_persist_state, compute_activation, diff_activation,
};
use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::handlers;

pub mod auth;
pub mod file_watch;
pub mod k_rebuild;
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
///
/// `usage` is the `Clone` producer handle for the usage-accounting
/// writer. It lives DIRECTLY on `AppState`, NOT inside the
/// `Arc<ArcSwap<Router>>`, so a Router hot-swap never rebuilds or
/// disturbs the writer (which owns a DB handle opened once at boot).
/// The owning `UsageWriter` (the shutdown handle) is kept in the
/// `serve_on_listener` scope, not here.
pub struct AppState {
    pub router: Arc<ArcSwap<Router>>,
    pub usage: UsageHandle,
    /// Auto-activation inventory: which routectl-owned OAuth providers
    /// currently carry a usable local credential. A SIBLING of `router`,
    /// NOT a field inside the `Arc<ArcSwap<Router>>` -- keeping it
    /// physically outside the Router makes "inventory never reaches
    /// dispatch" true by construction (the dispatch path reads only the
    /// Router). The reload coordinator swaps this alongside the router on a
    /// config or credentials change; `apply_activation` is the sole writer.
    pub activation: Arc<ArcSwap<ActivationState>>,
    /// The per-process value that makes the `x-routectl-mitm-proxied` seam
    /// header unspoofable in practice (see
    /// `crate::ingress::MitmSeamNonce`). Generated ONCE at server bootstrap
    /// and shared with the MITM proxy listener's `MitmCtx` (the sole
    /// legitimate stamper) via the SAME `Arc` -- never regenerated
    /// per-request, never logged, never in config.
    pub mitm_seam_nonce: Arc<crate::ingress::MitmSeamNonce>,
}

impl AppState {
    /// Test-only constructor: wraps `router` with a usage handle backed
    /// by a writer pointed at an isolated in-tempdir DB, so handler unit
    /// tests that build `AppState` directly never touch the real
    /// `~/.config/routectl/usage.db`. The owning `UsageWriter` is
    /// detached (dropped) -- the handle stays usable (it accepts-and-drops
    /// once the channel closes), which is all a non-usage handler test
    /// needs. Returns the `TempDir` guard; keep it alive for the test.
    #[cfg(test)]
    pub fn for_test(router: Arc<ArcSwap<Router>>) -> (Arc<Self>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("usage tempdir");
        let (usage, _writer) =
            UsageWriter::start(dir.path().join("usage.db"), CHANNEL_CAPACITY, 0, false);
        let state = Arc::new(Self {
            router,
            usage,
            activation: Arc::new(ArcSwap::from_pointee(ActivationState::default())),
            mitm_seam_nonce: Arc::new(crate::ingress::MitmSeamNonce::generate()),
        });
        (state, dir)
    }
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
///
/// `catalog_overlay` is the overlay [`load_effective_config`] loaded
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
fn cache_policy_banner(auto_emit_top_level: bool, reduction: bool) -> String {
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

    let app = build_axum_router(state, token_set, max_body_bytes);

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

/// Upper bound on how long graceful shutdown waits for a single
/// reload-side task (file watcher, SIGHUP listener, coordinator) to
/// observe the shutdown signal and return. Each task selects on the
/// shutdown `watch` channel and exits promptly, so this is a safety
/// cap, not the expected wait. Awaiting (not just dropping) the
/// coordinator handle is what releases its `UsageHandle` clone before
/// the usage drain runs.
const RELOAD_TASK_SHUTDOWN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Await each reload-side task to completion under a bounded per-task
/// deadline so their owned state -- notably the coordinator's
/// `UsageHandle` clone -- is dropped before the usage drain begins. A
/// `JoinError` or an elapsed timeout is logged and skipped; a wedged
/// task is left detached rather than blocking shutdown.
async fn await_reload_tasks(handles: Vec<tokio::task::JoinHandle<()>>) {
    for handle in handles {
        match tokio::time::timeout(RELOAD_TASK_SHUTDOWN_DEADLINE, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "reload task join failed during shutdown");
            }
            Err(_) => {
                tracing::warn!(
                    deadline_secs = RELOAD_TASK_SHUTDOWN_DEADLINE.as_secs(),
                    "reload task did not stop within deadline; detaching and continuing",
                );
            }
        }
    }
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
fn build_usage_writer(config: &Config) -> (UsageHandle, UsageWriter) {
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
async fn drain_usage_writer(usage_writer: UsageWriter) {
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

/// Build a `Router` from the parsed config + a shared
/// `Arc<dyn SecretStore>`, with an EMPTY catalog overlay. Thin wrapper kept
/// for the many call sites (this crate's own unit tests plus
/// `handlers::ingress_handle_tests`) that build a Router without caring
/// about the overlay -- an empty overlay is behaviorally identical to
/// "every target falls through to the baked catalog", which is what those
/// callers already exercised before the overlay existed. Callers that DO
/// care (the server's boot + reload paths) use
/// [`build_router_from_config_with_overlay`] instead.
#[cfg(test)]
pub(crate) async fn build_router_from_config(
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
) -> Result<Router> {
    build_router_from_config_with_overlay(config, &CatalogOverlay::default(), secrets).await
}

/// Build a `Router` from the parsed config + catalog overlay + a shared
/// `Arc<dyn SecretStore>`. The store is hoisted out of this function
/// (the caller passes it in) so a hot-reload config change that
/// triggers a Router rebuild reuses the SAME store handle, preserving
/// the OAuthStore in-memory token cache and the per-provider
/// single-flight refresh mutex across rebuilds.
pub(crate) async fn build_router_from_config_with_overlay(
    config: Arc<Config>,
    catalog_overlay: &CatalogOverlay,
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

    // Reject malformed `[aliases]` glob keys (embedded/bare asterisks)
    // at startup. Without this, `Router::new` warn-and-drops the
    // malformed key and the request mis-routes while `config check`
    // still reports ok.
    routectl_router::validate_alias_patterns(&config)?;

    // Reject the reserved `[retry.classes.feature-unsupported]` key and
    // any `[providers.X.class_overrides]` remap targeting a class the
    // router retries or debits for health. Advisory findings on the
    // same surface (a health-status source remapped away from breaker
    // accounting, an empty `ClassPolicy` block) are logged rather than
    // rejected.
    routectl_router::validate_class_policy(&config)?;
    for warning in routectl_router::class_policy_warnings(&config) {
        tracing::warn!(warning = %warning, "class policy warning");
    }

    // Reject malformed `[registry]` glob keys at startup so query-time
    // cost resolution never silently skips a key it cannot parse.
    routectl_router::validate_registry_patterns(&config)?;

    // Reject an incoherent `[mitm]` block (bad upstream_origin, a
    // listen_port colliding with [server] port, an empty mitm_host) at
    // startup. A no-op (`Ok(())`) when `[mitm]` is absent -- gated here
    // on `mitm.is_some()` purely for readability at the call site, since
    // the validator itself already treats absence as trivially valid.
    if config.mitm.is_some() {
        routectl_router::validate_mitm_config(&config)?;
    }

    // Provider-level credential_source coherence (forwarded => host pin +
    // empty api_key_ref; own => key present). Also runs in the cheap
    // pre-parse gate (`validate_effective_config`); repeated here because
    // this builder is also reachable without that gate (tests, callers
    // constructing a Config directly), and containment point (1) of the
    // forwarded-credential invariant must hold on every build path.
    routectl_router::validate_provider_credential_sources(&config)?;

    // Reject a degenerate `[cache_pricing]` override (unparseable selector
    // key or a multiplier that makes the break-even math degenerate) at
    // startup. Without this, a bad override silently goes inert at lookup
    // time and the operator never learns their correction did nothing;
    // failing here names the offending selector upfront.
    routectl_router::validate_overrides(&config.cache_pricing).map_err(Error::Config)?;

    // Advisory: warn (never fail) if the WHOLE baked catalog table's
    // snapshot has gone stale (> 90 days). A redesign dropped the per-row
    // `verified_at`, so this is now a single table-wide check rather than
    // per-cell (see `routectl_router::catalog::warn_if_stale`'s doc).
    routectl_router::catalog::warn_if_stale();

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
    // Stamp each resolved model's precomputed two-layer catalog merge
    // (baked table + this boot/reload's overlay) onto the table BEFORE
    // installing it, so `Router::record_would_trim` reads a resolved
    // `EffectiveRow` straight off the dispatch target instead of
    // re-resolving the merge per request.
    let resolved_models =
        routectl_router::apply_catalog_overlay(resolved_models, &config, catalog_overlay);
    router.install_resolved_models(resolved_models);
    // Stamp the overlay revision the resolved-model table was merged
    // against so a later hot-reload can detect an overlay change and
    // invalidate the learned-capability registry.
    router.note_overlay_revision(routectl_router::overlay_revision(catalog_overlay));

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
#[allow(clippy::too_many_arguments)]
fn spawn_reload_pipeline(
    initial_config: Arc<Config>,
    initial_overlay: Arc<CatalogOverlay>,
    config_path: Option<PathBuf>,
    oauth_store: Option<Arc<routectl_auth::OAuthStore>>,
    secrets: Arc<dyn SecretStore>,
    router_swap: Arc<ArcSwap<Router>>,
    activation_swap: Arc<ArcSwap<ActivationState>>,
    usage: UsageHandle,
    shutdown_rx: watch::Receiver<()>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    let mut targets: Vec<WatchTarget> = Vec::new();
    if let Some(path) = config_path.as_ref() {
        targets.push(WatchTarget::Config(path.clone()));
        // Registered alongside (not independently of) the config target:
        // `handle_config_reload` -- the handler both a config-file and an
        // overlay-file event route into -- requires `config_path` to do
        // anything at all (it re-reads config.toml from that path AND the
        // overlay from `overlay_default_path()` in the same call). Without
        // a registered config path there is no reload to trigger, so
        // watching the overlay would only add ambient-path fs-watch
        // overhead with no effect.
        targets.push(WatchTarget::CatalogOverlay(
            routectl_router::overlay_default_path(),
        ));
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
        initial_overlay,
        ReloadContext {
            config_path,
            oauth_store,
            secrets,
            router_swap,
            activation_swap,
            usage,
        },
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
    use tokio::signal::unix::{SignalKind, signal};

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

/// Which watched file's change triggered a call into
/// `handle_config_reload`. Both `ReloadRequest::Config` and
/// `ReloadRequest::CatalogOverlay` route into that SAME function --
/// `load_effective_config` always re-reads config.toml and the overlay
/// together, so there is no narrower per-file reload path to run. This
/// only labels the reload's success log line so an operator can tell
/// which watched file fired a given reload.
#[derive(Debug, Clone, Copy)]
enum ReloadTrigger {
    ConfigFile,
    CatalogOverlay,
}

impl ReloadTrigger {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigFile => "config change",
            Self::CatalogOverlay => "overlay change",
        }
    }
}

/// Long-lived dependencies the reload coordinator carries across every
/// `ReloadRequest`. Bundling them keeps the coordinator and its helpers
/// under the argument-count ceiling; the `current_config` it diffs
/// against evolves per reload and stays a separate loop variable.
struct ReloadContext {
    config_path: Option<PathBuf>,
    oauth_store: Option<Arc<routectl_auth::OAuthStore>>,
    secrets: Arc<dyn SecretStore>,
    router_swap: Arc<ArcSwap<Router>>,
    /// Read by the config- and credentials-reload paths, which recompute the
    /// activation inventory (via `apply_activation`) after a successful
    /// router rebuild / credentials refresh. Held here so the coordinator can
    /// reach the shared swap without re-threading it per request.
    activation_swap: Arc<ArcSwap<ActivationState>>,
    usage: UsageHandle,
}

/// Stable message string every activation audit event carries. Grep this
/// to isolate the auto-activation inventory trail (`grep "activation
/// inventory"`); documented in `docs/LOGGING.md`.
const ACTIVATION_EVENT_MSG: &str = "activation inventory";

/// What caused an activation recompute. Surfaced verbatim as the `trigger`
/// field on every activation audit event; the string vocabulary is a
/// stable contract (see `docs/LOGGING.md`). The `ConfigChange` /
/// `CredentialsChange` variants are consumed by the reload call sites.
#[derive(Debug, Clone, Copy)]
enum ActivationTrigger {
    /// Initial compute at server boot (diffs against an empty inventory).
    Startup,
    /// A config-file / overlay reload rebuilt the router.
    ConfigChange,
    /// A credentials-file change (login / logout / token refresh).
    CredentialsChange,
}

impl ActivationTrigger {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::ConfigChange => "config_change",
            Self::CredentialsChange => "credentials_change",
        }
    }
}

/// Local-only probe of every routectl-owned OAuth provider id. When the
/// OAuth arm is present each id is probed against the in-memory cache
/// (`probe_local`: no network, no refresh); when it is absent (no HOME/XDG)
/// every id yields [`LocalProbe::StoreUnavailable`]. The id set is the
/// stable `known_provider_ids()` candidate universe -- the same every
/// recompute, so diffs never lose a provider.
async fn gather_probes(
    oauth_store: &Option<Arc<routectl_auth::OAuthStore>>,
) -> Vec<(&'static str, LocalProbe)> {
    let ids = routectl_auth::oauth::known_provider_ids();
    match oauth_store {
        Some(store) => {
            let mut probes = Vec::with_capacity(ids.len());
            for id in ids {
                probes.push((*id, store.probe_local(id).await));
            }
            probes
        }
        None => ids
            .iter()
            .map(|id| (*id, LocalProbe::StoreUnavailable))
            .collect(),
    }
}

/// Recompute the activation inventory and swap it into `activation_swap`,
/// emitting one audit event per transition. The single shared recompute
/// entrypoint: called at startup and (from the reload coordinator) after a
/// config or credentials reload. Steps: gather local probes -> compute the
/// fresh inventory -> diff against the currently-installed state -> emit the
/// delta as tracing events -> store the fresh state. Infallible; never
/// blocks the caller's own failure posture (a broken store surfaces as
/// Unresolved entries, not an error).
async fn apply_activation(
    oauth_store: &Option<Arc<routectl_auth::OAuthStore>>,
    config: &Config,
    activation_swap: &Arc<ArcSwap<ActivationState>>,
    trigger: ActivationTrigger,
) {
    let probes = gather_probes(oauth_store).await;
    let next = compute_activation(&probes, config);
    let prev = activation_swap.load_full();
    let delta = diff_activation(&prev, &next);
    emit_activation_delta(&delta, trigger, oauth_store.is_none());
    activation_swap.store(Arc::new(next));
}

/// Map an [`ActivationDelta`] to tracing events -- the ONLY point where
/// activation state becomes log output. One INFO per transition; a single
/// WARN (not per-id) when no OAuth store was available to probe; nothing at
/// all when the delta is empty AND a store was present. Every field is a
/// display-safe discriminant (provider id, config-kind token, reason code,
/// bool) -- never a token, path, or env value (redaction contract of
/// `routectl_router::activation`).
fn emit_activation_delta(
    delta: &ActivationDelta,
    trigger: ActivationTrigger,
    store_unavailable: bool,
) {
    if store_unavailable {
        tracing::warn!(
            trigger = trigger.as_str(),
            "{ACTIVATION_EVENT_MSG}: no OAuth credential store available to probe",
        );
    }
    for change in &delta.newly_activated {
        tracing::info!(
            provider = %change.provider_id,
            kind = change.provider_kind,
            trigger = trigger.as_str(),
            transition = "activated",
            referenced_by_aliases = change.referenced_by_aliases,
            "{ACTIVATION_EVENT_MSG}",
        );
    }
    for change in &delta.newly_deactivated {
        tracing::info!(
            provider = %change.provider_id,
            kind = change.provider_kind,
            trigger = trigger.as_str(),
            transition = "deactivated",
            reason = change.reason.as_str(),
            referenced_by_aliases = change.referenced_by_aliases,
            "{ACTIVATION_EVENT_MSG}",
        );
    }
}

/// Drain `ReloadRequest`s and apply them. Each request is processed
/// to completion before the next is read so a Router swap and a
/// credentials reload do not interleave.
///
/// `current_overlay` is a loop variable alongside `current_config`: a
/// config-path OR catalog-overlay reload re-reads BOTH from disk (the
/// shared loader) via `handle_config_reload`, updating both; a
/// credentials-only reload rebuilds off the CURRENT (unchanged) overlay,
/// so a seat-set-change rebuild must not silently drop the operator's
/// overlay back to empty.
async fn run_reload_coordinator(
    mut current_config: Arc<Config>,
    mut current_overlay: Arc<CatalogOverlay>,
    ctx: ReloadContext,
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
                            &ctx.oauth_store,
                            &current_config,
                            &current_overlay,
                            ctx.secrets.clone(),
                            &ctx.router_swap,
                            &ctx.activation_swap,
                        )
                        .await;
                    }
                    ReloadRequest::Config => {
                        if let Some((new_config, new_overlay)) = handle_config_reload(
                            ctx.config_path.as_deref(),
                            &current_config,
                            ctx.secrets.clone(),
                            &ctx.router_swap,
                            &ctx.usage,
                            ReloadTrigger::ConfigFile,
                        ).await {
                            current_config = new_config;
                            current_overlay = new_overlay;
                            apply_activation(
                                &ctx.oauth_store,
                                &current_config,
                                &ctx.activation_swap,
                                ActivationTrigger::ConfigChange,
                            )
                            .await;
                        }
                    }
                    ReloadRequest::CatalogOverlay => {
                        // Same loader as `ReloadRequest::Config`:
                        // `handle_config_reload` re-reads config.toml AND
                        // the overlay together on every call regardless
                        // of which watched file changed. Only the
                        // tracing label differs.
                        if let Some((new_config, new_overlay)) = handle_config_reload(
                            ctx.config_path.as_deref(),
                            &current_config,
                            ctx.secrets.clone(),
                            &ctx.router_swap,
                            &ctx.usage,
                            ReloadTrigger::CatalogOverlay,
                        ).await {
                            current_config = new_config;
                            current_overlay = new_overlay;
                            apply_activation(
                                &ctx.oauth_store,
                                &current_config,
                                &ctx.activation_swap,
                                ActivationTrigger::ConfigChange,
                            )
                            .await;
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
    current_overlay: &Arc<CatalogOverlay>,
    secrets: Arc<dyn SecretStore>,
    router_swap: &Arc<ArcSwap<Router>>,
    activation_swap: &Arc<ArcSwap<ActivationState>>,
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

    // Recompute activation on EVERY successful credentials reload, before the
    // seat-gated router-rebuild early return below: activation tracks token
    // PRESENCE (expired -> present is a real transition), not the seat-key set,
    // so a token-value-only refresh that skips the rebuild must still recompute.
    // Runs after `reload_from_disk` so the probe observes the post-reload cache.
    apply_activation(
        oauth_store,
        current_config,
        activation_swap,
        ActivationTrigger::CredentialsChange,
    )
    .await;

    // Gate the Router rebuild on a real seat-set change. A token-value-only
    // refresh (same keys) must not rebuild, or every routine auto-refresh
    // would needlessly re-run the startup validators and re-expand the pool.
    let after = store.credential_keys().await;
    if before == after {
        return;
    }

    rebuild_router_for_seat_change(
        current_config,
        current_overlay,
        secrets,
        router_swap,
        &before,
        &after,
    )
    .await;
}

/// Rebuild the live Router from the unchanged config + overlay after a
/// seat-set change, preserving per-seat runtime state and honoring the
/// disk-first-keep-old invariant on a build failure. Neither config nor
/// overlay is re-read (both are unchanged); re-running the startup
/// validators on them is harmless. Split out of `handle_credentials_reload`
/// to keep that function under the size ceiling.
async fn rebuild_router_for_seat_change(
    current_config: &Arc<Config>,
    current_overlay: &Arc<CatalogOverlay>,
    secrets: Arc<dyn SecretStore>,
    router_swap: &Arc<ArcSwap<Router>>,
    before: &std::collections::BTreeSet<String>,
    after: &std::collections::BTreeSet<String>,
) {
    let mut new_router = match build_router_from_config_with_overlay(
        current_config.clone(),
        current_overlay,
        secrets,
    )
    .await
    {
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
    new_router.carry_over_sticky_from(&router_swap.load_full());
    new_router.carry_over_k_store_from(&router_swap.load_full());
    new_router.carry_over_learned_from(&router_swap.load_full());
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
/// Handle a `ReloadRequest::Config` or `ReloadRequest::CatalogOverlay`:
/// re-read config.toml + the catalog overlay via the shared loader,
/// rebuild the Router against BOTH, and swap it in. Both request
/// variants call this SAME function -- `load_effective_config` always
/// re-reads both files together, so there is no narrower "overlay-only"
/// reload to perform; `trigger` only labels which watched file's change
/// fired this call, for the success log line. Returns the new
/// `(config, overlay)` pair on success so the coordinator's loop
/// variables advance together -- a partial update (new config, stale
/// overlay or vice versa) would desync the pair the next reload diffs
/// against.
async fn handle_config_reload(
    config_path: Option<&Path>,
    current_config: &Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    router_swap: &Arc<ArcSwap<Router>>,
    usage: &UsageHandle,
    trigger: ReloadTrigger,
) -> Option<(Arc<Config>, Arc<CatalogOverlay>)> {
    let Some(path) = config_path else {
        tracing::debug!("config reload requested but no config path was registered; ignoring",);
        return None;
    };

    // `read_parse_validate_config` is fully synchronous (TOML parse, the
    // v1 -> v2 migration's fsyncs, and the overlay read all hit disk
    // directly) -- run it off the runtime so a slow disk or a large
    // migration never stalls every other task sharing this worker
    // thread. A panic inside the closure (`JoinError`) is treated the
    // same as any other load failure: reject the reload, keep the prior
    // router live.
    let path_owned = path.to_path_buf();
    let loaded =
        match tokio::task::spawn_blocking(move || read_parse_validate_config(&path_owned)).await {
            Ok(Some(loaded)) => loaded,
            Ok(None) => return None,
            Err(join_err) => {
                tracing::warn!(
                    error = %join_err,
                    "config reload failed: loader task panicked; keeping previous config",
                );
                return None;
            }
        };
    let new_config = Arc::new(loaded.config);
    let new_overlay = Arc::new(loaded.catalog_overlay);

    let mut new_router = match build_router_from_config_with_overlay(
        new_config.clone(),
        &new_overlay,
        secrets,
    )
    .await
    {
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
    new_router.carry_over_sticky_from(&router_swap.load_full());
    new_router.carry_over_k_store_from(&router_swap.load_full());
    new_router.carry_over_learned_from(&router_swap.load_full());

    router_swap.store(Arc::new(new_router));

    // Flip the usage capture gate live. `db_path` and `retention_days` are
    // restart-required (the writer holds the DB handle opened at boot, and
    // pruning runs only at startup, so a changed value takes effect at the
    // next daemon start -- both surface in the restart-required warning
    // below). Only `enabled` flips at runtime.
    usage.set_enabled(new_config.usage.enabled);

    tracing::info!(
        path = %path.display(),
        trigger = trigger.as_str(),
        "config reloaded; router rebuilt and swapped",
    );

    let restart_required =
        crate::config_classify::collect_restart_required_changes(current_config, &new_config);
    if !restart_required.is_empty() {
        tracing::warn!(
            restart_required = ?restart_required,
            "config reload swapped routing state, but the listed fields require a daemon restart to take effect",
        );
    }

    Some((new_config, new_overlay))
}

/// SINGLE shared config loader: preflight the schema `version`, parse
/// `config.toml`, load the catalog overlay (fail-closed per
/// `routectl_router::load_catalog_overlay`'s matrix), and run the startup
/// validators. Used by BOTH the CLI's cold-start `load_config` (`main.rs`)
/// and this module's hot-reload path (`read_parse_validate_config`) --
/// PRE-EXISTING split-brain this closed: only the cold-start path used to
/// merge the sidecar, so a config reload silently dropped sidecar /
/// `[cache_pricing]` data. A reload now re-reads the overlay from disk too
/// -- both a config-file touch and a dedicated overlay-file write
/// (`WatchTarget::CatalogOverlay` in `file_watch.rs`) trigger this same
/// re-read via `ReloadRequest::Config` / `ReloadRequest::CatalogOverlay`.
/// The load NEVER migrates the file in place: a `version` outside the
/// range this build writes fails closed in the preflight (a too-old file
/// points at `config migrate`; a too-new file at upgrading routectl). A
/// cold-start error propagates; a reload rejects and keeps the prior
/// router live, same posture as every other load failure below.
///
/// The loaded overlay rides back on [`LoadedConfig::catalog_overlay`] --
/// callers that build a Router thread it into
/// [`build_router_from_config_with_overlay`] so the two-layer merge
/// (`routectl_router::apply_catalog_overlay`) sees the SAME overlay this
/// call validated, at both cold start and every config reload.
///
/// Overlay / parse / validate failure ALWAYS returns `Err` here; callers
/// choose the posture -- cold startup propagates the error (fails hard),
/// a hot reload logs a warn and keeps the prior config + router live
/// (`read_parse_validate_config` below does exactly that).
pub fn load_effective_config(path: &Path) -> Result<LoadedConfig, String> {
    let loaded = load_effective_config_unvalidated(path)?;
    validate_effective_config(&loaded.config)?;
    warn_deprecated_capability_lists(&loaded.config);
    Ok(loaded)
}

/// Emit ONE structured deprecation WARN when a serve-loaded config carries
/// any legacy capability-list key, naming which keys are present, the
/// `[capability.overrides]` successor, and the `config migrate` command that
/// rewrites them. Runs on the serve cold-start AND hot-reload load paths
/// (both flow through [`load_effective_config`]); `config check` loads via
/// [`load_effective_config_unvalidated`], which never calls this, so the
/// check surface stays silent per the settled constraint. The WARN carries
/// key NAMES only -- no config values (secrets can live near these tables).
fn warn_deprecated_capability_lists(config: &Config) {
    let present = crate::commands::capability_legacy::present_legacy_capability_keys(config);
    if present.is_empty() {
        return;
    }

    tracing::warn!(
        event = "legacy_deprecation",
        legacy_keys = ?present,
        successor = "[capability.overrides]",
        migrate_command = "config migrate",
        "deprecated capability-list keys are set; they are tolerated for one release cycle and \
         rejected at the next config schema version. Move them under [capability.overrides] with \
         `config migrate`.",
    );
}

/// The parse + overlay body of [`load_effective_config`], WITHOUT the
/// fail-fast [`validate_effective_config`] gate.
///
/// Only `config check` uses this: it is the showcase surface that runs the
/// FULL shared validator suite itself and renders EVERY error with a source
/// line, so it must receive a parseable-but-semantically-invalid config
/// intact rather than have the load abort on the first semantic error.
///
/// Parse-level failures (unreadable file, version out of range, unknown
/// fields, the legacy `[mitm] credential_source` key) still return `Err`
/// here -- a config that does not parse has nothing for `check` to render
/// against, and this keeps the did-you-mean / migration guidance those
/// preflights emit. In particular a too-old `version` is REJECTED with the
/// `config migrate` pointer on this path identically to the serve/reload
/// path; neither path mutates the file on load.
///
/// Every other caller (serve cold start, hot reload, test, prompt-size) goes
/// through [`load_effective_config`], which wraps this and keeps the
/// fail-fast validation posture unchanged.
pub fn load_effective_config_unvalidated(path: &Path) -> Result<LoadedConfig, String> {
    let config = parse_config_only(path)?;
    let catalog_overlay = load_overlay_default()?;
    Ok(LoadedConfig {
        config,
        catalog_overlay,
    })
}

/// Parse `config.toml` ONLY -- version preflight, the legacy-mitm preflight,
/// and the typed `deny_unknown_fields` deserialize -- WITHOUT loading the
/// catalog overlay. Doctor's capability panel uses this so an unreadable
/// overlay degrades the catalog priors alone while the config-derived
/// override rows still render. Produces the SAME wrapped error strings as
/// the coupled [`load_effective_config_unvalidated`] so callers redact them
/// through one shared path.
pub(crate) fn parse_config_only(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config `{}`: {e}", path.display()))?;

    // PREFLIGHT: read `version` off the raw TOML before the full,
    // `deny_unknown_fields` typed deserialize below. This rejects a config
    // whose schema version is out of the range this build writes -- a
    // too-new file (whose unknown fields would otherwise fail the typed
    // parse with a confusing "unknown field" error) and, equally, a too-old
    // file (pointed at `config migrate`, never mutated on load). A hot
    // reload hitting either rejects and keeps the prior router live (the
    // caller of this function on that path, `read_parse_validate_config`,
    // already treats any `Err` from here that way).
    routectl_router::preflight_config_version(&text).map_err(|e| e.to_string())?;

    // Same pre-parse pattern for the removed `[mitm] credential_source`
    // key: the raw serde "unknown field" error would not tell the operator
    // how to migrate, so detect the legacy key first and return the error
    // that names the exact provider-block replacement.
    routectl_router::preflight_legacy_mitm_credential_source(&text).map_err(|e| e.to_string())?;

    routectl_router::parse_config(&text)
        .map_err(|e| format!("config parse error in `{}`: {e}", path.display()))
}

/// Load the default catalog overlay independently of any config parse
/// (fail-closed per `routectl_router::load_catalog_overlay`'s matrix).
/// Doctor loads this layer separately so an unreadable overlay degrades the
/// capability priors without tainting the config-derived override rows.
pub(crate) fn load_overlay_default() -> Result<CatalogOverlay, String> {
    let overlay_path = routectl_router::overlay_default_path();
    routectl_router::load_catalog_overlay(&overlay_path)
        .map_err(|e| format!("catalog overlay load error: {e}"))
}

/// Return of [`load_effective_config`]: the parsed `Config` alongside the
/// catalog overlay loaded from the SAME call, so a caller building a Router
/// (`build_router_from_config_with_overlay`) never has the two drift apart
/// by loading them at different times.
pub struct LoadedConfig {
    pub config: Config,
    pub catalog_overlay: CatalogOverlay,
}

/// The startup validators shared by [`load_effective_config`] and
/// `build_router_from_config`'s own validation pass (intentionally
/// redundant: this is the cheap fail-fast gate before the heavier router
/// rebuild).
fn validate_effective_config(config: &Config) -> Result<(), String> {
    // The suite returns bare messages; re-add the `config: ` prefix so the
    // surfaced error reads the same as when these validators propagated
    // through `Error::Config`'s Display on this path.
    match routectl_router::collect_config_validation(config)
        .errors
        .into_iter()
        .next()
    {
        Some(first) => Err(format!("config: {first}")),
        None => Ok(()),
    }
}

/// Read, parse, and validate the config at `path` via the shared
/// [`load_effective_config`]. Returns `None` and emits a warn on any
/// failure so the coordinator can keep the previous config installed.
/// Pulled out of `handle_config_reload` to keep that function focused on
/// the swap + diff phases.
fn read_parse_validate_config(path: &Path) -> Option<LoadedConfig> {
    let loaded = match load_effective_config(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config reload failed; keeping previous config",
            );
            return None;
        }
    };

    // Unix-only: WARN when the config file is group/world-readable
    // and carries sensitive values. Non-fatal so dev setups with
    // literal: secrets still start; the operator is informed and can
    // restrict permissions when it matters. A second small read is
    // acceptable on this rarely-hit reload path -- `load_effective_config`
    // does not hand back the raw text, and this check is orthogonal to
    // parsing.
    #[cfg(unix)]
    if let Ok(text) = std::fs::read_to_string(path) {
        warn_if_config_world_readable(path, &loaded.config, &text);
    }

    Some(loaded)
}

/// Emit a one-time WARN when `path` is group/world-readable AND the
/// config text carries listener auth tokens or `literal:` secrets.
/// Non-fatal: the caller keeps the config regardless so dev setups
/// that store credentials in plain TOML still start. Operators running
/// in shared environments should restrict the file to `0600`.
#[cfg(unix)]
fn warn_if_config_world_readable(path: &Path, config: &Config, raw_text: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_policy_banner_reflects_both_switches() {
        // Arrange / Act / Assert: each switch maps to enabled / disabled.
        assert_eq!(
            cache_policy_banner(true, true),
            "cache policy: auto-emit top-level breakpoint enabled, context reduction enabled"
        );
        assert_eq!(
            cache_policy_banner(false, false),
            "cache policy: auto-emit top-level breakpoint disabled, context reduction disabled"
        );
        assert_eq!(
            cache_policy_banner(true, false),
            "cache policy: auto-emit top-level breakpoint enabled, context reduction disabled"
        );
    }

    /// Point a config's usage DB at a per-test tempdir so server tests
    /// never touch the real `~/.config/routectl/usage.db` (the
    /// `UsageConfig` default). Returns the `TempDir` guard the caller
    /// MUST keep alive for the test's duration. Isolating the path --
    /// rather than disabling usage -- keeps the writer wiring exercised.
    #[cfg(test)]
    fn isolate_usage_db(config: &mut Config) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("usage tempdir");
        config.usage.db_path = dir.path().join("usage.db");
        dir
    }

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

    /// `validate_provider_credential_sources` is wired into
    /// `build_router_from_config_with_overlay` itself (not only reachable
    /// via the separate `commands::config::check` call to the same
    /// validator) -- a forwarded provider pointed at a non-Anthropic host
    /// must fail the router build, i.e. serve startup and hot reload,
    /// which both call this builder directly.
    #[tokio::test]
    async fn build_router_from_config_rejects_forwarded_provider_on_non_anthropic_host() {
        use routectl_router::ProviderEntry;
        use routectl_router::config::CredentialSource;

        let mut config = Config::default();
        let _usage_dir = isolate_usage_db(&mut config);
        config.providers.insert(
            "sneaky".into(),
            ProviderEntry::anthropic_api("")
                .with_base_url("https://evil.example.com")
                .with_credential_source(CredentialSource::Forwarded),
        );
        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());

        // `Router` is not `Debug`, so match rather than `expect_err`.
        let err = match build_router_from_config(Arc::new(config), secrets).await {
            Ok(_) => panic!("forwarded provider off the pinned host must fail the router build"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Config(_)), "got: {err:?}");
    }

    /// Build a `Config` whose single listener-auth token is the given
    /// secret URI. Isolates the usage DB so the test never touches the
    /// real path; returns the config plus the tempdir guard.
    #[cfg(test)]
    fn config_with_single_listener_token(uri: &str) -> (Config, tempfile::TempDir) {
        use routectl_router::{Config, ServerAuth, ServerConfig};
        let mut config = Config {
            server: ServerConfig {
                auth: Some(ServerAuth {
                    tokens: vec![uri.to_string()],
                }),
                ..ServerConfig::default()
            },
            ..Config::default()
        };
        let dir = isolate_usage_db(&mut config);
        (config, dir)
    }

    /// Write `contents` to a 0600 tempfile and return a `file://` URI
    /// pointing at it plus the `NamedTempFile` guard (kept alive by the
    /// caller). `file://` is a use-time source the secret-ref parser
    /// cannot pre-reject for emptiness, and it avoids mutating
    /// process-global env state (which races with concurrent tests).
    #[cfg(test)]
    fn file_token_uri(contents: &str) -> (String, tempfile::NamedTempFile) {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(contents.as_bytes()).expect("write secret file");
        f.flush().expect("flush");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .expect("chmod 600");
        }
        let uri = format!("file://{}", f.path().display());
        (uri, f)
    }

    #[tokio::test]
    async fn resolve_listener_tokens_rejects_empty_file_source() {
        // Arrange: a file:// source the parser CANNOT pre-reject (the URI
        // is well-formed) whose contents trim to an empty string.
        let (uri, _file) = file_token_uri("");
        let (config, _guard) = config_with_single_listener_token(&uri);

        // Act
        let err = resolve_listener_tokens(&config)
            .await
            .expect_err("empty file token must be rejected");

        // Assert: Config error naming entry #1 + the empty-token risk;
        // never the raw value.
        match err {
            Error::Config(msg) => {
                assert!(msg.contains("entry #1"), "must name the entry: {msg}");
                assert!(
                    msg.contains("empty token") && msg.contains("disable authentication"),
                    "must name the empty-token risk: {msg}"
                );
            }
            other => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_listener_tokens_rejects_whitespace_only_file_source() {
        // Arrange: an all-whitespace file value must ALSO be rejected --
        // the guard trims before the emptiness check. (The file store
        // trims trailing whitespace, so seed leading spaces too.)
        let (uri, _file) = file_token_uri("   \n");
        let (config, _guard) = config_with_single_listener_token(&uri);

        // Act
        let err = resolve_listener_tokens(&config)
            .await
            .expect_err("whitespace-only file token must be rejected");

        // Assert
        match err {
            Error::Config(msg) => {
                assert!(msg.contains("entry #1"), "must name the entry: {msg}");
                assert!(
                    msg.contains("disable authentication"),
                    "must name the empty-token risk: {msg}"
                );
            }
            other => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_listener_tokens_accepts_non_empty_token() {
        // Arrange: a valid non-empty token must resolve into a TokenSet
        // without error (positive path).
        let (uri, _file) = file_token_uri("tok-not-empty");
        let (config, _guard) = config_with_single_listener_token(&uri);

        // Act
        let result = resolve_listener_tokens(&config).await;

        // Assert
        assert!(
            result.is_ok(),
            "a non-empty token must build the TokenSet: {result:?}"
        );
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
        use nix::sys::signal::{Signal, kill};
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
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        use std::time::Duration;

        // Arrange: bind an ephemeral loopback port and start the server.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral loopback port");
        let mut config = Config::default();
        let _usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
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
            () = tokio::time::sleep(Duration::from_hours(1)) => false,
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
        let overlay = Arc::new(CatalogOverlay::default());
        handle_credentials_reload(
            &Some(oauth),
            &config,
            &overlay,
            secrets,
            &swap,
            &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
        )
        .await;

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
        let overlay = Arc::new(CatalogOverlay::default());
        handle_credentials_reload(
            &Some(oauth),
            &config,
            &overlay,
            secrets,
            &swap,
            &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
        )
        .await;

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
        let overlay = Arc::new(CatalogOverlay::default());
        handle_credentials_reload(
            &Some(oauth),
            &config,
            &overlay,
            secrets,
            &swap,
            &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
        )
        .await;

        // Assert: a failed reload leaves the previous Router installed.
        let after = swap.load_full();
        assert!(
            Arc::ptr_eq(&before, &after),
            "a failed credentials reload must keep the previous router"
        );
    }

    // ---- Usage writer: boot, hot-reload enabled-gate, shutdown drain ----

    /// Minimal on-disk config text with the usage block's `enabled` set
    /// to `enabled` and `db_path` pointed at `db_path`, so a config
    /// reload picks up an isolated DB and a flippable gate.
    #[cfg(test)]
    fn usage_config_text(enabled: bool, db_path: &std::path::Path) -> String {
        format!(
            "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n\n\
             [usage]\nenabled = {enabled}\ndb_path = \"{}\"\nretention_days = 0\n",
            db_path.display()
        )
    }

    /// A config reload that flips `usage.enabled` true -> false must flip
    /// the live gate WITHOUT rebuilding the writer: the same `UsageHandle`
    /// the daemon holds reports `is_enabled() == false` after the reload,
    /// and the Router Arc is swapped (proving the reload ran).
    ///
    /// `#[serial]`: `handle_config_reload` reads the AMBIENT
    /// `catalog_overlay.json` via `routectl_router::overlay_default_path()`
    /// (XDG_CONFIG_HOME / HOME), same as every other loader call. Without
    /// this, a concurrently-running `#[serial]` test that points
    /// `XDG_CONFIG_HOME` at a tempdir holding a DELIBERATELY corrupt
    /// overlay (see `config_reload_picks_up_overlay_file_change_and_fails_closed_on_corruption`)
    /// can race this test's reload into reading that corrupt file and
    /// failing closed here too -- `serial_test` only excludes OTHER
    /// `#[serial]` tests, so this test must join the same group to be
    /// protected.
    #[tokio::test]
    #[serial_test::serial]
    async fn config_reload_flips_usage_enabled_gate_live() {
        // Arrange: a temp DB + a config file starting enabled=true, and a
        // writer/handle pair the daemon would own across the reload.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("usage.db");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, usage_config_text(false, &db_path)).unwrap();

        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let mut start_config = Config::default();
        start_config.usage.db_path = db_path.clone();
        start_config.usage.enabled = true;
        let start_config = Arc::new(start_config);
        let (usage, _writer) = build_usage_writer(&start_config);
        assert!(usage.is_enabled(), "writer must start enabled");

        let router = build_router_from_config(start_config.clone(), secrets.clone())
            .await
            .expect("initial router build");
        let swap = Arc::new(ArcSwap::from_pointee(router));
        let before_router = swap.load_full();

        // Act: reload the on-disk config (enabled=false).
        let (new_config, _new_overlay) = handle_config_reload(
            Some(&cfg_path),
            &start_config,
            secrets,
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
        )
        .await
        .expect("config reload must apply");

        // Assert: gate flipped live (same handle), router swapped.
        assert!(!new_config.usage.enabled);
        assert!(
            !usage.is_enabled(),
            "reload must flip the live usage gate to disabled"
        );
        let after_router = swap.load_full();
        assert!(
            !Arc::ptr_eq(&before_router, &after_router),
            "config reload must swap the router"
        );
    }

    /// Shared-loader symmetry: a config reload re-reads
    /// the catalog overlay from disk, not just config.toml -- and a corrupt
    /// overlay fails the reload closed, keeping the previously-installed
    /// router live. Isolated `XDG_CONFIG_HOME` so this test controls
    /// exactly what `routectl_router::overlay_default_path()` resolves to.
    #[tokio::test]
    #[serial_test::serial]
    async fn config_reload_picks_up_overlay_file_change_and_fails_closed_on_corruption() {
        // Arrange: an isolated config dir, a minimal config.toml, and NO
        // overlay file yet (first run -> empty overlay).
        let dir = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
        )
        .unwrap();

        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let mut initial_config = Config::default();
        let _usage_dir = isolate_usage_db(&mut initial_config);
        let initial_config = Arc::new(initial_config);
        let (usage, _writer) = build_usage_writer(&initial_config);
        let router = build_router_from_config_with_overlay(
            initial_config.clone(),
            &CatalogOverlay::default(),
            secrets.clone(),
        )
        .await
        .expect("initial router build");
        let swap = Arc::new(ArcSwap::from_pointee(router));

        // Act 1: write a real overlay cell to disk, then reload.
        let overlay_dir = dir.path().join("routectl");
        std::fs::create_dir_all(&overlay_dir).unwrap();
        std::fs::write(
            overlay_dir.join("catalog_overlay.json"),
            r#"{"schema_version":1,"revision":1,"cells":{"anthropic-api:claude-opus-4-8*":
               {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
        )
        .unwrap();
        let (new_config, new_overlay) = handle_config_reload(
            Some(&cfg_path),
            &initial_config,
            secrets.clone(),
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
        )
        .await
        .expect("reload with a fresh overlay file must apply");

        // Assert: the reload re-read the overlay from disk (not the empty
        // overlay the initial router booted with).
        assert!(
            new_overlay
                .cells
                .contains_key("anthropic-api:claude-opus-4-8*"),
            "config reload must re-read the catalog overlay file from disk",
        );
        let router_after_good_reload = swap.load_full();

        // Act 2: corrupt the overlay file, reload again.
        std::fs::write(overlay_dir.join("catalog_overlay.json"), b"not json {{{").unwrap();
        let result = handle_config_reload(
            Some(&cfg_path),
            &new_config,
            secrets,
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
        )
        .await;

        // Assert: a corrupt overlay fails the reload closed -- no
        // config/overlay update, and the router installed by the LAST GOOD
        // reload stays live.
        assert!(result.is_none(), "a corrupt overlay must fail the reload");
        assert!(
            Arc::ptr_eq(&swap.load_full(), &router_after_good_reload),
            "a failed reload must keep the previously-installed router",
        );
    }

    /// `ReloadRequest::Config` and `ReloadRequest::CatalogOverlay` both
    /// call `handle_config_reload` -- the SAME loader re-reads config +
    /// overlay together regardless of which file changed -- but each
    /// must label its own success log with the trigger that fired it, so
    /// an operator can tell an overlay-driven reload from a config-file
    /// one. Drives `handle_config_reload` directly (no `tokio::spawn`)
    /// under `with_capture` so the thread-local capture subscriber sees
    /// the event.
    #[tokio::test]
    #[serial_test::serial]
    async fn handle_config_reload_labels_its_trigger_in_the_success_log() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
        )
        .unwrap();

        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let mut config = Config::default();
        let _usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let (usage, _writer) = build_usage_writer(&config);
        let router = build_router_from_config_with_overlay(
            config.clone(),
            &CatalogOverlay::default(),
            secrets.clone(),
        )
        .await
        .unwrap();
        let swap = Arc::new(ArcSwap::from_pointee(router));

        // Act: the SAME function, once per trigger. `Box::pin` keeps
        // each call's future off this test's own stack frame --
        // `handle_config_reload`'s future is large enough to trip
        // clippy's large-futures lint otherwise.
        let (config_result, config_events) =
            routectl_testkit::with_capture(Box::pin(handle_config_reload(
                Some(&cfg_path),
                &config,
                secrets.clone(),
                &swap,
                &usage,
                ReloadTrigger::ConfigFile,
            )))
            .await;
        config_result.expect("config-triggered reload must apply");

        let (overlay_result, overlay_events) =
            routectl_testkit::with_capture(Box::pin(handle_config_reload(
                Some(&cfg_path),
                &config,
                secrets,
                &swap,
                &usage,
                ReloadTrigger::CatalogOverlay,
            )))
            .await;
        overlay_result.expect("overlay-triggered reload must apply");

        // Assert: each call's success log names its OWN trigger.
        let success_message = "config reloaded; router rebuilt and swapped";
        let config_trigger = config_events
            .iter()
            .find(|e| e.message == success_message)
            .and_then(|e| e.field("trigger"))
            .expect("config-triggered reload must log a trigger field");
        assert_eq!(config_trigger, "config change");

        let overlay_trigger = overlay_events
            .iter()
            .find(|e| e.message == success_message)
            .and_then(|e| e.field("trigger"))
            .expect("overlay-triggered reload must log a trigger field");
        assert_eq!(overlay_trigger, "overlay change");
    }

    /// Full-stack proof that `spawn_reload_pipeline` -- the actual
    /// production wiring, not a hand-rolled substitute -- watches the
    /// catalog overlay path and routes its writes through the SAME
    /// reload coordinator as config: a real overlay write (through the
    /// real `notify` watcher) swaps the router, and a corrupt overlay
    /// write keeps the PRIOR router live. `#[serial]`: the path this
    /// wiring watches is the ambient `routectl_router::overlay_default_path()`
    /// (XDG_CONFIG_HOME-derived).
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_reload_pipeline_watches_overlay_and_swaps_router_on_write() {
        // Arrange: config.toml and the (not-yet-written) overlay share
        // `routectl_config_dir()`, mirroring the real deployment layout
        // -- the watcher needs the overlay's PARENT directory to exist at
        // watch-install time even though the overlay FILE does not.
        let dir = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let config_dir = dir.path().join("routectl");
        std::fs::create_dir_all(&config_dir).unwrap();
        let cfg_path = config_dir.join("config.toml");
        std::fs::write(
            &cfg_path,
            "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
        )
        .unwrap();

        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let mut initial_config = Config::default();
        let _usage_dir = isolate_usage_db(&mut initial_config);
        let initial_config = Arc::new(initial_config);
        let (usage, _writer) = build_usage_writer(&initial_config);
        let router = build_router_from_config_with_overlay(
            initial_config.clone(),
            &CatalogOverlay::default(),
            secrets.clone(),
        )
        .await
        .unwrap();
        let swap = Arc::new(ArcSwap::from_pointee(router));
        let before_router = swap.load_full();

        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let _handles = spawn_reload_pipeline(
            initial_config,
            Arc::new(CatalogOverlay::default()),
            Some(cfg_path),
            None,
            secrets,
            swap.clone(),
            Arc::new(ArcSwap::from_pointee(ActivationState::default())),
            usage,
            shutdown_rx,
        );

        // Settle the watch install before writing.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Act 1: a real overlay write, via the same tempfile + rename
        // pattern `catalog_overlay::save` uses.
        let overlay_path = routectl_router::overlay_default_path();
        std::fs::create_dir_all(overlay_path.parent().unwrap()).unwrap();
        let tmp = tempfile::Builder::new()
            .prefix(".catalog_overlay.tmp.")
            .suffix(".json")
            .tempfile_in(overlay_path.parent().unwrap())
            .unwrap();
        std::fs::write(
            tmp.path(),
            br#"{"schema_version":1,"revision":1,"cells":{"anthropic-api:claude-opus-4-8*":
               {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
        )
        .unwrap();
        tmp.persist(&overlay_path).unwrap();

        // Assert: the router swaps within the debounce + reload window.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut router_after_write = swap.load_full();
        while tokio::time::Instant::now() < deadline {
            router_after_write = swap.load_full();
            if !Arc::ptr_eq(&before_router, &router_after_write) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !Arc::ptr_eq(&before_router, &router_after_write),
            "an overlay write through the real watcher must trigger a reload and swap the router",
        );

        // Act 2: a corrupt overlay write, same rename pattern.
        let tmp2 = tempfile::Builder::new()
            .prefix(".catalog_overlay.tmp.")
            .suffix(".json")
            .tempfile_in(overlay_path.parent().unwrap())
            .unwrap();
        std::fs::write(tmp2.path(), b"not json {{{").unwrap();
        tmp2.persist(&overlay_path).unwrap();

        // No positive signal to poll for here (the swap must NOT
        // happen); a fixed settle window mirrors the negative-outcome
        // checks in the integration suite (`partial_write_keeps_old_config`).
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        assert!(
            Arc::ptr_eq(&swap.load_full(), &router_after_write),
            "a corrupt overlay write must keep the previously-installed router live",
        );
    }

    /// Boot wiring: the writer/handle are constructed before the ArcSwap
    /// and a Router hot-swap must NOT disturb the usage handle. We hold a
    /// handle, swap the Router under it, and assert the handle's gate and
    /// counters object survive the swap unchanged (handler call sites keep
    /// a stable usage handle across any number of Router rebuilds).
    #[tokio::test]
    async fn router_hot_swap_does_not_disturb_usage_handle() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.usage.db_path = dir.path().join("usage.db");
        let config = Arc::new(config);
        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let (usage, _writer) = build_usage_writer(&config);

        let r1 = build_router_from_config(config.clone(), secrets.clone())
            .await
            .unwrap();
        let swap = Arc::new(ArcSwap::from_pointee(r1));
        let counters_ptr = Arc::as_ptr(usage.counters());
        usage.set_enabled(true);

        // Act: swap a freshly-built Router under the same handle.
        let r2 = build_router_from_config(config.clone(), secrets)
            .await
            .unwrap();
        swap.store(Arc::new(r2));

        // Assert: the handle's gate and shared counters are untouched by
        // the Router swap (same counters allocation, gate value preserved).
        assert!(usage.is_enabled(), "router swap must not flip the gate");
        assert_eq!(
            counters_ptr,
            Arc::as_ptr(usage.counters()),
            "router swap must not rebuild the usage handle's counters"
        );
    }

    /// Graceful shutdown must drain queued usage rows to the (temp) DB
    /// before returning, and complete within the writer's bounded
    /// deadline. Drives `drain_usage_writer` (the spawn_blocking path the
    /// serve loop uses) directly with rows already queued.
    #[tokio::test]
    async fn drain_usage_writer_flushes_queued_rows() {
        // Arrange: a live writer at a temp DB with rows queued behind it.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("usage.db");
        let mut config = Config::default();
        config.usage.db_path = db_path.clone();
        config.usage.enabled = true;
        config.usage.retention_days = 0;
        let config = Arc::new(config);
        let (usage, writer) = build_usage_writer(&config);

        let n = 25usize;
        for i in 0..n {
            usage.try_send(sample_usage_record(&format!("drain-{i}")));
        }
        // Drop the producer handle so the channel closes once the writer
        // drops its sender -- mirrors serve return, where the app owning
        // the handle is dropped before the drain runs.
        drop(usage);

        // Act: the serve loop's drain dispatch must flush + return within
        // the bounded deadline.
        let start = std::time::Instant::now();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drain_usage_writer(writer),
        )
        .await
        .expect("drain must complete within the bounded deadline");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(7),
            "drain exceeded the bounded deadline"
        );

        // Assert: every queued row landed in the temp DB.
        let count: i64 = rusqlite_count(&db_path);
        assert_eq!(count, n as i64, "all queued rows must be flushed on drain");
    }

    /// Shutdown with no rows queued must still complete within the
    /// writer's bounded deadline. With the producer handle dropped (the
    /// serve-return steady state) the empty channel closes and the drain
    /// returns promptly rather than waiting out the full deadline.
    #[tokio::test]
    async fn drain_usage_writer_empty_queue_returns_fast() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.usage.db_path = dir.path().join("usage.db");
        let config = Arc::new(config);
        let (usage, writer) = build_usage_writer(&config);
        // Drop the producer handle so the channel can close once the
        // writer drops its own sender -- the steady state after the axum
        // app (which owned the only handle) is dropped on serve return.
        drop(usage);

        let start = std::time::Instant::now();
        drain_usage_writer(writer).await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "empty-queue drain must return promptly once the handle is dropped"
        );
    }

    /// Models the production shutdown topology the other drain tests
    /// miss: a SECOND `UsageHandle` clone (the reload coordinator's, in
    /// real wiring) is alive in a background task when the drain begins
    /// and is only dropped after a brief delay. The drain must still
    /// complete cleanly within the bounded deadline (no timeout-warn
    /// path) once that clone goes away, and every queued row must land.
    /// The existing `drain_usage_writer_flushes_queued_rows` drops the
    /// sole handle up front, which hides this race.
    #[tokio::test]
    async fn drain_usage_writer_completes_with_concurrent_handle() {
        // Arrange: a live writer at a temp DB with rows queued behind it.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("usage.db");
        let mut config = Config::default();
        config.usage.db_path = db_path.clone();
        config.usage.enabled = true;
        config.usage.retention_days = 0;
        let config = Arc::new(config);
        let (usage, writer) = build_usage_writer(&config);

        let n = 25usize;
        for i in 0..n {
            usage.try_send(sample_usage_record(&format!("concurrent-{i}")));
        }

        // Hold a second handle clone alive in a background task that
        // releases it after a brief delay -- the channel stays open until
        // BOTH this clone and the writer's own sender are gone.
        let second = usage.clone();
        let dropper = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            drop(second);
        });
        // Drop the primary producer handle now; the background clone is
        // still keeping the channel open.
        drop(usage);

        // Act: the drain must flush and return within the bounded deadline
        // once the lingering clone is released.
        let start = std::time::Instant::now();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drain_usage_writer(writer),
        )
        .await
        .expect("drain must complete within the bounded deadline");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(7),
            "drain exceeded the bounded deadline despite the clone being released"
        );
        let _ = dropper.await;

        // Assert: every queued row landed despite the concurrent handle.
        let count: i64 = rusqlite_count(&db_path);
        assert_eq!(count, n as i64, "all queued rows must be flushed on drain");
    }

    /// Read `SELECT COUNT(*)` from the usage DB at `path`. Test helper.
    #[cfg(test)]
    fn rusqlite_count(path: &std::path::Path) -> i64 {
        use routectl_usage::open;
        let db = open(path).expect("open usage db for read");
        db.conn()
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .expect("count rows")
    }

    /// Build a minimal valid `UsageRecord` with the given id for drain
    /// tests. Mirrors the writer crate's own fixture shape.
    #[cfg(test)]
    fn sample_usage_record(request_id: &str) -> routectl_usage::UsageRecord {
        use routectl_usage::{Outcome, UsageRecord};
        UsageRecord {
            ts_start: 0,
            ts_end: 0,
            request_id: request_id.to_string(),
            ingress_dialect: "openai".to_string(),
            requested_model: "m".to_string(),
            alias: "a".to_string(),
            model: None,
            upstream: None,
            provider: None,
            provider_kind: None,
            seat: None,
            session_id: None,
            stream: false,
            max_tokens_req: None,
            tool_count: 0,
            thinking_req: None,
            thinking_req_kind: None,
            msg_count: 1,
            service_tier: None,
            outcome: Outcome::Ok,
            http_status: None,
            error_class: None,
            finish_reason: None,
            attempt_count: 1,
            fallback_count: 0,
            latency_ms: 0,
            ttfb_ms: None,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
            server_tool_use: None,
            quota_claim: None,
            quota_status: None,
            quota_overage_status: None,
            quota_utilization: None,
            quota_overage_utilization: None,
            quota_reset: None,
            quota_extras: None,
            extra: None,
            strategy: None,
            reduction_strategy: None,
            selection_decision: None,
            would_trim_tokens: None,
            would_trim_break_even_k: None,
            would_trim_k_floor: None,
            would_trim_shadow_misfire: None,
            would_trim_dedup_tokens: None,
            would_trim_supersession_tokens: None,
            would_trim_path_units: None,
            would_trim_path_extractable: None,
            would_trim_recorder_version: None,
            would_trim_raw_marks: None,
            would_trim_context_fraction: None,
        }
    }

    /// Temporarily set an env var for the test's duration, restoring the
    /// prior value (or unsetting it) on drop. Mirrors `secrets.rs`'s
    /// private `EnvGuard` (not reused across modules -- kept local and
    /// minimal here).
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: serialized against every other `#[serial]` env-mutating
            // test in this crate via `serial_test::serial`.
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                // SAFETY: see `set` above.
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    /// A config older than this build writes is REJECTED at load, never
    /// migrated in place. Both the serve/reload loader
    /// (`load_effective_config`) and the `config check` unvalidated path
    /// (`load_effective_config_unvalidated`) must reject it identically,
    /// point at `config migrate`, and leave the file byte-identical -- the
    /// mutate-on-load incident class this replaces.
    #[test]
    #[serial_test::serial]
    fn load_rejects_a_too_old_config_and_leaves_it_byte_identical() {
        // Arrange: a v1 config (no explicit `version`) under an isolated
        // temp dir -- never the live config, per the loader learnings.
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        let body = "[server]\nhost = \"127.0.0.1\"\nport = 4000\n";
        std::fs::write(&cfg_path, body).expect("write config.toml");

        // Act: the serve/reload path.
        let serve_err = match load_effective_config(&cfg_path) {
            Ok(_) => panic!("a too-old config must be rejected on the serve path"),
            Err(e) => e,
        };
        // Act: the `config check` unvalidated path.
        let check_err = match load_effective_config_unvalidated(&cfg_path) {
            Ok(_) => panic!("a too-old config must be rejected on the check path"),
            Err(e) => e,
        };

        // Assert: both reject with the single-sourced migrate pointer.
        for err in [&serve_err, &check_err] {
            assert!(err.contains("config migrate"), "err: {err}");
            assert!(
                err.contains(&routectl_router::CURRENT_CONFIG_VERSION.to_string()),
                "err: {err}"
            );
        }

        // Assert: the file was not touched -- no stamp, no rewrite.
        let after = std::fs::read_to_string(&cfg_path).expect("read config after reject");
        assert_eq!(after, body, "a rejected config must stay byte-identical");
    }

    /// A current-version config loads unchanged: it passes the preflight,
    /// the load never merges the legacy sidecar or mutates the file, and the
    /// overlay -- untouched by this load -- stays as it was on disk.
    #[test]
    #[serial_test::serial]
    fn load_leaves_a_current_version_config_unchanged() {
        // Arrange: a v2 config, plus a sidecar file that the load must NOT
        // fold in (the load no longer merges sidecars at any version).
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        let body = "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 4000\n";
        std::fs::write(&cfg_path, body).expect("write config.toml");

        let sidecar_dir = dir.path().join("routectl");
        std::fs::create_dir_all(&sidecar_dir).expect("create sidecar dir");
        std::fs::write(
            sidecar_dir.join("pricing_verifications.json"),
            r#"{"verified":{"openai-compat:grok-*":"2026-06-30"}}"#,
        )
        .expect("write sidecar");

        // Act
        let loaded = load_effective_config(&cfg_path).expect("load must succeed");

        // Assert: version preserved, nothing folded, file byte-identical.
        assert_eq!(
            loaded.config.version,
            routectl_router::CURRENT_CONFIG_VERSION
        );
        assert!(loaded.config.cache_pricing.is_empty());
        assert!(loaded.catalog_overlay.cells.is_empty());
        let after = std::fs::read_to_string(&cfg_path).expect("read config after load");
        assert_eq!(
            after, body,
            "a current-version config must not be rewritten"
        );
    }

    /// Cold-start posture: a config whose `version` is newer than this
    /// build supports fails the load closed with a clear message, via the
    /// preflight raw-TOML read -- before the `deny_unknown_fields` typed
    /// deserialize would otherwise mask it behind an unknown-field error.
    #[test]
    fn load_effective_config_rejects_a_version_newer_than_supported() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, "version = 99\n[server]\nhost = \"127.0.0.1\"\n")
            .expect("write config.toml");

        // Act. `LoadedConfig` is not `Debug`, so match rather than
        // `expect_err`.
        let err = match load_effective_config(&cfg_path) {
            Ok(_) => panic!("version 99 must be rejected"),
            Err(e) => e,
        };

        // Assert
        assert!(err.contains("99"), "err: {err}");
        assert!(
            err.contains(&routectl_router::CURRENT_CONFIG_VERSION.to_string()),
            "err: {err}"
        );
    }

    /// `validate_provider_credential_sources` is wired into
    /// `validate_effective_config`, which `load_effective_config` calls at
    /// the end of its own pre-parse gate -- this proves the rejection
    /// fires on the actual serve/reload load path (the function
    /// `read_parse_validate_config` calls), not only via the separate
    /// `commands::config::check` entry point that also happens to call
    /// the same validator.
    #[test]
    #[serial_test::serial]
    fn load_effective_config_rejects_forwarded_provider_on_non_anthropic_host() {
        // Arrange: a current-version config so the load path exercises
        // only the credential-source validator, not the version preflight.
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "version = 3\n\
             [providers.sneaky]\n\
             kind = \"anthropic-api\"\n\
             base_url = \"https://evil.example.com\"\n\
             credential_source = \"forwarded\"\n",
        )
        .expect("write config.toml");

        // Act. `LoadedConfig` is not `Debug`, so match rather than
        // `expect_err`.
        let err = match load_effective_config(&cfg_path) {
            Ok(_) => panic!("forwarded provider off the pinned host must be rejected"),
            Err(e) => e,
        };

        // Assert
        assert!(err.contains("sneaky"), "err: {err}");
        assert!(err.contains("forwarded"), "err: {err}");
    }

    /// The serve/reload pre-parse gate (`validate_effective_config`, called
    /// by `load_effective_config`) routes through the same centralized
    /// suite as `config check` / `test` / `prompt-size`, so the identical
    /// bad configs are rejected here too. Pins the no-fork acceptance on
    /// the fourth caller path.
    #[test]
    #[serial_test::serial]
    fn load_effective_config_rejects_each_centralized_bad_config() {
        // The loader preflight-rejects a stale version, and each remaining
        // case is a valid v3 config that a centralized validator refuses.
        let cases = [
            (
                "unknown-alias-target",
                "version = 3\n[aliases]\nfast = \"ghost\"\n",
            ),
            (
                "reserved-class-override",
                "version = 3\n[retry.classes.feature-unsupported]\nfallback = false\n",
            ),
        ];

        for (name, body) in cases {
            let dir = tempfile::tempdir().expect("tempdir");
            let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
            let cfg_path = dir.path().join("config.toml");
            std::fs::write(&cfg_path, body).expect("write config.toml");

            assert!(
                load_effective_config(&cfg_path).is_err(),
                "serve pre-parse gate must reject `{name}`"
            );
        }
    }

    /// A serve-loadable config that sets all three legacy capability-list
    /// keys AND passes the shared validator suite: an `openai-compat`
    /// provider carrying `unsupported_features`, plus the two `[bedrock]`
    /// allowlists (non-empty). No Bedrock provider is configured, so the
    /// Bedrock allowlist validator short-circuits and the config loads
    /// cleanly -- letting the same file exercise both the serve WARN and
    /// the silent `config check` path.
    const LEGACY_LIST_CONFIG: &str = "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://api.example.com\"\n\
         api_key_ref = \"env://SOME_KEY\"\n\
         unsupported_features = [\"web_search\"]\n\
         [bedrock]\n\
         allowed_betas = [\"some-beta\"]\n\
         allowed_body_fields = [\"messages\", \"anthropic_version\", \"max_tokens\"]\n";

    fn deprecation_warns(
        events: &[routectl_testkit::CapturedEvent],
    ) -> Vec<&routectl_testkit::CapturedEvent> {
        events
            .iter()
            .filter(|e| e.field("legacy_keys").is_some())
            .collect()
    }

    /// Serve COLD START (`load_effective_config`, the loader both the
    /// cold-start `load_config_with_overlay` and the hot-reload
    /// `read_parse_validate_config` flow through) emits exactly ONE
    /// deprecation WARN on a legacy-list config, naming which keys are
    /// present plus the successor + migrate pointer -- and no config VALUES.
    #[test]
    #[serial_test::serial]
    fn serve_load_warns_once_on_legacy_capability_lists() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, LEGACY_LIST_CONFIG).expect("write config.toml");

        // Act
        let events = routectl_testkit::capture_events(|| {
            load_effective_config(&cfg_path).expect("legacy-list config must still load");
        });

        // Assert: exactly one deprecation WARN.
        let warns = deprecation_warns(&events);
        assert_eq!(
            warns.len(),
            1,
            "serve cold-start load must emit exactly one deprecation WARN; got {}",
            warns.len()
        );
        let warn = warns[0];
        assert_eq!(warn.level, tracing::Level::WARN);
        assert_eq!(warn.field("event"), Some("legacy_deprecation"));

        // Names all three present legacy keys, the successor, the command.
        let keys = warn.field("legacy_keys").expect("legacy_keys field");
        assert!(keys.contains("unsupported_features"), "keys: {keys}");
        assert!(keys.contains("allowed_betas"), "keys: {keys}");
        assert!(keys.contains("allowed_body_fields"), "keys: {keys}");
        assert_eq!(warn.field("successor"), Some("[capability.overrides]"));
        assert_eq!(warn.field("migrate_command"), Some("config migrate"));

        // Log hygiene: the WARN carries key NAMES, never the operator's
        // list VALUES (which can sit next to secrets).
        let blob = format!("{} {:?}", warn.message, warn.fields);
        assert!(!blob.contains("web_search"), "leaked value: {blob}");
        assert!(!blob.contains("some-beta"), "leaked value: {blob}");
    }

    /// HOT RELOAD (`read_parse_validate_config`, the synchronous loader the
    /// reload path runs off-runtime) emits exactly ONE deprecation WARN on a
    /// legacy-list config -- the same site as cold start, fired once.
    #[test]
    #[serial_test::serial]
    fn hot_reload_load_warns_once_on_legacy_capability_lists() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, LEGACY_LIST_CONFIG).expect("write config.toml");

        // Act
        let events = routectl_testkit::capture_events(|| {
            read_parse_validate_config(&cfg_path).expect("legacy-list config must reload");
        });

        // Assert
        assert_eq!(
            deprecation_warns(&events).len(),
            1,
            "hot reload must emit exactly one deprecation WARN"
        );
    }

    /// `config check` loads via `load_effective_config_unvalidated` (never
    /// `load_effective_config`), so the SAME legacy-list config produces NO
    /// deprecation WARN there -- and still PASSES the shared validator suite,
    /// so existing configs keep passing `check`. Asserts both sides.
    #[test]
    #[serial_test::serial]
    fn config_check_stays_silent_and_passing_on_legacy_capability_lists() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, LEGACY_LIST_CONFIG).expect("write config.toml");

        // Act: the loader `config check` uses, under capture.
        let mut loaded = None;
        let events = routectl_testkit::capture_events(|| {
            loaded = Some(
                load_effective_config_unvalidated(&cfg_path)
                    .expect("legacy-list config must parse for check"),
            );
        });

        // Assert (silent): no deprecation WARN on the check load path.
        assert!(
            deprecation_warns(&events).is_empty(),
            "config check load path must emit no deprecation WARN"
        );

        // Assert (passing): the shared validator suite `config check` runs
        // finds no errors, so the legacy-list config still passes.
        let config = loaded.expect("config loaded").config;
        let validation = routectl_router::collect_config_validation(&config);
        assert!(
            validation.errors.is_empty(),
            "legacy-list config must still pass config check; errors: {:?}",
            validation.errors
        );
    }

    /// A config with no legacy list set (empty lists are the pass-through
    /// default) emits no deprecation WARN on the serve load path.
    #[test]
    #[serial_test::serial]
    fn serve_load_is_silent_without_legacy_capability_lists() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
        )
        .expect("write config.toml");

        // Act
        let events = routectl_testkit::capture_events(|| {
            load_effective_config(&cfg_path).expect("clean config must load");
        });

        // Assert
        assert!(
            deprecation_warns(&events).is_empty(),
            "a config with no legacy lists must emit no deprecation WARN"
        );
    }

    /// Hot-reload posture: a config edited to a too-new `version`
    /// rejects the reload and keeps the prior router live, same as any
    /// other reload-time load failure.
    #[tokio::test]
    #[serial_test::serial]
    async fn config_reload_rejects_a_version_newer_than_supported_and_keeps_prior_router() {
        // Arrange: a good current-version config, an initial router built from it.
        let dir = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
        )
        .unwrap();

        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let mut initial_config = Config {
            version: routectl_router::CURRENT_CONFIG_VERSION,
            ..Default::default()
        };
        let _usage_dir = isolate_usage_db(&mut initial_config);
        let initial_config = Arc::new(initial_config);
        let (usage, _writer) = build_usage_writer(&initial_config);
        let router = build_router_from_config_with_overlay(
            initial_config.clone(),
            &CatalogOverlay::default(),
            secrets.clone(),
        )
        .await
        .expect("initial router build");
        let swap = Arc::new(ArcSwap::from_pointee(router));
        let router_before = swap.load_full();

        // Act: edit the config to a version newer than this build
        // supports, then reload.
        std::fs::write(
            &cfg_path,
            "version = 99\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
        )
        .unwrap();
        let result = handle_config_reload(
            Some(&cfg_path),
            &initial_config,
            secrets,
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
        )
        .await;

        // Assert: the reload rejects and the prior router stays installed.
        assert!(result.is_none(), "a too-new version must reject the reload");
        let router_after = swap.load_full();
        assert!(
            Arc::ptr_eq(&router_before, &router_after),
            "the prior router must stay installed on a rejected reload"
        );
    }
}

/// Activation-recompute + audit-event tests. Driven on the `#[tokio::test]`
/// default current-thread runtime so the thread-local capture subscriber
/// (`routectl_testkit::with_capture`) sees every event `apply_activation`
/// emits -- a multi-thread runtime would move the awaited future to a worker
/// the subscriber is not installed on.
#[cfg(test)]
mod activation_tests {
    use super::*;
    use routectl_testkit::with_capture;

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Seed a `credentials.json` at `path` holding one Present record for
    /// `provider` (unexpired access token + a refresh token). The access and
    /// refresh values are distinctive substrings so a leakage assertion can
    /// prove they never reach any log field.
    fn seed_present(path: &std::path::Path, provider: &str, expires_at_unix: u64) {
        let creds = serde_json::json!({
            "schema_version": 1,
            "providers": {
                provider: {
                    "access_token": "seeded-access-secret-token",
                    "refresh_token": "seeded-refresh-secret-token",
                    "token_type": "Bearer",
                    "expires_at_unix": expires_at_unix,
                    "scopes": ["user:inference"],
                    "obtained_at_unix": now_unix()
                }
            }
        });
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir creds parent");
        std::fs::write(path, serde_json::to_vec_pretty(&creds).unwrap()).expect("write creds");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }
    }

    async fn oauth_at(path: &std::path::Path) -> Option<Arc<routectl_auth::OAuthStore>> {
        CompositeStore::open_at(path)
            .await
            .expect("open composite")
            .oauth_store()
    }

    #[tokio::test]
    async fn startup_emits_activated_fields_without_leakage() {
        // Arrange: a Present anthropic credential; empty config.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        seed_present(&path, "anthropic", now_unix() + 3600);
        let oauth = oauth_at(&path).await;
        let cfg = Config::default();
        let swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));

        // Act
        let ((), events) = with_capture(apply_activation(
            &oauth,
            &cfg,
            &swap,
            ActivationTrigger::Startup,
        ))
        .await;

        // Assert: exactly one activation event, for anthropic, with the
        // documented startup fields.
        let inventory: Vec<_> = events
            .iter()
            .filter(|e| e.message == ACTIVATION_EVENT_MSG)
            .collect();
        let anthropic = inventory
            .iter()
            .find(|e| e.field("provider") == Some("anthropic"))
            .expect("anthropic activation event");
        assert_eq!(anthropic.level, tracing::Level::INFO);
        assert_eq!(anthropic.field("transition"), Some("activated"));
        assert_eq!(anthropic.field("kind"), Some("anthropic-api"));
        assert_eq!(anthropic.field("trigger"), Some("startup"));
        assert_eq!(anthropic.field("referenced_by_aliases"), Some("false"));

        // Redaction contract: neither the seeded token nor the credentials
        // path appears in any message or field of any captured event.
        let path_str = path.display().to_string();
        for e in &events {
            assert!(!e.message.contains("seeded-"), "secret in message: {e:?}");
            assert!(!e.message.contains(&path_str), "path in message: {e:?}");
            for (k, v) in &e.fields {
                assert!(!v.contains("seeded-"), "secret in field {k}={v}");
                assert!(!v.contains(&path_str), "path in field {k}={v}");
            }
        }
    }

    #[tokio::test]
    async fn deactivation_emits_reason_code() {
        // Arrange: activate anthropic, then recompute against a store with no
        // record (Missing) so it transitions out of the activated set.
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.json");
        seed_present(&present, "anthropic", now_unix() + 3600);
        let empty = dir.path().join("empty.json");
        let cfg = Config::default();
        let swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));

        apply_activation(
            &oauth_at(&present).await,
            &cfg,
            &swap,
            ActivationTrigger::Startup,
        )
        .await;

        // Act: recompute on a credentials change with the empty store.
        let ((), events) = with_capture(apply_activation(
            &oauth_at(&empty).await,
            &cfg,
            &swap,
            ActivationTrigger::CredentialsChange,
        ))
        .await;

        // Assert: anthropic deactivated, carrying its machine-readable reason.
        let deactivation = events
            .iter()
            .find(|e| e.message == ACTIVATION_EVENT_MSG && e.field("provider") == Some("anthropic"))
            .expect("anthropic deactivation event");
        assert_eq!(deactivation.field("transition"), Some("deactivated"));
        assert_eq!(deactivation.field("reason"), Some("oauth_missing"));
        assert_eq!(deactivation.field("trigger"), Some("credentials_change"));
    }

    #[tokio::test]
    async fn empty_delta_emits_no_events() {
        // Arrange: seed the swap ALREADY at the computed inventory so the
        // recompute produces an empty diff.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        seed_present(&path, "anthropic", now_unix() + 3600);
        let oauth = oauth_at(&path).await;
        let cfg = Config::default();
        let probes = gather_probes(&oauth).await;
        let swap = Arc::new(ArcSwap::from_pointee(compute_activation(&probes, &cfg)));

        // Act
        let ((), events) = with_capture(apply_activation(
            &oauth,
            &cfg,
            &swap,
            ActivationTrigger::ConfigChange,
        ))
        .await;

        // Assert: no activation events at all (store present -> no WARN; diff
        // empty -> no INFO).
        let inventory: Vec<_> = events
            .iter()
            .filter(|e| e.message.contains(ACTIVATION_EVENT_MSG))
            .collect();
        assert!(
            inventory.is_empty(),
            "an empty delta with a present store must emit nothing, got {inventory:?}"
        );
    }

    #[tokio::test]
    async fn store_unavailable_warns_once_without_leaking_events() {
        // Arrange: no OAuth store at all.
        let none: Option<Arc<routectl_auth::OAuthStore>> = None;
        let cfg = Config::default();
        let swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));

        // Act
        let ((), events) = with_capture(apply_activation(
            &none,
            &cfg,
            &swap,
            ActivationTrigger::Startup,
        ))
        .await;

        // Assert: exactly one WARN, no transition INFO events (every id is
        // StoreUnavailable -> unresolved, so the diff against empty is empty).
        let warns: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN && e.message.contains(ACTIVATION_EVENT_MSG))
            .collect();
        assert_eq!(warns.len(), 1, "one store-unavailable WARN per apply");
        assert_eq!(warns[0].field("trigger"), Some("startup"));
        let infos: Vec<_> = events
            .iter()
            .filter(|e| e.message == ACTIVATION_EVENT_MSG)
            .collect();
        assert!(infos.is_empty(), "no transition events, got {infos:?}");
    }

    fn write_creds_json(path: &std::path::Path, value: &serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir creds parent");
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).expect("write creds");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }
    }

    /// Write a credentials.json holding no provider records (post-logout /
    /// first-run shape). Every probe against it reports `Missing`.
    fn seed_missing(path: &std::path::Path) {
        write_creds_json(
            path,
            &serde_json::json!({ "schema_version": 1, "providers": {} }),
        );
    }

    /// Seed one provider record whose access token is already expired and
    /// that carries NO refresh token -> `Expired` (Unresolved), yet the seat
    /// key is present so the credential-key set is unchanged by a later
    /// refresh to a live token.
    fn seed_expired(path: &std::path::Path, provider: &str) {
        write_creds_json(
            path,
            &serde_json::json!({
                "schema_version": 1,
                "providers": { provider: {
                    "access_token": "seeded-access-secret-token",
                    "refresh_token": "",
                    "token_type": "Bearer",
                    "expires_at_unix": now_unix().saturating_sub(3600),
                    "scopes": ["user:inference"],
                    "obtained_at_unix": now_unix().saturating_sub(7200)
                }}
            }),
        );
    }

    /// Open a composite store at `creds_path`, build a Router from `config`,
    /// and return the shared OAuth arm, the composite secrets store, and the
    /// live Router swap -- the dependency set `handle_credentials_reload`
    /// needs. Mirrors `tests::coordinator_rig`.
    async fn creds_rig(
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
        (oauth, secrets, Arc::new(ArcSwap::from_pointee(router)))
    }

    fn activation_events(
        events: &[routectl_testkit::CapturedEvent],
    ) -> Vec<&routectl_testkit::CapturedEvent> {
        events
            .iter()
            .filter(|e| e.message == ACTIVATION_EVENT_MSG)
            .collect()
    }

    #[tokio::test]
    async fn credentials_reload_adding_live_token_emits_activated() {
        // Arrange: no anthropic record on disk; baseline activation computed
        // against that (Unresolved).
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("credentials.json");
        seed_missing(&creds);
        let cfg = Arc::new(Config::default());
        let (oauth, secrets, router_swap) = creds_rig(&creds, &cfg).await;
        let activation_swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));
        apply_activation(
            &Some(oauth.clone()),
            &cfg,
            &activation_swap,
            ActivationTrigger::Startup,
        )
        .await;

        // Act: a login writes a live anthropic token; reload credentials.
        seed_present(&creds, "anthropic", now_unix() + 3600);
        let ((), events) = with_capture(Box::pin(handle_credentials_reload(
            &Some(oauth),
            &cfg,
            &Arc::new(CatalogOverlay::default()),
            secrets,
            &router_swap,
            &activation_swap,
        )))
        .await;

        // Assert: a newly-activated transition attributed to the credentials
        // reload.
        let activated = activation_events(&events)
            .into_iter()
            .find(|e| e.field("provider") == Some("anthropic"))
            .expect("anthropic activation event");
        assert_eq!(activated.field("transition"), Some("activated"));
        assert_eq!(activated.field("trigger"), Some("credentials_change"));
    }

    #[tokio::test]
    async fn credentials_reload_removing_token_emits_deactivated_with_reason() {
        // Arrange: a live anthropic record; baseline activation = Activated.
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("credentials.json");
        seed_present(&creds, "anthropic", now_unix() + 3600);
        let cfg = Arc::new(Config::default());
        let (oauth, secrets, router_swap) = creds_rig(&creds, &cfg).await;
        let activation_swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));
        apply_activation(
            &Some(oauth.clone()),
            &cfg,
            &activation_swap,
            ActivationTrigger::Startup,
        )
        .await;

        // Act: a logout removes the record; reload credentials.
        seed_missing(&creds);
        let ((), events) = with_capture(Box::pin(handle_credentials_reload(
            &Some(oauth),
            &cfg,
            &Arc::new(CatalogOverlay::default()),
            secrets,
            &router_swap,
            &activation_swap,
        )))
        .await;

        // Assert: a newly-deactivated transition carrying its machine-readable
        // reason, attributed to the credentials reload.
        let deactivated = activation_events(&events)
            .into_iter()
            .find(|e| e.field("provider") == Some("anthropic"))
            .expect("anthropic deactivation event");
        assert_eq!(deactivated.field("transition"), Some("deactivated"));
        assert_eq!(deactivated.field("reason"), Some("oauth_missing"));
        assert_eq!(deactivated.field("trigger"), Some("credentials_change"));
    }

    #[tokio::test]
    async fn credentials_reload_unchanged_seat_set_recomputes_without_router_rebuild() {
        // Arrange: an expired-no-refresh anthropic record (Unresolved) whose
        // seat key is nonetheless present.
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("credentials.json");
        seed_expired(&creds, "anthropic");
        let cfg = Arc::new(Config::default());
        let (oauth, secrets, router_swap) = creds_rig(&creds, &cfg).await;
        let activation_swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));
        apply_activation(
            &Some(oauth.clone()),
            &cfg,
            &activation_swap,
            ActivationTrigger::Startup,
        )
        .await;
        let router_before = router_swap.load_full();

        // Act: refresh the SAME seat to a live token -- a token-value-only
        // change, so the credential-key set is identical across the reload.
        seed_present(&creds, "anthropic", now_unix() + 3600);
        let ((), events) = with_capture(Box::pin(handle_credentials_reload(
            &Some(oauth),
            &cfg,
            &Arc::new(CatalogOverlay::default()),
            secrets,
            &router_swap,
            &activation_swap,
        )))
        .await;

        // Assert: activation recomputed (Unresolved -> Activated) even though
        // the seat set did not change ...
        let activated = activation_events(&events)
            .into_iter()
            .find(|e| e.field("provider") == Some("anthropic"))
            .expect("recompute must emit an activation transition");
        assert_eq!(activated.field("transition"), Some("activated"));
        assert_eq!(activated.field("trigger"), Some("credentials_change"));

        // ... and the Router was NOT rebuilt (pointer-identical swap payload).
        assert!(
            Arc::ptr_eq(&router_before, &router_swap.load_full()),
            "an unchanged seat set must not rebuild the router",
        );
    }

    #[tokio::test]
    async fn failed_config_reload_keeps_previous_activation() {
        // Arrange: a live anthropic record and an activation swap seeded to
        // the computed (Activated) state.
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("credentials.json");
        seed_present(&creds, "anthropic", now_unix() + 3600);
        let oauth = oauth_at(&creds).await;
        let cfg = Arc::new(Config::default());
        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let router = build_router_from_config(cfg.clone(), secrets.clone())
            .await
            .expect("initial router build");
        let router_swap = Arc::new(ArcSwap::from_pointee(router));
        let probes = gather_probes(&oauth).await;
        let activation_swap = Arc::new(ArcSwap::from_pointee(compute_activation(&probes, &cfg)));
        let activation_before = activation_swap.load_full();

        // A syntactically invalid config the reload must reject (parse fails
        // before any overlay / XDG read, so no environment isolation needed).
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, b"<<<not valid toml>>>").unwrap();
        let (usage, _writer) =
            UsageWriter::start(dir.path().join("usage.db"), CHANNEL_CAPACITY, 0, false);

        // Drive the coordinator (not the handler) so the config-reload
        // recompute guard is the code under test. One Config request, then the
        // channel closes so the loop returns.
        let (reload_tx, reload_rx) = mpsc::channel::<ReloadRequest>(4);
        reload_tx.send(ReloadRequest::Config).await.unwrap();
        drop(reload_tx);
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let ctx = ReloadContext {
            config_path: Some(cfg_path),
            oauth_store: oauth,
            secrets,
            router_swap,
            activation_swap: activation_swap.clone(),
            usage,
        };

        // Act
        let ((), events) = with_capture(Box::pin(run_reload_coordinator(
            cfg.clone(),
            Arc::new(CatalogOverlay::default()),
            ctx,
            reload_rx,
            shutdown_rx,
        )))
        .await;

        // Assert: a rejected reload recomputes nothing -- no activation events
        // and the swap payload is pointer-unchanged.
        assert!(
            activation_events(&events).is_empty(),
            "a failed config reload must not recompute activation, got {events:?}",
        );
        assert!(
            Arc::ptr_eq(&activation_before, &activation_swap.load_full()),
            "a failed config reload must leave the activation state untouched",
        );
    }
}
