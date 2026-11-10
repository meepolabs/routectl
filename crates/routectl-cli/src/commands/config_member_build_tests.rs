//! `config check`'s pool-member build: it refuses what serve refuses, builds
//! what serve builds, and never refreshes a credential while doing so.

use std::path::Path;
use std::sync::Arc;

use routectl_auth::oauth::testing::{ScriptedRefresh, ScriptedRefreshFlow};
use routectl_auth::oauth::unix_now;
use routectl_auth::{OAuthStore, SecretRef, SecretStore};
use routectl_router::{BuildOptions, Config, build_resolved_models_reported};
use routectl_testkit::ScopedEnv;

use super::{OfflineCheckStore, member_build_errors};

/// One stored record: its access token, expiry and account id.
fn record(token: &str, expires_at_unix: u64, account_id: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "access_token": token,
        "refresh_token": "seeded-refresh-token",
        "token_type": "Bearer",
        "expires_at_unix": expires_at_unix,
        "scopes": [],
        "account": { "email": null, "account_id": account_id },
        "obtained_at_unix": expires_at_unix.saturating_sub(7200)
    })
}

fn write_credentials(path: &Path, records: &[(&str, serde_json::Value)]) {
    let providers: serde_json::Map<String, serde_json::Value> = records
        .iter()
        .map(|(key, rec)| ((*key).to_string(), rec.clone()))
        .collect();
    let doc = serde_json::json!({ "schema_version": 1, "providers": providers });
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

/// Two anthropic seats pooled behind model `opus`.
const ANTHROPIC_POOL: &str = "[providers.seat-a]\n\
     kind = \"anthropic-api\"\n\
     api_key_ref = \"oauth://anthropic#seat-a\"\n\
     auth_kind = \"oauth-bearer\"\n\
     [providers.seat-b]\n\
     kind = \"anthropic-api\"\n\
     api_key_ref = \"oauth://anthropic#seat-b\"\n\
     auth_kind = \"oauth-bearer\"\n\
     [pools.alpha]\n\
     members = [\"seat-a\", \"seat-b\"]\n\
     [models.opus]\n\
     provider = \"alpha\"\n\
     upstream = \"claude-opus-4-7\"\n";

/// One Codex seat pooled behind model `gpt`.
const CODEX_POOL: &str = "[providers.seat-c]\n\
     kind = \"openai-responses\"\n\
     api_key_ref = \"oauth://codex#seat-c\"\n\
     [pools.beta]\n\
     members = [\"seat-c\"]\n\
     [models.gpt]\n\
     provider = \"beta\"\n\
     upstream = \"gpt-5-codex\"\n";

struct Rig {
    flow: Arc<ScriptedRefreshFlow>,
    oauth: OAuthStore,
    _dir: tempfile::TempDir,
    _xdg: ScopedEnv,
}

impl Rig {
    async fn new(records: &[(&str, serde_json::Value)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let creds = dir.path().join("routectl").join("credentials.json");
        write_credentials(&creds, records);
        // Any refresh POST lands here and is counted; none should reach it.
        let flow = Arc::new(ScriptedRefreshFlow::new(ScriptedRefresh::Mint(
            "minted".into(),
        )));
        let oauth = OAuthStore::open_with_scripted_refresh_for_tests(&creds, Arc::clone(&flow))
            .await
            .expect("open the scripted credentials store");
        Self {
            flow,
            oauth,
            _dir: dir,
            _xdg: xdg,
        }
    }

    fn offline(&self) -> Arc<dyn SecretStore> {
        Arc::new(OfflineCheckStore::over(Some(self.oauth.clone())))
    }

    fn live(&self) -> Arc<dyn SecretStore> {
        Arc::new(self.oauth.clone())
    }
}

fn parse(text: &str) -> Config {
    toml::from_str(text).expect("fixture parses")
}

#[tokio::test]
#[serial_test::serial]
async fn a_valid_pool_passes_check_and_its_members_build() {
    // Arrange
    let fresh = unix_now() + 3600;
    let rig = Rig::new(&[
        ("anthropic#seat-a", record("tok-a", fresh, None)),
        ("anthropic#seat-b", record("tok-b", fresh, None)),
    ])
    .await;
    let config = parse(ANTHROPIC_POOL);

    // Act
    let report = super::super::config::validation_report(&config, Some(ANTHROPIC_POOL));
    let refusals = member_build_errors(&config, rig.offline()).await;
    let built = build_resolved_models_reported(&config, rig.live(), BuildOptions::default())
        .await
        .expect("serve builds the pool");

    // Assert
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(refusals.is_empty(), "{refusals:?}");
    let report = built
        .pool_reports
        .iter()
        .find(|r| r.pool == "alpha")
        .expect("the pool reports");
    assert_eq!(report.configured_members, 2);
    assert_eq!(report.usable_members, 2, "both members built");
    assert!(report.placeholders.is_empty(), "{:?}", report.placeholders);
}

#[tokio::test]
#[serial_test::serial]
async fn check_refuses_a_member_whose_provider_fails_to_build_as_serve_does() {
    // Arrange: a Codex seat that is logged in but carries no account id, so
    // its credential probe passes and its provider cannot be built.
    let rig = Rig::new(&[("codex#seat-c", record("tok-c", unix_now() + 3600, None))]).await;
    let config = parse(CODEX_POOL);

    // Act
    let check = member_build_errors(&config, rig.offline()).await;
    let serve = crate::server::build_router_from_config(Arc::new(config), rig.live())
        .await
        .err()
        .map(|e| e.to_string());

    // Assert: one refusal naming the pool, the entry and the rule, and the
    // same text serve refuses to start with.
    assert_eq!(check.len(), 1, "{check:?}");
    let line = &check[0];
    assert!(line.contains("pool `beta`"), "{line}");
    assert!(
        line.contains(
            "member `seat-c` has a readable credential but its provider could not be built"
        ),
        "{line}"
    );
    assert!(!line.contains("tok-c"), "{line}");
    assert_eq!(serve.as_deref(), Some(line.as_str()));
}

#[tokio::test]
#[serial_test::serial]
async fn check_keeps_a_logged_out_member_as_serve_does() {
    // Arrange: seat-b is never stored.
    let rig = Rig::new(&[("anthropic#seat-a", record("tok-a", unix_now() + 3600, None))]).await;
    let config = parse(ANTHROPIC_POOL);

    // Act
    let refusals = member_build_errors(&config, rig.offline()).await;

    // Assert: a logged-out member is an unusable seat, not a refusal.
    assert!(refusals.is_empty(), "{refusals:?}");
}

#[tokio::test]
#[serial_test::serial]
async fn the_check_build_never_refreshes_a_due_credential() {
    // Arrange: both seats' access tokens expired, refresh tokens stored, so
    // any read through the live store would refresh them.
    let expired = unix_now() - 60;
    let records = [
        ("anthropic#seat-a", record("tok-a", expired, None)),
        ("anthropic#seat-b", record("tok-b", expired, None)),
    ];
    let config = parse(ANTHROPIC_POOL);
    let check_rig = Rig::new(&records).await;

    // Act
    let refusals = member_build_errors(&config, check_rig.offline()).await;
    let check_posts = check_rig.flow.post_count();
    drop(check_rig);
    let control_rig = Rig::new(&records).await;
    let probed = control_rig
        .live()
        .probe_credential(&SecretRef::parse("oauth://anthropic#seat-a").expect("ref"))
        .await;

    // Assert: the check built both members with no refresh POST, while the
    // same record through the live store's probe does POST, so a zero here is
    // a property of the check store and not of the fixture.
    assert!(refusals.is_empty(), "{refusals:?}");
    assert_eq!(check_posts, 0, "config check must not refresh a credential");
    assert_eq!(probed, Ok(()));
    assert_eq!(
        control_rig.flow.post_count(),
        1,
        "control: the live probe refreshes"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn the_check_build_mints_no_installation_id_while_the_serve_build_does() {
    // Arrange: a Codex seat logged in with an account id, so its provider
    // builds and reaches the installation-id resolution.
    let rig = Rig::new(&[(
        "codex#seat-c",
        record("tok-c", unix_now() + 3600, Some("acct-seat-c")),
    )])
    .await;
    let config = parse(CODEX_POOL);
    let id_file = rig._dir.path().join("routectl").join("installation_id");

    // Act
    let refusals = member_build_errors(&config, rig.offline()).await;
    let after_check = id_file.exists();
    let served = build_resolved_models_reported(&config, rig.live(), BuildOptions::default())
        .await
        .expect("serve builds the pool");

    // Assert: the check built the member and left no id behind; the serve
    // build over the same fixture mints one, so the absence is the check's
    // posture and not a fixture that never reaches the mint.
    assert!(refusals.is_empty(), "{refusals:?}");
    assert!(
        !after_check,
        "config check must not create an installation id"
    );
    assert!(served.failed.is_empty(), "{:?}", served.failed);
    assert!(id_file.exists(), "control: the serve build mints the id");
    assert_eq!(rig.flow.post_count(), 0, "a fresh seat needs no refresh");
}
