//! What the router and the client read from an upstream error whose body
//! runs past the carried excerpt: an over-cap body holding a phrase the
//! classifier or the capability matcher keys on, beside a credential, through
//! a real provider of each lane. The failure class, the matcher verdict,
//! whether the carried body parses, and the client envelope message are
//! pinned per row; none of them reads the excerpt's text.

use std::sync::Arc;

use axum::body::to_bytes;
use routectl_core::failure_class::classify;
use routectl_core::provider_kind::config as kind;
use routectl_core::{ChatRequest, Error, InMemoryProjectCache, Provider, StaticToken, TokenSource};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth as bedrock_auth,
};
use routectl_providers::gemini::{GeminiConfig, GeminiProvider};
use routectl_providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use routectl_providers::openai_responses::{
    AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};
use serde_json::Value;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{ErrorEnvelopeShape, map_error};

const PROVIDER_ID: &str = "carried-reader-test";
const KEY: &str = "sk-ant-api03-SECRETBODY0123456789abcdefghij";

#[derive(Clone, Copy, Debug)]
enum Lane {
    Compat,
    Responses,
    Gemini,
    CloudCode,
    Anthropic,
    Bedrock,
}

impl Lane {
    const fn kind(self) -> &'static str {
        match self {
            Self::Compat => kind::OPENAI_COMPAT,
            Self::Responses => kind::OPENAI_RESPONSES,
            Self::Gemini | Self::CloudCode => kind::GEMINI,
            Self::Anthropic => kind::ANTHROPIC_API,
            Self::Bedrock => kind::BEDROCK,
        }
    }
}

/// What every reader of the built error saw.
#[derive(Debug, PartialEq, Eq)]
struct Reading {
    class: String,
    verdict: Option<String>,
    carried_parses: bool,
    client_message: String,
}

struct Row {
    name: &'static str,
    lane: Lane,
    status: u16,
    body: String,
    want: Reading,
}

/// `phrase`, then padding, then the credential straddling character 512, so
/// the body runs past the excerpt cut.
fn padded(phrase: &str) -> String {
    let key_at = routectl_core::MAX_LOG_BODY_EXCERPT - 23;
    let pad = "x".repeat(key_at - phrase.len() - 2);
    format!("{phrase} {pad} {KEY} tail")
}

fn flat_json(type_field: &str, phrase: &str) -> String {
    let message = padded(phrase);
    format!(r#"{{{type_field},"message":"{message}"}}"#)
}

fn reading(class: &str, verdict: Option<&str>, parses: bool, message: &str) -> Reading {
    Reading {
        class: class.to_string(),
        verdict: verdict.map(str::to_string),
        carried_parses: parses,
        client_message: message.to_string(),
    }
}

fn rows() -> Vec<Row> {
    let context_phrase = "This model's maximum context length is 8192 tokens.";
    let structured_phrase = "output_config.format: Extra inputs are not permitted";
    vec![
        Row {
            name: "compat flat context-window code",
            lane: Lane::Compat,
            status: 400,
            body: flat_json(r#""code":"context_length_exceeded""#, context_phrase),
            want: reading("ContextWindow", None, false, "upstream error (HTTP 400)"),
        },
        Row {
            name: "compat plain unsupported parameter",
            lane: Lane::Compat,
            status: 400,
            body: padded("Unsupported parameter: response_format"),
            want: reading("BadRequest", None, false, "upstream error (HTTP 400)"),
        },
        Row {
            name: "responses plain replay prefix",
            lane: Lane::Responses,
            status: 400,
            body: padded("encrypted content missing recognized prefix"),
            want: reading("BadRequest", None, false, "upstream error (HTTP 400)"),
        },
        Row {
            name: "gemini plain context window",
            lane: Lane::Gemini,
            status: 400,
            body: padded(context_phrase),
            want: reading("BadRequest", None, false, "upstream error (HTTP 400)"),
        },
        Row {
            name: "cloud code plain context window",
            lane: Lane::CloudCode,
            status: 400,
            body: padded(context_phrase),
            want: reading("BadRequest", None, false, "upstream error (HTTP 400)"),
        },
        Row {
            name: "anthropic aws-lift structured output",
            lane: Lane::Anthropic,
            status: 400,
            body: flat_json(r#""__type":"ValidationException""#, structured_phrase),
            want: reading("BadRequest", None, false, "upstream error (HTTP 400)"),
        },
        Row {
            name: "bedrock 500 flat validation",
            lane: Lane::Bedrock,
            status: 500,
            body: flat_json(r#""__type":"ValidationException""#, structured_phrase),
            want: reading("ServerError", None, false, "upstream error (HTTP 500)"),
        },
        Row {
            name: "bedrock 400 flat validation carried raw",
            lane: Lane::Bedrock,
            status: 400,
            body: format!(
                r#"{{"__type":"ValidationException","message":"{structured_phrase}","detail":"{}"}}"#,
                padded("extra")
            ),
            want: reading(
                "BadRequest",
                Some("structured_output/self-identifying/f1"),
                true,
                "upstream error (HTTP 400)",
            ),
        },
        Row {
            name: "bedrock 400 nested envelope",
            lane: Lane::Bedrock,
            status: 400,
            body: format!(
                r#"{{"error":{{"message":"{}"}}}}"#,
                padded(structured_phrase)
            ),
            want: reading("BadRequest", None, false, "upstream error (HTTP 400)"),
        },
    ]
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "carried-reader-model".into(),
        messages: vec![routectl_core::test_utils::user_msg("ping")].into(),
        max_tokens: Some(16),
        ..Default::default()
    }
}

async fn provider_for(lane: Lane, base: &str) -> Arc<dyn Provider> {
    match lane {
        Lane::Compat => Arc::new(OpenAiCompatProvider::new(OpenAiCompatConfig::new(
            PROVIDER_ID,
            base,
            "k",
        ))),
        Lane::Responses => {
            let mut cfg = OpenAiResponsesConfig::new(PROVIDER_ID, "k");
            cfg.auth_kind = AuthKind::ApiKey;
            cfg.base_url = base.to_string();
            Arc::new(OpenAiResponsesProvider::new(cfg))
        }
        Lane::Gemini => {
            let mut cfg = GeminiConfig::new(PROVIDER_ID, "k");
            cfg.base_url = base.to_string();
            Arc::new(GeminiProvider::new(cfg))
        }
        Lane::CloudCode => {
            let token: Arc<dyn TokenSource> = Arc::new(StaticToken::new("t"));
            let mut cfg = GeminiConfig::new_cloud_code(
                PROVIDER_ID,
                token,
                Arc::new(InMemoryProjectCache::new()),
            );
            cfg.base_url = base.to_string();
            Arc::new(GeminiProvider::new(cfg))
        }
        Lane::Anthropic => {
            let mut cfg = AnthropicApiConfig::new(PROVIDER_ID, "k");
            cfg.base_url = base.to_string();
            Arc::new(AnthropicApiProvider::new(cfg))
        }
        Lane::Bedrock => {
            let creds = BedrockCreds::BearerKey {
                key: ["carried", "reader", "test"].join("-"),
            };
            let cfg = BedrockConfig {
                id: PROVIDER_ID.into(),
                region: "us-east-1".into(),
                model_id: "us.anthropic.claude-opus-5-5".into(),
                api_shape: BedrockApiShape::Invoke,
                creds: creds.clone(),
                user_agent: None,
                header_extras: Vec::new(),
                additional_model_request_fields: None,
            };
            let resolved = bedrock_auth::resolve(&creds, "us-east-1")
                .await
                .expect("resolve");
            Arc::new(
                BedrockProvider::new(cfg, resolved)
                    .expect("canonical region")
                    .with_runtime_origin_for_tests(base),
            )
        }
    }
}

async fn lane_error(row: &Row) -> Error {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(row.status).set_body_string(row.body.clone()))
        .mount(&server)
        .await;
    let provider = provider_for(row.lane, &server.uri()).await;
    provider
        .complete(request())
        .await
        .expect_err("the upstream answers an error")
}

async fn client_message(err: Error) -> String {
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body reads");
    let value: Value = serde_json::from_slice(&bytes).expect("envelope is JSON");
    value["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn read(row: &Row) -> Reading {
    let err = lane_error(row).await;
    let classified = classify(&err, Some(row.lane.kind()));
    let verdict = routectl_router::resolve_requested_capability(row.lane.kind(), &err, &classified)
        .map(|(key, tier, phase)| format!("{key}/{}/{}", tier.as_str(), phase.as_str()));
    Reading {
        class: format!("{:?}", classified.class),
        verdict,
        carried_parses: err.parsed_body().is_some(),
        client_message: client_message(err).await,
    }
}

#[tokio::test]
async fn over_cap_body_reads_the_same_to_every_reader_on_every_lane() {
    let mut mismatches = Vec::new();
    for row in rows() {
        assert!(
            row.body.len() > routectl_core::MAX_LOG_BODY_EXCERPT,
            "{}",
            row.name
        );

        let got = read(&row).await;

        if got != row.want {
            mismatches.push(format!("{} ({:?}): got {got:?}", row.name, row.lane));
        }
        assert!(!got.client_message.contains("sk-ant"), "{}", row.name);
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// A Cloud Code onboarding error envelope `len` characters long whose
/// message ends in a vendor key the scrub collapses to `[redacted]`, so the
/// scrubbed envelope is far shorter than the onboarding excerpt's 256-char
/// cut and still valid JSON.
fn onboarding_envelope(len: usize) -> String {
    let open = r#"{"error":{"code":400,"status":"FAILED_PRECONDITION","message":"precondition failed ref sk-ant-api03-"#;
    let close = r#""}}"#;
    let body = format!(
        "{open}{}{close}",
        "S".repeat(len - open.len() - close.len())
    );
    assert_eq!(body.chars().count(), len);
    body
}

fn onboarding_row(name: &'static str, len: usize, want: Reading) -> Row {
    Row {
        name,
        lane: Lane::CloudCode,
        status: 400,
        body: onboarding_envelope(len),
        want,
    }
}

#[tokio::test]
async fn onboarding_body_around_its_cuts_reads_the_same_to_every_reader() {
    let not_parsed = || reading("BadRequest", None, false, "upstream error (HTTP 400)");
    let rows = [
        onboarding_row(
            "at the onboarding cut",
            256,
            reading(
                "BadRequest",
                None,
                true,
                "upstream error (HTTP 400): precondition failed ref [redacted]",
            ),
        ),
        onboarding_row("one past the onboarding cut", 257, not_parsed()),
        onboarding_row("at the excerpt cut", 512, not_parsed()),
        onboarding_row("one past the excerpt cut", 513, not_parsed()),
    ];
    let mut mismatches = Vec::new();
    for row in rows {
        let got = read(&row).await;

        if got != row.want {
            mismatches.push(format!("{}: got {got:?}", row.name));
        }
        assert!(!got.client_message.contains("sk-ant"), "{}", row.name);
    }

    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

const ONBOARDING_OPEN: &str =
    r#"{"error":{"code":400,"status":"FAILED_PRECONDITION","message":"precondition failed "#;

/// A Cloud Code onboarding error envelope `chars` characters and `bytes`
/// bytes long, its message localized: 3-byte CJK characters, at most one
/// 2-byte character, then ASCII padding.
fn localized_onboarding_envelope(chars: usize, bytes: usize) -> String {
    let close = r#""}}"#;
    let message_chars = chars - ONBOARDING_OPEN.len() - close.len();
    let extra = bytes - chars;
    let (cjk, accented) = (extra / 2, extra % 2);
    let message = format!(
        "{}{}{}",
        "\u{8a2d}".repeat(cjk),
        "\u{e9}".repeat(accented),
        "a".repeat(message_chars - cjk - accented)
    );
    let body = format!("{ONBOARDING_OPEN}{message}{close}");
    assert_eq!((body.chars().count(), body.len()), (chars, bytes));
    body
}

/// The client message the parsed localized envelope yields: its message
/// with each non-ASCII character filtered to `?`.
fn localized_client_message(body: &str) -> String {
    let value: Value = serde_json::from_str(body).expect("envelope parses");
    let filtered: String = value["error"]["message"]
        .as_str()
        .expect("message")
        .chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect();
    format!("upstream error (HTTP 400): {filtered}")
}

#[tokio::test]
async fn localized_onboarding_body_at_its_cut_and_over_the_excerpt_bytes_is_status_only() {
    let status_only = || reading("BadRequest", None, false, "upstream error (HTTP 400)");
    let parsed = |body: &str| reading("BadRequest", None, true, &localized_client_message(body));
    let rows = [
        ("256 chars, 512 bytes", 256, 512, true),
        ("256 chars, 513 bytes", 256, 513, false),
        ("256 chars, 550 bytes", 256, 550, false),
        (
            "256 chars, 596 bytes, every message char cjk",
            256,
            596,
            false,
        ),
        ("255 chars, 513 bytes", 255, 513, false),
        ("257 chars, 513 bytes", 257, 513, false),
    ];
    let mut mismatches = Vec::new();
    for (name, chars, bytes, parses) in rows {
        let body = localized_onboarding_envelope(chars, bytes);
        let want = if parses { parsed(&body) } else { status_only() };
        let row = Row {
            name,
            lane: Lane::CloudCode,
            status: 400,
            body,
            want,
        };

        let got = read(&row).await;

        if got != row.want {
            mismatches.push(format!("{name}: got {got:?}"));
        }
    }

    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[tokio::test]
async fn short_onboarding_body_reads_the_same_except_where_a_credential_was_redacted() {
    let rows = [
        onboarding_row_with(
            "string credential value",
            r#"{"error":{"code":400,"status":"FAILED_PRECONDITION","message":"precondition failed","api_key":"plantedvalue42"}}"#,
            reading(
                "BadRequest",
                None,
                true,
                "upstream error (HTTP 400): precondition failed",
            ),
        ),
        onboarding_row_with(
            "account number value",
            r#"{"error":{"code":400,"status":"FAILED_PRECONDITION","message":"precondition failed","account": 123456789012}}"#,
            reading("BadRequest", None, false, "upstream error (HTTP 400)"),
        ),
        Row {
            name: "credentials the scrub grows past the cut",
            lane: Lane::CloudCode,
            status: 400,
            body: format!(
                r#"{{"error":{{"code":400,"status":"FAILED_PRECONDITION","message":"precondition failed","detail":"{}"}}}}"#,
                ["secret=q1"; 16].join(" ")
            ),
            want: reading("BadRequest", None, false, "upstream error (HTTP 400)"),
        },
    ];
    let mut mismatches = Vec::new();
    for row in rows {
        assert!(row.body.chars().count() <= 256, "{}", row.name);

        let got = read(&row).await;

        if got != row.want {
            mismatches.push(format!("{}: got {got:?}", row.name));
        }
    }

    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

fn onboarding_row_with(name: &'static str, body: &str, want: Reading) -> Row {
    Row {
        name,
        lane: Lane::CloudCode,
        status: 400,
        body: body.to_string(),
        want,
    }
}
