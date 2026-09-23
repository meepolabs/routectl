// The authorization-provenance rendering this feature's log lines share:
// the translation from a live authorization to its reportable record, and the
// closed-set rendering used at every emit site. `include!`d into
// field_preflight.rs; the types and imports it renders live there, so do not
// add `use` lines here.

/// The closed-set provenance record for the authorization that permitted an
/// action.
///
/// THE single translation from the authorization value to the reportable
/// record, so the acting-rewrite and canary-restoration records cannot spell it
/// two ways. Every field is rendered through the OWNING surface's token table
/// (`FailurePhase::as_str`, `EvidenceSource::as_str`,
/// `CanaryPosture::as_str`, the status module's canary-outcome table), so a new
/// variant on any of them is a compile error here rather than a silent token
/// change on an operator-facing line.
fn authorization_record(
    authorization: PreflightAuthorization,
) -> FieldPreflightAuthorizationRecord {
    FieldPreflightAuthorizationRecord {
        phase: authorization.phase.as_str(),
        source: authorization.source.as_str(),
        confirmations: authorization.confirmations,
        canary: authorization.canary.as_str(),
        canary_last_outcome: authorization
            .canary_last_outcome
            .map_or(super::fidelity_status::CANARY_OUTCOME_NONE, |outcome| {
                super::fidelity_status::canary_outcome_token(outcome)
            }),
    }
}

/// One decision's authorization provenance, rendered for a log line -- with the
/// absent case spelled explicitly rather than left as a missing field.
///
/// A struct rather than five separate `unwrap_or` calls at each emit site,
/// because there are three emit sites (acting DEBUG, non-acting DEBUG, request
/// WARN) and five fields: spelled per site, one site could render a different
/// absent token from another and an operator querying across tiers would see two
/// vocabularies for one state.
#[derive(Debug, Clone, Copy)]
struct RenderedAuthorization {
    phase: &'static str,
    source: &'static str,
    confirmations: u32,
    canary: &'static str,
    canary_last_outcome: &'static str,
}

impl RenderedAuthorization {
    /// The rendering of `authorization`, or the all-absent rendering for a
    /// decision that held none.
    ///
    /// The absent confirmation count renders as ZERO rather than a sentinel, and
    /// that is honest rather than lossy: a decision holding no authorization is
    /// one no acknowledged confirmation permitted, so zero is the count that
    /// authorized it. The companion `provenance_phase` token says which case a
    /// reader is looking at.
    const fn of(authorization: Option<FieldPreflightAuthorizationRecord>) -> Self {
        match authorization {
            Some(record) => Self {
                phase: record.phase,
                source: record.source,
                confirmations: record.confirmations,
                canary: record.canary,
                canary_last_outcome: record.canary_last_outcome,
            },
            None => Self {
                phase: PROVENANCE_NONE,
                source: PROVENANCE_NONE,
                confirmations: 0,
                canary: PROVENANCE_NONE,
                canary_last_outcome: PROVENANCE_NONE,
            },
        }
    }
}

/// Rendered provenance token for a decision that held no authorization. A
/// literal rather than an empty string, mirroring [`FIELD_PATH_NONE`], so the
/// line is unambiguous to a reader and uniform to a query.
const PROVENANCE_NONE: &str = "none";
