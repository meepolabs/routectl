use super::*;
use crate::oauth::store::scripted::{ScriptedRefreshFlow, token_record};
use crate::oauth::store::test_support::*;

const SEAT_A: &str = "anthropic#seat-a";

/// Seed `seat` on disk with an access token expiring at `expires_at`, then
/// open a store whose refresh POSTs are answered by `script`.
async fn scripted_store_with_seat(
    path: &std::path::Path,
    seat: &str,
    expires_at: u64,
    script: ScriptedRefresh,
) -> (OAuthStore, Arc<ScriptedRefreshFlow>) {
    let seed = OAuthStore::open(path).await.unwrap();
    seed.write_record(seat, token_record("tok-stale", expires_at))
        .await
        .unwrap();
    drop(seed);
    let flow = Arc::new(ScriptedRefreshFlow::new(script));
    let store = OAuthStore::open_with_scripted_refresh_for_tests(path, flow.clone())
        .await
        .unwrap();
    (store, flow)
}

async fn disk_access_token(path: &std::path::Path, seat: &str) -> Option<String> {
    let reopened = OAuthStore::open(path).await.unwrap();
    let on_disk: BTreeMap<String, TokenRecord> = reopened.list().await.into_iter().collect();
    on_disk
        .get(seat)
        .map(|rec| rec.access_token.expose().to_string())
}

#[tokio::test]
async fn token_outside_the_lead_window_is_not_due_and_posts_nothing() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let fresh = unix_now() + REFRESH_LEAD_SECS + 600;
    let (store, flow) = scripted_store_with_seat(
        &path,
        "anthropic",
        fresh,
        ScriptedRefresh::Mint("new".into()),
    )
    .await;

    // Act
    let outcome = store.refresh_if_due("anthropic", None).await;

    // Assert
    assert_eq!(outcome, RefreshOutcome::NotDue);
    assert_eq!(flow.post_count(), 0, "a fresh token must not be POSTed");
}

#[tokio::test]
async fn due_token_that_mints_is_refreshed_and_persisted_for_its_labeled_seat() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let (store, flow) = scripted_store_with_seat(
        &path,
        SEAT_A,
        unix_now() + 10,
        ScriptedRefresh::Mint("tok-minted".into()),
    )
    .await;

    // Act
    let outcome = store.refresh_if_due("anthropic", Some("seat-a")).await;

    // Assert
    assert_eq!(outcome, RefreshOutcome::Refreshed);
    assert_eq!(flow.post_count(), 1);
    assert_eq!(
        disk_access_token(&path, SEAT_A).await.as_deref(),
        Some("tok-minted")
    );
}

#[tokio::test]
async fn refused_refresh_token_is_definitive_revoked() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let (store, flow) = scripted_store_with_seat(
        &path,
        "anthropic",
        unix_now() + 10,
        ScriptedRefresh::RefreshExpired,
    )
    .await;

    // Act
    let outcome = store.refresh_if_due("anthropic", None).await;

    // Assert
    assert_eq!(
        outcome,
        RefreshOutcome::Definitive(DefinitiveCause::Revoked)
    );
    assert_eq!(flow.post_count(), 1);
}

#[tokio::test]
async fn seat_with_no_stored_record_is_definitive_not_logged_in() {
    // Two distinct sources: the seat was never stored (the first read
    // misses, no POST), and the seat was logged out on disk while the
    // refresh was in flight (the commit finds it absent, after one POST).
    let rows: [(&str, bool, u32); 2] = [
        ("never stored", false, 0),
        ("logged out during the refresh", true, 1),
    ];
    for (name, logged_out_on_disk, expected_posts) in rows {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let seat = if logged_out_on_disk {
            "anthropic"
        } else {
            "other"
        };
        let (store, flow) = scripted_store_with_seat(
            &path,
            seat,
            unix_now() + 10,
            ScriptedRefresh::Mint("tok-minted".into()),
        )
        .await;
        if logged_out_on_disk {
            let sibling = OAuthStore::open(&path).await.unwrap();
            assert!(sibling.logout("anthropic").await.unwrap(), "{name}");
        }

        // Act
        let outcome = store.refresh_if_due("anthropic", None).await;

        // Assert
        assert_eq!(
            outcome,
            RefreshOutcome::Definitive(DefinitiveCause::NotLoggedIn),
            "{name}"
        );
        assert_eq!(flow.post_count(), expected_posts, "{name}");
    }
}

#[tokio::test]
async fn recoverable_refresh_failure_is_transient_with_access_expiry_against_the_clock() {
    let scripts = [
        ("token endpoint 503", ScriptedRefresh::TokenEndpoint(503)),
        ("token endpoint 500", ScriptedRefresh::TokenEndpoint(500)),
        ("token endpoint 429", ScriptedRefresh::TokenEndpoint(429)),
        ("token endpoint 400", ScriptedRefresh::TokenEndpoint(400)),
        ("token endpoint 401", ScriptedRefresh::TokenEndpoint(401)),
        ("network", ScriptedRefresh::Transient),
    ];
    let expiries: [(&str, i64, bool); 2] =
        [("before expiry", 10, false), ("after expiry", -10, true)];
    for (script_name, script) in &scripts {
        for (expiry_name, offset, access_expired) in expiries {
            // Arrange
            let name = format!("{script_name}, {expiry_name}");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("credentials.json");
            let expires_at = unix_now().saturating_add_signed(offset);
            let (store, flow) =
                scripted_store_with_seat(&path, "anthropic", expires_at, script.clone()).await;

            // Act
            let outcome = store.refresh_if_due("anthropic", None).await;

            // Assert
            assert_eq!(
                outcome,
                RefreshOutcome::Transient { access_expired },
                "{name}"
            );
            assert_eq!(flow.post_count(), 1, "{name}");
        }
    }
}

#[tokio::test]
async fn armed_cooldown_is_transient_without_a_second_post() {
    let rows: [(&str, i64, bool); 2] = [("before expiry", 10, false), ("after expiry", -10, true)];
    for (name, offset, access_expired) in rows {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let expires_at = unix_now().saturating_add_signed(offset);
        let (store, flow) =
            scripted_store_with_seat(&path, "anthropic", expires_at, ScriptedRefresh::Transient)
                .await;
        store.set_cooldown_clock_for_tests(1_000);
        let first = store.refresh_if_due("anthropic", None).await;
        assert_eq!(
            first,
            RefreshOutcome::Transient { access_expired },
            "{name}"
        );
        flow.set_script(ScriptedRefresh::Mint("tok-minted".into()));

        // Act
        let outcome = store.refresh_if_due("anthropic", None).await;

        // Assert
        assert_eq!(
            outcome,
            RefreshOutcome::Transient { access_expired },
            "{name}"
        );
        assert_eq!(
            flow.post_count(),
            1,
            "{name}: the cooldown suppressed the POST"
        );
    }
}

#[tokio::test]
async fn cooldown_window_elapsing_lets_the_next_check_refresh() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let (store, flow) = scripted_store_with_seat(
        &path,
        "anthropic",
        unix_now() + 10,
        ScriptedRefresh::Transient,
    )
    .await;
    store.set_cooldown_clock_for_tests(1_000);
    let _ = store.refresh_if_due("anthropic", None).await;
    flow.set_script(ScriptedRefresh::Mint("tok-minted".into()));
    store.set_cooldown_clock_for_tests(1_000 + 3600);

    // Act
    let outcome = store.refresh_if_due("anthropic", None).await;

    // Assert
    assert_eq!(outcome, RefreshOutcome::Refreshed);
    assert_eq!(flow.post_count(), 2);
}

#[tokio::test]
async fn relogin_landing_during_the_refresh_counts_as_refreshed() {
    // Arrange: the store's cache holds the stale record; a sibling process
    // re-logs the seat in on disk before the refresh commits.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let (store, flow) = scripted_store_with_seat(
        &path,
        "anthropic",
        unix_now() + 10,
        ScriptedRefresh::Mint("tok-minted".into()),
    )
    .await;
    let sibling = OAuthStore::open(&path).await.unwrap();
    sibling
        .write_record("anthropic", token_record("tok-relogin", unix_now() + 7200))
        .await
        .unwrap();

    // Act
    let outcome = store.refresh_if_due("anthropic", None).await;

    // Assert
    assert_eq!(outcome, RefreshOutcome::Refreshed);
    assert_eq!(flow.post_count(), 1, "premise: the refresh POST ran");
    assert_eq!(
        disk_access_token(&path, "anthropic").await.as_deref(),
        Some("tok-relogin"),
        "the re-login wins on disk"
    );
}

#[tokio::test]
async fn degraded_store_is_transient() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    write_creds_0600(&path, b"{ not json");
    let store = OAuthStore::open_or_degraded(&path).await.unwrap();

    // Act
    let outcome = store.refresh_if_due("anthropic", None).await;

    // Assert
    assert_eq!(
        outcome,
        RefreshOutcome::Transient {
            access_expired: false
        }
    );
}

#[tokio::test]
async fn concurrent_checks_on_one_seat_post_once() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let flow = Arc::new(CountingFlow::new(ScriptedRefresh::Mint("tok-minted".into())).with_yield());
    let store = seed_near_expiry_with_flow(&path, flow.clone()).await;

    // Act
    let (a, b) = tokio::join!(
        store.refresh_if_due("anthropic", None),
        store.refresh_if_due("anthropic", None)
    );

    // Assert
    assert_eq!(
        flow.call_count(),
        1,
        "the per-seat gate collapses to one POST"
    );
    assert_eq!(
        (a, b),
        (RefreshOutcome::Refreshed, RefreshOutcome::Refreshed)
    );
}
