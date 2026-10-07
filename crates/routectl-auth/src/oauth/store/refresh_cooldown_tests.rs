use super::*;
use crate::oauth::store::test_support::*;

#[tokio::test]
async fn transient_failure_enters_cooldown_second_call_skips_flow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Transient));
    let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
    store.set_test_now(1_000);

    // First get: the flow fires once and fails transiently, arming
    // the per-seat cooldown (5s base).
    let first = store.get(&anthropic_ref()).await;
    assert!(first.is_err(), "transient refresh failure must surface");
    assert_eq!(flow.call_count(), 1, "first wave POSTs exactly once");

    // Second get inside the cooldown window: must fail fast WITHOUT a
    // second POST. The flow count stays 1.
    let second = store.get(&anthropic_ref()).await;
    let err = second.expect_err("cooldown must fail fast");
    assert!(
        err.to_string().contains("temporarily unavailable"),
        "suppressed error must be the retryable cooldown message: {err}"
    );
    assert_eq!(
        flow.call_count(),
        1,
        "second call within cooldown must not invoke the flow"
    );
}

#[tokio::test]
async fn cooldown_expiry_allows_exactly_one_retry_under_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Transient).with_yield());
    let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
    store.set_test_now(1_000);

    // Arm the cooldown: one failed POST -> next_allowed = 1005.
    let _ = store.get(&anthropic_ref()).await;
    assert_eq!(flow.call_count(), 1);
    let (consecutive, next_allowed, _) = store.cooldown_snapshot("anthropic").unwrap();
    assert_eq!((consecutive, next_allowed), (1, 1_005));

    // Advance to the boundary (window elapsed) and fire two concurrent
    // callers. The per-seat single-flight lets exactly one through the
    // POST; the other parks on the lock, re-double-checks, and is then
    // suppressed by the freshly re-armed cooldown. Net: +1 POST only.
    store.set_test_now(1_005);
    let ref_a = anthropic_ref();
    let ref_b = anthropic_ref();
    let (a, b) = tokio::join!(store.get(&ref_a), store.get(&ref_b));
    assert!(a.is_err() && b.is_err());
    assert_eq!(
        flow.call_count(),
        2,
        "exactly one retry POST fires past the cooldown window"
    );
    // The retry re-armed the cooldown at the next exponential step
    // (consecutive 2 -> 10s window).
    let (consecutive, next_allowed, _) = store.cooldown_snapshot("anthropic").unwrap();
    assert_eq!((consecutive, next_allowed), (2, 1_015));
}

#[tokio::test]
async fn success_clears_cooldown_and_resets_consecutive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Transient));
    let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
    store.set_test_now(1_000);

    // Fail once to arm the cooldown.
    let _ = store.get(&anthropic_ref()).await;
    assert!(store.cooldown_snapshot("anthropic").is_some());

    // Recover: advance past the window, flip the flow to success.
    store.set_test_now(1_005);
    flow.set_outcome(RefreshOutcome::Mint("tok-ok".into()));
    let tok = store.get(&anthropic_ref()).await.unwrap();
    assert_eq!(tok, "tok-ok");
    assert_eq!(flow.call_count(), 2);
    assert!(
        store.cooldown_snapshot("anthropic").is_none(),
        "a successful refresh must clear the seat's cooldown"
    );

    // A subsequent transient failure re-enters at the 5s base, proving
    // consecutive reset to zero (not carried over from before).
    store.set_test_now(2_000);
    store.record_transient_failure("anthropic", "anthropic", &OAuthError::Network("x".into()));
    let (consecutive, next_allowed, _) = store.cooldown_snapshot("anthropic").unwrap();
    assert_eq!(
        (consecutive, next_allowed),
        (1, 2_005),
        "post-recovery backoff restarts at the 5s base"
    );
}

#[tokio::test]
async fn refresh_expired_never_enters_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let flow = Arc::new(CountingFlow::new(RefreshOutcome::RefreshExpired));
    let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
    store.set_test_now(1_000);

    // A terminal RefreshExpired must not arm the cooldown, so both
    // calls attempt a POST (two attempts, no suppression).
    let first = store.get(&anthropic_ref()).await;
    let second = store.get(&anthropic_ref()).await;
    assert!(first.is_err() && second.is_err());
    assert_eq!(
        flow.call_count(),
        2,
        "RefreshExpired must never be suppressed by a cooldown"
    );
    assert!(
        store.cooldown_snapshot("anthropic").is_none(),
        "RefreshExpired must never enter the cooldown"
    );
}

#[tokio::test]
async fn reset_triggers_clear_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    // A plain store is enough; the cooldown is armed directly.
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    drop(seed);
    let store = OAuthStore::open(&path).await.unwrap();
    store.set_test_now(1_000);
    let arm = |s: &OAuthStore| {
        s.record_transient_failure("anthropic", "anthropic", &OAuthError::Network("x".into()));
    };

    // reload_from_disk clears the WHOLE map.
    arm(&store);
    assert!(store.cooldown_snapshot("anthropic").is_some());
    store.reload_from_disk().await.unwrap();
    assert!(
        store.cooldown_snapshot("anthropic").is_none(),
        "reload_from_disk must clear the cooldown map"
    );

    // write_record clears the seat.
    arm(&store);
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    assert!(
        store.cooldown_snapshot("anthropic").is_none(),
        "write_record must clear the seat's cooldown"
    );

    // remove_provider clears the seat.
    arm(&store);
    store.remove_provider("anthropic").await.unwrap();
    assert!(
        store.cooldown_snapshot("anthropic").is_none(),
        "remove_provider must clear the seat's cooldown"
    );
}

#[tokio::test]
async fn cli_force_refresh_bypasses_active_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Transient));
    let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
    store.set_test_now(1_000);

    // Arm the cooldown via a request-time refresh.
    let _ = store.get(&anthropic_ref()).await;
    assert_eq!(flow.call_count(), 1);

    // A request-time get() inside the window is suppressed (no POST).
    let _ = store.get(&anthropic_ref()).await;
    assert_eq!(flow.call_count(), 1, "request-time path stays suppressed");

    // The CLI force-refresh escape hatch POSTs despite the cooldown.
    let forced = store.force_refresh("anthropic", None).await;
    assert!(forced.is_err(), "the forced POST still failed transiently");
    assert_eq!(
        flow.call_count(),
        2,
        "CLI force-refresh must bypass the cooldown and attempt the POST"
    );

    // The forced call's transient outcome must still re-arm the
    // cooldown for the request-time paths: consecutive advances 1 -> 2
    // (10s window) at the pinned clock (1_000 + 10 = 1_010).
    let (consecutive, next_allowed, _) = store.cooldown_snapshot("anthropic").unwrap();
    assert_eq!(
        (consecutive, next_allowed),
        (2, 1_010),
        "the bypassed force-refresh still records its transient outcome"
    );
}

#[test]
fn transient_classifier_matches_decision_taxonomy() {
    // Network -> transient.
    assert!(is_transient_refresh_error(&OAuthError::Network(
        "reset".into()
    )));
    // TokenEndpoint 429 / 5xx -> transient.
    assert!(is_transient_refresh_error(&OAuthError::TokenEndpoint(
        "429 https://idp.example/token".into()
    )));
    assert!(is_transient_refresh_error(&OAuthError::TokenEndpoint(
        "503 https://idp.example/token".into()
    )));
    // TokenEndpoint 4xx (bad request / dead grant) -> terminal.
    for code in ["400", "401", "403"] {
        assert!(
            !is_transient_refresh_error(&OAuthError::TokenEndpoint(format!(
                "{code} https://idp.example/token"
            ))),
            "{code} must be terminal"
        );
    }
    // Unparseable TokenEndpoint body -> transient (outage-like).
    assert!(is_transient_refresh_error(&OAuthError::TokenEndpoint(
        "token response is not valid UTF-8".into()
    )));
    // RefreshExpired and other variants -> terminal.
    assert!(!is_transient_refresh_error(&OAuthError::RefreshExpired(
        "anthropic".into()
    )));
    assert!(!is_transient_refresh_error(&OAuthError::NotLoggedIn(
        "anthropic".into()
    )));
}

#[test]
fn cooldown_reason_is_class_only_and_drops_urls() {
    // TokenEndpoint "{status} {url}" -> class + status, no URL.
    assert_eq!(
        cooldown_reason(&OAuthError::TokenEndpoint(
            "503 https://console.anthropic.com/v1/oauth/token".into()
        )),
        "token_endpoint 503"
    );
    // TokenEndpoint with no parseable leading status -> bare class.
    assert_eq!(
        cooldown_reason(&OAuthError::TokenEndpoint(
            "token response is not valid UTF-8".into()
        )),
        "token_endpoint"
    );
    // Network errors carry no endpoint detail worth retaining.
    assert_eq!(
        cooldown_reason(&OAuthError::Network(
            "connection reset by peer to https://idp.example/token".into()
        )),
        "network"
    );
}

#[tokio::test]
async fn cooldown_observability_contract() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store.set_test_now(1_000);
    // Provider refresh errors format as "{status} {url}"; the retained
    // reason and the log field must reduce that to a class-only label
    // with no URL.
    let url = "https://console.anthropic.com/v1/oauth/token";
    let boom = || OAuthError::TokenEndpoint(format!("503 {url}"));

    // Drive the observability surface synchronously through the
    // private state transitions so the captured subscriber sees every
    // event on this thread: one entry, two suppressed attempts, one
    // extension, then recovery.
    let events = routectl_testkit::capture_events(|| {
        store.record_transient_failure("anthropic", "anthropic", &boom());
        assert!(store.cooldown_remaining("anthropic").is_some());
        assert!(store.cooldown_remaining("anthropic").is_some());
        store.record_transient_failure("anthropic", "anthropic", &boom());
        store.clear_cooldown_on_success("anthropic", "anthropic");
    });

    let entered: Vec<_> = events
        .iter()
        .filter(|e| e.message == "oauth_refresh_cooldown_entered")
        .collect();
    assert_eq!(
        entered.len(),
        2,
        "WARN fires once per entry/extension, never per suppressed attempt"
    );
    for e in &entered {
        assert_eq!(e.level, tracing::Level::WARN);
        assert_eq!(e.field("provider"), Some("anthropic"));
        assert_eq!(e.field("seat"), Some("anthropic"));
        assert_eq!(e.field("failure_class"), Some("token_endpoint"));
        assert!(e.field("consecutive_failures").is_some());
        assert!(e.field("cooldown_ms").is_some());
        // Class-only reason: the leading status survives, the URL never
        // reaches the log field.
        assert_eq!(e.field("reason"), Some("token_endpoint 503"));
        assert!(
            !e.field("reason").unwrap().contains(url),
            "cooldown reason must not carry the token-endpoint URL"
        );
    }
    // Entry then extension: 5s then 10s windows.
    assert_eq!(entered[0].field("cooldown_ms"), Some("5000"));
    assert_eq!(entered[1].field("cooldown_ms"), Some("10000"));

    let recovered: Vec<_> = events
        .iter()
        .filter(|e| e.message == "oauth_refresh_recovered")
        .collect();
    assert_eq!(recovered.len(), 1, "recovery INFO fires exactly once");
    assert_eq!(recovered[0].level, tracing::Level::INFO);
    assert_eq!(
        recovered[0].field("suppressed_attempts"),
        Some("2"),
        "recovery reports the accumulated suppressed count"
    );
    assert_eq!(recovered[0].field("consecutive_failures"), Some("2"));
}
