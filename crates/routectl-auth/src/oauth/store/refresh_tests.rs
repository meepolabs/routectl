use super::*;
use crate::oauth::store::test_support::*;

#[tokio::test]
async fn get_near_expiry_triggers_refresh_and_returns_new_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    // Seed a near-expiry record on disk first (no flow yet).
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 10))
        .await
        .unwrap();
    drop(seed);

    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-refreshed".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    let tok = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(tok, "tok-refreshed");
    assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

    // The refreshed record must have been persisted: a fresh open
    // sees the new access token.
    let reopened = OAuthStore::open(&path).await.unwrap();
    let listed = reopened.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1.access_token.expose(), "tok-refreshed");
}

#[tokio::test]
async fn get_does_not_refresh_when_token_is_fresh_via_seam() {
    // Same wiring as the seam-based test above, but with a
    // not-near-expiry seed: refresh must NOT fire, even though the
    // override is set.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    drop(seed);

    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-should-not-be-used".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    let tok = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(tok, "tok-abc");
    assert_eq!(
        flow.call_count(),
        0,
        "no refresh should fire on fresh token"
    );
}

#[tokio::test]
async fn concurrent_get_calls_collapse_to_single_refresh() {
    // Two concurrent get() calls on a near-expiry token must
    // collapse to exactly one refresh through the per-provider
    // single-flight mutex. The double-check after acquiring the
    // lock returns the freshly-written record without re-POSTing.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 10))
        .await
        .unwrap();
    drop(seed);

    let flow =
        Arc::new(CountingFlow::new(RefreshOutcome::Mint("tok-refreshed".into())).with_yield());
    let store = open_with_flow(&path, flow.clone()).await;
    let store2 = store.clone();
    let r = SecretRef::OAuth {
        provider: "anthropic".into(),
        label: None,
    };
    let r2 = r.clone();

    let (a, b) = tokio::join!(async move { store.get(&r).await }, async move {
        store2.get(&r2).await
    });
    let tok_a = a.unwrap();
    let tok_b = b.unwrap();
    assert_eq!(tok_a, "tok-refreshed");
    assert_eq!(tok_b, "tok-refreshed");
    assert_eq!(
        flow.call_count(),
        1,
        "single-flight gate should collapse two concurrent gets to one refresh"
    );
}

#[tokio::test]
async fn concurrent_on_auth_failure_calls_collapse_to_single_refresh() {
    // Mirror of `concurrent_get_calls_collapse_to_single_refresh`
    // for the force-refresh path. Two concurrent
    // `on_auth_failure` calls (e.g., a 401 storm where multiple
    // in-flight requests all simultaneously detect their tokens
    // are dead) must collapse to exactly one refresh through the
    // per-provider single-flight mutex. This pins the
    // double-check semantics on the force path: the second
    // waiter compares the in-memory access token against its
    // dead-token snapshot and short-circuits when the first
    // waiter already rotated. Without this test the
    // double-check could regress to "always refresh under the
    // lock" and the test suite would not catch the redundant
    // refresh-token rotation.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    // Seed a healthy (not near expiry) record so the lazy path
    // does NOT fire; only the force path should run.
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    drop(seed);

    let flow =
        Arc::new(CountingFlow::new(RefreshOutcome::Mint("tok-after-401".into())).with_yield());
    let store = open_with_flow(&path, flow.clone()).await;
    let store2 = store.clone();
    let r = SecretRef::OAuth {
        provider: "anthropic".into(),
        label: None,
    };
    let r2 = r.clone();

    let (a, b) = tokio::join!(async move { store.on_auth_failure(&r).await }, async move {
        store2.on_auth_failure(&r2).await
    });
    a.expect("first concurrent on_auth_failure should succeed");
    b.expect("second concurrent on_auth_failure should succeed");
    assert_eq!(
        flow.call_count(),
        1,
        "single-flight + double-check should collapse two concurrent 401-recoveries to one refresh",
    );
}

#[tokio::test]
async fn on_auth_failure_forces_refresh_even_when_token_not_near_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    // Seed a healthy (not near expiry) record. on_auth_failure
    // must refresh anyway -- the upstream said the token is dead.
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    drop(seed);

    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-after-401".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    store
        .on_auth_failure(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .expect("forced refresh should succeed");
    assert_eq!(flow.call_count(), 1);

    // Subsequent `get` returns the new token.
    let tok = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(tok, "tok-after-401");
}

#[tokio::test]
async fn refresh_failure_surfaces_actionable_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 10))
        .await
        .unwrap();
    drop(seed);

    let flow = Arc::new(CountingFlow::new(RefreshOutcome::RefreshExpired));
    let store = open_with_flow(&path, flow).await;

    let err = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("oauth refresh failed for anthropic"),
        "expected wrapping prefix, got: {msg}"
    );
    assert!(
        msg.contains("routectl login anthropic"),
        "expected actionable login hint, got: {msg}"
    );
    // The wrapped root cause must include the Anthropic provider's
    // RefreshExpired Display string (its `invalid_grant` bucketing).
    assert!(
        msg.contains("refresh token expired or revoked"),
        "expected RefreshExpired display, got: {msg}"
    );
}

#[tokio::test]
async fn force_refresh_returns_new_record_for_cli() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    drop(seed);

    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-cli-refresh".into(),
    )));
    let store = open_with_flow(&path, flow).await;

    let new_rec = store.force_refresh("anthropic", None).await.unwrap();
    assert_eq!(new_rec.access_token.expose(), "tok-cli-refresh");
    assert!(new_rec.expires_at_unix > unix_now());
}

#[tokio::test]
async fn refresh_label_targets_named_seat() {
    // `force_refresh(provider, Some(label))` must refresh ONLY the
    // named seat's record and leave the default seat byte-for-byte
    // intact. Drives the `routectl refresh <provider> --label <name>`
    // store path.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    // Both seats healthy: only the forced refresh on seat-b runs.
    seed.write_record(
        "anthropic",
        rec_named("tok-default-orig", unix_now() + 3600),
    )
    .await
    .unwrap();
    seed.write_record(
        "anthropic#seat-b",
        rec_named("tok-b-orig", unix_now() + 3600),
    )
    .await
    .unwrap();
    drop(seed);

    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-b-refreshed".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    let new_rec = store
        .force_refresh("anthropic", Some("seat-b"))
        .await
        .unwrap();
    assert_eq!(new_rec.access_token.expose(), "tok-b-refreshed");
    assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

    // seat-b rotated; the default seat is untouched.
    let listed: BTreeMap<String, TokenRecord> = store.list().await.into_iter().collect();
    assert_eq!(
        listed["anthropic#seat-b"].access_token.expose(),
        "tok-b-refreshed"
    );
    assert_eq!(
        listed["anthropic"].access_token.expose(),
        "tok-default-orig",
        "the default seat must be untouched by a labeled refresh"
    );
}

/// `reload_from_disk` happy path: an external writer (sibling
/// `routectl login`, an editor) updated the credentials file. The
/// next reload must surface the new record via `list()`.
#[tokio::test]
async fn reload_from_disk_picks_up_external_mutation() {
    // Arrange: open a store, then mutate the on-disk file from
    // outside the store handle (mirroring a sibling `routectl
    // login` that writes through its own OAuthStore instance).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let store = OAuthStore::open(&path).await.unwrap();
    // First run: empty cache.
    assert!(store.list().await.is_empty());
    // External write through a fresh OAuthStore handle pinned to
    // the same path.
    let external = OAuthStore::open(&path).await.unwrap();
    external
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    drop(external);
    // The original handle's cache is still empty until reload.
    assert!(store.list().await.is_empty());

    // Act
    store.reload_from_disk().await.unwrap();

    // Assert: the freshly-loaded cache surfaces the new record.
    let listed: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
    assert_eq!(listed, vec!["anthropic"]);
}

/// `reload_from_disk` against a corrupted file (garbage bytes
/// written between snapshots) must surface the parse error AND
/// leave the in-memory cache untouched. Mirrors the disk-first
/// ordering invariant of `write_record`.
#[tokio::test]
async fn reload_from_disk_corrupt_file_keeps_cache() {
    // Arrange: seed a healthy record on disk and in memory.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    let pre: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
    assert_eq!(pre, vec!["anthropic"]);

    // Overwrite the file with garbage that still passes the
    // mode-600 hygiene check but fails JSON parse.
    std::fs::write(&path, b"<<corrupt-json>>").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    // Act
    let err = store.reload_from_disk().await.unwrap_err();

    // Assert: error is a CorruptedFile, cache unchanged.
    match err {
        OAuthError::CorruptedFile { .. } => {}
        other => panic!("expected CorruptedFile, got {other:?}"),
    }
    let post: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        pre, post,
        "memory cache must not change when reload parse fails"
    );
}

/// `reload_from_disk` against a missing file (deleted between
/// snapshots) must succeed with an empty cache -- callers treat
/// this as a degraded state but it is not a crash. Matches
/// `file_io::load`'s NotFound -> empty semantics.
#[tokio::test]
async fn reload_from_disk_missing_file_returns_empty_cache() {
    // Arrange: seed, then delete the file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    std::fs::remove_file(&path).unwrap();

    // Act
    store
        .reload_from_disk()
        .await
        .expect("reload of missing file should succeed (empty cache)");

    // Assert: cache reflects on-disk truth (nothing).
    assert!(
        store.list().await.is_empty(),
        "missing file must yield empty cache"
    );
}

/// Refresh preserves session_id across token rotation. The
/// OAuthFlow trait has no slot for the prior record; the store
/// preserves `session_id` from the disk-fresh unchanged incarnation
/// before persisting the freshly-minted one.
#[tokio::test]
async fn refresh_preserves_session_id_across_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    // Seed a record with a known session_id and a near-expiry
    // access token so the lazy refresh path fires.
    let seed = OAuthStore::open(&path).await.unwrap();
    let mut seeded = rec_at(unix_now() + 10);
    seeded.session_id = Some("seeded-session-uuid".into());
    seed.write_record("anthropic", seeded).await.unwrap();
    drop(seed);

    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-refreshed".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    // Trigger refresh through `get`.
    let _ = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

    // The persisted record carries the original session_id.
    let listed = store.list().await;
    assert_eq!(listed.len(), 1);
    let post = &listed[0].1;
    assert_eq!(
        post.session_id.as_deref(),
        Some("seeded-session-uuid"),
        "session_id must be preserved across token rotation",
    );
    assert_eq!(post.access_token.expose(), "tok-refreshed");
}

#[tokio::test]
async fn refresh_single_flight_is_per_seat() {
    // Two distinct near-expiry seats refreshed concurrently must run
    // their refreshes CONCURRENTLY -- per-seat single-flight keys the
    // gate on the seat key, so seat-a's refresh takes a different lock
    // than seat-b's and the two overlap. The concurrency gauge in the
    // fake flow observes max-in-flight == 2 only when both arms are
    // inside `refresh_token` at once; a shared per-provider lock would
    // serialize them (max == 1) even though the total count is 2 in
    // both designs (each seat's double-check still finds its own
    // record stale). The gauge is therefore the discriminating
    // assertion; the count is a secondary check.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_named("tok-a-stale", unix_now() + 10))
        .await
        .unwrap();
    seed.write_record(
        "anthropic#seat-b",
        rec_named("tok-b-stale", unix_now() + 10),
    )
    .await
    .unwrap();
    drop(seed);

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let flow = Arc::new(
        CountingFlow::new(RefreshOutcome::Mint("tok-refreshed".into()))
            .with_concurrency_gauge()
            .with_rendezvous(barrier.clone()),
    );
    let store = open_with_flow(&path, flow.clone()).await;
    let store2 = store.clone();
    let r_a = SecretRef::OAuth {
        provider: "anthropic".into(),
        label: None,
    };
    let r_b = SecretRef::OAuth {
        provider: "anthropic".into(),
        label: Some("seat-b".into()),
    };

    // Bound the join with a timeout: with per-seat locks both arms
    // reach the rendezvous and proceed; a shared per-provider lock
    // parks the second arm on the lock so it never reaches the
    // barrier, deadlocking -- the timeout turns that into a loud
    // failure rather than a silent pass.
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
        tokio::join!(async move { store.get(&r_a).await }, async move {
            store2.get(&r_b).await
        })
    })
    .await
    .expect(
        "per-seat single-flight must let both seats refresh concurrently; \
         a shared per-provider lock would deadlock the rendezvous barrier",
    );
    assert_eq!(a.unwrap(), "tok-refreshed");
    assert_eq!(b.unwrap(), "tok-refreshed");
    assert_eq!(
        flow.max_in_flight(),
        2,
        "distinct seats must refresh concurrently: a shared per-provider \
         lock would serialize them to max-in-flight 1"
    );
    assert_eq!(flow.call_count(), 2, "one refresh per seat");
}

#[tokio::test]
async fn concurrent_get_same_seat_collapses_to_one_refresh() {
    // Regression pin for the labeled-seat path: two concurrent gets
    // on the SAME labeled seat must still collapse to one refresh
    // through that seat's single-flight gate (mirrors the unlabeled
    // `concurrent_get_calls_collapse_to_single_refresh`).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record(
        "anthropic#seat-b",
        rec_named("tok-b-stale", unix_now() + 10),
    )
    .await
    .unwrap();
    drop(seed);

    let flow =
        Arc::new(CountingFlow::new(RefreshOutcome::Mint("tok-refreshed".into())).with_yield());
    let store = open_with_flow(&path, flow.clone()).await;
    let store2 = store.clone();
    let r = SecretRef::OAuth {
        provider: "anthropic".into(),
        label: Some("seat-b".into()),
    };
    let r2 = r.clone();

    let (a, b) = tokio::join!(async move { store.get(&r).await }, async move {
        store2.get(&r2).await
    });
    assert_eq!(a.unwrap(), "tok-refreshed");
    assert_eq!(b.unwrap(), "tok-refreshed");
    assert_eq!(
        flow.call_count(),
        1,
        "same-seat concurrent gets must collapse to one refresh"
    );
}

#[tokio::test]
async fn session_id_preserved_per_seat_across_refresh() {
    // seat-b's session_id must survive its own refresh and be
    // independent of the default seat's session_id. Per-seat map
    // keys make preservation automatic: the refresh reads and
    // re-writes the SAME seat's record.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    // Default seat: distinct session id, fresh (no refresh).
    let mut default = rec_named("tok-a", unix_now() + 3600);
    default.session_id = Some("session-default".into());
    seed.write_record("anthropic", default).await.unwrap();
    // seat-b: distinct session id, near-expiry so its refresh fires.
    let mut seat_b = rec_named("tok-b-stale", unix_now() + 10);
    seat_b.session_id = Some("session-seat-b".into());
    seed.write_record("anthropic#seat-b", seat_b).await.unwrap();
    drop(seed);

    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-b-refreshed".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    // Trigger seat-b's refresh via the near-expiry get path.
    let _ = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: Some("seat-b".into()),
        })
        .await
        .unwrap();
    assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

    // Read both seats back from the in-memory cache.
    let listed: BTreeMap<String, TokenRecord> = store.list().await.into_iter().collect();
    assert_eq!(
        listed["anthropic#seat-b"].session_id.as_deref(),
        Some("session-seat-b"),
        "seat-b's session_id must survive its own refresh"
    );
    assert_eq!(
        listed["anthropic#seat-b"].access_token.expose(),
        "tok-b-refreshed"
    );
    assert_eq!(
        listed["anthropic"].session_id.as_deref(),
        Some("session-default"),
        "the default seat's session_id must be independent and untouched"
    );
}

#[tokio::test]
async fn refresh_commit_does_not_clobber_sibling_seat() {
    // Arrange: seed seat A near-expiry so a `get` triggers a refresh;
    // open a handle with the fake flow (cache holds only seat A).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 10))
        .await
        .unwrap();
    drop(seed);
    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-a-refreshed".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    // A sibling writes seat B to disk out of band, after the flow-backed
    // handle's cache loaded.
    let sibling = OAuthStore::open(&path).await.unwrap();
    sibling
        .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
        .await
        .unwrap();
    drop(sibling);

    // Act: trigger seat A's refresh through the near-expiry get path.
    let tok = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(tok, "tok-a-refreshed");
    assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

    // Assert: the refresh commit merged onto the disk-fresh state, so the
    // sibling seat survives alongside the rotated seat A.
    let reopened = OAuthStore::open(&path).await.unwrap();
    let listed: BTreeMap<String, TokenRecord> = reopened.list().await.into_iter().collect();
    assert_eq!(
        listed["anthropic"].access_token.expose(),
        "tok-a-refreshed",
        "seat A must carry the refreshed token"
    );
    assert_eq!(
        listed["anthropic#seat-b"].access_token.expose(),
        "tok-b",
        "sibling seat B must survive the refresh commit"
    );
}

#[tokio::test]
async fn refresh_does_not_resurrect_logged_out_seat() {
    // Arrange: seed a seat, then open a flow-backed handle whose cache
    // still holds it. A sibling logs the seat OUT on disk before the
    // handle's refresh commits.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    drop(seed);
    let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
        "tok-refreshed".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    // Sibling logs the seat out on disk out of band.
    let sibling = OAuthStore::open(&path).await.unwrap();
    assert!(sibling.logout("anthropic").await.unwrap());
    drop(sibling);

    // Act: force a refresh from the stale handle. The POST runs, but the
    // commit re-reads the disk-fresh state (seat gone).
    let result = store.force_refresh("anthropic", None).await;

    // Assert: the sibling logout is authoritative -- the refresh must NOT
    // re-add the seat, and the operation surfaces the logged-out state.
    assert!(
        result.is_err(),
        "refresh against a logged-out seat must not succeed"
    );
    assert_eq!(
        flow.call_count(),
        1,
        "the refresh POST ran but its result was discarded"
    );
    let reopened = OAuthStore::open(&path).await.unwrap();
    assert!(
        reopened.list().await.is_empty(),
        "a logged-out seat must not be resurrected on disk"
    );
}
