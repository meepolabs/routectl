//! The live request-leg wire check on successful attempts: a sampled success
//! compares the client's body with what the egress deposited into the
//! attempt's tap, and records loss, clean or incomplete on the router's
//! wire-check state. A test provider stands in for the egress deposit.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::wire_check::{
    DeclaredEdit, EditKind, EditSite, EgressKind, IngressDialect, IngressWire,
};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Error, Message,
    MessageContent, Provider, Result, Role,
};
use serde_json::{Value, json};

use super::egress_kind_of;
use crate::config::{Config, ProviderEntry};
use crate::resolved::ResolvedModel;
use crate::router::Router;
use crate::wire_check_state::{CheckOutcome, WireCheckSnapshot};

const ALIAS: &str = "a";
const LOST_KEY: &str = "metadata";
const LOST_PATH: &str = "/metadata";

/// What the test provider reports through the attempt's tap.
#[derive(Clone)]
enum Deposit {
    /// The client's body with one top-level key removed.
    IngressWithout(&'static str),
    /// A fixed body.
    Fixed(Value),
    /// Nothing at all.
    Nothing,
}

/// A provider that answers every call successfully and, like a real egress,
/// deposits a request body into the tap it was handed. Records each request
/// it saw.
struct DepositingProvider {
    deposit: Deposit,
    declare: Option<DeclaredEdit>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl DepositingProvider {
    fn new(deposit: Deposit) -> Self {
        Self {
            deposit,
            declare: None,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn declaring(mut self, edit: DeclaredEdit) -> Self {
        self.declare = Some(edit);
        self
    }

    fn report(&self, req: &ChatRequest) {
        self.seen.lock().push(req.clone());
        let Some(tap) = req.routectl_internal.wire_tap.as_ref() else {
            return;
        };
        if let Some(edit) = &self.declare {
            tap.declare(edit.clone());
        }
        let body = match &self.deposit {
            Deposit::Nothing => return,
            Deposit::Fixed(body) => body.clone(),
            Deposit::IngressWithout(key) => {
                let ingress = req
                    .routectl_internal
                    .ingress_wire
                    .as_ref()
                    .expect("a tap is only installed beside an ingress body");
                let mut body: Value = serde_json::from_slice(&ingress.bytes).expect("ingress json");
                body.as_object_mut().expect("object body").remove(*key);
                body
            }
        };
        tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
    }
}

#[async_trait::async_trait]
impl Provider for DepositingProvider {
    fn id(&self) -> &'static str {
        "depositing"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("depositing", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.report(&req);
        Ok(ok_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.report(&req);
        Ok(stream::iter([Ok(content_chunk())]).boxed())
    }
}

/// Deposit the client's body unchanged into the attempt's tap, as an egress
/// that sent it and was then refused would.
fn deposit_ingress_unchanged(req: &ChatRequest) {
    let (Some(tap), Some(ingress)) = (
        req.routectl_internal.wire_tap.as_ref(),
        req.routectl_internal.ingress_wire.as_ref(),
    ) else {
        return;
    };
    tap.deposit(ingress.bytes.clone());
}

fn unavailable() -> Error {
    Error::upstream("unavailable", 503, "down")
}

/// A provider whose every call deposits the client's body and then fails
/// with a retryable, fallbackable 503.
struct UnavailableProvider;

#[async_trait::async_trait]
impl Provider for UnavailableProvider {
    fn id(&self) -> &'static str {
        "unavailable"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("unavailable", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        deposit_ingress_unchanged(&req);
        Err(unavailable())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        deposit_ingress_unchanged(&req);
        Err(unavailable())
    }
}

/// A provider whose first call deposits the client's body and fails with a
/// retryable 503; every later call succeeds without depositing anything.
#[derive(Default)]
struct FailsOnceThenServesSilently {
    calls: Mutex<u32>,
}

impl FailsOnceThenServesSilently {
    fn first_call(&self, req: &ChatRequest) -> bool {
        let mut calls = self.calls.lock();
        *calls += 1;
        if *calls == 1 {
            deposit_ingress_unchanged(req);
            return true;
        }
        false
    }
}

#[async_trait::async_trait]
impl Provider for FailsOnceThenServesSilently {
    fn id(&self) -> &'static str {
        "fails-once"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("fails-once", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        if self.first_call(&req) {
            return Err(unavailable());
        }
        Ok(ok_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        if self.first_call(&req) {
            return Err(unavailable());
        }
        Ok(stream::iter([Ok(content_chunk())]).boxed())
    }
}

fn assistant(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn ok_response() -> ChatResponse {
    ChatResponse {
        id: "ok".into(),
        model: "m".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: assistant("ok"),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: None,
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
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

/// A router whose alias walks one model per provider in `chain`, each
/// `[providers.pN]` an anthropic-api entry so every target's egress kind is
/// `anthropic-api`.
fn router(wire_check: &str, chain: Vec<Arc<dyn Provider>>) -> Router {
    router_retrying(1, wire_check, chain)
}

/// [`router`] with `max_attempts` calls per chain target.
fn router_retrying(max_attempts: u32, wire_check: &str, chain: Vec<Arc<dyn Provider>>) -> Router {
    let mut toml_text = format!(
        "version = 5\n[retry]\nmax_attempts = {max_attempts}\ninitial_backoff_ms = 0\n\
         jitter_ms = 0\n[wire_check]\n{wire_check}\n"
    );
    let mut nicknames = Vec::new();
    for idx in 0..chain.len() {
        toml_text.push_str(&format!(
            "[providers.p{idx}]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n"
        ));
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
            let model = ResolvedModel::new(&nickname, format!("p{idx}"), provider, "upstream");
            (nickname, Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    router
}

const CHECK_EVERY: &str = "first_n = 8\nsample_one_in = 1";

fn client_body() -> Value {
    json!({
        "model": ALIAS,
        "max_tokens": 16,
        "metadata": {"user_id": "u-1"},
        "messages": [{"role": "user", "content": "hi"}]
    })
}

fn request_from(dialect: IngressDialect, body: &Value) -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
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
    request_from(IngressDialect::Anthropic, &client_body())
}

fn remove_lost_key() -> DeclaredEdit {
    DeclaredEdit::new(EditSite::OperatorPayloadExtras, LOST_PATH, EditKind::Remove)
        .expect("a concrete pointer is a valid pattern")
}

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
}

const SURFACES: [Surface; 2] = [Surface::Complete, Surface::Stream];

/// Dispatch `req` on `surface`, asserting the client still receives the
/// provider's answer unchanged.
async fn dispatch(router: &Router, surface: Surface, req: ChatRequest) {
    match surface {
        Surface::Complete => {
            let resp = Box::pin(router.complete(req))
                .await
                .expect("dispatch succeeds");
            assert!(
                matches!(&resp.choices[0].message.content, MessageContent::Text(t) if t == "ok")
            );
            assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        }
        Surface::Stream => {
            let stream = Box::pin(router.stream(req)).await.expect("stream opens");
            let chunks: Vec<_> = stream.collect().await;
            assert_eq!(chunks.len(), 1);
            let chunk = chunks[0].as_ref().expect("content chunk");
            assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("ok"));
        }
    }
}

fn outcomes(snapshot: &WireCheckSnapshot) -> Vec<(&'static str, CheckOutcome, u64)> {
    snapshot
        .checks
        .iter()
        .map(|c| (c.lane.label(), c.outcome, c.reading.count))
        .collect()
}

#[tokio::test]
async fn a_deposit_missing_an_ingress_key_counts_a_loss_named_by_path() {
    for surface in SURFACES {
        let provider = Arc::new(DepositingProvider::new(Deposit::IngressWithout(LOST_KEY)));
        let router = router(CHECK_EVERY, vec![provider]);

        let ((), events) =
            routectl_testkit::with_capture(dispatch(&router, surface, anthropic_request())).await;

        let snapshot = router.wire_check_snapshot();
        assert_eq!(
            outcomes(&snapshot),
            [("anthropic->anthropic-api", CheckOutcome::Loss, 1)],
            "{surface:?}"
        );
        assert_eq!(snapshot.loss.len(), 1, "{surface:?}");
        assert_eq!(snapshot.loss[0].path_class.label(), "missing");
        assert_eq!(snapshot.loss[0].reading.count, 1);
        assert_eq!(snapshot.findings.len(), 1, "{surface:?}");
        assert_eq!(snapshot.findings[0].path.as_str(), LOST_PATH);
        let warn = events
            .iter()
            .find(|e| e.target == "routectl_router::wire_check" && e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("{surface:?}: the loss must WARN"));
        assert_eq!(warn.field("path"), Some(LOST_PATH));
        assert_eq!(warn.field("path_class"), Some("missing"));
    }
}

#[tokio::test]
async fn a_loss_the_egress_declared_records_clean() {
    for surface in SURFACES {
        let provider = Arc::new(
            DepositingProvider::new(Deposit::IngressWithout(LOST_KEY)).declaring(remove_lost_key()),
        );
        let router = router(CHECK_EVERY, vec![provider]);

        dispatch(&router, surface, anthropic_request()).await;

        let snapshot = router.wire_check_snapshot();
        assert_eq!(
            outcomes(&snapshot),
            [("anthropic->anthropic-api", CheckOutcome::Clean, 1)],
            "{surface:?}"
        );
        assert!(snapshot.loss.is_empty(), "{surface:?}");
        assert!(snapshot.findings.is_empty(), "{surface:?}");
    }
}

#[tokio::test]
async fn an_unconsumed_top_level_extras_key_on_a_translation_lane_is_counted() {
    for surface in SURFACES {
        let provider = Arc::new(DepositingProvider::new(Deposit::Fixed(json!({
            "model": "upstream",
            "messages": [],
            "carried_knob": true
        }))));
        let router = router(CHECK_EVERY, vec![provider]);
        let mut req = request_from(IngressDialect::Chat, &json!({"model": ALIAS}));
        req.provider_extras = Some(json!({"carried_knob": true, "vendor_knob": 1}));

        dispatch(&router, surface, req).await;

        let snapshot = router.wire_check_snapshot();
        assert_eq!(
            outcomes(&snapshot),
            [("chat->anthropic-api", CheckOutcome::Loss, 1)],
            "{surface:?}"
        );
        assert_eq!(snapshot.loss.len(), 1);
        assert_eq!(
            snapshot.loss[0].path_class.label(),
            "translation_unconsumed"
        );
        assert_eq!(snapshot.findings.len(), 1);
        assert_eq!(snapshot.findings[0].path.as_str(), "/vendor_knob");
    }
}

#[tokio::test]
async fn the_ingress_body_and_a_prior_declared_edit_survive_a_second_chain_attempt() {
    for surface in SURFACES {
        let serving = Arc::new(DepositingProvider::new(Deposit::Nothing));
        let router = router(
            CHECK_EVERY,
            vec![Arc::new(UnavailableProvider), serving.clone()],
        );
        let mut req = anthropic_request();
        req.routectl_internal.declared_edits.push(remove_lost_key());
        let posted = req
            .routectl_internal
            .ingress_wire
            .as_ref()
            .map(|w| w.bytes.clone())
            .expect("ingress body");

        dispatch(&router, surface, req).await;

        let seen = serving.seen.lock();
        let second = seen.first().expect("the second chain target was called");
        let carried = second
            .routectl_internal
            .ingress_wire
            .as_ref()
            .unwrap_or_else(|| panic!("{surface:?}: the ingress body was dropped"));
        assert_eq!(carried.bytes, posted);
        assert_eq!(
            second.routectl_internal.declared_edits.first(),
            Some(&remove_lost_key()),
            "{surface:?}: the pre-overlay edit leads this attempt's own declarations"
        );
        assert_eq!(
            outcomes(&router.wire_check_snapshot()),
            [("anthropic->anthropic-api", CheckOutcome::Incomplete, 1)],
            "{surface:?}: the failed attempt's deposit must not stand in for the serving one"
        );
    }
}

#[tokio::test]
async fn a_retry_on_the_same_target_gets_a_fresh_tap() {
    // Only the complete surface retries a target in place; a stream walks on.
    let provider = Arc::new(FailsOnceThenServesSilently::default());
    let router = router_retrying(2, CHECK_EVERY, vec![provider.clone()]);

    dispatch(&router, Surface::Complete, anthropic_request()).await;

    assert_eq!(*provider.calls.lock(), 2, "the 503 was retried");
    assert_eq!(
        outcomes(&router.wire_check_snapshot()),
        [("anthropic->anthropic-api", CheckOutcome::Incomplete, 1)],
        "the refused call's deposit must not stand in for the retry"
    );
}

#[tokio::test]
async fn admission_is_charged_once_per_attempt() {
    for surface in SURFACES {
        let provider = Arc::new(DepositingProvider::new(Deposit::Nothing));
        let router = router("first_n = 1\nsample_one_in = 1000", vec![provider]);

        dispatch(&router, surface, anthropic_request()).await;
        dispatch(&router, surface, anthropic_request()).await;

        assert_eq!(
            outcomes(&router.wire_check_snapshot()),
            [("anthropic->anthropic-api", CheckOutcome::Incomplete, 1)],
            "{surface:?}: the first attempt is checked, the second is not"
        );
    }
}

#[tokio::test]
async fn the_tap_handed_to_the_provider_carries_the_admission_decision() {
    let rows = [
        ("admitted", CHECK_EVERY, true),
        ("not admitted", "first_n = 0\nsample_one_in = 1000", false),
    ];
    for surface in SURFACES {
        for (name, wire_check, expected) in rows {
            let provider = Arc::new(DepositingProvider::new(Deposit::Nothing));
            let router = router(wire_check, vec![provider.clone()]);

            dispatch(&router, surface, anthropic_request()).await;

            let seen = provider.seen.lock();
            let tap = seen[0]
                .routectl_internal
                .wire_tap
                .as_ref()
                .unwrap_or_else(|| panic!("{surface:?} {name}: a tap is installed"));
            assert_eq!(tap.is_admitted(), expected, "{surface:?} {name}");
        }
    }
}

#[tokio::test]
async fn an_unadmitted_success_records_no_check() {
    for surface in SURFACES {
        let provider = Arc::new(DepositingProvider::new(Deposit::IngressWithout(LOST_KEY)));
        let router = router("first_n = 0\nsample_one_in = 1000", vec![provider]);

        dispatch(&router, surface, anthropic_request()).await;

        let snapshot = router.wire_check_snapshot();
        assert!(snapshot.checks.is_empty(), "{surface:?}");
        assert!(snapshot.loss.is_empty(), "{surface:?}");
    }
}

#[tokio::test]
async fn a_provider_that_never_deposits_records_incomplete_not_clean() {
    for surface in SURFACES {
        let provider = Arc::new(DepositingProvider::new(Deposit::Nothing));
        let router = router(CHECK_EVERY, vec![provider]);

        dispatch(&router, surface, anthropic_request()).await;

        assert_eq!(
            outcomes(&router.wire_check_snapshot()),
            [("anthropic->anthropic-api", CheckOutcome::Incomplete, 1)],
            "{surface:?}"
        );
    }
}

#[tokio::test]
async fn a_request_with_no_ingress_body_gets_no_tap_and_no_check() {
    for surface in SURFACES {
        let provider = Arc::new(DepositingProvider::new(Deposit::Nothing));
        let router = router(CHECK_EVERY, vec![provider.clone()]);
        let req = ChatRequest {
            model: ALIAS.into(),
            messages: vec![].into(),
            ..Default::default()
        };

        dispatch(&router, surface, req).await;

        assert!(provider.seen.lock()[0].routectl_internal.wire_tap.is_none());
        assert!(
            router.wire_check_snapshot().checks.is_empty(),
            "{surface:?}"
        );
    }
}

fn only_entry(body: &str) -> ProviderEntry {
    let config: Config =
        toml::from_str(&format!("[providers.p]\n{body}")).expect("valid provider toml");
    config.providers.into_values().next().expect("one provider")
}

const MANTLE: &str =
    "bedrock_mantle = { region = \"us-east-1\", creds = { kind = \"default-chain\" } }\n";

#[test]
fn each_provider_entry_shape_derives_its_egress_kind() {
    let mut rows: Vec<(&str, String, EgressKind)> = vec![
        (
            "anthropic-api",
            "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n".into(),
            EgressKind::AnthropicApi,
        ),
        (
            "openai-compat",
            "kind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\n\
             api_key_ref = \"literal:k\"\n"
                .into(),
            EgressKind::OpenaiCompat,
        ),
    ];
    if cfg!(feature = "openai-responses") {
        rows.push((
            "openai-responses",
            "kind = \"openai-responses\"\napi_key_ref = \"literal:k\"\n".into(),
            EgressKind::OpenaiResponses,
        ));
    }
    if cfg!(feature = "gemini") {
        rows.push((
            "gemini",
            "kind = \"gemini\"\napi_key_ref = \"literal:k\"\n".into(),
            EgressKind::Gemini,
        ));
    }
    if cfg!(feature = "bedrock") {
        let bedrock = "kind = \"bedrock\"\nregion = \"us-east-1\"\n\
                       creds = { kind = \"default-chain\" }\n";
        rows.extend([
            (
                "bedrock default shape",
                bedrock.into(),
                EgressKind::BedrockInvoke,
            ),
            (
                "bedrock invoke",
                format!("{bedrock}api_shape = \"invoke\"\n"),
                EgressKind::BedrockInvoke,
            ),
            (
                "bedrock converse",
                format!("{bedrock}api_shape = \"converse\"\n"),
                EgressKind::BedrockConverse,
            ),
            (
                "anthropic-api mantle",
                format!("kind = \"anthropic-api\"\napi_key_ref = \"\"\n{MANTLE}"),
                EgressKind::AnthropicApiMantle,
            ),
            (
                "openai-compat mantle",
                format!("kind = \"openai-compat\"\napi_key_ref = \"\"\n{MANTLE}"),
                EgressKind::OpenaiCompatMantle,
            ),
        ]);
        if cfg!(feature = "openai-responses") {
            rows.push((
                "openai-responses mantle",
                format!("kind = \"openai-responses\"\napi_key_ref = \"\"\n{MANTLE}"),
                EgressKind::OpenaiResponsesMantle,
            ));
        }
    }

    for (name, body, expected) in rows {
        assert_eq!(egress_kind_of(&only_entry(&body)), expected, "{name}");
    }
}

#[path = "wire_check_charge_tests.rs"]
mod wire_check_charge_tests;
