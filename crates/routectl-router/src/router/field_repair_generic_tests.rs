//! The generic carried-member arm of the reactive field repair, end to end
//! through the complete walk on an anthropic-api seat, plus the exact-path
//! rule at the carrier surface.
//!
//! The seat assembles each attempt with the real Anthropic egress, then
//! merges the canonical part carriers into the body the way a same-dialect
//! egress emits them, so the settlement reads a body that carries the client
//! member at the path the rejection names.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::BoxStream;
use parking_lot::Mutex;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ContentPart, Error, KnownContentPart, Message,
    MessageContent, Provider, Result, Role,
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

/// A seat refusing every body whose assistant part carries `caller` with
/// `message`, and serving every other.
struct MemberSeat {
    message: &'static str,
    bodies: Mutex<Vec<Value>>,
}

impl MemberSeat {
    fn new(message: &'static str) -> Arc<Self> {
        Arc::new(Self {
            message,
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
        merge_part_carriers(req, &mut body);
        if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
            tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
        }
        let refused = body["messages"][1]["content"][0].get("caller").is_some();
        self.bodies.lock().push(body);
        if refused {
            return Err(rejection(self.message));
        }
        Ok(())
    }
}

/// Each typed part's carried members, added where the part sits in `body`,
/// typed members winning.
fn merge_part_carriers(req: &ChatRequest, body: &mut Value) {
    for (m, message) in req.messages.iter().enumerate() {
        let MessageContent::Parts(parts) = &message.content else {
            continue;
        };
        for (p, part) in parts.iter().enumerate() {
            let ContentPart::Known(KnownContentPart::Text { extras, .. }) = part else {
                continue;
            };
            if let Some(slot) = body["messages"][m]["content"][p].as_object_mut() {
                for (key, value) in extras {
                    slot.entry(key.clone()).or_insert_with(|| value.clone());
                }
            }
        }
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
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("member-seat", "unused"))
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
