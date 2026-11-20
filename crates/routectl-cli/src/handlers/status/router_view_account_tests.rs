//! The per-account facade reads: pool membership, credential state,
//! managed accounts and the subscription flag, each checked against the
//! shared function it must agree with.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use routectl_auth::{OAuthStore, SecretRef, SecretStore};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, UpstreamReason};
use routectl_router::runtime_state::UnusableCause;
use routectl_router::{CURRENT_CONFIG_VERSION, Config, ResolvedModel, Router};
use routectl_testkit::ScopedEnv;
use serde_json::Value;

use super::{
    AccountCredential, CredentialCondition, CredentialState, StatusRouterHandle, StatusRouterView,
};
use crate::commands::pricing::is_subscription;
use crate::server::seat_driver::managed_seats;

struct Unused;

#[async_trait::async_trait]
impl Provider for Unused {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> routectl_core::Result<Value> {
        Ok(Value::Null)
    }
    fn normalize_response(&self, _: Value) -> routectl_core::Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> routectl_core::Result<ChatResponse> {
        Err(Error::upstream("p", 500, "unused"))
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> routectl_core::Result<BoxStream<'static, routectl_core::Result<ChatChunk>>> {
        Err(Error::upstream("p", 500, "unused"))
    }
}

const ANTHROPIC: &str = "format = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\nauth = \"bearer\"\n";
const CODEX: &str = "format = \"openai-responses\"\nbase_url = \
                     \"https://chatgpt.com/backend-api/codex\"\nauth = \"bearer\"\n";
const COMPAT: &str =
    "format = \"openai-chat\"\nbase_url = \"https://gamma.example.com\"\nauth = \"bearer\"\n";
const CLOUD: &str = "format = \"bedrock-invoke\"\ncloud = { name = \"aws\", region = \
                     \"us-west-2\" }\nauth = \"aws-sigv4\"\n";

/// `(model nickname, provider entry, provider block, credential ref)`.
type Seat = (&'static str, &'static str, &'static str, &'static str);

/// One non-pooled model per seat: a labeled Anthropic login named by two
/// entries, an unlabeled Codex login, an `env://` key and an AWS cloud
/// credential.
const DIRECT: [Seat; 5] = [
    ("m-z", "seat-z", ANTHROPIC, "oauth://anthropic#work"),
    ("m-b", "seat-b", ANTHROPIC, "oauth://anthropic#work"),
    ("m-codex", "codex-one", CODEX, "oauth://codex"),
    ("m-env", "gamma", COMPAT, "env://GAMMA_KEY_FOR_TESTS"),
    (
        "m-cloud",
        "bedrock-west",
        CLOUD,
        "aws://profile/test-profile",
    ),
];

fn config_text(seats: &[Seat], extra: &str) -> String {
    let mut text = format!("version = {CURRENT_CONFIG_VERSION}\n");
    let mut entries = BTreeSet::new();
    for (_, entry, block, uri) in seats {
        if entries.insert(*entry) {
            text.push_str(&format!(
                "[providers.{entry}]\n{block}credential = \"{uri}\"\n"
            ));
        }
    }
    for (nickname, entry, _, _) in seats {
        text.push_str(&format!(
            "[models.{nickname}]\nprovider = \"{entry}\"\nupstream = \"u\"\n"
        ));
    }
    text.push_str(extra);
    text
}

/// A router over one hand-installed non-pooled model per seat.
fn direct_router(seats: &[Seat]) -> Router {
    let config: Config = toml::from_str(&config_text(seats, "")).expect("fixture config parses");
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(Unused);
    let models: BTreeMap<String, Arc<ResolvedModel>> = seats
        .iter()
        .map(|(nickname, entry, _, uri)| {
            let model = ResolvedModel::new(*nickname, *entry, Arc::clone(&provider), "u");
            let model = if uri.starts_with("aws://") {
                model
            } else {
                model.with_auth_secret_ref(SecretRef::parse(uri).expect("fixture ref parses"))
            };
            ((*nickname).to_string(), Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    router
}

fn view_of(router: Router) -> StatusRouterView {
    StatusRouterHandle::new(Arc::new(ArcSwap::from_pointee(router))).view()
}

fn now() -> (Instant, i64) {
    (
        Instant::now(),
        crate::local_time::unix_ms(time::OffsetDateTime::now_utc()),
    )
}

/// The unusable cause token, or the park reason token.
const fn cause_token(credential: &AccountCredential) -> Option<&'static str> {
    match credential.condition {
        CredentialCondition::Usable => None,
        CredentialCondition::Unusable { cause, .. } => Some(cause.as_str()),
        CredentialCondition::Parked { reason, .. } => Some(reason.as_str()),
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

fn write_logins(path: &std::path::Path, seats: &[&str]) {
    let expires = routectl_auth::oauth::unix_now() + 3600;
    let record = serde_json::json!({
        "access_token": "tok",
        "refresh_token": "seeded-refresh-token",
        "token_type": "Bearer",
        "expires_at_unix": expires,
        "scopes": ["user:inference"],
        "account": { "email": null, "account_id": null },
        "obtained_at_unix": 0
    });
    let providers: serde_json::Map<String, Value> = seats
        .iter()
        .map(|seat| ((*seat).to_string(), record.clone()))
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

/// Two pools over three anthropic logins plus a direct entry: `alpha` holds
/// `a1` (login `anthropic#shared`) and `b1` (login `anthropic#b`), `beta`
/// holds `a2` (login `anthropic#shared` again). Direct model `m-a3` names
/// `a3` (login `anthropic#shared`, in no pool), and `m-solo` names `solo`
/// (login `anthropic#solo`, in no pool).
fn pooled_config_text() -> String {
    let mut text = format!("version = {CURRENT_CONFIG_VERSION}\n");
    for (entry, label) in [
        ("a1", "shared"),
        ("a2", "shared"),
        ("a3", "shared"),
        ("b1", "b"),
        ("solo", "solo"),
    ] {
        text.push_str(&format!(
            "[providers.{entry}]\n{ANTHROPIC}credential = \"oauth://anthropic#{label}\"\n"
        ));
    }
    text.push_str(
        "[pools.beta]\nmembers = [\"a2\"]\n\
         [pools.alpha]\nmembers = [\"a1\", \"b1\"]\n\
         [models.m-alpha]\nprovider = \"alpha\"\nupstream = \"claude-sonnet-4-6\"\n\
         [models.m-beta]\nprovider = \"beta\"\nupstream = \"claude-sonnet-4-6\"\n\
         [models.m-a3]\nprovider = \"a3\"\nupstream = \"claude-sonnet-4-6\"\n\
         [models.m-solo]\nprovider = \"solo\"\nupstream = \"claude-sonnet-4-6\"\n",
    );
    text
}

/// An account named by several entries lists every pool any of those
/// entries belongs to, sorted; an account in no pool lists none. Built
/// through the real factory, so pool seats carry the accounts dispatch
/// keys on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn account_pools_lists_every_pool_naming_the_account_sorted() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let creds = dir.path().join("routectl").join("credentials.json");
    write_logins(
        &creds,
        &["anthropic#shared", "anthropic#b", "anthropic#solo"],
    );
    let store = OAuthStore::open(&creds).await.expect("open the store");
    let secrets: Arc<dyn SecretStore> = Arc::new(store);
    let config = routectl_router::parse_config(&pooled_config_text()).expect("fixture parses");
    let router = crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("fixture builds");
    let view = view_of(router);

    // Act
    let pools = view.account_pools();

    // Assert
    let expected: BTreeMap<String, Vec<String>> = [
        ("anthropic#shared", strings(&["alpha", "beta"])),
        ("anthropic#b", strings(&["alpha"])),
        ("anthropic#solo", Vec::new()),
    ]
    .into_iter()
    .map(|(account, pools)| (account.to_string(), pools))
    .collect();
    assert_eq!(pools, expected);
}

/// A pool member whose provider did not build still places its account in
/// the pool: membership follows the served config's identity rule, not the
/// built targets. The router here built no target at all.
#[test]
fn account_pools_places_an_account_whose_provider_did_not_build() {
    // Arrange
    let config: Config = toml::from_str(&pooled_config_text()).expect("fixture config parses");
    let router = Router::new(Arc::new(config));
    assert!(
        router.status_targets(Instant::now()).is_empty(),
        "premise: no provider built"
    );
    let view = view_of(router);

    // Act
    let pools = view.account_pools();

    // Assert
    let expected: BTreeMap<String, Vec<String>> = [
        ("anthropic#shared", strings(&["alpha", "beta"])),
        ("anthropic#b", strings(&["alpha"])),
        ("anthropic#solo", Vec::new()),
    ]
    .into_iter()
    .map(|(account, pools)| (account.to_string(), pools))
    .collect();
    assert_eq!(pools, expected);
}

/// A direct-model router with no `[pools]` lists every configured account,
/// each with no pool, including the cloud and API-key accounts.
#[test]
fn account_pools_lists_every_account_with_none_when_no_pool_is_configured() {
    // Arrange
    let view = view_of(direct_router(&DIRECT));

    // Act
    let pools = view.account_pools();

    // Assert
    let accounts: Vec<&str> = pools.keys().map(String::as_str).collect();
    assert_eq!(
        accounts,
        ["#bedrock-west", "#gamma", "anthropic#work", "codex"]
    );
    assert!(pools.values().all(Vec::is_empty), "{pools:?}");
}

/// A logged-out seat reads unusable with its cause token, the epoch-ms it was
/// first marked, and the shared fix split: a bare command or a correction.
#[test]
fn a_logged_out_seat_reads_unusable_with_cause_since_and_the_shared_fix() {
    // Arrange
    let router = direct_router(&DIRECT);
    let before_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    assert!(router.mark_credential_unusable_for_tests("m-b", UnusableCause::NotLoggedIn));
    assert!(router.mark_credential_unusable_for_tests("m-env", UnusableCause::Unreadable));
    let view = view_of(router);

    // Act
    let credentials = view.account_credentials(now());
    let after_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());

    // Assert
    for (account, entry, cause) in [
        ("anthropic#work", "seat-b", "not_logged_in"),
        ("#gamma", "gamma", "unreadable"),
    ] {
        let credential = &credentials[account];
        assert_eq!(credential.state(), CredentialState::Unusable, "{account}");
        assert_eq!(cause_token(credential), Some(cause), "{account}");
        assert_eq!(credential.until_ms(), None, "{account}");
        let since = credential.since_ms().expect("an unusable mark has a since");
        assert!(
            (before_ms..=after_ms).contains(&since),
            "{account}: {before_ms} <= {since} <= {after_ms}"
        );
        let fix = view.seat_fix(entry);
        assert_eq!(credential.fix, fix.bare_command(), "{account}");
        assert_eq!(credential.remediation, fix.correction(), "{account}");
    }
    assert_eq!(
        credentials["anthropic#work"].fix.as_deref(),
        Some("routectl login anthropic --label work"),
        "the shared fix is the exact login command"
    );
    assert_eq!(credentials["anthropic#work"].remediation, None);
    assert_eq!(credentials["#gamma"].fix, None, "a key has no command");
    assert!(
        credentials["#gamma"].remediation.is_some(),
        "a key carries its correction"
    );
    assert_eq!(credentials["codex"], AccountCredential::usable());
}

/// A parked account reads parked with its reason, the epoch-ms the park
/// lapses and no since; a usable account reads usable with every other field
/// null. A park whose deadline has passed is a re-test and reads usable.
#[test]
fn a_parked_account_reads_parked_with_until_and_a_usable_one_reads_bare() {
    // Arrange
    let router = direct_router(&DIRECT);
    let before_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    assert!(router.park_credential_for_tests(
        "m-codex",
        UpstreamReason::CredentialInvalid,
        Instant::now() + Duration::from_mins(10),
    ));
    assert!(router.park_credential_for_tests(
        "m-env",
        UpstreamReason::CredentialExpired,
        Instant::now(),
    ));
    let view = view_of(router);

    // Act
    let credentials = view.account_credentials(now());
    let after_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());

    // Assert
    let parked = &credentials["codex"];
    assert_eq!(parked.state(), CredentialState::Parked);
    assert_eq!(cause_token(parked), Some("credential-invalid"));
    assert_eq!(parked.since_ms(), None);
    assert_eq!(parked.fix, None);
    assert_eq!(parked.remediation, None);
    let until = parked.until_ms().expect("a park has a deadline");
    assert!(
        (before_ms + 590_000..=after_ms + 600_000).contains(&until),
        "{until} is about ten minutes out"
    );
    assert_eq!(credentials["#gamma"], AccountCredential::usable(), "lapsed");
    assert_eq!(credentials["anthropic#work"], AccountCredential::usable());
    assert_eq!(credentials["anthropic#work"].state().as_str(), "usable");
    assert_eq!(parked.state().as_str(), "parked");
    assert_eq!(CredentialState::Unusable.as_str(), "unusable");
}

/// An account both parked and marked unusable reads unusable, as eligibility
/// ranks it.
#[test]
fn an_unusable_mark_outranks_a_live_park() {
    // Arrange
    let router = direct_router(&DIRECT);
    assert!(router.park_credential_for_tests(
        "m-codex",
        UpstreamReason::CredentialInvalid,
        Instant::now() + Duration::from_mins(10),
    ));
    assert!(router.mark_credential_unusable_for_tests("m-codex", UnusableCause::Revoked));
    let view = view_of(router);

    // Act
    let credential = view.account_credentials(now()).remove("codex");

    // Assert
    let credential = credential.expect("codex is listed");
    assert_eq!(credential.state(), CredentialState::Unusable);
    assert_eq!(cause_token(&credential), Some("revoked"));
    assert_eq!(credential.until_ms(), None);
}

/// The subscription flag is true for an `oauth://` account and false for the
/// API-key and cloud accounts, matching the shared cost predicate on each
/// account's reported entry.
#[test]
fn subscription_is_true_for_oauth_and_false_for_api_key_and_cloud() {
    // Arrange
    let router = direct_router(&DIRECT);
    let config: Config = toml::from_str(&config_text(&DIRECT, "")).expect("parses");
    let view = view_of(router);

    // Act
    let subscriptions = view.account_subscriptions();

    // Assert
    let expected: BTreeMap<String, bool> = [
        ("#bedrock-west", false),
        ("#gamma", false),
        ("anthropic#work", true),
        ("codex", true),
    ]
    .into_iter()
    .map(|(account, flag)| (account.to_string(), flag))
    .collect();
    assert_eq!(subscriptions, expected);
    for (account, entry) in [
        ("#bedrock-west", "bedrock-west"),
        ("#gamma", "gamma"),
        ("anthropic#work", "seat-b"),
        ("codex", "codex-one"),
    ] {
        assert_eq!(
            subscriptions[account],
            is_subscription(&config, entry),
            "{account}"
        );
    }
}

/// The managed-account set is exactly the accounts of the managed seats the
/// background drivers work on: the Anthropic and Codex logins, not the
/// API-key or cloud accounts.
#[test]
fn managed_accounts_equal_the_managed_seats_accounts() {
    // Arrange
    let view = view_of(direct_router(&DIRECT));
    let config: Config = toml::from_str(&config_text(&DIRECT, "")).expect("parses");
    let expected: BTreeSet<String> = managed_seats(&config)
        .into_iter()
        .map(|seat| seat.account)
        .collect();

    // Act
    let managed = view.managed_accounts();

    // Assert
    assert_eq!(managed, expected);
    assert_eq!(
        managed.into_iter().collect::<Vec<_>>(),
        strings(&["anthropic#work", "codex"]),
        "control: the fixture has managed and unmanaged accounts"
    );
}
