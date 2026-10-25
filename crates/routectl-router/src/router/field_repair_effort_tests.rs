//! The effort rows: welded to the effort rank, read and rewritten through
//! their two carriers, scoped off the kinds with no grounded effort refusal,
//! and absent while the target clamps the effort itself; the replacement a
//! refusal's level list picks, and the thinking shapes that may learn it.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use routectl_core::capability::SignalTier;
use routectl_core::{ChatRequest, ReasoningConfig};
use routectl_providers::effort::VALID_EFFORT_TOKENS;
use serde_json::json;

use super::super::class_observe::DispatchSurface;
use super::super::learn_engine::{EffortRank, SentEffort, ThinkingShape};
use super::super::probe_test_support::{OkProvider, remote_router_without_negative};
use super::super::rejection_parser::{NamedRejection, RejectionKind, RejectionValue};
use super::super::{DispatchTarget, Router};
use super::effort::{ranked_replacement, ranked_step_mints};
use super::{
    FIELD_REPAIRS, FieldEdit, FieldRepairTable, FieldSettlementMode, FieldSurface, FieldTransform,
    TransformClass, grounded_field_feature_keys, parse_rejection, present_rows,
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
/// Mutation check: make `RowScope::of_lane` answer `All` for openai-compat ->
/// red.
#[test]
fn the_openai_compat_view_carries_no_effort_row() {
    let compat = FieldRepairTable::grounded_only().for_lane(
        "openai-compat",
        crate::router::field_repair::direct_egress("openai-compat"),
    );
    let anthropic = FieldRepairTable::grounded_only().for_lane(
        "anthropic-api",
        crate::router::field_repair::direct_egress("anthropic-api"),
    );
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

/// The openai-responses view carries the effort rows alone: a request
/// carrying `temperature` beside a canonical effort presents only the effort
/// row, and a codex or Mantle Responses refusal naming `temperature` resolves
/// to no row. The parser does not localize that param either, so the view is
/// asserted on its own through `present_rows`.
///
/// Mutation check: make `RowScope::of_lane` answer `All` for openai-responses
/// -> red on the present rows and the row count.
#[test]
fn the_openai_responses_view_carries_only_the_effort_rows() {
    let responses = FieldRepairTable::grounded_only().for_lane(
        "openai-responses",
        crate::router::field_repair::direct_egress("openai-responses"),
    );
    let mut req = base_req();
    req.reasoning = canonical_effort("high");
    req.temperature = Some(0.5);

    let paths: Vec<&str> = present_rows(responses, &req).map(|row| row.path).collect();

    assert_eq!(paths, vec!["output_config.effort.high"]);
    assert_eq!(responses.lane_rows().count(), ranked_rows().len());
    assert_eq!(
        responses.rows().len(),
        FIELD_REPAIRS.len(),
        "a key lookup still sees every row"
    );
    let temperature_refusals = [
        (
            "codex unsupported parameter",
            json!({"error": {
                "message": "Unsupported parameter: 'temperature' is not supported with this model.",
                "type": "invalid_request_error",
                "param": "temperature",
                "code": "unsupported_parameter",
            }}),
        ),
        (
            "mantle responses invalid value",
            json!({"error": {
                "code": "invalid_value",
                "message": "Invalid value: 'hot'. Supported values are: 'cold'.",
                "param": "temperature",
                "type": "invalid_request_error",
            }}),
        ),
    ];
    for (name, body) in temperature_refusals {
        let body = routectl_core::UpstreamBody::new(body.to_string());

        let named = parse_rejection(
            responses,
            "openai-responses",
            crate::router::field_repair::direct_egress("openai-responses"),
            400,
            body.parsed(),
            SentEffort::Rank(rank("high")),
        );

        assert!(named.is_none(), "{name}");
    }
}

/// An openai-responses router, one api-key seat on its default base URL.
#[cfg(feature = "openai-responses")]
fn responses_router() -> Router {
    let mut config: crate::config::Config = toml::from_str(
        "[providers.p0]\nkind = \"openai-responses\"\napi_key_ref = \"literal:k\"\n\
         auth_kind = \"api-key\"\n",
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
            "gpt-5.6-terra",
        )),
    )]));
    router.with_capability_writes_assumed_durable_for_tests()
}

/// On an openai-responses lane a request carrying `temperature` beside an
/// effort admits the effort candidate alone.
#[cfg(feature = "openai-responses")]
#[test]
fn an_openai_responses_lane_admits_only_the_effort_candidate() {
    let router = responses_router();
    let mut req = base_req();
    req.reasoning = canonical_effort("high");
    req.temperature = Some(0.5);

    let plan = router.plan_field_carry(
        &target(&router, &req),
        &req,
        FieldSettlementMode::Settling,
        Instant::now(),
    );

    assert_eq!(
        plan.map_or_else(Vec::new, |plan| plan.candidate_paths()),
        vec!["output_config.effort.high"],
    );
}

/// The rank bitmask of `levels`.
fn mask(levels: &[&str]) -> u8 {
    levels
        .iter()
        .map(|level| EffortRank::of_token(level).expect("a rank member").bit())
        .fold(0, |mask, bit| mask | bit)
}

fn rank(level: &str) -> EffortRank {
    EffortRank::of_token(level).expect("a rank member")
}

/// `(name, refused, listed, excluded, picked)`.
type PickCase = (
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    Option<&'static str>,
);

/// The replacement is the nearest listed level below the refused one, else
/// the nearest above it, never the refused level or an excluded one, and none
/// when nothing listed is left.
///
/// Mutation checks: prefer the higher branch first in `ranked_replacement`
/// -> red on "lower over higher"; take the first lower level instead of the
/// last -> red on "nearest lower"; drop `!excluded` -> red on "excluded".
#[test]
fn the_replacement_is_the_nearest_lower_listed_level_else_the_nearest_higher() {
    let cases: [PickCase; 8] = [
        (
            "nearest lower",
            "xhigh",
            &["high", "low", "medium"],
            &[],
            Some("high"),
        ),
        (
            "lower over higher",
            "xhigh",
            &["high", "low", "max", "medium"],
            &[],
            Some("high"),
        ),
        (
            "only higher: nearest higher",
            "minimal",
            &["high", "low", "medium"],
            &[],
            Some("low"),
        ),
        (
            "only higher, a gap",
            "low",
            &["max", "xhigh"],
            &[],
            Some("xhigh"),
        ),
        ("empty list", "max", &[], &[], None),
        ("only the refused level", "high", &["high"], &[], None),
        (
            "excluded levels are skipped",
            "xhigh",
            &["high", "medium"],
            &["high"],
            Some("medium"),
        ),
        (
            "every listed level excluded",
            "xhigh",
            &["high"],
            &["high"],
            None,
        ),
    ];

    for (name, refused, listed, excluded, expected) in cases {
        let refused = rank(refused);
        let excluded = mask(excluded) | refused.bit();

        let picked = ranked_replacement(refused, mask(listed), excluded).map(EffortRank::token);

        assert_eq!(picked, expected, "{name}");
    }
}

/// Over every refused level, listed set and excluded set, the pick is a
/// listed rank member that is neither refused nor excluded, below the refused
/// level whenever such a member is open, and the nearest one on its side.
#[test]
fn every_pick_is_an_open_listed_level_nearest_on_its_side() {
    let members: Vec<EffortRank> = VALID_EFFORT_TOKENS
        .iter()
        .map(|level| rank(level))
        .collect();
    let masks = 0..=u8::MAX >> (u8::BITS as usize - VALID_EFFORT_TOKENS.len());
    for &refused in &members {
        for listed in masks.clone() {
            for excluded in masks.clone() {
                let open = |level: EffortRank| {
                    level != refused && listed & level.bit() != 0 && excluded & level.bit() == 0
                };
                let at = |level: EffortRank| members.iter().position(|m| *m == level);
                let lower = members
                    .iter()
                    .copied()
                    .rev()
                    .find(|&level| open(level) && at(level) < at(refused));
                let higher = members
                    .iter()
                    .copied()
                    .find(|&level| open(level) && at(level) > at(refused));

                let picked = ranked_replacement(refused, listed, excluded);

                assert_eq!(
                    picked,
                    lower.or(higher),
                    "refused {} listed {listed:#08b} excluded {excluded:#08b}",
                    refused.token(),
                );
                assert!(picked.is_none_or(open), "an open listed level");
            }
        }
    }
}

/// Only an adaptive or enabled thinking beside the effort, or a body with no
/// thinking object at all, lets a ranked repair be learned.
///
/// Mutation check: make `Disabled` mintable in `ranked_step_mints` -> red.
#[test]
fn a_ranked_repair_is_learnable_only_beside_a_shape_it_cannot_depend_on() {
    let cases = [
        (ThinkingShape::Adaptive, true),
        (ThinkingShape::Enabled, true),
        (ThinkingShape::NoThinkingObject, true),
        (ThinkingShape::Disabled, false),
        (ThinkingShape::BetweenTools, false),
        (ThinkingShape::Absent, false),
        (ThinkingShape::Unknown, false),
    ];

    for (shape, mints) in cases {
        assert_eq!(ranked_step_mints(shape), mints, "{shape:?}");
    }
}

/// The ranked row an effort refusal named, with the levels it listed.
fn ranked_named(level: &str, value: Option<RejectionValue>) -> NamedRejection {
    let row = FieldRepairTable::grounded_only()
        .row_for_path(&format!("{EFFORT_FIELD}.{level}"))
        .expect("a ranked row");
    NamedRejection {
        row,
        tier: SignalTier::SelfIdentifying,
        kind: if value.is_some() {
            RejectionKind::InvalidValue
        } else {
            RejectionKind::UnsupportedField
        },
        value,
    }
}

/// A ranked row rewrites the refused level to the picked one; a refused
/// effort field, which lists no level, makes no edit.
#[test]
fn a_ranked_row_rewrites_to_the_picked_level_and_a_refused_field_makes_no_edit() {
    let listed = ranked_named(
        "xhigh",
        Some(RejectionValue::Levels(mask(&["high", "low"]))),
    );
    let higher = ranked_named("minimal", Some(RejectionValue::Levels(mask(&["medium"]))));
    let field = ranked_named("xhigh", None);
    let edit = |named: &NamedRejection| FieldEdit::for_rejection(named.row.transform, named, 0);

    assert_eq!(edit(&listed), Some(FieldEdit::Rewrite("high")));
    assert_eq!(edit(&higher), Some(FieldEdit::Rewrite("medium")));
    assert_eq!(edit(&field), None);
}

/// A refusal of the effort FIELD on each lane that sends one resolves to the
/// sent level's row and makes no edit, so nothing is repaired or learned: the
/// Bedrock bare `Extra inputs`, and a structured Responses
/// `unsupported_parameter` naming `reasoning.effort`.
#[test]
fn a_refused_effort_field_resolves_to_its_row_and_makes_no_edit() {
    let cases = [
        (
            "bedrock bare extra inputs",
            "bedrock",
            json!({"message": "output_config.effort: Extra inputs are not permitted"}),
        ),
        (
            "responses unsupported parameter",
            "openai-responses",
            json!({"error": {
                "message": "Unsupported parameter: 'reasoning.effort' is not supported with this model.",
                "type": "invalid_request_error",
                "param": "reasoning.effort",
                "code": "unsupported_parameter",
            }}),
        ),
    ];

    for (name, kind, body) in cases {
        let body = routectl_core::UpstreamBody::new(body.to_string());
        let named = parse_rejection(
            FieldRepairTable::grounded_only()
                .for_lane(kind, crate::router::field_repair::direct_egress(kind)),
            kind,
            crate::router::field_repair::direct_egress(kind),
            400,
            body.parsed(),
            SentEffort::Rank(rank("high")),
        )
        .unwrap_or_else(|| panic!("{name}: premise: the refusal names the sent level's row"));

        assert_eq!(named.row.path, "output_config.effort.high", "{name}");
        assert_eq!(named.kind, RejectionKind::UnsupportedField, "{name}");
        assert_eq!(named.value, None, "{name}");
        assert_eq!(
            FieldEdit::for_rejection(named.row.transform, &named, 0),
            None,
            "{name}"
        );
    }
}
