//! Read-only `/status` family.
//!
//! Every route here is a GET that returns a [`Panel`] envelope (or, for the
//! `/status` aggregate, a map of them). The surface is deliberately
//! read-only: [`StatusState`] carries only read handles, so a status handler
//! is structurally incapable of mutating the router, the usage ledger, or the
//! request-forwarding seam. Each panel is built through [`guard_panel`], which
//! degrades a failing data source to a single unavailable panel rather than a
//! 500 or a process crash.
//!
//! [`status_router`] is merged into the serve process behind the
//! status-subtree-only middleware in [`crate::server::status_gate`] (a
//! `Host` allowlist + a bounded-concurrency load-shed) and, whenever the
//! bind requires it (tokens configured or a non-loopback bind), behind the
//! same listener auth layer as `/v1/*`; token-less loopback keeps the
//! zero-auth dev path. `/v1/*` inherits none of the status-only middleware.

mod config;
mod doctor;
mod health;
mod page;
mod router_view;
mod types;
mod usage;

use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use arc_swap::ArcSwap;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router as AxumRouter};
use parking_lot::Mutex;
use routectl_router::ActivationState;
use serde::Serialize;

pub use types::{Panel, now_utc_rfc3339, vocabulary};

pub use page::page_router;

use crate::server::AppState;
use router_view::StatusRouterHandle;

/// Shared state for the `/status` family. Carries ONLY read handles: the
/// live `Router` (behind a read-only facade, [`StatusRouterHandle`]) and the
/// activation inventory (behind its `ArcSwap`), plus the resolved paths a
/// panel builder needs to open the usage ledger or re-read the config file.
/// It never carries a usage writer handle or the MITM seam nonce, and the raw
/// `Router` is unreachable from a panel submodule (the facade exposes only
/// read methods), so no status handler can mutate state, dial an upstream, or
/// touch raw config.
pub struct StatusState {
    /// Live routing surface behind a read-only facade; `router.view()` once
    /// per panel build. The raw `Arc<ArcSwap<Router>>` is private to
    /// [`StatusRouterHandle`], so a panel can reach only its read methods.
    pub router: StatusRouterHandle,
    /// Live auto-activation inventory.
    pub activation: Arc<ArcSwap<ActivationState>>,
    /// Resolved usage-ledger path, read once at construction from the live
    /// config so a Router hot-swap never changes it out from under a build.
    pub usage_db_path: PathBuf,
    /// Resolved config-file path, when serving from a real on-disk config.
    pub config_path: Option<PathBuf>,
    /// Per-panel availability + shed-count tracking. Each panel build
    /// records its outcome here; an availability edge logs a single
    /// transition line (never per poll).
    pub observability: PanelObservability,
}

impl StatusState {
    /// Build from the running [`AppState`]. Clones the read handles and reads
    /// the usage-ledger path from the currently-installed config.
    pub fn from_app(app: &AppState, config_path: Option<PathBuf>) -> Self {
        let usage_db_path = app.router.load().config.usage.db_path.clone();
        Self {
            router: StatusRouterHandle::new(app.router.clone()),
            activation: app.activation.clone(),
            usage_db_path,
            config_path,
            observability: PanelObservability::default(),
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
}

impl Default for PanelObservability {
    fn default() -> Self {
        Self {
            usage: PanelCounters::new("usage"),
            health: PanelCounters::new("health"),
            config: PanelCounters::new("config"),
            doctor: PanelCounters::new("doctor"),
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
/// unavailable, never a 500 and never a process crash. This maps both
/// failure modes to an unavailable panel carrying `code`:
///   - the builder panics (caught via `catch_unwind`),
///   - the `spawn_blocking` join itself fails.
///
/// The builder runs on a blocking worker because real panel sources do
/// synchronous I/O (open the usage ledger, re-read the config file).
pub(super) async fn guard_panel<T, F>(
    schema_version: u32,
    code: &'static str,
    builder: F,
) -> Panel<T>
where
    T: Send + 'static,
    F: FnOnce() -> Panel<T> + Send + 'static,
{
    let joined =
        tokio::task::spawn_blocking(move || panic::catch_unwind(AssertUnwindSafe(builder))).await;
    match joined {
        Ok(Ok(panel)) => panel,
        _ => Panel::unavailable(schema_version, code),
    }
}

/// The typed sub-router for the `/status` family. Registers GET-only routes;
/// a non-GET request to any path gets a 405 from axum's method router. The
/// merge into the main app router happens in a later wiring task.
pub fn status_router() -> AxumRouter<Arc<StatusState>> {
    AxumRouter::new()
        .route("/status", get(status_aggregate))
        .route("/status/usage", get(usage::handler))
        .route("/status/health", get(health::handler))
        .route("/status/config", get(config::handler))
        .route("/status/doctor", get(doctor::handler))
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
    // Compose the four guarded builders CONCURRENTLY: each already isolates
    // its own source failure / panic through `guard_panel` (so one bad panel
    // renders only itself unavailable), and each does blocking I/O on a
    // `spawn_blocking` worker, so joining them means one slow panel does not
    // serialize behind -- or stall -- the other three. These are the SAME
    // builders the per-panel endpoints call; there is no divergent second
    // mapping.
    let (usage, health, config, doctor) = tokio::join!(
        usage::build(&state),
        health::build(&state),
        config::build(&state),
        doctor::build(&state),
    );
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
        Arc::new(StatusState::from_app(&app, None))
    }

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

            for method in ["POST", "PUT", "DELETE", "PATCH"] {
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
            ("page.rs", include_str!("page.rs"), &panel_forbidden),
            ("types.rs", include_str!("types.rs"), &panel_forbidden),
            ("usage.rs", include_str!("usage.rs"), &panel_forbidden),
            ("health.rs", include_str!("health.rs"), &panel_forbidden),
            ("config.rs", include_str!("config.rs"), &panel_forbidden),
            ("doctor.rs", include_str!("doctor.rs"), &panel_forbidden),
            (
                "router_view.rs",
                include_str!("router_view.rs"),
                &facade_forbidden,
            ),
        ];
        for (name, src, forbidden) in scans {
            let production = match src.find("#[cfg(test)]") {
                Some(idx) => &src[..idx],
                None => *src,
            };
            for token in *forbidden {
                assert!(
                    !production.contains(token.as_str()),
                    "forbidden token `{token}` present in status source {name}"
                );
            }
        }
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
        let panicked: Panel<u32> = guard_panel(1, vocabulary::codes::DB_UNAVAILABLE, || {
            panic!("data source blew up");
        })
        .await;
        assert_eq!(panicked.unavailable.as_deref(), Some("db_unavailable"));
        assert!(panicked.data.is_none());

        let ok: Panel<u32> = guard_panel(1, vocabulary::codes::NO_DATA, || {
            Panel::available(1, now_utc_rfc3339(), 5)
        })
        .await;
        assert_eq!(ok.data, Some(5));
        assert!(ok.unavailable.is_none());
    }

    /// Test #6 (panic isolation): the aggregate composes four guarded builders
    /// with `tokio::join!`. A panic in ONE builder degrades only that panel to
    /// unavailable; the other three build normally and no panic escapes.
    /// Exercises the exact concurrent-join shape via a panicking test double.
    #[tokio::test]
    async fn concurrent_composition_isolates_a_panicking_panel() {
        async fn ok_panel(value: u32) -> Panel<u32> {
            guard_panel(1, vocabulary::codes::DB_UNAVAILABLE, move || {
                Panel::available(1, now_utc_rfc3339(), value)
            })
            .await
        }
        async fn panicking_panel() -> Panel<u32> {
            guard_panel(1, vocabulary::codes::DB_UNAVAILABLE, || {
                panic!("panel source blew up")
            })
            .await
        }

        let (panicked, a, b, c) =
            tokio::join!(panicking_panel(), ok_panel(2), ok_panel(3), ok_panel(4),);

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
        std::fs::write(&config_path, b"version = 3\n").unwrap();

        let router = Router::new(Arc::new(Config::default()));
        let (app, _writer_dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        let mut status = StatusState::from_app(&app, Some(config_path));
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
        assert_eq!(panels["usage"]["schema_version"], 2);

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
        // Each panel carries its OWN schema_version (usage 2, health 2,
        // doctor 3, config 1).
        assert_eq!(panels["health"]["schema_version"], 2);
        assert_eq!(panels["config"]["schema_version"], 1);
        assert_eq!(panels["doctor"]["schema_version"], 3);
    }
}
