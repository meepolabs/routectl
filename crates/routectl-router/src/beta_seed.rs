//! The shipped prior for client beta flags a Bedrock lane rejects.
//!
//! A prior consulted outside the learned registry, never an evidence source:
//! nothing here is written to the registry, the events ledger, or any doctor
//! surface. A learned verdict for the same lane and flag beats it, and an
//! operator override beats both.

/// The provider kind ([`crate::config::ProviderEntry::kind_str`]) the seed
/// applies to.
pub const BEDROCK_SEED_PROVIDER_KIND: &str = routectl_core::provider_kind::config::BEDROCK;

/// Client-lifted beta flags AWS Bedrock rejects outright: each one alone 400s
/// the whole request on every Claude model measured, so forwarding any of them
/// can never succeed.
pub const BEDROCK_BETA_SEED: &[&str] = &[
    "advanced-tool-use-2025-11-20",
    "advisor-tool-2026-03-01",
    "prompt-caching-scope-2026-01-05",
];

/// The shipped seed as the scope the learned registry bounds its seed-clear
/// markers by.
pub const fn shipped_scope() -> crate::beta_capability::BetaSeedScope {
    crate::beta_capability::BetaSeedScope::new(BEDROCK_SEED_PROVIDER_KIND, BEDROCK_BETA_SEED)
}

/// The seed's state on one `(lane, flag)` cell under `scope`: `None` when the
/// seed does not cover the lane's kind and the flag, `Cleared` when the
/// registry holds a seed-clear marker for the cell, else `Withhold`.
/// `marker_present` is only consulted for a covered cell.
pub fn seed_state(
    scope: crate::beta_capability::BetaSeedScope,
    provider_kind: &str,
    flag: &str,
    marker_present: impl FnOnce() -> bool,
) -> Option<crate::capability_precedence::SeedCell> {
    use crate::capability_precedence::SeedCell;
    if !scope.covers_flag(provider_kind, flag) {
        return None;
    }
    Some(if marker_present() {
        SeedCell::Cleared
    } else {
        SeedCell::Withhold
    })
}

/// [`seed_state`] for a capability key: `None` for a key outside the
/// beta-flag namespace, which no seed covers.
pub fn seed_state_for_key(
    scope: crate::beta_capability::BetaSeedScope,
    provider_kind: &str,
    capability_key: &str,
    marker_present: impl FnOnce() -> bool,
) -> Option<crate::capability_precedence::SeedCell> {
    let flag = crate::beta_capability::beta_flag_of(capability_key)?;
    seed_state(scope, provider_kind, flag, marker_present)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_precedence::SeedCell;

    const KIND: &str = "bedrock";
    const SEED: &[&str] = &["fx-seeded"];

    fn scope() -> crate::beta_capability::BetaSeedScope {
        crate::beta_capability::BetaSeedScope::new(KIND, SEED)
    }

    #[test]
    fn seed_state_reads_coverage_then_the_clear_marker() {
        let rows: &[(&str, &str, &str, bool, Option<SeedCell>)] = &[
            (
                "covered, no marker",
                KIND,
                "fx-seeded",
                false,
                Some(SeedCell::Withhold),
            ),
            (
                "covered, marker",
                KIND,
                "fx-seeded",
                true,
                Some(SeedCell::Cleared),
            ),
            ("uncovered kind", "anthropic-api", "fx-seeded", true, None),
            ("unseeded flag", KIND, "fx-other", true, None),
        ];
        for (name, kind, flag, marker, expected) in rows {
            assert_eq!(
                seed_state(scope(), kind, flag, || *marker),
                *expected,
                "{name}"
            );
        }
    }

    #[test]
    fn seed_state_for_key_reads_the_flag_inside_a_beta_key() {
        let seeded = crate::beta_capability::beta_capability_key("fx-seeded").expect("a flag");
        let rows: &[(&str, &str, bool, Option<SeedCell>)] = &[
            ("seeded beta key", &seeded, false, Some(SeedCell::Withhold)),
            (
                "seeded beta key, marker",
                &seeded,
                true,
                Some(SeedCell::Cleared),
            ),
            ("the bare flag is not a key", "fx-seeded", false, None),
            ("a feature key", "web_search", false, None),
        ];
        for (name, key, marker, expected) in rows {
            assert_eq!(
                seed_state_for_key(scope(), KIND, key, || *marker),
                *expected,
                "{name}"
            );
        }
    }

    #[test]
    fn seed_state_never_reads_the_marker_for_an_uncovered_cell() {
        let state = seed_state(scope(), "anthropic-api", "fx-seeded", || {
            panic!("marker read for an uncovered cell")
        });
        assert_eq!(state, None);
    }
}
