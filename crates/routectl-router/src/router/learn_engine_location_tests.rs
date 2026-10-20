//! Where a rejection points on the body that went out, read per dialect the
//! way the upstream's request validator names a location: whether the body
//! carried the named path, did not, or cannot tell. What the engine then does
//! with each reading lives in the dispatch tests below.
//!
//! Test providers stand in for the egress deposit and the upstream's answer.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::wire_check::{IngressDialect, IngressWire};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result,
    TokenCount, ToolDef,
};
use serde_json::{Value, json};

use super::super::field_repair::{TEST_UNSUPPORTED_PATH, TEST_UNSUPPORTED_SENTINEL};
use super::RejectionSite;
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::{DispatchMeta, Router};

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
const BEDROCK_TEMPERATURE_FIXTURE: &str =
    include_str!("../../tests/fixtures/rejections/bedrock/temperature-field-unsupported.json");

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
/// upstream calls it took and keeps the messages each one carried.
struct ScriptedProvider {
    shape: Option<Shape>,
    answer: Answer,
    calls: AtomicUsize,
    messages: Mutex<Vec<Value>>,
}

impl ScriptedProvider {
    fn new(shape: Option<Shape>, answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            shape,
            answer,
            calls: AtomicUsize::new(0),
            messages: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn run(&self, req: &ChatRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.messages
            .lock()
            .push(serde_json::to_value(&*req.messages).expect("serialize messages"));
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
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.run(&req).map(|()| TokenCount {
            input_tokens: 1,
            extras: serde_json::Map::new(),
        })
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

/// A Converse body carrying temperature where Converse spells it.
fn converse_inference_temperature(_: &ChatRequest, mut client: Value) -> Value {
    client["inferenceConfig"] = json!({"temperature": 0.5});
    client
}

fn converse_inference_temperature_and_empty_bag(req: &ChatRequest, client: Value) -> Value {
    converse_empty_bag(req, converse_inference_temperature(req, client))
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
    anthropic_reason(path, REFUSED_FIELD)
}

/// An anthropic-api 400 whose message names `path` with `reason`.
fn anthropic_reason(path: &str, reason: &str) -> Error {
    anthropic_message(&format!("{path}: {reason}"))
}

/// An anthropic-api 400 carrying `message`.
fn anthropic_message(message: &str) -> Error {
    anthropic_status_message(400, message)
}

/// An anthropic-api rejection on `status` carrying `message`.
fn anthropic_status_message(status: u16, message: &str) -> Error {
    let body = json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": message},
    });
    Error::upstream("scripted", status, body.to_string())
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
    settle_site(row).in_egress
}

fn settle_site(row: &SiteRow) -> RejectionSite {
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
    router
        .settle_failed_attempt(
            &target,
            &attempt_req,
            tap.as_ref(),
            charge,
            &(row.err)(),
            &mut meta,
        )
        .unwrap_or_else(|| panic!("{}: premise, the rejection is recorded", row.name))
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

fn converse_site_rows() -> [SiteRow; 8] {
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
            name: "the captured Converse temperature refusal, temperature in inferenceConfig",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            sent: Some(converse_inference_temperature),
            err: || fixture_rejection(BEDROCK_TEMPERATURE_FIXTURE),
            in_egress: None,
        },
        SiteRow {
            name: "the captured Converse temperature refusal beside an empty bag",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            sent: Some(converse_inference_temperature_and_empty_bag),
            err: || fixture_rejection(BEDROCK_TEMPERATURE_FIXTURE),
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

/// The Converse temperature refusal names a root path a Converse body never
/// spells at the root, so the walk cannot call it absent and no proposer is
/// suppressed. The rejection does localize, and the same walk reads a root
/// field the body lacks as absent, so the `None` is about where Converse
/// spells temperature.
#[cfg(feature = "bedrock")]
#[test]
fn a_converse_temperature_refusal_is_not_read_as_an_absent_field() {
    let row = |name, sent: Shape, err: fn() -> Error, in_egress| SiteRow {
        name,
        provider_block: CONVERSE,
        dialect: IngressDialect::Anthropic,
        sent: Some(sent),
        err,
        in_egress,
    };
    let refusal = row(
        "temperature in inferenceConfig",
        converse_inference_temperature,
        || fixture_rejection(BEDROCK_TEMPERATURE_FIXTURE),
        None,
    );
    let site = settle_site(&refusal);
    assert_eq!(site.in_egress, None, "{}", refusal.name);
    assert!(
        site.locator
            .as_ref()
            .is_some_and(|locator| locator.segments().eq(["temperature"])),
        "premise: the refusal localizes to the root temperature path",
    );

    let control = row(
        "a refused root field the Converse body lacks everywhere",
        converse_empty_bag,
        || bedrock_rejection(&bag_prefixed("temperature")),
        Some(false),
    );
    assert_eq!(settle(&control), control.in_egress, "{}", control.name);
}

// ---------------------------------------------------------------------------
// Absence counts only where the validator read a field the body lacks
// ---------------------------------------------------------------------------

/// A tool with no `type`, which the validator reads as the default member
/// of the tool union and names by that member's tag.
fn untagged_tool(_: &ChatRequest, mut client: Value) -> Value {
    client["tools"] = json!([{"name": "x", "input_schema": {}}]);
    client
}

fn metadata_with_dotted_key(_: &ChatRequest, mut client: Value) -> Value {
    client["metadata"] = json!({"a.b": 1});
    client
}

/// The thinking union on its `enabled` member, which also carries a key
/// spelled like the tag, holding `budget`.
fn thinking_with_key_named_like_its_tag(_: &ChatRequest, mut client: Value) -> Value {
    client["thinking"] = json!({"type": "enabled", "enabled": {"budget": 1}});
    client
}

/// An openai-shaped 400 naming `param` as missing, not refused.
fn openai_missing(param: &str) -> Error {
    let body = json!({"error": {
        "message": format!("Missing required parameter: '{param}'."),
        "type": "invalid_request_error",
        "param": param,
        "code": "missing_required_parameter",
    }});
    Error::upstream_full(
        "scripted",
        400,
        body.to_string(),
        None,
        Some("invalid_request_error".into()),
        Some("missing_required_parameter".into()),
    )
}

#[test]
fn only_a_miss_at_the_final_segment_is_absence() {
    let rows = [
        SiteRow {
            name: "a default union member's implied tag, before the field it holds",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(untagged_tool),
            err: || anthropic_rejection("tools.0.custom.name"),
            in_egress: None,
        },
        SiteRow {
            name: "a client key containing a dot, read as two segments",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(metadata_with_dotted_key),
            err: || anthropic_rejection("metadata.a.b"),
            in_egress: None,
        },
        SiteRow {
            name: "every segment but the last resolved, the last missing",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(thinking_without_display),
            err: || anthropic_rejection("thinking.enabled.display"),
            in_egress: Some(false),
        },
    ];
    for row in rows {
        assert_eq!(settle(&row), row.in_egress, "{}", row.name);
    }
}

#[test]
fn a_segment_carried_both_as_a_key_and_as_the_tag_is_not_read_either_way() {
    let rows = [
        SiteRow {
            name: "the key's child carries the field",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(thinking_with_key_named_like_its_tag),
            err: || anthropic_rejection("thinking.enabled.budget"),
            in_egress: None,
        },
        SiteRow {
            name: "the key's child lacks the field",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(thinking_with_key_named_like_its_tag),
            err: || anthropic_rejection("thinking.enabled.display"),
            in_egress: None,
        },
    ];
    for row in rows {
        assert_eq!(settle(&row), row.in_egress, "{}", row.name);
    }
}

/// The wire check's provenance still reads such a segment as the key: the
/// field is found under it on the body that went out, and not on the
/// client's, so routectl added it.
#[test]
fn the_wire_check_provenance_still_reads_a_tag_spelled_key_as_the_key() {
    let row = SiteRow {
        name: "the key's child carries the field",
        provider_block: ANTHROPIC,
        dialect: IngressDialect::Anthropic,
        sent: Some(thinking_with_key_named_like_its_tag),
        err: || anthropic_rejection("thinking.enabled.budget"),
        in_egress: None,
    };

    let site = settle_site(&row);

    assert_eq!(site.provenance.label(), "routectl_added", "{}", row.name);
}

#[test]
fn absence_counts_only_for_a_reason_that_refuses_what_was_sent() {
    let rows = [
        SiteRow {
            name: "a field reported missing",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(thinking_without_display),
            err: || anthropic_reason("thinking.enabled.budget_tokens", "Field required"),
            in_egress: None,
        },
        SiteRow {
            name: "the same absent field refused as an extra input",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(thinking_without_display),
            err: || anthropic_rejection("thinking.enabled.budget_tokens"),
            in_egress: Some(false),
        },
        SiteRow {
            name: "a quoted path not supported, with no grounded continuation",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(thinking_without_display),
            err: || anthropic_message(r#""thinking.display" is not supported"#),
            in_egress: None,
        },
        SiteRow {
            name: "the same quoted path in the grounded template",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(thinking_without_display),
            err: || {
                anthropic_message(
                    r#""thinking.display" is not supported for this model. Use "thinking.type" instead."#,
                )
            },
            in_egress: Some(false),
        },
        SiteRow {
            name: "an openai parameter reported missing",
            provider_block: OPENAI_COMPAT,
            dialect: IngressDialect::Chat,
            sent: Some(unchanged),
            err: || openai_missing("temperature"),
            in_egress: None,
        },
        SiteRow {
            name: "the same absent openai parameter refused as unsupported",
            provider_block: OPENAI_COMPAT,
            dialect: IngressDialect::Chat,
            sent: Some(unchanged),
            err: || openai_rejection("temperature"),
            in_egress: Some(false),
        },
    ];
    for row in rows {
        assert_eq!(settle(&row), row.in_egress, "{}", row.name);
    }
}

#[path = "learn_engine_dispatch_tests.rs"]
mod learn_engine_dispatch_tests;

#[path = "transcript_defect_dispatch_tests.rs"]
mod transcript_defect_dispatch_tests;
