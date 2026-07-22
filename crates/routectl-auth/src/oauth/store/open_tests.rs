use crate::oauth::store::test_support::*;

#[tokio::test]
async fn open_round_trips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    {
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
    }
    // Re-open and verify state persisted.
    let store2 = OAuthStore::open(&path).await.unwrap();
    let tok = store2
        .get(&SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(tok, "tok-abc");
}

/// The shared OAuth transport client is identity-neutral: it stamps
/// NO per-provider fingerprint (no codex originator/residency, no
/// codex User-Agent). Per-provider identity is applied per-request
/// inside each `OAuthFlow` so one provider's fingerprint never leaks
/// onto another provider's token endpoint. (The codex fingerprint is
/// now proven present on the codex POSTs by the `codex_identity`
/// tests in `providers/codex.rs`.)
#[tokio::test]
async fn shared_client_is_identity_neutral() {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    // Arrange: stand up an OAuthStore so its production client
    // builder runs.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("creds.json");
    let store = OAuthStore::open(&path).await.unwrap();

    // Capture the headers of the next inbound request via a
    // wiremock mock that records the body+headers and answers 200.
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    // Act: drive a real request through `store.http()` with no
    // per-request identity stamped.
    let resp = store
        .http()
        .post(server.uri())
        .send()
        .await
        .expect("request send");
    assert_eq!(resp.status().as_u16(), 200);

    // Assert: the recorded request carries NO codex fingerprint.
    let received: Vec<Request> = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "one request reached the mock");
    let req = &received[0];
    let header = |name: &str| -> Option<String> {
        req.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    assert!(
        header("originator").is_none(),
        "shared client must NOT stamp the codex originator header",
    );
    assert!(
        header("x-openai-internal-codex-residency").is_none(),
        "shared client must NOT stamp the codex residency header",
    );
    let ua = header("user-agent");
    // A None/absent UA also satisfies this: the claim is "the codex UA
    // prefix is not stamped on the shared client," not "some UA is
    // always present." is_none_or(..) makes absence pass by design.
    assert!(
        ua.as_deref()
            .is_none_or(|u| !u.starts_with("codex_cli_rs/")),
        "shared client must NOT stamp the codex User-Agent, got: {ua:?}",
    );
}

#[cfg(unix)]
#[tokio::test]
async fn degraded_store_surfaces_perms_cause_not_missing_home() {
    use std::os::unix::fs::PermissionsExt;
    // Arrange: a valid-JSON credentials file with world-readable 0644
    // perms -- the loader refuses it (the file holds refresh tokens).
    let dir = tempfile::tempdir().unwrap();
    let dir_str = dir.path().to_string_lossy().to_string();
    let path = dir.path().join("credentials.json");
    std::fs::write(&path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    // Act: the serve start-and-degrade path always constructs a store.
    let store = OAuthStore::open_or_degraded(&path).await.unwrap();
    let msg = store
        .get(&oauth_ref("anthropic"))
        .await
        .unwrap_err()
        .to_string();

    // Assert: the true perms class, NOT the misleading HOME/XDG string,
    // and path-free / perms-value-free.
    assert!(
        msg.contains("could not be read"),
        "expected the perms class, got: {msg}"
    );
    assert!(
        !msg.contains("HOME") && !msg.contains("XDG"),
        "a degraded perms cause must not surface the no-config-dir string: {msg}"
    );
    assert!(
        !msg.contains(&dir_str) && !msg.contains("644"),
        "the cause must be path-free and perms-value-free: {msg}"
    );
    assert!(
        msg.contains("reloads without restart"),
        "expected the recovery hint, got: {msg}"
    );
}

#[tokio::test]
async fn degraded_store_surfaces_corrupt_cause() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let dir_str = dir.path().to_string_lossy().to_string();
    let path = dir.path().join("credentials.json");
    write_creds_0600(&path, b"<<corrupt-json>>");

    // Act
    let store = OAuthStore::open_or_degraded(&path).await.unwrap();
    let msg = store
        .get(&oauth_ref("anthropic"))
        .await
        .unwrap_err()
        .to_string();

    // Assert
    assert!(
        msg.contains("corrupted"),
        "expected the corrupt class, got: {msg}"
    );
    assert!(
        !msg.contains("HOME") && !msg.contains("XDG"),
        "must not surface the no-config-dir string: {msg}"
    );
    assert!(
        !msg.contains(&dir_str),
        "the cause must be path-free: {msg}"
    );
    assert!(
        msg.contains("reloads without restart"),
        "expected the recovery hint, got: {msg}"
    );
}

#[tokio::test]
async fn degraded_store_surfaces_schema_mismatch_cause() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let dir_str = dir.path().to_string_lossy().to_string();
    let path = dir.path().join("credentials.json");
    write_creds_0600(&path, br#"{"schema_version":99,"providers":{}}"#);

    // Act
    let store = OAuthStore::open_or_degraded(&path).await.unwrap();
    let msg = store
        .get(&oauth_ref("anthropic"))
        .await
        .unwrap_err()
        .to_string();

    // Assert: the schema class carries the version numbers (permitted)
    // and the re-login guidance, but no filesystem path.
    assert!(
        msg.contains("schema is v99"),
        "expected the found version, got: {msg}"
    );
    assert!(
        msg.contains("expects v1"),
        "expected the wanted version, got: {msg}"
    );
    assert!(
        msg.contains("routectl login"),
        "expected the re-login guidance, got: {msg}"
    );
    assert!(
        !msg.contains(&dir_str),
        "the cause must be path-free: {msg}"
    );
    assert!(
        msg.contains("reloads without restart"),
        "expected the recovery hint, got: {msg}"
    );
}

#[tokio::test]
async fn degraded_store_refuses_all_writes_and_preserves_file() {
    // Arrange: a corrupt file the store could not read.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    write_creds_0600(&path, b"<<corrupt-json>>");
    let before = std::fs::read(&path).unwrap();
    let store = OAuthStore::open_or_degraded(&path).await.unwrap();

    // Act / Assert: every mutation is refused with the degrade cause --
    // a store that could not READ the file must never OVERWRITE it.
    assert!(
        matches!(
            store
                .write_record("anthropic", rec_at(unix_now() + 3600))
                .await,
            Err(OAuthError::Degraded(_))
        ),
        "write_record must be refused on a degraded store"
    );
    assert!(
        matches!(
            store.remove_provider("anthropic").await,
            Err(OAuthError::Degraded(_))
        ),
        "remove_provider must be refused on a degraded store"
    );
    assert!(
        matches!(
            store.set_cloud_project_id("anthropic", "projects/x").await,
            Err(OAuthError::Degraded(_))
        ),
        "set_cloud_project_id must be refused on a degraded store"
    );

    // The unreadable file must be byte-identical -- no clobber.
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a degraded store must not overwrite the file it could not read"
    );
}

#[tokio::test]
async fn corrupt_file_hot_reloads_without_restart() {
    // Arrange: a corrupt file -> degraded store.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    write_creds_0600(&path, b"<<corrupt-json>>");
    let store = OAuthStore::open_or_degraded(&path).await.unwrap();
    // Degraded before the fix: the request errors.
    assert!(
        store.get(&oauth_ref("anthropic")).await.is_err(),
        "a degraded store must error before the file is fixed"
    );

    // Act: the operator fixes the file with a valid, fresh (not
    // near-expiry, so no network refresh) seat, then the existing
    // reload path runs -- exactly what the file-watch coordinator does.
    let valid = serde_json::json!({
        "schema_version": 1,
        "providers": {
            "anthropic": {
                "access_token": "tok-recovered",
                "refresh_token": "rtok",
                "token_type": "Bearer",
                "expires_at_unix": unix_now() + 3600,
                "scopes": ["user:inference"],
                "obtained_at_unix": unix_now()
            }
        }
    });
    write_creds_0600(&path, &serde_json::to_vec_pretty(&valid).unwrap());
    store.reload_from_disk().await.unwrap();

    // Assert: recovered WITHOUT a restart -- the same handle resolves.
    let tok = store.get(&oauth_ref("anthropic")).await.unwrap();
    assert_eq!(tok, "tok-recovered");
}

#[tokio::test]
async fn open_or_degraded_missing_file_is_not_degraded() {
    // A missing file is first-run, NOT a degrade: the request surfaces
    // the normal NotLoggedIn guidance, not a degrade cause.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let store = OAuthStore::open_or_degraded(&path).await.unwrap();

    let msg = store
        .get(&oauth_ref("anthropic"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("no credentials"),
        "first-run must be NotLoggedIn, got: {msg}"
    );
    assert!(
        !msg.contains("reloads without restart"),
        "a missing file is not a degrade: {msg}"
    );
}

#[tokio::test]
async fn open_or_degraded_valid_file_resolves_like_open() {
    // A clean file loads live: reads resolve exactly as `open` would.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let seed = OAuthStore::open(&path).await.unwrap();
    seed.write_record("anthropic", rec_named("tok-live", unix_now() + 3600))
        .await
        .unwrap();
    drop(seed);

    let store = OAuthStore::open_or_degraded(&path).await.unwrap();
    let tok = store.get(&oauth_ref("anthropic")).await.unwrap();
    assert_eq!(tok, "tok-live");
}

#[tokio::test]
#[serial_test::serial]
async fn open_default_degradable_yields_no_config_dir_without_home_or_xdg() {
    // Arrange: neither HOME nor XDG_CONFIG_HOME set -- the one case
    // that drops the oauth arm entirely.
    let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
    let prev_home = std::env::var_os("HOME");
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe {
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("HOME");
    }

    // Act
    let outcome = OAuthStore::open_default_degradable().await;

    // Restore env BEFORE asserting so a failure cannot leak into
    // sibling serial tests.
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe {
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    // Assert
    assert!(
        matches!(outcome.unwrap(), OpenOutcome::NoConfigDir),
        "no HOME/XDG must yield NoConfigDir, not a degraded Present store"
    );
}
