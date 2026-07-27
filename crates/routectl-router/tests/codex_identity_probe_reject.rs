//! `provider probe` / `doctor` load config through the unvalidated path,
//! which never runs `validate_codex_version`. A single provider built
//! directly via `build_provider_with_options` must therefore re-check the
//! `codex_version` syntax itself: an illegal value (here a control byte)
//! must NOT reach the derived User-Agent -- a header-illegal byte panics
//! reqwest's `ClientBuilder` downstream and crashes a diagnostic command
//! that promises to degrade. The defensive branch rejects-to-default
//! (never sanitizes) and warns.
//!
//! `resolved_identity` is a set-once process-global, so this lives in its
//! own test binary with a SINGLE test.

use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::identity::codex::{PINNED_CODEX_VERSION, resolved_identity};
use routectl_router::{BuildOptions, Config, build_provider_with_options};
use routectl_testkit::with_capture;

#[tokio::test]
async fn illegal_codex_version_on_probe_falls_back_to_pinned_and_warns() {
    // Arrange: an openai-responses provider whose codex_version carries a
    // control byte (DEL, 0x7f). The TOML unicode escape keeps the source
    // ASCII while the parsed value is header-illegal. Parsing does NOT
    // validate the syntax -- that is exactly the gap the probe path hits.
    let config: Config = toml::from_str(
        "[providers.cx]\nkind = \"openai-responses\"\napi_key_ref = \"literal:sk-test\"\n\
         auth_kind = \"api-key\"\ncodex_version = \"1.2.3\\u007f\"\n\
         [models.m]\nprovider = \"cx\"\nupstream = \"gpt-5\"\n",
    )
    .expect("fixture parses");
    let entry = config.providers.get("cx").expect("provider present");
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    // Act: build the single provider directly. This must complete without
    // panicking regardless of whether provider construction itself
    // succeeds -- the identity install happens before construction.
    let (_result, events) = with_capture(build_provider_with_options(
        "cx",
        entry,
        store,
        BuildOptions::default(),
    ))
    .await;

    // Assert: the process-global identity fell back to the pinned default
    // rather than the illegal value, and the derived UA carries the pinned
    // version (never a sanitized derivative of the rejected input).
    let identity = resolved_identity();
    assert_eq!(identity.version(), PINNED_CODEX_VERSION);
    assert!(
        identity.user_agent().contains(PINNED_CODEX_VERSION),
        "resolved UA must carry the pinned version: {}",
        identity.user_agent(),
    );

    // A structured WARN names the provider and the rejection reason.
    let warn = events
        .iter()
        .find(|e| e.message == "invalid codex_version; falling back to the pinned codex identity")
        .unwrap_or_else(|| panic!("no fallback WARN captured: {events:#?}"));
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(warn.field("provider"), Some("cx"));
    assert_eq!(warn.field("codex_version"), Some(PINNED_CODEX_VERSION));
    assert!(
        warn.field("reason").is_some_and(|r| !r.is_empty()),
        "WARN must carry a non-empty reason: {warn:#?}",
    );
}
