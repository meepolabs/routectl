//! Resolution-rule coverage for the `trace_body_bytes` knob: with
//! `ROUTECTL_TRACE_BODY_BYTES` unset, the seeded override reaches the
//! reader. Sister files cover the other two knobs.
//!
//! Why a dedicated integration-test binary: see the matching note in
//! `log_overrides_trace_headers.rs` -- each reader's OnceLock freezes
//! on first read, so testing override-takes-effect requires a
//! pristine process per knob.

use routectl_core::{init_log_overrides, trace_body_cap};

#[test]
fn trace_body_cap_returns_override_when_env_unset() {
    // Arrange: env unset; seed config-side fallback Some(99_999).
    // Pass None for the other two knobs to prove they do not get
    // seeded by accident.
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::remove_var("ROUTECTL_TRACE_BODY_BYTES") };
    init_log_overrides(None, Some(99_999), None);

    // Act + Assert: the reader returns the override value, NOT the
    // hardcoded `MAX_TRACE_BODY_BYTES` default.
    assert_eq!(
        trace_body_cap(),
        99_999,
        "env unset + override Some(99_999) must resolve to 99_999"
    );
}
