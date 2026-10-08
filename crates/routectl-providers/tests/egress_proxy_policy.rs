//! Process-isolated proof of the provider egress proxy policy: a loopback
//! provider target is dialed directly and never through an
//! environment-configured proxy, over `http://` and `https://` alike, while a
//! non-loopback target keeps reqwest's stock system-proxy discovery for both
//! schemes.
//!
//! # Why this is its own test binary
//!
//! reqwest reads `HTTP_PROXY` / `ALL_PROXY` / `HTTPS_PROXY` (and their
//! lower-case spellings, `NO_PROXY`, and the CGI `REQUEST_METHOD` guard) from
//! the PROCESS environment when a client is built. Setting them inside a
//! shared test binary would redirect unrelated siblings' HTTP calls, so this
//! binary carries only these cases, pulls in no shared harness, and marks every
//! case `#[serial_test::serial]` so no two of them touch the environment at
//! once.
//!
//! Every case clears all competing proxy variables before setting the one
//! under test. An inherited `NO_PROXY` covering localhost would otherwise make
//! reqwest bypass the proxy for its own reasons and pass the loopback cases
//! vacuously on a developer machine that happens to export one.

#![cfg(all(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses",
    feature = "gemini"
))]

use std::sync::{Arc, Mutex};

use routectl_core::{ChatRequest, Provider};
use routectl_testkit::ScopedEnv;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every variable reqwest's system-proxy matcher consults.
const PROXY_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "REQUEST_METHOD",
];

/// Proxy variables that apply to an `http://` destination.
const CLEARTEXT_PROXY_KEYS: &[&str] = &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"];

/// Proxy variables that apply to an `https://` destination.
const TLS_PROXY_KEYS: &[&str] = &["HTTPS_PROXY", "https_proxy"];

/// Synthetic external authority for the first-party HTTPS controls. The
/// proxy trap answers the CONNECT itself, so nothing ever resolves it.
const SYNTHETIC_HOST: &str = "provider.example.test";

const REGION: &str = "us-west-2";

/// Clear every proxy variable, then set exactly `key` to `proxy_url`.
fn proxy_env_only(key: &str, proxy_url: &str) -> Vec<ScopedEnv> {
    let mut guards: Vec<ScopedEnv> = PROXY_ENV_KEYS.iter().map(ScopedEnv::unset).collect();
    guards.push(ScopedEnv::set(key, proxy_url));
    guards
}

/// A per-case scratch directory under Cargo's integration-test tmpdir,
/// removed on drop. Holds the cookie jar the cookie-backed client persists.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(case: &str) -> Self {
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("egress-proxy-policy-{case}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A proxy stand-in that records the request line of every connection it
/// accepts and then refuses it. Records a CONNECT tunnel request without ever
/// dialing the named authority, so the HTTPS controls need no network.
struct ConnectTrap {
    url: String,
    request_lines: Arc<Mutex<Vec<String>>>,
}

impl ConnectTrap {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind trap");
        let addr = listener.local_addr().expect("trap addr");
        let request_lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&request_lines);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let head = read_request_head(&mut socket).await;
                let first = head.lines().next().unwrap_or_default().to_string();
                sink.lock().expect("trap lock").push(first);
                let _ = socket
                    .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });
        Self {
            url: format!("http://{addr}"),
            request_lines,
        }
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.request_lines.lock().expect("trap lock"))
    }
}

async fn read_request_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// A wiremock server that answers anything with a non-retryable 400.
async fn answering_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(400).set_body_string("{}"))
        .mount(&server)
        .await;
    server
}

async fn hits(server: &MockServer) -> usize {
    server.received_requests().await.expect("recording").len()
}

fn chat_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![routectl_core::test_utils::user_msg("ping")].into(),
        max_tokens: Some(16),
        ..Default::default()
    }
}

async fn mantle_auth() -> routectl_providers::mantle::MantleAuth {
    let creds = routectl_providers::bedrock::auth::resolve(
        &routectl_providers::bedrock::BedrockCreds::BearerKey {
            key: "proxy-policy-bearer".into(),
        },
        REGION,
    )
    .await
    .expect("bearer creds resolve offline");
    routectl_providers::mantle::MantleAuth {
        region: REGION.into(),
        creds,
    }
}

/// One provider transport: a constructor plus the single call it makes.
#[derive(Debug, Clone, Copy)]
enum Lane {
    CompatComplete,
    CompatProbe,
    CompatMantle,
    AnthropicComplete,
    AnthropicProbe,
    AnthropicMantle,
    ResponsesCookieBacked,
    ResponsesCookieLess,
    ResponsesProbe,
    ResponsesMantle,
    GeminiComplete,
}

/// Lanes whose base URL is operator-configured and so can name a loopback
/// cleartext target.
const CONFIGURABLE_LANES: &[Lane] = &[
    Lane::CompatComplete,
    Lane::CompatProbe,
    Lane::CompatMantle,
    Lane::AnthropicComplete,
    Lane::AnthropicProbe,
    Lane::AnthropicMantle,
    Lane::ResponsesCookieBacked,
    Lane::ResponsesCookieLess,
    Lane::ResponsesProbe,
    Lane::ResponsesMantle,
    Lane::GeminiComplete,
];

impl Lane {
    /// The base URL the router's factory resolves for this lane in
    /// production, when that URL is derived rather than configured.
    fn resolved_https_base(self) -> Option<String> {
        match self {
            Self::CompatMantle => Some(routectl_providers::mantle::mantle_openai_base(REGION)),
            Self::AnthropicMantle => {
                Some(routectl_providers::mantle::mantle_anthropic_base(REGION))
            }
            Self::ResponsesMantle => Some(routectl_providers::mantle::mantle_openai_base(REGION)),
            _ => None,
        }
        .map(|base| base.expect("REGION is a canonical AWS region"))
    }

    /// Construct this lane's provider against `base_url` and issue its one
    /// call. The outcome is irrelevant: the servers record what arrived.
    async fn fire(self, base_url: &str, scratch: &std::path::Path) {
        let _cookie_env = self.cookie_env(scratch);
        match self {
            Self::CompatComplete => {
                let _ = compat(base_url, false)
                    .await
                    .complete(chat_request("m"))
                    .await;
            }
            Self::CompatProbe => {
                let _ = compat(base_url, false).await.probe().await;
            }
            Self::CompatMantle => {
                let _ = compat(base_url, true)
                    .await
                    .complete(chat_request("m"))
                    .await;
            }
            Self::AnthropicComplete => {
                let _ = anthropic(base_url, false)
                    .await
                    .complete(chat_request("claude-haiku-4-5"))
                    .await;
            }
            Self::AnthropicProbe => {
                let _ = anthropic(base_url, false).await.probe().await;
            }
            Self::AnthropicMantle => {
                let _ = anthropic(base_url, true)
                    .await
                    .complete(chat_request("claude-haiku-4-5"))
                    .await;
            }
            Self::ResponsesCookieBacked | Self::ResponsesCookieLess => {
                let _ = responses(base_url, false)
                    .await
                    .complete(chat_request("gpt-5"))
                    .await;
            }
            Self::ResponsesProbe => {
                let _ = responses(base_url, false).await.probe().await;
            }
            Self::ResponsesMantle => {
                let _ = responses(base_url, true)
                    .await
                    .complete(chat_request("gpt-5"))
                    .await;
            }
            Self::GeminiComplete => {
                let _ = gemini(base_url)
                    .complete(chat_request("gemini-2.5-pro"))
                    .await;
            }
        }
    }

    /// Select the openai-responses client branch: a resolvable jar path
    /// builds the cookie-backed client, no resolvable path the plain one.
    fn cookie_env(self, scratch: &std::path::Path) -> Vec<ScopedEnv> {
        match self {
            Self::ResponsesCookieBacked => vec![ScopedEnv::set(
                "ROUTECTL_COOKIE_FILE",
                scratch.join("chatgpt.json"),
            )],
            Self::ResponsesCookieLess | Self::ResponsesProbe => vec![
                ScopedEnv::unset("ROUTECTL_COOKIE_FILE"),
                ScopedEnv::unset("HOME"),
            ],
            _ => Vec::new(),
        }
    }
}

async fn compat(
    base_url: &str,
    mantle: bool,
) -> routectl_providers::openai_compat::OpenAiCompatProvider {
    use routectl_providers::openai_compat::{
        HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
    };
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "compat-proxy-policy".into(),
        base_url: base_url.into(),
        api_key: if mantle {
            String::new()
        } else {
            "compat-key".into()
        },
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
        mantle: if mantle {
            Some(mantle_auth().await)
        } else {
            None
        },
    })
}

async fn anthropic(
    base_url: &str,
    mantle: bool,
) -> routectl_providers::anthropic_api::AnthropicApiProvider {
    use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
    let mut cfg = AnthropicApiConfig::new(
        "anthropic-proxy-policy",
        if mantle { "" } else { "anthropic-key" },
    );
    cfg.base_url = base_url.into();
    if mantle {
        cfg.mantle = Some(mantle_auth().await);
    }
    AnthropicApiProvider::new(cfg)
}

async fn responses(
    base_url: &str,
    mantle: bool,
) -> routectl_providers::openai_responses::OpenAiResponsesProvider {
    use routectl_providers::openai_responses::{
        AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
    };
    let mut cfg = OpenAiResponsesConfig::new(
        "responses-proxy-policy",
        if mantle { "" } else { "responses-key" },
    );
    cfg.base_url = base_url.into();
    cfg.auth_kind = AuthKind::ApiKey;
    if mantle {
        cfg.auth_kind = AuthKind::BedrockMantle;
        cfg.mantle = Some(mantle_auth().await);
    }
    OpenAiResponsesProvider::new(cfg)
}

fn gemini(base_url: &str) -> routectl_providers::gemini::GeminiProvider {
    use routectl_providers::gemini::{GeminiConfig, GeminiProvider};
    let mut cfg = GeminiConfig::new("gemini-proxy-policy", "gemini-key");
    cfg.base_url = base_url.into();
    GeminiProvider::new(cfg)
}

/// Premise, asserted rather than assumed: with only `key` set, a stock
/// client DOES send a loopback cleartext request to the proxy. If reqwest
/// stopped reading the variable, the bypass assertions would go vacuous.
async fn assert_stock_client_uses_cleartext_proxy(key: &str, proxy: &MockServer) {
    let stock = reqwest::Client::builder().build().expect("stock client");
    let _ = stock.get("http://127.0.0.1:1/premise").send().await;
    assert_eq!(
        hits(proxy).await,
        1,
        "premise: a stock client must route loopback http through {key}"
    );
    proxy.reset().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(400).set_body_string("{}"))
        .mount(proxy)
        .await;
}

async fn assert_loopback_lanes_bypass(key: &str) {
    let scratch = Scratch::new(key);
    let proxy = answering_server().await;
    let _env = proxy_env_only(key, &proxy.uri());
    assert_stock_client_uses_cleartext_proxy(key, &proxy).await;

    for &lane in CONFIGURABLE_LANES {
        // Arrange
        let target = answering_server().await;

        // Act
        lane.fire(&target.uri(), scratch.path()).await;

        // Assert
        assert_eq!(
            hits(&proxy).await,
            0,
            "{lane:?}: {key} must see no part of a loopback cleartext request"
        );
        assert_eq!(
            hits(&target).await,
            1,
            "{lane:?}: the loopback target must receive the request directly"
        );
    }
}

/// With `HTTP_PROXY` (either spelling) set and `NO_PROXY` absent, every
/// provider transport pointed at a loopback `http://` base URL reaches the
/// loopback listener and nothing reaches the proxy.
#[tokio::test]
#[serial_test::serial]
async fn http_proxy_never_receives_loopback_cleartext_provider_requests() {
    for key in &CLEARTEXT_PROXY_KEYS[..2] {
        assert_loopback_lanes_bypass(key).await;
    }
}

/// Same contract under `ALL_PROXY` (either spelling), which reqwest applies
/// to every scheme.
#[tokio::test]
#[serial_test::serial]
async fn all_proxy_never_receives_loopback_cleartext_provider_requests() {
    for key in &CLEARTEXT_PROXY_KEYS[2..] {
        assert_loopback_lanes_bypass(key).await;
    }
}

/// Premise for the HTTPS controls: a stock client sends a CONNECT to the
/// trap, so the trap can observe the tunnel request at all.
async fn assert_stock_client_tunnels(key: &str, trap: &ConnectTrap) {
    let stock = reqwest::Client::builder().build().expect("stock client");
    let _ = stock.get("https://premise.example.test/").send().await;
    let lines = trap.take();
    assert_eq!(
        lines,
        vec!["CONNECT premise.example.test:443 HTTP/1.1".to_string()],
        "premise: a stock client must tunnel https through {key}"
    );
}

fn assert_one_connect(lines: &[String], authority: &str, what: &str) {
    assert_eq!(
        lines,
        [format!("CONNECT {authority} HTTP/1.1")],
        "{what}: the configured https proxy must receive exactly one CONNECT \
         for the upstream authority"
    );
}

/// Existing HTTPS proxy support is unchanged: with `HTTPS_PROXY` (either
/// spelling) set, every provider transport tunnels its https request through
/// the proxy -- a synthetic first-party authority, the region-derived mantle
/// endpoints, and the native Bedrock runtime endpoint alike.
#[tokio::test]
#[serial_test::serial]
async fn https_proxy_still_receives_connect_for_external_https_targets() {
    let scratch = Scratch::new("https");
    for key in TLS_PROXY_KEYS {
        let trap = ConnectTrap::start().await;
        let _env = proxy_env_only(key, &trap.url);
        assert_stock_client_tunnels(key, &trap).await;

        for &lane in CONFIGURABLE_LANES {
            // Arrange
            let base = lane
                .resolved_https_base()
                .unwrap_or_else(|| format!("https://{SYNTHETIC_HOST}/v1"));
            let authority = format!(
                "{}:443",
                reqwest::Url::parse(&base)
                    .expect("base parses")
                    .host_str()
                    .expect("base host")
            );

            // Act
            lane.fire(&base, scratch.path()).await;

            // Assert
            assert_one_connect(&trap.take(), &authority, &format!("{lane:?} via {key}"));
        }

        bedrock_complete().await;
        assert_one_connect(
            &trap.take(),
            &format!("bedrock-runtime.{REGION}.amazonaws.com:443"),
            &format!("native bedrock via {key}"),
        );
    }
}

/// A direct library caller's non-loopback cleartext target keeps stock
/// system-proxy behavior: with `HTTP_PROXY` or `ALL_PROXY` (either spelling)
/// set, every provider transport sends its request to the proxy. The target
/// is a synthetic name nothing resolves, so the proxy is the only place the
/// request can land.
#[tokio::test]
#[serial_test::serial]
async fn cleartext_proxy_still_receives_non_loopback_http_provider_requests() {
    let scratch = Scratch::new("non-loopback-http");
    for key in CLEARTEXT_PROXY_KEYS {
        let proxy = answering_server().await;
        let _env = proxy_env_only(key, &proxy.uri());

        for &lane in CONFIGURABLE_LANES {
            // Arrange
            let base = format!("http://{SYNTHETIC_HOST}/v1");

            // Act
            lane.fire(&base, scratch.path()).await;

            // Assert
            let received = proxy.received_requests().await.expect("recording");
            assert_eq!(
                received.len(),
                1,
                "{lane:?} via {key}: a non-loopback http request must go to the proxy"
            );
            assert_eq!(
                received[0].url.host_str(),
                Some(SYNTHETIC_HOST),
                "{lane:?} via {key}: the proxy must see the upstream authority"
            );
            proxy.reset().await;
            Mock::given(any())
                .respond_with(ResponseTemplate::new(400).set_body_string("{}"))
                .mount(&proxy)
                .await;
        }
    }
}

/// A local listener that counts accepted connections and drops each one.
/// Stands in for a loopback `https://` upstream: the TLS handshake fails, but
/// the accepted connection proves the client dialed it directly.
struct ConnectionCounter {
    port: u16,
    accepted: Arc<Mutex<usize>>,
}

impl ConnectionCounter {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind counter");
        let port = listener.local_addr().expect("counter addr").port();
        let accepted = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&accepted);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                *sink.lock().expect("counter lock") += 1;
                drop(socket);
            }
        });
        Self { port, accepted }
    }

    fn take(&self) -> usize {
        std::mem::take(&mut *self.accepted.lock().expect("counter lock"))
    }
}

/// With `HTTPS_PROXY` or `ALL_PROXY` (either spelling) set, every provider
/// transport pointed at a loopback `https://` base URL dials the loopback
/// listener itself and never asks the proxy for a tunnel.
#[tokio::test]
#[serial_test::serial]
async fn tls_proxy_never_receives_loopback_https_provider_requests() {
    let scratch = Scratch::new("loopback-https");
    for key in TLS_PROXY_KEYS.iter().chain(&CLEARTEXT_PROXY_KEYS[2..]) {
        let trap = ConnectTrap::start().await;
        let _env = proxy_env_only(key, &trap.url);
        assert_stock_client_tunnels(key, &trap).await;

        for &lane in CONFIGURABLE_LANES {
            // Arrange
            let upstream = ConnectionCounter::start().await;
            let base = format!("https://127.0.0.1:{}/v1", upstream.port);

            // Act
            lane.fire(&base, scratch.path()).await;

            // Assert
            assert_eq!(
                trap.take(),
                Vec::<String>::new(),
                "{lane:?} via {key}: a loopback https request must not tunnel"
            );
            assert!(
                upstream.take() >= 1,
                "{lane:?} via {key}: the loopback https listener must be dialed directly"
            );
        }
    }
}

async fn bedrock_complete() {
    use routectl_providers::bedrock::{
        BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider,
    };
    let creds = BedrockCreds::BearerKey {
        key: "proxy-policy-bearer".into(),
    };
    let resolved = routectl_providers::bedrock::auth::resolve(&creds, REGION)
        .await
        .expect("bearer creds resolve offline");
    let provider = BedrockProvider::new(
        BedrockConfig {
            id: "bedrock-proxy-policy".into(),
            region: REGION.into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShape::Invoke,
            creds,
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            allowed_betas: Vec::new(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
        },
        resolved,
    )
    .expect("REGION is a canonical AWS region");
    let _ = provider.complete(chat_request("claude-haiku-4-5")).await;
}
