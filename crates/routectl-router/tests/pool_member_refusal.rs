//! Pool member refusals and placeholders at build. A member that cannot be
//! built refuses the config, naming the member and its rule. A member whose
//! credential the store refuses is kept as an unusable placeholder seat, and
//! its build diagnostic carries no store text.
//!
//! Field content is the whole point of these tests. Build output reaches
//! operator log destinations that get archived, shipped to third parties, and
//! audited -- so no store error string, credential path, token, or account id
//! may appear in ANY field, and a refusal names the member and its rule only.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_auth::{SecretRef, SecretStore};
use routectl_router::{
    BuildOptions, Config, ModelEntry, PoolEntry, ProviderEntry, build_resolved_models_reported,
};
use routectl_testkit::{CapturedEvent, with_capture};

/// A store whose `get` refuses for one named provider with a message carrying
/// exactly the kinds of bytes that must never reach a log field: a filesystem
/// path, an account id, and operator guidance naming the credential.
struct RefusingStore {
    dead_provider: String,
}

#[async_trait::async_trait]
impl SecretStore for RefusingStore {
    async fn get(&self, secret_ref: &SecretRef) -> routectl_core::Result<String> {
        if let SecretRef::OAuth { provider, .. } = secret_ref
            && *provider == self.dead_provider
        {
            return Err(routectl_core::Error::Auth(format!(
                "no credentials at /home/operator/.config/routectl/credentials.json \
                 for acct_9f3b21c8 ({provider}); run `routectl login {provider}`"
            )));
        }
        Ok("token".into())
    }
    async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
        Ok(())
    }
}

fn oauth_member(provider: &str) -> ProviderEntry {
    ProviderEntry::anthropic_api(format!("oauth://{provider}"))
        .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer)
}

/// A config whose `[pools.anthropic-pool]` groups `members`, with one
/// selectable model routed at it.
fn pooled_config(members: &[&str]) -> Config {
    let mut providers = BTreeMap::new();
    for member in members {
        providers.insert((*member).to_string(), oauth_member(member));
    }
    let mut models = BTreeMap::new();
    models.insert(
        "opus".to_string(),
        ModelEntry::new("anthropic-pool", "claude-opus-4-7"),
    );
    let mut pools = BTreeMap::new();
    pools.insert(
        "anthropic-pool".to_string(),
        PoolEntry::new(members.iter().map(|m| (*m).to_string()).collect()),
    );
    Config {
        providers,
        models,
        pools,
        ..Config::default()
    }
}

/// `pooled_config` with `member`'s entry carrying no credential reference,
/// which refuses the pool.
fn pooled_config_without_ref(members: &[&str], member: &str) -> Config {
    let mut cfg = pooled_config(members);
    cfg.providers
        .insert(member.into(), ProviderEntry::anthropic_api(""));
    cfg
}

/// Every WARN-or-louder event in a capture.
fn warnings(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.level <= tracing::Level::WARN)
        .collect()
}

#[tokio::test]
async fn a_member_with_no_credential_ref_refuses_the_build_and_warns_nothing() {
    // Arrange: two members, one of them with no credential reference.
    let store: Arc<dyn SecretStore> = Arc::new(RefusingStore {
        dead_provider: "nobody".into(),
    });
    let cfg = pooled_config_without_ref(&["anthropic-a", "anthropic-b"], "anthropic-b");

    // Act
    let (result, events) = with_capture(build_resolved_models_reported(
        &cfg,
        store,
        BuildOptions::default(),
    ))
    .await;

    // Assert: a config refusal naming the member, not a degraded pool.
    let err = result.expect_err("a member with no credential ref refuses the build");
    assert!(
        err.to_string()
            .contains("member `anthropic-b` has no credential reference"),
        "{err}"
    );
    assert!(warnings(&events).is_empty(), "{events:?}");
}

#[tokio::test]
async fn a_store_refusal_keeps_the_member_and_emits_no_store_error_text() {
    // The store's refusal names a credentials-file path, an account id, and the
    // provider it belongs to. The member is kept as a placeholder seat, and
    // none of the refusal's text reaches any event.
    let store: Arc<dyn SecretStore> = Arc::new(RefusingStore {
        dead_provider: "anthropic-b".into(),
    });
    let cfg = pooled_config(&["anthropic-a", "anthropic-b"]);

    let (result, events) = with_capture(build_resolved_models_reported(
        &cfg,
        store,
        BuildOptions::default(),
    ))
    .await;
    let built = result.expect("a degraded pool must still build");

    assert!(warnings(&events).is_empty(), "{events:?}");
    assert_eq!(built.pool_reports[0].placeholders.len(), 1);
    let rendered = format!("{events:?}");
    for banned in [
        "/home/operator",
        "credentials.json",
        "acct_9f3b21c8",
        "routectl login",
        "no credentials",
        "oauth://",
    ] {
        assert!(
            !rendered.contains(banned),
            "no build event may carry `{banned}`: {rendered}"
        );
    }
}

#[tokio::test]
async fn one_refusal_names_every_refused_member_with_its_own_rule() {
    // Two distinct refusal shapes across one build: a member with no
    // credential reference at all, and a member whose reference does not parse.
    let store: Arc<dyn SecretStore> = Arc::new(RefusingStore {
        dead_provider: "nobody".into(),
    });
    let mut cfg = pooled_config_without_ref(
        &["anthropic-a", "anthropic-b", "anthropic-c"],
        "anthropic-c",
    );
    cfg.providers.insert(
        "anthropic-b".into(),
        ProviderEntry::anthropic_api("not a ref"),
    );

    let (result, events) = with_capture(build_resolved_models_reported(
        &cfg,
        store,
        BuildOptions::default(),
    ))
    .await;

    let msg = result
        .expect_err("a survivor does not keep a misconfigured pool serving")
        .to_string();
    assert!(
        msg.contains(
            "member `anthropic-b` has a credential reference that is not a well-formed \
             secret reference"
        ),
        "{msg}"
    );
    assert!(
        msg.contains("member `anthropic-c` has no credential reference"),
        "{msg}"
    );
    assert!(
        !msg.contains("not a ref"),
        "the ref value is withheld: {msg}"
    );
    assert!(warnings(&events).is_empty(), "{events:?}");
}

#[tokio::test]
async fn a_fully_built_pool_emits_no_warning() {
    // A build with nothing wrong is silent: warnings are exception reports,
    // not a per-build heartbeat.
    let store: Arc<dyn SecretStore> = Arc::new(RefusingStore {
        dead_provider: "nobody".into(),
    });
    let cfg = pooled_config(&["anthropic-a", "anthropic-b"]);

    let (result, events) = with_capture(build_resolved_models_reported(
        &cfg,
        store,
        BuildOptions::default(),
    ))
    .await;
    let built = result.expect("a healthy pool builds");

    assert!(
        warnings(&events).is_empty(),
        "a healthy pool must be silent: {events:?}"
    );
    assert_eq!(built.pool_reports.len(), 1);
    assert!(!built.pool_reports[0].is_degraded());
    assert_eq!(built.pool_reports[0].usable_members, 2);
}

#[tokio::test]
async fn the_debug_diagnostic_for_a_placeholder_member_carries_no_store_error_text() {
    // The placeholder's report fields are sanitized by construction, but the
    // build ALSO emits a debug-level line for each one. That line reaches the
    // same archived, audited destinations every other level does, so the raw
    // store error must stay out of it at every verbosity.
    // Captures at TRACE so the debug line is in scope.
    let store: Arc<dyn SecretStore> = Arc::new(RefusingStore {
        dead_provider: "anthropic-b".into(),
    });
    let cfg = pooled_config(&["anthropic-a", "anthropic-b"]);

    let (result, events) = with_capture(build_resolved_models_reported(
        &cfg,
        store,
        BuildOptions::default(),
    ))
    .await;
    result.expect("a degraded pool must still build");

    // EVERY event this build emitted: the debug line is the surface under test
    // and it carries no `event` field to filter on.
    let all = format!("{events:?}");
    for banned in [
        "/home/operator",
        "credentials.json",
        "acct_9f3b21c8",
        "routectl login",
        "no credentials",
    ] {
        assert!(
            !all.contains(banned),
            "no build diagnostic at any level may carry `{banned}`: {all}"
        );
    }
    // And the debug line still says WHY, via the cause token.
    assert!(
        all.contains("unreadable"),
        "the cause token must survive as the diagnostic: {all}"
    );
}
