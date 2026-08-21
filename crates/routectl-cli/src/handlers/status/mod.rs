//! Read-only `/status` family.
//!
//! Every route here returns a [`Panel`] envelope (or, for the `/status`
//! aggregate, a map of them). All but one are GETs; `/status/query` answers the
//! `QUERY` method instead, since it carries a request body. The surface is
//! deliberately read-only: [`StatusState`] carries only read handles, so a
//! status handler is structurally incapable of mutating the router, the usage
//! ledger, or the request-forwarding seam. Each panel is built through
//! `guard_panel`, which degrades a failing data source to a single unavailable
//! panel rather than a 500 or a process crash.
//!
//! [`status_router`] is merged into the serve process behind the
//! status-subtree-only middleware in [`crate::server::status_gate`] (a
//! `Host` allowlist + a bounded-concurrency load-shed) and, whenever the
//! bind requires it (tokens configured or a non-loopback bind), behind the
//! same listener auth layer as `/v1/*`; token-less loopback keeps the
//! zero-auth dev path. `/v1/*` inherits none of the status-only middleware.

pub(crate) mod builder_probe;
mod config;
mod daemon_meta;
mod doctor;
mod health;
mod page;
#[cfg(test)]
mod production_source;
mod query;
mod router_view;
mod types;
mod usage;

use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use arc_swap::ArcSwap;
use axum::extract::State;
use axum::routing::{any, get};
use axum::{Json, Router as AxumRouter};
use parking_lot::Mutex;
use routectl_router::ActivationState;
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError as TryAcquire};

pub use types::{Panel, now_utc_rfc3339, utc_rfc3339, vocabulary};

pub use daemon_meta::DaemonMeta;

pub use page::page_router;

use crate::server::AppState;
use crate::server::status_gate::STATUS_MAX_INFLIGHT;
use daemon_meta::DaemonMetaHandle;
use router_view::StatusRouterHandle;

/// Capacity for the blocking panel builders, sized to [`STATUS_MAX_INFLIGHT`].
///
/// The Tower concurrency gate cannot express this. Its permit lives inside the
/// RESPONSE FUTURE, so a client that aborts mid-request releases it while the
/// `spawn_blocking` builder that request already started keeps running --
/// capacity reads as free while detached blocking scans pile up. A permit taken
/// from here is MOVED into the blocking closure instead, so it is released by
/// the blocking work ending, never by the request future or the `JoinHandle`
/// being dropped.
///
/// Waiting on it cannot deadlock. Tower admits at most [`STATUS_MAX_INFLIGHT`]
/// requests, and an admitted request holds at most ONE builder permit at a time
/// (the `/status` aggregate awaits its four panels sequentially, in
/// `status_aggregate`), so there is no hold-and-wait edge to close a cycle
/// with. In the uncancelled steady state it is therefore never contended; it
/// blocks only in the case it exists for, a permit still held by a detached
/// builder from a cancelled request. That case DELAYS the next builder, it does
/// not shed it: shedding would need a panel-level reason code the wire does not
/// have.
///
/// One instance per [`StatusState`], i.e. one per serve process -- the same
/// scope as the single subtree-wide semaphore the Tower gate installs.
pub struct BuilderCapacity(Arc<Semaphore>);

impl Default for BuilderCapacity {
    fn default() -> Self {
        Self(Arc::new(Semaphore::new(STATUS_MAX_INFLIGHT)))
    }
}

impl BuilderCapacity {
    /// Take one builder permit without waiting.
    ///
    /// This is NOT a shed path -- [`guard_panel`] falls through to
    /// [`acquire`](Self::acquire) on [`TryAcquire::NoPermits`] and waits. It
    /// exists only to split the uncontended fast path (the whole steady state)
    /// from the contended one, so the contended path is observable at the
    /// instant it is taken rather than inferred from an absence.
    fn try_acquire(&self) -> Result<OwnedSemaphorePermit, TryAcquire> {
        Arc::clone(&self.0).try_acquire_owned()
    }

    /// Take one builder permit, waiting for capacity.
    ///
    /// `None` means the semaphore was closed. Nothing closes it, but the result
    /// is matched by value rather than unwrapped: the release profile is
    /// `panic = "abort"`, so a reachable panic on the serve path would take the
    /// whole daemon -- including the live proxy -- with it. The caller degrades
    /// a `None` to an unavailable panel carrying an existing reason code.
    async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.0).acquire_owned().await.ok()
    }

    #[cfg(test)]
    fn available_permits(&self) -> usize {
        self.0.available_permits()
    }
}

/// Shared state for the `/status` family. Carries ONLY read handles: the
/// live `Router` (behind a read-only facade, `StatusRouterHandle`) and the
/// activation inventory (behind its `ArcSwap`), plus the resolved paths a
/// panel builder needs to open the usage ledger or re-read the config file.
/// It never carries a usage writer handle or the MITM seam nonce, and the raw
/// `Router` is unreachable from a panel submodule (the facade exposes only
/// read methods), so no status handler can mutate state, dial an upstream, or
/// touch raw config.
pub struct StatusState {
    /// Live routing surface behind a read-only facade; `router.view()` once
    /// per panel build. The raw `Arc<ArcSwap<Router>>` is private to
    /// `StatusRouterHandle`, so a panel can reach only its read methods.
    pub router: StatusRouterHandle,
    /// Live auto-activation inventory.
    pub activation: Arc<ArcSwap<ActivationState>>,
    /// Resolved usage-ledger path, read once at construction from the live
    /// config so a Router hot-swap never changes it out from under a build.
    pub usage_db_path: PathBuf,
    /// Resolved config-file path, when serving from a real on-disk config.
    pub config_path: Option<PathBuf>,
    /// Process-level daemon facts (bound address, binary version, last
    /// config-load instant) behind a read-only facade. The config panel's
    /// source strip is the only reader.
    pub daemon_meta: DaemonMetaHandle,
    /// Per-panel availability + shed-count tracking. Each panel build
    /// records its outcome here; an availability edge logs a single
    /// transition line (never per poll).
    pub observability: PanelObservability,
    /// Cancellation-survivable capacity for the blocking panel builders. A
    /// permit is moved INTO each `spawn_blocking` closure by `guard_panel`, so
    /// it is released by the blocking work ending -- not by a cancelled
    /// request's future dropping. See [`BuilderCapacity`].
    pub builder_capacity: BuilderCapacity,
}

impl StatusState {
    /// Build from the running [`AppState`]. Clones the read handles and reads
    /// the usage-ledger path from the currently-installed config.
    pub fn from_app(
        app: &AppState,
        config_path: Option<PathBuf>,
        daemon_meta: Arc<DaemonMeta>,
    ) -> Self {
        let usage_db_path = app.router.load().config.usage.db_path.clone();
        Self {
            router: StatusRouterHandle::new(app.router.clone()),
            activation: app.activation.clone(),
            usage_db_path,
            config_path,
            daemon_meta: DaemonMetaHandle::new(daemon_meta),
            observability: PanelObservability::default(),
            builder_capacity: BuilderCapacity::default(),
        }
    }
}

/// Per-panel availability + shed-count scaffold. Each [`PanelCounters`]
/// tracks its panel's last-seen availability and logs a single line on
/// every availability EDGE (available<->unavailable), never per poll -- a
/// steady 2-5s poll loop against a panel that stays unavailable emits at
/// most one line.
pub struct PanelObservability {
    pub usage: PanelCounters,
    pub health: PanelCounters,
    pub config: PanelCounters,
    pub doctor: PanelCounters,
    pub query: PanelCounters,
    /// The `/status/query` series mode. Tracked apart from `query` because a
    /// series read fails on its own terms (a wider GROUP BY over a larger temp
    /// b-tree), and one shared detector would let a healthy aggregate poll mask
    /// a consistently failing series poll.
    pub query_series: PanelCounters,
}

impl Default for PanelObservability {
    fn default() -> Self {
        Self {
            usage: PanelCounters::new("usage"),
            health: PanelCounters::new("health"),
            config: PanelCounters::new("config"),
            doctor: PanelCounters::new("doctor"),
            query: PanelCounters::new("status_query"),
            query_series: PanelCounters::new("status_query_series"),
        }
    }
}

/// `tracing` target for status panel availability transitions. One target
/// per module so an operator can filter the status surface's log lines.
const OBSERVABILITY_TARGET: &str = "routectl::status";

/// A panel's availability, tracked for edge detection. `Unknown` is the
/// pre-first-observation state, so the very first build is itself an edge
/// (a fresh install missing its usage DB logs one line, not a per-poll
/// repeat).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Availability {
    Unknown,
    Available,
    Unavailable,
}

impl Availability {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Available => 1,
            Self::Unavailable => 2,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Available,
            2 => Self::Unavailable,
            _ => Self::Unknown,
        }
    }
}

/// Availability scaffold for a single panel: the last observed
/// availability edge, the last instant it produced data, and how many
/// times it has been served unavailable.
pub struct PanelCounters {
    /// Panel name, emitted as a structured field on a transition line.
    name: &'static str,
    /// Last observed availability (see [`Availability`]), swapped
    /// atomically so exactly ONE concurrent poll observes a given edge.
    availability: AtomicU8,
    last_available: Mutex<Option<String>>,
    shed_count: AtomicU64,
}

impl PanelCounters {
    const fn new(name: &'static str) -> Self {
        Self {
            name,
            availability: AtomicU8::new(Availability::Unknown.as_u8()),
            last_available: Mutex::new(None),
            shed_count: AtomicU64::new(0),
        }
    }

    /// Record a build outcome: stamp `last_available` on a live panel, or
    /// bump the shed counter on an unavailable one, then log ONCE if this
    /// build flipped the panel's availability. The atomic swap makes the
    /// edge fire for exactly one concurrent caller, so a poll loop never
    /// spams. The `parking_lot::Mutex` has no poisoning, so the stamp
    /// always lands.
    fn record<T>(&self, panel: &Panel<T>) {
        let now = if panel.unavailable.is_none() {
            if let Some(as_of) = panel.as_of.as_ref() {
                *self.last_available.lock() = Some(as_of.clone());
            }
            Availability::Available
        } else {
            self.shed_count.fetch_add(1, Ordering::Relaxed);
            Availability::Unavailable
        };
        let prev = Availability::from_u8(self.availability.swap(now.as_u8(), Ordering::Relaxed));
        // Known, accepted limitation: under concurrent polls of the SAME panel
        // the swap serializes the edges but the subsequent `log_transition`
        // calls can interleave, so a rapid flap may log its transition lines
        // out of chronological order. This is a diagnostic-log artifact only --
        // the served panel data is unaffected -- so it is left as-is rather
        // than paying for generation-CAS ordering machinery.
        if prev != now {
            self.log_transition(now, panel.unavailable.as_deref());
        }
    }

    /// Log a single availability edge. Transitions are DEBUG (a panel going
    /// unavailable is graceful degradation, not a failure -- a fresh-install
    /// missing usage DB must not warn every poll). Only the fixed reason
    /// `code` is logged, never a path, secret, or raw error.
    fn log_transition(&self, now: Availability, code: Option<&str>) {
        match now {
            Availability::Unavailable => tracing::debug!(
                target: OBSERVABILITY_TARGET,
                panel = self.name,
                code = code.unwrap_or("unknown"),
                "status panel became unavailable",
            ),
            Availability::Available => tracing::debug!(
                target: OBSERVABILITY_TARGET,
                panel = self.name,
                "status panel became available",
            ),
            Availability::Unknown => {}
        }
    }
}

/// Panic-isolation wrapper every panel builder runs through. A status panel
/// is best-effort: a broken data source degrades that one panel to
/// unavailable, never a 500 and never a process crash. This maps three
/// outcomes to an unavailable panel carrying `code`:
///   - the builder panics (caught via `catch_unwind`),
///   - the `spawn_blocking` join itself fails,
///   - the builder-capacity semaphore is closed (never happens; matched by
///     value rather than unwrapped so no reachable panic reaches the daemon).
///
/// The builder runs on a blocking worker because real panel sources do
/// synchronous I/O (open the usage ledger, re-read the config file).
///
/// One capacity permit is taken on the async side and MOVED into the blocking
/// closure, so it is held for the builder's whole run and released only when
/// the blocking work ends -- on normal return, on error return, and (in unwind
/// builds) on panic. Dropping the response future or the `JoinHandle` does NOT
/// release it, because the permit lives inside the already-queued blocking
/// task. That is the property that keeps a cancelled request from freeing
/// capacity while its detached builder runs on. See [`BuilderCapacity`].
pub(super) async fn guard_panel<T, F>(
    capacity: &BuilderCapacity,
    schema_version: u32,
    code: &'static str,
    builder: F,
) -> Panel<T>
where
    T: Send + 'static,
    F: FnOnce() -> Panel<T> + Send + 'static,
{
    // Captured on the async side: a blocking worker inherits no task-locals.
    // Compiles away entirely outside test builds.
    let probe = builder_probe::current();
    // Match by value: a closed semaphore degrades to an unavailable panel, it
    // never unwraps into a panic on the serve path (`panic = "abort"`).
    let permit = match capacity.try_acquire() {
        Ok(permit) => permit,
        Err(TryAcquire::NoPermits) => {
            // Contended: every builder permit is held (in practice only by a
            // detached builder from a cancelled request). Signal the gate so a
            // test can observe the delay deterministically, then WAIT -- this is
            // a delay, never a shed.
            builder_probe::capacity_exhausted(&probe);
            match capacity.acquire().await {
                Some(permit) => permit,
                None => return Panel::unavailable(schema_version, code),
            }
        }
        Err(TryAcquire::Closed) => return Panel::unavailable(schema_version, code),
    };
    builder_probe::submitted(&probe);
    let joined = tokio::task::spawn_blocking(move || {
        // The permit is owned here, so it drops when this closure exits by any
        // path -- return, error, or (in unwind builds) panic -- and never when
        // a cancelled request drops the future or the join handle.
        let _permit = permit;
        builder_probe::park(&probe);
        panic::catch_unwind(AssertUnwindSafe(builder))
    })
    .await;
    match joined {
        Ok(Ok(panel)) => panel,
        _ => Panel::unavailable(schema_version, code),
    }
}

/// The typed sub-router for the `/status` family. Every panel path is GET-only;
/// a non-GET request to one gets a 405 from axum's method router.
///
/// `/status/query` is the ONE carve-out: it answers the `QUERY` method, which
/// axum's `MethodFilter` cannot express, so it registers with `any()` and its
/// handler guards the method itself (405 for everything but `QUERY`). The
/// carve-out is route-scoped -- `QUERY` against any other status path is still
/// a 405.
pub fn status_router() -> AxumRouter<Arc<StatusState>> {
    AxumRouter::new()
        .route("/status", get(status_aggregate))
        .route("/status/usage", get(usage::handler))
        .route("/status/health", get(health::handler))
        .route("/status/config", get(config::handler))
        .route("/status/doctor", get(doctor::handler))
        .route("/status/query", any(query::handler))
}

/// The `/status` aggregate: composes the four panels into one envelope.
///
/// Each panel is an INDEPENDENT envelope carrying its own `schema_version`,
/// `as_of`, and availability -- there is deliberately no outer envelope
/// version. This is push-ready by construction: a future push event is the
/// same per-panel shape keyed by panel name, so the wire contract does not
/// change when one arrives.
#[derive(Debug, Serialize)]
struct StatusAggregate {
    panels: AggregatePanels,
}

#[derive(Debug, Serialize)]
struct AggregatePanels {
    usage: Panel<usage::UsagePanel>,
    health: Panel<health::HealthPanel>,
    config: Panel<config::ConfigPanel>,
    doctor: Panel<doctor::DoctorPanel>,
}

async fn status_aggregate(State(state): State<Arc<StatusState>>) -> Json<StatusAggregate> {
    // Compose the four guarded builders SEQUENTIALLY: each spawns its own
    // `spawn_blocking` job, so awaiting them one at a time keeps a single
    // admitted request to at most ONE in-flight blocking builder. That is what
    // makes `STATUS_MAX_INFLIGHT` name a single unit -- admitted requests and
    // in-flight builders are then numerically identical. A concurrent join
    // would fan one admitted request into four blocking jobs, so the cap would
    // admit up to four times as many builders as requests. Each builder still
    // isolates its own source failure / panic through `guard_panel`, and these
    // are the SAME builders the per-panel endpoints call -- no divergent second
    // mapping.
    let usage = usage::build(&state).await;
    let health = health::build(&state).await;
    let config = config::build(&state).await;
    let doctor = doctor::build(&state).await;
    Json(StatusAggregate {
        panels: AggregatePanels {
            usage,
            health,
            config,
            doctor,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use routectl_router::{Config, Router};
    use serde_json::Value;
    use tower::ServiceExt;

    fn test_state() -> Arc<StatusState> {
        let router = Router::new(Arc::new(Config::default()));
        let (app, _dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        // No panel opens the ledger in the skeleton, so the temp dir may drop
        // immediately -- `usage_db_path` is only read when a real source is
        // wired.
        Arc::new(StatusState::from_app(&app, None, DaemonMeta::for_test()))
    }

    /// The GET-only panel paths. `/status/query` is deliberately absent: it
    /// owns the QUERY-method carve-out and is covered by its own tests.
    const STATUS_PATHS: &[&str] = &[
        "/status",
        "/status/usage",
        "/status/health",
        "/status/config",
        "/status/doctor",
    ];

    #[tokio::test]
    async fn get_succeeds_and_non_get_returns_405() {
        for path in STATUS_PATHS {
            let app = status_router().with_state(test_state());
            let get_resp = app
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(*path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(get_resp.status(), StatusCode::OK, "GET {path}");

            // "QUERY" is in the rejected set so the /status/query carve-out is
            // pinned as ROUTE-SCOPED: registering `any()` on one path must
            // never widen the method surface of its siblings.
            for method in ["POST", "PUT", "DELETE", "PATCH", "QUERY"] {
                let app = status_router().with_state(test_state());
                let resp = app
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(*path)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    resp.status(),
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path} must be 405 (GET-only route)"
                );
            }
        }
    }

    /// Guards the read-only contract: no status source may name the mutation
    /// / over-coupling surfaces. A future handler that reaches for a usage
    /// writer, an activation delta, a breaker force-open, the MITM seam
    /// nonce, provider-state construction, or the router config type (the
    /// import shape a raw config serialize would need) trips this. The
    /// dispatch tokens (`.complete(`, `.stream(`, `.dispatch`) and the raw
    /// `router.config` field access are the secondary tripwire behind the
    /// structural facade ([`router_view`]): the facade already makes a raw
    /// `&Router` unreachable from a panel, and these tokens catch a
    /// regression that tried to route around it. `router_view.rs` (the facade
    /// itself, the one file that could widen the public read surface) is
    /// scanned too, but with an ADJUSTED set: it legitimately names
    /// `Arc<Router>` as a private field and reads `router.config` INSIDE the
    /// derive, so those are not forbidden there; instead the facade is guarded
    /// against a NEW leak-capable method -- a raw-router / arc-router return, a
    /// `config` accessor, or a dispatch wrapper. Tokens are built by
    /// concatenation and only the pre-test slice of each file is scanned, so
    /// this test's own text never triggers a false positive.
    #[test]
    fn forbidden_tokens_absent_from_status_sources() {
        // Coupling / mutation / dispatch tokens forbidden in EVERY status
        // source, panels and facade alike.
        let common: Vec<String> = vec![
            format!("{}{}", "Usage", "Writer"),
            format!("{}{}", "Usage", "Handle"),
            format!("{}{}", "Activation", "Delta"),
            format!("{}{}", "force_open", "_breaker"),
            format!("{}{}", "mitm_seam", "_nonce"),
            format!("{}{}", "Provider", "State"),
            format!("{}{}", "routectl_router::", "Config"),
            format!("{}{}", ".complete", "("),
            format!("{}{}", ".stream", "("),
            format!("{}{}", ".dis", "patch"),
        ];

        // Panels additionally must never touch the raw `router.config` field;
        // the facade reads it legitimately inside `effective_view`, so this is
        // panel-only.
        let mut panel_forbidden = common.clone();
        panel_forbidden.push(format!("{}{}", "router", ".config"));

        // The facade is additionally guarded against widening its read surface
        // with a raw-router / arc-router return or a config accessor -- adding
        // any of these ships a leak-capable method and fails here.
        let mut facade_forbidden = common.clone();
        facade_forbidden.push(format!("{}{}", "-> &", "Router"));
        facade_forbidden.push(format!("{}{}{}", "-> Arc<", "Router", ">"));
        facade_forbidden.push(format!("{}{}", "fn ", "config"));

        let scans: &[(&str, &str, &[String])] = &[
            ("mod.rs", include_str!("mod.rs"), &panel_forbidden),
            (
                "builder_probe.rs",
                include_str!("builder_probe.rs"),
                &panel_forbidden,
            ),
            ("page.rs", include_str!("page.rs"), &panel_forbidden),
            ("types.rs", include_str!("types.rs"), &panel_forbidden),
            ("usage.rs", include_str!("usage.rs"), &panel_forbidden),
            ("query.rs", include_str!("query.rs"), &panel_forbidden),
            ("health.rs", include_str!("health.rs"), &panel_forbidden),
            ("config.rs", include_str!("config.rs"), &panel_forbidden),
            ("doctor.rs", include_str!("doctor.rs"), &panel_forbidden),
            (
                "daemon_meta.rs",
                include_str!("daemon_meta.rs"),
                &panel_forbidden,
            ),
            (
                "router_view.rs",
                include_str!("router_view.rs"),
                &facade_forbidden,
            ),
        ];
        for (name, src, forbidden) in scans {
            let production = production_source::production_source(src);
            for token in *forbidden {
                assert!(
                    !production.contains(token.as_str()),
                    "forbidden token `{token}` present in status source {name}"
                );
            }
        }
    }

    /// The builder permit is released on the NON-success exits too, so a panel
    /// that fails repeatedly cannot leak capacity a poll at a time until the
    /// status surface wedges.
    ///
    /// Success is covered by every other test in this module (they would all
    /// hang at the fourth build otherwise). This pins the two that a naive
    /// implementation gets wrong:
    ///   - an ERROR return, i.e. a builder that returns an unavailable panel;
    ///   - a PANIC, which unwinds THROUGH the permit rather than returning past
    ///     it.
    ///
    /// Scope note on the panic case: it holds under `debug_assertions` only, and
    /// proves that an unwind drops the permit. It does NOT -- and cannot --
    /// prove release-build panic recovery: the release profile is
    /// `panic = "abort"`, so a panicking builder there takes the process, and
    /// permit accounting is moot because restart reconstructs all state.
    #[tokio::test]
    async fn builder_permit_is_released_on_error_and_on_unwinding_panic() {
        let capacity = BuilderCapacity::default();
        let all = capacity.available_permits();
        assert_eq!(all, STATUS_MAX_INFLIGHT);

        let errored: Panel<u32> = guard_panel(&capacity, 1, vocabulary::codes::DB_BUSY, || {
            Panel::unavailable(1, vocabulary::codes::DB_UNAVAILABLE)
        })
        .await;
        assert_eq!(errored.unavailable.as_deref(), Some("db_unavailable"));
        assert_eq!(
            capacity.available_permits(),
            all,
            "an unavailable panel must return its builder permit"
        );

        #[cfg(debug_assertions)]
        {
            let panicked: Panel<u32> =
                guard_panel(&capacity, 1, vocabulary::codes::DB_UNAVAILABLE, || {
                    panic!("data source blew up")
                })
                .await;
            assert_eq!(panicked.unavailable.as_deref(), Some("db_unavailable"));
            assert_eq!(
                capacity.available_permits(),
                all,
                "an unwinding builder must drop its permit on the way out"
            );
        }

        // Capacity is genuinely reusable afterwards, not merely counted back:
        // STATUS_MAX_INFLIGHT more builders run without blocking.
        for _ in 0..STATUS_MAX_INFLIGHT {
            let ok: Panel<u32> = guard_panel(&capacity, 1, vocabulary::codes::NO_DATA, || {
                Panel::available(1, now_utc_rfc3339(), 1)
            })
            .await;
            assert_eq!(ok.data, Some(1));
        }
        assert_eq!(capacity.available_permits(), all);
    }

    #[test]
    fn available_panel_clears_unavailable_and_sets_as_of() {
        let panel = Panel::available(3, "2026-07-15T00:00:00Z".to_string(), 42u32);
        assert_eq!(panel.schema_version, 3);
        assert_eq!(panel.as_of.as_deref(), Some("2026-07-15T00:00:00Z"));
        assert_eq!(panel.data, Some(42));
        assert!(panel.unavailable.is_none());
    }

    #[test]
    fn unavailable_panel_clears_data_and_as_of() {
        let panel = Panel::<u32>::unavailable(3, vocabulary::codes::NO_DATA);
        assert!(panel.data.is_none());
        assert!(panel.as_of.is_none());
        assert_eq!(panel.unavailable.as_deref(), Some("no_data"));
    }

    #[test]
    fn panels_serialize_to_snake_case_wire_shape() {
        let available = serde_json::to_value(Panel::available(
            1,
            "2026-07-15T00:00:00Z".to_string(),
            7u32,
        ))
        .unwrap();
        assert_eq!(available["schema_version"], 1);
        assert_eq!(available["as_of"], "2026-07-15T00:00:00Z");
        assert_eq!(available["data"], 7);
        assert_eq!(available["unavailable"], Value::Null);

        let unavailable = serde_json::to_value(Panel::<u32>::unavailable(
            1,
            vocabulary::codes::DB_UNAVAILABLE,
        ))
        .unwrap();
        assert_eq!(unavailable["schema_version"], 1);
        assert_eq!(unavailable["as_of"], Value::Null);
        assert_eq!(unavailable["data"], Value::Null);
        assert_eq!(unavailable["unavailable"], "db_unavailable");
    }

    #[tokio::test]
    async fn guard_isolates_panic() {
        let capacity = BuilderCapacity::default();
        let panicked: Panel<u32> =
            guard_panel(&capacity, 1, vocabulary::codes::DB_UNAVAILABLE, || {
                panic!("data source blew up");
            })
            .await;
        assert_eq!(panicked.unavailable.as_deref(), Some("db_unavailable"));
        assert!(panicked.data.is_none());

        let ok: Panel<u32> = guard_panel(&capacity, 1, vocabulary::codes::NO_DATA, || {
            Panel::available(1, now_utc_rfc3339(), 5)
        })
        .await;
        assert_eq!(ok.data, Some(5));
        assert!(ok.unavailable.is_none());
    }

    /// Test #6 (panic isolation): the aggregate composes four guarded builders
    /// with sequential awaits. A panic in ONE builder degrades only that panel
    /// to unavailable; the builders after it still run and build normally, the
    /// one before it keeps its value, and no panic escapes. Exercises the exact
    /// sequential shape via a panicking test double, with the panicking builder
    /// in the MIDDLE so both the before- and after-it cases are covered.
    #[tokio::test]
    async fn sequential_composition_isolates_a_panicking_panel() {
        async fn ok_panel(capacity: &BuilderCapacity, value: u32) -> Panel<u32> {
            guard_panel(capacity, 1, vocabulary::codes::DB_UNAVAILABLE, move || {
                Panel::available(1, now_utc_rfc3339(), value)
            })
            .await
        }
        async fn panicking_panel(capacity: &BuilderCapacity) -> Panel<u32> {
            guard_panel(capacity, 1, vocabulary::codes::DB_UNAVAILABLE, || {
                panic!("panel source blew up")
            })
            .await
        }

        let capacity = BuilderCapacity::default();
        let a = ok_panel(&capacity, 2).await;
        let panicked = panicking_panel(&capacity).await;
        let b = ok_panel(&capacity, 3).await;
        let c = ok_panel(&capacity, 4).await;

        assert_eq!(panicked.unavailable.as_deref(), Some("db_unavailable"));
        assert!(panicked.data.is_none());
        assert!(panicked.as_of.is_none());
        assert_eq!(a.data, Some(2));
        assert_eq!(b.data, Some(3));
        assert_eq!(c.data, Some(4));
        assert!(a.unavailable.is_none());
    }

    /// Test #5 (partial failure) + the no-outer-version contract. The usage
    /// source is unavailable (absent ledger) while health/config/doctor
    /// succeed: `GET /status` is HTTP 200, the usage panel is unavailable, the
    /// other three are available, each panel carries its OWN `schema_version`,
    /// and there is no outer envelope version field.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn aggregate_partial_failure_isolates_unavailable_usage() {
        use axum::body::to_bytes;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!("version = {}\n", routectl_router::CURRENT_CONFIG_VERSION),
        )
        .unwrap();

        let router = Router::new(Arc::new(Config::default()));
        let (app, _writer_dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        let mut status = StatusState::from_app(&app, Some(config_path), DaemonMeta::for_test());
        // Point usage at an absent ledger so ONLY the usage panel sheds.
        status.usage_db_path = dir.path().join("absent-usage.db");
        let state = Arc::new(status);

        let app = status_router().with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();

        // No OUTER envelope version -- only the per-panel map.
        assert!(
            json.get("schema_version").is_none(),
            "aggregate must not carry an outer schema_version"
        );
        let panels = &json["panels"];

        // Usage sheds to a code-only unavailable panel.
        assert_eq!(panels["usage"]["unavailable"], "no_data");
        assert!(panels["usage"]["data"].is_null());
        assert!(panels["usage"]["as_of"].is_null());
        assert_eq!(panels["usage"]["schema_version"], 3);

        // The three siblings are available and untouched.
        for name in ["health", "config", "doctor"] {
            assert!(
                panels[name]["unavailable"].is_null(),
                "{name} panel should be available"
            );
            assert!(
                !panels[name]["data"].is_null(),
                "{name} panel should carry data"
            );
        }
        // Each panel carries its OWN schema_version (usage 3, health 5,
        // doctor 8, config 3).
        assert_eq!(panels["health"]["schema_version"], 5);
        assert_eq!(panels["config"]["schema_version"], 3);
        assert_eq!(panels["doctor"]["schema_version"], 8);
    }
}
