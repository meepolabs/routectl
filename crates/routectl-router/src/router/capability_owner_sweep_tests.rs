//! Learned history follows the provider entry that owns a lane, across a hot
//! reload and across a restart: a nickname rename keeps it, a model moved to
//! another entry starts fresh, and an entry that is removed or changes kind
//! takes its history with it.

use super::super::*;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use futures::stream::BoxStream;
use routectl_core::capability::{FailurePhase, SignalTier};
use routectl_core::failure_class::FailureClass;
use routectl_core::{ChatChunk, ChatResponse, Provider, Result, ToolDef};
use serde_json::json;

use crate::capability_rebuild::{CapabilityEventRow, CapabilityLedgerReader, ReplayTombstone};
use crate::learned_capability::RoutingDecision;
use crate::resolved::ResolvedModel;

struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("stub", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::upstream("stub", 500, "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("stub", 500, "unused"))
    }
}

const COMPAT: &str = "openai-compat";
const ANTHROPIC: &str = "anthropic-api";
const UPSTREAM: &str = "model-x";
const LANE: &str = "alpha#model-x";

const ALPHA_COMPAT: &str = r#"
[providers.alpha]
kind = "openai-compat"
base_url = "https://alpha.example.test/v1"
api_key_ref = "literal:k"
"#;

const ALPHA_ANTHROPIC: &str = r#"
[providers.alpha]
kind = "anthropic-api"
api_key_ref = "literal:k"
"#;

const BETA_COMPAT: &str = r#"
[providers.beta]
kind = "openai-compat"
base_url = "https://beta.example.test/v1"
api_key_ref = "literal:k"
"#;

const ALPHA_REPOINTED: &str = r#"
[providers.alpha]
kind = "openai-compat"
base_url = "https://elsewhere.example.test/v1"
api_key_ref = "literal:k"
"#;

fn router(providers: &str) -> Router {
    Router::new(Arc::new(
        toml::from_str(providers).expect("valid test toml"),
    ))
}

fn target(router: &Router, nickname: &str, provider: &str) -> DispatchTarget {
    let p: Arc<dyn Provider> = Arc::new(StubProvider);
    let model = ResolvedModel::new(nickname, provider, p, UPSTREAM);
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

/// Mint a self-identifying web_search negative through the production learn
/// path on `target`, recorded under `kind`.
fn learn_on(router: &Router, target: &DispatchTarget, kind: &'static str) {
    let err = Error::upstream_full("stub", 400, "{}".to_string(), None, None, None);
    let req = ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({ "type": "web_search" }))]),
        ..Default::default()
    };
    router.commit_learned_observation(
        (
            "web_search".to_string(),
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
        ),
        &FailureClass::BadRequest,
        &err,
        400,
        None,
        kind,
        target,
        &req,
        false,
        &mut HashSet::new(),
        &mut DispatchMeta::for_alias("m"),
        &mut LearnedProbeGuard::inert(),
    );
}

fn routes_away(router: &Router, target: &DispatchTarget, kind: &str) -> bool {
    let key = target
        .learned_key("web_search")
        .expect("the target has a lane");
    matches!(
        router
            .acting_negative_with_generation(key, "web_search", kind, Instant::now())
            .0,
        RoutingDecision::RouteAway { .. }
    )
}

/// Every resident version's lane, whichever kind wrote it: the registry the
/// next sweep judges, not only the versions this Router reads.
fn resident_lanes(router: &Router) -> Vec<String> {
    router
        .learned_capabilities
        .recorded_snapshot()
        .into_iter()
        .map(|recorded| recorded.entry.state_key)
        .collect()
}

/// Every resident version's recorded kind, sorted.
fn resident_kinds(router: &Router) -> Vec<String> {
    let mut kinds: Vec<String> = router
        .learned_capabilities
        .recorded_snapshot()
        .into_iter()
        .map(|recorded| recorded.provider_kind)
        .collect();
    kinds.sort();
    kinds
}

/// A restart's view of the ledger: one boundary and the rows after it.
struct Ledger(Vec<CapabilityEventRow>);

impl CapabilityLedgerReader for Ledger {
    fn tombstone(&self) -> Option<ReplayTombstone> {
        Some(ReplayTombstone::new(
            0,
            crate::catalog_baked::CATALOG_VERSION,
            0,
        ))
    }

    fn read_events(&self) -> Vec<CapabilityEventRow> {
        self.0.clone()
    }
}

/// The persisted shape of the negative `learn_on` mints, on `lane`, recorded
/// under `kind`.
fn persisted(rowid: i64, lane: &str, kind: &str) -> CapabilityEventRow {
    CapabilityEventRow::new(
        rowid,
        Instant::now(),
        "broken".to_string(),
        Some("f1".to_string()),
        "live".to_string(),
        Some("self-identifying".to_string()),
        None,
        "web_search".to_string(),
        lane.to_string(),
        kind.to_string(),
        crate::catalog_baked::CATALOG_VERSION,
        0,
    )
    .with_vocab_version(Some(crate::capability_vocab::CURRENT_VOCAB_VERSION))
}

#[test]
fn a_nickname_rename_keeps_learned_history_across_reload_and_restart() {
    let before = router(ALPHA_COMPAT);
    learn_on(&before, &target(&before, "opus", "alpha"), COMPAT);

    let mut reloaded = router(ALPHA_COMPAT);
    reloaded.carry_over_learned_from(&before);
    let renamed = target(&reloaded, "opus-renamed", "alpha");
    assert!(
        routes_away(&reloaded, &renamed, COMPAT),
        "the renamed nickname reads the history across the reload",
    );

    let restarted = router(ALPHA_COMPAT);
    let summary = restarted.rebuild_learned_from_ledger(&Ledger(vec![persisted(1, LANE, COMPAT)]));
    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(summary.skipped_owner, 0);
    assert!(
        routes_away(
            &restarted,
            &target(&restarted, "opus-renamed", "alpha"),
            COMPAT
        ),
        "the renamed nickname reads the history across the restart",
    );
}

#[test]
fn a_same_kind_repoint_keeps_learned_history_across_reload_and_restart() {
    let before = router(ALPHA_COMPAT);
    learn_on(&before, &target(&before, "opus", "alpha"), COMPAT);

    let mut reloaded = router(ALPHA_REPOINTED);
    reloaded.carry_over_learned_from(&before);
    assert_eq!(resident_lanes(&reloaded), vec![LANE.to_string()]);

    let restarted = router(ALPHA_REPOINTED);
    let summary = restarted.rebuild_learned_from_ledger(&Ledger(vec![persisted(1, LANE, COMPAT)]));
    assert_eq!(summary.replayed_negative, 1);
}

#[test]
fn a_model_moved_to_another_provider_entry_starts_fresh() {
    let both = format!("{ALPHA_COMPAT}{BETA_COMPAT}");
    let before = router(&both);
    learn_on(&before, &target(&before, "opus", "alpha"), COMPAT);

    let mut reloaded = router(&both);
    reloaded.carry_over_learned_from(&before);
    assert!(
        !routes_away(&reloaded, &target(&reloaded, "opus", "beta"), COMPAT),
        "the old entry's negative must not act on the new target after a reload",
    );

    let restarted = router(&both);
    let summary = restarted.rebuild_learned_from_ledger(&Ledger(vec![persisted(1, LANE, COMPAT)]));
    assert_eq!(
        summary.replayed_negative, 1,
        "the fact stays on its own lane"
    );
    assert!(
        !routes_away(&restarted, &target(&restarted, "opus", "beta"), COMPAT),
        "the old entry's negative must not act on the new target after a restart",
    );
}

#[test]
fn a_kind_flip_drops_the_lane_at_reload_and_skips_it_at_replay() {
    let before = router(ALPHA_COMPAT);
    learn_on(&before, &target(&before, "opus", "alpha"), COMPAT);
    assert_eq!(resident_lanes(&before), vec![LANE.to_string()]);

    let mut reloaded = router(ALPHA_ANTHROPIC);
    reloaded.carry_over_learned_from(&before);
    assert!(
        resident_lanes(&reloaded).is_empty(),
        "the reload drops the entry its kind-flipped owner left behind",
    );
    assert!(!routes_away(
        &reloaded,
        &target(&reloaded, "opus", "alpha"),
        ANTHROPIC
    ));

    let restarted = router(ALPHA_ANTHROPIC);
    let summary = restarted.rebuild_learned_from_ledger(&Ledger(vec![
        persisted(1, LANE, COMPAT),
        persisted(2, LANE, COMPAT),
    ]));
    assert_eq!(summary.skipped_owner, 2);
    assert_eq!(summary.replayed_negative, 0);
    assert!(resident_lanes(&restarted).is_empty());
}

#[test]
fn removing_then_re_adding_an_entry_of_another_kind_does_not_resurrect_its_state() {
    let before = router(ALPHA_COMPAT);
    learn_on(&before, &target(&before, "opus", "alpha"), COMPAT);

    let mut removed = router(BETA_COMPAT);
    removed.carry_over_learned_from(&before);
    assert!(
        resident_lanes(&removed).is_empty(),
        "removing the entry drops its lane"
    );

    let mut re_added = router(ALPHA_ANTHROPIC);
    re_added.carry_over_learned_from(&removed);
    assert!(resident_lanes(&re_added).is_empty());
    assert!(!routes_away(
        &re_added,
        &target(&re_added, "opus", "alpha"),
        ANTHROPIC
    ));

    let restarted = router(ALPHA_ANTHROPIC);
    let summary = restarted.rebuild_learned_from_ledger(&Ledger(vec![persisted(1, LANE, COMPAT)]));
    assert_eq!(summary.skipped_owner, 1);
    assert!(resident_lanes(&restarted).is_empty());
}

#[test]
fn the_owner_sweep_resets_a_dropped_field_verdict_canary() {
    let before = router(ALPHA_COMPAT);
    let lane = crate::state_key::StateKey::parse(LANE).expect("a lane");
    let key = crate::field_verdict::FieldVerdictKey::new(&lane, "thinking.enabled.display", COMPAT)
        .expect("a qualified dotted path on a pass-through kind");
    let field_key = crate::field_capability::field_capability_key("thinking.enabled.display")
        .expect("a qualified dotted path mints a key");
    before.learned_capabilities.observe(
        LANE,
        &field_key,
        COMPAT,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        routectl_core::capability::EvidenceSource::Live,
        None,
        Instant::now(),
    );
    before
        .field_verdicts
        .canaries()
        .seed_from_rebuild(&key, 1, 1, true);
    assert!(before.field_verdicts.canaries().snapshot(&key).is_some());

    let mut reloaded = router(ALPHA_ANTHROPIC);
    reloaded.carry_over_learned_from(&before);

    assert!(resident_lanes(&reloaded).is_empty());
    assert!(
        reloaded.field_verdicts.canaries().snapshot(&key).is_none(),
        "a dropped field verdict must not leave canary state for a relearn to inherit",
    );
}

#[test]
fn an_old_router_write_after_a_kind_flip_does_not_act_on_the_new_router() {
    let before = router(ALPHA_COMPAT);
    let old_target = target(&before, "opus", "alpha");
    learn_on(&before, &old_target, COMPAT);

    let mut reloaded = router(ALPHA_ANTHROPIC);
    reloaded.carry_over_learned_from(&before);
    assert!(resident_lanes(&reloaded).is_empty());

    learn_on(&before, &old_target, COMPAT);
    assert_eq!(
        resident_lanes(&reloaded),
        vec![LANE.to_string()],
        "the in-flight write through the old router lands in the shared registry",
    );

    assert!(
        !routes_away(&reloaded, &target(&reloaded, "opus", "alpha"), ANTHROPIC),
        "an entry recorded under the old kind must not act for the new kind",
    );
    assert!(
        reloaded.learned_capability_snapshot().is_empty(),
        "the new router's display surfaces do not report the old kind's version",
    );
    assert_eq!(
        resident_lanes(&reloaded),
        vec![LANE.to_string()],
        "a read removes nothing; removal belongs to the owner sweep",
    );
    assert_eq!(reloaded.sweep_unowned_learned_entries(), 1);
    assert!(resident_lanes(&reloaded).is_empty());
}

/// The reload race: after an A->B kind flip, B learns a fact, then requests
/// still holding the A router look up and write the same lane. B keeps acting
/// on its own entry, untouched; A's version is resident but never acts for B;
/// and the next sweep under B's config removes only A's version.
#[test]
fn a_late_old_router_lookup_and_write_leave_the_new_kinds_fact_acting() {
    let before = router(ALPHA_COMPAT);
    let old_target = target(&before, "opus", "alpha");
    let mut reloaded = router(ALPHA_ANTHROPIC);
    reloaded.carry_over_learned_from(&before);
    let new_target = target(&reloaded, "opus", "alpha");
    learn_on(&reloaded, &new_target, ANTHROPIC);
    let learned = reloaded.learned_capability_snapshot();

    assert!(
        !routes_away(&before, &old_target, COMPAT),
        "premise: the old router finds nothing of its own kind",
    );
    learn_on(&before, &old_target, COMPAT);
    assert!(
        routes_away(&before, &old_target, COMPAT),
        "the old router acts on its own late write",
    );

    assert!(routes_away(&reloaded, &new_target, ANTHROPIC));
    assert_eq!(
        reloaded.learned_capability_snapshot(),
        learned,
        "the new kind's entry is intact and is the only one the new router reports",
    );
    assert_eq!(
        resident_kinds(&reloaded),
        vec![ANTHROPIC.to_string(), COMPAT.to_string()],
    );

    assert_eq!(reloaded.sweep_unowned_learned_entries(), 1);
    assert_eq!(resident_kinds(&reloaded), vec![ANTHROPIC.to_string()]);
    assert!(routes_away(&reloaded, &new_target, ANTHROPIC));
}

/// The same race resolved by the next reload rather than a direct sweep: the
/// carry-over onto a config that still configures the new kind removes the
/// stray old-kind version.
#[test]
fn the_next_reload_removes_a_late_old_kind_version() {
    let before = router(ALPHA_COMPAT);
    let old_target = target(&before, "opus", "alpha");
    let mut reloaded = router(ALPHA_ANTHROPIC);
    reloaded.carry_over_learned_from(&before);
    learn_on(&reloaded, &target(&reloaded, "opus", "alpha"), ANTHROPIC);
    learn_on(&before, &old_target, COMPAT);
    assert_eq!(resident_kinds(&reloaded).len(), 2, "premise: both versions");

    let mut next = router(ALPHA_ANTHROPIC);
    next.carry_over_learned_from(&reloaded);

    assert_eq!(resident_kinds(&next), vec![ANTHROPIC.to_string()]);
    assert!(routes_away(
        &next,
        &target(&next, "opus", "alpha"),
        ANTHROPIC
    ));
}

#[test]
fn an_old_router_write_after_a_same_kind_reload_still_acts_on_the_new_router() {
    let before = router(ALPHA_COMPAT);
    let old_target = target(&before, "opus", "alpha");
    learn_on(&before, &old_target, COMPAT);

    let mut reloaded = router(ALPHA_REPOINTED);
    reloaded.carry_over_learned_from(&before);
    learn_on(&before, &old_target, COMPAT);

    assert!(
        routes_away(&reloaded, &target(&reloaded, "opus", "alpha"), COMPAT),
        "a same-kind owner keeps acting on what the old router learned",
    );
    assert_eq!(resident_lanes(&reloaded), vec![LANE.to_string()]);
}

#[test]
fn a_write_under_the_new_kind_does_not_refresh_an_entry_recorded_under_the_old_kind() {
    let before = router(ALPHA_COMPAT);
    let old_target = target(&before, "opus", "alpha");
    let mut reloaded = router(ALPHA_ANTHROPIC);
    reloaded.carry_over_learned_from(&before);
    learn_on(&before, &old_target, COMPAT);

    let new_target = target(&reloaded, "opus", "alpha");
    learn_on(&reloaded, &new_target, ANTHROPIC);

    let snapshot = reloaded.learned_capability_snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(
        snapshot[0].observations, 1,
        "the new owner's first observation, not a refresh of the old owner's",
    );
    assert!(
        routes_away(&reloaded, &new_target, ANTHROPIC),
        "the new owner's own observation acts",
    );
}

#[test]
fn a_delayed_write_from_the_original_owner_survives_a_flip_and_flip_back() {
    let original = router(ALPHA_COMPAT);
    let original_target = target(&original, "opus", "alpha");

    let mut flipped = router(ALPHA_ANTHROPIC);
    flipped.carry_over_learned_from(&original);
    learn_on(&original, &original_target, COMPAT);
    assert_eq!(
        resident_lanes(&flipped),
        vec![LANE.to_string()],
        "premise: the original router's in-flight write lands after the first sweep",
    );

    let mut restored = router(ALPHA_COMPAT);
    restored.carry_over_learned_from(&flipped);

    assert_eq!(
        resident_lanes(&restored),
        vec![LANE.to_string()],
        "the entry was written under the kind the restored config has again",
    );
    assert!(
        routes_away(&restored, &target(&restored, "opus", "alpha"), COMPAT),
        "the original owner's fact acts once its kind is back",
    );
}
