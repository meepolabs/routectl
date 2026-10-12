//! Where a rejection points on the body that went out, read per dialect the
//! way the upstream's request validator names a location, and what the engine
//! does with a rejection naming a path that body did not carry: no repair, no
//! verdict for that path, and the wire check records it unrepaired. Every
//! reading the egress cannot settle leaves the rejection to proceed as it
//! would with no wire record at all.
//!
//! Test providers stand in for the egress deposit and the upstream's answer.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, BoxStream, StreamExt};
use routectl_core::wire_check::{IngressDialect, IngressWire};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result, ToolDef,
};
use serde_json::{Value, json};

use super::super::field_repair::{TEST_UNSUPPORTED_PATH, TEST_UNSUPPORTED_SENTINEL};
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::{DispatchMeta, Router, RouterOptions};
use crate::wire_check_state::{FieldRepairOutcome, WireCheckSnapshot};

const ALIAS: &str = "a";
const NEVER_SAMPLED: &str = "first_n = 0\nsample_one_in = 1000";
const REFUSED_FIELD: &str = "Extra inputs are not permitted";
const CONVERSE_BAG: &str = "additionalModelRequestFields";
const DISPLAY_FIXTURE: &str =
    include_str!("../../tests/fixtures/rejections/anthropic-api/thinking-display-enum.json");
const CONVERSE_FIXTURE: &str =
    include_str!("../../tests/fixtures/rejections/bedrock/thinking-type-enabled-unsupported.json");
const TEMPERATURE_FIXTURE: &str = include_str!(
    "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-temperature.json"
);

const ANTHROPIC: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";
const OPENAI_COMPAT: &str = "kind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";
const CONVERSE: &str = "kind = \"bedrock\"\nregion = \"us-east-1\"\n\
                        creds = { kind = \"default-chain\" }\napi_shape = \"converse\"\n";
const INVOKE: &str = "kind = \"bedrock\"\nregion = \"us-east-1\"\n\
                      creds = { kind = \"default-chain\" }\napi_shape = \"invoke\"\n";
const OPENAI_RESPONSES: &str = "kind = \"openai-responses\"\napi_key_ref = \"literal:k\"\n";
const GEMINI: &str = "kind = \"gemini\"\napi_key_ref = \"literal:k\"\n";

/// How the test provider builds the body it deposits from the client's.
type Shape = fn(&ChatRequest, Value) -> Value;
/// How the test provider answers an attempt, given the request.
type Answer = fn(&ChatRequest) -> Result<()>;

/// A provider that, like a real egress, deposits the body it sent into the
/// attempt's tap (unless it has no `shape`), then answers as told. Counts the
/// upstream calls it took.
struct ScriptedProvider {
    shape: Option<Shape>,
    answer: Answer,
    calls: AtomicUsize,
}

impl ScriptedProvider {
    fn new(shape: Option<Shape>, answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            shape,
            answer,
            calls: AtomicUsize::new(0),
        })
    }

    fn run(&self, req: &ChatRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let (Some(shape), Some(tap), Some(ingress)) = (
            self.shape,
            req.routectl_internal.wire_tap.as_ref(),
            req.routectl_internal.ingress_wire.as_ref(),
        ) {
            let client: Value = serde_json::from_slice(&ingress.bytes).expect("ingress json");
            let sent = shape(req, client);
            tap.deposit(serde_json::to_vec(&sent).expect("serialize").into());
        }
        (self.answer)(req)
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("scripted", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.run(&req).map(|()| ChatResponse::default())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.run(&req)
            .map(|()| stream::iter([Ok(content_chunk())]).boxed())
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

/// A router whose alias walks one target, `provider` on an entry configured
/// by `provider_block`, one call per attempt.
fn router(provider_block: &str, provider: Arc<dyn Provider>) -> Router {
    let toml_text = format!(
        "version = 5\n[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\
         [wire_check]\n{NEVER_SAMPLED}\n[providers.p0]\n{provider_block}\
         [aliases]\n{ALIAS} = [\"m0\"]\n"
    );
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let model = ResolvedModel::new("m0", "p0", provider, "upstream");
    router.install_resolved_models(BTreeMap::from([("m0".to_string(), Arc::new(model))]));
    router
}

fn request(dialect: IngressDialect) -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let client = json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]});
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect,
        client_version: None,
        bytes: serde_json::to_vec(&client).expect("serialize").into(),
    });
    req
}

// ---------------------------------------------------------------------------
// Carriers: what makes a closed-table row repairable on the attempt
// ---------------------------------------------------------------------------

fn carry_test_row(req: &mut ChatRequest) {
    req.user = Some(TEST_UNSUPPORTED_SENTINEL.to_string());
}

fn carries_test_row(req: &ChatRequest) -> bool {
    req.user.as_deref() == Some(TEST_UNSUPPORTED_SENTINEL)
}

fn carry_display(req: &mut ChatRequest) {
    req.routectl_internal.anthropic_thinking_display = Some("bogus".into());
}

fn carries_display(req: &ChatRequest) -> bool {
    req.routectl_internal.anthropic_thinking_display.is_some()
}

fn carry_web_search(req: &mut ChatRequest) {
    req.tools = Some(vec![ToolDef::Other(json!({"type": "web_search"}))]);
}

// ---------------------------------------------------------------------------
// Egress shapes
// ---------------------------------------------------------------------------

fn test_row_at_root(req: &ChatRequest, mut client: Value) -> Value {
    if carries_test_row(req) {
        client[TEST_UNSUPPORTED_PATH] = json!(true);
    }
    client
}

fn converse_test_row_in_bag(req: &ChatRequest, mut client: Value) -> Value {
    client[CONVERSE_BAG] = json!({});
    if carries_test_row(req) {
        client[CONVERSE_BAG][TEST_UNSUPPORTED_PATH] = json!(true);
    }
    client
}

fn converse_thinking_enabled(_: &ChatRequest, mut client: Value) -> Value {
    client[CONVERSE_BAG] = json!({"thinking": {"type": "enabled", "budget_tokens": 1024}});
    client
}

fn converse_empty_bag(_: &ChatRequest, mut client: Value) -> Value {
    client[CONVERSE_BAG] = json!({});
    client
}

/// The thinking union on its `enabled` member, carrying `display` while the
/// request carries it.
fn thinking_with_display(req: &ChatRequest, mut client: Value) -> Value {
    client["thinking"] = json!({"type": "enabled"});
    if carries_display(req) {
        client["thinking"]["display"] = json!("bogus");
    }
    client
}

fn thinking_and_display(_: &ChatRequest, mut client: Value) -> Value {
    client["thinking"] = json!({"type": "enabled", "display": "bogus"});
    client
}

fn thinking_without_display(_: &ChatRequest, mut client: Value) -> Value {
    client["thinking"] = json!({"type": "enabled"});
    client
}

fn unchanged(_: &ChatRequest, client: Value) -> Value {
    client
}

fn web_search_tool(_: &ChatRequest, mut client: Value) -> Value {
    client["tools"] = json!([{"type": "web_search"}]);
    client
}

fn temperature(_: &ChatRequest, mut client: Value) -> Value {
    client["temperature"] = json!(0.5);
    client
}

// ---------------------------------------------------------------------------
// Upstream rejections
// ---------------------------------------------------------------------------

/// `(status, body)` of a committed rejection fixture.
fn fixture(text: &str) -> (u16, String) {
    let fixture: Value = serde_json::from_str(text).expect("fixture json");
    let status = u16::try_from(fixture["status"].as_u64().expect("status")).expect("u16");
    (status, fixture["body"].as_str().expect("body").to_string())
}

fn fixture_rejection(text: &str) -> Error {
    let (status, body) = fixture(text);
    Error::upstream("scripted", status, body)
}

/// An anthropic-api 400 whose message names `path` as a refused field.
fn anthropic_rejection(path: &str) -> Error {
    let body = json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": format!("{path}: {REFUSED_FIELD}")},
    });
    Error::upstream("scripted", 400, body.to_string())
}

/// A bedrock-runtime 400 whose flat message names `path` as a refused field.
fn bedrock_rejection(path: &str) -> Error {
    let body = json!({"message": format!("{path}: {REFUSED_FIELD}")});
    Error::upstream("scripted", 400, body.to_string())
}

/// An openai-shaped 400 refusing `param` as an unsupported parameter.
fn openai_rejection(param: &str) -> Error {
    let body = json!({"error": {
        "message": format!("Unsupported parameter: '{param}' is not supported with this model."),
        "type": "invalid_request_error",
        "param": param,
        "code": "unsupported_parameter",
    }});
    Error::upstream_full(
        "scripted",
        400,
        body.to_string(),
        None,
        Some("invalid_request_error".into()),
        Some("unsupported_parameter".into()),
    )
}

fn bag_prefixed(path: &str) -> String {
    format!("{CONVERSE_BAG}.{path}")
}

// ---------------------------------------------------------------------------
// What the settlement reads off the wire, per dialect
// ---------------------------------------------------------------------------

/// One rejection settled against one deposited body.
struct SiteRow {
    name: &'static str,
    provider_block: &'static str,
    dialect: IngressDialect,
    /// The body deposited, built from the client's; `None` deposits nothing.
    sent: Option<Shape>,
    err: fn() -> Error,
    /// What the settlement says about the body carrying the named path.
    in_egress: Option<bool>,
}

/// Settle `row`'s rejection the way a failed attempt is settled, and return
/// what the settlement read off the wire.
fn settle(row: &SiteRow) -> Option<bool> {
    let provider: Arc<dyn Provider> = ScriptedProvider::new(None, |_| Ok(()));
    let router = router(row.provider_block, Arc::clone(&provider));
    let model = ResolvedModel::new("m0", "p0", provider, "upstream");
    let target = router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target");
    let mut attempt_req = request(row.dialect);
    carry_test_row(&mut attempt_req);
    let (tap, charge) = router.install_wire_tap(&target, &mut attempt_req);
    if let (Some(shape), Some(tap)) = (row.sent, tap.as_ref()) {
        let client = json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]});
        let sent = shape(&attempt_req, client);
        tap.deposit(serde_json::to_vec(&sent).expect("serialize").into());
    }
    let mut meta = DispatchMeta::for_alias(ALIAS);
    let site = router
        .settle_failed_attempt(
            &target,
            &attempt_req,
            tap.as_ref(),
            charge,
            &(row.err)(),
            &mut meta,
        )
        .unwrap_or_else(|| panic!("{}: premise, the rejection is recorded", row.name));
    site.in_egress
}

fn site_rows() -> Vec<SiteRow> {
    let mut rows = vec![
        SiteRow {
            name: "a tagged-union member named by its tag, carried",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(thinking_and_display),
            err: || fixture_rejection(DISPLAY_FIXTURE),
            in_egress: Some(true),
        },
        SiteRow {
            name: "a tagged-union member named by its tag, not carried",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(thinking_without_display),
            err: || fixture_rejection(DISPLAY_FIXTURE),
            in_egress: Some(false),
        },
        SiteRow {
            name: "a body parameter named in error.param, carried",
            provider_block: OPENAI_COMPAT,
            dialect: IngressDialect::Chat,
            sent: Some(temperature),
            err: || fixture_rejection(TEMPERATURE_FIXTURE),
            in_egress: Some(true),
        },
        SiteRow {
            name: "a capability named in error.param, which is no body key",
            provider_block: OPENAI_COMPAT,
            dialect: IngressDialect::Chat,
            sent: Some(web_search_tool),
            err: || openai_rejection("web_search"),
            in_egress: Some(false),
        },
        SiteRow {
            name: "a Responses client on a translation lane, located on the egress",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Responses,
            sent: Some(test_row_at_root),
            err: || anthropic_rejection(TEST_UNSUPPORTED_PATH),
            in_egress: Some(true),
        },
        SiteRow {
            name: "a path past the end of an array the body carries",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(unchanged),
            err: || anthropic_rejection("messages.5.content"),
            in_egress: None,
        },
        SiteRow {
            name: "a path coarsened past the pointer bound",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(unchanged),
            err: || anthropic_rejection("a.b.c.d.e.f.g.h.i"),
            in_egress: None,
        },
        SiteRow {
            name: "no deposit",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: None,
            err: || anthropic_rejection(TEST_UNSUPPORTED_PATH),
            in_egress: None,
        },
        SiteRow {
            name: "a rejection naming no path",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(unchanged),
            err: || {
                let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"malformed request"}}"#;
                Error::upstream("scripted", 400, body)
            },
            in_egress: None,
        },
    ];
    if cfg!(feature = "bedrock") {
        rows.extend(converse_site_rows());
    }
    if cfg!(feature = "openai-responses") {
        rows.push(SiteRow {
            name: "an openai-responses egress, whose envelope no parser reads",
            provider_block: OPENAI_RESPONSES,
            dialect: IngressDialect::Responses,
            sent: Some(unchanged),
            err: || openai_rejection(TEST_UNSUPPORTED_PATH),
            in_egress: None,
        });
    }
    if cfg!(feature = "gemini") {
        rows.push(SiteRow {
            name: "a gemini egress, whose envelope no parser reads",
            provider_block: GEMINI,
            dialect: IngressDialect::Chat,
            sent: Some(unchanged),
            err: || openai_rejection(TEST_UNSUPPORTED_PATH),
            in_egress: None,
        });
    }
    rows
}

fn converse_site_rows() -> [SiteRow; 6] {
    [
        SiteRow {
            name: "a Converse model field named relative to the bag, carried in it",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            sent: Some(converse_test_row_in_bag),
            err: || bedrock_rejection(TEST_UNSUPPORTED_PATH),
            in_egress: Some(true),
        },
        SiteRow {
            name: "a Converse field named through the bag, carried in it",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            sent: Some(converse_test_row_in_bag),
            err: || bedrock_rejection(&bag_prefixed(TEST_UNSUPPORTED_PATH)),
            in_egress: Some(true),
        },
        SiteRow {
            name: "a Converse field named through the bag, not carried",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            sent: Some(converse_empty_bag),
            err: || bedrock_rejection(&bag_prefixed(TEST_UNSUPPORTED_PATH)),
            in_egress: Some(false),
        },
        SiteRow {
            name: "a Converse model field the bag lacks, which Converse may spell elsewhere",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            sent: Some(converse_empty_bag),
            err: || bedrock_rejection(TEST_UNSUPPORTED_PATH),
            in_egress: None,
        },
        SiteRow {
            // The captured rejection names the refused VALUE: the walk meets
            // a scalar before the path ends, which says nothing of absence.
            name: "the captured Converse rejection naming a refused value",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            sent: Some(converse_thinking_enabled),
            err: || fixture_rejection(CONVERSE_FIXTURE),
            in_egress: None,
        },
        SiteRow {
            name: "a bag-prefixed path on an Invoke body, which has no bag",
            provider_block: INVOKE,
            dialect: IngressDialect::Anthropic,
            sent: Some(unchanged),
            err: || bedrock_rejection(&bag_prefixed(TEST_UNSUPPORTED_PATH)),
            in_egress: None,
        },
    ]
}

#[test]
fn the_settlement_reads_each_rejection_against_the_body_that_went_out() {
    for row in site_rows() {
        assert_eq!(settle(&row), row.in_egress, "{}", row.name);
    }
}

// ---------------------------------------------------------------------------
// What the engine does with it, on both dispatch surfaces
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
}

const SURFACES: [Surface; 2] = [Surface::Complete, Surface::Stream];

/// What one dispatch did: whether it served, the repair records, and the
/// capabilities it learned.
struct Outcome {
    served: bool,
    field_repairs: usize,
    learned: Vec<String>,
}

async fn dispatch(router: &Router, surface: Surface, req: ChatRequest) -> Outcome {
    let (served, meta) = match surface {
        Surface::Complete => {
            let out = Box::pin(router.complete_with_options(req, RouterOptions::new())).await;
            (out.result.is_ok(), out.meta)
        }
        Surface::Stream => {
            let out = Box::pin(router.stream_with_options(req, RouterOptions::new())).await;
            (out.result.is_ok(), out.meta)
        }
    };
    Outcome {
        served,
        field_repairs: meta.field_repairs.len(),
        learned: meta
            .learned_capabilities
            .iter()
            .map(|event| event.capability_key.clone())
            .collect(),
    }
}

/// `(provenance, field-repair outcome)` of every rejection finding.
fn rejections(snapshot: &WireCheckSnapshot) -> Vec<(&'static str, FieldRepairOutcome)> {
    snapshot
        .findings
        .iter()
        .filter_map(|f| Some((f.provenance?.label(), f.field_repair_outcome?)))
        .collect()
}

/// One repairable rejection dispatched end to end.
struct DispatchRow {
    name: &'static str,
    provider_block: &'static str,
    dialect: IngressDialect,
    carry: fn(&mut ChatRequest),
    sent: Option<Shape>,
    answer: Answer,
    /// Upstream calls the dispatch took: two when a repair re-sent the body.
    calls: usize,
    /// The finding the wire check records: provenance label and outcome.
    finding: (&'static str, FieldRepairOutcome),
}

async fn run_dispatch_rows(rows: &[DispatchRow]) {
    for surface in SURFACES {
        for row in rows {
            let provider = ScriptedProvider::new(row.sent, row.answer);
            let router = router(
                row.provider_block,
                Arc::clone(&provider) as Arc<dyn Provider>,
            );
            let mut req = request(row.dialect);
            (row.carry)(&mut req);

            let outcome = dispatch(&router, surface, req).await;

            let repaired = row.calls == 2;
            let name = row.name;
            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                row.calls,
                "{surface:?} {name}: upstream calls"
            );
            assert_eq!(outcome.served, repaired, "{surface:?} {name}: served");
            assert_eq!(
                outcome.field_repairs,
                usize::from(repaired),
                "{surface:?} {name}: repair records"
            );
            if !repaired {
                assert!(
                    outcome.learned.is_empty(),
                    "{surface:?} {name}: no verdict: {:?}",
                    outcome.learned
                );
            }
            assert_eq!(
                rejections(&router.wire_check_snapshot()),
                [row.finding],
                "{surface:?} {name}: the wire check finding"
            );
        }
    }
}

/// Refuse with `err` while the request carries the test-only row.
fn refuse_test_row(req: &ChatRequest, err: fn() -> Error) -> Result<()> {
    if carries_test_row(req) {
        return Err(err());
    }
    Ok(())
}

fn reject_display_while_carried(req: &ChatRequest) -> Result<()> {
    if carries_display(req) {
        return Err(fixture_rejection(DISPLAY_FIXTURE));
    }
    Ok(())
}

#[tokio::test]
async fn an_anthropic_rejection_is_repaired_only_when_the_egress_carries_its_path() {
    run_dispatch_rows(&[
        DispatchRow {
            name: "tagged-union path carried: repaired",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_display,
            sent: Some(thinking_with_display),
            answer: reject_display_while_carried,
            calls: 2,
            finding: ("routectl_added", FieldRepairOutcome::RepairedLossy),
        },
        DispatchRow {
            name: "tagged-union path not carried: ignored",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_display,
            sent: Some(thinking_without_display),
            answer: reject_display_while_carried,
            calls: 1,
            finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
        },
        DispatchRow {
            name: "field not carried: ignored",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: Some(unchanged),
            answer: |req| refuse_test_row(req, || anthropic_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 1,
            finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
        },
        DispatchRow {
            name: "no deposit: repaired as with no wire record",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: None,
            answer: |req| refuse_test_row(req, || anthropic_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 2,
            finding: ("unlocalized", FieldRepairOutcome::Repaired),
        },
        DispatchRow {
            name: "coarsened path: proceeds as with no wire record",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: Some(unchanged),
            answer: |_| Err(anthropic_rejection("a.b.c.d.e.f.g.h.i")),
            calls: 1,
            finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
        },
    ])
    .await;
}

#[tokio::test]
async fn a_responses_client_on_a_translation_lane_is_located_on_the_egress() {
    run_dispatch_rows(&[
        DispatchRow {
            name: "carried on the egress: repaired",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Responses,
            carry: carry_test_row,
            sent: Some(test_row_at_root),
            answer: |req| refuse_test_row(req, || anthropic_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 2,
            finding: ("n_a", FieldRepairOutcome::Repaired),
        },
        DispatchRow {
            name: "not carried on the egress: ignored",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Responses,
            carry: carry_test_row,
            sent: Some(unchanged),
            answer: |req| refuse_test_row(req, || anthropic_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 1,
            finding: ("n_a", FieldRepairOutcome::Unrepaired),
        },
    ])
    .await;
}

#[tokio::test]
async fn an_openai_param_rejection_is_located_on_the_body() {
    run_dispatch_rows(&[
        DispatchRow {
            name: "param carried: repaired",
            provider_block: OPENAI_COMPAT,
            dialect: IngressDialect::Chat,
            carry: carry_test_row,
            sent: Some(test_row_at_root),
            answer: |req| refuse_test_row(req, || openai_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 2,
            finding: ("routectl_added", FieldRepairOutcome::Repaired),
        },
        DispatchRow {
            name: "param not carried: ignored",
            provider_block: OPENAI_COMPAT,
            dialect: IngressDialect::Chat,
            carry: carry_test_row,
            sent: Some(unchanged),
            answer: |req| refuse_test_row(req, || openai_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 1,
            finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
        },
    ])
    .await;
}

#[tokio::test]
async fn a_converse_rejection_is_located_in_the_field_bag() {
    if !cfg!(feature = "bedrock") {
        return;
    }
    run_dispatch_rows(&[
        DispatchRow {
            name: "model field carried in the bag: repaired",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: Some(converse_test_row_in_bag),
            answer: |req| refuse_test_row(req, || bedrock_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 2,
            finding: ("n_a", FieldRepairOutcome::Repaired),
        },
        DispatchRow {
            name: "bag-prefixed field not carried: ignored",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: Some(converse_empty_bag),
            answer: |req| {
                refuse_test_row(req, || {
                    bedrock_rejection(&bag_prefixed(TEST_UNSUPPORTED_PATH))
                })
            },
            calls: 1,
            finding: ("n_a", FieldRepairOutcome::Unrepaired),
        },
    ])
    .await;
}

#[tokio::test]
async fn an_openai_responses_rejection_proceeds_as_with_no_wire_record() {
    if !cfg!(feature = "openai-responses") {
        return;
    }
    run_dispatch_rows(&[DispatchRow {
        name: "no parser arm",
        provider_block: OPENAI_RESPONSES,
        dialect: IngressDialect::Responses,
        carry: carry_test_row,
        sent: Some(unchanged),
        answer: |req| refuse_test_row(req, || openai_rejection(TEST_UNSUPPORTED_PATH)),
        calls: 1,
        finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
    }])
    .await;
}

/// A capability named in `error.param` is no body path, so its absence from
/// the body suppresses no capability learning: the generic learner keeps its
/// own gate on what the request carries.
#[tokio::test]
async fn a_capability_named_as_a_param_is_still_learned_when_the_body_lacks_that_key() {
    for surface in SURFACES {
        let provider = ScriptedProvider::new(Some(web_search_tool), |_| {
            Err(openai_rejection("web_search"))
        });
        let router = router(OPENAI_COMPAT, Arc::clone(&provider) as Arc<dyn Provider>);
        let mut req = request(IngressDialect::Chat);
        carry_web_search(&mut req);

        let outcome = dispatch(&router, surface, req).await;

        assert!(!outcome.served, "{surface:?}: premise, the 400 is terminal");
        assert_eq!(outcome.learned, ["web_search"], "{surface:?}");
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("unlocalized", FieldRepairOutcome::Unrepaired)],
            "{surface:?}"
        );
    }
}
