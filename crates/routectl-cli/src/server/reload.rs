//! Config/credential/seat reload + activation coordinator (owns the spawn_blocking hot-reload boundary).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_auth::{LocalProbe, SecretStore};
use routectl_router::{
    ActivationDelta, ActivationState, Config, Router, compute_activation, diff_activation,
    unresolved_alias_referenced_entries,
};
use routectl_usage::UsageHandle;
use tokio::sync::{mpsc, watch};

use super::cache_ignore_driver::run_cache_ignore_driver;
use super::config_load::{LoadedConfig, read_parse_validate_config};
use super::file_watch::{self, ReloadRequest, WatchTarget};
#[cfg(test)]
use super::metrics_driver::ROUTER_METRICS_SNAPSHOT_INTERVAL;
use super::metrics_driver::run_router_metrics_snapshot_driver;
#[cfg(test)]
use super::probe_driver::PROBE_DRIVER_INTERVAL;
use super::probe_driver::run_probe_driver;
use super::reload_failure::ReloadFailure;
use super::router_publish::publish_router;
use super::seat_driver::{RefreshJob, run_seat_driver};
use super::{build_router_from_config, log_advisory_warnings};

#[cfg(test)]
use super::CompositeStore;
#[cfg(test)]
use routectl_auth::MemoryStore;
#[cfg(test)]
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};

/// Spawn the file-watch task, the SIGHUP listener (cfg(unix)), and
/// the reload coordinator. The returned vector keeps the
/// `JoinHandle`s alive until `serve_on_listener` returns; dropping
/// them only detaches because each task observes the shared
/// `shutdown_rx` and exits cleanly.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_reload_pipeline(
    initial_config: Arc<Config>,
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
    }
    if let Some(store) = oauth_store.as_ref() {
        targets.push(WatchTarget::Credentials(store.path().to_path_buf()));
    }

    let (reload_tx, reload_rx) = mpsc::channel::<ReloadRequest>(16);

    handles.push(tokio::spawn(run_router_metrics_snapshot_driver(
        router_swap.clone(),
        shutdown_rx.clone(),
    )));

    // Bounded probe driver. Spawned unconditionally and idle by
    // construction: it dials nothing until an admitted real request has
    // queued a job, so a daemon that never serves traffic never probes.
    handles.push(tokio::spawn(run_probe_driver(
        router_swap.clone(),
        shutdown_rx.clone(),
    )));

    // Cache-ignore driver: reads the ledger read-only and only logs.
    handles.push(tokio::spawn(run_cache_ignore_driver(
        router_swap.clone(),
        shutdown_rx.clone(),
    )));

    // Background seat driver. Only with a credential store: without one no
    // managed login can be read, so there is nothing to keep refreshed.
    if let Some(store) = oauth_store.as_ref() {
        handles.push(tokio::spawn(run_seat_driver(
            RefreshJob::new(store.clone()),
            router_swap.clone(),
            shutdown_rx.clone(),
            daemon_meta.seat_driver_health(),
        )));
    }

    match file_watch::spawn_watcher(targets, reload_tx.clone(), shutdown_rx.clone()) {
        Ok(handle) => handles.push(handle),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "file-watch init failed; SIGHUP remains as the reload escape hatch",
            );
        }
    }

    // Registered here rather than inside the spawned task so the handler is
    // in place before the daemon starts serving: a SIGHUP that lands before
    // registration takes the default disposition and terminates the process.
    #[cfg(unix)]
    if let Some(sighup) = register_sighup() {
        handles.push(tokio::spawn(fan_out_reload_triggers(
            sighup,
            reload_tx.clone(),
            shutdown_rx.clone(),
        )));
    }
    // Drop the original sender so the coordinator's `recv()` returns
    // None when every clone is closed. (cfg(unix) clones above keep
    // the channel open under the SIGHUP listener; the coordinator
    // exits via `shutdown.changed()` instead.)
    drop(reload_tx);

    let coordinator_handle = tokio::spawn(run_reload_coordinator(
        initial_config,
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

/// Install the `SIGHUP` handler and expose each delivery as a `()` item.
/// Returns `None` (after a warning) when the handler cannot be installed,
/// leaving reload-via-signal disabled.
#[cfg(unix)]
fn register_sighup() -> Option<impl futures::Stream<Item = ()> + Unpin + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::hangup()) {
        Ok(mut sig) => Some(futures::stream::poll_fn(move |cx| sig.poll_recv(cx))),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to install SIGHUP handler; reload-via-signal disabled",
            );
            None
        }
    }
}

/// Fan each `SIGHUP` delivery from `triggers` into the reload channel as a
/// paired (Config + Credentials) full-rescan request. Returns when the
/// trigger stream ends or `shutdown` fires. Sends are best-effort: a closed
/// coordinator (post-shutdown) silently drops.
#[cfg(unix)]
pub(super) async fn fan_out_reload_triggers(
    mut triggers: impl futures::Stream<Item = ()> + Unpin,
    tx: mpsc::Sender<ReloadRequest>,
    mut shutdown: watch::Receiver<()>,
) {
    use futures::StreamExt as _;

    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            received = triggers.next() => {
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

/// The `trigger` field every config reload success line carries. A stable
/// log contract (see `docs/LOGGING.md`); only the config file triggers one.
const CONFIG_RELOAD_TRIGGER: &str = "config change";

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

/// Most provider entry ids the boot-time unresolved summary names; the rest
/// are counted in its `omitted` field so one line stays bounded however
/// many entries a config declares.
const MAX_LOGGED_UNRESOLVED_ENTRIES: usize = 8;

/// What caused an activation recompute. Surfaced verbatim as the `trigger`
/// field on every activation audit event; the string vocabulary is a
/// stable contract (see `docs/LOGGING.md`). The `ConfigChange` /
/// `CredentialsChange` variants are consumed by the reload call sites.
#[derive(Debug, Clone, Copy)]
pub(super) enum ActivationTrigger {
    /// Initial compute at server boot (diffs against an empty inventory).
    Startup,
    /// A config-file reload rebuilt the router.
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
/// (`probe_all_local`: no network, no refresh); when it is absent (no HOME/XDG)
/// every id yields [`LocalProbe::StoreUnavailable`]. The id set is the
/// stable `known_provider_ids()` candidate universe -- the same every
/// recompute, so diffs never lose a provider.
async fn gather_probes(
    oauth_store: &Option<Arc<routectl_auth::OAuthStore>>,
) -> Vec<(&'static str, LocalProbe)> {
    let ids = routectl_auth::oauth::known_provider_ids();
    match oauth_store {
        Some(store) => store.probe_all_local().await,
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
    if matches!(trigger, ActivationTrigger::Startup) {
        warn_unresolved_at_startup(&unresolved_alias_referenced_entries(&next, config));
    }
    activation_swap.store(Arc::new(next));
}

/// One WARN naming the alias-referenced provider entries whose credential is
/// unresolved at boot. The transition diff never reports these (they were
/// never activated), so without this line a fallback onto such an entry
/// stays silent. Names at most [`MAX_LOGGED_UNRESOLVED_ENTRIES`] sanitized
/// entry ids; nothing when `entries` is empty.
fn warn_unresolved_at_startup(entries: &[&str]) {
    if entries.is_empty() {
        return;
    }
    let named: Vec<String> = entries
        .iter()
        .take(MAX_LOGGED_UNRESOLVED_ENTRIES)
        .map(|id| routectl_core::sanitize_for_log(id))
        .collect();
    tracing::warn!(
        trigger = ActivationTrigger::Startup.as_str(),
        count = entries.len(),
        entries = ?named,
        omitted = entries.len() - named.len(),
        "{ACTIVATION_EVENT_MSG}: alias-referenced provider entries have no usable credential",
    );
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
            provider = %routectl_core::sanitize_for_log(&change.provider_id),
            kind = change.provider_kind,
            trigger = trigger.as_str(),
            transition = "activated",
            referenced_by_aliases = change.referenced_by_aliases,
            "{ACTIVATION_EVENT_MSG}",
        );
    }
    for change in &delta.newly_deactivated {
        tracing::info!(
            provider = %routectl_core::sanitize_for_log(&change.provider_id),
            kind = change.provider_kind,
            trigger = trigger.as_str(),
            transition = "deactivated",
            reason = change.reason.as_str(),
            referenced_by_aliases = change.referenced_by_aliases,
            "{ACTIVATION_EVENT_MSG}",
        );
    }
}

/// Drain `ReloadRequest`s, each to completion before the next is read so
/// a Router swap and a credentials reload do not interleave.
async fn run_reload_coordinator(
    mut current_config: Arc<Config>,
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
                            ctx.secrets.clone(),
                            &ctx.router_swap,
                            &ctx.activation_swap,
                        )
                        .await;
                    }
                    ReloadRequest::Config => {
                        apply_config_reload(&ctx, &mut current_config, &mut shutdown).await;
                    }
                }
            }
        }
    }
}

/// Run one config reload and record its outcome on the daemon meta: an
/// accepted reload advances the config, stamps the load, and recomputes
/// activation; a rejected one records its class. An abandoned one records
/// nothing.
async fn apply_config_reload(
    ctx: &ReloadContext,
    current_config: &mut Arc<Config>,
    shutdown: &mut watch::Receiver<()>,
) {
    let outcome = handle_config_reload(
        ctx.config_path.as_deref(),
        current_config,
        ctx.secrets.clone(),
        &ctx.router_swap,
        &ctx.usage,
        shutdown,
    )
    .await;
    match outcome {
        Ok(Some(new_config)) => {
            *current_config = new_config;
            ctx.daemon_meta.stamp_config_loaded();
            apply_activation(
                &ctx.oauth_store,
                current_config,
                &ctx.activation_swap,
                ActivationTrigger::ConfigChange,
            )
            .await;
        }
        Ok(None) => {}
        Err(failure) => ctx.daemon_meta.record_reload_failure(failure),
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
/// the new per-seat target set without a daemon restart. A changed record
/// for a seat the live Router holds as a placeholder rebuilds too, since a
/// placeholder has no provider for the in-place lift to readmit. The common
/// case -- routectl's own hourly-ish token auto-refresh, which rewrites
/// credentials.json with the same keys but new token values -- leaves
/// the seat set identical and skips the rebuild, so a routine refresh
/// costs only the cache reload it already performed.
pub(super) async fn handle_credentials_reload(
    oauth_store: &Option<Arc<routectl_auth::OAuthStore>>,
    current_config: &Arc<Config>,
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
    let changed_seats = match store.reload_from_disk().await {
        Ok(changed) => changed,
        Err(e) => {
            tracing::warn!(
                path = %store.path().display(),
                error = %e,
                "credentials reload failed; keeping previous in-memory cache",
            );
            return;
        }
    };
    tracing::info!(
        path = %store.path().display(),
        "credentials reloaded from disk",
    );

    // Gate the Router rebuild on a real seat-set change or a changed
    // placeholder seat. A token-value-only refresh of built seats (same keys)
    // must not rebuild, or every routine auto-refresh would needlessly re-run
    // the startup validators and re-expand the pool. A placeholder seat was
    // never built, so a lift alone cannot readmit it: a changed record for one
    // forces the rebuild even when the key set is unchanged (a dead record
    // re-logged under the same seat).
    let after = store.credential_keys().await;
    let readmits_placeholder = changed_seats
        .iter()
        .any(|seat| router_swap.load().is_placeholder_account(seat));
    let rebuilds = before != after || readmits_placeholder;

    // Without a rebuild the reload commits here: the live router already
    // dials with the reloaded tokens (the OAuth store is shared), so a seat
    // whose stored record changed gets a fresh attempt now. A seat this
    // process refreshed itself reads as unchanged, so a routine token refresh
    // lifts nothing.
    if !rebuilds {
        lift_changed_seat_parks(router_swap, &changed_seats);
    }

    // Recompute activation on EVERY successful credentials reload, rebuild or
    // not: activation tracks token PRESENCE (expired -> present is a real
    // transition), not the seat-key set, so a token-value-only refresh that
    // skips the rebuild must still recompute. Runs after `reload_from_disk`
    // so the probe observes the post-reload cache.
    apply_activation(
        oauth_store,
        current_config,
        activation_swap,
        ActivationTrigger::CredentialsChange,
    )
    .await;

    // A rebuild that fails publishes nothing and lifts nothing: the live
    // router keeps every park and placeholder mark it had.
    if rebuilds
        && rebuild_router_for_seat_change(current_config, secrets, router_swap, &before, &after)
            .await
    {
        lift_changed_seat_parks(router_swap, &changed_seats);
    }
}

/// Lift the parks of the OAuth seats a credentials reload changed, on the
/// router published at that reload's commit point, sparing every seat that
/// router holds as a placeholder: its unusable mark is the build's own
/// evidence about the record just read.
fn lift_changed_seat_parks(
    router_swap: &Arc<ArcSwap<Router>>,
    changed_seats: &std::collections::BTreeSet<String>,
) {
    let published = router_swap.load_full();
    let resets = published.lift_oauth_credential_parks(
        changed_seats
            .iter()
            .filter(|seat| !published.is_placeholder_account(seat)),
    );
    log_lifted_parks(&resets, "credentials");
}

/// Log the parks a reload of `source` lifted, if any. The lift itself also
/// started a new credential generation on each account it targeted, so an
/// attempt admitted before the reload cannot re-park that account.
fn log_lifted_parks(resets: &[routectl_router::runtime_state::AccountReset], source: &'static str) {
    let cleared = resets
        .iter()
        .filter(|reset| reset.outcome.park_lifted)
        .count();
    if cleared > 0 {
        tracing::info!(
            source,
            cleared,
            "reload cleared credential parks; parked seats will be retried",
        );
    }
}

/// Rebuild the live Router from the unchanged config after a seat-set
/// change or a changed placeholder seat (whose key set can be unchanged, so
/// the lines say which with `seat_set_changed`), preserving per-seat runtime
/// state and honoring the disk-first-keep-old invariant on a build failure. The config is not
/// re-read (it is unchanged); re-running the startup validators on it is
/// harmless. Split out of `handle_credentials_reload` to keep that function
/// under the size ceiling. Returns whether the rebuilt router was published.
async fn rebuild_router_for_seat_change(
    current_config: &Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    router_swap: &Arc<ArcSwap<Router>>,
    before: &std::collections::BTreeSet<String>,
    after: &std::collections::BTreeSet<String>,
) -> bool {
    let mut new_router = match build_router_from_config(current_config.clone(), secrets).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                error = %e,
                seat_set_changed = before != after,
                "credentials reload needed a router rebuild, but it failed; keeping previous router",
            );
            return false;
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
    new_router.carry_over_metrics_from(&router_swap.load_full());
    // After the pins are shared and the counters are: this reconciles the
    // pins against the new pool membership and counts each re-pick.
    new_router.carry_over_pool_state_from(&router_swap.load_full());
    new_router.carry_over_learned_from(&router_swap.load_full());
    // Stamps then stores, in that order -- see `publish_router`. This is the
    // commit point: every failure and abandonment path above has returned, so
    // the carried gates move to the new tuning and the new placeholder marks
    // reach the shared account map here and not at carry-over.
    new_router.commit_carried_state();
    publish_router(router_swap, Arc::new(new_router));
    tracing::info!(
        seats_before = before.len(),
        seats_after = after.len(),
        seat_set_changed = before != after,
        "credentials reload rebuilt the router and swapped it in",
    );
    true
}

/// Handle a `ReloadRequest::Config`: re-read config.toml via the shared
/// loader, rebuild the Router, and swap it in. Returns the new config on
/// success so the coordinator diffs the next reload against it. A rejected
/// reload warns, keeps the existing Router, and returns its
/// [`ReloadFailure`] class; `Ok(None)` means no verdict (no config path, or
/// shutdown abandoned the replay boundary).
pub(super) async fn handle_config_reload(
    config_path: Option<&Path>,
    current_config: &Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    router_swap: &Arc<ArcSwap<Router>>,
    usage: &UsageHandle,
    shutdown: &mut watch::Receiver<()>,
) -> Result<Option<Arc<Config>>, ReloadFailure> {
    let Some(path) = config_path else {
        tracing::debug!("config reload requested but no config path was registered; ignoring",);
        return Ok(None);
    };

    // `read_parse_validate_config` hits disk synchronously (the TOML read
    // and parse) -- run it off the runtime so a slow disk never stalls
    // every other task sharing this worker thread.
    let path_owned = path.to_path_buf();
    let loaded = loader_verdict(
        tokio::task::spawn_blocking(move || read_parse_validate_config(&path_owned)).await,
    )?;
    let new_config = Arc::new(loaded.config);

    let mut new_router = match build_router_from_config(new_config.clone(), secrets).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "config reload failed: router rebuild error; keeping previous router",
            );
            return Err(ReloadFailure::RouterBuildFailed);
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
    new_router.carry_over_metrics_from(&previous_router);
    // After the pins are shared and the counters are: this reconciles the
    // pins against the new pool membership and counts each re-pick.
    new_router.carry_over_pool_state_from(&previous_router);
    new_router.carry_over_learned_from(&previous_router);

    // A reload that advanced the capability revision stamps moves the replay
    // boundary. Read both stamps before the swap (this coordinator is the sole
    // writer, so the loaded Arc is the router being replaced).
    let revision_changed = previous_router.catalog_version() != new_router.catalog_version()
        || previous_router.overlay_revision() != new_router.overlay_revision();

    // Moving the boundary makes every row before the new tombstone invisible
    // to every later boot, so the carried verdicts must be re-appended past it
    // BEFORE the replacement router is published -- otherwise they act for
    // this process's life and vanish at the next restart.
    //
    // Three phases, in this order for a reason. The cut (snapshot + admit) runs
    // under the registry guard so no observation can land between the two and
    // be lost by both. The wait then runs with the guard RELEASED -- a lock is
    // never held across SQLite. Only a committed batch advances the generation,
    // runs the (no-op) catalog-scoped prune, and publishes; every failure leaves
    // generation, registry and published Router untouched.
    //
    // Unreachable while the revision stamps are frozen: both routers report
    // the same constants, so `revision_changed` is always false here.
    if revision_changed {
        let boundary_router = Arc::new(new_router);
        let Ok(admitted) =
            super::capability_boundary::admit_capability_boundary(usage, &boundary_router)
        else {
            tracing::warn!(
                "config reload failed: capability replay boundary not admitted; \
                 keeping previous router",
            );
            return Err(ReloadFailure::BoundaryNotAdmitted);
        };
        // Observations made from here on carry the PENDING generation, so their
        // events sort after the boundary the writer is about to commit rather
        // than being rejected as pre-boundary.
        boundary_router.set_pending_registry_generation(admitted.pending_generation());
        match admitted.settle(shutdown).await {
            super::capability_boundary::BoundaryOutcomeReport::Committed { .. } => {}
            super::capability_boundary::BoundaryOutcomeReport::Failed(reason) => {
                tracing::warn!(
                    reason = ?reason,
                    "config reload failed: capability replay boundary not durable; \
                     keeping previous router",
                );
                return Err(ReloadFailure::BoundaryNotDurable);
            }
            super::capability_boundary::BoundaryOutcomeReport::Abandoned => {
                tracing::debug!("config reload abandoned at shutdown; keeping previous router",);
                return Ok(None);
            }
        }
        // Past every failure and abandonment return, so this is the commit
        // point for the boundary-rebuilt router: only now may the carried
        // gates and the account map, still shared with the live router, take
        // the new tuning and placeholder marks.
        boundary_router.commit_carried_state();
        publish_router(router_swap, boundary_router);
    } else {
        new_router.commit_carried_state();
        publish_router(router_swap, Arc::new(new_router));
    }
    // Only past the commit point: the candidate shares the live router's park
    // map from carry-over on, so lifting any earlier would lift the live parks
    // on a reload that is then rejected.
    log_lifted_parks(
        &router_swap
            .load()
            .lift_entry_credential_parks(&previous_router.config, &new_config),
        "config",
    );

    // Flip the usage capture gate live. `db_path` and `retention_days` are
    // restart-required (the writer holds the DB handle opened at boot, and
    // pruning runs only at startup, so a changed value takes effect at the
    // next daemon start -- both surface in the restart-required warning
    // below). Only `enabled` flips at runtime.
    usage.set_enabled(new_config.usage.enabled);

    // Stamp the replay boundary at the post-reload revision so negatives
    // learned during the post-reload session sort after this tombstone and
    // replay on the next boot. The revision-changed case already committed its
    // boundary above (atomically, with the survivor restatements), so nothing
    // remains to enqueue here.

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
                trigger = CONFIG_RELOAD_TRIGGER,
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
    log_advisory_warnings(&new_config);

    let restart_required =
        crate::config_classify::collect_restart_required_changes(current_config, &new_config);
    if !restart_required.is_empty() {
        tracing::warn!(
            restart_required = ?restart_required,
            "config reload swapped routing state, but the listed fields require a daemon restart to take effect",
        );
    }

    Ok(Some(new_config))
}

/// Fold the blocking loader's join result into the reload verdict: a loader
/// panic or cancellation rejects the reload like any other load failure.
fn loader_verdict(
    joined: Result<Result<LoadedConfig, ReloadFailure>, tokio::task::JoinError>,
) -> Result<LoadedConfig, ReloadFailure> {
    joined.unwrap_or_else(|join_err| {
        // The join error's Display carries the panic payload, which can quote
        // whatever the loader was holding; log only its kind.
        tracing::warn!(
            is_panic = join_err.is_panic(),
            is_cancelled = join_err.is_cancelled(),
            "config reload failed: loader task did not complete; keeping previous config",
        );
        Err(ReloadFailure::LoaderPanicked)
    })
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
