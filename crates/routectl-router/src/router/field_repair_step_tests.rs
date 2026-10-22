//! The pending-step plan: a second repair on a different field, the refusal of
//! a field a pending step already repaired, the re-claim of a released guard,
//! and per-step settlement.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use routectl_core::capability::SignalTier;
use routectl_core::failure_class::FailureClass;
use routectl_core::{ChatRequest, Error, Message, MessageContent, Role};

use super::super::super::class_observe::DispatchSurface;
use super::super::super::probe_test_support::{OkProvider, remote_router_without_negative};
use super::super::super::rejection_parser::RejectionValue;
use super::super::super::repair_budget::RepairBudget;
use super::super::super::{DispatchMeta, DispatchTarget, Router};
use super::super::{FieldRepairTable, FieldSettlementMode, closed_table, provisional};
use super::{FieldEdit, FieldRepairPlan, RepairStep, rerepair_opt_in, rerepairs_same_field};

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
fn no_row_rerepairs_its_field() {
    let rows = closed_table();
    assert!(!rows.is_empty());
    for row in rows {
        assert!(!rerepairs_same_field(row), "{}", row.path);
    }
}

/// A request carrying only the max_tokens row.
fn max_tokens_req() -> ChatRequest {
    ChatRequest {
        temperature: None,
        top_p: None,
        ..three_field_req()
    }
}

/// A row that re-repairs its field supersedes the pending step on it: the
/// new step takes over that step's claim rather than claiming again, so one
/// claim is held across both repairs, and only the final step commits.
///
/// Mutation checks: claim afresh instead of taking the superseded step's
/// guard -> red (the in-flight claim refuses the second repair); drop the
/// superseded step's guard instead of handing it over -> red (a sibling
/// admits, and the commit learns nothing).
#[test]
fn a_same_field_rerepair_takes_over_the_pending_steps_claim() {
    let router = router();
    let mut req = max_tokens_req();
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let mut budget = RepairBudget::per_request();
    let _opt_in = rerepair_opt_in::opt_in(MAX_TOKENS);
    assert_eq!(
        apply_ceiling(&mut plan, &mut req, &mut budget, 2048),
        Some(400),
        "premise",
    );

    let second = apply_ceiling(&mut plan, &mut req, &mut budget, 1024);

    assert_eq!(second, Some(400), "the same field repairs again");
    assert_eq!(req.max_tokens, Some(1024));
    assert_eq!(budget.spent(), 2, "each repair draws");
    assert_eq!(
        plan.pending_paths(),
        vec![MAX_TOKENS],
        "superseded, not added"
    );
    assert!(
        plan_or_none(&router, &max_tokens_req()).is_none(),
        "the field's claim is still held, so a sibling is refused",
    );
    let settled = plan.commit_pending(&[], Instant::now());
    let learned: Vec<_> = settled
        .into_iter()
        .map(|step| step.learned.map(|event| event.value))
        .collect();
    assert_eq!(
        learned,
        vec![Some(Some(crate::VerdictValue::Ceiling(1024)))],
        "only the final step commits, with the value that succeeded",
    );
}

/// The plan holds no one-step-per-field invariant: two pending steps on one
/// field are representable, and each settles.
#[test]
fn two_pending_steps_on_one_field_are_representable() {
    let router = router();
    let req = three_field_req();
    let mut plan = plan(&router, &req, FieldSettlementMode::Settling);
    let row = *FieldRepairTable::default()
        .row_for_path(MAX_TOKENS)
        .expect("a grounded row");
    for ceiling in [2048, 1024] {
        plan.pending.push(RepairStep {
            row,
            edit: FieldEdit::Ceiling(ceiling),
            kind: super::RejectionKind::InvalidValue,
            tier: SignalTier::SelfIdentifying,
            fill_clamped: false,
            handle: None,
            reject_status: 400,
            guard: None,
        });
    }

    assert_eq!(plan.pending_paths(), vec![MAX_TOKENS, MAX_TOKENS]);
    assert_eq!(plan.commit_pending(&[], Instant::now()).len(), 2);
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
/// field's memory alone refuses the repair.
///
/// Mutation check: drop the committed-identity read in `repaired_field` -> red
/// (the committed ceiling clamps again and draws).
#[test]
fn a_committed_field_is_never_repaired_again_and_the_plan_stays_applied() {
    let router = router();
    let mut req = three_field_req();
    let mut plan = plan(&router, &req, FieldSettlementMode::NonSettling);
    let mut budget = RepairBudget::per_request();
    assert!(!plan.has_applied(), "premise: nothing applied yet");
    assert!(apply_ceiling(&mut plan, &mut req, &mut budget, 2048).is_some());
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
