//! Config/credential/seat reload + activation coordinator (owns the spawn_blocking hot-reload boundary).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_auth::{LocalProbe, SecretStore};
use routectl_router::{
    ActivationDelta, ActivationState, CatalogOverlay, Config, Router, compute_activation,
    diff_activation,
};
use routectl_usage::{CapabilityEvent, UsageHandle};
use tokio::sync::{mpsc, watch};

use super::build_router_from_config_with_overlay;
use super::config_load::read_parse_validate_config;
use super::file_watch::{self, ReloadRequest, WatchTarget};

#[cfg(test)]
use super::{CompositeStore, build_router_from_config};
#[cfg(test)]
use routectl_auth::MemoryStore;
#[cfg(test)]
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};

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
pub(super) async fn await_reload_tasks(handles: Vec<tokio::task::JoinHandle<()>>) {
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

/// Spawn the file-watch task, the SIGHUP listener (cfg(unix)), and
/// the reload coordinator. The returned vector keeps the
/// `JoinHandle`s alive until `serve_on_listener` returns; dropping
/// them only detaches because each task observes the shared
/// `shutdown_rx` and exits cleanly.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_reload_pipeline(
    initial_config: Arc<Config>,
    initial_overlay: Arc<CatalogOverlay>,
    config_path: Option<PathBuf>,
    oauth_store: Option<Arc<routectl_auth::OAuthStore>>,
    secrets: Arc<dyn SecretStore>,
    router_swap: Arc<ArcSwap<Router>>,
    activation_swap: Arc<ArcSwap<ActivationState>>,
    usage: UsageHandle,
    shutdown_rx: watch::Receiver<()>,
    daemon_meta: Arc<crate::handlers::status::DaemonMeta>,
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
            daemon_meta,
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
pub(super) async fn run_sighup_listener(
    tx: mpsc::Sender<ReloadRequest>,
    mut shutdown: watch::Receiver<()>,
) {
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
pub(super) enum ReloadTrigger {
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
    /// Stamped after every successful config reload so the status surface's
    /// config-load age tracks the config actually in effect, not boot time.
    /// The credentials path deliberately never stamps it: a token refresh
    /// rebuilds the router from the SAME config.
    daemon_meta: Arc<crate::handlers::status::DaemonMeta>,
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
pub(super) enum ActivationTrigger {
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
pub(super) async fn apply_activation(
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
                            ctx.daemon_meta.stamp_config_loaded();
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
                            ctx.daemon_meta.stamp_config_loaded();
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
pub(super) async fn handle_credentials_reload(
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
/// validators on them is harmless. Passing `current_overlay` through is
/// load-bearing twice over: the builder both merges it into the resolved
/// models and RETAINS it on the replacement Router, so a credentials-only
/// reload can neither un-apply nor un-report the operator's overlay. Split
/// out of `handle_credentials_reload` to keep that function under the size
/// ceiling.
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
    new_router.carry_over_prefix_epochs_from(&router_swap.load_full());
    new_router.carry_over_calibration_from(&router_swap.load_full());
    new_router.carry_over_quota_from(&router_swap.load_full());
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
pub(super) async fn handle_config_reload(
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
    let previous_router = router_swap.load_full();
    new_router.carry_over_runtime_state_from(&previous_router);
    new_router.carry_over_sticky_from(&previous_router);
    new_router.carry_over_k_store_from(&previous_router);
    new_router.carry_over_prefix_epochs_from(&previous_router);
    new_router.carry_over_calibration_from(&previous_router);
    new_router.carry_over_quota_from(&previous_router);
    new_router.carry_over_learned_from(&previous_router);

    // A reload that advanced the catalog version or overlay revision moves the
    // replay boundary. Read both revisions before the swap (this coordinator is
    // the sole writer, so the loaded Arc is the router being replaced) and stamp
    // the new revision below.
    let revision_changed = previous_router.catalog_version() != new_router.catalog_version()
        || previous_router.overlay_revision() != new_router.overlay_revision();
    let new_catalog_version = new_router.catalog_version();
    let new_overlay_revision = new_router.overlay_revision();

    router_swap.store(Arc::new(new_router));

    // Flip the usage capture gate live. `db_path` and `retention_days` are
    // restart-required (the writer holds the DB handle opened at boot, and
    // pruning runs only at startup, so a changed value takes effect at the
    // next daemon start -- both surface in the restart-required warning
    // below). Only `enabled` flips at runtime.
    usage.set_enabled(new_config.usage.enabled);

    // Stamp the replay boundary at the post-reload revision so negatives
    // learned during the post-reload session sort after this tombstone and
    // replay on the next boot. Enqueued after the gate flip so it honors the
    // freshly-applied usage setting; best-effort, never blocks the reload.
    if revision_changed {
        enqueue_reload_tombstone(usage, new_catalog_version, new_overlay_revision);
    }

    // The reduction and K-gated-emission master switches are the operator's
    // live kill switches for the dispatch-path minifier and for
    // break-even-gated cache emission, so a reload that flipped either says so
    // on the success line -- an operator disabling one needs proof the intended
    // transition landed, not just that the router swapped. Each pair is
    // omitted when that value did not change, which keeps every ordinary
    // reload's line unchanged and makes a flip greppable. Every arm must carry
    // the identical message text; the local macro single-sources it and the
    // sibling unit tests pin it.
    macro_rules! reload_success_line {
        ($($transition:tt)*) => {
            tracing::info!(
                path = %path.display(),
                trigger = trigger.as_str(),
                $($transition)*
                "config reloaded; router rebuilt and swapped",
            )
        };
    }
    let reduction_before = current_config.reduction.enabled;
    let reduction_after = new_config.reduction.enabled;
    let k_gated_before = current_config.cache.k_gated_emission;
    let k_gated_after = new_config.cache.k_gated_emission;
    match (
        reduction_before != reduction_after,
        k_gated_before != k_gated_after,
    ) {
        (false, false) => reload_success_line!(),
        (true, false) => reload_success_line!(
            reduction_enabled_before = reduction_before,
            reduction_enabled_after = reduction_after,
        ),
        (false, true) => reload_success_line!(
            k_gated_emission_before = k_gated_before,
            k_gated_emission_after = k_gated_after,
        ),
        (true, true) => reload_success_line!(
            reduction_enabled_before = reduction_before,
            reduction_enabled_after = reduction_after,
            k_gated_emission_before = k_gated_before,
            k_gated_emission_after = k_gated_after,
        ),
    }

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

/// Enqueue exactly one tombstone stamped the post-reload revision when a hot
/// reload advanced the catalog version or overlay revision. Non-blocking and
/// best-effort like every usage write (`try_send_capability_event` never
/// blocks, awaits, or panics; the enabled gate applies and a disabled writer
/// drops it), so it never fails the reload. Shares the boot seam's tombstone
/// clock source so both replay boundaries stamp the same wall-clock basis.
fn enqueue_reload_tombstone(usage: &UsageHandle, catalog_version: u32, overlay_revision: u64) {
    let event = CapabilityEvent::tombstone(
        super::ledger_reader::epoch_ms_now(),
        i64::from(catalog_version),
        i64::try_from(overlay_revision).unwrap_or(i64::MAX),
    );
    usage.try_send_capability_event(event);
    tracing::info!(
        catalog_version,
        overlay_revision,
        "hot reload advanced the capability revision; enqueued a fresh tombstone (replay boundary)"
    );
}

/// Activation-recompute + audit-event tests. Driven on the `#[tokio::test]`
/// default current-thread runtime so the thread-local capture subscriber
/// (`routectl_testkit::with_capture`) sees every event `apply_activation`
/// emits -- a multi-thread runtime would move the awaited future to a worker
/// the subscriber is not installed on.
#[cfg(test)]
#[path = "activation_tests.rs"]
mod activation_tests;

#[cfg(test)]
#[path = "reload_tests.rs"]
mod reload_tests;
