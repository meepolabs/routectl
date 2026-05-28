//! Resolution-rule coverage for the `trace_headers` knob: with
//! `ROUTECTL_TRACE_HEADERS` unset, the seeded override reaches the
//! reader. Sister files cover the other two knobs.
//!
//! Why a dedicated integration-test binary: each reader has its own
//! `OnceLock` and the seeded override is consulted only on the
//! env-unset branch of the OnceLock-init closure. Once the reader has
//! frozen, a later test cannot exercise a different override value.
//! A separate test binary gets its own process so this test's seeding
//! happens against a pristine `OVERRIDE_TRACE_HEADERS` and a pristine
//! reader OnceLock, isolated from any sibling test's state.

use routectl_core::{header_trace_enabled, init_log_overrides};

#[test]
fn header_trace_enabled_returns_override_when_env_unset() {
    // Arrange: env unset; seed config-side fallback Some(true). Pass
    // None for the other two knobs to prove they do not get seeded
    // by accident -- only the matching argument lands on the
    // matching OVERRIDE OnceLock.
    std::env::remove_var("ROUTECTL_TRACE_HEADERS");
    init_log_overrides(Some(true), None, None);

    // Act + Assert: the reader returns the override value.
    assert!(
        header_trace_enabled(),
        "env unset + override Some(true) must resolve to true"
    );
}
