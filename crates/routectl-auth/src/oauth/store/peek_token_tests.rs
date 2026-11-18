use std::path::Path;
use std::time::SystemTime;

use super::*;
use crate::oauth::store::scripted::{ScriptedRefreshFlow, token_record};
use crate::oauth::store::test_support::*;

const TOKEN_MARKER: &str = "tok-peek-marker-7f3a";
const LABELED_SEAT: &str = "anthropic#seat-a";

/// Seed `seat` on disk with `TOKEN_MARKER` expiring at `expires_at`, then
/// open a store whose refresh POSTs would be answered (and counted) by a
/// scripted flow.
async fn store_with_seat(
    path: &Path,
    seat: &str,
    expires_at: u64,
) -> (OAuthStore, Arc<ScriptedRefreshFlow>) {
    let seed = OAuthStore::open(path).await.unwrap();
    seed.write_record(seat, token_record(TOKEN_MARKER, expires_at))
        .await
        .unwrap();
    drop(seed);
    let flow = Arc::new(ScriptedRefreshFlow::new(ScriptedRefresh::Mint(
        "tok-minted".into(),
    )));
    let store = OAuthStore::open_with_scripted_refresh_for_tests(path, flow.clone())
        .await
        .unwrap();
    (store, flow)
}

fn file_state(path: &Path) -> (Vec<u8>, SystemTime) {
    let bytes = std::fs::read(path).unwrap();
    let mtime = std::fs::metadata(path).unwrap().modified().unwrap();
    (bytes, mtime)
}

/// Peek `seat` and assert the peek left the credentials file and the
/// token endpoint untouched.
async fn peek_without_side_effects(
    path: &Path,
    store: &OAuthStore,
    flow: &ScriptedRefreshFlow,
    seat: &str,
) -> PeekedAccessToken {
    let before = file_state(path);
    let peeked = store.peek_access_token(seat).await;
    assert_eq!(file_state(path), before, "peek must not write the file");
    assert_eq!(flow.post_count(), 0, "peek must not refresh");
    peeked
}

#[tokio::test]
async fn fresh_token_is_returned_for_its_seat() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let fresh = unix_now() + REFRESH_LEAD_SECS + 600;
    let (store, flow) = store_with_seat(&path, LABELED_SEAT, fresh).await;

    // Act
    let peeked = peek_without_side_effects(&path, &store, &flow, LABELED_SEAT).await;

    // Assert
    let PeekedAccessToken::Ready(token) = peeked else {
        panic!("a fresh token must be Ready, got {peeked:?}");
    };
    assert_eq!(token.expose(), TOKEN_MARKER);
}

#[tokio::test]
async fn token_not_outside_the_lead_window_is_not_ready() {
    let now = unix_now();
    let cases = [
        ("inside the lead window", now + REFRESH_LEAD_SECS - 60),
        ("already expired", now.saturating_sub(60)),
    ];
    for (name, expires_at) in cases {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let (store, flow) = store_with_seat(&path, LABELED_SEAT, expires_at).await;

        // Act
        let peeked = peek_without_side_effects(&path, &store, &flow, LABELED_SEAT).await;

        // Assert
        assert_eq!(peeked, PeekedAccessToken::NotReady, "case: {name}");
    }
}

#[tokio::test]
async fn seat_without_a_record_is_missing() {
    // Arrange: only the labeled seat exists, so the bare provider key and
    // another label have no record.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let fresh = unix_now() + REFRESH_LEAD_SECS + 600;
    let (store, flow) = store_with_seat(&path, LABELED_SEAT, fresh).await;

    for seat in ["anthropic", "anthropic#seat-b"] {
        // Act
        let peeked = peek_without_side_effects(&path, &store, &flow, seat).await;

        // Assert
        assert_eq!(peeked, PeekedAccessToken::Missing, "seat: {seat}");
    }
}

#[tokio::test]
async fn debug_of_a_ready_token_does_not_show_the_token() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let fresh = unix_now() + REFRESH_LEAD_SECS + 600;
    let (store, _flow) = store_with_seat(&path, LABELED_SEAT, fresh).await;
    let peeked = store.peek_access_token(LABELED_SEAT).await;
    assert!(
        matches!(peeked, PeekedAccessToken::Ready(_)),
        "fixture must yield a token to redact"
    );

    // Act
    let rendered = format!("{peeked:?}");

    // Assert
    assert!(
        !rendered.contains(TOKEN_MARKER),
        "Debug leaked the token: {rendered}"
    );
}

const ACCOUNT_MARKER: &str = "acct-peek-marker-2c91";

/// `store_with_seat` with `ACCOUNT_MARKER` recorded as the seat's account id.
async fn store_with_account(
    path: &Path,
    seat: &str,
    expires_at: u64,
) -> (OAuthStore, Arc<ScriptedRefreshFlow>) {
    let seed = OAuthStore::open(path).await.unwrap();
    let mut record = token_record(TOKEN_MARKER, expires_at);
    record.account.account_id = Some(ACCOUNT_MARKER.into());
    seed.write_record(seat, record).await.unwrap();
    drop(seed);
    let flow = Arc::new(ScriptedRefreshFlow::new(ScriptedRefresh::Mint(
        "tok-minted".into(),
    )));
    let store = OAuthStore::open_with_scripted_refresh_for_tests(path, flow.clone())
        .await
        .unwrap();
    (store, flow)
}

#[tokio::test]
async fn a_credential_peek_reads_token_and_account_id_without_side_effects() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let fresh = unix_now() + REFRESH_LEAD_SECS + 600;
    let (store, flow) = store_with_account(&path, LABELED_SEAT, fresh).await;
    let before = file_state(&path);

    // Act
    let peeked = store.peek_credential(LABELED_SEAT).await;
    let missing = store.peek_credential("anthropic#seat-b").await;

    // Assert
    assert_eq!(file_state(&path), before, "peek must not write the file");
    assert_eq!(flow.post_count(), 0, "peek must not refresh");
    let PeekedAccessToken::Ready(token) = &peeked.token else {
        panic!("a fresh token must be Ready, got {peeked:?}");
    };
    assert_eq!(token.expose(), TOKEN_MARKER);
    assert_eq!(peeked.account_id.as_deref(), Some(ACCOUNT_MARKER));
    assert_eq!(missing.token, PeekedAccessToken::Missing);
    assert_eq!(missing.account_id, None);
}

#[tokio::test]
async fn a_credential_peek_reports_the_reload_generation_a_reload_advances() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let fresh = unix_now() + REFRESH_LEAD_SECS + 600;
    let (store, _flow) = store_with_account(&path, LABELED_SEAT, fresh).await;
    let peeked = store.peek_credential(LABELED_SEAT).await;
    assert_eq!(
        store.reload_generation(),
        peeked.reload_generation,
        "premise: no reload yet"
    );

    // Act
    store.reload_from_disk().await.unwrap();

    // Assert
    assert_ne!(store.reload_generation(), peeked.reload_generation);
    assert_eq!(
        store.peek_credential(LABELED_SEAT).await.reload_generation,
        store.reload_generation()
    );
}

#[tokio::test]
async fn debug_of_a_credential_peek_shows_neither_token_nor_account_id() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let fresh = unix_now() + REFRESH_LEAD_SECS + 600;
    let (store, _flow) = store_with_account(&path, LABELED_SEAT, fresh).await;
    let peeked = store.peek_credential(LABELED_SEAT).await;
    assert!(
        matches!(peeked.token, PeekedAccessToken::Ready(_)) && peeked.account_id.is_some(),
        "fixture must yield a token and an account id to redact"
    );

    // Act
    let rendered = format!("{peeked:?}");

    // Assert
    for secret in [TOKEN_MARKER, ACCOUNT_MARKER] {
        assert!(!rendered.contains(secret), "Debug leaked: {rendered}");
    }
}
