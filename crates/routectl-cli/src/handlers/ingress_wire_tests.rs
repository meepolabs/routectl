use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use routectl_core::identity::anthropic::default_claude_code_user_agent;
use routectl_core::identity::codex::codex_user_agent;
use routectl_core::wire_check::IngressDialect;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, Provider,
    RequestProvenance, Role,
};
use routectl_router::{AliasValue, Config, ProviderEntry, ResolvedModel, Router};
use routectl_usage::{CHANNEL_CAPACITY, Harness, UsageWriter};
use serde_json::{Value, json};

use super::{capture_ingress_wire, classify_harness, client_version};
use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::server::AppState;

fn user_agent(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::USER_AGENT, HeaderValue::from_str(value).unwrap());
    headers
}

#[test]
fn client_version_prefers_the_claude_cli_version_then_the_first_safe_product_token() {
    let long_token = format!("client/{}", "9".repeat(80));
    let unsafe_then_safe = format!("{long_token} client/1.0");
    let cases: [(&str, Option<&str>); 7] = [
        ("claude-cli/2.1.246 (external, sdk-cli)", Some("2.1.246")),
        (
            "claude-cli/2.1.246.1e8 (external, cli)",
            Some("claude-cli/2.1.246.1e8"),
        ),
        ("Mozilla compat OpenAI/Python 1.40.0", Some("OpenAI/Python")),
        ("codex_cli_rs/0.40.0 (Linux)", Some("codex_cli_rs/0.40.0")),
        ("no product token here", None),
        (&long_token, None),
        (&unsafe_then_safe, Some("client/1.0")),
    ];

    for (ua, expected) in cases {
        assert_eq!(client_version(&user_agent(ua)).as_deref(), expected, "{ua}");
    }
}

#[test]
fn client_version_is_none_without_a_user_agent() {
    assert_eq!(client_version(&HeaderMap::new()), None);
}

#[test]
fn classify_harness_names_the_minted_client_shapes_and_nothing_else() {
    let non_utf8 = {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_bytes(b"claude-cli/2.1.0 \xff").unwrap(),
        );
        headers
    };
    let cases: [(&str, HeaderMap, Harness); 9] = [
        (
            "claude code producer",
            user_agent(default_claude_code_user_agent()),
            Harness::ClaudeCode,
        ),
        (
            "codex producer",
            user_agent(codex_user_agent()),
            Harness::CodexCli,
        ),
        ("missing", HeaderMap::new(), Harness::Other),
        ("non-utf8", non_utf8, Harness::Other),
        (
            "claude-cli empty version",
            user_agent("claude-cli/"),
            Harness::Other,
        ),
        (
            "codex prefix of a longer name",
            user_agent("codex_cli_rsX/1"),
            Harness::Other,
        ),
        ("curl", user_agent("curl/8"), Harness::Other),
        ("opencode", user_agent("opencode/1"), Harness::Other),
        ("empty", user_agent(""), Harness::Other),
    ];

    for (name, headers, expected) in cases {
        assert_eq!(classify_harness(&headers), expected, "{name}");
    }
}

#[test]
fn a_request_with_no_ingress_provenance_gets_no_ingress_wire() {
    let mut req = ChatRequest::default();

    capture_ingress_wire(&HeaderMap::new(), &Bytes::from_static(b"{}"), &mut req);

    assert!(req.routectl_internal.ingress_wire.is_none());
}

/// Serves every request and keeps each canonical request it was handed.
struct CapturingProvider {
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn id(&self) -> &'static str {
        "capturing"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value, Error> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse, Error> {
        Err(Error::normalize_response("capturing", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, Error> {
        self.seen.lock().unwrap().push(req);
        Ok(ChatResponse {
            id: "ok".into(),
            model: "m".into(),
            created: 1,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: Vec::new(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: None,
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk, Error>>, Error> {
        Err(Error::upstream("capturing", 501, "unused"))
    }
}

fn capturing_router() -> (Router, Arc<Mutex<Vec<ChatRequest>>>) {
    let mut providers = BTreeMap::new();
    providers.insert("p".to_string(), ProviderEntry::anthropic_api("literal:k"));
    let mut aliases = BTreeMap::new();
    aliases.insert("a".to_string(), AliasValue::Single("m".to_string()));
    let config = Arc::new(Config {
        providers,
        aliases,
        ..Default::default()
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingProvider {
        seen: Arc::clone(&seen),
    };
    let mut router = Router::new(config);
    let mut models = BTreeMap::new();
    models.insert(
        "m".to_string(),
        Arc::new(ResolvedModel::new("m", "p", Arc::new(provider), "upstream")),
    );
    router.install_resolved_models(models);
    (router, seen)
}

fn capturing_state() -> (Arc<AppState>, Arc<Mutex<Vec<ChatRequest>>>) {
    let (router, seen) = capturing_router();
    let state = AppState::for_test(Arc::new(arc_swap::ArcSwap::from_pointee(router)));
    (state, seen)
}

/// Post `body` through the real ingress driver and return the canonical
/// request the provider received.
async fn posted_through<A: IngressAdapter + 'static>(
    adapter: A,
    body: &'static str,
) -> ChatRequest {
    let (state, seen) = capturing_state();
    let mut headers = user_agent("claude-cli/2.1.246 (external, cli)");
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let resp = ingress_handle(
        state,
        headers,
        None,
        Ok(Bytes::from_static(body.as_bytes())),
        adapter,
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let seen = seen.lock().unwrap();
    seen.first().expect("the provider was called").clone()
}

#[tokio::test]
async fn the_ingress_wire_carries_the_posted_body_dialect_and_version() {
    const ANTHROPIC: &str =
        r#"{"model":"a","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"x":1}"#;
    const CHAT: &str = r#"{"model":"a","messages":[{"role":"user","content":"hi"}],"x":1}"#;
    const RESPONSES: &str = r#"{"model":"a","input":"hi","x":1}"#;

    let cases = [
        (
            "anthropic",
            posted_through(AnthropicIngress, ANTHROPIC).await,
            ANTHROPIC,
            IngressDialect::Anthropic,
            RequestProvenance::AnthropicIngress,
        ),
        (
            "chat",
            posted_through(OpenAiIngress, CHAT).await,
            CHAT,
            IngressDialect::Chat,
            RequestProvenance::OpenaiIngress,
        ),
        (
            "responses",
            posted_through(ResponsesIngress, RESPONSES).await,
            RESPONSES,
            IngressDialect::Responses,
            RequestProvenance::ResponsesIngress,
        ),
    ];

    for (name, req, posted, dialect, provenance) in cases {
        let wire = req
            .routectl_internal
            .ingress_wire
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: no ingress wire"));
        assert_eq!(wire.bytes.as_ref(), posted.as_bytes(), "{name}");
        assert_eq!(wire.dialect, dialect, "{name}");
        assert_eq!(wire.client_version.as_deref(), Some("2.1.246"), "{name}");
        assert_eq!(req.routectl_internal.provenance, provenance, "{name}");
    }
}

/// Post one chat request with `ua` through the real ingress into a fresh
/// usage ledger and return the ledger path (with its tempdir guard).
async fn persisted_with_user_agent(ua: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    const CHAT: &str = r#"{"model":"a","messages":[{"role":"user","content":"hi"}]}"#;
    let (router, _seen) = capturing_router();
    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (usage, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let state =
        AppState::for_test_with_usage(Arc::new(arc_swap::ArcSwap::from_pointee(router)), usage);
    let mut headers = user_agent(ua);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let resp = ingress_handle(
        state,
        headers,
        None,
        Ok(Bytes::from_static(CHAT.as_bytes())),
        OpenAiIngress,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("response body drains");
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .expect("usage writer shutdown");
    (dir, db_path)
}

/// Every `requests` column whose TEXT value contains `needle`, across all
/// rows. Columns are enumerated from the live schema, so a column added
/// later is scanned too.
fn text_columns_containing(conn: &rusqlite::Connection, needle: &str) -> Vec<String> {
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('requests')")
        .expect("prepare columns")
        .query_map([], |r| r.get(0))
        .expect("query columns")
        .collect::<Result<_, _>>()
        .expect("columns");
    assert!(
        columns.iter().any(|c| c == "extra"),
        "the scan covers `extra`"
    );
    columns
        .into_iter()
        .filter(|column| {
            let sql = format!(
                "SELECT COUNT(*) FROM requests \
                 WHERE typeof(\"{column}\") = 'text' AND instr(\"{column}\", ?1) > 0"
            );
            let hits: i64 = conn
                .query_row(&sql, [needle], |r| r.get(0))
                .expect("scan column");
            hits > 0
        })
        .collect()
}

#[tokio::test]
async fn the_persisted_row_carries_the_harness_token_and_never_the_user_agent() {
    const MARKER: &str = "zqharnessmarker";
    let claude_ua = format!("claude-cli/{MARKER} (external, cli)");
    let codex_ua = format!("codex_cli_rs/{MARKER} (Linux 6.0; x86_64) xterm");
    let cases = [
        (claude_ua.as_str(), "claude_code"),
        (codex_ua.as_str(), "codex_cli"),
        ("curl/8", "other"),
    ];

    for (ua, token) in cases {
        let (_dir, db_path) = persisted_with_user_agent(ua).await;
        let conn = rusqlite::Connection::open(&db_path).expect("read open");

        let harness: Vec<String> = conn
            .prepare("SELECT harness FROM requests")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(harness, [token], "{token}");
        assert_eq!(
            text_columns_containing(&conn, ua),
            Vec::<String>::new(),
            "{token}"
        );
        assert_eq!(
            text_columns_containing(&conn, MARKER),
            Vec::<String>::new(),
            "{token}"
        );

        conn.execute(
            "UPDATE requests SET extra = json_object('ua', ?1)",
            [claude_ua.as_str()],
        )
        .expect("plant control");
        assert_eq!(
            text_columns_containing(&conn, MARKER),
            ["extra"],
            "{token} control"
        );
    }
}
