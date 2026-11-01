//! The background seat driver: which seats it sweeps, how each refresh
//! outcome lands on the router under the generation acquired before the
//! refresh, its cadence across a long gap, its shutdown bound, and the
//! health it keeps. Token-endpoint answers come from the auth store's
//! scripted refresh seam; nothing dials a vendor.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use routectl_auth::SecretStore;
use routectl_auth::oauth::testing::{ScriptedRefresh, ScriptedRefreshFlow};
use routectl_router::{ActivationState, CURRENT_CONFIG_VERSION, CatalogOverlay};
use routectl_testkit::{ScopedEnv, with_capture};
use tokio::sync::oneshot;

use super::*;
use crate::server::reload::{handle_credentials_reload, spawn_reload_pipeline};
use crate::server::reload_shutdown::{RELOAD_TASK_SHUTDOWN_DEADLINE, await_reload_tasks};
use crate::server::router_build::build_router_from_config;
use crate::server::test_support::isolate_usage_db;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Inside the refresh lead window, still valid.
fn due_soon() -> u64 {
    unix_now() + 60
}

/// Outside the refresh lead window.
fn far_off() -> u64 {
    unix_now() + 3600
}

/// Already past its expiry.
fn expired() -> u64 {
    unix_now() - 10
}

/// Write a credentials file holding one record per `(seat key, access
/// token, expires_at_unix)`.
fn write_credentials(path: &Path, seats: &[(&str, &str, u64)]) {
    let mut providers = serde_json::Map::new();
    for (key, token, expires_at) in seats {
        providers.insert(
            (*key).to_string(),
            serde_json::json!({
                "access_token": token,
                "refresh_token": "seeded-refresh-token",
                "token_type": "Bearer",
                "expires_at_unix": expires_at,
                "scopes": ["user:inference"],
                "account": { "email": null, "account_id": null },
                "obtained_at_unix": 0
            }),
        );
    }
    let doc = serde_json::json!({ "schema_version": 1, "providers": providers });
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// Two models on managed seat `anthropic#a` and one on an xAI login: the
/// driver must refresh the first seat once and never the second.
fn rig_config_text() -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\nhost = \"127.0.0.1\"\nport = 0\n\n\
         [providers.seat_a]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#a\"\n\
         auth_kind = \"oauth-bearer\"\n\n\
         [providers.grok]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://api.x.ai/v1\"\n\
         api_key_ref = \"oauth://xai\"\n\n\
         [models.claude-a]\nprovider = \"seat_a\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.claude-a2]\nprovider = \"seat_a\"\nupstream = \"claude-opus-4-8\"\n\n\
         [models.grok]\nprovider = \"grok\"\nupstream = \"grok-4\"\n"
    )
}

const SEAT_A: &str = "anthropic#a";
const SEAT_A_MODELS: [&str; 2] = ["claude-a", "claude-a2"];

/// A live router over [`rig_config_text`] whose credential store answers
/// every refresh POST from a script.
struct Rig {
    creds: PathBuf,
    flow: Arc<ScriptedRefreshFlow>,
    store: Arc<OAuthStore>,
    config: Arc<Config>,
    swap: Arc<ArcSwap<Router>>,
    health: SeatDriverHealth,
    _usage_dir: tempfile::TempDir,
    _xdg: ScopedEnv,
    _dir: tempfile::TempDir,
}

impl Rig {
    async fn new(seats: &[(&str, &str, u64)], script: ScriptedRefresh) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let creds = dir.path().join("routectl").join("credentials.json");
        write_credentials(&creds, seats);
        let flow = Arc::new(ScriptedRefreshFlow::new(script));
        let store = Arc::new(
            OAuthStore::open_with_scripted_refresh_for_tests(&creds, flow.clone())
                .await
                .expect("open the scripted store"),
        );
        let mut config = routectl_router::parse_config(&rig_config_text()).expect("fixture parses");
        let usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let secrets: Arc<dyn SecretStore> = store.clone();
        let router = build_router_from_config(config.clone(), secrets)
            .await
            .expect("router builds");
        Self {
            creds,
            flow,
            store,
            config,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            health: SeatDriverHealth::default(),
            _usage_dir: usage_dir,
            _xdg: xdg,
            _dir: dir,
        }
    }

    fn job(&self) -> RefreshJob {
        RefreshJob::new(self.store.clone())
    }

    async fn sweep_with(&self, job: &impl SeatJob) {
        let router = self.swap.load_full();
        self.health.record_sweep(sweep(job, &router).await);
    }

    async fn sweep(&self) {
        self.sweep_with(&self.job()).await;
    }

    async fn reload_credentials(&self) {
        let secrets: Arc<dyn SecretStore> = self.store.clone();
        handle_credentials_reload(
            &Some(self.store.clone()),
            &self.config,
            &Arc::new(CatalogOverlay::default()),
            secrets,
            &self.swap,
            &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
        )
        .await;
    }

    fn unusable(&self, state_key: &str) -> Option<UnusableCause> {
        self.swap
            .load()
            .status_targets(Instant::now())
            .into_iter()
            .find(|target| target.state_key == state_key)
            .unwrap_or_else(|| panic!("no status target {state_key}"))
            .gate
            .credential_unusable
            .map(|mark| mark.cause)
    }

    fn seat_a_marks(&self) -> Vec<Option<UnusableCause>> {
        SEAT_A_MODELS
            .iter()
            .map(|model| self.unusable(model))
            .collect()
    }

    fn seat_a_admits(&self) -> Vec<bool> {
        SEAT_A_MODELS
            .iter()
            .map(|model| self.swap.load().admit_for_credential_tests(model).is_some())
            .collect()
    }
}

fn driver_warnings(events: &[routectl_testkit::CapturedEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
            event.level == tracing::Level::WARN && event.field("event") == Some("seat_unusable")
        })
        .count()
}

#[test]
fn the_seat_list_is_the_configured_anthropic_and_codex_logins_once_each() {
    // Arrange: seat a behind two models, a codex pool of two members, an
    // xAI and an antigravity login, an env key, a forwarded entry, and a
    // managed seat no model routes to.
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.seat_a]\nkind = \"anthropic-api\"\napi_key_ref = \"oauth://anthropic#a\"\n\
         auth_kind = \"oauth-bearer\"\n\n\
         [providers.seat_unrouted]\nkind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#unrouted\"\nauth_kind = \"oauth-bearer\"\n\n\
         [providers.codex_default]\nkind = \"openai-responses\"\nauth_kind = \"chatgpt-oauth\"\n\
         api_key_ref = \"oauth://codex\"\n\n\
         [providers.codex_b]\nkind = \"openai-responses\"\nauth_kind = \"chatgpt-oauth\"\n\
         api_key_ref = \"oauth://codex#b\"\n\n\
         [pools.codex]\nmembers = [\"codex_default\", \"codex_b\"]\n\n\
         [providers.grok]\nkind = \"openai-compat\"\nbase_url = \"https://api.x.ai/v1\"\n\
         api_key_ref = \"oauth://xai\"\n\n\
         [providers.gem]\nkind = \"gemini\"\napi_key_ref = \"oauth://antigravity\"\n\
         auth_mode = \"cloud-code\"\n\n\
         [providers.compat]\nkind = \"openai-compat\"\nbase_url = \"https://example.invalid/v1\"\n\
         api_key_ref = \"env://SEAT_DRIVER_UNUSED_KEY\"\n\n\
         [providers.fwd]\nkind = \"anthropic-api\"\ncredential_source = \"forwarded\"\n\
         api_key_ref = \"\"\n\n\
         [models.m1]\nprovider = \"seat_a\"\nupstream = \"u\"\n\
         [models.m2]\nprovider = \"seat_a\"\nupstream = \"u2\"\n\
         [models.m3]\nprovider = \"codex\"\nupstream = \"u\"\n\
         [models.m4]\nprovider = \"grok\"\nupstream = \"u\"\n\
         [models.m5]\nprovider = \"gem\"\nupstream = \"u\"\n\
         [models.m6]\nprovider = \"compat\"\nupstream = \"u\"\n\
         [models.m7]\nprovider = \"fwd\"\nupstream = \"u\"\n"
    );
    let config = routectl_router::parse_config(&text).expect("fixture parses");

    // Act
    let seats = managed_seats(&config);

    // Assert
    let listed: Vec<(&str, Option<&str>, &str)> = seats
        .iter()
        .map(|seat| {
            (
                seat.provider.as_str(),
                seat.label.as_deref(),
                seat.account.as_str(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("anthropic", Some("a"), "anthropic#a"),
            ("codex", None, "codex"),
            ("codex", Some("b"), "codex#b"),
        ]
    );
}

#[tokio::test]
#[serial_test::serial]
async fn one_sweep_refreshes_each_due_managed_seat_once_and_no_other_login() {
    // Arrange: the router builds while no login is due (building the xAI
    // entry reads its token), then a token-only reload makes every stored
    // login due: seat a, the xAI login a model uses, and an anthropic seat
    // no entry names.
    let rig = Rig::new(
        &[
            (SEAT_A, "tok-a", far_off()),
            ("anthropic#orphan", "tok-o", far_off()),
            ("xai", "tok-x", far_off()),
        ],
        ScriptedRefresh::Mint("tok-minted".into()),
    )
    .await;
    write_credentials(
        &rig.creds,
        &[
            (SEAT_A, "tok-a", due_soon()),
            ("anthropic#orphan", "tok-o", due_soon()),
            ("xai", "tok-x", due_soon()),
        ],
    );
    rig.reload_credentials().await;

    // Act
    rig.sweep().await;

    // Assert
    let tokens: Vec<(String, String)> = rig
        .store
        .list()
        .await
        .into_iter()
        .map(|(seat, record)| (seat, record.access_token.expose().to_string()))
        .collect();
    assert_eq!(rig.flow.post_count(), 1, "one refresh in total");
    assert_eq!(
        tokens,
        [
            (SEAT_A.to_string(), "tok-minted".to_string()),
            ("anthropic#orphan".to_string(), "tok-o".to_string()),
            ("xai".to_string(), "tok-x".to_string()),
        ],
        "the refresh was seat a's"
    );
    assert_eq!(rig.seat_a_marks(), [None, None]);
    let health = rig.health.snapshot();
    assert_eq!(
        (health.sweeps, health.transient, health.definitive),
        (1, 0, 0)
    );
    assert!(health.last_good_sweep_ms.is_some());
}

#[tokio::test]
#[serial_test::serial]
async fn a_refused_refresh_token_marks_the_seat_revoked_once_and_a_relogin_readmits_it() {
    // Arrange: an idle seat near expiry whose refresh token is refused.
    let rig = Rig::new(
        &[(SEAT_A, "tok-a", due_soon())],
        ScriptedRefresh::RefreshExpired,
    )
    .await;

    // Act: two sweeps, no request ever dispatched.
    let ((), events) = with_capture(async {
        rig.sweep().await;
        rig.sweep().await;
    })
    .await;

    // Assert: unusable on every model of the account, one POST, one WARN.
    let revoked = Some(UnusableCause::Revoked);
    assert_eq!(rig.seat_a_marks(), [revoked, revoked]);
    assert_eq!(rig.seat_a_admits(), [false, false]);
    assert_eq!(
        rig.flow.post_count(),
        1,
        "a revoked seat is not re-posted until a login"
    );
    assert_eq!(driver_warnings(&events), 1);
    let health = rig.health.snapshot();
    assert_eq!((health.sweeps, health.definitive), (2, 1));
    assert!(
        health.last_good_sweep_ms.is_some(),
        "the second sweep found nothing wrong"
    );

    // Act: a fresh login lands on disk and the watcher reloads it.
    write_credentials(&rig.creds, &[(SEAT_A, "tok-relogin", far_off())]);
    let before = rig.swap.load_full();
    rig.reload_credentials().await;

    // Assert: the same router serves the seat again.
    assert!(Arc::ptr_eq(&before, &rig.swap.load_full()), "no rebuild");
    assert_eq!(rig.seat_a_marks(), [None, None]);
    assert_eq!(rig.seat_a_admits(), [true, true]);
}

#[tokio::test]
#[serial_test::serial]
async fn a_seat_with_no_stored_login_is_marked_not_logged_in() {
    // Arrange
    let rig = Rig::new(&[], ScriptedRefresh::Mint("unused".into())).await;

    // Act: two sweeps; the second finds the seat still awaiting a login.
    rig.sweep().await;
    rig.sweep().await;

    // Assert
    let missing = Some(UnusableCause::NotLoggedIn);
    assert_eq!(rig.seat_a_marks(), [missing, missing]);
    assert_eq!(rig.flow.post_count(), 0);
    let health = rig.health.snapshot();
    assert_eq!((health.sweeps, health.definitive), (2, 1));
    assert!(
        health.last_good_sweep_ms.is_some(),
        "a seat awaiting login is not re-counted"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn a_failing_token_endpoint_keeps_a_valid_seat_and_marks_an_expired_one_until_a_refresh() {
    // Arrange: one 503 while the access token is still valid.
    let rig = Rig::new(
        &[(SEAT_A, "tok-a", due_soon())],
        ScriptedRefresh::TokenEndpoint(503),
    )
    .await;

    // Act
    rig.sweep().await;

    // Assert: still usable.
    assert_eq!(rig.seat_a_marks(), [None, None]);
    assert_eq!(rig.health.snapshot().transient, 1);

    // Act: the access token is now past its expiry; two failing sweeps,
    // the second inside the cooldown the first armed.
    write_credentials(&rig.creds, &[(SEAT_A, "tok-a", expired())]);
    rig.reload_credentials().await;
    let ((), events) = with_capture(async {
        rig.sweep().await;
        rig.sweep().await;
    })
    .await;

    // Assert: expired on every model of the account, one WARN.
    let gone = Some(UnusableCause::Expired);
    assert_eq!(rig.seat_a_marks(), [gone, gone]);
    assert_eq!(rig.seat_a_admits(), [false, false]);
    assert_eq!(driver_warnings(&events), 1);
    assert_eq!(rig.health.snapshot().transient, 3);

    // Act: the endpoint recovers once the cooldown has passed.
    rig.flow.set_script(ScriptedRefresh::Mint("tok-a2".into()));
    rig.store.set_cooldown_clock_for_tests(far_off());
    rig.sweep().await;

    // Assert
    assert_eq!(rig.seat_a_marks(), [None, None]);
    assert_eq!(rig.seat_a_admits(), [true, true]);
}

/// Resets the seat's account and re-marks it expired while the wrapped
/// job's refresh is in flight: a newer credential generation, then an
/// expired mark the in-flight refresh knows nothing about.
struct ResetDuringRun {
    inner: RefreshJob,
    router: Arc<Router>,
}

impl SeatJob for ResetDuringRun {
    type Outcome = RefreshOutcome;

    fn due(&self, router: &Router) -> Vec<ManagedSeat> {
        self.inner.due(router)
    }

    fn run(&self, seat: &ManagedSeat) -> impl Future<Output = RefreshOutcome> + Send {
        self.router
            .lift_oauth_credential_parks([seat.account.as_str()]);
        self.router
            .mark_credential_unusable(&seat.account, UnusableCause::Expired, None);
        self.inner.run(seat)
    }

    fn apply(
        &self,
        router: &Router,
        seat: &ManagedSeat,
        generation: u64,
        outcome: RefreshOutcome,
    ) -> SeatVerdict {
        self.inner.apply(router, seat, generation, outcome)
    }
}

#[tokio::test]
#[serial_test::serial]
async fn a_refresh_clears_an_expired_mark_only_under_the_generation_it_started_in() {
    let rows: [(&str, bool, Option<UnusableCause>); 2] = [
        ("no reset in between", false, None),
        ("reset in between", true, Some(UnusableCause::Expired)),
    ];
    for (name, reset_in_between, expected) in rows {
        // Arrange: seat a marked expired, its refresh about to succeed.
        let rig = Rig::new(
            &[(SEAT_A, "tok-a", due_soon())],
            ScriptedRefresh::Mint("tok-a2".into()),
        )
        .await;
        if !reset_in_between {
            rig.swap
                .load()
                .mark_credential_unusable(SEAT_A, UnusableCause::Expired, None);
        }

        // Act
        if reset_in_between {
            let job = ResetDuringRun {
                inner: rig.job(),
                router: rig.swap.load_full(),
            };
            rig.sweep_with(&job).await;
        } else {
            rig.sweep().await;
        }

        // Assert
        assert_eq!(rig.flow.post_count(), 1, "{name}: premise, refreshed");
        assert_eq!(rig.seat_a_marks(), [expected, expected], "{name}");
    }
}

/// A job that counts its sweeps and has nothing due.
struct CountingJob {
    sweeps: Arc<AtomicUsize>,
}

impl SeatJob for CountingJob {
    type Outcome = ();

    fn due(&self, _router: &Router) -> Vec<ManagedSeat> {
        self.sweeps.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }

    async fn run(&self, _seat: &ManagedSeat) {}

    fn apply(&self, _: &Router, _: &ManagedSeat, _: u64, (): ()) -> SeatVerdict {
        SeatVerdict::Quiet
    }
}

fn empty_router() -> Arc<ArcSwap<Router>> {
    Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(
        Config::default(),
    ))))
}

/// Let every ready task run without letting the paused clock move.
async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_long_gap_coalesces_into_one_sweep_then_the_cadence_resumes() {
    // Arrange
    let sweeps = Arc::new(AtomicUsize::new(0));
    let health = Arc::new(SeatDriverHealth::default());
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let driver = tokio::spawn(run_seat_driver(
        CountingJob {
            sweeps: sweeps.clone(),
        },
        empty_router(),
        shutdown_rx,
        health.clone(),
    ));
    settle().await;

    // Act
    tokio::time::advance(Duration::from_mins(10)).await;
    settle().await;
    let after_gap = sweeps.load(Ordering::SeqCst);
    tokio::time::advance(SEAT_DRIVER_INTERVAL).await;
    settle().await;
    let after_one_more_interval = sweeps.load(Ordering::SeqCst);

    // Assert
    assert_eq!(after_gap, 1, "ten missed intervals are one sweep");
    assert_eq!(after_one_more_interval, 2);
    assert!(health.snapshot().running);
    shutdown_tx.send(()).unwrap();
    driver.await.unwrap();
    assert!(!health.snapshot().running);
}

/// Reports when it is dropped.
struct DropSignal(Option<oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

/// A job with one due seat whose run never finishes.
struct HangingJob {
    started: Mutex<Option<oneshot::Sender<()>>>,
    dropped: Mutex<Option<DropSignal>>,
}

impl SeatJob for HangingJob {
    type Outcome = ();

    fn due(&self, _router: &Router) -> Vec<ManagedSeat> {
        vec![ManagedSeat {
            provider: "anthropic".into(),
            label: Some("a".into()),
            account: SEAT_A.into(),
        }]
    }

    fn run(&self, _seat: &ManagedSeat) -> impl Future<Output = ()> + Send {
        let started = self.started.lock().unwrap().take();
        let held = self.dropped.lock().unwrap().take();
        async move {
            let _held = held;
            if let Some(started) = started {
                let _ = started.send(());
            }
            std::future::pending::<()>().await;
        }
    }

    fn apply(&self, _: &Router, _: &ManagedSeat, _: u64, (): ()) -> SeatVerdict {
        SeatVerdict::Quiet
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_a_sweep_stops_the_driver_within_the_shutdown_bound() {
    // Arrange: a sweep in flight on a refresh that never answers.
    let (started_tx, started_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let health = Arc::new(SeatDriverHealth::default());
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let driver = tokio::spawn(run_seat_driver(
        HangingJob {
            started: Mutex::new(Some(started_tx)),
            dropped: Mutex::new(Some(DropSignal(Some(dropped_tx)))),
        },
        empty_router(),
        shutdown_rx,
        health.clone(),
    ));
    settle().await;
    tokio::time::advance(SEAT_DRIVER_INTERVAL).await;
    started_rx.await.expect("the sweep started");

    // Act
    let (joined, events) = with_capture(async {
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(
            RELOAD_TASK_SHUTDOWN_DEADLINE * 4,
            await_reload_tasks(vec![driver]),
        )
        .await
    })
    .await;

    // Assert
    joined.expect("the join returns within the bound");
    assert!(
        !events
            .iter()
            .any(|event| event.message.contains("did not stop within deadline")),
        "the driver stopped on its own, not by abort"
    );
    dropped_rx.await.expect("the in-flight refresh was dropped");
    assert!(!health.snapshot().running);
    assert_eq!(
        health.snapshot().sweeps,
        0,
        "a dropped sweep is not counted"
    );
}

#[tokio::test]
async fn the_driver_logs_one_error_only_when_it_stops_without_a_shutdown_signal() {
    let rows: [(&str, bool, usize); 2] = [
        ("shutdown signalled", true, 0),
        ("shutdown channel closed unsignalled", false, 1),
    ];
    for (name, signalled, expected_errors) in rows {
        // Arrange
        let health = Arc::new(SeatDriverHealth::default());
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let driver = tokio::spawn(run_seat_driver(
            CountingJob {
                sweeps: Arc::default(),
            },
            empty_router(),
            shutdown_rx,
            health.clone(),
        ));

        // Act
        let ((), events) = with_capture(async {
            settle().await;
            if signalled {
                shutdown_tx.send(()).unwrap();
            }
            drop(shutdown_tx);
            driver.await.unwrap();
        })
        .await;

        // Assert
        let errors = events
            .iter()
            .filter(|event| {
                event.level == tracing::Level::ERROR && event.target.ends_with("seat_driver")
            })
            .count();
        assert_eq!(errors, expected_errors, "{name}");
        assert!(!health.snapshot().running, "{name}");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn the_reload_pipeline_runs_the_driver_only_with_a_credential_store_and_joins_it() {
    let rows: [(&str, bool); 2] = [("with a credential store", true), ("without one", false)];
    for (name, with_store) in rows {
        // Arrange
        let rig = Rig::new(&[], ScriptedRefresh::Mint("unused".into())).await;
        let meta = Arc::new(crate::handlers::status::DaemonMeta::new(
            "127.0.0.1:0".into(),
        ));
        let secrets: Arc<dyn SecretStore> = rig.store.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(());

        // Act
        let handles = spawn_reload_pipeline(
            rig.config.clone(),
            Arc::new(CatalogOverlay::default()),
            None,
            with_store.then(|| rig.store.clone()),
            secrets,
            rig.swap.clone(),
            Arc::new(ArcSwap::from_pointee(ActivationState::default())),
            routectl_usage::handle_with_closed_channel(),
            shutdown_rx,
            meta.clone(),
        );
        settle().await;
        let running_while_served = meta.seat_driver_health().snapshot().running;
        shutdown_tx.send(()).unwrap();
        await_reload_tasks(handles).await;

        // Assert
        assert_eq!(running_while_served, with_store, "{name}");
        assert!(
            !meta.seat_driver_health().snapshot().running,
            "{name}: stopped once the pipeline is joined"
        );
    }
}
