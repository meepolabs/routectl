//! Handler-level integration tests for the per-source-port rate limit
//! on the OAuth loopback callback. Loaded via
//! `#[cfg(test)] #[path = "login_rate_limit_tests.rs"] mod rate_limit_tests;`
//! from `login.rs`, which keeps `super::*` resolving to the `login`
//! module (so private items like `spawn_callback_server` and
//! `CallbackResult` remain accessible).

use super::*;

/// Common test setup: bind a kernel-assigned ephemeral port on
/// 127.0.0.1, spawn the same callback sub-app `run` uses against it,
/// and return the `(base_url, expected_state, code_rx, server_handle,
/// client)` quintuple. Mirrors the setup of
/// `callback_handler_rejects_unauthenticated_hits` in `login.rs`.
async fn spawn_test_callback_server() -> (
    String,
    &'static str,
    oneshot::Receiver<CallbackResult>,
    tokio::task::JoinHandle<()>,
    reqwest::Client,
) {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    let flow = providers::lookup("anthropic").expect("known provider");
    let expected_state = "expected-csrf-state-token";
    let (code_rx, server_handle) =
        spawn_callback_server(flow, listener, expected_state.to_string());
    let base = format!("http://127.0.0.1:{port}{}", flow.callback_path());
    // Relies on reqwest+hyper default connection reuse keeping a single
    // source port across the 40 sequential hits. If that assumption ever
    // breaks (e.g. axum middleware emits Connection: close on 400s), this
    // test fails noisily via assert!(got_429, ...) rather than silently.
    (
        base,
        expected_state,
        code_rx,
        server_handle,
        reqwest::Client::new(),
    )
}

/// Spam the handler from a single client (one source port) past the
/// production threshold and assert at least some requests are
/// escalated from 400 to 429. With production knobs (30 / 10s),
/// 40 sequential rejected hits within ~100ms guarantees the trip.
#[tokio::test]
async fn callback_handler_returns_429_after_threshold_per_port() {
    // Arrange.
    let (base, _expected_state, mut code_rx, server_handle, client) =
        spawn_test_callback_server().await;

    // Act: fire well above the 30-rejection threshold; a real attacker
    // could go much faster, but 40 is enough to land in the
    // post-threshold band.
    let mut got_429 = false;
    for _ in 0..40 {
        let resp = client
            .get(format!("{base}?error=noise"))
            .send()
            .await
            .expect("noise hit completes");
        let code = resp.status().as_u16();
        assert!(
            matches!(code, 400 | 429),
            "rejected hit must be 400 or 429, got {code}"
        );
        if code == 429 {
            got_429 = true;
        }
    }

    // Assert: at least some hits hit the 429 escalation, and the
    // wait-future is still pending (the rate limit must not abort
    // the legit browser flow as a side effect).
    assert!(
        got_429,
        "spam from a single source port must trigger 429 within the production window"
    );
    match code_rx.try_recv() {
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            panic!("rate-limited spam closed the callback channel");
        }
        Ok(cb) => panic!("rate-limited spam delivered a result: {cb:?}"),
    }

    // Reap the listener; no shutdown one-shot fired (rejected hits
    // never trigger it), so abort is fine.
    server_handle.abort();
}

/// Regression: the rate limit fires only on REJECTED hits, so a
/// state-valid callback must succeed even while the same source port is
/// still rate-limited on noise. This guarantees the legitimate browser
/// flow is not collateral damage.
#[tokio::test]
async fn callback_handler_valid_state_callback_bypasses_rate_limit_unconditionally() {
    // Arrange.
    let (base, expected_state, code_rx, server_handle, client) = spawn_test_callback_server().await;

    // Act 1: drive the same source port past the 429 threshold.
    for _ in 0..40 {
        let _ = client.get(format!("{base}?error=noise")).send().await;
    }

    // Act 2: send a state-valid callback from the SAME client (same
    // source port). State-valid hits skip the rate-limit branch
    // entirely, so this must still resolve even while the same source
    // port is still rate-limited on noise.
    let resp = client
        .get(format!("{base}?code=GOODCODE&state={expected_state}"))
        .send()
        .await
        .expect("legit hit completes");

    // Assert: the legit hit gets 200 and the wait resolves with the
    // expected code -- the rate limit did not break the flow.
    assert_eq!(
        resp.status().as_u16(),
        200,
        "state-valid callback must bypass the rate limit"
    );
    let cb = tokio::time::timeout(Duration::from_secs(2), code_rx)
        .await
        .expect("wait must resolve once state matches")
        .expect("callback channel must not drop");
    assert_eq!(
        cb.code.expect("legit hit yields a code"),
        "GOODCODE",
        "state-valid callback delivered the wrong code"
    );

    // Reap the listener; the legit hit fired the shutdown one-shot.
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}
