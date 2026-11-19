//! Reducer from the Codex `x-codex-*` response-header quota family onto one
//! partial account observation.
//!
//! The family reports each window in a slot (`primary`, `secondary`) with
//! its own length in minutes, a 0-100 used percent and a reset (`reset-at` in
//! epoch seconds, or `reset-after-seconds`). Besides the account-wide family,
//! each extra limit reports its own slots under its own name
//! (`x-codex-<limit>-primary-*`). A slot's name says nothing about its length:
//! one plan reports a five-hour primary, another a weekly one. So every window
//! is keyed by the length it reports, and a window that reports no length is
//! refused as `UnknownLength` rather than given the length some captured plan
//! happened to use.
//!
//! An extra limit's window is scoped `codex_<limit>`, the limit id the
//! vendor's usage endpoint reports as that limit's `metered_feature`, so a
//! header reading and a polled reading of one window share one key.
//!
//! A window whose `reset-after-seconds` is at least its own length has not
//! started (its reset slides with every response); one with less has. Without
//! `reset-after-seconds` the headers do not say.
//!
//! A slot is dormant, not refused, when its used percent is not reported, when
//! it reports no reset, or when it is declared unused (zero used and zero
//! length, as the wire sends an unused secondary). A slot with a reset but no
//! length is refused as `UnknownLength`. Any other refusal costs that window
//! alone and is returned for the store to count.

use std::time::{Duration, SystemTime};

use routectl_core::upstream_meta::CodexQuota;

use super::freshness::ObservationStamp;
use super::reading::{
    AccountObservation, ObservationReport, ReadingSource, Scope, Started, WindowKey, WindowReading,
    WindowSet,
};
use super::reduce::{PERCENT_SCALE, RejectionReason};

/// The slots each family reports, in the order a reading lists them.
const SLOTS: [&str; 2] = ["primary", "secondary"];

/// Prefix of an extra limit's scope: the vendor's own limit-id namespace.
const SCOPE_PREFIX: &str = "codex_";

const SECS_PER_MINUTE: u64 = 60;

/// One reduction's output: the partial observation, when any window was
/// read, plus every reported window the trust rules refused.
#[derive(Debug)]
pub struct CodexHeaderReduction {
    /// Every window read, as one partial response observation.
    pub observation: Option<AccountObservation>,
    /// Reported windows refused, in slot order.
    pub rejections: Vec<RejectionReason>,
}

/// Reduce the Codex header family read at `observed`.
pub fn reduce_codex_headers(
    quota: &CodexQuota,
    observed: ObservationStamp,
) -> CodexHeaderReduction {
    let mut rejections = Vec::new();
    let mut windows = Vec::new();
    for slot in reported_slots(quota) {
        match slot_window(quota, &slot, &observed) {
            Ok(Some(window)) => windows.push(window),
            Ok(None) => {}
            Err(reason) => rejections.push(reason),
        }
    }
    let observation = match partial(windows, observed) {
        Ok(observation) => observation,
        Err(reason) => {
            rejections.push(reason);
            None
        }
    };
    CodexHeaderReduction {
        observation,
        rejections,
    }
}

/// One reported slot: its family (`None` for the account-wide one) and name.
#[derive(Debug)]
struct Slot<'a> {
    family: Option<&'a str>,
    name: &'static str,
}

impl Slot<'_> {
    /// The header suffix (after `x-codex-`) of this slot's `field`.
    fn suffix(&self, field: &str) -> String {
        match self.family {
            None => format!("{}-{field}", self.name),
            Some(family) => format!("{family}-{}-{field}", self.name),
        }
    }
}

/// Every slot whose used percent the family reports. The account-wide
/// primary's percent is a typed field; every other slot's sits in `extras`.
fn reported_slots(quota: &CodexQuota) -> Vec<Slot<'_>> {
    let mut slots = Vec::new();
    if quota.primary_used_percent.is_some() {
        slots.push(Slot {
            family: None,
            name: "primary",
        });
    }
    for (key, _) in &quota.extras {
        for name in SLOTS {
            let account_wide = format!("{name}-used-percent");
            if *key == account_wide {
                slots.push(Slot { family: None, name });
            } else if let Some(family) = key
                .strip_suffix(account_wide.as_str())
                .and_then(|rest| rest.strip_suffix('-'))
                .filter(|family| !family.is_empty())
            {
                slots.push(Slot {
                    family: Some(family),
                    name,
                });
            }
        }
    }
    slots
}

/// The raw value of `slot`'s `field`, `None` when absent or empty (the wire
/// sends an empty reset for an unused slot).
fn raw<'a>(quota: &'a CodexQuota, slot: &Slot<'_>, field: &str) -> Option<&'a str> {
    let typed = match (slot.family, slot.name, field) {
        (None, "primary", "used-percent") => Some(quota.primary_used_percent.as_deref()),
        (None, "primary", "reset-at") => Some(quota.primary_reset_at.as_deref()),
        _ => None,
    };
    let value = typed.unwrap_or_else(|| {
        let suffix = slot.suffix(field);
        quota
            .extras
            .iter()
            .find(|(key, _)| *key == suffix)
            .map(|(_, value)| value.as_str())
    });
    value.filter(|value| !value.is_empty())
}

/// The window `slot` reports, `None` when it is dormant, or the reason it
/// was refused.
fn slot_window(
    quota: &CodexQuota,
    slot: &Slot<'_>,
    observed: &ObservationStamp,
) -> Result<Option<WindowReading>, RejectionReason> {
    let Some(raw_used) = raw(quota, slot, "used-percent") else {
        return Ok(None);
    };
    let used = raw_used
        .parse::<f64>()
        .map_err(|_| RejectionReason::InvalidUtilization)?
        / PERCENT_SCALE;
    let minutes = raw(quota, slot, "window-minutes")
        .map(|raw| {
            raw.parse::<u64>()
                .map_err(|_| RejectionReason::UnknownLength)
        })
        .transpose()?;
    if used == 0.0 && minutes == Some(0) {
        return Ok(None);
    }
    let reset_after_secs = raw(quota, slot, "reset-after-seconds")
        .map(|raw| raw.parse::<u64>().map_err(|_| RejectionReason::Overflow))
        .transpose()?;
    let Some(reset_at) = reset_of(raw(quota, slot, "reset-at"), reset_after_secs, observed)? else {
        return Ok(None);
    };
    let length_secs = minutes
        .ok_or(RejectionReason::UnknownLength)?
        .checked_mul(SECS_PER_MINUTE)
        .ok_or(RejectionReason::UnknownLength)?;
    let scope = slot.family.map(scope_of).transpose()?;
    let key = WindowKey::new(length_secs, scope)?;
    let started = match reset_after_secs {
        Some(after) if after >= length_secs => Started::No,
        Some(_) => Started::Yes,
        None => Started::Unknown,
    };
    WindowReading::new(key, used, Some(reset_at), started, observed).map(Some)
}

/// The slot's reset: `reset-at` when reported, else the observation plus
/// `reset-after-seconds`, else `None` (dormant).
fn reset_of(
    raw_reset_at: Option<&str>,
    reset_after_secs: Option<u64>,
    observed: &ObservationStamp,
) -> Result<Option<SystemTime>, RejectionReason> {
    match (raw_reset_at, reset_after_secs) {
        (Some(raw), _) => epoch_seconds(raw).map(Some),
        (None, Some(after)) => observed
            .wall()
            .checked_add(Duration::from_secs(after))
            .map(Some)
            .ok_or(RejectionReason::Overflow),
        (None, None) => Ok(None),
    }
}

/// The scope of an extra limit's family, its name taken verbatim: a name
/// outside the scope's one canonical layout is refused, never rewritten, as
/// the poll refuses the same `metered_feature`.
fn scope_of(family: &str) -> Result<Scope, RejectionReason> {
    Scope::new(&format!("{SCOPE_PREFIX}{family}"))
}

/// Parse an epoch-SECONDS instant, the scale the family reports.
fn epoch_seconds(raw: &str) -> Result<SystemTime, RejectionReason> {
    raw.parse::<u64>()
        .ok()
        .and_then(|secs| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs)))
        .ok_or(RejectionReason::Overflow)
}

fn partial(
    windows: Vec<WindowReading>,
    observed: ObservationStamp,
) -> Result<Option<AccountObservation>, RejectionReason> {
    if windows.is_empty() {
        return Ok(None);
    }
    AccountObservation::new(
        ReadingSource::Response,
        observed,
        ObservationReport::Partial(WindowSet::new(windows)?),
    )
    .map(Some)
}

/// `(length_secs, scope, used fraction)` of each window [`reduce_codex_headers`]
/// reads from `quota` as observed at `observed_wall`. Test seam for comparing
/// the routing conversion with the request ledger's, which lives in a crate
/// above this one.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn codex_header_fractions_for_tests(
    quota: &CodexQuota,
    observed_wall: SystemTime,
) -> Vec<(u64, Option<String>, f64)> {
    let observed = ObservationStamp::from_parts(observed_wall, std::time::Instant::now());
    let Some(observation) = reduce_codex_headers(quota, observed).observation else {
        return Vec::new();
    };
    let ObservationReport::Partial(set) = observation.report() else {
        return Vec::new();
    };
    set.as_slice()
        .iter()
        .map(|window| {
            (
                window.key().length_secs(),
                window.key().scope().map(|scope| scope.as_str().to_owned()),
                window.used().fraction(),
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "codex_headers_tests.rs"]
mod codex_headers_tests;
