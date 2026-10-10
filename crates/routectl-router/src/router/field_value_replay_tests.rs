//! Value-aware warm rebuild: which persisted value tokens replay reads, how a
//! refused one degrades, and how a value change splits a verdict's history.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::capability_rebuild::{
    CapabilityEventRow, CapabilityLedgerReader, CapabilityRebuildSummary, ReplayTombstone,
    ReplayedFieldValues, rebuild_capabilities_with_values,
};
use crate::field_verdict_value::VerdictValue;
use crate::learned_capability::{DEFAULT_MAX_ENTRIES, LearnedCapabilityRegistry, RoutingDecision};

const CV: u32 = 7;
const OV: u64 = 42;
const LANE: &str = "nn#upstream";

struct FakeReader {
    rows: Vec<CapabilityEventRow>,
}

impl CapabilityLedgerReader for FakeReader {
    fn tombstone(&self) -> Option<ReplayTombstone> {
        Some(ReplayTombstone::new(0, CV, OV))
    }

    fn read_events(&self) -> Vec<CapabilityEventRow> {
        self.rows.clone()
    }
}

/// A `broken` (F1 self-identifying, live) row on [`LANE`].
fn broken(rowid: i64, at: Instant, capability: &str) -> CapabilityEventRow {
    CapabilityEventRow::new(
        rowid,
        at,
        "broken".to_string(),
        Some("f1".to_string()),
        "live".to_string(),
        Some("self-identifying".to_string()),
        None,
        capability.to_string(),
        LANE.to_string(),
        "openai-compat".to_string(),
        CV,
        OV,
    )
    .with_vocab_version(Some(crate::capability_vocab::CURRENT_VOCAB_VERSION))
}

fn providers() -> BTreeMap<String, crate::config::ProviderEntry> {
    BTreeMap::from([(
        "nn".to_string(),
        crate::config::ProviderEntry::openai_compat("https://nn.example.test/v1", "literal:k"),
    )])
}

fn registry() -> LearnedCapabilityRegistry {
    LearnedCapabilityRegistry::new(
        Duration::from_hours(1),
        Duration::from_mins(1),
        DEFAULT_MAX_ENTRIES,
    )
}

// ---------------------------------------------------------------------------
// Field verdict values
// ---------------------------------------------------------------------------

fn field_key(path: &str) -> String {
    crate::field_capability::field_capability_key(path).expect("a qualified path")
}

/// A `broken` field row on [`LANE`] carrying `token` as its persisted value.
fn valued(rowid: i64, at: Instant, path: &str, token: Option<&str>) -> CapabilityEventRow {
    broken(rowid, at, &field_key(path)).with_upstream_token(token.map(str::to_string))
}

fn rebuild_values(
    rows: Vec<CapabilityEventRow>,
) -> (CapabilityRebuildSummary, ReplayedFieldValues) {
    let reader = FakeReader { rows };
    rebuild_capabilities_with_values(&reader, &registry(), &providers())
}

fn value_of(values: &ReplayedFieldValues, capability: &str) -> Option<VerdictValue> {
    values.value_for(LANE, capability, "openai-compat")
}

/// A persisted token is read ONLY for a field row whose code-owned repair row
/// writes a value. The same well-formed tokens planted on a catalog row and on
/// a drop row are ignored -- no value, no refusal counted -- while the value
/// rows beside them (rewrite and ceiling) restore theirs.
///
/// Mutation check: give `FieldTransform::Drop` a value slot (`ValueSlot::Ceiling`)
/// in `FieldTransform::value_slot` -> red (the drop row restores `Ceiling(5)`).
#[test]
fn a_value_token_is_read_only_for_a_row_whose_transform_writes_one() {
    let base = Instant::now();
    let rewrite_member = super::field_repair::TEST_REWRITE_ACCEPTED[1];
    let rewrite_token = format!("shape:{rewrite_member}");
    let rows = vec![
        broken(1, base, "web_search").with_upstream_token(Some("ceiling:5".to_string())),
        valued(
            2,
            base,
            super::field_repair::TEST_UNSUPPORTED_PATH,
            Some("ceiling:5"),
        ),
        valued(
            3,
            base,
            super::field_repair::TEST_REWRITE_PATH,
            Some(&rewrite_token),
        ),
        valued(
            4,
            base,
            super::field_repair::TEST_CEILING_PATH,
            Some("ceiling:5"),
        ),
    ];

    let (summary, values) = rebuild_values(rows);

    assert_eq!(summary.replayed_negative, 4, "every row replays");
    assert_eq!(summary.degraded_value, 0, "no token was refused");
    assert_eq!(
        value_of(&values, "web_search"),
        None,
        "a catalog row's token is ignored"
    );
    assert_eq!(
        value_of(
            &values,
            &field_key(super::field_repair::TEST_UNSUPPORTED_PATH)
        ),
        None,
        "a drop row's token is ignored",
    );
    assert_eq!(
        value_of(&values, &field_key(super::field_repair::TEST_REWRITE_PATH)),
        Some(VerdictValue::Shape(rewrite_member)),
        "positive control: a rewrite row restores its value",
    );
    assert_eq!(
        value_of(&values, &field_key(super::field_repair::TEST_CEILING_PATH)),
        Some(VerdictValue::Ceiling(5)),
        "positive control: the same token on a ceiling row is read",
    );
}

/// A token the strict decode refuses degrades its row to a plain negative
/// without a value: the verdict is still resident and acting, but carries
/// nothing to apply, and the refusal is counted on the summary and on the
/// codec's own counter.
///
/// Mutation check: make `decode_row_value` skip the `degraded_value` bump ->
/// red.
#[test]
fn a_refused_value_token_replays_as_a_plain_negative_and_is_counted() {
    let base = Instant::now();
    let path = super::field_repair::TEST_REWRITE_PATH;
    let refused_before = VerdictValue::malformed_total();
    let reader = FakeReader {
        rows: vec![valued(1, base, path, Some("shape:not-a-member"))],
    };
    let reg = registry();

    let (summary, values) = rebuild_capabilities_with_values(&reader, &reg, &providers());

    assert_eq!(summary.replayed_negative, 1, "the row still replays");
    assert_eq!(summary.degraded_value, 1, "the refusal is counted");
    assert!(
        VerdictValue::malformed_total() > refused_before,
        "the codec counted it"
    );
    assert_eq!(
        value_of(&values, &field_key(path)),
        None,
        "and it carries no value"
    );
    assert!(
        matches!(
            reg.acting_negative_for(LANE, &field_key(path), "openai-compat", base),
            RoutingDecision::RouteAway { .. }
        ),
        "the plain negative is acting",
    );
}

/// The ledger's history of one identity corroborates only while its value
/// holds: a repeat of the same value adds an observation, a different value
/// starts the verdict over from one, and the identity then carries the newer
/// value.
///
/// Mutation check: remove the `remove_keyed` call in `reconcile_field_value`
/// -> red (the changed value inherits three observations).
#[test]
fn replay_corroborates_only_rows_carrying_the_same_value() {
    let base = Instant::now();
    let path = super::field_repair::TEST_REWRITE_PATH;
    let [first, second] = [
        super::field_repair::TEST_REWRITE_ACCEPTED[0],
        super::field_repair::TEST_REWRITE_ACCEPTED[1],
    ];
    let first_token = format!("shape:{first}");
    let second_token = format!("shape:{second}");
    let observations = |rows: Vec<CapabilityEventRow>| {
        let reader = FakeReader { rows };
        let reg = registry();
        let (_, values) = rebuild_capabilities_with_values(&reader, &reg, &providers());
        let resident = reg.snapshot();
        assert_eq!(resident.len(), 1, "one identity is resident");
        (
            resident[0].observations,
            value_of(&values, &field_key(path)),
        )
    };

    let same = observations(vec![
        valued(1, base, path, Some(&first_token)),
        valued(2, base, path, Some(&first_token)),
    ]);
    let changed = observations(vec![
        valued(1, base, path, Some(&first_token)),
        valued(2, base, path, Some(&first_token)),
        valued(3, base, path, Some(&first_token)),
        valued(4, base, path, Some(&second_token)),
    ]);

    assert_eq!(
        same,
        (2, Some(VerdictValue::Shape(first))),
        "same value: +1"
    );
    assert_eq!(
        changed,
        (1, Some(VerdictValue::Shape(second))),
        "new value: restarts at one"
    );
}
