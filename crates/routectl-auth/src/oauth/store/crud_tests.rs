use crate::oauth::store::Inner;
use crate::oauth::store::test_support::*;

#[tokio::test]
async fn login_with_label_does_not_overwrite_default_seat() {
    // The login write path persists through `write_record(seat_key)`.
    // Writing a labeled seat after a default is present must leave the
    // default intact -- both keys coexist. Pins the
    // `routectl login <provider> --label <name>` non-overwrite
    // contract at the store layer (the live login flow's only mutation
    // is this `write_record`).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_named("tok-default", unix_now() + 3600))
        .await
        .unwrap();

    // Labeled login effect: write under the seat key.
    store
        .write_record(
            &seat_key("anthropic", Some("seat-b")),
            rec_named("tok-seat-b", unix_now() + 3600),
        )
        .await
        .unwrap();

    let listed: BTreeMap<String, TokenRecord> = store.list().await.into_iter().collect();
    assert_eq!(listed.len(), 2, "both seats must be present");
    assert_eq!(listed["anthropic"].access_token.expose(), "tok-default");
    assert_eq!(
        listed["anthropic#seat-b"].access_token.expose(),
        "tok-seat-b"
    );
}

#[tokio::test]
async fn login_without_label_writes_bare_provider_unchanged() {
    // Back-compat pin: a label-less login writes the bare provider
    // key (`seat_key(provider, None) == provider`), byte-for-byte as
    // before. A subsequent labeled write does not move it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record(
            &seat_key("anthropic", None),
            rec_named("tok-default", unix_now() + 3600),
        )
        .await
        .unwrap();

    let listed: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        listed,
        vec!["anthropic"],
        "no-label login must write exactly the bare provider key"
    );
}

#[tokio::test]
async fn logout_returns_true_when_record_existed_and_persists_removal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();

    let removed = store.logout("anthropic").await.unwrap();
    assert!(removed, "logout should report a record was removed");
    assert!(store.list().await.is_empty());

    // Re-opening from disk must not surface the removed record.
    let reopened = OAuthStore::open(&path).await.unwrap();
    assert!(reopened.list().await.is_empty());
}

#[tokio::test]
async fn logout_returns_false_when_no_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    let removed = store.logout("anthropic").await.unwrap();
    assert!(!removed, "logout on empty store reports no removal");
}

#[tokio::test]
async fn list_returns_all_providers_in_sorted_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    // Future codex provider, written through the store's
    // back-channel. (Not gated on `lookup` -- write_record is
    // pub(crate); only `get` validates.)
    store
        .write_record("codex", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    let listed: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
    assert_eq!(listed, vec!["anthropic", "codex"]); // BTreeMap = sorted
}

#[tokio::test]
async fn write_record_failure_does_not_corrupt_memory_cache() {
    // If the disk save fails, the in-memory cache MUST keep its
    // pre-write state. We construct an OAuthStore by hand whose
    // path has a non-directory component in it -- save_blocking's
    // `create_dir_all` then fails with ENOTDIR, exercising the
    // disk-first ordering invariant.
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let bad_path = blocker.join("credentials.json");

    let http = reqwest::Client::builder().build().unwrap();
    let store = OAuthStore {
        inner: Arc::new(Inner {
            path: bad_path,
            file: RwLock::new(CredentialsFile::empty()),
            load_error: std::sync::RwLock::new(None),
            http,
            refresh_locks: std::sync::Mutex::new(BTreeMap::new()),
            reload_gen: std::sync::atomic::AtomicU64::new(0),
            refresh_cooldowns: std::sync::Mutex::new(BTreeMap::new()),
            refresh_flow: None,
            now_override: std::sync::atomic::AtomicU64::new(0),
        }),
    };

    // Pre-populate the in-memory cache so we can verify it is NOT
    // mutated by a failed save.
    store
        .inner
        .file
        .write()
        .await
        .upsert("anthropic", rec_at(unix_now() + 3600));
    let pre_cache: Vec<String> = store
        .inner
        .file
        .read()
        .await
        .providers
        .keys()
        .cloned()
        .collect();
    assert_eq!(pre_cache, vec!["anthropic"]);

    // Try to write a different provider. Save should fail (blocker
    // is a regular file, can't create dir under it). The in-memory
    // cache must not pick up the new "codex" entry.
    let result = store.write_record("codex", rec_at(unix_now() + 3600)).await;
    assert!(result.is_err(), "save should have failed (ENOTDIR)");

    let post_cache: Vec<String> = store
        .inner
        .file
        .read()
        .await
        .providers
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        pre_cache, post_cache,
        "memory cache must not change when disk save fails"
    );
}

// ---- peek_session_id ----

#[tokio::test]
async fn peek_session_id_returns_per_seat_value_for_labeled_ref() {
    // The labeled ref must resolve THAT seat's session_id, distinct
    // from the default seat's.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    let mut default = rec_at(unix_now() + 3600);
    default.session_id = Some("session-default".into());
    store.write_record("anthropic", default).await.unwrap();
    let mut seat_b = rec_at(unix_now() + 3600);
    seat_b.session_id = Some("session-seat-b".into());
    store
        .write_record("anthropic#seat-b", seat_b)
        .await
        .unwrap();

    let via_label = SecretStore::peek_session_id(
        &store,
        &SecretRef::OAuth {
            provider: "anthropic".into(),
            label: Some("seat-b".into()),
        },
    )
    .await;
    assert_eq!(via_label.as_deref(), Some("session-seat-b"));

    let via_default = SecretStore::peek_session_id(
        &store,
        &SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        },
    )
    .await;
    assert_eq!(via_default.as_deref(), Some("session-default"));
}

#[tokio::test]
async fn peek_session_id_none_for_missing_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();

    let sid = SecretStore::peek_session_id(
        &store,
        &SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        },
    )
    .await;
    assert!(sid.is_none(), "missing record must yield None");
}

#[tokio::test]
async fn peek_session_id_none_for_record_without_session_id() {
    // A record with session_id: None (e.g. a pre-existing credential)
    // yields None.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();

    let sid = SecretStore::peek_session_id(
        &store,
        &SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        },
    )
    .await;
    assert!(sid.is_none(), "record without session_id must yield None");
}

// ---- cloud_project_id ----

#[tokio::test]
async fn set_cloud_project_id_then_peek_returns_value() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    // Act
    store
        .set_cloud_project_id("anthropic", "projects/my-project")
        .await
        .unwrap();
    // Assert
    let pid = store.peek_cloud_project_id("anthropic").await;
    assert_eq!(
        pid.as_deref(),
        Some("projects/my-project"),
        "peek after set must return the stored value"
    );
}

#[tokio::test]
async fn set_cloud_project_id_persists_across_reload() {
    // Arrange: write a record, set the project id, reload, verify
    // it survived.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    store
        .set_cloud_project_id("anthropic", "projects/persistent")
        .await
        .unwrap();
    // Reopen from disk.
    let reopened = OAuthStore::open(&path).await.unwrap();
    let pid = reopened.peek_cloud_project_id("anthropic").await;
    assert_eq!(
        pid.as_deref(),
        Some("projects/persistent"),
        "cloud_project_id must survive reload_from_disk"
    );
}

#[tokio::test]
async fn set_cloud_project_id_errors_when_no_record() {
    // Arrange: empty store.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    // Act
    let result = store
        .set_cloud_project_id("anthropic", "projects/no-record")
        .await;
    // Assert
    assert!(
        result.is_err(),
        "set_cloud_project_id on a missing record must return an error"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("routectl login anthropic") || msg.contains("no credentials"),
        "expected NotLoggedIn guidance, got: {msg}"
    );
}

#[tokio::test]
async fn peek_cloud_project_id_none_for_missing_record() {
    // Arrange: empty store.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    // Act + Assert
    assert!(
        store.peek_cloud_project_id("anthropic").await.is_none(),
        "missing record must yield None"
    );
}

#[tokio::test]
async fn probe_local_present_when_access_token_unexpired() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    assert_eq!(store.probe_local("anthropic").await, LocalProbe::Present);
}

#[tokio::test]
async fn probe_local_present_when_near_expiry_but_not_yet_expired() {
    // Inside the 300s refresh lead but NOT yet expired: probe_local
    // uses raw `expires_at_unix > now`, so this is Present (no
    // inventory flap on the refresh lead).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_no_refresh(unix_now() + 10))
        .await
        .unwrap();
    assert_eq!(store.probe_local("anthropic").await, LocalProbe::Present);
}

#[tokio::test]
async fn probe_local_present_when_expired_but_refresh_token_stored() {
    // Expired access token but a refresh token is stored: revives
    // transparently on first use, so Present rather than Expired.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    // rec_at seeds a non-empty refresh token ("rtok-xyz").
    store
        .write_record("anthropic", rec_at(unix_now().saturating_sub(10)))
        .await
        .unwrap();
    assert_eq!(store.probe_local("anthropic").await, LocalProbe::Present);
}

#[tokio::test]
async fn probe_local_expired_when_expired_and_no_refresh_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_no_refresh(unix_now().saturating_sub(10)))
        .await
        .unwrap();
    assert_eq!(store.probe_local("anthropic").await, LocalProbe::Expired);
}

#[tokio::test]
async fn probe_local_missing_when_no_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    assert_eq!(store.probe_local("anthropic").await, LocalProbe::Missing);
}

#[tokio::test]
async fn probe_local_present_when_any_seat_resolves() {
    // The default seat is expired-no-refresh (would be Expired alone),
    // but a labeled seat is healthy: ANY seat resolving counts as
    // Present.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_no_refresh(unix_now().saturating_sub(10)))
        .await
        .unwrap();
    store
        .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
        .await
        .unwrap();
    assert_eq!(store.probe_local("anthropic").await, LocalProbe::Present);
}

#[tokio::test]
async fn probe_local_never_triggers_refresh() {
    // Using the fake OAuthFlow seam: probe_local must NOT invoke the
    // refresh flow for present, near-expiry, or expired inputs. Seed
    // all three seat shapes and assert zero refresh calls.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    // Present (fresh) default seat.
    seed.write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    // Near-expiry seat (inside the 300s lead) -- get() would refresh
    // this; probe_local must not.
    seed.write_record("anthropic#near", rec_named("tok-near", unix_now() + 10))
        .await
        .unwrap();
    // Expired-no-refresh seat.
    seed.write_record(
        "anthropic#dead",
        rec_no_refresh(unix_now().saturating_sub(10)),
    )
    .await
    .unwrap();
    drop(seed);

    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-should-not-be-minted".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    // Any seat resolving makes the aggregate Present; the point of
    // this test is the refresh-call count, not the discriminant.
    let _ = store.probe_local("anthropic").await;
    assert_eq!(
        flow.call_count(),
        0,
        "probe_local must never touch the refresh flow"
    );
}

// ---- cross-process re-read-under-lock merge ----
//
// Two `OAuthStore::open` handles on ONE credentials file model the
// daemon and a `routectl login`/`refresh`/`logout` CLI process writing
// the same file. A handle whose in-memory cache is stale must NOT erase
// a seat a sibling wrote since the cache loaded: every mutation re-reads
// the disk-fresh state under the advisory lock and merges its single-seat
// change onto it, rather than atomic-renaming a whole-file clone of the
// stale cache.

#[tokio::test]
async fn stale_handle_write_preserves_sibling_seat() {
    // Arrange: two handles open on one empty file. Handle 1's cache is
    // captured empty and never reloaded, so it is stale after handle 2
    // writes.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let handle1 = OAuthStore::open(&path).await.unwrap();
    let handle2 = OAuthStore::open(&path).await.unwrap();

    // Act: sibling writes seat B; then the stale handle writes seat A.
    handle2
        .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
        .await
        .unwrap();
    handle1
        .write_record("anthropic", rec_named("tok-a", unix_now() + 3600))
        .await
        .unwrap();

    // Assert: the on-disk file carries BOTH seats. Pre-fix, handle 1's
    // whole-file clone of its stale (empty) cache clobbers seat B.
    let reopened = OAuthStore::open(&path).await.unwrap();
    let listed: BTreeMap<String, TokenRecord> = reopened.list().await.into_iter().collect();
    assert_eq!(
        listed["anthropic#seat-b"].access_token.expose(),
        "tok-b",
        "sibling seat B must survive the stale-handle write"
    );
    assert_eq!(
        listed["anthropic"].access_token.expose(),
        "tok-a",
        "seat A must be written"
    );
}

#[tokio::test]
async fn remove_does_not_clobber_sibling_seat() {
    // Arrange: seed seat A so the stale handle's cache holds it; two
    // handles open on it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_named("tok-a", unix_now() + 3600))
        .await
        .unwrap();
    drop(seed);
    let handle1 = OAuthStore::open(&path).await.unwrap();
    let handle2 = OAuthStore::open(&path).await.unwrap();

    // Act: sibling writes seat B; the stale handle removes seat A.
    handle2
        .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
        .await
        .unwrap();
    let removed = handle1.remove_provider("anthropic").await.unwrap();

    // Assert: seat A removed, sibling seat B preserved.
    assert!(removed, "seat A was present in the disk-fresh state");
    let reopened = OAuthStore::open(&path).await.unwrap();
    let listed: BTreeMap<String, TokenRecord> = reopened.list().await.into_iter().collect();
    assert!(
        !listed.contains_key("anthropic"),
        "seat A must be removed from disk"
    );
    assert_eq!(
        listed["anthropic#seat-b"].access_token.expose(),
        "tok-b",
        "sibling seat B must survive the removal"
    );
}

#[tokio::test]
async fn remove_absent_seat_reports_false_against_disk_fresh_state() {
    // A logout of a seat absent from the disk-fresh state reports
    // Ok(false) and writes nothing (preserving the remove-absent
    // semantics against the re-read state, not the stale cache).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    let removed = store.remove_provider("anthropic").await.unwrap();
    assert!(!removed, "removing an absent seat reports no removal");
}

#[tokio::test]
async fn set_project_id_does_not_clobber_sibling_seat() {
    // Arrange: seed seat A; two handles.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_named("tok-a", unix_now() + 3600))
        .await
        .unwrap();
    drop(seed);
    let handle1 = OAuthStore::open(&path).await.unwrap();
    let handle2 = OAuthStore::open(&path).await.unwrap();

    // Act: sibling writes seat B; the stale handle sets a project id on
    // seat A.
    handle2
        .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
        .await
        .unwrap();
    handle1
        .set_cloud_project_id("anthropic", "projects/foo")
        .await
        .unwrap();

    // Assert: seat A carries the project id, sibling seat B preserved.
    let reopened = OAuthStore::open(&path).await.unwrap();
    let listed: BTreeMap<String, TokenRecord> = reopened.list().await.into_iter().collect();
    assert_eq!(
        listed["anthropic"].cloud_project_id.as_deref(),
        Some("projects/foo"),
        "seat A's project id must be written"
    );
    assert_eq!(
        listed["anthropic#seat-b"].access_token.expose(),
        "tok-b",
        "sibling seat B must survive set_cloud_project_id"
    );
}

#[tokio::test]
async fn set_project_id_clears_stale_seat_when_sibling_logged_out() {
    // Arrange: seed seat A with a project id on disk, then open a handle
    // whose cache holds it. A sibling logs the seat OUT on disk out of
    // band, leaving the first handle's cache stale.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    seed.set_cloud_project_id("anthropic", "projects/original")
        .await
        .unwrap();
    drop(seed);
    let stale = OAuthStore::open(&path).await.unwrap();
    let sibling = OAuthStore::open(&path).await.unwrap();
    sibling.logout("anthropic").await.unwrap();
    drop(sibling);

    // Precondition: the stale handle's cache still holds seat A -- the
    // sibling logout has not yet been observed through this handle.
    assert_eq!(
        stale.peek_cloud_project_id("anthropic").await.as_deref(),
        Some("projects/original"),
        "stale cache must still hold the seat before the not-found merge"
    );

    // Act: set a project id through the stale handle. The re-read under
    // the lock sees the disk-fresh (empty) state and reports not-found.
    let result = stale
        .set_cloud_project_id("anthropic", "projects/new")
        .await;

    // Assert: the call surfaces NotLoggedIn AND the stale seat is cleared
    // from the in-memory cache immediately -- a subsequent read through
    // the same handle no longer sees it (not deferred to a reload).
    assert!(
        matches!(result, Err(OAuthError::NotLoggedIn(_))),
        "setting a project id on a sibling-logged-out seat must return NotLoggedIn"
    );
    assert!(
        stale.read_record("anthropic").await.is_err(),
        "the stale seat must be cleared from the cache on the not-found path"
    );
    assert!(
        stale.peek_cloud_project_id("anthropic").await.is_none(),
        "a subsequent read through the same handle must not see the stale seat"
    );
}
