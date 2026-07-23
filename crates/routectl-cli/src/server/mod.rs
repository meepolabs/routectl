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
mod config_load;
pub mod file_watch;
pub mod k_rebuild;
pub mod request_id;
pub mod secrets;
pub mod status_gate;

use auth::TokenSet;
pub use config_load::{LoadedConfig, load_effective_config, load_effective_config_unvalidated};
use config_load::{compute_max_body_bytes, read_parse_validate_config};
pub(crate) use config_load::{load_overlay_default, parse_config_only};
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

pub(crate) fn is_loopback(host: &str) -> bool {
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

/// Whether the read-only `/status*` subtree (and the `GET /` dashboard)
/// must sit behind the listener auth layer for this bind: true whenever
/// tokens are configured OR the bind is non-loopback. One decision point,
/// fail-closed by construction -- a no-tokens + non-loopback bind (which
/// the startup invariant at `serve_on_listener` already refuses) still
/// returns true, so status is never served auth-exempt on a public
/// address even if that invariant ever drifts.
fn status_requires_auth(token_set: &TokenSet, bound: std::net::SocketAddr) -> bool {
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

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

/// Activation-recompute + audit-event tests. Driven on the `#[tokio::test]`
/// default current-thread runtime so the thread-local capture subscriber
/// (`routectl_testkit::with_capture`) sees every event `apply_activation`
/// emits -- a multi-thread runtime would move the awaited future to a worker
/// the subscriber is not installed on.
#[cfg(test)]
#[path = "activation_tests.rs"]
mod activation_tests;
