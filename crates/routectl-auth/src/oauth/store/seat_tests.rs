use crate::oauth::store::test_support::*;

#[tokio::test]
async fn get_returns_token_when_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();

    let tok = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(tok, "tok-abc");
}

#[tokio::test]
async fn get_errors_when_provider_not_logged_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();

    let err = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no credentials"));
}

#[tokio::test]
async fn get_errors_for_unknown_provider() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    let err = store
        .get(&SecretRef::OAuth {
            provider: "made-up".into(),
            label: None,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown oauth provider"));
}

#[tokio::test]
async fn get_rejects_non_oauth_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    let err = store.get(&SecretRef::Env("FOO".into())).await.unwrap_err();
    assert!(err.to_string().contains("oauth://"));
}

#[tokio::test]
async fn delete_removes_provider() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    store
        .delete(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert!(store.list().await.is_empty());
}

#[tokio::test]
async fn set_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    let err = store
        .set(
            &SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            },
            "tok",
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("routectl login"));
}

#[tokio::test]
async fn on_auth_failure_without_record_returns_provider_specific_error() {
    // No record on disk -> force_refresh reads the missing record
    // first and surfaces NotLoggedIn ("...run `routectl login
    // anthropic` first"). Pinned because `CompositeStore` and
    // upstream callers rely on the actionable login hint.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    let err = store
        .on_auth_failure(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("routectl login anthropic"),
        "expected provider-specific guidance, got: {msg}"
    );
}

// ---- Labeled-seat resolution + per-seat single-flight ----

#[tokio::test]
async fn get_resolves_labeled_seat_token() {
    // Arrange: seed the default seat and a labeled seat with DISTINCT
    // tokens, both fresh so no refresh fires.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_named("tok-default", unix_now() + 3600))
        .await
        .unwrap();
    store
        .write_record(
            "anthropic#seat-b",
            rec_named("tok-seat-b", unix_now() + 3600),
        )
        .await
        .unwrap();

    // Act / Assert: the labeled ref resolves seat-b's token; the
    // bare ref resolves the unlabeled record.
    let seat_b = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: Some("seat-b".into()),
        })
        .await
        .unwrap();
    assert_eq!(seat_b, "tok-seat-b");

    let default = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(default, "tok-default");
}

#[tokio::test]
async fn on_auth_failure_targets_only_the_named_seat() {
    // A 401 on a labeled seat force-refreshes that seat's record and
    // leaves the sibling default seat untouched.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    // Both seats healthy: only the force path on seat-b should run.
    seed.write_record("anthropic", rec_named("tok-a-orig", unix_now() + 3600))
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
        "tok-b-rotated".into(),
    )));
    let store = open_with_flow(&path, flow.clone()).await;

    store
        .on_auth_failure(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: Some("seat-b".into()),
        })
        .await
        .expect("forced refresh of seat-b should succeed");
    assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

    // seat-b rotated; the default seat is byte-for-byte unchanged.
    let seat_b = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: Some("seat-b".into()),
        })
        .await
        .unwrap();
    assert_eq!(seat_b, "tok-b-rotated");
    let default = store
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(
        default, "tok-a-orig",
        "the default seat must be untouched by a 401 on seat-b"
    );
}

// ---- list_seats ----

#[tokio::test]
async fn oauth_list_seats_returns_default_plus_labeled_refs() {
    // A bare pool ref expands to one SecretRef per stored seat:
    // default first, then labeled seats in sorted order.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    store
        .write_record("anthropic#seat-b", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    store
        .write_record("anthropic#alpha", rec_at(unix_now() + 3600))
        .await
        .unwrap();

    let seats = store
        .list_seats(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();

    assert_eq!(
        seats,
        vec![
            SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            },
            SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some("alpha".into()),
            },
            SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some("seat-b".into()),
            },
        ]
    );
}

#[tokio::test]
async fn oauth_list_seats_on_labeled_ref_returns_just_that_seat() {
    // An already-pinned ref returns only itself -- the operator
    // selected the seat, so enumeration does not widen it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    store
        .write_record("anthropic", rec_at(unix_now() + 3600))
        .await
        .unwrap();
    store
        .write_record("anthropic#seat-b", rec_at(unix_now() + 3600))
        .await
        .unwrap();

    let pinned = SecretRef::OAuth {
        provider: "anthropic".into(),
        label: Some("seat-b".into()),
    };
    let seats = store.list_seats(&pinned).await.unwrap();
    assert_eq!(seats, vec![pinned]);
}

#[tokio::test]
async fn oauth_list_seats_no_record_falls_back_to_single_ref() {
    // No stored seats (not logged in): enumeration returns the bare
    // ref so downstream "not logged in" guidance fires rather than an
    // empty pool that resolves to nothing.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();

    let bare = SecretRef::OAuth {
        provider: "anthropic".into(),
        label: None,
    };
    let seats = store.list_seats(&bare).await.unwrap();
    assert_eq!(seats, vec![bare]);
}

#[tokio::test]
async fn peek_session_id_none_for_non_oauth_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();

    let sid = SecretStore::peek_session_id(&store, &SecretRef::Env("FOO".into())).await;
    assert!(sid.is_none(), "non-oauth ref must yield None");
}

#[tokio::test]
async fn peek_cloud_project_id_via_secret_store_trait_none_for_non_oauth_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    let pid = SecretStore::peek_cloud_project_id(&store, &SecretRef::Env("FOO".into())).await;
    assert!(pid.is_none(), "non-oauth ref must yield None");
}

#[tokio::test]
async fn set_cloud_project_id_via_secret_store_trait_non_oauth_ref_is_noop() {
    // Non-oauth refs use the default no-op; must not error.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();
    let result =
        SecretStore::set_cloud_project_id(&store, &SecretRef::Env("FOO".into()), "proj").await;
    assert!(
        result.is_ok(),
        "non-oauth ref must be a no-op, not an error"
    );
}
