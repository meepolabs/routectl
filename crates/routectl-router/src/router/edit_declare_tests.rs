//! Router edit declarations at their sites: each transform that fires on a
//! fidelity lane leaves exactly its own declaration on the request the
//! provider receives, and a transform that does not fire leaves none. A
//! recording provider stands in for the egress and keeps every request it
//! was handed.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::wire_check::{
    DeclaredEdit, EditKind, EditPatternError, EditSite, IngressDialect, IngressWire,
};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, ContentPart, Error,
    KnownContentPart, Message, MessageContent, Provider, ReasoningDetail, ReasoningDetailKind,
    Result, Role, SystemBlock, SystemContent,
};
use serde_json::{Value, json};

use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::Router;

const ALIAS: &str = "decl-alias";
const UPSTREAM: &str = "wire-model";

/// What the provider answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    Serve,
    Unavailable,
}

/// Records each request it is handed and answers per [`Answer`].
struct Recorder {
    answer: Answer,
    seen: Mutex<Vec<ChatRequest>>,
}

impl Recorder {
    fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn admit(&self, req: &ChatRequest) -> Result<()> {
        self.seen.lock().push(req.clone());
        match self.answer {
            Answer::Serve => Ok(()),
            Answer::Unavailable => Err(Error::upstream("decl-mock", 503, "down")),
        }
    }

    fn only(&self) -> ChatRequest {
        let seen = self.seen.lock();
        assert_eq!(seen.len(), 1, "exactly one upstream call");
        seen[0].clone()
    }
}

#[async_trait::async_trait]
impl Provider for Recorder {
    fn id(&self) -> &'static str {
        "decl-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("decl-mock", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.admit(&req)?;
        Ok(ok_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.admit(&req)?;
        Ok(stream::iter([Ok(content_chunk())]).boxed())
    }
}

fn ok_response() -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: text_message(Role::Assistant, "ok"),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

fn content_chunk() -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("ok".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

fn text_message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

/// Which optional transforms a router runs. Both are off by default so only
/// the transform under test fires.
#[derive(Clone, Copy, Default)]
struct Switches {
    cache: bool,
    reduction: bool,
}

const OFF: Switches = Switches {
    cache: false,
    reduction: false,
};
const CACHE: Switches = Switches {
    cache: true,
    reduction: false,
};
const REDUCTION: Switches = Switches {
    cache: false,
    reduction: true,
};

/// A router over a chain of `[providers.pN]` entries, one per provider in
/// `chain`, each built from `provider_toml(idx)`.
fn router_with(
    switches: Switches,
    provider_toml: impl Fn(usize) -> String,
    chain: Vec<Arc<dyn Provider>>,
) -> Router {
    let Switches { cache, reduction } = switches;
    let mut toml_text = format!(
        "version = 5\n[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\
         [cache]\nauto_emit_top_level_breakpoint = {cache}\n[reduction]\nenabled = {reduction}\n"
    );
    let mut nicknames = Vec::new();
    for idx in 0..chain.len() {
        toml_text.push_str(&format!("[providers.p{idx}]\n{}\n", provider_toml(idx)));
        nicknames.push(format!("\"m{idx}\""));
    }
    toml_text.push_str(&format!(
        "[aliases]\n{ALIAS} = [{}]\n",
        nicknames.join(", ")
    ));
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let models: BTreeMap<String, Arc<ResolvedModel>> = chain
        .into_iter()
        .enumerate()
        .map(|(idx, provider)| {
            let nickname = format!("m{idx}");
            let model = ResolvedModel::new(&nickname, format!("p{idx}"), provider, UPSTREAM);
            (nickname, Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    router
}

const ANTHROPIC_API: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"";
const OPENAI_COMPAT: &str =
    "kind = \"openai-compat\"\napi_key_ref = \"literal:k\"\nbase_url = \"https://x.invalid/v1\"";

fn anthropic_router(switches: Switches, provider: Arc<dyn Provider>) -> Router {
    router_with(switches, |_| ANTHROPIC_API.to_owned(), vec![provider])
}

/// A request whose captured client body is `body` in `dialect`.
fn request(dialect: IngressDialect, body: &Value) -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![text_message(Role::User, "hi")].into(),
        ..Default::default()
    };
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect,
        client_version: None,
        bytes: serde_json::to_vec(body).expect("serialize").into(),
    });
    req
}

fn anthropic_request() -> ChatRequest {
    request(IngressDialect::Anthropic, &json!({"model": ALIAS}))
}

/// The declarations `site` left, as `(path, kind)` pairs.
fn declared(req: &ChatRequest, site: EditSite) -> Vec<(String, EditKind)> {
    req.routectl_internal
        .declared_edits
        .iter()
        .filter(|edit| edit.site() == site)
        .map(|edit| (edit.path().to_owned(), edit.kind()))
        .collect()
}

fn edit(path: &str, kind: EditKind) -> (String, EditKind) {
    (path.to_owned(), kind)
}

async fn complete(router: &Router, req: ChatRequest) {
    Box::pin(router.complete(req))
        .await
        .expect("dispatch succeeds");
}

// ---------------------------------------------------------------------------
// Model rename
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_own_target_declares_the_model_rename_on_both_surfaces() {
    for streaming in [false, true] {
        let provider = Recorder::new(Answer::Serve);
        let router = anthropic_router(OFF, provider.clone());

        if streaming {
            let stream = Box::pin(router.stream(anthropic_request()))
                .await
                .expect("opens");
            let _: Vec<_> = stream.collect().await;
        } else {
            complete(&router, anthropic_request()).await;
        }

        let sent = provider.only();
        assert_eq!(sent.model, UPSTREAM);
        assert_eq!(
            declared(&sent, EditSite::ModelRename),
            [edit("/model", EditKind::Rewrite)],
            "streaming={streaming}"
        );
    }
}

#[tokio::test]
async fn a_forwarded_target_keeps_the_model_and_declares_no_rename() {
    let provider = Recorder::new(Answer::Serve);
    let router = router_with(
        OFF,
        |_| format!("{ANTHROPIC_API}\ncredential_source = \"forwarded\""),
        vec![provider.clone()],
    );
    let mut req = anthropic_request();
    req.routectl_internal.forwarded_bearer = Some(routectl_core::schema::ForwardedBearer::new(
        "sk-ant-oat01-test".to_string(),
    ));

    complete(&router, req).await;

    let sent = provider.only();
    assert_eq!(sent.model, ALIAS, "premise: the model is kept verbatim");
    assert!(declared(&sent, EditSite::ModelRename).is_empty());
}

#[tokio::test]
async fn a_rename_on_a_translation_lane_is_not_declared() {
    let provider = Recorder::new(Answer::Serve);
    let router = anthropic_router(OFF, provider.clone());

    complete(&router, request(IngressDialect::Chat, &json!({}))).await;

    let sent = provider.only();
    assert_eq!(sent.model, UPSTREAM, "premise: the rename fired");
    assert!(sent.routectl_internal.declared_edits.is_empty());
}

#[tokio::test]
async fn a_chain_hop_starts_from_a_clean_register() {
    let first = Recorder::new(Answer::Unavailable);
    let second = Recorder::new(Answer::Serve);
    let router = router_with(
        OFF,
        |idx| {
            if idx == 0 {
                format!("{ANTHROPIC_API}\npayload_extras = {{ hop_a = 1 }}")
            } else {
                ANTHROPIC_API.to_owned()
            }
        },
        vec![first.clone(), second.clone()],
    );

    complete(&router, anthropic_request()).await;

    let on_a = first.only();
    assert_eq!(
        declared(&on_a, EditSite::OperatorPayloadExtras),
        [edit("/hop_a", EditKind::Add)],
        "premise: target A declared its own extras"
    );
    let on_b = second.only();
    assert!(
        declared(&on_b, EditSite::OperatorPayloadExtras).is_empty(),
        "target A's declaration must not ride onto target B"
    );
    assert_eq!(
        on_b.routectl_internal.declared_edits.len(),
        1,
        "target B carries only its own model rename"
    );
}

// ---------------------------------------------------------------------------
// Operator payload extras
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operator_extras_declare_an_add_per_new_key_and_a_rewrite_per_changed_leaf() {
    let provider = Recorder::new(Answer::Serve);
    let router = router_with(
        OFF,
        |_| {
            format!(
                "{ANTHROPIC_API}\npayload_extras = {{ new_knob = 1, same = \"s\", \
                 nested = {{ kept = 1, changed = 2 }}, model = \"ignored\" }}"
            )
        },
        vec![provider.clone()],
    );
    let mut req = anthropic_request();
    req.provider_extras = Some(json!({"same": "s", "nested": {"kept": 1, "changed": 1}}));

    complete(&router, req).await;

    let mut got = declared(&provider.only(), EditSite::OperatorPayloadExtras);
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        [
            edit("/nested/changed", EditKind::Rewrite),
            edit("/new_knob", EditKind::Add),
        ]
    );
}

#[tokio::test]
async fn no_operator_extras_declare_nothing() {
    let provider = Recorder::new(Answer::Serve);
    let router = anthropic_router(OFF, provider.clone());

    complete(&router, anthropic_request()).await;

    assert!(declared(&provider.only(), EditSite::OperatorPayloadExtras).is_empty());
}

// ---------------------------------------------------------------------------
// Context reduction
// ---------------------------------------------------------------------------

fn tool_result_message(content: &str) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: json!(content),
            is_error: None,
            cache_control: None,
        })]),
        ..text_message(Role::User, "")
    }
}

fn tool_use_message() -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
            id: "toolu_1".into(),
            name: "Tool".into(),
            input: json!({}),
            cache_control: None,
        })]),
        ..text_message(Role::Assistant, "")
    }
}

fn reduction_request(dialect: IngressDialect, content: &str) -> ChatRequest {
    let mut req = request(dialect, &json!({}));
    req.messages = vec![tool_use_message(), tool_result_message(content)].into();
    req
}

#[tokio::test]
async fn a_compacted_tool_result_declares_its_block_on_the_anthropic_wire() {
    let provider = Recorder::new(Answer::Serve);
    let router = anthropic_router(REDUCTION, provider.clone());

    complete(
        &router,
        reduction_request(IngressDialect::Anthropic, "{\n  \"rows\": [1, 2]\n}"),
    )
    .await;

    assert_eq!(
        declared(&provider.only(), EditSite::ContextReduction),
        [edit("/messages/1/content/0/content", EditKind::Rewrite)]
    );
}

#[tokio::test]
async fn a_tool_result_with_nothing_to_compact_declares_nothing() {
    let provider = Recorder::new(Answer::Serve);
    let router = anthropic_router(REDUCTION, provider.clone());

    complete(
        &router,
        reduction_request(IngressDialect::Anthropic, "plain prose output"),
    )
    .await;

    assert!(declared(&provider.only(), EditSite::ContextReduction).is_empty());
}

#[tokio::test]
async fn a_compacted_chat_tool_call_declares_its_arguments_with_a_wildcard_message() {
    let provider = Recorder::new(Answer::Serve);
    let router = router_with(
        REDUCTION,
        |_| OPENAI_COMPAT.to_owned(),
        vec![provider.clone()],
    );
    let mut req = request(IngressDialect::Chat, &json!({}));
    let mut call = text_message(Role::Assistant, "");
    call.tool_calls = Some(vec![json!({
        "id": "c1",
        "type": "function",
        "function": {"name": "f", "arguments": "{\n  \"a\": 1\n}"}
    })]);
    let mut result = text_message(Role::Tool, "done");
    result.tool_call_id = Some("c1".into());
    req.messages = vec![call, result].into();

    complete(&router, req).await;

    assert_eq!(
        declared(&provider.only(), EditSite::ContextReduction),
        [edit(
            "/messages/*/tool_calls/0/function/arguments",
            EditKind::Rewrite
        )]
    );
}

// ---------------------------------------------------------------------------
// Cache placement
// ---------------------------------------------------------------------------

fn system_blocks_request() -> ChatRequest {
    let mut req = anthropic_request();
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "be brief".into(),
        cache_control: None,
        citations: None,
    }]));
    req
}

#[tokio::test]
async fn placed_markers_declare_the_front_slot_and_the_top_level_marker() {
    let provider = Recorder::new(Answer::Serve);
    let router = anthropic_router(CACHE, provider.clone());

    complete(&router, system_blocks_request()).await;

    let mut got = declared(&provider.only(), EditSite::CachePlacement);
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        [
            edit("/cache_control", EditKind::Add),
            edit("/system/0/cache_control", EditKind::Add),
        ]
    );
}

#[tokio::test]
async fn a_caller_breakpoint_skips_placement_and_declares_nothing() {
    let provider = Recorder::new(Answer::Serve);
    let router = anthropic_router(CACHE, provider.clone());
    let mut req = system_blocks_request();
    req.cache_control = Some(routectl_core::CacheControl::ephemeral_5m());

    complete(&router, req).await;

    assert!(declared(&provider.only(), EditSite::CachePlacement).is_empty());
}

// ---------------------------------------------------------------------------
// Replay strip
// ---------------------------------------------------------------------------

fn encrypted_detail(id: &str, format: &str) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: Some(id.into()),
        format: Some(format.into()),
        index: None,
        payload: json!({"encrypted_content": "blob"}),
    }
}

#[test]
fn a_replay_strip_declares_each_wholly_removed_reasoning_input_item() {
    let body = json!({"input": [
        {"type": "message", "role": "user", "content": "q"},
        {"type": "reasoning", "id": "rs_drop", "encrypted_content": "blob"},
        {"type": "message", "role": "assistant", "content": "a"},
        {"type": "reasoning", "id": "rs_keep", "encrypted_content": "blob"}
    ]});
    let mut req = request(IngressDialect::Responses, &body);
    let mut assistant = text_message(Role::Assistant, "a");
    assistant.reasoning_details = vec![
        encrypted_detail("rs_drop", routectl_core::BEDROCK_MANTLE),
        encrypted_detail("rs_keep", routectl_core::CODEX_OAUTH),
    ];
    req.messages = vec![assistant].into();
    let mut meta = super::super::DispatchMeta::for_alias(ALIAS);

    let stripped = super::super::replay_repair::strip_replay_artifacts_recalibrating(
        &mut req,
        routectl_core::ReplayScheme::Codex,
        &mut meta,
    );

    assert!(stripped, "premise: the foreign artifact was stripped");
    assert_eq!(
        declared(&req, EditSite::ReplayStrip),
        [edit("/input/1", EditKind::Remove)]
    );
}

#[test]
fn a_replay_strip_with_nothing_removable_declares_nothing() {
    let body = json!({"input": [{"type": "reasoning", "id": "rs_keep"}]});
    let mut req = request(IngressDialect::Responses, &body);
    let mut assistant = text_message(Role::Assistant, "a");
    assistant.reasoning_details = vec![encrypted_detail("rs_keep", routectl_core::CODEX_OAUTH)];
    req.messages = vec![assistant].into();
    let mut meta = super::super::DispatchMeta::for_alias(ALIAS);

    let stripped = super::super::replay_repair::strip_replay_artifacts_recalibrating(
        &mut req,
        routectl_core::ReplayScheme::Codex,
        &mut meta,
    );

    assert!(!stripped);
    assert!(declared(&req, EditSite::ReplayStrip).is_empty());
}

// ---------------------------------------------------------------------------
// The patterns these sites emit
// ---------------------------------------------------------------------------

/// Every pattern shape a router site in this module can emit, by site. A new
/// shape belongs in this list; the constructor's blanket refusal must never
/// be the thing that silently drops one.
const SITE_PATTERNS: &[(EditSite, &str)] = &[
    (EditSite::ModelRename, "/model"),
    (EditSite::OperatorPayloadExtras, "/new_knob"),
    (EditSite::OperatorPayloadExtras, "/nested/changed"),
    (EditSite::ContextReduction, "/messages/1/content/0/content"),
    (EditSite::ContextReduction, "/messages/1/content/0/input"),
    (
        EditSite::ContextReduction,
        "/messages/*/tool_calls/0/function/arguments",
    ),
    (EditSite::ContextReduction, "/input/*/arguments"),
    (EditSite::CachePlacement, "/cache_control"),
    (EditSite::CachePlacement, "/system/0/cache_control"),
    (EditSite::CachePlacement, "/tools/0/cache_control"),
    (EditSite::FieldRepair, "/thinking/display"),
    (EditSite::Preflight, "/thinking/display"),
    (EditSite::Canary, "/thinking/display"),
    (EditSite::LearnedRepair, "/tools/0"),
    (EditSite::LearnedRepair, "/context_management"),
    (EditSite::ReplayStrip, "/input/1"),
];

#[test]
fn every_router_site_pattern_is_accepted_and_none_is_blanket() {
    for (site, pattern) in SITE_PATTERNS {
        let made = DeclaredEdit::new(*site, *pattern, EditKind::Remove);

        assert!(made.is_ok(), "{} {pattern}: {made:?}", site.label());
    }
}

#[test]
fn a_blanket_operator_extras_key_is_refused_rather_than_declared() {
    for blanket in ["/messages", "/system", "/tools"] {
        assert_eq!(
            DeclaredEdit::new(EditSite::OperatorPayloadExtras, blanket, EditKind::Add),
            Err(EditPatternError::Blanket),
            "{blanket}"
        );
    }
}
