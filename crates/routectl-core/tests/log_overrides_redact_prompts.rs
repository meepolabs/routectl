//! Resolution-rule coverage for the `redact_prompts` knob: with
//! `ROUTECTL_LOG_REDACT_PROMPTS` unset, the seeded override reaches
//! the redaction path. Sister files cover the other two knobs.
//!
//! Why a dedicated integration-test binary: see the matching note in
//! `log_overrides_trace_headers.rs` -- each reader's OnceLock freezes
//! on first read, so testing override-takes-effect requires a
//! pristine process per knob.
//!
//! The `redact_enabled` reader is module-private; this test reaches
//! it through the public `redact_prompts_in` facade by feeding a
//! body whose prompt content must end up replaced with the
//! `<redacted len=N>` marker when the resolved flag is true.

use routectl_core::{init_log_overrides, redact_prompts_in};
use serde_json::json;

#[test]
fn redact_prompts_in_uses_override_when_env_unset() {
    // Arrange: env unset; seed config-side fallback Some(true). Pass
    // None for the other two knobs to prove they do not get seeded
    // by accident.
    std::env::remove_var("ROUTECTL_LOG_REDACT_PROMPTS");
    init_log_overrides(None, None, Some(true));

    // Act: redact_prompts_in is the public surface that reads
    // redact_enabled() under the hood. With the flag resolved to
    // true via the override, a body carrying user-content text
    // fields must come back redacted.
    let body = json!({
        "messages": [
            {"role": "user", "content": "secret prompt content"}
        ]
    });
    let redacted = redact_prompts_in(&body);

    // Assert: the secret string is gone, replaced by the redacted
    // marker. Proves the resolved flag came from the override seed
    // (the env-unset + override-Some(true) -> true resolution).
    let s = serde_json::to_string(&redacted).expect("serialize redacted");
    assert!(
        !s.contains("secret prompt content"),
        "override-seeded redaction must strip user content; got: {s}"
    );
    assert!(
        s.contains("<redacted"),
        "redaction marker missing from output; got: {s}"
    );
}
