//! A reload that publishes a router in which a seat the live router held as a
//! placeholder is built readmits that seat at its commit point, whatever the
//! reload changed: the build mark lapses even when the seat's `[providers]`
//! entry and its stored credential are as they were.

use super::*;
use routectl_auth::oauth::testing::{ScriptedRefresh, ScriptedRefreshFlow};
use routectl_router::runtime_state::UnusableCause;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const KEY_ENV: &str = "ROUTECTL_PLACEHOLDER_LAPSE_TEST_KEY";

/// The unusable cause `router` reports on `state_key`.
fn unusable_cause(router: &Router, state_key: &str) -> Option<UnusableCause> {
    router
        .status_targets(Instant::now())
        .into_iter()
        .find(|target| target.state_key == state_key)
        .unwrap_or_else(|| panic!("no status target {state_key}"))
        .gate
        .credential_unusable
        .map(|mark| mark.cause)
}

/// The config reload rig: a config file, the secrets over one credentials
/// file, and the live router swap.
struct Rig {
    creds: PathBuf,
    cfg_path: PathBuf,
    oauth: Arc<routectl_auth::OAuthStore>,
    secrets: Arc<dyn SecretStore>,
    config: Arc<Config>,
    swap: Arc<ArcSwap<Router>>,
    _usage_dir: tempfile::TempDir,
}

impl Rig {
    /// The rig over the composite store, with `seats` stored fresh.
    async fn new(dir: &Path, config_text: &str, seats: &[(&str, &str)]) -> Self {
        let creds = dir.join("routectl").join("credentials.json");
        write_pool_credentials(&creds, seats);
        let composite = CompositeStore::open_at(&creds)
            .await
            .expect("open composite store at temp creds path");
        let oauth = composite.oauth_store().expect("oauth arm present");
        Self::over(dir, config_text, creds, oauth, Arc::new(composite)).await
    }

    /// The rig over a store whose refreshes `flow` answers, the store
    /// doubling as the secrets the build probes. The caller has written the
    /// credentials file.
    async fn scripted(dir: &Path, config_text: &str, flow: Arc<ScriptedRefreshFlow>) -> Self {
        let creds = dir.join("routectl").join("credentials.json");
        let oauth = Arc::new(
            routectl_auth::OAuthStore::open_with_scripted_refresh_for_tests(&creds, flow)
                .await
                .expect("open the scripted credentials store"),
        );
        let secrets: Arc<dyn SecretStore> = Arc::new((*oauth).clone());
        Self::over(dir, config_text, creds, oauth, secrets).await
    }

    async fn over(
        dir: &Path,
        config_text: &str,
        creds: PathBuf,
        oauth: Arc<routectl_auth::OAuthStore>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, config_text).unwrap();
        let mut config = routectl_router::parse_config(config_text).expect("fixture parses");
        let usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let router =
            build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
                .await
                .expect("initial router build");
        Self {
            creds,
            cfg_path,
            oauth,
            secrets,
            config,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            _usage_dir: usage_dir,
        }
    }

    async fn reload_credentials(&self) {
        handle_credentials_reload(
            &Some(self.oauth.clone()),
            &self.config,
            &Arc::new(CatalogOverlay::default()),
            self.secrets.clone(),
            &self.swap,
            &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
        )
        .await;
    }

    /// A SIGHUP with config.toml unchanged: a config reload, then, when it
    /// committed, a credentials reload.
    async fn sighup(&self) -> Result<(), ReloadFailure> {
        let (usage, writer) = build_usage_writer(&self.config);
        let outcome = self.reload_config(&usage).await;
        drop(usage);
        drain_usage_writer_strict(writer).await;
        if outcome.is_ok() {
            self.reload_credentials().await;
        }
        outcome.map(|_| ())
    }

    async fn reload_config(
        &self,
        usage: &routectl_usage::UsageHandle,
    ) -> Result<Option<(Arc<Config>, Arc<CatalogOverlay>)>, ReloadFailure> {
        Box::pin(handle_config_reload(
            Some(&self.cfg_path),
            &self.config,
            self.secrets.clone(),
            &self.swap,
            usage,
            ReloadTrigger::ConfigFile,
            &mut never_shutdown(),
        ))
        .await
    }

    /// The seat's mark and whether it admits a client attempt, as the
    /// published router reports them.
    fn seat(&self, state_key: &str) -> (Option<UnusableCause>, bool) {
        let router = self.swap.load();
        (
            unusable_cause(&router, state_key),
            router.admit_for_credential_tests(state_key).is_some(),
        )
    }
}

/// Pool `the-pool` of OAuth seats `anthropic#a` and `anthropic#b` behind
/// model `claude`, plus direct model `glm` on an env key that alias `fast`
/// routes, so a rebuild without that key fails.
fn oauth_pool_config_text() -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\nhost = \"127.0.0.1\"\nport = 0\n\n"
    );
    for seat in ["a", "b"] {
        text.push_str(&format!(
            "[providers.seat_{seat}]\nkind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic#{seat}\"\nauth_kind = \"oauth-bearer\"\n\n"
        ));
    }
    text.push_str(&format!(
        "[providers.compat]\nkind = \"openai-compat\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\
         api_key_ref = \"env://{KEY_ENV}\"\n\n\
         [pools.the-pool]\nmembers = [\"seat_a\", \"seat_b\"]\n\n\
         [models.claude]\nprovider = \"the-pool\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.glm]\nprovider = \"compat\"\nupstream = \"glm-4.6\"\n\n\
         [aliases]\nfast = \"glm\"\n"
    ));
    text
}

/// Seat `a` is logged in while the env key is missing, so the credentials
/// reload loads its record but its rebuild fails and the live router keeps
/// seat `a` as a placeholder. The operator restores the key and sends SIGHUP:
/// config.toml and the credentials file are unchanged, so neither entry nor
/// record lift applies, and only the build readmits seat `a`.
#[tokio::test]
#[serial_test::serial]
async fn a_sighup_after_a_failed_credentials_rebuild_readmits_the_seat_it_builds() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let key = ScopedEnv::set(KEY_ENV, "test-key");
    let rig = Rig::new(
        dir.path(),
        &oauth_pool_config_text(),
        &[("anthropic#b", "tok-b")],
    )
    .await;
    assert_eq!(
        rig.seat("claude#seat_a"),
        (Some(UnusableCause::NotLoggedIn), false),
        "premise: seat a is a placeholder"
    );
    drop(key);
    write_pool_credentials(
        &rig.creds,
        &[("anthropic#a", "tok-a"), ("anthropic#b", "tok-b")],
    );
    let live = rig.swap.load_full();
    rig.reload_credentials().await;
    assert!(
        Arc::ptr_eq(&live, &rig.swap.load_full()),
        "premise: the credentials rebuild failed"
    );
    assert!(
        rig.swap.load().is_placeholder_account("anthropic#a"),
        "premise: seat a is still a placeholder"
    );
    let _key = ScopedEnv::set(KEY_ENV, "test-key");

    // Act
    let outcome = rig.sighup().await;

    // Assert
    assert_eq!(outcome, Ok(()));
    assert!(
        !rig.swap.load().is_placeholder_account("anthropic#a"),
        "the committed reload publishes the build of seat a"
    );
    assert_eq!(rig.seat("claude#seat_a"), (None, true));
    assert_eq!(
        rig.seat("claude#seat_b"),
        (None, true),
        "a seat built throughout is untouched"
    );
}

/// Pool `the-pool` of OAuth seats `seat_a` and `seat_b` behind model
/// `claude`, and nothing else, so a store holding only OAuth records serves
/// every credential the build reads.
fn pool_only_config_text() -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\nhost = \"127.0.0.1\"\nport = 0\n\n"
    );
    for seat in ["a", "b"] {
        text.push_str(&format!(
            "[providers.seat_{seat}]\nkind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic#{seat}\"\nauth_kind = \"oauth-bearer\"\n\n"
        ));
    }
    text.push_str(
        "[pools.the-pool]\nmembers = [\"seat_a\", \"seat_b\"]\n\n\
         [models.claude]\nprovider = \"the-pool\"\nupstream = \"claude-sonnet-4-6\"\n",
    );
    text
}

/// Seat `a`'s stored access token has expired and its refresh fails in
/// transit at boot, so the build keeps it as a placeholder. The token
/// endpoint recovers, its refresh cooldown passes, and the operator sends SIGHUP with config.toml and the
/// stored record unchanged: no entry changed and no record changed, so only
/// the build, which now refreshes the record, readmits seat `a`.
#[tokio::test]
#[serial_test::serial]
async fn a_sighup_after_a_failed_boot_refresh_readmits_the_seat_it_builds() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let creds = dir.path().join("routectl").join("credentials.json");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_pool_credentials_at(&creds, &[("anthropic#b", "tok-b")], now);
    expire_seat_a(&creds, now);
    let flow = Arc::new(ScriptedRefreshFlow::new(ScriptedRefresh::Transient));
    let rig = Rig::scripted(dir.path(), &pool_only_config_text(), Arc::clone(&flow)).await;
    rig.oauth.set_cooldown_clock_for_tests(now);
    assert_eq!(
        rig.seat("claude#seat_a"),
        (Some(UnusableCause::Unreadable), false),
        "premise: seat a is a placeholder"
    );
    assert!(
        flow.post_count() >= 1,
        "premise: the build tried to refresh seat a"
    );
    flow.set_script(ScriptedRefresh::Mint("tok-a-refreshed".into()));
    rig.oauth.set_cooldown_clock_for_tests(now + 3600);

    // Act
    let outcome = rig.sighup().await;

    // Assert
    assert_eq!(outcome, Ok(()));
    assert_eq!(rig.seat("claude#seat_a"), (None, true));
    assert_eq!(
        rig.seat("claude#seat_b"),
        (None, true),
        "a seat built throughout is untouched"
    );
}

/// Add seat `a` to the credentials file at `creds` with an access token that
/// expired an hour before `now`.
fn expire_seat_a(creds: &Path, now: u64) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(creds).expect("read creds")).expect("parse creds");
    doc["providers"]["anthropic#a"] = serde_json::json!({
        "access_token": "tok-a-expired",
        "refresh_token": "seeded-refresh-token",
        "token_type": "Bearer",
        "expires_at_unix": now - 3600,
        "scopes": ["user:inference"],
        "account": { "email": null, "account_id": null },
        "obtained_at_unix": now - 7200
    });
    std::fs::write(
        creds,
        serde_json::to_vec_pretty(&doc).expect("serialize creds"),
    )
    .expect("write creds");
}
