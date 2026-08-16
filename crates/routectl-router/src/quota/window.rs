//! The normalized per-window quota value, its role tag, and its billing state.
//!
//! # Why unknown is a variant and not a number
//!
//! A subscription window is either observed or it is not, and the two cases
//! are not points on one scale. Any numeric encoding of "not observed" has to
//! pick a number, and every candidate is wrong in the same direction: a low
//! value claims the seat is empty and attracts every new session to the seat
//! routectl knows least about, while a high value claims it is exhausted and
//! silently removes a seat that may be perfectly healthy. Making
//! [`QuotaWindow::Unknown`] a variant removes the choice: a caller that wants
//! a number has to `match`, and the compiler makes it say what it does about
//! the unknown case.
//!
//! [`Utilization`] carries the same discipline one level down. Its field is
//! private and [`Utilization::new`] is the only way in, so no call site can
//! hold a utilization outside `[0.0, 1.0]` or a non-finite one -- and because
//! there is no `Default`, no call site can hold "known 0%" it never observed.
//!
//! The reset instant carries it too, and for the same reason. `Known` demands
//! a [`ValidatedReset`](super::freshness::ValidatedReset), which only
//! [`accept_reset`](super::freshness::accept_reset) can mint, so the
//! plausibility bound is unavoidable rather than advisory. An earlier draft of
//! this type held a raw `SystemTime` and DOCUMENTED that it had already been
//! validated -- which is precisely the shape of guarantee a later call site
//! skips, and it would have re-admitted the milliseconds-as-seconds reading
//! this module exists to refuse.

use super::freshness::ValidatedReset;

/// A validated subscription-window utilization: finite, in `[0.0, 1.0]`,
/// where `0.0` is an empty window and `1.0` is an exhausted one.
///
/// The field is private and [`Utilization::new`] is the only constructor, so
/// the range is a type-level guarantee rather than a convention every call
/// site has to remember. Deliberately NOT `Default`: a defaulted zero reads
/// as maximal headroom and is indistinguishable from a genuine empty-window
/// reading, which is precisely the confusion [`QuotaWindow`] exists to
/// prevent.
#[derive(Debug, Clone, PartialEq)]
pub struct Utilization {
    fraction: f64,
}

impl Utilization {
    /// Validate one raw fraction, or refuse it.
    ///
    /// `None` for a non-finite value (`NaN`, either infinity) and for a
    /// negative one: both are evidence the source was misparsed, and there is
    /// no headroom reading to salvage from them.
    ///
    /// A value above `1.0` SATURATES to `1.0`. The direction is the whole
    /// point: an upstream that reports slightly over its own limit is telling
    /// routectl the window is exhausted, so the saturation must land on
    /// exhausted. Saturating to `0.0` -- or wrapping, or refusing and letting
    /// the caller fall back to unknown-as-permissive -- would INVENT headroom
    /// on a seat that just said it has none.
    pub fn new(raw: f64) -> Option<Self> {
        if !raw.is_finite() || raw < 0.0 {
            return None;
        }
        Some(Self {
            fraction: raw.min(1.0),
        })
    }

    /// The validated fraction, in `[0.0, 1.0]`.
    pub const fn fraction(&self) -> f64 {
        self.fraction
    }
}

/// Which capacity horizon a window describes.
///
/// The vocabulary is two-valued on purpose. FAST is the short recovering
/// window a placement decision can act on, because a seat over its FAST cap
/// recovers within the lifetime of a conversation. SLOW is the long window
/// where being over cap is a durable fact about the seat rather than a
/// transient one, so it informs a guard rather than a ranking. A window whose
/// horizon cannot be determined gets no role and stays
/// [`QuotaWindow::Unknown`]; a role is never guessed from a window's upstream
/// NAME.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRole {
    /// Short recovering window -- the placement signal.
    Fast,
    /// Long window -- a durable fact about the seat, not a ranking value.
    Slow,
}

/// Which budget an upstream is currently billing against.
///
/// Three states rather than a bool, because the shipped
/// `AnthropicUnifiedQuota::is_overage` predicate reports `false` both for a
/// missing claim and for a known non-overage claim, and that conflation is
/// exactly what a routing decision must not inherit: unknown billing is not
/// evidence a seat is cheap.
///
/// This is a COST signal and never a capacity one. It may inform which seat a
/// NEW session is placed on; it must never rewrite a utilization, mark a seat
/// unavailable, or move an established session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Billing {
    /// No billing claim was reported, or it could not be interpreted.
    Unknown,
    /// Billing against the included subscription budget.
    Included,
    /// Billing beyond the included budget.
    Overage,
}

/// One provider-agnostic subscription window.
///
/// `Unknown` and `Known { utilization: 0.0, .. }` are DIFFERENT facts and
/// never compare equal: the first says routectl has no reading for this
/// window, the second says the upstream reported an empty one. A malformed,
/// unpairable or implausible reading collapses to `Unknown` rather than
/// failing the request, matching the source types' posture that a weird value
/// never fails a request.
///
/// Both fields of `Known` are constrained BY TYPE rather than by convention:
/// `Utilization` cannot hold a value outside `[0.0, 1.0]`, and
/// [`ValidatedReset`](super::freshness::ValidatedReset) cannot be minted
/// except by [`accept_reset`](super::freshness::accept_reset). So a reducer
/// cannot assemble a trusted window around an unchecked reset -- notably a
/// seconds-scale instant misparsed as milliseconds, which every expiry check
/// reads as permanently valid. Documenting that the reset "was already
/// validated" would have left exactly the gap a later call site skips.
#[derive(Debug, Clone, PartialEq)]
pub enum QuotaWindow {
    /// No trustworthy reading for this window. Carries no number, so no
    /// caller can accidentally rank on it.
    Unknown,
    /// A reading routectl trusts, valid until its reset.
    Known {
        /// How much of the window is consumed.
        utilization: Utilization,
        /// Wall-clock instant at which this window resets. Only obtainable
        /// from [`accept_reset`](super::freshness::accept_reset), which is
        /// what makes the plausibility bound unavoidable rather than
        /// advisory.
        reset_at: ValidatedReset,
    },
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod window_tests;
