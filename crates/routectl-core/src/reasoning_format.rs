//! Reasoning-artifact format-tag vocabulary and the pure predicates every
//! consumer shares.
//!
//! [`crate::ReasoningDetail::format`] is BOTH a forever vocabulary and a
//! wire contract: it serializes outward to OpenAI-dialect clients and is
//! already in flight inside client histories. Tags are added, never
//! mutated or removed, and every reader stays open-set tolerant.
//!
//! Two separable facts live here:
//!
//! - A TAG records which lane produced an artifact. That is an
//!   observation, so it can never be wrong and never needs a migration.
//! - A [`ReplayScheme`] records which validator family the artifact
//!   belongs to. That is where proven-vs-unproven judgment lives, and it
//!   is revisable without touching the vocabulary.
//!
//! This module lives in `routectl-core` (alongside the `schema` types it
//! describes) rather than in `routectl-providers`, for the same reason
//! [`crate::CoreReasoningDialect`] does: the tag values ride on
//! `ReasoningDetail`, and the dep direction is providers -> core. The
//! providers-side `AuthKind` maps INTO [`ReplayScheme`] on its own side;
//! this module never names it.

/// Compatibility tag: RECOGNIZED FOREVER, NO LONGER EMITTED.
///
/// It conflated lanes whose replay validators reject each other's
/// artifacts, so a detail bearing it is genuinely ambiguous about which
/// lane produced it. Readers must keep accepting it (in-flight client
/// histories carry it), but it maps to [`ReplayScheme::Gray`] rather than
/// to any deterministic carry/strip rule -- which is precisely why it
/// cannot keep being emitted for any lane.
pub const OPENAI_RESPONSES_V1: &str = "openai-responses-v1";

/// Lane tag for artifacts produced by the codex OAuth lane.
pub const CODEX_OAUTH: &str = "codex-oauth";

/// Lane tag for artifacts produced by the OpenAI API-key lane.
pub const OPENAI_APIKEY: &str = "openai-apikey";

/// Lane tag for artifacts produced by the Bedrock mantle lane.
pub const BEDROCK_MANTLE: &str = "bedrock-mantle";

/// Every recognized Responses-family tag, oldest first.
const RESPONSES_FAMILY_TAGS: &[&str] = &[
    OPENAI_RESPONSES_V1,
    CODEX_OAUTH,
    OPENAI_APIKEY,
    BEDROCK_MANTLE,
];

/// Whether a format tag belongs to the Responses family.
///
/// Every tag comparison goes through this predicate, never through `==`
/// against a single constant: an exact-equality check silently DROPS a
/// newly-tagged detail instead of failing loudly, which is the specific
/// regression this vocabulary exists to prevent.
#[must_use]
pub fn is_responses_family(format: Option<&str>) -> bool {
    matches!(format, Some(tag) if RESPONSES_FAMILY_TAGS.contains(&tag))
}

/// Which validator family a reasoning artifact belongs to.
///
/// Replay portability is per-LANE, not per-model: the codex family
/// validates the reasoning item id and ignores the content, while the
/// mantle family validates the content prefix and ignores the id. Both
/// lanes mint identically shaped ids, so the id alone can never
/// discriminate -- the tag is the only signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayScheme {
    /// Id-validating family.
    Codex,
    /// Content-prefix-validating family.
    Mantle,
    /// Scheme not established: an untagged detail, the ambiguous
    /// compatibility tag, or a tag this build does not recognize.
    Gray,
}

/// Map a format tag to its validator family.
///
/// Unknown and absent tags land in [`ReplayScheme::Gray`] rather than
/// erroring: the vocabulary is open-set, so a build older than the tag it
/// is reading must degrade, never reject.
#[must_use]
pub fn scheme_of(format: Option<&str>) -> ReplayScheme {
    match format {
        Some(CODEX_OAUTH | OPENAI_APIKEY) => ReplayScheme::Codex,
        Some(BEDROCK_MANTLE) => ReplayScheme::Mantle,
        _ => ReplayScheme::Gray,
    }
}

/// Verdict on replaying an artifact of one scheme onto a lane of another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replayability {
    /// Proven-compatible pair: send the artifact as-is.
    Carry,
    /// Proven-incompatible pair: drop the artifact before dispatch.
    Strip,
    /// Not established either way.
    Gray,
}

/// Deterministic replay verdict for a `(detail scheme, lane scheme)` pair.
///
/// Only PROVEN pairs get a deterministic answer. Anything touching
/// [`ReplayScheme::Gray`] stays gray so the optimistic carry-once path
/// can run and the learned layer can settle the pair from a real
/// upstream verdict -- guessing here would bake an unproven distinction
/// into the deterministic rules.
#[must_use]
pub fn is_replayable(detail: ReplayScheme, lane: ReplayScheme) -> Replayability {
    match (detail, lane) {
        (ReplayScheme::Gray, _) | (_, ReplayScheme::Gray) => Replayability::Gray,
        (a, b) if a == b => Replayability::Carry,
        _ => Replayability::Strip,
    }
}

#[cfg(test)]
#[path = "reasoning_format_tests.rs"]
mod tests;
