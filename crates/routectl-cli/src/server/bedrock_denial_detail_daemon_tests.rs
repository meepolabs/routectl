//! A Bedrock 403 reaches the client of every ingress dialect as the
//! provider's own access-denied detail: the AWS type, the credential reason
//! when the rejection carried one, never an ARN, account id or principal. A
//! real daemon, real HTTP into its ingress, a real Bedrock egress against a
//! mock origin answering with the recorded 403s.

use routectl_testkit::ScopedEnv;
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::wire_loss_daemon_tests::{ALIAS, Daemon, bedrock_config, bedrock_router};

const NATIVE_CAPTURE: &str = include_str!(
    "../../../routectl-providers/tests/fixtures/bedrock_native_credential_capture.json"
);
const IAM_DENY_403: &str =
    include_str!("../../../routectl-providers/tests/fixtures/bedrock/error_403_access_denied.json");
const DETAIL_STRINGS: &str =
    include_str!("../../../routectl-providers/tests/fixtures/bedrock_403_detail_strings.json");
const ACCESS_DENIED_TYPE: &str =
    "AccessDeniedException:http://internal.amazon.com/coral/com.amazon.coral.service/";

/// One recorded 403 and the key of the detail string the native lane renders
/// for it.
struct Denial {
    detail_key: &'static str,
    errortype: String,
    body: String,
}

fn captured(response: &str, detail_key: &'static str) -> Denial {
    let capture: Value = serde_json::from_str(NATIVE_CAPTURE).expect("capture is JSON");
    let row = &capture["responses"][response];
    Denial {
        detail_key,
        errortype: row["x-amzn-errortype"].as_str().expect("type").to_string(),
        body: row["body"].as_str().expect("body").to_string(),
    }
}

fn denials() -> [Denial; 3] {
    [
        captured("expired-invoke", "expired"),
        captured("invalid-invoke", "invalid"),
        Denial {
            detail_key: "model_access",
            errortype: ACCESS_DENIED_TYPE.to_string(),
            body: IAM_DENY_403.to_string(),
        },
    ]
}

fn detail_string(key: &str) -> String {
    let fixture: Value = serde_json::from_str(DETAIL_STRINGS).expect("detail strings JSON");
    fixture["strings"][key]
        .as_str()
        .unwrap_or_else(|| panic!("no detail string {key}"))
        .to_string()
}

#[derive(Clone, Copy, Debug)]
enum Dialect {
    Chat,
    Anthropic,
    Responses,
}

const DIALECTS: [Dialect; 3] = [Dialect::Chat, Dialect::Anthropic, Dialect::Responses];

impl Dialect {
    const fn route(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Anthropic => "/v1/messages",
            Self::Responses => "/v1/responses",
        }
    }

    fn request_body(self) -> Value {
        match self {
            Self::Chat => json!({
                "model": ALIAS, "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            Self::Anthropic => json!({
                "model": ALIAS, "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            Self::Responses => json!({
                "model": ALIAS, "max_output_tokens": 16,
                "input": [{"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "hi"}]}]
            }),
        }
    }
}

/// The `error` object a client of `dialect` reads when the Bedrock origin
/// answers `denial`. A fresh daemon per call: the first credential rejection
/// parks the seat, so a second request would read the local park instead.
async fn client_error(dialect: Dialect, denial: &Denial) -> Value {
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-amzn-errortype", denial.errortype.as_str())
                .insert_header("content-type", "application/json")
                .set_body_string(denial.body.clone()),
        )
        .mount(&origin)
        .await;
    let config = bedrock_config();
    let router = bedrock_router(std::sync::Arc::new(config.clone()), &origin.uri()).await;
    let daemon = Daemon::spawn(config, router).await;

    let resp = reqwest::Client::new()
        .post(format!("{}{}", daemon.base_url, dialect.route()))
        .header("anthropic-version", "2023-06-01")
        .json(&dialect.request_body())
        .send()
        .await
        .expect("the daemon answers");
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.expect("error envelope JSON");
    daemon.shutdown().await;

    assert_eq!(status, 403, "{dialect:?} {}: {body}", denial.detail_key);
    body["error"].clone()
}

#[tokio::test]
#[serial_test::serial]
async fn each_bedrock_denial_reaches_every_dialect_as_its_own_scrubbed_detail() {
    let xdg = tempfile::tempdir().expect("config tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
    for leak in ["arn:", "123456789012", "User:"] {
        assert!(
            IAM_DENY_403.contains(leak),
            "the deny fixture carries {leak:?}"
        );
    }
    for dialect in DIALECTS {
        let mut seen = Vec::new();
        for denial in denials() {
            let error = client_error(dialect, &denial).await;
            let message = error["message"].as_str().unwrap_or_default().to_string();

            let expected = format!(
                "upstream error (HTTP 403): {}",
                detail_string(denial.detail_key)
            );
            assert_eq!(message, expected, "{dialect:?} {}", denial.detail_key);
            for leak in ["arn:", "123456789012", "User:"] {
                assert!(
                    !message.contains(leak),
                    "{dialect:?} {}: leaks {leak:?}: {message}",
                    denial.detail_key
                );
            }
            if matches!(dialect, Dialect::Anthropic) {
                assert_eq!(error["type"], "permission_error", "{dialect:?}");
            }
            seen.push(message);
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 3, "{dialect:?}: {seen:?}");
    }
}
