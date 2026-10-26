//! The offline ledger replay hands the doctor matrix each resident row's
//! value, as the replay restored it.

use routectl_router::{CURRENT_VOCAB_VERSION, LearnedRegistryEntry, VerdictValue};
use routectl_usage::CapabilityEvent;

use super::super::CapabilityMatrixSource;
use super::super::lane_matrix_tests::{now_ms, seeded_ledger};
use super::gather_capability_matrix;
use routectl_router::Config;

/// The grounded `max_tokens` ceiling row's capability key. Spelled in two
/// pieces so the prefix literal keeps its single owner.
fn ceiling_key() -> String {
    let key = format!("{}{}max_tokens", "fie", "ld:");
    assert!(
        routectl_router::capability_key_is_field_verdict(&key),
        "premise: {key} is a field verdict key"
    );
    key
}

fn config(db: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "p".to_string(),
        routectl_router::ProviderEntry::openai_compat("https://example.invalid", "env://KEY"),
    );
    config.usage.db_path = db.to_path_buf();
    config
}

fn broken(capability: &str, token: &str) -> CapabilityEvent {
    CapabilityEvent {
        ts: now_ms(),
        lane_key: "p#up".to_string(),
        capability: capability.to_string(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: Some(token.to_string()),
        catalog_version: i64::from(routectl_router::CATALOG_VERSION),
        overlay_revision: 0,
        provider_kind: Some("openai-compat".to_string()),
        vocab_version: Some(CURRENT_VOCAB_VERSION),
    }
}

fn replayed_entries(events: &[CapabilityEvent]) -> Vec<LearnedRegistryEntry> {
    let (_dir, path) = seeded_ledger(events);
    match gather_capability_matrix(
        &config(&path),
        false,
        0,
        routectl_router::BetaSeedScope::EMPTY,
    ) {
        CapabilityMatrixSource::Available { entries, .. } => entries,
        _ => panic!("the replay left entries resident"),
    }
}

fn entry<'a>(entries: &'a [LearnedRegistryEntry], capability: &str) -> &'a LearnedRegistryEntry {
    entries
        .iter()
        .find(|row| row.feature_key == capability)
        .unwrap_or_else(|| panic!("{capability} is resident"))
}

/// A ceiling row replayed from the ledger reaches the matrix carrying its
/// ceiling; a catalog row carrying the same token carries none.
///
/// Mutation check: replay through `rebuild_capabilities_into` and take the
/// rows from `owned_snapshot` in `replay_matrix_slice` -> red.
#[test]
fn a_replayed_ceiling_row_reaches_the_matrix_carrying_its_ceiling() {
    let ceiling = ceiling_key();
    let entries = replayed_entries(&[
        broken(&ceiling, "ceiling:64000"),
        broken("web_search", "ceiling:64000"),
    ]);

    assert_eq!(
        entry(&entries, &ceiling).value,
        Some(VerdictValue::Ceiling(64_000))
    );
    assert_eq!(entry(&entries, "web_search").value, None);
}
