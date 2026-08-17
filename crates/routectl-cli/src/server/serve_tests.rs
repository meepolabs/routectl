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

    // Let the server install its signal handler and pass the MITM arm.
    // The degradation itself is proven by the final assertion: a
    // regression that made a MITM start failure fatal resolves the task
    // with Err, which the Ok check below rejects.
    tokio::time::sleep(Duration::from_millis(100)).await;

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

// ---- Route auth inventory: every served path is explicitly classified ----

/// Recursively collect every `.rs` file under `dir` that is NOT a test
/// sidecar. Sidecars (`*_tests.rs`, `tests.rs`) may register throwaway
/// routes for their own fixtures, which are not part of the served
/// surface.
#[cfg(test)]
fn production_rust_sources(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read crate source dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if name == "tests.rs" || name.ends_with("_tests.rs") {
                continue;
            }
            found.push(path);
        }
    }
    found
}

/// The crate's own `src/` tree, resolved from the compile-time manifest
/// dir so the scan is independent of the test process's cwd.
#[cfg(test)]
fn crate_src_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Strip the inline `mod tests { .. }` tail of a source file so routes
/// registered by a test fixture are not mistaken for served ones. Keyed on
/// the module opener rather than `#[cfg(test)]`, which also decorates
/// test-only items (the inventory consts themselves) that sit ABOVE real
/// route registrations.
#[cfg(test)]
fn production_prefix(src: &str) -> &str {
    match src.find("mod tests {") {
        Some(idx) => &src[..idx],
        None => src,
    }
}

/// Drop `//`-introduced tails (line comments AND doc comments) so prose
/// that merely NAMES a route-registration call is not scanned as one.
#[cfg(test)]
fn without_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every string-literal path passed to a route registration in `src`'s
/// production prefix. Panics if a registration's first argument is NOT a
/// literal: a computed path would silently evade the inventory.
#[cfg(test)]
fn registered_paths_in(src: &str, label: &str) -> Vec<String> {
    let needle = ".route(";
    let mut paths = Vec::new();
    // Comments are stripped BEFORE truncating, not after: a `//` comment
    // containing the literal `mod tests {` would otherwise truncate the
    // scan at the comment, and any route registered BELOW it would vanish
    // from `served` while the already-classified routes above stayed
    // visible -- leaving BOTH difference assertions empty so the guard
    // passes vacuously and the route ships unauthenticated.
    let scannable = production_prefix(&without_comments(src)).to_string();
    let mut rest = scannable.as_str();
    while let Some(idx) = rest.find(needle) {
        rest = &rest[idx + needle.len()..];
        let arg = rest.trim_start();
        let literal = arg.strip_prefix('"').unwrap_or_else(|| {
            panic!(
                "{label}: route registered with a non-literal path -- the route auth \
                 inventory can only classify literal paths; register it with a \
                 literal or extend the inventory scan"
            )
        });
        let end = literal
            .find('"')
            .expect("unterminated route path string literal");
        paths.push(literal[..end].to_string());
    }
    paths
}

/// A route added to the serve router without being classified as either
/// public or auth-gated must FAIL here rather than ship unauthenticated.
///
/// The scan reads the crate's production sources (test sidecars and inline
/// `mod tests` tails excluded) for every registered path literal and
/// compares that set against the declared inventory. Adding a `.route()`
/// line anywhere in the crate -- on the `public` builder, the `authed`
/// builder, the status subtree, or a brand-new module -- fails this test
/// until its author names the path in `PUBLIC_ROUTES` or
/// `AUTH_GATED_ROUTES`. The reverse direction (a declared path that is no
/// longer served) fails too, so the inventory cannot go stale.
#[test]
fn every_registered_route_is_classified_public_or_auth_gated() {
    // Arrange
    let mut served: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for path in production_rust_sources(&crate_src_dir()) {
        let src = std::fs::read_to_string(&path).expect("read crate source file");
        let label = path.display().to_string();
        served.extend(registered_paths_in(&src, &label));
    }
    let declared: std::collections::BTreeSet<String> = PUBLIC_ROUTES
        .iter()
        .chain(AUTH_GATED_ROUTES.iter())
        .map(|p| (*p).to_string())
        .collect();

    // Assert: neither direction may drift.
    let unclassified: Vec<&String> = served.difference(&declared).collect();
    assert!(
        unclassified.is_empty(),
        "route(s) registered but not classified: {unclassified:?} -- add each to \
         PUBLIC_ROUTES (with the reason it is auth-exempt) or AUTH_GATED_ROUTES"
    );
    let unserved: Vec<&String> = declared.difference(&served).collect();
    assert!(
        unserved.is_empty(),
        "route(s) declared in the auth inventory but no longer registered: \
         {unserved:?} -- drop them from the inventory"
    );

    // A path may not be claimed by both lists at once.
    for path in PUBLIC_ROUTES {
        assert!(
            !AUTH_GATED_ROUTES.contains(path),
            "{path} is declared both public and auth-gated"
        );
    }
}

/// The serve router must never widen its unclassified surface through a
/// nested/fallback/service route, which registers paths the literal scan
/// above cannot see.
#[test]
fn production_sources_register_no_opaque_route_surfaces() {
    // `.fallback_service(` and `.method_not_allowed_fallback(` are axum 0.8
    // catch-all registrars that do NOT contain `.fallback(` as a substring,
    // so each needs its own entry: either one mounts a surface the literal
    // `.route(` scan cannot enumerate.
    let opaque = [
        ".nest(",
        ".nest_service(",
        ".route_service(",
        ".fallback(",
        ".fallback_service(",
        ".method_not_allowed_fallback(",
    ];
    for path in production_rust_sources(&crate_src_dir()) {
        let src = std::fs::read_to_string(&path).expect("read crate source file");
        let production = production_prefix(&without_comments(&src)).to_string();
        for token in opaque {
            assert!(
                !production.contains(token),
                "`{token}` in {} registers paths the route auth inventory cannot \
                 enumerate; classify the surface explicitly before using it",
                path.display()
            );
        }
    }
}

/// Behavioral half of the inventory: with tokens configured, drive a real
/// request at every declared path through the REAL `build_axum_router`
/// output and assert the auth layer actually challenges each auth-gated
/// path while each public path is served. This is what makes the declared
/// lists evidence rather than a restatement -- a path listed in
/// `AUTH_GATED_ROUTES` that is wired onto the `public` builder fails here.
#[tokio::test]
async fn declared_auth_gated_routes_challenge_unauthenticated_requests() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    // Arrange: the production router with a non-empty token set on a
    // loopback bind (so `status_requires_auth` holds too).
    let router = routectl_router::Router::new(Arc::new(Config::default()));
    let (state, _usage_dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let bound: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let token_set = Arc::new(TokenSet::new(vec!["secret".to_string()]));
    let app = build_axum_router(
        state,
        token_set,
        1024,
        None,
        bound,
        crate::handlers::status::DaemonMeta::for_test(),
    );

    // Act + Assert: no credential -> 401 on every auth-gated path.
    for path in AUTH_GATED_ROUTES {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(*path)
                    .header("host", "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router must respond");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{path} is declared auth-gated but served an unauthenticated request"
        );
    }

    // And the declared-public paths are served without a credential.
    for path in PUBLIC_ROUTES {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(*path)
                    .header("host", "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router must respond");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{path} is declared public (liveness probes on --unsafe-public \
             deployments depend on it) but did not serve"
        );
    }
}

/// The token-less loopback dev path is unchanged: with no configured
/// tokens on a loopback bind, an auth-gated path is served without a
/// credential (the auth layer is never mounted, so there is no
/// per-request cost either).
#[tokio::test]
async fn token_less_loopback_serves_auth_gated_routes_without_credentials() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    // Arrange
    let router = routectl_router::Router::new(Arc::new(Config::default()));
    let (state, _usage_dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let bound: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let app = build_axum_router(
        state,
        Arc::new(TokenSet::default()),
        1024,
        None,
        bound,
        crate::handlers::status::DaemonMeta::for_test(),
    );

    // Act
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .header("host", "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router must respond");

    // Assert
    assert_ne!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "token-less loopback must keep the zero-auth dev path"
    );
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
        calib_estimated_tokens: None,
        calib_prompt_tokens: None,
        reduction_decision: None,
        reduction_strings_compressed: None,
        reduction_strings_skipped: None,
        reduction_strings_rejected: None,
        reduction_bytes_saved: None,
        cache_front_decision: None,
        cache_terminal_decision: None,
        prefix_epoch_event: None,
    }
}
