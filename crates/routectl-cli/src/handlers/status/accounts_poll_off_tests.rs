//! With `[usage] poll = false` every managed login reads `off` under its
//! vendor: a poll state of its own, never `polled`, `awaiting_login` or
//! `not_polled`, and with no sweep state.

use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_router::Router;
use routectl_router::runtime_state::UnusableCause;
use serde_json::Value;

use super::tests::{every_kind, read_accounts, router_of, row, state_with_running_poll};

/// The fixture router with `[usage] poll` set to `poll`.
fn router_with_poll(poll: bool) -> Router {
    let mut router = router_of(&every_kind());
    let mut config = (*router.config).clone();
    config.usage.poll = poll;
    router.config = Arc::new(config);
    router
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_the_poll_off_each_managed_login_reads_off_under_its_vendor() {
    // (row, `[usage] poll`, account, applicability, vendor)
    let rows = [
        (
            "off",
            false,
            "anthropic#work",
            "off",
            Value::from("anthropic"),
        ),
        ("off", false, "codex", "off", Value::from("codex")),
        ("off", false, "#gamma", "not_polled", Value::Null),
        ("off", false, "#bedrock-west", "not_polled", Value::Null),
        (
            "on",
            true,
            "anthropic#work",
            "polled",
            Value::from("anthropic"),
        ),
        ("on", true, "codex", "polled", Value::from("codex")),
    ];
    for (label, poll, account, applicability, vendor) in rows {
        // Arrange
        let swap = Arc::new(ArcSwap::from_pointee(router_with_poll(poll)));
        let state = state_with_running_poll(swap).await;

        // Act
        let json = read_accounts(&state).await;

        // Assert
        let poll_row = &row(&json, account)["poll"];
        assert_eq!(
            poll_row["applicability"], applicability,
            "{label} {account}"
        );
        assert_eq!(poll_row["vendor"], vendor, "{label} {account}");
        assert!(poll_row["next_due_ms"].is_null(), "{label} {account}");
        assert!(poll_row["failure_since_ms"].is_null(), "{label} {account}");
    }
}

/// A login awaiting a fresh login is not raised as needing one while the
/// poll is off: the poll would not read it either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_the_poll_off_a_login_awaiting_login_reads_off() {
    // Arrange
    let swap = Arc::new(ArcSwap::from_pointee(router_with_poll(false)));
    let state = state_with_running_poll(Arc::clone(&swap)).await;
    assert!(
        swap.load()
            .mark_credential_unusable_for_tests("m-b", UnusableCause::Revoked),
        "premise: marked"
    );

    // Act
    let json = read_accounts(&state).await;

    // Assert
    assert_eq!(row(&json, "anthropic#work")["poll"]["applicability"], "off");
}
