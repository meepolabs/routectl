//! Deterministic refresh/reload interleavings. All tokens and flows are fake;
//! oneshot gates hold the POST open until the competing mutation completes.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::*;
use crate::oauth::providers::{AuthParams, OAuthFlow};
use crate::oauth::store::test_support::*;
use crate::oauth::types::SecretToken;

struct PendingPost {
    refresh_token: String,
    reply: oneshot::Sender<OAuthResult<TokenRecord>>,
}

/// No HTTP and no scheduler-yield timing: each request announces its exact
/// prior refresh token, then waits for the test to supply the endpoint result.
struct GatedFlow {
    posts: mpsc::UnboundedSender<PendingPost>,
    calls: AtomicUsize,
}

#[async_trait]
impl OAuthFlow for GatedFlow {
    fn provider_id(&self) -> &'static str {
        "anthropic"
    }
    fn display_name(&self) -> &'static str {
        "Offline gated refresh"
    }
    fn auth_url(&self, _: &AuthParams<'_>) -> url::Url {
        unreachable!("no login in offline refresh tests")
    }
    fn manual_redirect_url(&self) -> &'static str {
        "https://example.invalid/callback"
    }
    async fn exchange_code(
        &self,
        _: &reqwest::Client,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> OAuthResult<TokenRecord> {
        unreachable!("no login in offline refresh tests")
    }
    async fn refresh_token(
        &self,
        _: &reqwest::Client,
        refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (reply, response) = oneshot::channel();
        self.posts
            .send(PendingPost {
                refresh_token: refresh_token.into(),
                reply,
            })
            .expect("test must observe the POST");
        response.await.expect("test must release the POST")
    }
}

fn credential(name: &str, generation: u64) -> TokenRecord {
    let mut rec = rec_named(&format!("test-access-{name}"), u64::MAX);
    rec.refresh_token = SecretToken::new(format!("test-refresh-{name}"));
    rec.obtained_at_unix = generation;
    rec.account.account_id = Some(format!("test-account-{name}"));
    rec.account.email = Some(format!("{name}@example.invalid"));
    rec
}

fn initial_credential() -> TokenRecord {
    let mut rec = credential("original", 100);
    rec.session_id = Some("test-original-session".into());
    rec.cloud_project_id = Some("test-original-project".into());
    rec
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("interleaving gate did not complete (no sleeps required)")
}

struct Harness {
    _dir: tempfile::TempDir,
    store: OAuthStore,
    sibling: OAuthStore,
    flow: Arc<GatedFlow>,
    posts: mpsc::UnboundedReceiver<PendingPost>,
    provider: &'static str,
    label: Option<&'static str>,
    seat: String,
}

impl Harness {
    async fn new(provider: &'static str, label: Option<&'static str>, rec: TokenRecord) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let seat = seat_key(provider, label);
        let sibling = OAuthStore::open(&path).await.unwrap();
        sibling.write_record(&seat, rec).await.unwrap();
        let (posts, receiver) = mpsc::unbounded_channel();
        let flow = Arc::new(GatedFlow {
            posts,
            calls: AtomicUsize::new(0),
        });
        let store = open_with_flow(&path, flow.clone()).await;
        Self {
            _dir: dir,
            store,
            sibling,
            flow,
            posts: receiver,
            provider,
            label,
            seat,
        }
    }

    fn spawn_refresh(&self) -> JoinHandle<Result<TokenRecord>> {
        let store = self.store.clone();
        let provider = self.provider;
        let label = self.label;
        tokio::spawn(async move { store.force_refresh(provider, label).await })
    }

    async fn start(&mut self) -> (JoinHandle<Result<TokenRecord>>, PendingPost) {
        let task = self.spawn_refresh();
        let post = bounded(self.posts.recv()).await.unwrap();
        assert_eq!(post.refresh_token, "test-refresh-original");
        (task, post)
    }

    async fn assert_winner(&self, expected: &TokenRecord) {
        assert_eq!(self.store.read_record(&self.seat).await.unwrap(), *expected);
        let reopened = OAuthStore::open(self.store.path()).await.unwrap();
        assert_eq!(reopened.read_record(&self.seat).await.unwrap(), *expected);
    }

    fn assert_calls(&self, count: usize) {
        assert_eq!(self.flow.calls.load(Ordering::SeqCst), count);
    }
}

fn rotated_with_metadata(prior: &TokenRecord) -> TokenRecord {
    let mut rotated = credential("rotated", 200);
    rotated.session_id = prior.session_id.clone();
    rotated.cloud_project_id = prior.cloud_project_id.clone();
    rotated
}

#[tokio::test]
async fn unchanged_seat_reload_during_refresh_keeps_rotating_token() {
    let original = initial_credential();
    let mut h = Harness::new("anthropic", None, original.clone()).await;
    let (task, post) = h.start().await;
    // Watcher events from our own writes (and repeated SIGHUP) are no-ops
    // for this seat. Neither may discard a successfully rotated token.
    h.store.reload_from_disk().await.unwrap();
    h.store.reload_from_disk().await.unwrap();
    post.reply.send(Ok(credential("rotated", 200))).unwrap();
    let returned = bounded(task).await.unwrap().unwrap();
    let expected = rotated_with_metadata(&original);
    assert_eq!(returned, expected);
    h.assert_winner(&expected).await;
    assert_eq!(
        h.store.get(&anthropic_ref()).await.unwrap(),
        "test-access-rotated"
    );
    h.assert_calls(1);
}

#[tokio::test]
async fn unchanged_seat_reload_during_lazy_refresh_keeps_rotating_token() {
    let mut original = initial_credential();
    original.expires_at_unix = 0; // Always stale, independent of the wall clock.
    let mut h = Harness::new("anthropic", None, original.clone()).await;
    let store = h.store.clone();
    let task = tokio::spawn(async move { store.get(&anthropic_ref()).await });
    let post = bounded(h.posts.recv()).await.unwrap();
    assert_eq!(post.refresh_token, "test-refresh-original");
    h.store.reload_from_disk().await.unwrap();
    post.reply.send(Ok(credential("rotated", 200))).unwrap();
    assert_eq!(bounded(task).await.unwrap().unwrap(), "test-access-rotated");
    h.assert_winner(&rotated_with_metadata(&original)).await;
    h.assert_calls(1);
}

#[tokio::test]
async fn unrelated_seat_updates_and_reload_do_not_discard_refresh() {
    let original = initial_credential();
    let mut h = Harness::new("anthropic", Some("target"), original.clone()).await;
    let (task, post) = h.start().await;
    let other_seat = credential("other-seat", 300);
    let other_provider = credential("other-provider", 400);
    h.sibling
        .write_record("anthropic", other_seat.clone())
        .await
        .unwrap();
    h.sibling
        .write_record("codex#other", other_provider.clone())
        .await
        .unwrap();
    h.store.reload_from_disk().await.unwrap();
    post.reply.send(Ok(credential("rotated", 200))).unwrap();
    assert_eq!(
        bounded(task).await.unwrap().unwrap(),
        rotated_with_metadata(&original)
    );
    h.assert_winner(&rotated_with_metadata(&original)).await;
    let reopened = OAuthStore::open(h.store.path()).await.unwrap();
    for store in [&h.store, &reopened] {
        assert_eq!(store.read_record("anthropic").await.unwrap(), other_seat);
        assert_eq!(
            store.read_record("codex#other").await.unwrap(),
            other_provider
        );
    }
    h.assert_calls(1);
}

#[tokio::test]
async fn watcher_reload_after_overlapping_other_seat_refresh_keeps_both_rotations() {
    let original = initial_credential();
    let mut h = Harness::new("anthropic", None, original.clone()).await;
    let other = credential("other", 300);
    h.store
        .write_record("anthropic#other", other.clone())
        .await
        .unwrap();
    let task_a = h.spawn_refresh();
    let store = h.store.clone();
    let task_b = tokio::spawn(async move { store.force_refresh("anthropic", Some("other")).await });
    // Both POSTs must be in flight before either is released. This also
    // verifies per-seat (not per-provider) single-flight without yields.
    let first = bounded(h.posts.recv()).await.unwrap();
    let second = bounded(h.posts.recv()).await.unwrap();
    let (post_a, post_b) = if first.refresh_token == "test-refresh-original" {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(post_a.refresh_token, "test-refresh-original");
    assert_eq!(post_b.refresh_token, "test-refresh-other");
    let rotated_b = credential("other-rotated", 400);
    post_b.reply.send(Ok(rotated_b.clone())).unwrap();
    assert_eq!(bounded(task_b).await.unwrap().unwrap(), rotated_b);
    // The daemon's own B commit emits a file event while A is still POSTing.
    h.store.reload_from_disk().await.unwrap();
    post_a.reply.send(Ok(credential("rotated", 200))).unwrap();
    assert_eq!(
        bounded(task_a).await.unwrap().unwrap(),
        rotated_with_metadata(&original)
    );
    h.assert_winner(&rotated_with_metadata(&original)).await;
    let reopened = OAuthStore::open(h.store.path()).await.unwrap();
    assert_eq!(
        h.store.read_record("anthropic#other").await.unwrap(),
        rotated_b
    );
    assert_eq!(
        reopened.read_record("anthropic#other").await.unwrap(),
        rotated_b
    );
    h.assert_calls(2);
}

/// Preserve the existing changed-seat-reload contract, but pin the exact
/// ordering with a gate instead of hoping scheduler yields overlap disk I/O.
#[tokio::test]
async fn reload_during_refresh_wins_over_stale_result() {
    let mut h = Harness::new("anthropic", None, initial_credential()).await;
    let (task, post) = h.start().await;
    let replacement = credential("login", 500); // No stale session/project.
    h.sibling
        .write_record(&h.seat, replacement.clone())
        .await
        .unwrap();
    h.store.reload_from_disk().await.unwrap();
    let bytes = std::fs::read(h.store.path()).unwrap();
    post.reply.send(Ok(credential("rotated", 200))).unwrap();
    assert_eq!(bounded(task).await.unwrap().unwrap(), replacement);
    assert_eq!(
        std::fs::read(h.store.path()).unwrap(),
        bytes,
        "discard writes nothing"
    );
    h.assert_winner(&replacement).await;
    h.assert_calls(1);
}

#[tokio::test]
async fn sibling_same_seat_login_during_post_wins_without_reload() {
    for label in [None, Some("target")] {
        let mut h = Harness::new("anthropic", label, initial_credential()).await;
        let (task, post) = h.start().await;
        let mut replacement = credential("login", 500);
        replacement.session_id = Some("test-login-session".into());
        replacement.cloud_project_id = Some("test-login-project".into());
        h.sibling
            .write_record(&h.seat, replacement.clone())
            .await
            .unwrap();
        let bytes = std::fs::read(h.store.path()).unwrap();
        post.reply.send(Ok(credential("rotated", 200))).unwrap();
        assert_eq!(bounded(task).await.unwrap().unwrap(), replacement);
        assert_eq!(std::fs::read(h.store.path()).unwrap(), bytes);
        h.assert_winner(&replacement).await;
        h.assert_calls(1);
    }
}

#[tokio::test]
async fn same_access_token_different_refresh_token_is_a_replacement() {
    for reload in [false, true] {
        let original = initial_credential();
        let mut h = Harness::new("anthropic", Some("target"), original.clone()).await;
        let (task, post) = h.start().await;
        let mut replacement = original;
        replacement.refresh_token = SecretToken::new("test-replacement-refresh");
        replacement.session_id = None;
        replacement.cloud_project_id = None;
        h.sibling
            .write_record(&h.seat, replacement.clone())
            .await
            .unwrap();
        if reload {
            h.store.reload_from_disk().await.unwrap();
        }
        post.reply.send(Ok(credential("rotated", 200))).unwrap();
        assert_eq!(bounded(task).await.unwrap().unwrap(), replacement);
        h.assert_winner(&replacement).await;
        h.assert_calls(1);
    }
}

#[tokio::test]
async fn in_process_same_seat_login_during_post_wins() {
    let mut h = Harness::new("anthropic", None, initial_credential()).await;
    let (task, post) = h.start().await;
    let replacement = credential("login", 500);
    h.store
        .write_record(&h.seat, replacement.clone())
        .await
        .unwrap();
    post.reply.send(Ok(credential("rotated", 200))).unwrap();
    assert_eq!(bounded(task).await.unwrap().unwrap(), replacement);
    h.assert_winner(&replacement).await;
    h.assert_calls(1);
}

#[tokio::test]
async fn sibling_logout_during_post_never_resurrects_seat() {
    for label in [None, Some("target")] {
        for reload in [false, true] {
            let mut h = Harness::new("anthropic", label, initial_credential()).await;
            let (task, post) = h.start().await;
            assert!(h.sibling.logout(&h.seat).await.unwrap());
            if reload {
                h.store.reload_from_disk().await.unwrap();
            }
            let bytes = std::fs::read(h.store.path()).unwrap();
            post.reply.send(Ok(credential("rotated", 200))).unwrap();
            let err = bounded(task).await.unwrap().unwrap_err();
            assert!(err.to_string().contains("no credentials"), "{err}");
            assert_eq!(std::fs::read(h.store.path()).unwrap(), bytes);
            assert!(h.store.list().await.is_empty());
            assert!(
                OAuthStore::open(h.store.path())
                    .await
                    .unwrap()
                    .list()
                    .await
                    .is_empty()
            );
            h.assert_calls(1);
        }
    }
}

#[tokio::test]
async fn in_process_logout_during_post_never_resurrects_seat() {
    let mut h = Harness::new("anthropic", None, initial_credential()).await;
    let (task, post) = h.start().await;
    assert!(h.store.logout(&h.seat).await.unwrap());
    post.reply.send(Ok(credential("rotated", 200))).unwrap();
    assert!(bounded(task).await.unwrap().is_err());
    assert!(h.store.list().await.is_empty());
    assert!(
        OAuthStore::open(h.store.path())
            .await
            .unwrap()
            .list()
            .await
            .is_empty()
    );
    h.assert_calls(1);
}

#[tokio::test]
async fn cloud_project_and_session_metadata_survive_refresh_and_reopen() {
    for label in [None, Some("target")] {
        let original = initial_credential();
        let mut h = Harness::new("antigravity", label, original.clone()).await;
        let (task, post) = h.start().await;
        // Like antigravity, the endpoint has no knowledge of local metadata.
        let response = credential("rotated", 200);
        assert!(response.cloud_project_id.is_none() && response.session_id.is_none());
        post.reply.send(Ok(response)).unwrap();
        let expected = rotated_with_metadata(&original);
        assert_eq!(bounded(task).await.unwrap().unwrap(), expected);
        h.assert_winner(&expected).await;
        assert_eq!(
            h.store.peek_cloud_project_id(&h.seat).await,
            original.cloud_project_id
        );
        h.assert_calls(1);
    }
}

#[tokio::test]
async fn concurrent_metadata_updates_are_preserved_from_disk_fresh_incarnation() {
    for same_process in [false, true] {
        for reload in [false, true] {
            let original = initial_credential();
            let mut h = Harness::new("antigravity", Some("target"), original).await;
            let (task, post) = h.start().await;
            let writer = if same_process { &h.store } else { &h.sibling };
            writer
                .set_cloud_project_id(&h.seat, "test-updated-project")
                .await
                .unwrap();
            // Mirrors factory-driven session backfill, which is a record
            // write with no change to endpoint-owned grant/account fields.
            let mut metadata = writer.read_record(&h.seat).await.unwrap();
            metadata.session_id = Some("test-updated-session".into());
            writer
                .write_record(&h.seat, metadata.clone())
                .await
                .unwrap();
            if reload {
                h.store.reload_from_disk().await.unwrap();
            }
            post.reply.send(Ok(credential("rotated", 200))).unwrap();
            let expected = rotated_with_metadata(&metadata);
            assert_eq!(bounded(task).await.unwrap().unwrap(), expected);
            h.assert_winner(&expected).await;
            h.assert_calls(1);
        }
    }
}

#[tokio::test]
async fn concurrent_project_clear_is_not_undone_by_stale_refresh_metadata() {
    for reload in [false, true] {
        let mut original = initial_credential();
        let mut h = Harness::new("antigravity", None, original.clone()).await;
        let (task, post) = h.start().await;
        assert!(
            h.sibling
                .clear_cloud_project_id_if_matches(&h.seat, "test-original-project")
                .await
                .unwrap()
        );
        if reload {
            h.store.reload_from_disk().await.unwrap();
        }
        // Endpoint values for credential-local fields cannot undo a
        // concurrent clear/backfill from the authoritative local record.
        let mut response = credential("rotated", 200);
        response.cloud_project_id = Some("test-stale-project".into());
        response.session_id = Some("test-stale-session".into());
        post.reply.send(Ok(response)).unwrap();
        original.cloud_project_id = None;
        let expected = rotated_with_metadata(&original);
        assert_eq!(bounded(task).await.unwrap().unwrap(), expected);
        h.assert_winner(&expected).await;
        h.assert_calls(1);
    }
}

#[tokio::test]
async fn replaced_login_without_project_does_not_inherit_previous_account_metadata() {
    let mut h = Harness::new("antigravity", None, initial_credential()).await;
    let (task, post) = h.start().await;
    let replacement = credential("different-account", 500);
    h.sibling
        .write_record(&h.seat, replacement.clone())
        .await
        .unwrap();
    post.reply.send(Ok(credential("rotated", 200))).unwrap();
    assert_eq!(bounded(task).await.unwrap().unwrap(), replacement);
    h.assert_winner(&replacement).await;
    assert!(h.store.peek_cloud_project_id(&h.seat).await.is_none());
    assert!(h.store.peek_session_id(&h.seat).await.is_none());
    h.assert_calls(1);
}

#[tokio::test]
async fn corrupt_disk_during_post_refuses_commit_and_keeps_cache() {
    let original = initial_credential();
    let mut h = Harness::new("anthropic", None, original.clone()).await;
    let (task, post) = h.start().await;
    write_creds_0600(h.store.path(), b"not json");
    post.reply.send(Ok(credential("rotated", 200))).unwrap();
    assert!(bounded(task).await.unwrap().is_err());
    assert_eq!(std::fs::read(h.store.path()).unwrap(), b"not json");
    assert_eq!(h.store.read_record(&h.seat).await.unwrap(), original);
    h.assert_calls(1);
}

#[tokio::test]
async fn reload_waits_for_cache_lock_before_reading_disk_snapshot() {
    let h = Harness::new("anthropic", None, initial_credential()).await;
    let mut guard = h.store.inner.file.write().await;
    let read_started = std::cell::Cell::new(false);
    // This is the disk-read future used by the public reload path, with
    // a test-only poll marker. No task scheduling or filesystem timing.
    let load = async {
        read_started.set(true);
        file_io::load(h.store.path()).await
    };
    let mut reload = std::pin::pin!(h.store.reload_from(load));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(reload.as_mut().poll(&mut context), Poll::Pending));
    assert!(
        !read_started.get(),
        "reload must not read disk while awaiting the cache lock"
    );
    // A refresh commit already holding the cache lock writes and caches
    // the rotation before the blocked reload can capture any snapshot.
    let expected = credential("rotated", 200);
    let copy = expected.clone();
    let (merged, ()) = file_io::update_under_lock(h.store.path(), move |cf| {
        cf.upsert("anthropic", copy);
        file_io::Mutation {
            directive: file_io::WriteDirective::Write,
            report: (),
        }
    })
    .await
    .unwrap();
    *guard = merged;
    drop(guard);
    bounded(reload).await.unwrap();
    assert!(read_started.get());
    h.assert_winner(&expected).await;
}

#[test]
fn credential_comparison_excludes_only_local_metadata() {
    let original = initial_credential();
    let mut changes = Vec::new();
    let mut rec = original.clone();
    rec.access_token = SecretToken::new("test-new-access");
    changes.push(rec);
    let mut rec = original.clone();
    rec.refresh_token = SecretToken::new("test-new-refresh");
    changes.push(rec);
    let mut rec = original.clone();
    rec.token_type = "Other".into();
    changes.push(rec);
    let mut rec = original.clone();
    rec.expires_at_unix -= 1;
    changes.push(rec);
    let mut rec = original.clone();
    rec.scopes.push("test-new-scope".into());
    changes.push(rec);
    let mut rec = original.clone();
    rec.account.account_id = Some("test-new-account".into());
    changes.push(rec);
    let mut rec = original.clone();
    rec.account.email = Some("new@example.invalid".into());
    changes.push(rec);
    let mut rec = original.clone();
    rec.obtained_at_unix += 1;
    changes.push(rec);
    for changed in changes {
        assert!(!same_credential(&original, &changed));
        assert!(!same_credential(&changed, &original));
    }
    let mut metadata = original.clone();
    metadata.session_id = None;
    metadata.cloud_project_id = Some("test-new-project".into());
    assert!(same_credential(&original, &metadata));
    assert!(same_credential(&metadata, &original));
}
