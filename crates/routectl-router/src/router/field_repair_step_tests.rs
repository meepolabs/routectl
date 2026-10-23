//! The pending-step plan: a second repair on a different field, the refusal of
//! a field a pending step already repaired, a ranked effort row's step per
//! refused level, the re-claim of a released guard, and per-step settlement.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use routectl_core::capability::SignalTier;
use routectl_core::failure_class::FailureClass;
use routectl_core::{ChatRequest, Error, Message, MessageContent, Role};

use super::super::super::class_observe::DispatchSurface;
use super::super::super::learn_engine::{EffortRank, ThinkingShape};
use super::super::super::probe_test_support::{OkProvider, remote_router_without_negative};
use super::super::super::rejection_parser::{NamedRejection, RejectionKind, RejectionValue};
use super::super::super::repair_budget::RepairBudget;
use super::super::super::{DispatchMeta, DispatchTarget, Router};
use super::super::{FieldRepairTable, FieldSettlementMode, closed_table, provisional};
use super::{FieldEdit, FieldRepairPlan, RepairStep, rerepairs_same_field};

const TEMPERATURE: &str = "temperature";
const TOP_P: &str = "top_p";
const MAX_TOKENS: &str = "max_tokens";
const ANTHROPIC: &str = "anthropic-api";
const CLIENT_MAX_TOKENS: u32 = 4096;

fn router() -> Router {
    remote_router_without_negative(Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    }))
}

/// A request carrying the temperature, top_p and max_tokens rows.
fn three_field_req() -> ChatRequest {
    ChatRequest {
        model: "default".into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        temperature: Some(0.5),
        top_p: Some(0.9),
        max_tokens: Some(CLIENT_MAX_TOKENS),
        ..Default::default()
    }
}

/// A request carrying only the top_p row.
fn top_p_req() -> ChatRequest {
    ChatRequest {
        temperature: None,
        max_tokens: None,
        ..three_field_req()
    }
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

fn plan<'r>(
    router: &'r Router,
    req: &ChatRequest,
    mode: FieldSettlementMode,
) -> FieldRepairPlan<'r> {
    router
        .plan_field_carry(&target(router, req), req, mode, Instant::now())
        .expect("a request carrying closed-table rows admits a plan")
}

fn rejection(status: u16) -> Error {
    Error::upstream("p1", status, "{}")
}

/// One `apply` for whatever the thread's injection names.
fn apply(
    plan: &mut FieldRepairPlan<'_>,
    req: &mut ChatRequest,
    budget: &mut RepairBudget,
    status: u16,
) -> Option<u16> {
    plan.apply(
        req,
        &mut DispatchMeta::for_alias("default"),
        budget,
        &FailureClass::BadRequest,
        &rejection(status),
        ANTHROPIC,
    )
}

fn apply_field(
    plan: &mut FieldRepairPlan<'_>,
    req: &mut ChatRequest,
    budget: &mut RepairBudget,
    path: &str,
) -> Option<u16> {
    let _injection = provisional::inject(path);
    apply(plan, req, budget, 400)
}

fn apply_ceiling(
    plan: &mut FieldRepairPlan<'_>,
    req: &mut ChatRequest,
    budget: &mut RepairBudget,
    ceiling: u32,
) -> Option<u16> {
    let _injection = provisional::inject_value(MAX_TOKENS, RejectionValue::Ceiling(ceiling));
    apply(plan, req, budget, 400)
}

#[test]
fn a_second_rejection_naming_a_different_field_adds_a_step() {
    let router = router();
    let mut req = three_field_req();
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    assert!(apply_field(&mut plan, &mut req, &mut budget, TEMPERATURE).is_some());
    let remaining = budget.remaining();

    let second = apply_field(&mut plan, &mut req, &mut budget, TOP_P);

    assert_eq!(second, Some(400));
    assert_eq!(req.top_p, None, "the second field is dropped");
    assert_eq!(req.temperature, None, "and the first edit stays");
    assert_eq!(budget.remaining(), remaining - 1, "one more draw");
    assert_eq!(plan.pending_paths(), vec![TEMPERATURE, TOP_P]);
}

/// On a settling plan the first step's own claim also refuses a second claim
/// on the same key; a non-settling plan holds no claim, so there the
/// pending-field refusal alone stands between the rejection and a second clamp.
///
/// Mutation check: delete the `repaired_field` refusal in
/// `apply_named` -> red on `non_settling` (the second ceiling clamps again and
/// draws).
#[test]
fn a_second_rejection_naming_a_pending_field_repairs_nothing() {
    let modes = [
        ("settling", FieldSettlementMode::Settling),
        ("non_settling", FieldSettlementMode::NonSettling),
    ];
    for (name, mode) in modes {
        let router = router();
        let mut req = three_field_req();
        let mut plan = plan(&router, &req, mode);
        let mut budget = RepairBudget::per_request();
        assert_eq!(
            apply_ceiling(&mut plan, &mut req, &mut budget, 2048),
            Some(400),
            "{name}: premise",
        );
        assert_eq!(req.max_tokens, Some(2048), "{name}: premise");
        let body = serde_json::to_value(&req).expect("serializable");
        let remaining = budget.remaining();

        // The field is still present and a lower ceiling would still lower it.
        let second = apply_ceiling(&mut plan, &mut req, &mut budget, 1024);

        assert_eq!(second, None, "{name}");
        assert_eq!(
            serde_json::to_value(&req).expect("serializable"),
            body,
            "{name}"
        );
        assert_eq!(budget.remaining(), remaining, "{name}: no draw");
        assert_eq!(plan.pending_paths(), vec![MAX_TOKENS], "{name}");
    }
}

/// Mutation check: make `claim` return `Claim::Kept` unconditionally -> red
/// (the field is dropped while a sibling holds its claim).
#[test]
fn a_released_row_is_claimed_again_and_a_sibling_claim_refuses_the_step() {
    let router = router();
    let mut req = three_field_req();
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    assert!(apply_field(&mut plan, &mut req, &mut budget, TEMPERATURE).is_some());
    // The first repair released top_p's guard, so a sibling claims it.
    let sibling = plan_or_none(&router, &top_p_req());
    assert!(
        sibling.is_some(),
        "premise: the released slot admits a sibling"
    );
    let body = serde_json::to_value(&req).expect("serializable");
    let remaining = budget.remaining();

    let refused = apply_field(&mut plan, &mut req, &mut budget, TOP_P);

    assert_eq!(refused, None, "the sibling holds the claim");
    assert_eq!(serde_json::to_value(&req).expect("serializable"), body);
    assert_eq!(budget.remaining(), remaining, "no draw");
    assert_eq!(plan.pending_paths(), vec![TEMPERATURE]);

    drop(sibling);
    let reclaimed = apply_field(&mut plan, &mut req, &mut budget, TOP_P);

    assert!(reclaimed.is_some(), "the freed slot is claimed again");
    assert_eq!(req.top_p, None);
    assert!(
        plan_or_none(&router, &top_p_req()).is_none(),
        "and the step now holds that claim, so a sibling is refused",
    );
}

fn plan_or_none<'r>(router: &'r Router, req: &ChatRequest) -> Option<FieldRepairPlan<'r>> {
    router.plan_field_carry(
        &target(router, req),
        req,
        FieldSettlementMode::Settling,
        Instant::now(),
    )
}

#[test]
fn commit_pending_commits_each_step_with_its_own_tier_value_and_status() {
    let router = router();
    let mut req = three_field_req();
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    let mut meta = DispatchMeta::for_alias("default");
    let target = target(&router, &req);
    {
        let _injection = provisional::inject_with_tier(TEMPERATURE, SignalTier::Inferred);
        assert!(apply(&mut plan, &mut req, &mut budget, 422).is_some());
    }
    router.note_field_repair(&mut meta, target.repair_target(), &mut plan);
    assert!(apply_ceiling(&mut plan, &mut req, &mut budget, 2048).is_some());
    router.note_field_repair(&mut meta, target.repair_target(), &mut plan);

    let settled = plan.commit_pending(&[], Instant::now());

    assert!(!plan.has_pending(), "the commit drains the steps");
    let handles: Vec<_> = settled
        .iter()
        .map(|step| step.handle.map(|h| h.0))
        .collect();
    assert_eq!(
        handles,
        vec![Some(0), Some(1)],
        "each step settles its own record"
    );
    let events: Vec<_> = settled
        .into_iter()
        .map(|step| step.learned.expect("each step persists its verdict"))
        .collect();
    let key = |path| crate::field_capability::field_capability_key(path).expect("a key");
    assert_eq!(events[0].capability_key, key(TEMPERATURE));
    assert_eq!(events[0].signal_tier, SignalTier::Inferred);
    assert_eq!(events[0].upstream_status, 422);
    assert_eq!(events[0].value, None);
    assert_eq!(events[1].capability_key, key(MAX_TOKENS));
    assert_eq!(events[1].signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(events[1].upstream_status, 400);
    assert_eq!(events[1].value, Some(crate::VerdictValue::Ceiling(2048)));
}

#[test]
fn a_non_settling_plan_commits_nothing_and_holds_no_claim() {
    let router = router();
    let mut req = three_field_req();
    let mut plan = plan(&router, &req, FieldSettlementMode::NonSettling);
    let mut budget = RepairBudget::per_request();
    assert!(apply_field(&mut plan, &mut req, &mut budget, TEMPERATURE).is_some());
    assert!(apply_field(&mut plan, &mut req, &mut budget, TOP_P).is_some());

    assert!(
        plan_or_none(&router, &top_p_req()).is_some(),
        "no claim held: a settling sibling admits",
    );
    let settled = plan.commit_pending(&[], Instant::now());

    assert_eq!(settled.len(), 2);
    assert!(settled.iter().all(|step| step.learned.is_none()));
}

#[test]
fn only_the_ranked_effort_rows_rerepair_their_field() {
    let rows = closed_table();
    assert!(
        rows.iter().any(rerepairs_same_field),
        "premise: a ranked row"
    );
    for row in rows {
        let ranked = matches!(row.transform, super::FieldTransform::RankedRewrite { .. });
        assert_eq!(rerepairs_same_field(row), ranked, "{}", row.path);
    }
}

// -----------------------------------------------------------------------
// A ranked effort row: one step per refused level
// -----------------------------------------------------------------------

const EFFORT: &str = "output_config.effort";

/// A request whose only carried row is the effort at `level`.
fn effort_req(level: &str) -> ChatRequest {
    ChatRequest {
        temperature: None,
        top_p: None,
        max_tokens: None,
        reasoning: Some(routectl_core::ReasoningConfig {
            effort: Some(level.into()),
            ..Default::default()
        }),
        ..three_field_req()
    }
}

fn sent_level(req: &ChatRequest) -> Option<&str> {
    req.reasoning.as_ref().and_then(|r| r.effort.as_deref())
}

fn effort_path(level: &str) -> String {
    format!("{EFFORT}.{level}")
}

/// Repair a refusal of the `refused` level listing `listed`, sent beside
/// the thinking shape `thinking`.
fn apply_level(
    plan: &mut FieldRepairPlan<'_>,
    req: &mut ChatRequest,
    budget: &mut RepairBudget,
    (refused, listed): (&str, &[&str]),
    thinking: ThinkingShape,
) -> Option<u16> {
    let row = FieldRepairTable::default()
        .row_for_path(&effort_path(refused))
        .expect("a ranked row");
    let levels = listed
        .iter()
        .map(|level| EffortRank::of_token(level).expect("a rank member").bit())
        .fold(0, |mask, bit| mask | bit);
    let named = NamedRejection {
        row,
        tier: SignalTier::SelfIdentifying,
        kind: RejectionKind::InvalidValue,
        value: Some(RejectionValue::Levels(levels)),
    };
    plan.apply_named(
        named,
        thinking,
        req,
        &mut DispatchMeta::for_alias("default"),
        budget,
        &rejection(400),
    )
}

/// Whether a sibling request at `level` is admitted that level's row.
fn sibling_admits(router: &Router, level: &str) -> bool {
    let path = effort_path(level);
    plan_or_none(router, &effort_req(level))
        .is_some_and(|sibling| sibling.candidate_paths().contains(&path.as_str()))
}

/// A refusal of the level a ranked repair wrote adds a step on that level's
/// row beside the first, claimed on the request's behalf for its own key:
/// each step holds its level's claim, each draws, and once proven each
/// commits the level that finally went out under its own key.
///
/// Mutation checks: refuse a pending ranked row in `repaired_field` -> red
/// (the second refusal repairs nothing); commit each step's own written value
/// instead of `proven_value` -> red (`max` learns `xhigh`).
#[test]
fn each_refused_level_adds_its_own_step_and_commits_the_level_that_went_out() {
    let router = router();
    let mut req = effort_req("max");
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    let adaptive = ThinkingShape::Adaptive;
    let first = ("max", ["high", "low", "medium", "xhigh"].as_slice());
    assert!(apply_level(&mut plan, &mut req, &mut budget, first, adaptive).is_some());
    assert_eq!(sent_level(&req), Some("xhigh"), "premise");

    let second = ("xhigh", ["high", "low", "medium"].as_slice());
    let applied = apply_level(&mut plan, &mut req, &mut budget, second, adaptive);

    assert_eq!(applied, Some(400));
    assert_eq!(sent_level(&req), Some("high"));
    assert_eq!(budget.spent(), 2, "each step draws");
    let (max, xhigh) = (effort_path("max"), effort_path("xhigh"));
    assert_eq!(plan.pending_paths(), vec![max.as_str(), xhigh.as_str()]);
    for level in ["max", "xhigh"] {
        assert!(
            !sibling_admits(&router, level),
            "{level}: its step holds the claim",
        );
    }
    let learned: Vec<_> = plan
        .commit_pending(&[], Instant::now())
        .into_iter()
        .map(|step| {
            let event = step.learned.expect("each step learns");
            (event.capability_key, event.value)
        })
        .collect();
    let key = |path: &str| crate::field_capability::field_capability_key(path).expect("a key");
    let high = Some(crate::VerdictValue::Shape("high"));
    assert_eq!(learned, vec![(key(&max), high), (key(&xhigh), high)]);
}

/// Each step commits only where its own refusal may be learned: one sent
/// beside a disabled thinking commits nothing, while the next commits.
#[test]
fn each_ranked_step_commits_under_its_own_mintable_bit() {
    let router = router();
    let mut req = effort_req("max");
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    let first = ("max", ["high", "xhigh"].as_slice());
    let second = ("xhigh", ["high"].as_slice());
    assert!(
        apply_level(
            &mut plan,
            &mut req,
            &mut budget,
            first,
            ThinkingShape::Disabled
        )
        .is_some()
    );
    assert!(
        apply_level(
            &mut plan,
            &mut req,
            &mut budget,
            second,
            ThinkingShape::Adaptive
        )
        .is_some()
    );

    let learned: Vec<_> = plan
        .commit_pending(&[], Instant::now())
        .into_iter()
        .map(|step| step.learned.map(|event| event.value))
        .collect();

    assert_eq!(
        learned,
        vec![None, Some(Some(crate::VerdictValue::Shape("high")))]
    );
}

/// The replacement is never a level the request already sent: `medium`
/// refused listing `high, low` moves up to `high`, not back to the refused
/// `low`, and a refusal whose list holds only levels already sent repairs
/// nothing and draws nothing.
///
/// Mutation check: exclude only the refused level in `ranked_edit_value`
/// (drop `sent`) -> red (the second repair goes back to `low`).
#[test]
fn a_level_already_sent_is_never_picked_again() {
    let router = router();
    let mut req = effort_req("low");
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    let adaptive = ThinkingShape::Adaptive;
    let low = ("low", ["high", "medium"].as_slice());
    assert!(apply_level(&mut plan, &mut req, &mut budget, low, adaptive).is_some());
    assert_eq!(
        sent_level(&req),
        Some("medium"),
        "premise: nearest lower is none, so higher"
    );

    let medium = ("medium", ["high", "low"].as_slice());
    assert!(apply_level(&mut plan, &mut req, &mut budget, medium, adaptive).is_some());

    assert_eq!(sent_level(&req), Some("high"), "low was already sent");
    let spent = budget.spent();
    let high = ("high", ["low", "medium"].as_slice());
    assert_eq!(
        apply_level(&mut plan, &mut req, &mut budget, high, adaptive),
        None
    );
    assert_eq!(sent_level(&req), Some("high"), "nothing left to pick");
    assert_eq!(budget.spent(), spent, "no draw");
}

/// When a sibling request holds the replacement level's claim, the refusal of
/// that level is still repaired: the step draws and writes the next level,
/// but holds no claim, so it learns nothing and its commit leaves the
/// sibling's claim where it is. The earlier step still learns.
///
/// Mutation checks: make `claim` answer `None` for a held replacement again
/// -> red (the refusal repairs nothing); mint the unheld step in
/// `install_step` -> red on `pending_mintable`.
#[test]
fn a_sibling_holding_the_replacement_levels_claim_repairs_without_learning() {
    let router = router();
    let mut req = effort_req("max");
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    let adaptive = ThinkingShape::Adaptive;
    let first = ("max", ["high", "xhigh"].as_slice());
    assert!(apply_level(&mut plan, &mut req, &mut budget, first, adaptive).is_some());
    let sibling = plan_or_none(&router, &effort_req("xhigh"));
    assert!(
        sibling.as_ref().is_some_and(|plan| plan
            .candidate_paths()
            .contains(&effort_path("xhigh").as_str())),
        "premise: the sibling claims xhigh",
    );
    let spent = budget.spent();

    let second = ("xhigh", ["high"].as_slice());
    let repaired = apply_level(&mut plan, &mut req, &mut budget, second, adaptive);

    assert_eq!(repaired, Some(400));
    assert_eq!(sent_level(&req), Some("high"));
    assert_eq!(budget.spent(), spent + 1, "the step draws");
    let (max, xhigh) = (effort_path("max"), effort_path("xhigh"));
    assert_eq!(plan.pending_paths(), vec![max.as_str(), xhigh.as_str()]);
    assert_eq!(plan.pending_mintable(), vec![true, false]);
    let learned: Vec<_> = plan
        .commit_pending(&[], Instant::now())
        .into_iter()
        .map(|step| step.learned.map(|event| event.capability_key))
        .collect();
    let key = |path: &str| crate::field_capability::field_capability_key(path).expect("a key");
    assert_eq!(learned, vec![Some(key(&max)), None]);
    assert!(
        !sibling_admits(&router, "xhigh"),
        "the sibling still holds xhigh's claim",
    );
    drop(sibling);
    assert!(sibling_admits(&router, "xhigh"), "and releases it itself");
}

/// A replacement level on which a learned verdict already acts is repaired
/// the same way: the step holds no claim, learns nothing, and leaves the
/// acting verdict standing.
#[test]
fn an_acting_verdict_on_the_replacement_level_repairs_without_learning() {
    let router = router();
    let adaptive = ThinkingShape::Adaptive;
    let mut taught = effort_req("xhigh");
    let mut teacher = plan(&router, &taught, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    let xhigh_refused = ("xhigh", ["high"].as_slice());
    assert!(
        apply_level(
            &mut teacher,
            &mut taught,
            &mut budget,
            xhigh_refused,
            adaptive
        )
        .is_some()
    );
    let event = teacher
        .commit_pending(&[], Instant::now())
        .pop()
        .and_then(|step| step.learned)
        .expect("premise: xhigh learns high");
    assert!(router.acknowledge_durable_field_confirmation(
        &event.state_key,
        &event.capability_key,
        &event.provider_kind,
        event.persistence_generation,
        event.incarnation,
        event.observations,
    ));
    drop(teacher);
    let xhigh_key = crate::field_verdict::FieldVerdictKey::from_capability_key(
        crate::state_key::StateKey::parse(&event.state_key).expect("a lane"),
        event.capability_key.clone(),
        event.provider_kind.clone(),
    );
    let acting = || {
        router
            .field_verdicts()
            .is_negative_acting(&xhigh_key, Instant::now())
    };
    assert!(acting(), "premise: the verdict acts");
    let mut req = effort_req("max");
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    let max_refused = ("max", ["high", "xhigh"].as_slice());
    assert!(apply_level(&mut plan, &mut req, &mut budget, max_refused, adaptive).is_some());

    let repaired = apply_level(&mut plan, &mut req, &mut budget, xhigh_refused, adaptive);

    assert_eq!(repaired, Some(400));
    assert_eq!(sent_level(&req), Some("high"));
    assert_eq!(plan.pending_mintable(), vec![true, false]);
    let learned: Vec<_> = plan
        .commit_pending(&[], Instant::now())
        .into_iter()
        .map(|step| step.learned.is_some())
        .collect();
    assert_eq!(learned, vec![true, false]);
    assert!(acting(), "the acting verdict stands");
}

/// The first ranked step keeps the single-step rule: with no pending step on
/// the effort field, a sibling holding the refused level's claim leaves the
/// refusal unrepaired, with no edit and no draw.
#[test]
fn a_sibling_holding_the_first_refused_levels_claim_refuses_the_step() {
    let router = router();
    let sibling = plan_or_none(&router, &effort_req("max"));
    assert!(sibling.is_some(), "premise: the sibling claims max");
    let mut req = ChatRequest {
        temperature: Some(0.5),
        ..effort_req("max")
    };
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    let first = ("max", ["high", "xhigh"].as_slice());

    let refused = apply_level(
        &mut plan,
        &mut req,
        &mut budget,
        first,
        ThinkingShape::Adaptive,
    );

    assert_eq!(refused, None);
    assert_eq!(sent_level(&req), Some("max"), "no edit");
    assert_eq!(budget.spent(), 0, "no draw");
    assert!(plan.pending_paths().is_empty());
}

/// A committed effort is never repaired again on the target, whatever level
/// the refusal names.
#[test]
fn a_committed_effort_is_never_repaired_again() {
    let router = router();
    let mut req = effort_req("max");
    let mut plan = plan(&router, &req, FieldSettlementMode::NonSettling);
    let mut budget = RepairBudget::per_request();
    let adaptive = ThinkingShape::Adaptive;
    let first = ("max", ["high", "xhigh"].as_slice());
    assert!(apply_level(&mut plan, &mut req, &mut budget, first, adaptive).is_some());
    assert_eq!(plan.commit_pending(&[], Instant::now()).len(), 1, "premise");

    let again = ("xhigh", ["high"].as_slice());

    assert_eq!(
        apply_level(&mut plan, &mut req, &mut budget, again, adaptive),
        None
    );
    assert_eq!(sent_level(&req), Some("xhigh"));
}

/// A request whose native thinking carrier is `{type: enabled}`.
fn thinking_enabled_req() -> ChatRequest {
    let mut req = three_field_req();
    req.temperature = None;
    req.top_p = None;
    req.reasoning = Some(routectl_core::ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: Some(2048),
        exclude: Some(false),
        enabled: Some(true),
    });
    req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::Sent(
        routectl_core::ThinkingObject::from_client(
            serde_json::json!({"type": "enabled", "budget_tokens": 2048}),
        )
        .expect("an object builds a carrier"),
    );
    req
}

fn thinking_step(path: &str) -> RepairStep<'static> {
    RepairStep {
        row: *FieldRepairTable::default()
            .row_for_path(path)
            .expect("a grounded row"),
        edit: FieldEdit::Rewrite("thinking.type.adaptive"),
        kind: super::RejectionKind::InvalidValue,
        tier: SignalTier::SelfIdentifying,
        fill_clamped: false,
        handle: None,
        reject_status: 400,
        mintable: true,
        guard: None,
    }
}

/// The two thinking-type rows name one field, so a pending step on either
/// refuses a repair of the other. A real disabled-then-adaptive sequence never
/// reaches this check (neither row is present after the rewrite), so the
/// pending step is planted beside a request that still carries the other row.
///
/// Mutation check: make `FieldIdentity` keep the value segment -> red on
/// `disabled_pending`.
#[test]
fn a_pending_step_on_one_thinking_type_row_refuses_the_other() {
    let rows = [
        ("disabled_pending", Some("thinking.type.disabled"), None),
        ("nothing_pending", None, Some(400)),
    ];
    for (name, pending, expected) in rows {
        let router = router();
        let mut req = thinking_enabled_req();
        let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
        plan.pending.extend(pending.map(thinking_step));
        let mut budget = RepairBudget::per_request();
        let remaining = budget.remaining();

        let applied = {
            let _injection = provisional::inject_value(
                "thinking.type.enabled",
                RejectionValue::Shape("thinking.type.adaptive"),
            );
            apply(&mut plan, &mut req, &mut budget, 400)
        };

        assert_eq!(applied, expected, "{name}");
        let drawn = remaining - budget.remaining();
        assert_eq!(drawn, u8::from(expected.is_some()), "{name}");
    }
}

/// A committed field stays repaired for the rest of the target, and the plan
/// still reads as having applied a repair once nothing is pending. On a
/// settling plan the verdict the commit persisted also refuses a fresh claim,
/// so the non-settling plan, which claims nothing, is where the committed
/// field's memory alone refuses the repair. A ranked row re-repairs only a
/// PENDING step's field, so a committed effort is refused the same way (see
/// `a_committed_effort_is_never_repaired_again`).
///
/// Mutation check: drop the committed-identity read in `repaired_field` ->
/// red.
#[test]
fn a_committed_field_is_never_repaired_again_and_the_plan_stays_applied() {
    let router = router();
    let mut req = three_field_req();
    let mut plan = plan(&router, &req, FieldSettlementMode::NonSettling);
    let mut budget = RepairBudget::per_request();
    assert!(!plan.has_applied(), "premise: nothing applied yet");
    let first = apply_ceiling(&mut plan, &mut req, &mut budget, 2048);
    assert!(first.is_some(), "premise");
    let committed = plan.commit_pending(&[], Instant::now());
    assert_eq!(committed.len(), 1, "premise");
    let remaining = budget.remaining();

    let again = apply_ceiling(&mut plan, &mut req, &mut budget, 1024);

    assert_eq!(again, None);
    assert_eq!(req.max_tokens, Some(2048), "the committed value stands");
    assert_eq!(budget.remaining(), remaining, "no draw");
    assert!(!plan.has_pending());
    assert!(plan.has_applied(), "a drained plan still applied a repair");
}
