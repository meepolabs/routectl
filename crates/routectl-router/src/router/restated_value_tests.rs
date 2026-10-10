//! Which value a boundary restatement carries for a surviving entry: only a
//! field verdict whose repair row writes one, and only the value recorded for
//! the exact incarnation the cut snapshotted.

use super::super::*;

use std::sync::Arc;
use std::time::Instant;

use crate::capability_rebuild::{CapabilityEventRow, CapabilityLedgerReader, ReplayTombstone};
use crate::field_verdict::FieldVerdictKey;
use crate::field_verdict_value::VerdictValue;
use crate::learned_capability::RecordedLearnedEntry;

const KIND: &str = "openai-compat";
const LANE: &str = "alpha#model-x";

const ALPHA: &str = r#"
[providers.alpha]
kind = "openai-compat"
base_url = "https://alpha.example.test/v1"
api_key_ref = "literal:k"
"#;

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

fn field_key(path: &str) -> String {
    crate::field_capability::field_capability_key(path).expect("a qualified path")
}

fn broken(rowid: i64, capability: &str, token: Option<String>) -> CapabilityEventRow {
    CapabilityEventRow::new(
        rowid,
        Instant::now(),
        "broken".to_string(),
        Some("f1".to_string()),
        "live".to_string(),
        Some("self-identifying".to_string()),
        None,
        capability.to_string(),
        LANE.to_string(),
        KIND.to_string(),
        crate::catalog_baked::CATALOG_VERSION,
        0,
    )
    .with_vocab_version(Some(crate::capability_vocab::CURRENT_VOCAB_VERSION))
    .with_upstream_token(token)
}

const MEMBER: &str = crate::router::field_repair::TEST_REWRITE_ACCEPTED[1];

/// A router whose registry holds a rewrite verdict restored with [`MEMBER`],
/// a drop verdict, and a catalog verdict, all on [`LANE`].
fn router_with_three_verdicts() -> Router {
    let router = Router::new(Arc::new(toml::from_str(ALPHA).expect("valid test toml")));
    let summary = router.rebuild_learned_from_ledger(&Ledger(vec![
        broken(
            1,
            &field_key(crate::router::field_repair::TEST_REWRITE_PATH),
            Some(format!("shape:{MEMBER}")),
        ),
        broken(
            2,
            &field_key(crate::router::field_repair::TEST_UNSUPPORTED_PATH),
            None,
        ),
        broken(3, "web_search", None),
    ]));
    assert_eq!(summary.replayed_negative, 3, "premise: all three replayed");
    router
}

fn recorded(router: &Router, capability: &str) -> RecordedLearnedEntry {
    router
        .learned_capabilities
        .recorded_snapshot()
        .into_iter()
        .find(|row| row.entry.feature_key == capability)
        .expect("the verdict is resident")
}

/// Plant `value` in the value store for `recorded`'s identity at its own
/// incarnation -- the state a restatement would carry if it read the store
/// without consulting the repair row.
fn plant_value(router: &Router, recorded: &RecordedLearnedEntry, value: VerdictValue) {
    let key = FieldVerdictKey::from_capability_key(
        crate::state_key::StateKey::parse(LANE).expect("a lane"),
        recorded.entry.feature_key.clone(),
        recorded.provider_kind.clone(),
    );
    router
        .field_verdicts
        .seed_value_from_rebuild(&key, recorded.incarnation, Some(value));
}

/// Mutation check: make `with_restated_value` return `recorded` unchanged ->
/// red on the rewrite row.
#[test]
fn a_restated_rewrite_verdict_carries_the_value_of_its_snapshotted_incarnation() {
    let router = router_with_three_verdicts();
    let rewrite = recorded(
        &router,
        &field_key(crate::router::field_repair::TEST_REWRITE_PATH),
    );

    let restated = router.with_restated_value(rewrite);

    assert_eq!(restated.value, Some(VerdictValue::Shape(MEMBER)));
}

/// A value recorded for another lifecycle is not this one's: a snapshot whose
/// incarnation the store does not hold restates with no value.
///
/// Mutation check: read `latest` instead of `verdict_value` at the snapshotted
/// incarnation -> red.
#[test]
fn a_restatement_never_carries_a_value_recorded_for_another_incarnation() {
    let router = router_with_three_verdicts();
    let mut rewrite = recorded(
        &router,
        &field_key(crate::router::field_repair::TEST_REWRITE_PATH),
    );
    rewrite.incarnation += 1;

    let restated = router.with_restated_value(rewrite);

    assert_eq!(restated.value, None);
}

/// A drop row and a catalog row restate with no value even when the store
/// holds one for their identity at their own incarnation: the repair row
/// decides whether a value is carried at all, never the store.
///
/// Mutation check: drop the `value_slot_for_capability` gate in
/// `with_restated_value` -> red on both rows.
#[test]
fn drop_and_catalog_rows_restate_without_a_value_even_when_one_is_planted() {
    let router = router_with_three_verdicts();
    let planted = VerdictValue::Shape(MEMBER);
    let rows = [
        field_key(crate::router::field_repair::TEST_UNSUPPORTED_PATH),
        "web_search".to_string(),
    ];

    for capability in &rows {
        let row = recorded(&router, capability);
        plant_value(&router, &row, planted);
        assert_eq!(
            router.field_verdicts.verdict_value(
                &FieldVerdictKey::from_capability_key(
                    crate::state_key::StateKey::parse(LANE).expect("a lane"),
                    capability.clone(),
                    KIND.to_string(),
                ),
                row.incarnation,
            ),
            Some(planted),
            "control: the planted value is readable for {capability}",
        );

        let restated = router.with_restated_value(row);

        assert_eq!(restated.value, None, "{capability}");
    }
}
