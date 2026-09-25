//! The opening-count selector: which estimate seeds a turn's first
//! `message_start` when no measured upstream opening is available.
//!
//! Tier order, first match wins:
//!
//! 1. the session anchor, when it verifies against this request and lane;
//! 2. the lane's calibrated estimate, when the lane has a usable factor;
//! 3. the raw request estimate.
//!
//! Pure: every input is known before dispatch settles, so the chosen
//! source and reason never depend on the terminal usage they are later
//! compared against. Calibration corrects only the raw estimate; an
//! anchored opening already carries the provider's own count and is never
//! corrected again.

use routectl_router::{DispatchMeta, OpeningLane};

use super::{AnchorLane, AnchorRecord, AnchorVerdict, MissReason, RequestIdentity, evaluate};

/// Which tier produced an opening count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpeningSource {
    /// Prior turn's provider count plus normalized growth.
    Anchor,
    /// Raw estimate corrected by the lane's learned factor.
    Calibrated,
    /// Raw request estimate, uncorrected.
    Raw,
}

impl OpeningSource {
    /// Stable, log-safe label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anchor => "anchor",
            Self::Calibrated => "calibrated",
            Self::Raw => "raw",
        }
    }
}

/// Why the anchor tier did or did not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpeningReason {
    /// The anchor verified and was used.
    AnchorHit,
    /// The anchor was checked and did not apply.
    AnchorMiss(MissReason),
    /// The request has no anchor key (no session, or a key past its bound).
    Unanchored,
    /// No lane could be resolved, so neither anchor nor calibration applies.
    LaneUnresolved,
}

impl OpeningReason {
    /// Stable, log-safe label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnchorHit => "anchor_hit",
            Self::AnchorMiss(miss) => miss.as_str(),
            Self::Unanchored => "unanchored",
            Self::LaneUnresolved => "lane_unresolved",
        }
    }
}

/// The lane an opening is selected against.
#[derive(Debug, Clone, Copy)]
pub enum OpeningLaneBasis<'a> {
    /// The lane whose attempt won; the opening is final.
    Served(&'a AnchorLane),
    /// The route head, predicted before the winner is known (the warm
    /// path after the flush grace); the opening is provisional.
    Head(&'a AnchorLane),
    /// No route head resolved, before the winner is known (the warm path
    /// after the flush grace); the raw opening is provisional.
    UnresolvedHead,
    /// The winning dispatch recorded no usable lane; the raw opening is
    /// final.
    UnresolvedServed,
}

impl OpeningLaneBasis<'_> {
    /// Whether this basis was taken before the winning lane was known --
    /// exactly the selections a selection's `provisional` flag marks.
    pub const fn is_pre_dispatch(self) -> bool {
        matches!(self, Self::Head(_) | Self::UnresolvedHead)
    }

    /// The pre-dispatch basis for a route-head lookup result
    /// (`Router::opening_lane`, converted).
    pub const fn head(lane: Option<&AnchorLane>) -> OpeningLaneBasis<'_> {
        match lane {
            Some(lane) => OpeningLaneBasis::Head(lane),
            None => OpeningLaneBasis::UnresolvedHead,
        }
    }

    /// The post-dispatch basis for a served-lane result
    /// (`AnchorLane::served`).
    pub const fn served(lane: Option<&AnchorLane>) -> OpeningLaneBasis<'_> {
        match lane {
            Some(lane) => OpeningLaneBasis::Served(lane),
            None => OpeningLaneBasis::UnresolvedServed,
        }
    }
}

/// The anchor evidence for one keyed turn.
#[derive(Debug, Clone, Copy)]
pub struct AnchorProbe<'a> {
    /// The session's current record, `None` when cold.
    pub record: Option<&'a AnchorRecord>,
    /// This request's measured identity.
    pub identity: &'a RequestIdentity,
}

/// The selected opening count and its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpeningSelection {
    /// Opening input tokens to report.
    pub tokens: u64,
    /// Tier that produced `tokens`.
    pub source: OpeningSource,
    /// Anchor outcome behind the choice.
    pub reason: OpeningReason,
    /// Selected before the winning lane was known.
    pub provisional: bool,
}

/// Select a turn's opening count. See the module docs for the tier order.
///
/// `raw_tokens` is the client-facing meter estimate of the request.
/// `calibrate` answers the calibrated estimate for a lane (in production,
/// `Router::calibrated_estimate`); it is consulted at most once, only for
/// the selected lane, and only when the anchor does not apply.
pub fn select_opening(
    anchor: Option<AnchorProbe<'_>>,
    basis: OpeningLaneBasis<'_>,
    raw_tokens: u64,
    calibrate: impl FnOnce(&AnchorLane, u64) -> Option<u64>,
) -> OpeningSelection {
    let provisional = basis.is_pre_dispatch();
    let lane = match basis {
        OpeningLaneBasis::Served(lane) | OpeningLaneBasis::Head(lane) => lane,
        OpeningLaneBasis::UnresolvedHead | OpeningLaneBasis::UnresolvedServed => {
            return OpeningSelection {
                tokens: raw_tokens,
                source: OpeningSource::Raw,
                reason: OpeningReason::LaneUnresolved,
                provisional,
            };
        }
    };
    let reason = match anchor.map(|probe| evaluate(probe.record, probe.identity, lane)) {
        Some(AnchorVerdict::Hit { opening_input }) => {
            return OpeningSelection {
                tokens: opening_input,
                source: OpeningSource::Anchor,
                reason: OpeningReason::AnchorHit,
                provisional,
            };
        }
        Some(AnchorVerdict::Miss(miss)) => OpeningReason::AnchorMiss(miss),
        None => OpeningReason::Unanchored,
    };
    let (tokens, source) = calibrate(lane, raw_tokens)
        .map_or((raw_tokens, OpeningSource::Raw), |corrected| {
            (corrected, OpeningSource::Calibrated)
        });
    OpeningSelection {
        tokens,
        source,
        reason,
        provisional,
    }
}

impl From<OpeningLane> for AnchorLane {
    fn from(lane: OpeningLane) -> Self {
        Self {
            provider_kind: lane.provider_kind.to_owned(),
            nickname: lane.nickname,
            upstream_model: lane.upstream_model,
            generation: lane.generation,
        }
    }
}

impl AnchorLane {
    /// The lane that served a dispatch, under the publication generation of
    /// the Router that ran it. `None` when the dispatch recorded no provider
    /// kind, nickname or wire model -- such a turn cannot anchor. The served
    /// seat is deliberately not part of the lane.
    pub fn served(meta: &DispatchMeta, generation: u64) -> Option<Self> {
        Some(Self {
            provider_kind: meta.served_provider_kind.clone()?,
            nickname: meta.served_model.clone()?,
            upstream_model: meta.served_upstream.clone()?,
            generation,
        })
    }
}

#[cfg(test)]
#[path = "opening_tests.rs"]
mod tests;
