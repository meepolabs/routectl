//! The catalog-constrained output profile a paid-probe body would be sized
//! from: whether this build may size one at all, and what the smallest viable
//! output allowance is.
//!
//! # Why the resolved model's own effective row
//!
//! The question "may a paid call be sized for this identity" is answered from
//! the row the chain already resolved for that identity's model
//! (`ResolvedModel::effective_row`), never from a provider-name table and never
//! from a per-provider spend profile. A name table would have to be kept in step
//! with the catalog by hand, and would answer for models the catalog does not
//! price and decline for models it does. The row is the same merge result every
//! other pricing surface reads, so this stage cannot admit a cell the economics
//! view calls unpriced.
//!
//! # Fail closed, and closed means no smaller body
//!
//! A profile exists only when the row is PRESENT and confirms all four facts a
//! priced call needs: a finite positive input rate, a finite positive output
//! rate, a positive output ceiling, and a ceiling at or above the wire shape's
//! own minimum viable allowance. Anything else -- missing, disabled, unpriced in
//! either dimension, zero, negative, non-finite, no ceiling, or a ceiling below
//! the floor -- yields no profile, which is what leaves the paid path unable to
//! size a body at all.
//!
//! The ceiling comparison is a VIABILITY constraint, not a value to clamp
//! toward. A cell whose confirmed ceiling sits below the floor cannot carry the
//! field under test in the first place, so there is no smaller body to fall back
//! to; clamping below the floor would ship an allowance the serializer answers
//! by dropping the very field the probe exists to ask about.
//!
//! # Where the two floors come from
//!
//! Both are properties of the request serializer, pinned against it in the
//! sidecar rather than asserted here. The legacy thinking shape couples the
//! thinking budget to the output allowance, so the smallest allowance that keeps
//! the field on the wire is one token above the budget floor; the adaptive shape
//! has no such coupling, so the smallest POSITIVE allowance serves. This build
//! makes no claim about what the serializer does at a zero allowance -- the
//! derivation simply cannot produce one, and the sidecar pins that.

use super::Router;
use crate::catalog::CatalogRow;
use crate::field_verdict::FieldVerdictKey;

/// Smallest `max_tokens` that keeps the grounded field on the wire under the
/// LEGACY thinking shape.
///
/// The serializer drops the whole thinking object unless the output allowance
/// exceeds the budget floor, because that shape requires a budget at the floor
/// PLUS at least one visible output token. So this is the budget floor plus one:
/// one token below it, the body carries no thinking object and the probe asks
/// the upstream nothing about the field.
///
/// Welded to the serializer rather than to its constant: the providers crate
/// keeps its budget floor private, and duplicating the number would produce two
/// values that can disagree silently. What pins this one is a pair of real
/// serializer cases -- the field present at this value, the whole object absent
/// one below -- so a serializer change moves a test rather than leaving a stale
/// literal here.
pub(super) const LEGACY_MIN_VIABLE_MAX_TOKENS: u32 = 1025;

/// Smallest `max_tokens` that keeps the grounded field on the wire under the
/// ADAPTIVE thinking shape.
///
/// The adaptive shape carries no budget field, so nothing couples the field's
/// presence to the size of the output allowance and the smallest POSITIVE one
/// serves. Positive rather than zero deliberately: a zero allowance is not a
/// shape this build has established anything about, and the derivation is where
/// that is enforced -- no admitted cell can size a body at zero.
pub(super) const ADAPTIVE_MIN_VIABLE_MAX_TOKENS: u32 = 1;

/// A paid-probe output profile: the sized allowance, plus the catalog
/// confirmation it was admitted against.
///
/// Small, immutable, and `Copy`: two counts and nothing borrowed, so a later
/// authorization stage can hold one across an await without keeping the resolved
/// model alive. It carries no provider name, no model id, and no rate -- an
/// authorization decision needs to know a body CAN be sized and how large, and
/// a dollar figure or a vendor identity here would be state this stage does not
/// own and a log line would eventually leak.
///
/// Constructible only by [`Router::paid_probe_profile`], which is what makes the
/// existence of a value evidence that every catalog fact was confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PaidProbeProfile {
    max_tokens: u32,
    output_ceiling_tokens: u32,
}

impl PaidProbeProfile {
    /// The `max_tokens` a paid probe body would carry: the smallest allowance
    /// this identity's wire shape can keep the field under test on.
    ///
    /// Read by the paid dial when it builds the one authorized request.
    pub(super) const fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    /// The EFFECTIVE output ceiling this profile was admitted against: the lower
    /// of the catalog's confirmed ceiling and the operator's configured one.
    ///
    /// Retained because a later authorization stage needs the CONFIRMATION, not
    /// just the size: the effective ceiling is the fact that made the allowance
    /// viable, so a stage that re-derived a ceiling of its own could authorize
    /// against a different one than the derivation admitted -- and reading only
    /// the catalog half is exactly how it would authorize past an operator cap.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn output_ceiling_tokens(&self) -> u32 {
        self.output_ceiling_tokens
    }
}

impl Router {
    /// The paid-probe output profile for `key`, or `None` when this build may
    /// not size a paid body for that identity.
    ///
    /// Resolves the seat through the EXISTING exact resolved-model/seat lookup
    /// (`Self::probe_seat_for`), so a pooled identity reaches its own member's
    /// model by recomposing and comparing canonical state keys -- never by
    /// splitting a composite key on a separator, which no delimiter choice
    /// parses soundly, and never under an AMBIGUOUS key, which that lookup
    /// refuses outright.
    ///
    /// Called by paid-probe authorization before reservation and dispatch. A
    /// missing or ambiguous profile returns `None`, so no paid call is authorized.
    pub(super) fn paid_probe_profile(&self, key: &FieldVerdictKey) -> Option<PaidProbeProfile> {
        let seat = self.probe_seat_for(key)?;
        // The ceiling read goes through the shared accessor, which already
        // degrades a zero to unconfirmed -- so this stage cannot admit a ceiling
        // the factory's own fill would decline.
        let catalog_ceiling = seat.effective_row.output_ceiling_tokens()?;
        let row = seat.effective_row.priced()?;
        if !prices_both_dimensions(row) {
            return None;
        }
        // THE LANE GATE, and it sits here rather than at a caller because the
        // next decision is serializer-specific: the two floors below describe
        // ONE egress's wire shapes, so applying either to a lane with a
        // different serializer would assert something this build has not
        // established. An absent entry (removed by a reload) is not that lane's
        // kind either, so it declines with everything else.
        if seat.provider_kind != Some(super::field_repair::ANTHROPIC_API_KIND) {
            return None;
        }
        let max_tokens = if seat.supports_adaptive_thinking {
            ADAPTIVE_MIN_VIABLE_MAX_TOKENS
        } else {
            LEGACY_MIN_VIABLE_MAX_TOKENS
        };
        let output_ceiling_tokens =
            effective_output_ceiling(catalog_ceiling, seat.configured_output_ceiling);
        // VIABILITY, not a clamp: a ceiling below the shape's floor cannot carry
        // the field at all, and a body sized under the floor would ask nothing.
        if output_ceiling_tokens < max_tokens {
            return None;
        }
        Some(PaidProbeProfile {
            max_tokens,
            output_ceiling_tokens,
        })
    }
}

/// The ceiling a paid body is actually bound by: the LOWER of the catalog's
/// confirmed ceiling and the operator's configured one.
///
/// Precedence, stated because both directions matter. A configured ceiling
/// LOWERS the effective one, because the operator's `max_output_tokens` binds
/// the very request a probe would send -- ignoring it would size a body the
/// operator capped below. It never RAISES it, because the catalog ceiling is a
/// vendor fact an operator cannot opt out of by writing a larger number.
///
/// `0` is the production sentinel for "no operator override" on the resolved
/// model, never a real ceiling of zero, so it leaves the catalog ceiling
/// untouched rather than refusing -- a `min` over a raw zero would refuse every
/// model that configures nothing, which is almost all of them.
fn effective_output_ceiling(catalog_ceiling: u32, configured_ceiling: u32) -> u32 {
    if configured_ceiling == 0 {
        catalog_ceiling
    } else {
        catalog_ceiling.min(configured_ceiling)
    }
}

/// Whether `row` confirms a usable rate in BOTH base dimensions.
///
/// Both are required even though only the output dimension bounds what a probe
/// generates: a cell that prices one dimension and not the other is a cell whose
/// economics are half-confirmed, and a paid call sized off it would be
/// authorized on evidence the catalog does not carry. Checked as a conjunction
/// of two identical per-dimension predicates so each dimension has its own
/// named case in the sidecar.
fn prices_both_dimensions(row: &CatalogRow) -> bool {
    is_usable_rate(row.input_cost_per_token) && is_usable_rate(row.output_cost_per_token)
}

/// Whether one base per-token rate is a confirmed, usable price: present,
/// finite, and strictly positive.
///
/// STRICTER than the catalog's own cell validation, which permits a zero rate
/// because a genuinely free vendor tier is a real offering. This stage is
/// deciding whether to spend, and a zero rate does not distinguish a free tier
/// from an unpopulated cell -- so it declines rather than treating an
/// indistinguishable pair as permission.
fn is_usable_rate(rate: Option<f32>) -> bool {
    rate.is_some_and(|value| value.is_finite() && value > 0.0)
}

#[cfg(test)]
#[path = "paid_probe_profile_tests.rs"]
mod paid_probe_profile_tests;
