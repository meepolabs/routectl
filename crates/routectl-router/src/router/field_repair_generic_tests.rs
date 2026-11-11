//! The generic carried-member arm of the reactive field repair, end to end
//! through the complete, pre-content stream and token-count walks on an
//! anthropic-api seat, for every carrier kind, plus the gates at the plan's
//! own entry point.
//!
//! The seat assembles each attempt with the real Anthropic egress, which
//! emits the client's carried members on a same-dialect request, so the
//! settlement reads a body that carries the client member at the path the
//! rejection names.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, ContentPart, CustomTool, Error,
    KnownContentPart, Message, MessageContent, Provider, Result, Role, SystemBlock, SystemContent,
    TokenCount, ToolDef,
};
use serde_json::{Map, Value, json};

use crate::config::{AliasValue, Config};
use crate::resolved::ResolvedModel;
use crate::router::Router;

use super::field_repair_tests::success_response;

const ALIAS: &str = "member-repair-alias";
const CALLER_PATH: &str = "messages.content.caller";
const REFUSED_CALLER: &str = "messages.1.content.0.caller: Extra inputs are not permitted";

/// The client's caller member on the assistant text part.
fn caller() -> Value {
    json!({"type": "direct", "inner": 1})
}

/// The client body: an assistant text part carrying `caller` and
/// `estimated_tokens`, neither of which the canonical part models.
fn client_body(part_caller: Option<Value>) -> Value {
    let mut part = json!({"type": "text", "text": "earlier", "estimated_tokens": 12});
    if let Some(value) = part_caller {
        part["caller"] = value;
    }
    json!({
        "model": ALIAS,
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "hello"}]},
            {"role": "assistant", "content": [part]},
            {"role": "user", "content": [{"type": "text", "text": "again"}]},
        ],
    })
}

fn text_part(text: &str, extras: Map<String, Value>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
        extras,
    })
}

fn message(role: Role, part: ContentPart) -> Message {
    Message {
        role,
        content: MessageContent::Parts(vec![part]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
        extras: Map::new(),
    }
}

/// The canonical request the Anthropic ingress parses `client` into, with
/// `carried` on the assistant part's carrier.
fn member_req(carried: Map<String, Value>, client: &Value) -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        max_tokens: Some(1024),
        messages: vec![
            message(Role::User, text_part("hello", Map::new())),
            message(Role::Assistant, text_part("earlier", carried)),
            message(Role::User, text_part("again", Map::new())),
        ]
        .into(),
        ..Default::default()
    };
    req.routectl_internal.provenance = routectl_core::RequestProvenance::AnthropicIngress;
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(client).expect("serialize").into(),
    });
    req
}

fn carried(with_caller: bool) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("estimated_tokens".into(), json!(12));
    if with_caller {
        map.insert("caller".into(), caller());
    }
    map
}

/// A client that sent both members, parsed as the ingress parses it.
fn caller_req() -> ChatRequest {
    member_req(carried(true), &client_body(Some(caller())))
}

fn rejection(message: &str) -> Error {
    Error::upstream(
        "member-seat",
        400,
        json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message},
        })
        .to_string(),
    )
}

/// The wire pointer of the client's `caller` member on the assistant part.
const CALLER_POINTER: &str = "/messages/1/content/0/caller";

/// A seat refusing with `message` every body that carries a member at
/// `pointer`, and serving every other. Every walk's body is the real
/// Anthropic egress's; a count body is the messages body, which carries
/// every node a count body does.
struct MemberSeat {
    message: &'static str,
    pointer: &'static str,
    bodies: Mutex<Vec<Value>>,
}

impl MemberSeat {
    fn new(message: &'static str) -> Arc<Self> {
        Self::refusing_at(message, CALLER_POINTER)
    }

    fn refusing_at(message: &'static str, pointer: &'static str) -> Arc<Self> {
        Arc::new(Self {
            message,
            pointer,
            bodies: Mutex::new(Vec::new()),
        })
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().clone()
    }

    fn answer(&self, req: &ChatRequest) -> Result<()> {
        let egress = routectl_providers::anthropic_api::AnthropicApiProvider::new(
            routectl_providers::anthropic_api::AnthropicApiConfig::new("member-seat", "k"),
        );
        let mut body = egress.normalize_request(req)?;
        if let Some(object) = body.as_object_mut() {
            object.remove("stream");
        }
        if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
            tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
        }
        let refused = body.pointer(self.pointer).is_some();
        self.bodies.lock().push(body);
        if refused {
            return Err(rejection(self.message));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for MemberSeat {
    fn id(&self) -> &'static str {
        "member-seat"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("member-seat", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer(&req).map(|()| success_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.answer(&req).map(|()| {
            futures::stream::iter(vec![Ok(content_chunk())]).boxed()
                as BoxStream<'static, Result<ChatChunk>>
        })
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.answer(&req).map(|()| TokenCount {
            input_tokens: 7,
            ..Default::default()
        })
    }
}

/// A content-bearing chunk: the stream walk commits on first content.
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

fn member_router(seat: &Arc<MemberSeat>) -> Router {
    let toml_text = "[wire_check]\nfirst_n = 8\nsample_one_in = 1\n\
         [providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";
    let mut config: Config = toml::from_str(toml_text).expect("valid test toml");
    config.retry.max_attempts = 1;
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        "m0".to_string(),
        Arc::new(ResolvedModel::new(
            "m0",
            "p0",
            Arc::clone(seat) as Arc<dyn Provider>,
            "upstream",
        )),
    )]));
    router.with_capability_writes_assumed_durable_for_tests()
}

/// Builds one row's client request.
type RequestFn = fn() -> ChatRequest;

fn caller_key() -> String {
    crate::field_capability::field_capability_key(CALLER_PATH).expect("a qualified path")
}

fn learned_keys(meta: &crate::router::DispatchMeta) -> Vec<String> {
    meta.learned_capabilities
        .iter()
        .map(|event| event.capability_key.clone())
        .collect()
}

fn sent_part(body: &Value) -> &Value {
    &body["messages"][1]["content"][0]
}

/// Mutation check: return `None` from `CarriedMemberProposer::propose` ->
/// red (the 400 reaches the client).
#[tokio::test]
async fn a_refused_carried_member_retries_without_it_and_settles_that_member_only() {
    let seat = MemberSeat::new(REFUSED_CALLER);
    let router = member_router(&seat);

    let dispatched =
        Box::pin(router.complete_with_options(caller_req(), crate::router::RouterOptions::new()))
            .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 2, "one refused attempt, one retry");
    assert_eq!(sent_part(&bodies[0])["caller"], caller(), "premise: sent");
    assert!(
        sent_part(&bodies[1]).get("caller").is_none(),
        "{}",
        bodies[1]
    );
    assert_eq!(learned_keys(&dispatched.meta), vec![caller_key()]);
    let repair = &dispatched.meta.field_repairs[0];
    assert_eq!(repair.field_path, super::FIELD_PATH_CARRIED_MEMBER);
    assert_eq!(repair.member_path.as_deref(), Some(CALLER_PATH));
    assert!(repair.repair_succeeded && repair.learned);
}

/// Mutation check: make the removal clear the whole carrier -> red.
#[tokio::test]
async fn a_sibling_member_on_the_same_part_stays_on_the_retried_wire() {
    let seat = MemberSeat::new(REFUSED_CALLER);
    let router = member_router(&seat);

    let dispatched =
        Box::pin(router.complete_with_options(caller_req(), crate::router::RouterOptions::new()))
            .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    let retried = &seat.bodies()[1];
    assert_eq!(
        sent_part(retried)["estimated_tokens"],
        json!(12),
        "{retried}"
    );
    assert_eq!(sent_part(retried)["text"], json!("earlier"));
}

/// One carrier kind's row: the canonical request the ingress parses a client
/// body carrying a member at `pointer` into, and the grounded refusal of that
/// member.
struct CarrierCase {
    name: &'static str,
    req: ChatRequest,
    refusal: &'static str,
    pointer: &'static str,
    member_path: &'static str,
}

fn one_member(key: &str, value: Value) -> Map<String, Value> {
    Map::from_iter([(key.to_string(), value)])
}

/// The canonical request the Anthropic ingress parses `client` into: three
/// plain turns, then `shape` applies the carrier under test.
fn carried_req(client: &Value, shape: impl FnOnce(&mut ChatRequest)) -> ChatRequest {
    let mut req = member_req(Map::new(), client);
    shape(&mut req);
    req
}

/// The client body with three plain turns, then `shape` applies the member
/// under test.
fn plain_client(shape: impl FnOnce(&mut Value)) -> Value {
    let mut client = json!({
        "model": ALIAS,
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "hello"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "earlier"}]},
            {"role": "user", "content": [{"type": "text", "text": "again"}]},
        ],
    });
    shape(&mut client);
    client
}

fn top_level_case() -> CarrierCase {
    let client = plain_client(|c| c["zz_knob"] = json!({"level": 2}));
    let req = carried_req(&client, |r| {
        r.provider_extras = Some(json!({"zz_knob": {"level": 2}}));
    });
    CarrierCase {
        name: "top-level provider_extras",
        req,
        refusal: "zz_knob: Extra inputs are not permitted",
        pointer: "/zz_knob",
        member_path: "zz_knob",
    }
}

fn message_case() -> CarrierCase {
    let client = plain_client(|c| c["messages"][1]["zz_note"] = json!("m"));
    let req = carried_req(&client, |r| {
        Arc::make_mut(&mut r.messages)[1].extras = one_member("zz_note", json!("m"));
    });
    CarrierCase {
        name: "message",
        req,
        refusal: "messages.1.zz_note: Extra inputs are not permitted",
        pointer: "/messages/1/zz_note",
        member_path: "messages.zz_note",
    }
}

fn part_case() -> CarrierCase {
    let client = client_body(Some(caller()));
    CarrierCase {
        name: "content part",
        req: member_req(carried(true), &client),
        refusal: REFUSED_CALLER,
        pointer: CALLER_POINTER,
        member_path: CALLER_PATH,
    }
}

fn custom_tool_case() -> CarrierCase {
    let schema = json!({"type": "object", "properties": {}});
    let client = plain_client(|c| {
        c["tools"] = json!([{"name": "t", "input_schema": schema, "zz_tool_note": 1}]);
    });
    let req = carried_req(&client, |r| {
        r.tools = Some(vec![ToolDef::Custom(CustomTool {
            name: "t".into(),
            description: None,
            input_schema: schema.clone(),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
            extras: one_member("zz_tool_note", json!(1)),
        })]);
    });
    CarrierCase {
        name: "custom tool",
        req,
        refusal: "tools.0.zz_tool_note: Extra inputs are not permitted",
        pointer: "/tools/0/zz_tool_note",
        member_path: "tools.zz_tool_note",
    }
}

fn system_block_case() -> CarrierCase {
    let client = plain_client(|c| {
        c["system"] = json!([{"type": "text", "text": "be brief", "zz_block_note": 1}]);
    });
    let req = carried_req(&client, |r| {
        r.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "be brief".into(),
            cache_control: None,
            citations: None,
            extras: one_member("zz_block_note", json!(1)),
        }]));
    });
    CarrierCase {
        name: "system block",
        req,
        refusal: "system.0.zz_block_note: Extra inputs are not permitted",
        pointer: "/system/0/zz_block_note",
        member_path: "system.zz_block_note",
    }
}

/// Every carrier kind the arm reaches, through the complete walk: the
/// refused member is gone from the retried wire and only its verdict is
/// learned.
///
/// Mutation check: drop one kind from `Carrier::ALL` -> red on that row.
#[tokio::test]
async fn a_refused_member_is_removed_from_each_carrier_kind() {
    for case in [
        top_level_case(),
        message_case(),
        part_case(),
        custom_tool_case(),
        system_block_case(),
    ] {
        let name = case.name;
        let seat = MemberSeat::refusing_at(case.refusal, case.pointer);
        let router = member_router(&seat);

        let dispatched =
            Box::pin(router.complete_with_options(case.req, crate::router::RouterOptions::new()))
                .await;

        assert!(
            dispatched.result.is_ok(),
            "{name}: {:?}",
            dispatched.result.err()
        );
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 2, "{name}: one refused attempt, one retry");
        assert!(
            bodies[0].pointer(case.pointer).is_some(),
            "{name}: premise: sent"
        );
        assert!(
            bodies[1].pointer(case.pointer).is_none(),
            "{name}: {}",
            bodies[1]
        );
        let key = crate::field_capability::field_capability_key(case.member_path).expect(name);
        assert_eq!(learned_keys(&dispatched.meta), vec![key], "{name}");
        assert_eq!(
            dispatched.meta.field_repairs[0].member_path.as_deref(),
            Some(case.member_path),
            "{name}",
        );
    }
}

/// A canonical field is removed from every carrier at its position: both
/// parts carrying `caller` lose it on the retry, though the rejection named
/// one, and the sibling member on the first part stays.
///
/// Mutation check: stop the removal after the first matching carrier in
/// `remove_member` -> red (the second instance survives).
#[tokio::test]
async fn every_matching_instance_goes_and_a_sibling_stays() {
    let part = |text: &str, extra: Option<(&str, Value)>| {
        let mut wire = json!({"type": "text", "text": text, "caller": caller()});
        let mut extras = one_member("caller", caller());
        if let Some((key, value)) = extra {
            wire[key] = value.clone();
            extras.insert(key.to_string(), value);
        }
        (wire, text_part(text, extras))
    };
    let (first_wire, first) = part("earlier", Some(("estimated_tokens", json!(12))));
    let (second_wire, second) = part("later", None);
    let client = plain_client(|c| c["messages"][1]["content"] = json!([first_wire, second_wire]));
    let req = carried_req(&client, |r| {
        Arc::make_mut(&mut r.messages)[1].content = MessageContent::Parts(vec![first, second]);
    });
    let seat = MemberSeat::new(REFUSED_CALLER);
    let router = member_router(&seat);

    let dispatched =
        Box::pin(router.complete_with_options(req, crate::router::RouterOptions::new())).await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 2);
    let sent = &bodies[0]["messages"][1]["content"];
    assert!(
        sent[0].get("caller").is_some() && sent[1].get("caller").is_some(),
        "premise"
    );
    let retried = &bodies[1]["messages"][1]["content"];
    assert!(retried[0].get("caller").is_none(), "{retried}");
    assert!(retried[1].get("caller").is_none(), "{retried}");
    assert_eq!(retried[0]["estimated_tokens"], json!(12), "{retried}");
    assert_eq!(learned_keys(&dispatched.meta), vec![caller_key()]);
}

/// The same refusal on a stream answered before any content repairs and
/// settles exactly as the complete walk does.
///
/// Mutation check: return `None` from `CarriedMemberProposer::propose` ->
/// red.
#[tokio::test]
async fn a_pre_content_stream_rejection_removes_the_member_and_settles_it() {
    let seat = MemberSeat::new(REFUSED_CALLER);
    let router = member_router(&seat);

    let dispatched =
        Box::pin(router.stream_with_options(caller_req(), crate::router::RouterOptions::new()))
            .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 2, "one refused attempt, one retry");
    assert!(
        sent_part(&bodies[1]).get("caller").is_none(),
        "{}",
        bodies[1]
    );
    assert_eq!(sent_part(&bodies[1])["estimated_tokens"], json!(12));
    assert_eq!(learned_keys(&dispatched.meta), vec![caller_key()]);
}

/// A token count the upstream refuses for the member is re-counted without
/// it. Field verdicts carry no surface, and a settling count commits a
/// repaired step the way the messages walks do (as it does for a closed-table
/// row); `count_tokens_validates` decides only which verdicts a free count
/// may re-check, so it plays no part here. A non-settling count repairs for
/// its answer and learns nothing.
///
/// Mutation check: return `None` from `CarriedMemberProposer::propose` ->
/// red on both rows.
#[tokio::test]
async fn a_refused_member_is_removed_from_the_token_count() {
    let settling_seat = MemberSeat::new(REFUSED_CALLER);
    let settling_router = member_router(&settling_seat);

    let counted = settling_router.count_tokens_with_meta(caller_req()).await;

    assert!(counted.result.is_ok(), "{:?}", counted.result.err());
    let bodies = settling_seat.bodies();
    assert_eq!(bodies.len(), 2, "settling: one refused count, one re-count");
    assert!(
        sent_part(&bodies[1]).get("caller").is_none(),
        "{}",
        bodies[1]
    );
    assert_eq!(learned_keys(&counted.meta), vec![caller_key()]);

    let seat = MemberSeat::new(REFUSED_CALLER);
    let router = member_router(&seat);

    let answered = router.count_tokens(caller_req()).await;

    assert!(answered.is_ok(), "{:?}", answered.err());
    let bodies = seat.bodies();
    assert_eq!(
        bodies.len(),
        2,
        "non-settling: one refused count, one re-count"
    );
    assert!(
        sent_part(&bodies[1]).get("caller").is_none(),
        "{}",
        bodies[1]
    );
    assert!(router.learned_capability_snapshot().is_empty());
    assert_eq!(router.field_verdicts().snapshot_len(), 0);
}

/// Each refusal the arm must not answer: the client gets the upstream 400 and
/// nothing is removed or learned. The positive control above shows the same
/// seat and request do repair a grounded refusal of the member.
#[tokio::test]
async fn a_refusal_the_arm_cannot_attribute_mints_nothing_and_reaches_the_client() {
    let rows: [(&str, &'static str, RequestFn); 6] = [
        (
            "a typed part field",
            "messages.1.content.0.text: Extra inputs are not permitted",
            caller_req,
        ),
        (
            "an inferred prose reason",
            "messages.1.content.0.caller: Field not recognized here",
            caller_req,
        ),
        (
            "a refused value",
            "messages.1.content.0.caller: Input should be 'a' or 'b'",
            caller_req,
        ),
        (
            "an ungrounded quoted refusal",
            "\"messages.1.content.0.caller\" is not supported",
            caller_req,
        ),
        ("a member routectl added", REFUSED_CALLER, || {
            member_req(carried(true), &client_body(None))
        }),
        ("a member routectl changed", REFUSED_CALLER, || {
            member_req(carried(true), &client_body(Some(json!("other"))))
        }),
    ];
    for (name, message, req) in rows {
        let seat = MemberSeat::new(message);
        let router = member_router(&seat);

        let dispatched =
            Box::pin(router.complete_with_options(req(), crate::router::RouterOptions::new()))
                .await;

        let err = dispatched.result.expect_err(name);
        assert!(
            matches!(err, Error::Upstream { status: 400, .. }),
            "{name}: {err:?}"
        );
        assert_eq!(seat.bodies().len(), 1, "{name}: no retry");
        assert!(dispatched.meta.field_repairs.is_empty(), "{name}");
        assert!(learned_keys(&dispatched.meta).is_empty(), "{name}");
    }
}

/// The exact-path rule at the carrier surface, through the plan's own entry
/// point: only a path naming exactly the carried member removes it.
///
/// Mutation check: compare the member path with `starts_with` (either
/// direction) in `names_member` -> red on the parent or the child row.
#[test]
fn only_the_exact_member_path_removes_a_carried_member() {
    use super::super::rejection_parser::RawPath;
    use super::FieldRepairPlan;
    use routectl_core::capability::SignalTier;
    use routectl_core::wire_check::IngressDialect;

    let rows: [(&str, &[&str], bool); 6] = [
        ("exact member", &["messages", "content", "caller"], true),
        ("parent", &["messages", "content"], false),
        ("child", &["messages", "content", "caller", "inner"], false),
        ("typed field", &["messages", "content", "text"], false),
        ("other position", &["messages", "caller"], false),
        ("top level", &["caller"], false),
    ];
    for (name, segments, removes) in rows {
        let mut req = caller_req();
        let mut plan = FieldRepairPlan::admitted(Vec::new(), Some(IngressDialect::Anthropic), None);
        let mut meta = crate::router::DispatchMeta::for_alias(ALIAS);
        let mut budget = super::super::repair_budget::RepairBudget::per_request();
        let path = RawPath::for_tests(segments);

        let applied = plan.apply_member(
            &path,
            SignalTier::SelfIdentifying,
            &mut req,
            &mut meta,
            &mut budget,
            400,
        );

        let MessageContent::Parts(parts) = &req.messages[1].content else {
            panic!("{name}: parts");
        };
        let ContentPart::Known(KnownContentPart::Text { extras, .. }) = &parts[0] else {
            panic!("{name}: text part");
        };
        assert_eq!(applied.is_some(), removes, "{name}");
        assert_eq!(extras.contains_key("caller"), !removes, "{name}");
        assert!(
            extras.contains_key("estimated_tokens"),
            "{name}: sibling kept"
        );
    }
}

/// A plan on a translation lane has no client dialect on the wire, so the
/// arm removes nothing there.
#[test]
fn a_translation_lane_plan_removes_no_member() {
    use super::super::rejection_parser::RawPath;
    use super::FieldRepairPlan;
    use routectl_core::capability::SignalTier;

    let mut req = caller_req();
    let mut plan = FieldRepairPlan::admitted(Vec::new(), None, None);
    let mut meta = crate::router::DispatchMeta::for_alias(ALIAS);
    let mut budget = super::super::repair_budget::RepairBudget::per_request();

    let applied = plan.apply_member(
        &RawPath::for_tests(&["messages", "content", "caller"]),
        SignalTier::SelfIdentifying,
        &mut req,
        &mut meta,
        &mut budget,
        400,
    );

    assert!(applied.is_none());
    assert_eq!(
        budget.remaining(),
        super::super::repair_budget::REPAIRS_PER_REQUEST
    );
}

/// A request whose shared repair allowance is spent removes no member and
/// charges nothing, though the path names exactly a carried member.
///
/// Mutation check: drop the `check_or_mark_refusal` check in
/// `apply_member` -> red (the member is removed).
#[test]
fn a_spent_repair_allowance_removes_no_member_and_charges_nothing() {
    use super::super::rejection_parser::RawPath;
    use super::super::repair_budget::{REPAIRS_PER_REQUEST, RepairBudget};
    use super::FieldRepairPlan;
    use routectl_core::capability::SignalTier;
    use routectl_core::wire_check::IngressDialect;

    let mut req = caller_req();
    let mut plan = FieldRepairPlan::admitted(Vec::new(), Some(IngressDialect::Anthropic), None);
    let mut meta = crate::router::DispatchMeta::for_alias(ALIAS);
    let mut budget = RepairBudget::per_request();
    while budget.draw() {}
    assert_eq!(budget.spent(), REPAIRS_PER_REQUEST, "premise: spent");

    let applied = plan.apply_member(
        &RawPath::for_tests(&["messages", "content", "caller"]),
        SignalTier::SelfIdentifying,
        &mut req,
        &mut meta,
        &mut budget,
        400,
    );

    assert!(applied.is_none());
    let MessageContent::Parts(parts) = &req.messages[1].content else {
        panic!("parts");
    };
    let ContentPart::Known(KnownContentPart::Text { extras, .. }) = &parts[0] else {
        panic!("text part");
    };
    assert!(extras.contains_key("caller"), "nothing removed");
    assert_eq!(budget.spent(), REPAIRS_PER_REQUEST, "nothing charged");
    assert!(!plan.has_applied());
}

/// A seat that is never called, for tests that only need a target.
pub(in crate::router) struct NoopSeat;

#[async_trait::async_trait]
impl Provider for NoopSeat {
    fn id(&self) -> &'static str {
        "noop-seat"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("noop-seat", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(success_response())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("noop-seat", "unused"))
    }
}
