//! Unit tests for the credential-key sweep the body-trace redactor runs
//! in both prompt-redaction branches.

use super::{
    body_key_looks_credential, clean_upstream_error_body, redact_error_body_text,
    redact_prompts_with_flag,
};
use serde_json::json;
use std::borrow::Cow;

const SENTINEL: &str = "SENTINEL-CREDENTIAL-VALUE-7f3a";

/// Anthropic Messages connector shape: top-level `mcp_servers` entries
/// carrying an `authorization_token`.
fn anthropic_mcp_body() -> serde_json::Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "mcp_servers": [
            {
                "type": "url",
                "url": "https://mcp.example.com/sse",
                "name": "example-mcp",
                "authorization_token": SENTINEL,
            }
        ],
        "messages": [{"role": "user", "content": "hi"}],
    })
}

/// OpenAI Responses MCP tool shape: `authorization` on the tool and a
/// nested `headers.Authorization`.
fn responses_mcp_body() -> serde_json::Value {
    json!({
        "model": "gpt-5",
        "max_output_tokens": 512,
        "tools": [
            {
                "type": "mcp",
                "server_label": "deepwiki",
                "server_url": "https://mcp.example.com/mcp",
                "authorization": SENTINEL,
                "headers": {
                    "Authorization": format!("Bearer {SENTINEL}"),
                    "Cookie": format!("session={SENTINEL}"),
                    "Ocp-Apim-Subscription-Key": SENTINEL,
                    "Content-Type": "application/json",
                    "x-ratelimit-remaining-tokens": "900",
                },
            }
        ],
        "input": "hi",
    })
}

/// A credential at the top level of the body rather than under a
/// connector entry.
fn top_level_credential_body() -> serde_json::Value {
    json!({
        "model": "m",
        "authorization": SENTINEL,
        "accessToken": SENTINEL,
        "SessionToken": SENTINEL,
        "usage": {"input_tokens": 11, "output_tokens": 22},
    })
}

fn assert_sentinel_absent(body: &serde_json::Value, enabled: bool) {
    let dump = redact_prompts_with_flag(body, enabled).to_string();
    assert!(
        !dump.contains(SENTINEL),
        "credential leaked with prompt redaction {enabled}: {dump}"
    );
}

#[test]
fn mcp_servers_authorization_token_redacted_with_prompt_redaction_off() {
    assert_sentinel_absent(&anthropic_mcp_body(), false);
}

#[test]
fn mcp_servers_authorization_token_redacted_with_prompt_redaction_on() {
    assert_sentinel_absent(&anthropic_mcp_body(), true);
}

#[test]
fn responses_mcp_authorization_and_headers_redacted_with_prompt_redaction_off() {
    assert_sentinel_absent(&responses_mcp_body(), false);
}

#[test]
fn responses_mcp_authorization_and_headers_redacted_with_prompt_redaction_on() {
    assert_sentinel_absent(&responses_mcp_body(), true);
}

#[test]
fn top_level_authorization_redacted_with_prompt_redaction_off() {
    assert_sentinel_absent(&top_level_credential_body(), false);
}

#[test]
fn top_level_authorization_redacted_with_prompt_redaction_on() {
    assert_sentinel_absent(&top_level_credential_body(), true);
}

#[test]
fn header_map_secrets_redacted_by_header_rule_in_both_branches() {
    // `Cookie` and a `*-Key` header pass the body-key rule; only the
    // header rule applied to a `headers` map catches them.
    for enabled in [false, true] {
        let got = redact_prompts_with_flag(&responses_mcp_body(), enabled);
        let headers = &got["tools"][0]["headers"];

        for name in ["Cookie", "Ocp-Apim-Subscription-Key"] {
            let value = headers[name].as_str().unwrap_or_default();
            assert!(
                value.starts_with("<redacted len="),
                "{name} enabled={enabled}: {got}"
            );
        }
    }
}

#[test]
fn credential_string_value_becomes_length_marker() {
    let got = redact_prompts_with_flag(&anthropic_mcp_body(), false);

    let expected = format!("<redacted len={}>", SENTINEL.chars().count());
    assert_eq!(got["mcp_servers"][0]["authorization_token"], expected);
}

#[test]
fn structured_credential_value_collapses_whole() {
    // An object under a credential-named key collapses without exposing
    // its children, including non-credential-named ones like `kind`.
    let body = json!({"auth": {"api_key": {"value": SENTINEL, "kind": "static"}}});

    let got = redact_prompts_with_flag(&body, false);

    assert_eq!(got["auth"]["api_key"], json!({"redacted": true}));
}

#[test]
fn structural_fields_stay_visible_in_both_branches() {
    for enabled in [false, true] {
        let anthropic = redact_prompts_with_flag(&anthropic_mcp_body(), enabled);
        let responses = redact_prompts_with_flag(&responses_mcp_body(), enabled);
        let top = redact_prompts_with_flag(&top_level_credential_body(), enabled);

        assert_eq!(anthropic["max_tokens"], 1024, "enabled={enabled}");
        assert_eq!(
            anthropic["thinking"]["budget_tokens"], 2048,
            "enabled={enabled}"
        );
        assert_eq!(anthropic["mcp_servers"][0]["name"], "example-mcp");
        assert_eq!(anthropic["mcp_servers"][0]["type"], "url");
        assert_eq!(responses["max_output_tokens"], 512, "enabled={enabled}");
        assert_eq!(responses["tools"][0]["server_label"], "deepwiki");
        assert_eq!(responses["tools"][0]["type"], "mcp");
        let headers = &responses["tools"][0]["headers"];
        assert_eq!(headers["Content-Type"], "application/json");
        assert_eq!(headers["x-ratelimit-remaining-tokens"], "900");
        assert_eq!(top["usage"]["input_tokens"], 11, "enabled={enabled}");
        assert_eq!(top["usage"]["output_tokens"], 22, "enabled={enabled}");
    }
}

#[test]
fn redaction_leaves_the_source_body_unmodified() {
    for body in [anthropic_mcp_body(), responses_mcp_body()] {
        let before = body.clone();

        let _ = redact_prompts_with_flag(&body, false);
        let _ = redact_prompts_with_flag(&body, true);

        assert_eq!(body, before);
    }
}

#[test]
fn debug_upstream_error_body_redacts_echoed_credentials() {
    let body = json!({
        "error": {"message": "bad request", "echo": {"authorization_token": SENTINEL}},
    })
    .to_string();

    let cleaned = clean_upstream_error_body(&body);

    assert!(!cleaned.contains(SENTINEL), "{cleaned}");
    assert!(cleaned.contains("bad request"), "{cleaned}");
}

#[test]
fn credential_key_predicate_matches_credential_shapes() {
    for key in [
        "authorization",
        "authorization_token",
        "proxy-authentication",
        "api_key",
        "x-api-key",
        "apikey",
        "client_secret",
        "password",
        "bearer",
        "access_token",
        "refresh-token",
        "accesstoken",
        "refreshtoken",
        "idtoken",
        "sessiontoken",
        "private_key",
        "client_private_key",
        "private-key",
        "privatekey",
    ] {
        assert!(body_key_looks_credential(key), "{key} should match");
    }
}

#[test]
fn credential_key_predicate_keeps_token_counts_visible() {
    for key in [
        "max_tokens",
        "budget_tokens",
        "max_output_tokens",
        "max_completion_tokens",
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "total_tokens",
        "prompttokencount",
        "candidatestokencount",
        "totaltokencount",
        "server_label",
        "name",
        "type",
    ] {
        assert!(!body_key_looks_credential(key), "{key} should not match");
    }
}

#[test]
fn error_body_text_redacts_credential_in_non_envelope_json() {
    let body = format!(
        r#"{{"detail":[{{"loc":["body"],"input":{{"api_key":"{SENTINEL}"}},"msg":"{}"}}]}}"#,
        "m".repeat(600)
    );

    let got = redact_error_body_text(&body);

    assert!(!got.contains(SENTINEL), "{got}");
    assert!(got.contains("\"loc\""), "{got}");
}

#[test]
fn error_body_text_borrows_clean_json_and_plain_text() {
    let clean = r#"{"error": {"message": "no such model"}}"#;
    let text = "upstream exploded";

    assert!(matches!(redact_error_body_text(clean), Cow::Borrowed(s) if s == clean));
    assert!(matches!(redact_error_body_text(text), Cow::Borrowed(s) if s == text));
}

#[test]
fn error_body_text_marks_json_it_cannot_redact() {
    let oversized = format!(
        r#"{{"api_key":"{SENTINEL}","pad":"{}"}}"#,
        "x".repeat(crate::MAX_ERROR_BODY_BYTES)
    );
    let truncated = format!(r#"{{"input":{{"api_key":"{SENTINEL}"... [truncated]"#);

    let over = redact_error_body_text(&oversized);
    let cut = redact_error_body_text(&truncated);

    let marker = format!("(json body, {} bytes, not excerpted)", oversized.len());
    assert_eq!(over, marker);
    assert!(cut.starts_with("(json body, "), "{cut}");
    assert!(!cut.contains(SENTINEL), "{cut}");
}

#[test]
fn error_body_text_strips_bom_before_redacting() {
    let body = format!("\u{feff}{{\"detail\":{{\"api_key\":\"{SENTINEL}\"}}}}");

    let got = redact_error_body_text(&body);

    assert!(!got.contains(SENTINEL), "{got}");
    assert!(got.contains("<redacted len="), "{got}");
}

#[test]
fn error_body_marker_survives_every_sanitizer_unchanged() {
    // The marker must open with none of `<` (the excerpt sanitizers would
    // re-label it as an HTML page) or `{` / `[` (it would be re-marked
    // with its own length as unparseable JSON).
    let body = format!("{{\"api_key\":\"{SENTINEL}\"");
    let marker = redact_error_body_text(&body).into_owned();

    assert_eq!(crate::sanitize_upstream_body(&marker), marker);
    assert_eq!(
        crate::sanitize_upstream_body_with_byte_cap(&marker, crate::MAX_ERROR_BODY_BYTES),
        marker
    );
    assert_eq!(crate::sanitize_for_log(&marker), marker);
    assert_eq!(redact_error_body_text(&marker), marker);
    assert_eq!(clean_upstream_error_body(&marker), marker);
}

#[test]
fn debug_upstream_error_body_redacts_bom_prefixed_json() {
    let body = format!("\u{feff}{{\"error\":{{\"message\":\"bad\",\"api_key\":\"{SENTINEL}\"}}}}");

    let events = routectl_testkit::capture_events(|| {
        crate::debug_upstream_error_body("kind", "prov", 400, &body);
    });

    let debug = events
        .iter()
        .find(|e| e.message == "upstream error body")
        .expect("DEBUG upstream error body event");
    let logged = debug.field("body").expect("body field");
    assert!(!logged.contains(SENTINEL), "{logged}");
    assert!(logged.contains("bad"), "{logged}");
}
