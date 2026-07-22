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
/// backfills `session_id` from the in-memory `current` record
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

/// A `reload_from_disk` that completes while a refresh POST is
/// in-flight must win: the refresh result must be discarded and the
/// reloaded token left in cache. This exercises the generation-counter
/// guard in `refresh_under_lock` step 4.
///
/// Interleaving: `CountingFlow` yields twice inside `refresh_token`.
/// The reload arm yields once first (so the refresh task starts and
/// captures `gen_before`), then calls `reload_from_disk` (bumps gen).
/// When the refresh task resumes it finds `gen_now != gen_before` and
/// returns the reloaded record without clobbering the cache.
#[tokio::test]
async fn reload_during_refresh_wins_over_stale_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");

    // Seed a near-expiry record on disk so `get` triggers a refresh.
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 10))
        .await
        .unwrap();
    drop(seed);

    // CountingFlow with yield_once: refresh POST suspends mid-flight
    // so the reload arm can run between gen-snapshot and write-back.
    let flow =
        Arc::new(CountingFlow::new(RefreshOutcome::Mint("tok-from-refresh".into())).with_yield());
    let store = open_with_flow(&path, flow.clone()).await;

    // Write a newer record to disk via a separate handle -- this is
    // what reload_from_disk will pick up.
    let writer = OAuthStore::open(&path).await.unwrap();
    writer
        .write_record("anthropic", rec_named("tok-from-reload", unix_now() + 7200))
        .await
        .unwrap();
    drop(writer);

    // Run both operations concurrently on the same store. The refresh
    // (triggered by near-expiry) wins the per-provider mutex first,
    // captures gen_before, then yields to let the reload arm advance.
    // The reload arm yields once so the refresh arm can start and
    // reach its yield point before reload runs.
    let store_a = store.clone();
    let store_b = store.clone();
    let (get_result, reload_result) = tokio::join!(
        // Arm A: trigger a refresh via the near-expiry path.
        async move {
            store_a
                .get(&SecretRef::OAuth {
                    provider: "anthropic".into(),
                    label: None,
                })
                .await
        },
        // Arm B: yield once (so A starts and captures gen_before),
        // then reload. This bumps the generation counter before A
        // can acquire the file write lock.
        async move {
            tokio::task::yield_now().await;
            store_b.reload_from_disk().await
        }
    );

    assert!(get_result.is_ok(), "get should succeed: {get_result:?}");
    assert!(
        reload_result.is_ok(),
        "reload should succeed: {reload_result:?}"
    );
    // The refresh endpoint was called exactly once (it just
    // discarded its result due to the gen mismatch).
    assert_eq!(flow.call_count(), 1, "refresh endpoint called exactly once");

    // The in-memory cache must reflect the reloaded token, not the
    // refresh result. The generation counter guard must have forced
    // the refresh arm to discard its stale write-back.
    let listed = store.list().await;
    assert_eq!(listed.len(), 1, "exactly one provider in cache");
    let in_memory_tok = listed[0].1.access_token.expose().to_string();
    assert_eq!(
        in_memory_tok, "tok-from-reload",
        "reload must win over in-flight stale refresh; got: {in_memory_tok}"
    );
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
