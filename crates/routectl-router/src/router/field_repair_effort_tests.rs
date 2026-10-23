//! The effort rows: welded to the effort rank, read and rewritten through
//! their two carriers, scoped off the kinds with no grounded effort refusal,
//! and absent while the target clamps the effort itself.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use routectl_core::{ChatRequest, ReasoningConfig};
use routectl_providers::effort::VALID_EFFORT_TOKENS;
use serde_json::json;

use super::super::class_observe::DispatchSurface;
use super::super::probe_test_support::{OkProvider, remote_router_without_negative};
use super::super::{DispatchTarget, Router};
use super::{
    FIELD_REPAIRS, FieldRepairTable, FieldSettlementMode, FieldSurface, FieldTransform,
    TransformClass, grounded_field_feature_keys, present_rows,
};
use crate::field_verdict_value::ValueSlot;

const EFFORT_FIELD: &str = "output_config.effort";

/// The grounded rows whose transform is a ranked rewrite, in table order.
fn ranked_rows() -> Vec<&'static super::FieldRepairRow> {
    FIELD_REPAIRS
        .iter()
        .filter(|row| matches!(row.transform, FieldTransform::RankedRewrite { .. }))
        .collect()
}

/// One row per member of the effort rank, in rank order, each spelled from
/// its level and decoded against the rank itself.
///
/// Mutation checks: delete one `effort_row!` invocation, or swap two members
/// of `VALID_EFFORT_TOKENS` -> red.
#[test]
fn the_effort_rows_follow_the_effort_rank_member_for_member() {
    let rows = ranked_rows();

    let levels: Vec<&str> = rows
        .iter()
        .map(|row| match row.surface {
            FieldSurface::Effort(level) => level,
            other => panic!("{}: a ranked row on {other:?}", row.path),
        })
        .collect();

    assert_eq!(
        levels, VALID_EFFORT_TOKENS,
        "one row per level, in rank order"
    );
    for row in rows {
        let FieldSurface::Effort(level) = row.surface else {
            unreachable!("checked above");
        };
        assert_eq!(row.path, format!("{EFFORT_FIELD}.{level}"));
        assert_eq!(
            row.transform,
            FieldTransform::RankedRewrite {
                rank: &VALID_EFFORT_TOKENS,
            },
        );
        assert_eq!(row.class, TransformClass::Envelope, "{}", row.path);
        assert!(!row.count_tokens_validates, "{}", row.path);
    }
}

/// Every effort key's persisted value decodes against the rank, so no level
/// ever learned is stranded by a key lookup.
#[test]
fn every_effort_key_reads_its_value_against_the_rank() {
    for level in VALID_EFFORT_TOKENS {
        let key = crate::field_capability::field_capability_key(&format!("{EFFORT_FIELD}.{level}"))
            .expect("a qualified path");

        let slot = super::value_slot_for_capability_key(FieldRepairTable::grounded_only(), &key);

        assert_eq!(slot, Some(ValueSlot::Shape(&VALID_EFFORT_TOKENS)), "{key}");
    }
}

fn base_req() -> ChatRequest {
    ChatRequest {
        model: "default".into(),
        messages: vec![routectl_core::Message {
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    }
}

fn canonical_effort(effort: &str) -> Option<ReasoningConfig> {
    Some(ReasoningConfig {
        effort: Some(effort.into()),
        ..Default::default()
    })
}

/// An Anthropic client's effort-only request as the ingress parses it: the
/// `output_config` carried in the extras and its effort lifted onto the
/// canonical effort.
fn effort_only(level: &str) -> ChatRequest {
    let mut req = base_req();
    req.provider_extras = Some(json!({"output_config": {"effort": level}}));
    req.reasoning = canonical_effort(level);
    req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::SentNone;
    req
}

/// The paths of the effort rows `req` carries.
fn present_effort_paths(table: FieldRepairTable, req: &ChatRequest) -> Vec<&'static str> {
    present_rows(table, req)
        .filter(|row| matches!(row.surface, FieldSurface::Effort(_)))
        .map(|row| row.path)
        .collect()
}

/// The emitted effort is the client's `output_config.effort` when sent and the
/// canonical effort otherwise, and exactly one row reads present for it.
#[test]
fn the_row_of_the_emitted_level_alone_reads_present() {
    let mut client_wins = effort_only("xhigh");
    client_wins.reasoning = canonical_effort("low");
    let mut canonical_only = base_req();
    canonical_only.reasoning = canonical_effort("high");
    let mut non_string = base_req();
    non_string.provider_extras = Some(json!({"output_config": {"effort": 3}}));
    non_string.reasoning = canonical_effort("high");
    let mut off = base_req();
    off.reasoning = canonical_effort("none");
    let cases: [(&str, ChatRequest, Vec<&str>); 5] = [
        (
            "the client's effort",
            effort_only("max"),
            vec!["output_config.effort.max"],
        ),
        (
            "the client's effort over the canonical one",
            client_wins,
            vec!["output_config.effort.xhigh"],
        ),
        (
            "the canonical effort",
            canonical_only,
            vec!["output_config.effort.high"],
        ),
        (
            "a non-string client effort emits no level",
            non_string,
            vec![],
        ),
        ("an effort outside the rank", off, vec![]),
    ];

    for (name, req, expected) in cases {
        assert_eq!(
            present_effort_paths(FieldRepairTable::grounded_only(), &req),
            expected,
            "{name}"
        );
    }
}

/// A rewrite moves every carrier holding the refused level and nothing else:
/// a request carrying only the client's `output_config.effort` gains no
/// `reasoning`, and one carrying only the canonical effort gains no
/// `output_config`. Thinking is never touched.
///
/// Mutation check: create `reasoning` in `write_effort` (`get_or_insert_with`)
/// -> red on the extras-only case.
#[test]
fn an_effort_rewrite_moves_only_the_carriers_holding_the_refused_level() {
    let surface = FieldSurface::Effort("xhigh");
    let mut extras_only = base_req();
    extras_only.provider_extras =
        Some(json!({"output_config": {"effort": "xhigh", "format": {"type": "json"}}}));
    let mut canonical_only = base_req();
    canonical_only.reasoning = canonical_effort("xhigh");
    let mut both = effort_only("xhigh");
    both.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::Sent(
        routectl_core::ThinkingObject::from_client(json!({"type": "adaptive"})).expect("object"),
    );
    let thinking_before = format!("{:?}", both.routectl_internal.anthropic_thinking);

    assert!(surface.rewrite_in(&mut extras_only, "high"));
    assert!(surface.rewrite_in(&mut canonical_only, "high"));
    assert!(surface.rewrite_in(&mut both, "high"));

    assert_eq!(
        extras_only.provider_extras,
        Some(json!({"output_config": {"effort": "high", "format": {"type": "json"}}})),
    );
    assert!(extras_only.reasoning.is_none(), "no reasoning is created");
    assert_eq!(
        canonical_only.reasoning.and_then(|r| r.effort).as_deref(),
        Some("high")
    );
    assert!(
        canonical_only.provider_extras.is_none(),
        "no output_config is created"
    );
    assert_eq!(
        both.provider_extras,
        Some(json!({"output_config": {"effort": "high"}}))
    );
    assert_eq!(
        both.reasoning.and_then(|r| r.effort).as_deref(),
        Some("high")
    );
    assert_eq!(
        format!("{:?}", both.routectl_internal.anthropic_thinking),
        thinking_before,
        "the thinking carrier is untouched",
    );
}

/// A carrier holding another level than the refused one is left as sent, and
/// a rewrite to the level already emitted changes nothing.
#[test]
fn an_effort_rewrite_leaves_a_carrier_holding_another_level() {
    let surface = FieldSurface::Effort("xhigh");
    let mut split = effort_only("xhigh");
    split.reasoning = canonical_effort("low");
    let mut already = effort_only("high");

    assert!(surface.rewrite_in(&mut split, "high"));
    assert!(!surface.rewrite_in(&mut already, "high"));

    assert_eq!(
        split.provider_extras,
        Some(json!({"output_config": {"effort": "high"}}))
    );
    assert_eq!(
        split.reasoning.and_then(|r| r.effort).as_deref(),
        Some("low")
    );
    assert_eq!(
        already.provider_extras,
        Some(json!({"output_config": {"effort": "high"}}))
    );
}

/// An effort row is never dropped: no repair removes a client's effort.
#[test]
fn an_effort_row_drops_nothing() {
    let mut req = effort_only("xhigh");
    let before = req.clone();

    assert!(!FieldSurface::Effort("xhigh").drop_from(&mut req));

    assert_eq!(req.provider_extras, before.provider_extras);
    assert_eq!(
        req.reasoning.and_then(|r| r.effort),
        before.reasoning.and_then(|r| r.effort)
    );
}

fn router() -> Router {
    remote_router_without_negative(Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    }))
}

fn target(router: &Router, req: &ChatRequest) -> DispatchTarget {
    router
        .dispatch_chain_for_request(req, DispatchSurface::Complete)
        .expect("the fixture chain resolves")
        .0
        .into_iter()
        .next()
        .expect("one target")
}

/// While the target clamps the effort itself, no effort row is present for a
/// request carrying any level: no feature key, no pre-flight decision and no
/// reactive candidate. The same request with no clamp carries the row.
///
/// Mutation check: drop the `effort_levels` check in `effort_present` -> red.
#[test]
fn a_target_clamping_the_effort_carries_no_effort_row() {
    let router = router();
    for level in VALID_EFFORT_TOKENS {
        let unclamped = effort_only(level);
        let mut clamped = effort_only(level);
        clamped.routectl_internal.effort_levels = Arc::from(["low".to_string()].as_slice());
        let path = format!("{EFFORT_FIELD}.{level}");
        let key = crate::field_capability::field_capability_key(&path).expect("a qualified path");

        assert_eq!(
            present_effort_paths(FieldRepairTable::grounded_only(), &unclamped),
            vec![path.as_str()],
            "control: {level} is carried without a clamp",
        );
        assert!(
            grounded_field_feature_keys(FieldRepairTable::grounded_only(), &unclamped)
                .contains(&key),
            "control: {level} keys the request without a clamp",
        );
        assert!(
            present_effort_paths(FieldRepairTable::grounded_only(), &clamped).is_empty(),
            "{level}: no row while clamped",
        );
        assert!(
            !grounded_field_feature_keys(FieldRepairTable::grounded_only(), &clamped)
                .contains(&key),
            "{level}: no feature key while clamped",
        );
        let target = target(&router, &clamped);
        let candidates = router
            .plan_field_carry(
                &target,
                &clamped,
                FieldSettlementMode::Settling,
                Instant::now(),
            )
            .map_or_else(Vec::new, |plan| plan.candidate_paths());
        assert!(
            candidates.is_empty(),
            "{level}: no reactive candidate while clamped: {candidates:?}",
        );
        let (planned, records, _plan) =
            router.plan_field_preflight(&clamped, &target, DispatchSurface::Complete);
        assert!(
            records.iter().all(|record| record.field_path.is_none()),
            "{level}: no pre-flight decision on any row: {records:?}",
        );
        assert_eq!(planned.provider_extras, clamped.provider_extras);
    }
}

/// The same effort-only request admits a reactive candidate on an
/// anthropic-api lane: the positive control for the clamp and the
/// openai-compat scope.
#[test]
fn an_effort_only_request_admits_its_row_on_anthropic_api() {
    let router = router();
    let req = effort_only("xhigh");
    let target = target(&router, &req);

    let plan =
        router.plan_field_carry(&target, &req, FieldSettlementMode::Settling, Instant::now());

    assert!(plan.is_some(), "the effort row admits a candidate");
}

/// The openai-compat view carries no effort row, so a request carrying a
/// canonical effort there admits no effort candidate, plans no pre-flight on
/// one and resolves no rejection to one; every other row stays in the view.
///
/// Mutation check: make `RowScope::of_kind` answer `All` for openai-compat ->
/// red.
#[test]
fn the_openai_compat_view_carries_no_effort_row() {
    let compat = FieldRepairTable::grounded_only().for_kind("openai-compat");
    let anthropic = FieldRepairTable::grounded_only().for_kind("anthropic-api");
    let mut req = base_req();
    req.reasoning = canonical_effort("high");
    req.temperature = Some(0.5);

    let compat_paths: Vec<&str> = present_rows(compat, &req).map(|row| row.path).collect();
    let anthropic_paths: Vec<&str> = present_rows(anthropic, &req).map(|row| row.path).collect();

    assert!(compat_paths.contains(&"temperature"), "{compat_paths:?}");
    assert!(
        !compat_paths
            .iter()
            .any(|path| path.starts_with(EFFORT_FIELD)),
        "{compat_paths:?}"
    );
    assert!(
        anthropic_paths.contains(&"output_config.effort.high"),
        "control: {anthropic_paths:?}"
    );
    let unranked = FIELD_REPAIRS.len() - ranked_rows().len();
    assert_eq!(compat.lane_rows().count(), unranked);
    assert_eq!(
        compat.rows().len(),
        FIELD_REPAIRS.len(),
        "a key lookup still sees every row"
    );
}

/// An openai-compat router, one seat on a non-loopback base URL.
fn compat_router() -> Router {
    let mut config: crate::config::Config = toml::from_str(
        "[providers.p0]\nkind = \"openai-compat\"\nbase_url = \"https://compat.example.test/v1\"\n\
         api_key_ref = \"literal:k\"\n",
    )
    .expect("valid test toml");
    config.aliases.insert(
        "default".to_string(),
        crate::config::AliasValue::Single("m0".to_string()),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(std::collections::BTreeMap::from([(
        "m0".to_string(),
        Arc::new(crate::resolved::ResolvedModel::new(
            "m0",
            "p0",
            Arc::new(OkProvider {
                count_calls: AtomicUsize::new(0),
            }) as Arc<dyn routectl_core::Provider>,
            "acme-chat-1",
        )),
    )]));
    router.with_capability_writes_assumed_durable_for_tests()
}

/// On an openai-compat lane a request carrying `reasoning.effort` admits no
/// effort candidate, while the same lane still admits a row its view carries.
#[test]
fn an_openai_compat_lane_admits_no_effort_candidate() {
    let router = compat_router();
    let mut effort = base_req();
    effort.reasoning = canonical_effort("high");
    let mut sampled = effort.clone();
    sampled.temperature = Some(0.5);
    let plan = |req: &ChatRequest| {
        router.plan_field_carry(
            &target(&router, req),
            req,
            FieldSettlementMode::Settling,
            Instant::now(),
        )
    };

    let candidates = |req: &ChatRequest| -> Vec<&'static str> {
        plan(req).map_or_else(Vec::new, |plan| plan.candidate_paths())
    };

    assert!(
        !candidates(&effort)
            .iter()
            .any(|path| path.starts_with(EFFORT_FIELD)),
        "no effort candidate: {:?}",
        candidates(&effort),
    );
    assert!(
        candidates(&sampled).contains(&"temperature"),
        "control: the lane admits temperature"
    );
}
