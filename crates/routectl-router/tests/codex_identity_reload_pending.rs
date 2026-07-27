//! `install_resolved_codex_identity` installs the codex identity ONCE
//! (the underlying `OnceLock` is set-once; `codex_version` is
//! restart-required). A hot reload that re-runs the factory with a
//! CHANGED `codex_version` must NOT log the new value as resolved -- the
//! running process still serves the boot value. Instead it warns that the
//! change is pending a daemon restart. A same-value re-install stays
//! quiet.
//!
//! `resolved_identity` is a set-once process-global, so this lives in its
//! own test binary with a SINGLE test that walks the boot -> reload ->
//! no-op sequence in order.

use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::identity::codex::resolved_identity;
use routectl_router::{BuildOptions, Config, build_resolved_models};
use routectl_testkit::with_capture;

const RESOLVED_MSG: &str = "codex identity resolved";
const PENDING_RESTART_MSG: &str =
    "codex_version changed but requires a daemon restart to take effect";

fn config_with_version(version: &str) -> Config {
    toml::from_str(&format!(
        "[providers.cx]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://openai\"\n\
         auth_kind = \"chatgpt-oauth\"\ncodex_version = \"{version}\"\n\
         [models.m]\nprovider = \"cx\"\nupstream = \"gpt-5\"\n",
    ))
    .expect("fixture parses")
}

#[tokio::test]
async fn changed_codex_version_on_reload_warns_pending_restart_without_false_info() {
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    // Boot: the first resolution installs the identity and announces it.
    let cfg_first = config_with_version("5.5.5-first");
    let (_r1, boot_events) = with_capture(build_resolved_models(
        &cfg_first,
        store.clone(),
        BuildOptions::default(),
    ))
    .await;
    assert_eq!(resolved_identity().version(), "5.5.5-first");
    let info = boot_events
        .iter()
        .find(|e| e.message == RESOLVED_MSG)
        .unwrap_or_else(|| panic!("boot must announce the resolved identity: {boot_events:#?}"));
    assert_eq!(info.level, tracing::Level::INFO);
    assert_eq!(info.field("codex_version"), Some("5.5.5-first"));

    // Reload with a CHANGED value: the set-once identity still serves the
    // boot value, so the log must report a pending restart and must NOT
    // falsely announce the new version as resolved.
    let cfg_changed = config_with_version("6.6.6-second");
    let (_r2, reload_events) = with_capture(build_resolved_models(
        &cfg_changed,
        store.clone(),
        BuildOptions::default(),
    ))
    .await;
    assert_eq!(
        resolved_identity().version(),
        "5.5.5-first",
        "the set-once identity must keep the boot value across a reload",
    );
    assert!(
        reload_events.iter().all(|e| e.message != RESOLVED_MSG),
        "a reload must not falsely announce a new resolved identity: {reload_events:#?}",
    );
    let warn = reload_events
        .iter()
        .find(|e| e.message == PENDING_RESTART_MSG)
        .unwrap_or_else(|| panic!("reload must warn pending restart: {reload_events:#?}"));
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(warn.field("configured"), Some("6.6.6-second"));
    assert_eq!(warn.field("active"), Some("5.5.5-first"));

    // Same-value re-install: nothing changed, so neither the resolved INFO
    // nor the pending-restart WARN fires.
    let cfg_same = config_with_version("5.5.5-first");
    let (_r3, noop_events) = with_capture(build_resolved_models(
        &cfg_same,
        store.clone(),
        BuildOptions::default(),
    ))
    .await;
    assert!(
        noop_events
            .iter()
            .all(|e| e.message != RESOLVED_MSG && e.message != PENDING_RESTART_MSG),
        "a same-value re-install must stay quiet: {noop_events:#?}",
    );
}
