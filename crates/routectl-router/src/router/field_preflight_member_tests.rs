//! A learned drop of a carried client member applied on the NEXT request:
//! the generic arm settles `field:<path>` on a repaired success, the durable
//! write is acknowledged as the daemon does, and every later request on that
//! lane goes out without the member, before any upstream sees it.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite};
use serde_json::json;

use crate::config::{AliasValue, Config};
use crate::resolved::ResolvedModel;
use crate::router::field_repair::{
    FIELD_PATH_CARRIED_MEMBER, FieldRepairTable, TransformClass,
    carried_member_path_of_capability_key, transform_class_of_capability_key,
    value_slot_for_capability_key,
};
use crate::router::{Router, RouterOptions};
use routectl_core::Provider;

use super::{
    ALIAS, CALLER_PATH, MemberSeat, REFUSED_CALLER, caller, caller_key, caller_req, learned_keys,
    member_router, sent_part,
};

/// The pattern a removal of `caller` from every content part is declared under.
const CALLER_PATTERN: &str = "/messages/*/content/*/caller";

/// A pointer no request in this file carries: a seat refusing at it serves all.
const NOWHERE: &str = "/routectl_never_sent";

/// Serve one request that repairs and settles `caller`, then acknowledge the
/// durable write the way the daemon does, so the verdict is pre-flight eligible.
async fn learn_and_acknowledge(router: &Router) {
    let first = Box::pin(router.complete_with_options(caller_req(), RouterOptions::new())).await;
    assert!(first.result.is_ok(), "premise: {:?}", first.result.err());
    assert_eq!(learned_keys(&first.meta), vec![caller_key()], "premise");
    let event = &first.meta.learned_capabilities[0];
    assert!(
        router.acknowledge_durable_field_confirmation(
            &event.state_key,
            &event.capability_key,
            &event.provider_kind,
            event.persistence_generation,
            event.incarnation,
            event.observations,
        ),
        "premise: the durable write acknowledges the confirmation",
    );
}

fn caller_removal() -> DeclaredEdit {
    DeclaredEdit::new(EditSite::Preflight, CALLER_PATTERN, EditKind::Remove).expect("a pattern")
}

/// The pre-flight record a request planned for the carried `caller` member.
fn member_record(meta: &crate::router::DispatchMeta) -> &crate::router::FieldPreflight {
    meta.field_preflight
        .iter()
        .find(|record| record.member_path.as_deref() == Some(CALLER_PATH))
        .expect("the planner decided the carried member")
}

/// After a settled `field:messages.content.caller`, the next request goes out
/// once, without `caller` and with its sibling, on both messages walks, and the
/// removal is declared.
///
/// Mutation check: make `carried_member_path_of_capability_key` return `None`
/// -> red (the next request ships `caller`, draws the refusal, and the acting
/// verdict refuses a reactive repair, so the client gets the 400).
#[tokio::test]
async fn the_request_after_a_settled_member_drop_goes_out_without_it() {
    for stream in [false, true] {
        let seat = MemberSeat::new(REFUSED_CALLER);
        let router = member_router(&seat);
        learn_and_acknowledge(&router).await;

        let (served, meta) = dispatch(&router, stream).await;

        assert!(served.is_ok(), "stream={stream}: {:?}", served.err());
        let bodies = seat.bodies();
        assert_eq!(
            bodies.len(),
            3,
            "stream={stream}: the next request is sent once"
        );
        let sent = sent_part(&bodies[2]);
        assert!(sent.get("caller").is_none(), "stream={stream}: {sent}");
        assert_eq!(
            sent["estimated_tokens"],
            json!(12),
            "stream={stream}: sibling kept"
        );
        assert_eq!(sent["text"], json!("earlier"), "stream={stream}");
        assert!(
            seat.edits()[2].contains(&caller_removal()),
            "stream={stream}: the removal is declared: {:?}",
            seat.edits()[2],
        );
        assert!(
            meta.field_repairs.is_empty(),
            "stream={stream}: no reactive repair"
        );
        let record = member_record(&meta);
        assert!(record.acted, "stream={stream}");
        assert_eq!(record.field_path, Some(FIELD_PATH_CARRIED_MEMBER));
        assert_eq!(
            record.transform_class,
            Some(TransformClass::Envelope.as_str())
        );
        assert!(record.authorization.is_some(), "stream={stream}");
    }
}

/// The token count after a settled member drop counts a body without the
/// member, on both count entry points, and the verdict stays one no free count
/// re-checks.
///
/// Mutation check: make `carried_member_path_of_capability_key` return `None`
/// -> red (the count ships `caller` and draws the refusal unrepaired).
#[tokio::test]
async fn the_token_count_after_a_settled_member_drop_counts_without_it() {
    let seat = MemberSeat::new(REFUSED_CALLER);
    let router = member_router(&seat);
    learn_and_acknowledge(&router).await;

    let counted = router.count_tokens_with_meta(caller_req()).await;
    let answered = router.count_tokens(caller_req()).await;

    assert!(counted.result.is_ok(), "{:?}", counted.result.err());
    assert!(answered.is_ok(), "{:?}", answered.err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 4, "each count is sent once");
    for (name, body) in [("with meta", &bodies[2]), ("result only", &bodies[3])] {
        let sent = sent_part(body);
        assert!(sent.get("caller").is_none(), "{name}: {sent}");
        assert_eq!(sent["estimated_tokens"], json!(12), "{name}");
    }
    assert!(member_record(&counted.meta).acted);
    let event_lane = router
        .learned_capability_snapshot()
        .into_iter()
        .find(|entry| entry.feature_key == caller_key())
        .expect("the verdict is resident")
        .state_key;
    assert!(
        !super::super::negative_recovers_only_by_recheck(
            router.field_repair_table(),
            &event_lane,
            &caller_key(),
            "anthropic-api",
        ),
        "a member verdict keeps count_tokens_validates false on a counting seat",
    );
}

/// Two anthropic-api seats on distinct provider entries, each behind its own
/// alias, so each is its own learned lane.
fn two_lane_router(learning: &Arc<MemberSeat>, other: &Arc<MemberSeat>) -> Router {
    let toml_text = "[providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n\
         [providers.p1]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";
    let mut config: Config = toml::from_str(toml_text).expect("valid test toml");
    config.retry.max_attempts = 1;
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    config.aliases.insert(
        OTHER_ALIAS.to_string(),
        AliasValue::Chain(vec!["m1".to_string()]),
    );
    let mut router = Router::new(Arc::new(config));
    let seat = |nickname: &str, provider: &str, seat: &Arc<MemberSeat>| {
        (
            nickname.to_string(),
            Arc::new(ResolvedModel::new(
                nickname,
                provider,
                Arc::clone(seat) as Arc<dyn Provider>,
                "upstream",
            )),
        )
    };
    router.install_resolved_models(BTreeMap::from([
        seat("m0", "p0", learning),
        seat("m1", "p1", other),
    ]));
    router.with_capability_writes_assumed_durable_for_tests()
}

const OTHER_ALIAS: &str = "member-other-lane-alias";

fn caller_req_on(alias: &str) -> routectl_core::ChatRequest {
    let mut req = caller_req();
    req.model = alias.into();
    req
}

/// A member verdict learned on one lane drops nothing on another; the
/// learning lane's own next request is the positive control.
///
/// Mutation check: key the planner's member identity on a fixed lane instead
/// of `target.learned_lane` -> red on one of the two rows.
#[tokio::test]
async fn a_member_verdict_on_another_lane_does_not_apply() {
    let learning = MemberSeat::new(REFUSED_CALLER);
    let other = MemberSeat::refusing_at(REFUSED_CALLER, NOWHERE);
    let router = two_lane_router(&learning, &other);
    learn_and_acknowledge(&router).await;

    let elsewhere =
        Box::pin(router.complete_with_options(caller_req_on(OTHER_ALIAS), RouterOptions::new()))
            .await;
    let here =
        Box::pin(router.complete_with_options(caller_req_on(ALIAS), RouterOptions::new())).await;

    assert!(elsewhere.result.is_ok(), "{:?}", elsewhere.result.err());
    let sent = sent_part(&other.bodies()[0]).clone();
    assert_eq!(
        sent["caller"],
        caller(),
        "the other lane forwards it: {sent}"
    );
    assert!(
        !other.edits()[0].contains(&caller_removal()),
        "and declares no removal"
    );
    assert!(
        elsewhere
            .meta
            .field_preflight
            .iter()
            .all(|record| record.member_path.is_none()),
        "no member decision on a lane with no verdict",
    );
    assert!(here.result.is_ok(), "{:?}", here.result.err());
    let bodies = learning.bodies();
    assert_eq!(bodies.len(), 3, "control: the learning lane sends once");
    assert!(sent_part(&bodies[2]).get("caller").is_none(), "control");
}

/// How a persisted `field:` key resolves: a carried-member path to the generic
/// drop, everything else to its closed row or to nothing.
#[test]
fn a_persisted_field_key_resolves_to_a_closed_row_first_then_to_a_member_drop() {
    let table = FieldRepairTable::default();
    let key = |path: &str| crate::field_capability::field_capability_key(path).expect(path);
    let rewrite = super::super::TEST_REWRITE_PATH;
    let rows: [(&str, String, Option<&str>, Option<TransformClass>); 9] = [
        (
            "part member",
            key(CALLER_PATH),
            Some(CALLER_PATH),
            Some(TransformClass::Envelope),
        ),
        (
            "message member",
            key("messages.zz_note"),
            Some("messages.zz_note"),
            Some(TransformClass::Envelope),
        ),
        (
            "tool member",
            key("tools.zz_tool_note"),
            Some("tools.zz_tool_note"),
            Some(TransformClass::Envelope),
        ),
        (
            "system member",
            key("system.zz_block_note"),
            Some("system.zz_block_note"),
            Some(TransformClass::Envelope),
        ),
        (
            "top-level member",
            key("zz_knob"),
            Some("zz_knob"),
            Some(TransformClass::Envelope),
        ),
        (
            "closed row at a carrier position",
            key(rewrite),
            None,
            Some(TransformClass::Envelope),
        ),
        (
            "capability-strip key",
            key("context_management"),
            None,
            None,
        ),
        ("canonical request key", key("model"), None, None),
        (
            "not a carrier position",
            key("some.future.envelope.field"),
            None,
            None,
        ),
    ];
    for (name, capability_key, path, class) in rows {
        assert_eq!(
            carried_member_path_of_capability_key(table, &capability_key),
            path,
            "{name}"
        );
        assert_eq!(
            transform_class_of_capability_key(table, &capability_key),
            class,
            "{name}"
        );
    }
    assert!(
        value_slot_for_capability_key(table, &key(rewrite)).is_some(),
        "premise: the colliding key keeps its closed row's value slot",
    );
    assert_eq!(
        value_slot_for_capability_key(table, &key(CALLER_PATH)),
        None,
        "a member drop carries no value slot",
    );
}

/// The status surface renders a settled member verdict with the envelope
/// class and its quorum, unblocked once acknowledged.
///
/// Mutation check: make `transform_class_of_capability_key` ignore the
/// member resolution -> red (no class, and the row reads not eligible).
#[tokio::test]
async fn the_status_surface_renders_a_member_verdict_as_envelope() {
    let seat = MemberSeat::new(REFUSED_CALLER);
    let router = member_router(&seat);
    learn_and_acknowledge(&router).await;

    let rows = router.field_verdict_status();

    let row = rows
        .iter()
        .find(|row| row.capability_key == caller_key())
        .expect("the member verdict has a row");
    assert_eq!(row.transform_class, Some(TransformClass::Envelope.as_str()));
    assert_eq!(row.required_quorum, Some(crate::config::ENVELOPE_QUORUM));
    assert!(!row.prefix_impacting);
    assert_eq!(row.blocked_reason, None, "{row:?}");
}

/// One request through the complete or the stream walk: whether it was
/// served, and its metadata.
async fn dispatch(
    router: &Router,
    stream: bool,
) -> (routectl_core::Result<()>, crate::router::DispatchMeta) {
    if stream {
        let dispatched =
            Box::pin(router.stream_with_options(caller_req(), RouterOptions::new())).await;
        (dispatched.result.map(drop), dispatched.meta)
    } else {
        let dispatched =
            Box::pin(router.complete_with_options(caller_req(), RouterOptions::new())).await;
        (dispatched.result.map(drop), dispatched.meta)
    }
}
