//! Emit-path coverage for credential redaction in the ingress and
//! outgoing body traces. A dedicated binary so the process-frozen
//! prompt-redaction toggle can be pinned OFF -- the branch where only
//! the always-on redaction stands between a connector credential and
//! the log.

use routectl_core::{trace_ingress_body, trace_outgoing_body};
use routectl_testkit::capture_events;
use serde_json::json;

const SENTINEL: &str = "SENTINEL-CONNECTOR-TOKEN-91c2";

#[test]
fn body_traces_drop_connector_credentials_and_keep_structure() {
    // Arrange: pin the prompt-redaction knob off before its OnceLock is
    // first read in this process.
    // SAFETY: the only test in this binary, so no other thread touches the
    // environment concurrently.
    unsafe { std::env::remove_var("ROUTECTL_LOG_REDACT_PROMPTS") };
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "mcp_servers": [
            {"type": "url", "name": "example-mcp", "authorization_token": SENTINEL}
        ],
        "tools": [
            {"type": "mcp", "server_label": "deepwiki",
             "headers": {"Authorization": format!("Bearer {SENTINEL}")}}
        ],
        "messages": [{"role": "user", "content": "visible prompt"}],
    });
    let before = body.clone();

    // Act
    let events = capture_events(|| {
        trace_ingress_body("anthropic", &body);
        trace_outgoing_body("anthropic-api", "prov-1", &body);
    });

    // Assert: both directions emitted, neither carries the credential,
    // structure (and, knob off, the prompt) stays readable, and the
    // caller's body is untouched.
    assert_eq!(events.len(), 2, "captured: {events:#?}");
    for message in ["ingress request body", "outgoing request body"] {
        let event = events
            .iter()
            .find(|e| e.message == message)
            .unwrap_or_else(|| panic!("no {message:?} event; captured: {events:#?}"));
        let traced = event.field("body").expect("body field");
        assert!(!traced.contains(SENTINEL), "{message}: {traced}");
        for visible in [
            "example-mcp",
            "deepwiki",
            "\"max_tokens\":1024",
            "visible prompt",
        ] {
            assert!(
                traced.contains(visible),
                "{message} lost {visible}: {traced}"
            );
        }
    }
    assert_eq!(body, before);
}
