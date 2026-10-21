//! Welds the beta-rejection parser to the committed beta fixtures and pins
//! every way an error fails to be a beta rejection.

use std::path::PathBuf;

use routectl_core::UpstreamBody;
use serde_json::{Value, json};

use super::rejected_beta_flags;

const ANTHROPIC: &str = "anthropic-api";
const BEDROCK: &str = "bedrock";
const OPENAI_COMPAT: &str = "openai-compat";
const VALIDATION: &str = "ValidationException";
const INVALID_REQUEST: &str = "invalid_request_error";

const HEAD: &str = "Unexpected value(s) ";
const TAIL: &str = " for the `anthropic-beta` header. Please consult our documentation at \
                    platform.claude.com/docs or try again without the header.";
const CONVERSE_WRAPPER: &str = "The model returned the following errors: ";

/// A committed `rejections/<kind>/<case>.json` fixture.
struct Fixture {
    status: u16,
    lifted_type: String,
    body: String,
}

fn fixture(kind: &str, case: &str) -> Fixture {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rejections")
        .join(kind)
        .join(format!("{case}.json"));
    let value: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    Fixture {
        status: u16::try_from(value["status"].as_u64().unwrap()).unwrap(),
        lifted_type: value["lifted_type"].as_str().unwrap().to_string(),
        body: value["body"].as_str().unwrap().to_string(),
    }
}

fn flags(kind: &str, status: u16, upstream_type: Option<&str>, body: &str) -> Option<Vec<String>> {
    let body = UpstreamBody::new(body);
    rejected_beta_flags(kind, status, upstream_type, body.parsed())
}

fn envelope(list: &str) -> String {
    format!("{HEAD}{list}{TAIL}")
}

fn bedrock_body(message: &str) -> String {
    json!({ "message": message }).to_string()
}

fn anthropic_body(error_type: &str, message: &str) -> String {
    json!({"type": "error", "error": {"type": error_type, "message": message}}).to_string()
}

#[test]
fn committed_beta_fixtures_name_their_flags_in_request_order() {
    let cases: [(&str, &str, &str, &[&str]); 4] = [
        (
            "bedrock invoke, bare envelope",
            BEDROCK,
            "beta-redact-thinking-invoke",
            &["redact-thinking-2026-02-12"],
        ),
        (
            "bedrock converse, wrapped envelope",
            BEDROCK,
            "beta-redact-thinking-converse",
            &["redact-thinking-2026-02-12"],
        ),
        (
            "mantle, one flag",
            ANTHROPIC,
            "beta-prompt-caching-scope-mantle",
            &["prompt-caching-scope-2026-01-05"],
        ),
        (
            "mantle, two flags",
            ANTHROPIC,
            "beta-two-flags-mantle",
            &[
                "advanced-tool-use-2025-11-20",
                "prompt-caching-scope-2026-01-05",
            ],
        ),
    ];

    for (name, kind, case, expected) in cases {
        let fx = fixture(kind, case);

        let named = flags(kind, fx.status, Some(&fx.lifted_type), &fx.body);

        let expected: Vec<String> = expected.iter().map(|f| (*f).to_string()).collect();
        assert_eq!(named, Some(expected), "{name}");
    }
}

#[test]
fn a_repeated_flag_collapses_to_its_first_position() {
    let body = bedrock_body(&envelope("`b-1`, `a-1`, `b-1`"));

    let named = flags(BEDROCK, 400, Some(VALIDATION), &body);

    assert_eq!(named, Some(vec!["b-1".to_string(), "a-1".to_string()]));
}

#[test]
fn a_namespaced_validation_discriminator_still_gates_in() {
    let body = bedrock_body(&envelope("`a-1`"));

    let named = flags(
        BEDROCK,
        400,
        Some("com.amazon.coral.validate#ValidationException"),
        &body,
    );

    assert_eq!(named, Some(vec!["a-1".to_string()]));
}

#[test]
fn an_error_that_is_not_an_exact_beta_rejection_names_no_flags() {
    let valid = envelope("`a-1`");
    let rows: Vec<(&str, &str, u16, Option<&str>, String)> = vec![
        (
            "bedrock 400 with no lifted type",
            BEDROCK,
            400,
            None,
            bedrock_body(&valid),
        ),
        (
            "bedrock 400 lifted as another exception",
            BEDROCK,
            400,
            Some("ThrottlingException"),
            bedrock_body(&valid),
        ),
        (
            "bedrock non-400",
            BEDROCK,
            422,
            Some(VALIDATION),
            bedrock_body(&valid),
        ),
        (
            "anthropic non-400",
            ANTHROPIC,
            422,
            Some(INVALID_REQUEST),
            anthropic_body(INVALID_REQUEST, &valid),
        ),
        (
            "head deviates by one byte",
            BEDROCK,
            400,
            Some(VALIDATION),
            bedrock_body(&format!("Unexpected value(s): `a-1`{TAIL}")),
        ),
        (
            "tail deviates by one byte, head exact",
            BEDROCK,
            400,
            Some(VALIDATION),
            bedrock_body(&valid.replace("header.", "header!")),
        ),
        (
            "tail truncated, head exact",
            ANTHROPIC,
            400,
            Some(INVALID_REQUEST),
            anthropic_body(
                INVALID_REQUEST,
                &format!("{HEAD}`a-1` for the `anthropic-beta` header."),
            ),
        ),
        (
            "doubled converse wrapper",
            BEDROCK,
            400,
            Some(VALIDATION),
            bedrock_body(&format!("{CONVERSE_WRAPPER}{CONVERSE_WRAPPER}{valid}")),
        ),
        (
            "converse wrapper on the anthropic shape",
            ANTHROPIC,
            400,
            Some(INVALID_REQUEST),
            anthropic_body(INVALID_REQUEST, &format!("{CONVERSE_WRAPPER}{valid}")),
        ),
        (
            "backtick inside a token",
            BEDROCK,
            400,
            Some(VALIDATION),
            bedrock_body(&envelope("`a`b`")),
        ),
        (
            "space inside a token",
            BEDROCK,
            400,
            Some(VALIDATION),
            bedrock_body(&envelope("`a b`")),
        ),
        (
            "empty token",
            BEDROCK,
            400,
            Some(VALIDATION),
            bedrock_body(&envelope("``")),
        ),
        (
            "unquoted token",
            BEDROCK,
            400,
            Some(VALIDATION),
            bedrock_body(&envelope("a-1")),
        ),
        (
            "one malformed token among valid ones",
            ANTHROPIC,
            400,
            Some(INVALID_REQUEST),
            anthropic_body(INVALID_REQUEST, &envelope("`a-1`, `b 1`")),
        ),
        (
            "anthropic body with another error type",
            ANTHROPIC,
            400,
            Some("api_error"),
            anthropic_body("api_error", &valid),
        ),
        (
            "anthropic shape on the bedrock kind",
            BEDROCK,
            400,
            Some(VALIDATION),
            anthropic_body(INVALID_REQUEST, &valid),
        ),
        (
            "flat bedrock shape on the anthropic kind",
            ANTHROPIC,
            400,
            Some(INVALID_REQUEST),
            bedrock_body(&valid),
        ),
        (
            "openai-compat kind",
            OPENAI_COMPAT,
            400,
            Some(INVALID_REQUEST),
            anthropic_body(INVALID_REQUEST, &valid),
        ),
        (
            "body that is not JSON",
            BEDROCK,
            400,
            Some(VALIDATION),
            valid.clone(),
        ),
    ];

    for (name, kind, status, upstream_type, body) in &rows {
        assert_eq!(
            flags(kind, *status, *upstream_type, body),
            None,
            "{name} must name no flags",
        );
    }
    assert_eq!(
        flags(BEDROCK, 400, Some(VALIDATION), &bedrock_body(&valid)),
        Some(vec!["a-1".to_string()]),
        "control: the unaltered bedrock envelope names its flag",
    );
    assert_eq!(
        flags(
            ANTHROPIC,
            400,
            Some(INVALID_REQUEST),
            &anthropic_body(INVALID_REQUEST, &valid)
        ),
        Some(vec!["a-1".to_string()]),
        "control: the unaltered anthropic envelope names its flag",
    );
}
