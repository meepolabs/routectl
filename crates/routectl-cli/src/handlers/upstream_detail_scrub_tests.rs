//! Upstream-authored text reaching a client (the `error.message` detail and
//! the OpenAI `error.param`, on the JSON envelope and the stream terminal
//! event alike) is scrubbed of credential- and principal-shaped substrings,
//! control-char filtered and length-bounded through one rendering. Driven
//! through the real ingress, router and an openai-compat egress against a
//! mock upstream answering HTTP 400; stream rows delay that answer past the
//! early-flush grace so the error lands after the SSE head, on the terminal
//! event, rather than on the pre-stream JSON envelope.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use futures::future::join_all;
use routectl_core::{Error, UpstreamReason};
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, Router};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    ErrorEnvelopeShape, MAX_UPSTREAM_DETAIL_CHARS, ingress_handle, map_error,
    sanitize_stream_error_for_client,
};
use crate::ingress::STREAM_ERROR_MESSAGE_CAP;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::server::AppState;

const ALIAS: &str = "a";
const NICKNAME: &str = "m";
const CONTROL: &str = "max_tokens is too large";
const PLACEHOLDER: &str = "[redacted]";
const TRUNCATED: &str = "... [truncated]";
const NON_STREAM_LEAD: &str = "upstream error (HTTP 400): ";
const STREAM_LEAD: &str = "upstream stream error (HTTP 400): ";

/// Longer than the handler's early-flush grace, so a stream request commits
/// its SSE head before the upstream 400 arrives.
const PAST_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug)]
enum Dialect {
    Messages,
    Chat,
    Responses,
}

const DIALECTS: [Dialect; 3] = [Dialect::Messages, Dialect::Chat, Dialect::Responses];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    NonStream,
    Stream,
}

const MODES: [Mode; 2] = [Mode::NonStream, Mode::Stream];

impl Mode {
    fn lead(self) -> &'static str {
        match self {
            Self::NonStream => NON_STREAM_LEAD,
            Self::Stream => STREAM_LEAD,
        }
    }
}

const ACCOUNT: &str = "210987654321";

/// A credential- or principal-shaped value and a fragment of it that must not
/// survive the scrub. Key-shaped values are assembled at run time.
struct Plant {
    name: &'static str,
    value: String,
    secret: String,
}

fn proj_key_body() -> String {
    ["Ab3dE6gH9jK2", "mN5pQ8sT1vW4yZ7"].concat()
}

fn plants() -> Vec<Plant> {
    vec![
        Plant {
            name: "plain ARN",
            value: format!("arn:aws:iam::{ACCOUNT}:role/OpsDeploy"),
            secret: "OpsDeploy".into(),
        },
        Plant {
            name: "percent-encoded ARN",
            value: format!("arn%3Aaws%3Aiam%3A%3A{ACCOUNT}%3Arole%2FOpsDeploy"),
            secret: "OpsDeploy".into(),
        },
        Plant {
            name: "account id",
            value: format!("account ID {ACCOUNT}"),
            secret: ACCOUNT.into(),
        },
        Plant {
            name: "project key",
            value: ["sk", "proj", &proj_key_body()].join("-"),
            secret: proj_key_body(),
        },
        Plant {
            name: "URL userinfo",
            value: ["https://", "user", ":", "pass", "@host"].concat(),
            secret: "user:pass".into(),
        },
    ]
}

fn request_body(dialect: Dialect, mode: Mode) -> Bytes {
    let stream = mode == Mode::Stream;
    let body = match dialect {
        Dialect::Messages | Dialect::Chat => json!({
            "model": ALIAS, "max_tokens": 16, "stream": stream,
            "messages": [{"role": "user", "content": "hi"}]
        }),
        Dialect::Responses => json!({"model": ALIAS, "input": "hi", "stream": stream}),
    };
    Bytes::from(serde_json::to_vec(&body).expect("body serializes"))
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers
}

/// A non-AWS upstream answering every call with HTTP 400 carrying `error`,
/// after `delay`.
async fn upstream_400(error: Value, delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({ "error": error }))
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

/// Alias `a` -> model `m` on one openai-compat provider at `base`.
async fn single_target(base: &str) -> Arc<AppState> {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let entry = ProviderEntry::openai_compat(base, crate::test_secret::file_ref("k"));
    let config = Config {
        providers: BTreeMap::from([("p".to_string(), entry)]),
        models: BTreeMap::from([(NICKNAME.to_string(), ModelEntry::new("p", "gpt-4o"))]),
        aliases: BTreeMap::from([(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()))]),
        retry,
        ..Default::default()
    };
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    let router: Router = crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("build router");
    AppState::for_test(Arc::new(arc_swap::ArcSwap::from_pointee(router)))
}

/// The terminal `error` object of an SSE body: the first JSON data payload
/// (or its Responses `response`) carrying an `error` object.
fn stream_error(text: &str) -> Value {
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .map(|payload| payload.get("response").cloned().unwrap_or(payload))
        .find_map(|payload| payload.get("error").filter(|e| e.is_object()).cloned())
        .unwrap_or_else(|| panic!("no terminal error event in {text}"))
}

/// The `error` object a client of `dialect` in `mode` reads when the upstream
/// answers 400 with `error`. Asserts the response status shows which side of
/// the SSE head the error landed on.
async fn client_error(dialect: Dialect, mode: Mode, error: Value) -> Value {
    let delay = match mode {
        Mode::NonStream => Duration::ZERO,
        Mode::Stream => PAST_GRACE,
    };
    let upstream = upstream_400(error, delay).await;
    let state = single_target(&upstream.uri()).await;
    let body = Ok(request_body(dialect, mode));
    let resp = Box::pin(async {
        match dialect {
            Dialect::Chat => ingress_handle(state, headers(), None, body, OpenAiIngress).await,
            Dialect::Messages => {
                ingress_handle(state, headers(), None, body, AnthropicIngress).await
            }
            Dialect::Responses => {
                ingress_handle(state, headers(), None, body, ResponsesIngress).await
            }
        }
    })
    .await;
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body reads");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    match mode {
        Mode::NonStream => {
            assert_eq!(status, StatusCode::BAD_REQUEST, "{dialect:?}: {text}");
            let value: Value = serde_json::from_str(&text).expect("envelope is JSON");
            value["error"].clone()
        }
        Mode::Stream => {
            assert_eq!(
                status,
                StatusCode::OK,
                "{dialect:?}: head committed: {text}"
            );
            stream_error(&text)
        }
    }
}

fn message_of(error: &Value) -> String {
    error["message"].as_str().unwrap_or_default().to_string()
}

fn every_sink() -> impl Iterator<Item = (Dialect, Mode)> {
    DIALECTS
        .into_iter()
        .flat_map(|dialect| MODES.into_iter().map(move |mode| (dialect, mode)))
}

#[tokio::test]
async fn a_planted_upstream_message_is_redacted_on_every_dialect_and_mode() {
    // Arrange
    let rows: Vec<(String, Dialect, Mode, Plant)> = every_sink()
        .flat_map(|(dialect, mode)| {
            plants().into_iter().map(move |plant| {
                (
                    format!("{dialect:?} {mode:?} {}", plant.name),
                    dialect,
                    mode,
                    plant,
                )
            })
        })
        .collect();

    // Act
    let messages = join_all(rows.iter().map(|(_, dialect, mode, plant)| async move {
        let error = json!({"type": "invalid_request_error",
                           "message": format!("{CONTROL}; {}", plant.value)});
        message_of(&client_error(*dialect, *mode, error).await)
    }))
    .await;

    // Assert
    for ((row, _, mode, plant), message) in rows.iter().zip(messages) {
        assert!(
            message.starts_with(&format!("{}{CONTROL}; ", mode.lead())),
            "{row}: control text verbatim: {message}"
        );
        assert!(
            !message.contains(&plant.secret),
            "{row}: secret absent: {message}"
        );
        assert!(message.contains(PLACEHOLDER), "{row}: {message}");
    }
}

#[tokio::test]
async fn a_clean_upstream_message_arrives_verbatim_on_every_dialect_and_mode() {
    let sinks: Vec<_> = every_sink().collect();

    let messages = join_all(sinks.iter().map(|(dialect, mode)| async move {
        let error = json!({"type": "invalid_request_error", "message": CONTROL});
        message_of(&client_error(*dialect, *mode, error).await)
    }))
    .await;

    for ((dialect, mode), message) in sinks.iter().zip(messages) {
        assert_eq!(
            message,
            format!("{}{CONTROL}", mode.lead()),
            "{dialect:?} {mode:?}"
        );
    }
}

#[tokio::test]
async fn a_planted_param_is_redacted_on_the_openai_envelope_and_a_plain_param_passes() {
    let key = ["sk", "proj", &proj_key_body()].join("-");
    for dialect in [Dialect::Chat, Dialect::Responses] {
        let planted = json!({"type": "invalid_request_error", "message": CONTROL,
                             "param": format!("temperature {key}")});
        let plain = json!({"type": "invalid_request_error", "message": CONTROL,
                           "param": "temperature"});

        let planted_param = client_error(dialect, Mode::NonStream, planted).await["param"].clone();
        let plain_param = client_error(dialect, Mode::NonStream, plain).await["param"].clone();

        let planted_param = planted_param.as_str().unwrap_or_default().to_string();
        assert!(
            !planted_param.contains(&proj_key_body()),
            "{dialect:?}: {planted_param}"
        );
        assert_eq!(
            planted_param,
            format!("temperature {PLACEHOLDER}"),
            "{dialect:?}"
        );
        assert_eq!(plain_param, "temperature", "{dialect:?}");
    }
}

#[tokio::test]
async fn a_control_character_in_the_non_stream_detail_is_filtered() {
    for dialect in DIALECTS {
        let error = json!({"type": "invalid_request_error",
                           "message": format!("{CONTROL}\u{1b}[31m\r\nforged")});

        let message = message_of(&client_error(dialect, Mode::NonStream, error).await);

        assert!(
            !message.chars().any(char::is_control),
            "{dialect:?}: {message:?}"
        );
        assert_eq!(
            message,
            format!("{NON_STREAM_LEAD}{CONTROL}?[31m??forged"),
            "{dialect:?}"
        );
    }
}

#[tokio::test]
async fn a_long_detail_is_bounded_identically_on_the_stream_and_non_stream_paths() {
    let long = format!("{CONTROL} {}", "x".repeat(2 * MAX_UPSTREAM_DETAIL_CHARS));
    let sinks: Vec<_> = every_sink().collect();

    let messages = join_all(sinks.iter().map(|(dialect, mode)| {
        let error = json!({"type": "invalid_request_error", "message": long});
        async move { message_of(&client_error(*dialect, *mode, error).await) }
    }))
    .await;

    let bounded: String = long.chars().take(MAX_UPSTREAM_DETAIL_CHARS).collect();
    let expected_detail = format!("{bounded}{TRUNCATED}");
    for ((dialect, mode), message) in sinks.iter().zip(messages) {
        let detail = message
            .strip_prefix(mode.lead())
            .unwrap_or_else(|| panic!("{dialect:?} {mode:?}: lead-in: {message}"));
        assert_eq!(detail, expected_detail, "{dialect:?} {mode:?}");
    }
}

/// The longest messages the stream path builds: an oversized upstream detail
/// under a three-digit status with each reason suffix, and an oversized local
/// refusal.
fn longest_stream_messages() -> Vec<(String, String)> {
    let oversized = "x".repeat(4 * MAX_UPSTREAM_DETAIL_CHARS);
    let body = json!({"error": {"message": oversized}}).to_string();
    let mut rows: Vec<(String, Error)> = [
        UpstreamReason::CredentialExpired,
        UpstreamReason::CredentialInvalid,
    ]
    .into_iter()
    .map(|reason| {
        let err = Error::upstream_full("p", 599, body.clone(), None, None, None)
            .with_upstream_reason(Some(reason));
        (format!("upstream {}", reason.as_str()), err)
    })
    .collect();
    rows.push((
        "upstream without reason".into(),
        Error::upstream_full("p", 599, body, None, None, None),
    ));
    rows.push((
        "local refusal".into(),
        Error::LocalRefusal {
            provider: "p".into(),
            detail: oversized,
            reason: routectl_core::RefusalReason::Unrepresentable,
        },
    ));
    rows.into_iter()
        .map(|(name, err)| (name, sanitize_stream_error_for_client(&err)))
        .collect()
}

#[test]
fn the_stream_message_cap_holds_the_longest_message_the_handler_builds() {
    for (name, message) in longest_stream_messages() {
        assert!(message.contains(TRUNCATED), "{name}: fixture is bounded");

        assert!(
            message.chars().count() <= STREAM_ERROR_MESSAGE_CAP,
            "{name}: {} chars over the cap {STREAM_ERROR_MESSAGE_CAP}",
            message.chars().count()
        );
    }
}

#[tokio::test]
async fn a_stream_framing_error_quoting_upstream_bytes_is_scrubbed_on_the_envelope() {
    for plant in plants() {
        let err = Error::Streaming(format!("provider `p`: SSE JSON: {}", plant.value));

        let resp = map_error(ErrorEnvelopeShape::OpenAi, err);

        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let message = message_of(&body["error"]);
        assert!(
            message.starts_with("streaming: provider `p`: SSE JSON: "),
            "{}: {message}",
            plant.name
        );
        assert!(
            !message.contains(&plant.secret),
            "{}: {message}",
            plant.name
        );
        assert!(message.contains(PLACEHOLDER), "{}: {message}", plant.name);
    }
}
