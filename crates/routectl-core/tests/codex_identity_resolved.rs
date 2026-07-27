//! The process-global codex identity resolves ONCE and every fingerprint
//! surface reads the same value.
//!
//! `RESOLVED` is a set-once `OnceLock`, so this lives in its own test
//! binary (its own process) with a SINGLE test: two tests in one process
//! would race over the slot. The lib-internal unit tests cover the pinned
//! default path; this one covers the configured path via `set_resolved`.

use routectl_core::identity::codex::{
    CODEX_ORIGINATOR, CodexIdentity, VERSION_HEADER_NAME, codex_user_agent,
    default_identity_headers, resolved_identity, set_resolved,
};

#[test]
fn configured_version_reaches_every_fingerprint_surface_coherently() {
    // Arrange + Act: install a configured identity once.
    let installed = set_resolved(CodexIdentity::new("7.7.7-int"));
    assert!(installed, "first set_resolved must install the identity");

    // A second install is refused -- the value is set-once.
    assert!(
        !set_resolved(CodexIdentity::new("8.8.8-other")),
        "a second set_resolved must not re-install",
    );

    // Assert: the resolved identity carries the configured version.
    assert_eq!(resolved_identity().version(), "7.7.7-int");

    // The User-Agent surface (read by the egress client-level default AND
    // the OAuth refresh client) carries the configured version.
    assert!(
        codex_user_agent().starts_with(&format!("{CODEX_ORIGINATOR}/7.7.7-int ")),
        "resolved UA must carry the configured version: {}",
        codex_user_agent(),
    );

    // The egress `version` identity header carries the SAME version -- UA
    // and version header cannot drift because both derive from the one
    // resolved identity.
    let version_header = default_identity_headers()
        .iter()
        .find_map(|(n, v)| (*n == VERSION_HEADER_NAME).then_some(*v));
    assert_eq!(version_header, Some("7.7.7-int"));
}
