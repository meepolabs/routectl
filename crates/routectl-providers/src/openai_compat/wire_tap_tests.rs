//! The wire tap on the openai-compat egress: every send deposits the exact
//! bytes the upstream received, and every body edit this egress makes is
//! declared at its site.

use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite, Leg, TapContents, WireTap};
use routectl_core::{
    CacheControl, ChatRequest, ContentPart, KnownContentPart, Message, MessageContent,
    ReasoningConfig, Role, SystemContent, ToolDef,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect};
use routectl_core::Provider;

fn config(base_url: &str, dialect: ReasoningDialect) -> OpenAiCompatConfig {
    OpenAiCompatConfig {
        id: "compat-tap".into(),
        base_url: base_url.into(),
        api_key: "test-key".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: dialect,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    }
}

fn provider(dialect: ReasoningDialect) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(config("http://127.0.0.1:9", dialect))
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

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![text(Role::User, "hi")].into(),
        ..Default::default()
    }
}

fn tapped(mut req: ChatRequest) -> (ChatRequest, WireTap) {
    let wire_tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(wire_tap.clone());
    (req, wire_tap)
}

/// The request-leg edits a normalize declared, as (site, pattern, kind).
fn declared(
    provider: &OpenAiCompatProvider,
    req: ChatRequest,
) -> Vec<(EditSite, String, EditKind)> {
    let (req, wire_tap) = tapped(req);
    provider.normalize_request(&req).expect("normalize");
    edits(&wire_tap.take_request())
}

fn edits(contents: &TapContents) -> Vec<(EditSite, String, EditKind)> {
    contents
        .edits(Leg::Request)
        .iter()
        .map(|e: &DeclaredEdit| (e.site(), e.path().to_string(), e.kind()))
        .collect()
}

fn has(
    edits: &[(EditSite, String, EditKind)],
    site: EditSite,
    pattern: &str,
    kind: EditKind,
) -> bool {
    edits
        .iter()
        .any(|(s, p, k)| *s == site && p == pattern && *k == kind)
}

fn of_site(edits: &[(EditSite, String, EditKind)], site: EditSite) -> usize {
    edits.iter().filter(|(s, _, _)| *s == site).count()
}

fn ok_completion() -> Value {
    json!({
        "id": "c1", "model": "m", "created": 1,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]
    })
}

fn sse_body() -> &'static str {
    concat!(
        "data: {\"id\":\"c1\",\"model\":\"m\",\"created\":1,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    )
}

async fn received_body(server: &MockServer) -> (Vec<u8>, Option<String>) {
    let requests = server.received_requests().await.expect("recording on");
    let last = requests.last().expect("one request");
    let content_type = last
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (last.body.clone(), content_type)
}

#[tokio::test]
async fn complete_deposits_the_exact_bytes_the_upstream_received() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion()))
        .mount(&server)
        .await;
    let provider = OpenAiCompatProvider::new(config(&server.uri(), ReasoningDialect::OpenAi));
    let (req, wire_tap) = tapped(request("gpt-4o"));

    provider.complete(req).await.expect("complete");

    let (body, _) = received_body(&server).await;
    let contents = wire_tap.take_request();
    assert_eq!(
        contents.request().map(|b| b.as_ref()),
        Some(body.as_slice())
    );
}

#[tokio::test]
async fn stream_deposits_the_exact_bytes_the_upstream_received() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;
    let provider = OpenAiCompatProvider::new(config(&server.uri(), ReasoningDialect::OpenAi));
    let (req, wire_tap) = tapped(request("gpt-4o"));

    let _stream = provider.stream(req).await.expect("stream open");

    let (body, _) = received_body(&server).await;
    let contents = wire_tap.take_request();
    assert_eq!(
        contents.request().map(|b| b.as_ref()),
        Some(body.as_slice())
    );
}

/// The bytes and content-type the old `.json(&body)` builder produced, so
/// the switch to an explicit body is pinned against the builder it replaced
/// rather than against itself.
fn json_builder_reference(body: &Value) -> (Vec<u8>, Option<String>) {
    let request = reqwest::Client::new()
        .post("http://127.0.0.1:9/chat/completions")
        .json(body)
        .build()
        .expect("reference request");
    let bytes = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .expect("buffered body")
        .to_vec();
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (bytes, content_type)
}

#[tokio::test]
async fn first_party_body_and_content_type_match_the_json_builder() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion()))
        .mount(&server)
        .await;
    let provider = OpenAiCompatProvider::new(config(&server.uri(), ReasoningDialect::OpenAi));
    let mut req = request("gpt-4o");
    req.temperature = Some(0.25);
    let mut expected_body = provider.normalize_request(&req).expect("normalize");
    expected_body["stream"] = Value::Bool(false);

    provider.complete(req).await.expect("complete");

    let (body, content_type) = received_body(&server).await;
    let (reference_body, reference_type) = json_builder_reference(&expected_body);
    assert_eq!(body, reference_body);
    assert_eq!(content_type, reference_type);
    assert_eq!(content_type.as_deref(), Some("application/json"));
}

#[test]
fn no_tap_records_nothing_and_changes_nothing() {
    let provider = provider(ReasoningDialect::OpenAi);
    let mut req = request("o3-mini");
    req.temperature = Some(0.5);
    let untapped = provider.normalize_request(&req).expect("normalize");
    let (tapped_req, wire_tap) = tapped(req);

    let with_tap = provider.normalize_request(&tapped_req).expect("normalize");

    assert_eq!(untapped, with_tap);
    assert!(!wire_tap.take_request().edits(Leg::Request).is_empty());
}

fn with_cache_marker() -> ChatRequest {
    let mut req = request("gpt-4o");
    req.messages = vec![Message {
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
            text: "cached".into(),
            citations: None,
            cache_control: Some(CacheControl::ephemeral_5m()),
        })]),
        ..text(Role::User, "")
    }]
    .into();
    req
}

fn assistant_with_reasoning() -> ChatRequest {
    let mut req = request("deepseek-chat");
    req.messages = vec![
        text(Role::User, "hi"),
        Message {
            reasoning: Some("thought".into()),
            ..text(Role::Assistant, "answer")
        },
        text(Role::User, "again"),
    ]
    .into();
    req
}

/// `req` on a model that declares only `low` and `high`.
fn clamped(mut req: ChatRequest) -> ChatRequest {
    req.routectl_internal.effort_levels = vec!["low".to_string(), "high".to_string()].into();
    req
}

fn with_effort(model: &str, effort: &str) -> ChatRequest {
    let mut req = request(model);
    req.reasoning = Some(ReasoningConfig {
        effort: Some(effort.into()),
        ..Default::default()
    });
    req
}

const FINGERPRINT: &str = "x-anthropic-billing-header: cc_version=1; cch=abc";

/// A request whose system carries the billing fingerprint block, followed
/// by `rest` as a second block when it is non-empty.
fn with_fingerprint(rest: &str) -> ChatRequest {
    let block = |text: &str| routectl_core::SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    };
    let system = if rest.is_empty() {
        SystemContent::Text(FINGERPRINT.into())
    } else {
        SystemContent::Blocks(vec![block(FINGERPRINT), block(rest)])
    };
    ChatRequest {
        system: Some(system),
        ..request("gpt-4o")
    }
}

/// One row per declaring site: the request that makes the site fire, the
/// declaration it must leave, and a sibling request on the same path that
/// must not leave it.
struct SiteRow {
    name: &'static str,
    dialect: ReasoningDialect,
    fires: fn() -> ChatRequest,
    quiet: fn() -> ChatRequest,
    site: EditSite,
    pattern: &'static str,
    kind: EditKind,
}

fn site_rows() -> Vec<SiteRow> {
    vec![
        SiteRow {
            name: "top-level cache_control strip",
            dialect: ReasoningDialect::OpenAi,
            fires: || ChatRequest {
                cache_control: Some(CacheControl::ephemeral_5m()),
                ..request("gpt-4o")
            },
            quiet: || request("gpt-4o"),
            site: EditSite::CacheMarkerStrip,
            pattern: "/cache_control",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "anthropic_beta strip",
            dialect: ReasoningDialect::OpenAi,
            fires: || ChatRequest {
                anthropic_beta: vec!["context-1m-2025-08-07".into()],
                ..request("gpt-4o")
            },
            quiet: || request("gpt-4o"),
            site: EditSite::AnthropicBetaStrip,
            pattern: "/anthropic_beta",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "chat_template_kwargs strip",
            dialect: ReasoningDialect::OpenAi,
            fires: || ChatRequest {
                chat_template_kwargs: Some(json!({"enable_thinking": true})),
                ..request("gpt-4o")
            },
            quiet: || request("gpt-4o"),
            site: EditSite::DialectFieldStrip,
            pattern: "/chat_template_kwargs",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "per-block cache_control strip",
            dialect: ReasoningDialect::OpenAi,
            fires: with_cache_marker,
            quiet: || request("gpt-4o"),
            site: EditSite::CacheMarkerStrip,
            pattern: "/messages/0/content/0/cache_control",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "sampling params strip",
            dialect: ReasoningDialect::OpenAi,
            fires: || ChatRequest {
                temperature: Some(0.5),
                ..request("o3-mini")
            },
            quiet: || ChatRequest {
                temperature: Some(0.5),
                ..request("gpt-4o")
            },
            site: EditSite::SamplingParamsStrip,
            pattern: "/temperature",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "max_tokens rename",
            dialect: ReasoningDialect::OpenAi,
            fires: || ChatRequest {
                max_tokens: Some(64),
                ..request("o3-mini")
            },
            quiet: || ChatRequest {
                max_tokens: Some(64),
                ..request("gpt-4o")
            },
            site: EditSite::MaxTokensRename,
            pattern: "/max_completion_tokens",
            kind: EditKind::Add,
        },
        SiteRow {
            name: "effort injection",
            dialect: ReasoningDialect::OpenAi,
            fires: || with_effort("o3-mini", "low"),
            quiet: || request("o3-mini"),
            site: EditSite::EffortInjection,
            pattern: "/reasoning_effort",
            kind: EditKind::Add,
        },
        SiteRow {
            name: "effort clamped to a supported level",
            dialect: ReasoningDialect::OpenAi,
            fires: || clamped(with_effort("o3-mini", "medium")),
            quiet: || clamped(with_effort("o3-mini", "low")),
            site: EditSite::EffortInjection,
            pattern: "/reasoning_effort",
            kind: EditKind::Rewrite,
        },
        SiteRow {
            name: "effort withheld",
            dialect: ReasoningDialect::OpenAi,
            fires: || with_effort("o3-mini", "none"),
            quiet: || with_effort("o3-mini", "low"),
            site: EditSite::DialectFieldStrip,
            pattern: "/reasoning_effort",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "vllm thinking kwargs injection",
            dialect: ReasoningDialect::Vllm,
            fires: || with_effort("qwen3", "low"),
            quiet: || request("qwen3"),
            site: EditSite::EffortInjection,
            pattern: "/chat_template_kwargs",
            kind: EditKind::Add,
        },
        SiteRow {
            name: "openrouter reasoning injection",
            dialect: ReasoningDialect::OpenRouter,
            fires: || with_effort("anthropic/claude", "low"),
            quiet: || request("anthropic/claude"),
            site: EditSite::EffortInjection,
            pattern: "/reasoning",
            kind: EditKind::Add,
        },
        SiteRow {
            name: "history reasoning strip",
            dialect: ReasoningDialect::DeepSeek,
            fires: assistant_with_reasoning,
            quiet: || request("deepseek-chat"),
            site: EditSite::HistoryReasoningStrip,
            pattern: "/messages/1/reasoning_content",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "builtin tool withheld",
            dialect: ReasoningDialect::OpenAi,
            fires: || ChatRequest {
                tools: Some(vec![ToolDef::Other(
                    json!({"type": "web_search_20250305", "name": "web_search"}),
                )]),
                ..request("gpt-4o")
            },
            quiet: || ChatRequest {
                tools: Some(vec![ToolDef::Other(
                    json!({"type": "function", "function": {"name": "f", "parameters": {}}}),
                )]),
                ..request("gpt-4o")
            },
            site: EditSite::DialectFieldStrip,
            pattern: "/tools/0",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "tool_choice withheld",
            dialect: ReasoningDialect::OpenAi,
            fires: || ChatRequest {
                tool_choice: Some(json!("required")),
                ..request("gpt-4o")
            },
            quiet: || ChatRequest {
                tool_choice: Some(json!("auto")),
                ..request("gpt-4o")
            },
            site: EditSite::DialectFieldStrip,
            pattern: "/tool_choice",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "unrecognized tool_choice withheld",
            dialect: ReasoningDialect::OpenAi,
            fires: || ChatRequest {
                tool_choice: Some(json!({"type": "bogus"})),
                ..request("gpt-4o")
            },
            quiet: || ChatRequest {
                tool_choice: Some(json!({"type": "auto"})),
                ..request("gpt-4o")
            },
            site: EditSite::DialectFieldStrip,
            pattern: "/tool_choice",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "managed extra withheld",
            dialect: ReasoningDialect::OpenAi,
            fires: || ChatRequest {
                provider_extras: Some(json!({"metadata": {"user_id": "u"}})),
                ..request("gpt-4o")
            },
            quiet: || ChatRequest {
                provider_extras: Some(json!({"service_tier": "auto"})),
                ..request("gpt-4o")
            },
            site: EditSite::ManagedExtrasStrip,
            pattern: "/metadata",
            kind: EditKind::Remove,
        },
        SiteRow {
            name: "fingerprint withheld from a mixed system",
            dialect: ReasoningDialect::OpenAi,
            fires: || with_fingerprint("keep me"),
            quiet: || ChatRequest {
                system: Some(SystemContent::Text("keep me".into())),
                ..request("gpt-4o")
            },
            site: EditSite::ClientFingerprintStrip,
            pattern: "/messages/0/content",
            kind: EditKind::Rewrite,
        },
        SiteRow {
            name: "fingerprint-only system withheld",
            dialect: ReasoningDialect::OpenAi,
            fires: || with_fingerprint(""),
            quiet: || request("gpt-4o"),
            site: EditSite::ClientFingerprintStrip,
            pattern: "/messages/0",
            kind: EditKind::Remove,
        },
    ]
}

#[test]
fn each_normalize_site_declares_exactly_when_it_fires() {
    for row in site_rows() {
        let provider = provider(row.dialect);

        let fired = declared(&provider, (row.fires)());
        let quiet = declared(&provider, (row.quiet)());

        assert!(
            has(&fired, row.site, row.pattern, row.kind),
            "{}: missing {:?} {} {:?} in {fired:?}",
            row.name,
            row.site,
            row.pattern,
            row.kind
        );
        assert!(
            !has(&quiet, row.site, row.pattern, row.kind),
            "{}: declared without firing: {quiet:?}",
            row.name
        );
    }
}

#[test]
fn a_dialect_that_reinjects_a_stripped_key_declares_no_removal() {
    let provider = provider(ReasoningDialect::Vllm);
    let req = ChatRequest {
        chat_template_kwargs: Some(json!({"enable_thinking": true})),
        ..request("qwen3")
    };

    let edits = declared(&provider, req);

    assert!(!has(
        &edits,
        EditSite::DialectFieldStrip,
        "/chat_template_kwargs",
        EditKind::Remove
    ));
}

#[test]
fn operator_payload_extras_declare_an_add_per_key() {
    let mut cfg = config("http://127.0.0.1:9", ReasoningDialect::OpenAi);
    cfg.payload_extras = Some(json!({"top_k": 5}));
    let with_extras = OpenAiCompatProvider::new(cfg);

    let fired = declared(&with_extras, request("gpt-4o"));
    let quiet = declared(&provider(ReasoningDialect::OpenAi), request("gpt-4o"));

    assert!(has(
        &fired,
        EditSite::OperatorPayloadExtras,
        "/top_k",
        EditKind::Add
    ));
    assert_eq!(of_site(&quiet, EditSite::OperatorPayloadExtras), 0);
}

fn declared_on_send(stream: bool, req: ChatRequest) -> Vec<(EditSite, String, EditKind)> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let server = MockServer::start().await;
        let template = if stream {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body())
        } else {
            ResponseTemplate::new(200).set_body_json(ok_completion())
        };
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(template)
            .mount(&server)
            .await;
        let provider = OpenAiCompatProvider::new(config(&server.uri(), ReasoningDialect::OpenAi));
        let (req, wire_tap) = tapped(req);
        if stream {
            let _stream = provider.stream(req).await.expect("stream");
        } else {
            provider.complete(req).await.expect("complete");
        }
        edits(&wire_tap.take_request())
    })
}

#[test]
fn the_stream_flag_is_declared_only_when_forced_against_the_request() {
    let rows: [(&str, bool, Option<bool>, Option<EditKind>); 5] = [
        ("complete, flag unset", false, None, Some(EditKind::Add)),
        (
            "complete, client asked to stream",
            false,
            Some(true),
            Some(EditKind::Rewrite),
        ),
        (
            "complete, client asked not to stream",
            false,
            Some(false),
            None,
        ),
        ("stream, client asked to stream", true, Some(true), None),
        ("stream, flag unset", true, None, Some(EditKind::Add)),
    ];
    for (name, stream, requested, want) in rows {
        let req = ChatRequest {
            stream: requested,
            ..request("gpt-4o")
        };

        let edits = declared_on_send(stream, req);

        let got: Vec<EditKind> = edits
            .iter()
            .filter(|(s, _, _)| *s == EditSite::StreamFlagForce)
            .map(|(_, _, k)| *k)
            .collect();
        assert_eq!(got, want.into_iter().collect::<Vec<_>>(), "{name}");
    }
}

#[test]
fn stream_options_injection_names_the_member_it_creates() {
    let rows: [(&str, Option<Value>, Option<&str>); 3] = [
        ("absent object", None, Some("/stream_options")),
        (
            "object without the flag",
            Some(json!({"stream_options": {"other": 1}})),
            Some("/stream_options/include_usage"),
        ),
        (
            "flag already set",
            Some(json!({"stream_options": {"include_usage": false}})),
            None,
        ),
    ];
    for (name, extras, want) in rows {
        let req = ChatRequest {
            provider_extras: extras,
            stream: Some(true),
            ..request("gpt-4o")
        };

        let edits = declared_on_send(true, req);

        let got: Vec<&str> = edits
            .iter()
            .filter(|(s, _, _)| *s == EditSite::StreamOptionsInjection)
            .map(|(_, p, _)| p.as_str())
            .collect();
        assert_eq!(got, want.into_iter().collect::<Vec<_>>(), "{name}");
    }
}
