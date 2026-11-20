//! `/status/accounts` per-row pools, subscription flag and credential state,
//! and the refresh driver row beside the poll driver row.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use routectl_core::UpstreamReason;
use routectl_router::Billing;
use routectl_router::runtime_state::UnusableCause;
use serde_json::{Value, json};

use super::SCHEMA_VERSION;
use super::tests::{every_kind, read_accounts, router_of, router_with, row, state_over};
use crate::commands::doctor::attention::{
    AccountSnapshot, AttentionInputs, AttentionKind, active_items,
};
use crate::handlers::status::DaemonMeta;
use crate::handlers::status::router_view::StatusRouterHandle;

/// `seat-z` (login `anthropic#work`) in pool `duo`, `codex-one` in pool
/// `solo`; the API-key and cloud entries in no pool.
const POOLS: &str =
    "[pools.duo]\nmembers = [\"seat-z\"]\n[pools.solo]\nmembers = [\"codex-one\"]\n";

/// A logged-out seat's credential on the accounts panel carries the same fix
/// line and the same plain cause as the needs-attention item built for that
/// account from the same router.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_logged_out_seats_credential_fix_and_cause_equal_its_attention_item() {
    // Arrange
    let swap = Arc::new(ArcSwap::from_pointee(router_of(&every_kind())));
    assert!(
        swap.load()
            .mark_credential_unusable_for_tests("m-b", UnusableCause::NotLoggedIn)
    );
    let state = state_over(Arc::clone(&swap), DaemonMeta::for_test());
    let now_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    let credential = StatusRouterHandle::new(Arc::clone(&swap))
        .view()
        .account_credentials((Instant::now(), now_ms))
        .remove("anthropic#work")
        .expect("the seat is built");
    let inputs = AttentionInputs {
        now_ms,
        accounts: vec![AccountSnapshot {
            account: "anthropic#work".to_string(),
            display: "seat-b (work)".to_string(),
            credential: credential.condition,
            fix: credential.fix.clone(),
            remediation: None,
            refresh_failure: None,
            poll_failure: None,
            windows: Vec::new(),
            billing: Billing::Unknown,
            billing_fresh: false,
        }],
        ..AttentionInputs::default()
    };
    let items = active_items(&inputs);
    let item = items
        .iter()
        .find(|item| item.kind == AttentionKind::AccountUnusable)
        .expect("a logged-out account is listed");

    // Act
    let json = read_accounts(&state).await;

    // Assert
    let row_credential = &row(&json, "anthropic#work")["credential"];
    assert_eq!(row_credential["state"], "unusable", "{row_credential}");
    assert_eq!(
        row_credential["fix"],
        json!(item.fix),
        "the panel and the attention item give one fix"
    );
    assert_eq!(
        row_credential["fix"], "routectl login anthropic --label work",
        "the fix is the bare login line"
    );
    assert_eq!(
        row_credential["cause"],
        json!(item.cause),
        "{row_credential}"
    );
    assert_eq!(row_credential["cause"], "Not logged in, for under 1 min");
}

/// Every row carries its pools, its subscription flag and its credential:
/// an oauth seat (unusable, pooled, subscription), a parked oauth login
/// (pooled, subscription), an API-key entry and a cloud entry (usable, no
/// pool, not a subscription). No row carries a `placement` key and the
/// panel stays at schema 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pools_subscription_and_credential_serialize_for_each_credential_kind() {
    // Arrange
    let swap = Arc::new(ArcSwap::from_pointee(router_with(&every_kind(), POOLS)));
    let router = swap.load();
    assert!(router.mark_credential_unusable_for_tests("m-b", UnusableCause::Revoked));
    assert!(router.park_credential_for_tests(
        "m-codex",
        UpstreamReason::CredentialInvalid,
        Instant::now() + Duration::from_mins(10),
    ));
    let state = state_over(Arc::clone(&swap), DaemonMeta::for_test());

    // Act
    let json = read_accounts(&state).await;

    // Assert
    assert_eq!(json["schema_version"], 1);
    assert_eq!(SCHEMA_VERSION, 1);
    let seat = row(&json, "anthropic#work");
    assert_eq!(seat["pools"], json!(["duo"]), "{seat}");
    assert_eq!(seat["subscription"], true, "{seat}");
    assert_eq!(seat["credential"]["state"], "unusable", "{seat}");
    assert_eq!(
        seat["credential"]["cause"], "Login revoked by the provider, for under 1 min",
        "{seat}"
    );
    assert!(seat["credential"]["since_ms"].is_i64(), "{seat}");
    assert!(seat["credential"]["until_ms"].is_null(), "{seat}");

    let codex = row(&json, "codex");
    assert_eq!(codex["pools"], json!(["solo"]), "{codex}");
    assert_eq!(codex["subscription"], true, "{codex}");
    assert_eq!(codex["credential"]["state"], "parked", "{codex}");
    let cause = codex["credential"]["cause"].as_str().expect("a cause");
    assert!(
        cause.starts_with("Credential refused by the provider, retried in "),
        "{cause}"
    );
    assert!(codex["credential"]["since_ms"].is_null(), "{codex}");
    assert!(codex["credential"]["until_ms"].is_i64(), "{codex}");
    assert!(codex["credential"]["fix"].is_null(), "{codex}");

    let usable = json!({
        "state": "usable", "cause": null, "since_ms": null, "until_ms": null, "fix": null,
    });
    for account in ["#gamma", "#bedrock-west"] {
        let entry = row(&json, account);
        assert_eq!(entry["pools"], json!([]), "{account}");
        assert_eq!(entry["subscription"], false, "{account}");
        assert_eq!(entry["credential"], usable, "{account}");
    }

    let rows = json["data"]["accounts"].as_array().expect("rows");
    assert!(rows.iter().all(|row| row.get("credential").is_some()));
    let with_placement: Vec<&Value> = rows
        .iter()
        .filter(|row| row.get("placement").is_some())
        .collect();
    assert!(with_placement.is_empty(), "{with_placement:?}");
}

/// The refresh driver row and the poll driver row are read from their own
/// healths: each carries its own distinct counters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_driver_and_poll_driver_are_separate_rows_from_separate_healths() {
    // Arrange
    let meta = DaemonMeta::for_test();
    meta.seat_driver_health().record_two_sweeps_for_tests(1, 2);
    meta.usage_poll_driver_health()
        .record_two_sweeps_for_tests(3, 4);
    let state = state_over(
        Arc::new(ArcSwap::from_pointee(router_of(&every_kind()))),
        Arc::clone(&meta),
    );

    // Act
    let json = read_accounts(&state).await;

    // Assert
    let refresh = &json["data"]["refresh_driver"];
    let poll = &json["data"]["poll_driver"];
    assert_eq!(refresh["running"], true, "{refresh}");
    assert_eq!(refresh["sweeps"], 2, "{refresh}");
    assert_eq!(refresh["transient"], 1, "{refresh}");
    assert_eq!(refresh["definitive"], 2, "{refresh}");
    assert_eq!(poll["transient"], 3, "{poll}");
    assert_eq!(poll["definitive"], 4, "{poll}");
    let keys = |row: &Value| -> Vec<String> {
        row.as_object()
            .expect("a driver row")
            .keys()
            .cloned()
            .collect()
    };
    assert_eq!(keys(refresh), keys(poll), "both rows share one key set");
}
