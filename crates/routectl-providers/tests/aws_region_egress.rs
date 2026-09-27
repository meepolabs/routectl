//! Hermetic egress pins for the region-derived AWS lanes: a provider built
//! directly through its public constructor (no config validation in
//! front of it) with a region carrying URL structure must fail before any
//! byte leaves the process.
//!
//! Each hostile region is shaped so that, interpolated unchecked into the
//! lane's host template, the parsed URL's host becomes a local tripwire
//! listener rather than the AWS endpoint. The tripwire is a raw TCP
//! socket that records every accepted connection and every byte, so it
//! observes a TLS ClientHello as readily as a plaintext request, and a
//! positive control proves it does. Nothing resolves DNS or leaves
//! loopback.

#![cfg(feature = "bedrock")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use routectl_core::{ChatRequest, Error, Message, MessageContent, Provider, Role, StaticToken};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind as AnthropicAuthKind, CloakConfig,
};
use routectl_providers::bedrock::auth::{ResolvedCreds, resolve};
use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider};
use routectl_providers::mantle::MantleAuth;
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use routectl_providers::openai_responses::{
    AuthKind as ResponsesAuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

const BEARER_KEY: &str = "tripwire-bearer-key-must-not-leave";
const ACCESS_KEY: &str = "AKIATRIPWIRE00000000";
const PROMPT: &str = "tripwire-prompt-body-must-not-leave";

/// A loopback listener that records every connection it accepts and every
/// byte those connections send, and never answers.
struct Tripwire {
    addr: std::net::SocketAddr,
    connections: Arc<Mutex<usize>>,
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Tripwire {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(Mutex::new(0));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (conn_count, captured) = (Arc::clone(&connections), Arc::clone(&bytes));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                *conn_count.lock().unwrap() += 1;
                let captured = Arc::clone(&captured);
                // Read until the peer goes quiet, then drop the socket so a
                // client that did connect fails fast instead of hanging.
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(Ok(n)) =
                        tokio::time::timeout(Duration::from_millis(200), socket.read(&mut buf))
                            .await
                    {
                        if n == 0 {
                            break;
                        }
                        captured.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                });
            }
        });
        Self {
            addr,
            connections,
            bytes,
        }
    }

    /// Region values that redirect an unchecked host template to this
    /// listener: userinfo (`@`) moves the real host after it, and a
    /// trailing `/` or `#` detaches the template's own suffix.
    fn hostile_regions(&self) -> [String; 3] {
        [
            format!("x@{}/", self.addr),
            format!("x@{}#", self.addr),
            format!("x@{}?", self.addr),
        ]
    }

    /// Settle any in-flight accept/read, then assert nothing arrived.
    async fn assert_untouched(&self, context: &str) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let bytes = self.bytes.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(
            *self.connections.lock().unwrap(),
            0,
            "{context}: a connection reached the tripwire; bytes: {text}"
        );
        assert!(!text.contains(BEARER_KEY), "{context}: bearer key leaked");
        assert!(!text.contains(ACCESS_KEY), "{context}: access key leaked");
        assert!(!text.contains(PROMPT), "{context}: request body leaked");
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(PROMPT.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(16),
        ..Default::default()
    }
}

fn credential_shapes() -> [BedrockCreds; 2] {
    [
        BedrockCreds::BearerKey {
            key: BEARER_KEY.into(),
        },
        BedrockCreds::Static {
            access_key: ACCESS_KEY.into(),
            secret_key: "tripwire-secret-key".into(),
            session_token: Some("tripwire-session-token".into()),
        },
    ]
}

/// Credentials resolved under a canonical region, so the hostile region is
/// only ever seen by the provider under test.
async fn resolved(creds: &BedrockCreds) -> ResolvedCreds {
    resolve(creds, "us-west-2").await.unwrap()
}

fn assert_region_refusal<T: std::fmt::Debug>(result: &routectl_core::Result<T>, context: &str) {
    match result {
        Err(Error::Config(msg)) => assert!(msg.contains("region"), "{context}: {msg}"),
        other => panic!("{context}: expected a region refusal, got {other:?}"),
    }
}

fn native_provider(
    region: &str,
    creds: BedrockCreds,
    resolved: ResolvedCreds,
    shape: BedrockApiShape,
) -> BedrockProvider {
    BedrockProvider::new(
        BedrockConfig {
            id: "bedrock:tripwire".into(),
            region: region.into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: shape,
            creds,
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            allowed_betas: Vec::new(),
            allowed_body_fields: Vec::new(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
        },
        resolved,
    )
}

#[tokio::test]
async fn tripwire_positive_control_observes_a_plaintext_request() {
    let tripwire = Tripwire::start().await;

    let _ = reqwest::Client::new()
        .post(format!("http://{}/probe", tripwire.addr))
        .timeout(Duration::from_millis(300))
        .body(PROMPT)
        .send()
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(*tripwire.connections.lock().unwrap(), 1);
    assert!(String::from_utf8_lossy(&tripwire.bytes.lock().unwrap()).contains(PROMPT));
}

#[tokio::test]
async fn native_bedrock_refuses_a_host_altering_region_on_every_operation() {
    let tripwire = Tripwire::start().await;

    for region in tripwire.hostile_regions() {
        for creds in credential_shapes() {
            for shape in [BedrockApiShape::Invoke, BedrockApiShape::Converse] {
                let context = format!("region={region:?} creds={creds:?} shape={shape:?}");
                let provider =
                    native_provider(&region, creds.clone(), resolved(&creds).await, shape);

                let complete = provider.complete(request()).await;
                let stream = provider.stream(request()).await.map(|_| ());
                let count = provider.count_tokens(request()).await;

                assert_region_refusal(&complete, &format!("complete {context}"));
                assert_region_refusal(&stream, &format!("stream {context}"));
                assert_region_refusal(&count, &format!("count_tokens {context}"));
            }
        }
    }

    tripwire.assert_untouched("native bedrock").await;
}

/// The mantle lanes take their host from the caller's `base_url`; a
/// library caller composing it by hand from the region reproduces the
/// unchecked template. The region still scopes signing, so the signer is
/// the boundary that must refuse. Plain `http` so that, were the signer to
/// pass, the tripwire would read the auth header in the clear.
fn hand_built_mantle_base(region: &str, vocabulary: &str) -> String {
    format!("http://bedrock-mantle.{region}.api.aws{vocabulary}")
}

fn anthropic_mantle(region: &str, resolved: ResolvedCreds) -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "mantle:tripwire".into(),
        auth: Arc::new(StaticToken::new("")),
        base_url: hand_built_mantle_base(region, "/anthropic"),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AnthropicAuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        mantle: Some(MantleAuth {
            region: region.into(),
            creds: resolved,
        }),
    })
}

fn compat_mantle(region: &str, resolved: ResolvedCreds) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "mantle-compat:tripwire".into(),
        base_url: hand_built_mantle_base(region, "/openai/v1"),
        api_key: String::new(),
        header_extras: Vec::new(),
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
        mantle: Some(MantleAuth {
            region: region.into(),
            creds: resolved,
        }),
    })
}

fn responses_mantle(region: &str, resolved: ResolvedCreds) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(OpenAiResponsesConfig {
        id: "mantle-responses:tripwire".into(),
        auth: Arc::new(StaticToken::new("")),
        account_id: None,
        base_url: hand_built_mantle_base(region, "/openai/v1"),
        auth_kind: ResponsesAuthKind::BedrockMantle,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
        installation_id: None,
        mantle: Some(MantleAuth {
            region: region.into(),
            creds: resolved,
        }),
    })
}

#[tokio::test]
async fn every_mantle_lane_refuses_a_host_altering_region_before_signing() {
    let tripwire = Tripwire::start().await;

    for region in tripwire.hostile_regions() {
        for creds in credential_shapes() {
            let context = format!("region={region:?} creds={creds:?}");
            let lanes: [(&str, Box<dyn Provider>); 3] = [
                (
                    "anthropic",
                    Box::new(anthropic_mantle(&region, resolved(&creds).await)),
                ),
                (
                    "openai-compat",
                    Box::new(compat_mantle(&region, resolved(&creds).await)),
                ),
                (
                    "openai-responses",
                    Box::new(responses_mantle(&region, resolved(&creds).await)),
                ),
            ];

            for (lane, provider) in lanes {
                let complete = provider.complete(request()).await;
                let stream = provider.stream(request()).await.map(|_| ());

                assert_region_refusal(&complete, &format!("{lane} complete {context}"));
                assert_region_refusal(&stream, &format!("{lane} stream {context}"));
            }
            let count = anthropic_mantle(&region, resolved(&creds).await)
                .count_tokens(request())
                .await;
            assert_region_refusal(&count, &format!("anthropic count_tokens {context}"));
        }
    }

    tripwire.assert_untouched("mantle lanes").await;
}
