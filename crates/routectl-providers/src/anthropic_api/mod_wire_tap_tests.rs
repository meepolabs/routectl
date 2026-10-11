//! The bytes each send site deposits on the attempt's wire tap are exactly
//! the bytes the upstream received, on both the complete and the stream
//! path, including the billing checksum re-signed after serialization.

use std::sync::{Arc, Mutex, PoisonError};

use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use routectl_core::{
    ChatRequest, Message, MessageContent, Provider, Role, SystemBlock, SystemContent,
};
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;

const PROVIDER_ID: &str = "deposit-test";

/// A billing marker whose checksum the re-signer rewrites. Every billing
/// carrier inside `system` or `messages` is stripped before serialization,
/// so the fixture carries it in a tool description, which serializes after
/// `system` where the re-signer looks.
const BILLING: &str = "x-anthropic-billing-header: cc_version=1; cch=abcde;";

const COMPLETE_BODY: &str = r#"{"id":"msg_d","type":"message","role":"assistant","model":"claude-x","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}}"#;

const STREAM_BODY: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_d\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-x\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

struct Capture {
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    stream: bool,
}

impl Respond for Capture {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.bodies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.body.clone());
        if self.stream {
            ResponseTemplate::new(200)
                .set_body_string(STREAM_BODY)
                .append_header("content-type", "text/event-stream")
        } else {
            ResponseTemplate::new(200).set_body_raw(COMPLETE_BODY, "application/json")
        }
    }
}

async fn upstream(stream: bool) -> (MockServer, Arc<Mutex<Vec<Vec<u8>>>>) {
    let server = MockServer::start().await;
    let bodies = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(Capture {
            bodies: Arc::clone(&bodies),
            stream,
        })
        .mount(&server)
        .await;
    (server, bodies)
}

/// The cloak lane: an OAuth seat on the exact Anthropic host, resolved to
/// the mock, so the cloak and the checksum re-sign both run.
fn cloak_lane_provider(server: &MockServer) -> AnthropicApiProvider {
    let addr = *server.address();
    let mut provider = AnthropicApiProvider::new(AnthropicApiConfig {
        id: PROVIDER_ID.into(),
        auth: Arc::new(routectl_core::StaticToken::new("oat-token")),
        base_url: format!("http://api.anthropic.com:{}", addr.port()),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: Some("session-stable".into()),
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    });
    provider.client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve("api.anthropic.com", addr)
        .build()
        .expect("loopback client");
    provider
}

fn text(role: Role, body: &str) -> Message {
    Message {
        refusal: None,
        role,
        content: MessageContent::Text(body.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A genuine Claude Code request whose transmitted bytes hold a checksum to
/// re-sign.
fn billing_request(stream: bool) -> (ChatRequest, WireTap) {
    let tap = WireTap::default();
    let mut req = ChatRequest {
        model: "claude-x".into(),
        system: Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "rules".into(),
            cache_control: None,
            citations: None,
        }])),
        messages: vec![text(Role::User, "hi")].into(),
        tools: Some(vec![routectl_core::ToolDef::Custom(
            routectl_core::CustomTool {
                name: "mcp__lookup".into(),
                description: Some(BILLING.into()),
                input_schema: serde_json::json!({"type": "object"}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            },
        )]),
        max_tokens: Some(64),
        stream: stream.then_some(true),
        ..Default::default()
    };
    req.routectl_internal.claude_code_headers =
        vec![("x-claude-code-session-id".into(), "s".into())];
    req.routectl_internal.wire_tap = Some(tap.clone());
    (req, tap)
}

fn received(bodies: &Arc<Mutex<Vec<Vec<u8>>>>) -> Vec<u8> {
    let bodies = bodies.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(bodies.len(), 1, "exactly one upstream call");
    bodies.first().cloned().unwrap_or_default()
}

fn assert_deposit_is_the_wire(tap: &WireTap, wire: &[u8]) {
    let contents = tap.take_request();
    let deposited = contents
        .request()
        .expect("the send site deposited the body");
    assert_eq!(
        deposited.as_ref(),
        wire,
        "deposit differs from the transmitted body"
    );
    let text = String::from_utf8_lossy(wire);
    assert!(
        text.contains("cch="),
        "fixture must carry a checksum: {text}"
    );
    assert!(
        !text.contains("cch=abcde"),
        "the transmitted checksum must be the re-signed one: {text}"
    );
}

#[tokio::test]
#[serial_test::serial(anthropic_api_cloak_split, anthropic_api_cloak_policy_actions)]
async fn complete_deposits_the_resigned_bytes_the_upstream_received() {
    let (server, bodies) = upstream(false).await;
    let provider = cloak_lane_provider(&server);
    let (req, tap) = billing_request(false);

    provider.complete(req).await.expect("completes");

    assert_deposit_is_the_wire(&tap, &received(&bodies));
}

#[tokio::test]
#[serial_test::serial(anthropic_api_cloak_split, anthropic_api_cloak_policy_actions)]
async fn stream_deposits_the_resigned_bytes_the_upstream_received() {
    let (server, bodies) = upstream(true).await;
    let provider = cloak_lane_provider(&server);
    let (req, tap) = billing_request(true);

    let mut stream = provider.stream(req).await.expect("stream opens");
    while futures::StreamExt::next(&mut stream).await.is_some() {}

    assert_deposit_is_the_wire(&tap, &received(&bodies));
}

#[tokio::test]
#[serial_test::serial(anthropic_api_cloak_split, anthropic_api_cloak_policy_actions)]
async fn an_untapped_request_sends_the_same_bytes() {
    let (server, bodies) = upstream(false).await;
    let provider = cloak_lane_provider(&server);
    let (tapped, _tap) = billing_request(false);
    let mut untapped = tapped.clone();
    untapped.routectl_internal.wire_tap = None;

    provider.complete(tapped).await.expect("completes");
    provider.complete(untapped).await.expect("completes");

    let bodies = bodies.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies.first(), bodies.get(1));
}

fn site_edits(tap: &WireTap, site: EditSite) -> Vec<(EditKind, String)> {
    tap.take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|edit| edit.site() == site)
        .map(|edit| (edit.kind(), edit.path().to_string()))
        .collect()
}

/// Drive one call through the cloak lane; `stream` picks the path.
async fn send(req: ChatRequest, stream: bool) {
    let (server, _bodies) = upstream(stream).await;
    let provider = cloak_lane_provider(&server);
    if stream {
        let mut chunks = provider.stream(req).await.expect("stream opens");
        while futures::StreamExt::next(&mut chunks).await.is_some() {}
    } else {
        provider.complete(req).await.expect("completes");
    }
}

fn plain_request(stream: Option<bool>) -> (ChatRequest, WireTap) {
    let (mut req, tap) = billing_request(false);
    req.tools = None;
    req.stream = stream;
    (req, tap)
}

struct DispatchRow {
    name: &'static str,
    site: EditSite,
    stream: bool,
    firing: fn(&mut ChatRequest),
    idle: fn(&mut ChatRequest),
    expected: &'static [(EditKind, &'static str)],
}

/// Each dispatch-site strip, on each path: the edit is declared when it
/// fires and absent when it does not.
#[tokio::test]
#[serial_test::serial(anthropic_api_cloak_split, anthropic_api_cloak_policy_actions)]
async fn each_dispatch_site_declares_its_edit_only_when_it_fires() {
    let rows = [
        DispatchRow {
            name: "complete strips the client stream flag",
            site: EditSite::StreamStrip,
            stream: false,
            firing: |req| req.stream = Some(false),
            idle: |req| req.stream = None,
            expected: &[(EditKind::Remove, "/stream")],
        },
        DispatchRow {
            name: "stream adds the stream flag the client did not send",
            site: EditSite::StreamFlagForce,
            stream: true,
            firing: |req| req.stream = None,
            idle: |req| req.stream = Some(true),
            expected: &[(EditKind::Add, "/stream")],
        },
        DispatchRow {
            name: "complete strips a body anthropic_beta",
            site: EditSite::AnthropicBetaStrip,
            stream: false,
            firing: |req| req.anthropic_beta = vec!["some-beta-2099-01-01".into()],
            idle: |_| {},
            expected: &[(EditKind::Remove, "/anthropic_beta")],
        },
        DispatchRow {
            name: "stream strips a body anthropic_beta",
            site: EditSite::AnthropicBetaStrip,
            stream: true,
            firing: |req| req.anthropic_beta = vec!["some-beta-2099-01-01".into()],
            idle: |_| {},
            expected: &[(EditKind::Remove, "/anthropic_beta")],
        },
        DispatchRow {
            name: "the seat's sampling strip removes temperature",
            site: EditSite::ClaudeSamplingNormalize,
            stream: false,
            firing: |req| req.temperature = Some(0.5),
            idle: |_| {},
            expected: &[(EditKind::Remove, "/temperature")],
        },
    ];
    for row in rows {
        let (mut fired_req, fired_tap) = plain_request(None);
        (row.firing)(&mut fired_req);
        let (mut idle_req, idle_tap) = plain_request(None);
        (row.idle)(&mut idle_req);

        send(fired_req, row.stream).await;
        send(idle_req, row.stream).await;

        let want: Vec<(EditKind, String)> = row
            .expected
            .iter()
            .map(|(kind, path)| (*kind, (*path).to_string()))
            .collect();
        assert_eq!(
            site_edits(&fired_tap, row.site),
            want,
            "{}: firing",
            row.name
        );
        assert!(
            site_edits(&idle_tap, row.site).is_empty(),
            "{}: idle",
            row.name
        );
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_api_cloak_split, anthropic_api_cloak_policy_actions)]
async fn the_stream_sampling_strip_is_declared() {
    let (mut req, tap) = plain_request(None);
    req.top_p = Some(0.5);

    send(req, true).await;

    assert_eq!(
        site_edits(&tap, EditSite::ClaudeSamplingNormalize),
        [(EditKind::Remove, "/top_p".to_string())]
    );
}
