//! Per-provider reduction from a captured vendor quota family onto the
//! normalized snapshot.
//!
//! Each reducer walks the curated rows for its provider kind, finds the raw
//! values that row names, and assembles a window only when every part of it
//! survives validation. Nothing here decides what a window MEANS: the role,
//! the window's length and the threshold all come from the curated table, so a
//! reducer cannot pair a role with the wrong duration or bound a reset against
//! a window it does not belong to.
//!
//! # Why these are strict where the ledger's quota mapping is loose
//!
//! `UsageCapture::observe_quota` in the CLI crate reads the same two vendor
//! families and converts the same Codex percent, and the bounds there are
//! DELIBERATELY loose -- it admits Anthropic fractions well above 1.0 and
//! Codex percentages far above 100 so that a weird upstream value is still
//! RECORDED in the observability columns rather than silently dropped. These
//! reducers produce a ROUTING signal instead, so they are bounded: a value
//! they cannot interpret at all -- unparseable, non-finite, or negative --
//! becomes a cap-dormant `Unknown` rather than a recorded oddity. A value that
//! is merely OVER its scale is a different case and is not refused: a finite
//! percent above 100 saturates to an exhausted `Known` window, because an
//! upstream reporting past its own limit is telling routectl the window is
//! spent, and reading that as "no information" would hand the seat back its
//! headroom. Two sites converting the same percent can drift, which is why the
//! sibling carries the matching note and why a test pins that both derive the
//! same fraction from the same captured input.
//!
//! # Why the bare Anthropic `reset` is never read
//!
//! The unified family carries a bare `anthropic-ratelimit-unified-reset`
//! alongside the per-window `5h-reset` and `7d-reset`. On every captured
//! envelope the bare value happens to EQUAL the 5h one, so pairing it to the
//! FAST window would look correct in every test that exists -- and would be a
//! guess about which window an unlabeled value describes, one that silently
//! becomes wrong the day the upstream picks a different representative window.
//! A window's reset comes from the suffix that names that window, or the
//! window stays `Unknown`.

use std::time::{Duration, SystemTime};

use routectl_core::upstream_meta::{AnthropicUnifiedQuota, CodexQuota, OVERAGE_CLAIM};

use super::curation::{
    ANTHROPIC_FAST_SOURCE_ID, ANTHROPIC_PROVIDER_KIND, CODEX_PROVIDER_KIND, CuratedWindow,
    RESET_TOLERANCE, row_for,
};
use super::freshness::{ObservationStamp, accept_reset};
use super::window::{Billing, QuotaWindow, Utilization, WindowRole};

/// Scale of the Codex `primary-used-percent` value, which is a 0-100 percent
/// rather than the 0-1 fraction the normalized shape carries.
const PERCENT_SCALE: f64 = 100.0;

/// One seat's normalized quota reading, as the reducers produce it.
///
/// Both roles are always present as values, because "this provider curates no
/// FAST window" and "this provider's FAST window could not be read" are both
/// the same thing to a placement decision: no evidence. There is no `Default`
/// and no constructor that invents a reading -- a snapshot only exists as the
/// output of a reduction over real observed metadata.
#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    /// When the metadata this snapshot came from was read, on both clocks.
    pub observed: ObservationStamp,
    /// The short recovering window, or `Unknown`.
    pub fast: QuotaWindow,
    /// The long window, or `Unknown`.
    pub slow: QuotaWindow,
    /// Which budget the upstream reported billing against.
    pub billing: Billing,
}

/// Reduce Anthropic's unified quota family onto the normalized snapshot.
///
/// `5h-utilization` is the only CAPACITY-WINDOW utilization the shipped header
/// parser types (as `utilization`). It types five more suffixes -- `status`,
/// `overage-status`, `overage-utilization`, `representative-claim` and the bare
/// `reset` -- and pushes everything else, including `7d-utilization` and all
/// three per-window resets, into `extras`. So this reducer reads the 7d
/// utilization and every per-window reset by walking `extras`, and a fixture
/// must place a suffix exactly where `assign_suffix` would put it or the test
/// built on it proves the wrong thing.
///
/// Widening that struct instead is out of scope by design: it feeds the shared
/// ledger write path, which this derived-only reading must not disturb.
pub fn reduce_anthropic(
    quota: &AnthropicUnifiedQuota,
    observed: &ObservationStamp,
) -> QuotaSnapshot {
    QuotaSnapshot {
        observed: *observed,
        fast: anthropic_window(quota, observed, &WindowRole::Fast),
        slow: anthropic_window(quota, observed, &WindowRole::Slow),
        billing: anthropic_billing(quota),
    }
}

/// Reduce the Codex quota family onto the normalized snapshot.
///
/// Codex yields a SLOW window and nothing else, and that comes out of the
/// curated table rather than out of this function: there is no curated Codex
/// FAST row, so the FAST lookup returns nothing and the window stays
/// `Unknown`. Billing stays `Unknown` too -- `active-limit` names which plan
/// limit is in force, not which budget the request billed against, and reading
/// it as billing evidence would be the same unknown-as-known confusion the
/// tri-state exists to prevent.
pub fn reduce_codex(quota: &CodexQuota, observed: &ObservationStamp) -> QuotaSnapshot {
    QuotaSnapshot {
        observed: *observed,
        fast: codex_window(quota, observed, &WindowRole::Fast),
        slow: codex_window(quota, observed, &WindowRole::Slow),
        billing: Billing::Unknown,
    }
}

/// The Anthropic window for one role, or `Unknown` when it is uncurated or
/// unreadable.
fn anthropic_window(
    quota: &AnthropicUnifiedQuota,
    observed: &ObservationStamp,
    role: &WindowRole,
) -> QuotaWindow {
    let Some(row) = row_for(ANTHROPIC_PROVIDER_KIND, role) else {
        return QuotaWindow::Unknown;
    };
    let raw_utilization = if row.source_id == ANTHROPIC_FAST_SOURCE_ID {
        quota.utilization.as_deref()
    } else {
        extra(&quota.extras, &format!("{}-utilization", row.source_id))
    };
    // Only the suffix naming THIS window; never the bare `reset`.
    let raw_reset = extra(&quota.extras, &format!("{}-reset", row.source_id));
    assemble(raw_utilization.and_then(fraction), raw_reset, observed, row)
}

/// The Codex window for one role, or `Unknown` when it is uncurated or
/// unreadable.
fn codex_window(quota: &CodexQuota, observed: &ObservationStamp, role: &WindowRole) -> QuotaWindow {
    let Some(row) = row_for(CODEX_PROVIDER_KIND, role) else {
        return QuotaWindow::Unknown;
    };
    // The captured family types exactly one window's values, and the curated
    // Codex row is that window; a further Codex row would need its own
    // capture and its own accessor rather than a guess at a suffix.
    let utilization = quota
        .primary_used_percent
        .as_deref()
        .and_then(percent_as_fraction);
    assemble(
        utilization,
        quota.primary_reset_at.as_deref(),
        observed,
        row,
    )
}

/// Which budget the upstream reported billing against.
///
/// A missing or empty claim is `Unknown`, NOT `Included`: the shipped
/// `is_overage` predicate answers `false` for both, and inheriting that
/// conflation would let an absent header read as evidence a seat is cheap.
fn anthropic_billing(quota: &AnthropicUnifiedQuota) -> Billing {
    match quota.representative_claim.as_deref() {
        None | Some("") => Billing::Unknown,
        Some(OVERAGE_CLAIM) => Billing::Overage,
        Some(_) => Billing::Included,
    }
}

/// Build a `Known` window, or `Unknown` if any part of it cannot be trusted.
///
/// The reset goes through `accept_reset` with the CURATED duration, which is
/// the only way to obtain the `ValidatedReset` the variant demands -- so a
/// misparsed reset cannot reach a trusted window even by mistake. A rejection
/// costs this window alone.
fn assemble(
    utilization: Option<Utilization>,
    raw_reset: Option<&str>,
    observed: &ObservationStamp,
    row: &CuratedWindow,
) -> QuotaWindow {
    let Some(utilization) = utilization else {
        return QuotaWindow::Unknown;
    };
    let Some(reset_at) = raw_reset.and_then(epoch_seconds) else {
        return QuotaWindow::Unknown;
    };
    match accept_reset(reset_at, observed, row.duration, RESET_TOLERANCE) {
        Ok(reset_at) => QuotaWindow::Known {
            utilization,
            reset_at,
        },
        Err(_) => QuotaWindow::Unknown,
    }
}

/// Value of one `extras` suffix, in the raw form the upstream sent it.
fn extra<'a>(extras: &'a [(String, String)], suffix: &str) -> Option<&'a str> {
    extras
        .iter()
        .find(|(key, _)| key == suffix)
        .map(|(_, value)| value.as_str())
}

/// Parse a raw 0-1 fraction string.
fn fraction(raw: &str) -> Option<Utilization> {
    Utilization::new(raw.parse::<f64>().ok()?)
}

/// Parse a raw 0-100 percent string onto the 0-1 fraction scale.
fn percent_as_fraction(raw: &str) -> Option<Utilization> {
    Utilization::new(raw.parse::<f64>().ok()? / PERCENT_SCALE)
}

/// Parse an epoch-SECONDS instant. Seconds is the scale both vendor families
/// report and the scale the ledger stores; a value that does not parse as
/// whole seconds is not silently reinterpreted on another scale.
fn epoch_seconds(raw: &str) -> Option<SystemTime> {
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(raw.parse::<u64>().ok()?))
}

#[cfg(test)]
#[path = "reduce_tests.rs"]
mod reduce_tests;
