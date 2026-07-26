//! Capability matrix panel builder: lanes (config model nicknames plus any
//! stale learned lanes) by capability keys, one resolved display cell each.
//!
//! Merges the three capability signal layers -- operator overrides, the
//! learned ledger-replay registry, and catalog priors -- onto config model
//! nicknames as the lane identity. Each cell's verdict comes from the shared
//! pure resolver (`resolve_display_verdict`), so the panel can never drift
//! from the router's precedence order. Ages and stale flags are layered on
//! top here (a display concern the pure resolver deliberately omits).

use std::collections::BTreeSet;
use std::time::Instant;

use routectl_core::capability::WELL_KNOWN_CAPABILITY_KEYS;
use routectl_router::{
    CapabilityMatrixPanel, LearnedRegistryEntry, MatrixAvailability, MatrixCell, MatrixLane,
    OverrideRegistry, is_stale_days, resolve_display_verdict,
};

use super::sections::staleness_threshold_days;
use super::{CapabilityMatrixSource, DoctorContext, PriorCell};

/// How many observed capability keys outside the well-known five are
/// rendered as their own columns before the rest collapse into a
/// `(+N more)` overflow count.
const OTHER_COLUMN_CAP: usize = 10;

/// Milliseconds per day, for converting a cell's last-seen age to whole days
/// against the operator staleness hint.
const MS_PER_DAY: i64 = 86_400_000;

/// A lane's identity plus the config binding needed to consult overrides.
struct LaneMeta {
    lane: String,
    provider: Option<String>,
    provider_kind: &'static str,
    routed: bool,
}

/// Build the capability matrix panel from the read-only doctor context. The
/// learned matrix source supplies the availability tri-state and the learned
/// cells; the parsed config supplies lanes, overrides, and priors. All three
/// layers merge per cell through the shared display resolver.
pub(super) fn build_capability_matrix_panel(ctx: &DoctorContext) -> CapabilityMatrixPanel {
    let availability = match &ctx.capability_matrix {
        CapabilityMatrixSource::Available { .. } => MatrixAvailability::Available,
        CapabilityMatrixSource::Empty => MatrixAvailability::Empty,
        CapabilityMatrixSource::Unavailable(code) => MatrixAvailability::Unavailable { code },
    };
    let (entries, now): (&[LearnedRegistryEntry], Option<Instant>) = match &ctx.capability_matrix {
        CapabilityMatrixSource::Available { entries, now, .. } => (entries.as_slice(), Some(*now)),
        CapabilityMatrixSource::Empty | CapabilityMatrixSource::Unavailable(_) => (&[], None),
    };

    let overrides = OverrideRegistry::build(&ctx.config);
    let priors: &[PriorCell] = ctx.capability.config.as_ref().map_or(&[], |c| &c.priors);
    let threshold = staleness_threshold_days(ctx.freshness.staleness_hint_days);
    let today = ctx.freshness.today_epoch_day;

    let lanes_meta = lane_metas(ctx, entries);
    let (columns, other_overflow) = columns_for(entries, priors, &overrides);

    let lanes = lanes_meta
        .iter()
        .map(|meta| MatrixLane {
            lane: meta.lane.clone(),
            routed: meta.routed,
            cells: columns
                .iter()
                .map(|cap| {
                    build_cell(
                        meta, cap, &overrides, entries, priors, now, today, threshold,
                    )
                })
                .collect(),
        })
        .collect();

    CapabilityMatrixPanel {
        availability,
        columns,
        other_overflow,
        lanes,
    }
}

/// The lane rows: every configured model nickname (routed), then any learned
/// state key with no config entry (a stale ledger row for a removed model),
/// surfaced unrouted rather than dropped.
fn lane_metas(ctx: &DoctorContext, entries: &[LearnedRegistryEntry]) -> Vec<LaneMeta> {
    let mut metas: Vec<LaneMeta> = ctx
        .config
        .models
        .iter()
        .map(|(nickname, model)| LaneMeta {
            lane: nickname.clone(),
            provider: Some(model.provider.clone()),
            provider_kind: provider_kind_for(ctx, &model.provider),
            routed: true,
        })
        .collect();

    let configured: BTreeSet<&str> = ctx.config.models.keys().map(String::as_str).collect();
    let mut unrouted: BTreeSet<&str> = BTreeSet::new();
    for entry in entries {
        if !configured.contains(entry.state_key.as_str()) {
            unrouted.insert(entry.state_key.as_str());
        }
    }
    for lane in unrouted {
        metas.push(LaneMeta {
            lane: lane.to_string(),
            provider: None,
            provider_kind: "",
            routed: false,
        });
    }
    metas
}

/// The column keys: the five well-known keys, then the observed keys outside
/// that set (from learned entries, priors, and overrides), sorted and capped
/// at [`OTHER_COLUMN_CAP`]. The second return value is the count of observed
/// other keys beyond the cap.
fn columns_for(
    entries: &[LearnedRegistryEntry],
    priors: &[PriorCell],
    overrides: &OverrideRegistry,
) -> (Vec<String>, u32) {
    let mut others: BTreeSet<String> = BTreeSet::new();
    for entry in entries {
        insert_other(&mut others, &entry.feature_key);
    }
    for prior in priors {
        for (key, _) in &prior.capabilities {
            insert_other(&mut others, key);
        }
    }
    for row in overrides.snapshot() {
        insert_other(&mut others, &row.capability_key);
    }

    let overflow = u32::try_from(others.len().saturating_sub(OTHER_COLUMN_CAP)).unwrap_or(u32::MAX);
    let mut columns: Vec<String> = WELL_KNOWN_CAPABILITY_KEYS
        .iter()
        .map(|key| (*key).to_string())
        .collect();
    columns.extend(others.into_iter().take(OTHER_COLUMN_CAP));
    (columns, overflow)
}

fn insert_other(set: &mut BTreeSet<String>, key: &str) {
    if !WELL_KNOWN_CAPABILITY_KEYS.contains(&key) {
        set.insert(key.to_string());
    }
}

/// Resolve one `(lane, capability)` cell: consult the three layers, run the
/// shared display resolver, then layer on the display-only age and stale
/// flag from whichever layer won.
#[allow(clippy::too_many_arguments)]
fn build_cell(
    meta: &LaneMeta,
    capability: &str,
    overrides: &OverrideRegistry,
    entries: &[LearnedRegistryEntry],
    priors: &[PriorCell],
    now: Option<Instant>,
    today: i64,
    threshold: i64,
) -> MatrixCell {
    let override_cell = meta.provider.as_deref().and_then(|provider| {
        overrides.resolve(provider, &meta.lane, capability, meta.provider_kind)
    });

    let learned_entry = entries
        .iter()
        .find(|e| e.state_key == meta.lane && e.feature_key == capability);
    let learned = learned_entry.map(|e| (e.verdict, e.source));

    let prior_stamp = priors
        .iter()
        .find(|p| p.nickname == meta.lane)
        .and_then(|p| {
            p.capabilities
                .iter()
                .find(|(key, _)| key == capability)
                .map(|(_, supported)| (*supported, p.verified_at.as_str()))
        });
    let prior = prior_stamp.map(|(supported, _)| supported);

    let display = resolve_display_verdict(override_cell, learned, prior);

    let (age_ms, stale) = match display.source {
        Some("live" | "probe") => {
            let entry = learned_entry.expect("a live/probe cell has a learned entry");
            let age = now.map(|clock| {
                let elapsed = clock.saturating_duration_since(entry.last_seen).as_millis();
                i64::try_from(elapsed).unwrap_or(i64::MAX)
            });
            // Only a verified positive (supported) carries a staleness flag;
            // a learned negative's freshness is governed by its decay window.
            let stale =
                display.supported == Some(true) && age.is_some_and(|a| a / MS_PER_DAY > threshold);
            (age, stale)
        }
        Some("prior") => {
            let stale = prior_stamp
                .is_some_and(|(_, verified_at)| is_stale_days(verified_at, today, threshold));
            (None, stale)
        }
        _ => (None, false),
    };

    MatrixCell {
        verdict: display.verdict,
        supported: display.supported,
        source: display.source,
        age_ms,
        stale,
    }
}

/// Provider kind (`kind_str`) for a configured provider, or `""` when the
/// provider is absent -- an empty kind normalizes as a pass-through, exactly
/// as the override registry treats an unconfigured provider.
fn provider_kind_for(ctx: &DoctorContext, provider: &str) -> &'static str {
    ctx.config
        .providers
        .get(provider)
        .map_or("", routectl_router::ProviderEntry::kind_str)
}
