use super::*;
use crate::server::build_router_from_config;
use crate::server::test_support::isolate_usage_db;

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

#[test]
fn status_requires_auth_covers_the_four_cells() {
    use std::net::SocketAddr;

    let empty = TokenSet::new(vec![]);
    let tokened = TokenSet::new(vec!["secret".to_string()]);
    let loopback: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let public: SocketAddr = "0.0.0.0:8080".parse().unwrap();

    assert!(
        !status_requires_auth(&empty, loopback),
        "token-less loopback is the open dev path"
    );
    assert!(
        status_requires_auth(&tokened, loopback),
        "configured tokens gate status even on loopback"
    );
    assert!(
        status_requires_auth(&empty, public),
        "a non-loopback bind gates status even with no tokens (fail-closed)"
    );
    assert!(
        status_requires_auth(&tokened, public),
        "tokens plus a non-loopback bind gate status"
    );
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
    kill(Pid::from_raw(std::process::id() as i32), Signal::SIGTERM).expect("kill(SIGTERM) to self");

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

/// A MITM proxy that fails to start (here: a `cert_dir` that is a
/// regular file, so directory creation fails) must NOT take the whole
/// server down -- routectl's own HTTP listener still starts and serves,
/// just without the MITM front. This pins the documented "degraded, not
/// down" reliability promise: a fatal-MITM regression would make
/// `serve_on_listener` return `Err` within milliseconds instead of
/// serving until shutdown.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn mitm_start_failure_degrades_and_server_keeps_serving() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use routectl_router::MitmConfig;
    use std::time::Duration;

    // Arrange: ephemeral loopback listener + a `[mitm]` block whose
    // `cert_dir` points at an existing regular FILE. `start_mitm_proxy`
    // fails at cert-directory creation -- a resource failure, distinct
    // from the non-loopback security invariant that hard-refuses.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let bad_cert_dir = tempfile::NamedTempFile::new().expect("temp file to stand in for cert_dir");
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    config.mitm = Some(MitmConfig {
        upstream_origin: "https://api.anthropic.com".into(),
        listen_port: 0,
        cert_dir: bad_cert_dir.path().to_path_buf(),
        mitm_host: "api.anthropic.com".into(),
        tested_cc_version: None,
    });
    let config = Arc::new(config);
    let server = tokio::spawn(async move { serve_on_listener(config, listener, None).await });

    // Assert (degradation): give serve time to reach the MITM arm and
    // fail it. The server task must still be running -- a regression that
    // made a MITM start failure fatal would have returned Err by now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !server.is_finished(),
        "a MITM start failure must not abort serve_on_listener"
    );

    // Act: graceful shutdown.
    kill(Pid::from_raw(std::process::id() as i32), Signal::SIGTERM).expect("kill(SIGTERM) to self");

    // Assert: serve returned Ok -- it served normally without the MITM
    // front and shut down cleanly.
    let outcome = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("degraded serve must return within 5s of SIGTERM")
        .expect("server task must not panic");
    assert!(
        outcome.is_ok(),
        "degraded serve (MITM failed to start) must still return Ok: {outcome:?}"
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
        resolved_class: None,
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
