//! The wire tap on the openai-responses egress: both sends deposit the exact
//! bytes the upstream received, and every body edit this egress makes is
//! declared at its site.

use std::sync::Arc;

use routectl_core::wire_check::{EditKind, EditSite, Leg, TapContents, WireTap};
use routectl_core::{
    ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, Provider, ReasoningConfig,
    ResponsesPassthroughItem, Role, StaticToken, SystemContent, TokenSource,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider};

type Edit = (EditSite, String, EditKind);

fn config(base_url: &str, auth_kind: AuthKind) -> OpenAiResponsesConfig {
    OpenAiResponsesConfig {
        id: "responses-tap".into(),
        auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
        account_id: Some("acct".into()),
        base_url: base_url.to_string(),
        auth_kind,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
        installation_id: None,
        #[cfg(feature = "bedrock")]
        mantle: None,
    }
}

fn provider(auth_kind: AuthKind) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(config("http://127.0.0.1:9", auth_kind))
}

fn user(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        messages: vec![user("ping")].into(),
        system: Some(SystemContent::Text("be brief".into())),
        ..Default::default()
    }
}

fn tapped(mut req: ChatRequest) -> (ChatRequest, WireTap) {
    let wire_tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(wire_tap.clone());
    (req, wire_tap)
}

fn edits(contents: &TapContents) -> Vec<Edit> {
    contents
        .edits(Leg::Request)
        .iter()
        .map(|e| (e.site(), e.path().to_string(), e.kind()))
        .collect()
}

fn declared(provider: &OpenAiResponsesProvider, req: ChatRequest) -> Vec<Edit> {
    let (req, wire_tap) = tapped(req);
    provider.normalize_request(&req).expect("normalize");
    edits(&wire_tap.take_request())
}

fn has(edits: &[Edit], site: EditSite, pattern: &str, kind: EditKind) -> bool {
    edits
        .iter()
        .any(|(s, p, k)| *s == site && p == pattern && *k == kind)
}

fn completed_sse() -> String {
    let body = json!({
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5",
        "output": [{"type": "message", "id": "m1", "role": "assistant",
                    "content": [{"type": "output_text", "text": "pong"}]}],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!(
        "data: {}\n\n",
        json!({"type": "response.completed", "response": body})
    )
}

async fn send(stream: bool, req: ChatRequest) -> (Vec<u8>, TapContents) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(completed_sse()),
        )
        .mount(&server)
        .await;
    let provider = OpenAiResponsesProvider::new(config(&server.uri(), AuthKind::ChatgptOauth));
    let (req, wire_tap) = tapped(req);
    if stream {
        let _stream = provider.stream(req).await.expect("stream open");
    } else {
        provider.complete(req).await.expect("complete");
    }
    let requests = server.received_requests().await.expect("recording on");
    let body = requests.last().expect("one request").body.clone();
    (body, wire_tap.take_request())
}

#[tokio::test]
async fn both_sends_deposit_the_exact_bytes_the_upstream_received() {
    for stream in [false, true] {
        let (body, contents) = send(stream, request()).await;

        assert_eq!(
            contents.request().map(|b| b.as_ref()),
            Some(body.as_slice()),
            "stream={stream}"
        );
    }
}

#[tokio::test]
async fn both_sends_declare_the_forced_stream_flag_only_against_the_request() {
    let rows: [(&str, bool, Option<bool>, Option<EditKind>); 4] = [
        ("complete, flag unset", false, None, Some(EditKind::Add)),
        (
            "complete, client asked not to stream",
            false,
            Some(false),
            Some(EditKind::Rewrite),
        ),
        ("stream, client asked to stream", true, Some(true), None),
        ("stream, flag unset", true, None, Some(EditKind::Add)),
    ];
    for (name, stream, requested, want) in rows {
        let req = ChatRequest {
            stream: requested,
            ..request()
        };

        let (_, contents) = send(stream, req).await;

        let got: Vec<EditKind> = edits(&contents)
            .into_iter()
            .filter(|(s, _, _)| *s == EditSite::StreamFlagForce)
            .map(|(_, _, k)| k)
            .collect();
        assert_eq!(got, want.into_iter().collect::<Vec<_>>(), "{name}");
    }
}

/// `req` on a model that declares only `low` and `high`.
fn clamped(mut req: ChatRequest) -> ChatRequest {
    req.routectl_internal.effort_levels = vec!["low".to_string(), "high".to_string()].into();
    req
}

fn with_effort(effort: &str) -> ChatRequest {
    ChatRequest {
        reasoning: Some(ReasoningConfig {
            effort: Some(effort.into()),
            ..Default::default()
        }),
        ..request()
    }
}

/// A passthrough whose inbound prefix counts more modeled items than this
/// egress emits, so the splice clamps it to the tail of `input[]`.
fn with_passthrough_past_the_tail() -> ChatRequest {
    let mut req = request();
    req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
        modeled_prefix: 3,
        item: json!({"type": "local_shell_call", "id": "x"}),
    }];
    req
}

fn with_user_file_url() -> ChatRequest {
    ChatRequest {
        messages: vec![Message {
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::File {
                file: json!({"file_url": "https://example.com/a.pdf", "file_id": "file-1"}),
                cache_control: None,
            })]),
            ..user("")
        }]
        .into(),
        ..request()
    }
}

fn with_fingerprint(rest: Option<&str>) -> ChatRequest {
    let fingerprint = "x-anthropic-billing-header: cc_version=1; cch=abc";
    let block = |text: &str| routectl_core::SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    };
    let system = match rest {
        Some(rest) => SystemContent::Blocks(vec![block(fingerprint), block(rest)]),
        None => SystemContent::Text(fingerprint.into()),
    };
    ChatRequest {
        system: Some(system),
        ..request()
    }
}

/// One row per declaring site: the auth lane, a request that makes the site
/// fire, the declaration it must leave, and a sibling request on the same
/// path that must not leave it.
struct SiteRow {
    name: &'static str,
    lane: AuthKind,
    installation_id: Option<&'static str>,
    fires: fn() -> ChatRequest,
    quiet: fn() -> ChatRequest,
    site: EditSite,
    pattern: &'static str,
    kind: EditKind,
}

const fn row(
    name: &'static str,
    fires: fn() -> ChatRequest,
    quiet: fn() -> ChatRequest,
    site: EditSite,
    pattern: &'static str,
    kind: EditKind,
) -> SiteRow {
    SiteRow {
        name,
        lane: AuthKind::ChatgptOauth,
        installation_id: None,
        fires,
        quiet,
        site,
        pattern,
        kind,
    }
}

fn site_rows() -> Vec<SiteRow> {
    vec![
        row(
            "empty instructions default",
            || ChatRequest {
                system: None,
                ..request()
            },
            request,
            EditSite::RequiredFieldDefault,
            "/instructions",
            EditKind::Add,
        ),
        row(
            "parallel_tool_calls default",
            request,
            || ChatRequest {
                provider_extras: Some(json!({"parallel_tool_calls": true})),
                ..request()
            },
            EditSite::RequiredFieldDefault,
            "/parallel_tool_calls",
            EditKind::Add,
        ),
        row(
            "max_output_tokens withheld on chatgpt-oauth",
            || ChatRequest {
                max_tokens: Some(64),
                ..request()
            },
            request,
            EditSite::AuthSurfaceGate,
            "/max_output_tokens",
            EditKind::Remove,
        ),
        row(
            "reasoning summary default",
            || with_effort("low"),
            request,
            EditSite::ReasoningSummaryDefault,
            "/reasoning/summary",
            EditKind::Add,
        ),
        row(
            "effort clamped to a supported level",
            || clamped(with_effort("medium")),
            || clamped(with_effort("low")),
            EditSite::EffortInjection,
            "/reasoning/effort",
            EditKind::Rewrite,
        ),
        row(
            "effort none withholds reasoning",
            || with_effort("none"),
            || with_effort("low"),
            EditSite::DialectFieldStrip,
            "/reasoning",
            EditKind::Remove,
        ),
        row(
            "encrypted reasoning include",
            request,
            || ChatRequest {
                provider_extras: Some(json!({"include": []})),
                ..request()
            },
            EditSite::ReasoningIncludeInjection,
            "/include",
            EditKind::Add,
        ),
        row(
            "json_schema name default",
            || ChatRequest {
                response_format: Some(
                    json!({"type": "json_schema", "json_schema": {"schema": {}}}),
                ),
                ..request()
            },
            || ChatRequest {
                response_format: Some(
                    json!({"type": "json_schema", "json_schema": {"name": "n", "schema": {}}}),
                ),
                ..request()
            },
            EditSite::RequiredFieldDefault,
            "/text/format/name",
            EditKind::Add,
        ),
        row(
            "passthrough clamped to the tail",
            with_passthrough_past_the_tail,
            || {
                let mut req = request();
                req.routectl_internal.responses_input_passthrough =
                    vec![ResponsesPassthroughItem {
                        modeled_prefix: 1,
                        item: json!({"type": "local_shell_call", "id": "x"}),
                    }];
                req
            },
            EditSite::ResponsesPassthroughSplice,
            "/input/1",
            EditKind::Relocate,
        ),
        row(
            "fingerprint withheld from a mixed system",
            || with_fingerprint(Some("keep me")),
            request,
            EditSite::ClientFingerprintStrip,
            "/instructions",
            EditKind::Rewrite,
        ),
        SiteRow {
            name: "installation id stamp",
            installation_id: Some("iid-1"),
            ..row(
                "",
                request,
                request,
                EditSite::InstallationIdStamp,
                "/client_metadata",
                EditKind::Add,
            )
        },
        SiteRow {
            name: "file_url withheld on mantle",
            lane: AuthKind::BedrockMantle,
            ..row(
                "",
                with_user_file_url,
                request,
                EditSite::AuthSurfaceGate,
                "/input/*/content/*/file_url",
                EditKind::Remove,
            )
        },
    ]
}

fn provider_for(row: &SiteRow, with_installation_id: bool) -> OpenAiResponsesProvider {
    let mut cfg = config("http://127.0.0.1:9", row.lane);
    if with_installation_id {
        cfg.installation_id = row.installation_id.map(str::to_string);
    }
    OpenAiResponsesProvider::new(cfg)
}

#[test]
fn each_normalize_site_declares_exactly_when_it_fires() {
    for row in site_rows() {
        let fired = declared(&provider_for(&row, true), (row.fires)());
        let quiet_provider = provider_for(&row, false);
        let quiet = declared(&quiet_provider, (row.quiet)());

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
fn store_is_declared_as_routectl_written_on_every_lane() {
    for lane in [
        AuthKind::ChatgptOauth,
        AuthKind::ApiKey,
        AuthKind::BedrockMantle,
    ] {
        let edits = declared(&provider(lane), request());

        assert!(
            has(
                &edits,
                EditSite::RequiredFieldDefault,
                "/store",
                EditKind::Add
            ),
            "{lane:?}"
        );
        assert!(
            has(
                &edits,
                EditSite::RequiredFieldDefault,
                "/store",
                EditKind::Rewrite
            ),
            "{lane:?}"
        );
    }
}

#[test]
fn a_url_only_user_file_on_mantle_forwards_its_url_without_a_gate_declaration() {
    let req = ChatRequest {
        messages: vec![Message {
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::File {
                file: json!({"file_url": "https://example.com/a.pdf"}),
                cache_control: None,
            })]),
            ..user("")
        }]
        .into(),
        ..request()
    };

    let edits = declared(&provider(AuthKind::BedrockMantle), req);

    assert!(
        !has(
            &edits,
            EditSite::AuthSurfaceGate,
            "/input/*/content/*/file_url",
            EditKind::Remove
        ),
        "{edits:?}"
    );
}

#[test]
fn the_api_key_lane_forwards_max_output_tokens_without_a_gate_declaration() {
    let req = ChatRequest {
        max_tokens: Some(64),
        ..request()
    };

    let edits = declared(&provider(AuthKind::ApiKey), req);

    assert!(
        !edits
            .iter()
            .any(|(s, _, _)| *s == EditSite::AuthSurfaceGate)
    );
}

#[test]
fn an_installation_id_lands_beside_client_metadata_keys_as_a_masked_add() {
    let mut cfg = config("http://127.0.0.1:9", AuthKind::ChatgptOauth);
    cfg.installation_id = Some("iid-1".into());
    let req = ChatRequest {
        provider_extras: Some(json!({"client_metadata": {"team": "a"}})),
        ..request()
    };

    let edits = declared(&OpenAiResponsesProvider::new(cfg), req);

    assert!(has(
        &edits,
        EditSite::InstallationIdStamp,
        "/client_metadata/*",
        EditKind::Add
    ));
}

#[test]
fn a_passthrough_left_in_place_declares_no_relocation() {
    let mut req = request();
    req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
        modeled_prefix: 1,
        item: json!({"type": "local_shell_call", "id": "x"}),
    }];

    let edits = declared(&provider(AuthKind::ChatgptOauth), req);

    assert!(
        !edits
            .iter()
            .any(|(s, _, _)| *s == EditSite::ResponsesPassthroughSplice),
        "{edits:?}"
    );
}

#[tokio::test]
async fn a_response_checking_tap_assembles_the_upstream_stream_on_both_sends() {
    for stream in [false, true] {
        let wire_tap =
            WireTap::checking_response(routectl_core::wire_check::IngressDialect::Responses);
        let mut req = request();
        req.routectl_internal.wire_tap = Some(wire_tap.clone());
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(completed_sse()),
            )
            .mount(&server)
            .await;
        let provider = OpenAiResponsesProvider::new(config(&server.uri(), AuthKind::ChatgptOauth));

        if stream {
            let mut chunks = provider.stream(req).await.expect("stream open");
            while futures::StreamExt::next(&mut chunks).await.is_some() {}
        } else {
            provider.complete(req).await.expect("complete");
        }

        let assembled =
            wire_tap.finish_response_stream(routectl_core::wire_check::StreamExit::Ended);
        let Some(routectl_core::wire_check::Assembled::Complete(document)) = assembled else {
            panic!("stream={stream}: expected an assembled stream, got {assembled:?}");
        };
        assert_eq!(document["id"], "resp_1", "stream={stream}");
        assert_eq!(document["status"], "completed", "stream={stream}");
    }
}
