//! The structured degraded-pool WARN: `event="pool_member_omitted"`, one line
//! per omitted member per build attempt.
//!
//! Field content is the whole point of these tests. The line reaches operator
//! log destinations that get archived, shipped to third parties, and audited,
//! and the same sanitized facts feed the operator-facing pools report -- so the
//! `reason` must be one of four allowlisted tokens and no store error string,
//! credential path, token, or account id may appear in ANY field.

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

/// Every `pool_member_omitted` event in a capture.
fn omission_events(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.field("event") == Some("pool_member_omitted"))
        .collect()
}

#[tokio::test]
async fn a_degraded_pool_warns_exactly_once_for_the_member_it_lost() {
    // Arrange: two members, one of them unable to resolve a credential.
    let store: Arc<dyn SecretStore> = Arc::new(RefusingStore {
        dead_provider: "anthropic-b".into(),
    });
    let cfg = pooled_config(&["anthropic-a", "anthropic-b"]);

    // Act
    let (result, events) = with_capture(build_resolved_models_reported(
        &cfg,
        store,
        BuildOptions::default(),
    ))
    .await;
    result.expect("a degraded pool must still build");

    // Assert: one line, for the lost member only -- the survivor is not an
    // event, and the loss is not repeated per model or per retry.
    let omissions = omission_events(&events);
    assert_eq!(
        omissions.len(),
        1,
        "exactly one omission WARN, got {}: {events:?}",
        omissions.len()
    );
    let e = omissions[0];
    assert_eq!(e.field("pool"), Some("anthropic-pool"));
    assert_eq!(e.field("member"), Some("anthropic-b"));
    assert_eq!(e.field("provider_kind"), Some("anthropic-api"));
    assert_eq!(e.field("reason"), Some("credential_unreadable"));
    assert_eq!(e.field("configured_members"), Some("2"));
    assert_eq!(e.field("usable_members"), Some("1"));
}

#[tokio::test]
async fn the_omission_warn_carries_no_store_error_text() {
    // The store's refusal names a credentials-file path, an account id, and the
    // provider it belongs to. None of it may reach the line: the reason is an
    // allowlisted enum, never a formatted error.
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

    let omissions = omission_events(&events);
    let rendered = format!("{:?}", omissions[0]);
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
            "the omission WARN must not carry `{banned}`: {rendered}"
        );
    }
}

#[tokio::test]
async fn the_reason_is_always_one_of_the_four_allowlisted_tokens() {
    // Two distinct failure shapes across one build: a member with no credential
    // reference at all, and a member whose reference the store refuses.
    let store: Arc<dyn SecretStore> = Arc::new(RefusingStore {
        dead_provider: "anthropic-b".into(),
    });
    let mut cfg = pooled_config(&["anthropic-a", "anthropic-b", "anthropic-c"]);
    // An entry with an empty credential reference: nothing to authenticate an
    // account with, which is a distinct cause from a refused one.
    cfg.providers
        .insert("anthropic-c".into(), ProviderEntry::anthropic_api(""));

    let (result, events) = with_capture(build_resolved_models_reported(
        &cfg,
        store,
        BuildOptions::default(),
    ))
    .await;
    result.expect("one survivor keeps the pool serving");

    let omissions = omission_events(&events);
    assert_eq!(omissions.len(), 2, "one line per lost member");
    let allowlist = [
        "credential_missing",
        "credential_unreadable",
        "credential_invalid",
        "provider_init_failed",
    ];
    for e in &omissions {
        let reason = e.field("reason").expect("every omission names a reason");
        assert!(
            allowlist.contains(&reason),
            "`{reason}` is not an allowlisted omission reason"
        );
    }
}

#[tokio::test]
async fn a_fully_healthy_pool_emits_no_omission_warn() {
    // The line must be an exception report, not a per-build heartbeat: an
    // operator who sees it should know something was lost.
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
        omission_events(&events).is_empty(),
        "a healthy pool must be silent: {events:?}"
    );
    assert_eq!(built.pool_reports.len(), 1);
    assert!(!built.pool_reports[0].is_degraded());
    assert_eq!(built.pool_reports[0].usable_members, 2);
}

#[tokio::test]
async fn the_debug_diagnostic_for_a_lost_member_carries_no_store_error_text() {
    // The omission's own fields are sanitized by construction, but the build
    // ALSO emits a debug-level line beside each one. That line reached the same
    // archived, audited destinations every other level does, so formatting the
    // raw store error into it defeated the sanitization contract whenever debug
    // logging was on -- a leak that only opens at one verbosity is still a leak.
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

    // EVERY event this build emitted, not just the WARN -- the debug line is
    // the surface under test and it carries no `event` field to filter on.
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
    // And the debug line still says WHY, via the allowlisted token.
    assert!(
        all.contains("credential_unreadable"),
        "the reason token must survive as the diagnostic: {all}"
    );
}
