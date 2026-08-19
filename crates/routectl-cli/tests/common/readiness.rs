//! Readiness polling for integration tests that boot a real server.
//!
//! Readiness is always a signal, never a sleep: `serve_on_listener`
//! receives an ALREADY-bound listener, so a bare TCP connect succeeds
//! from the OS backlog well before the router can answer. A fixed wait is
//! therefore both a latent flake (slow boot on a loaded box) and a
//! false-green risk (the wait elapses before the surface is reached at
//! all). Polling the live endpoint against a deadline is the only shape
//! that proves the server is serving.
//!
//! One deadline serves every call site. It is a give-up bound, not a
//! wait: a healthy server answers in milliseconds, so a generous bound
//! costs nothing on the green path while keeping a genuinely dead front
//! from stalling a shard.

#![allow(dead_code)]

use std::time::Duration;

use tokio::time::Instant;

/// How long a readiness poll keeps trying before declaring the surface
/// broken rather than slow. Generous enough to absorb a cold, loaded,
/// fully parallel suite; short enough that a dead front fails the test
/// instead of stalling the shard.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);

/// Pause between readiness attempts. A poll cadence, not a readiness
/// wait -- readiness is always the successful response itself.
pub const POLL_CADENCE: Duration = Duration::from_millis(20);

/// Poll `GET {base_url}/health` until it returns success or
/// [`READY_DEADLINE`] elapses, panicking on the deadline.
///
/// Every attempt (and the cadence pause after it) is bounded by the
/// REMAINING deadline, not just checked between attempts: a server that
/// completes the TCP accept but never sends response headers would
/// otherwise park `send()` on reqwest's own (default: none) timeout and
/// stall the shard far past the deadline.
pub async fn await_health(base_url: &str) {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + READY_DEADLINE;
    while Instant::now() < deadline {
        let attempt = client.get(format!("{base_url}/health")).send();
        if let Ok(Ok(response)) = tokio::time::timeout_at(deadline, attempt).await
            && response.status().is_success()
        {
            return;
        }
        sleep_until_cadence_or_deadline(deadline).await;
    }
    panic!("the test server did not become healthy at {base_url}");
}

/// Wait one poll cadence, but never past `deadline` -- so the pause
/// between attempts cannot itself push a poll loop over its bound.
pub async fn sleep_until_cadence_or_deadline(deadline: Instant) {
    let wake = (Instant::now() + POLL_CADENCE).min(deadline);
    tokio::time::sleep_until(wake).await;
}
