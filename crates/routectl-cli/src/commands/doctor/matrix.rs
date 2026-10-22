//! Capability matrix panel builder: learned lanes by capability keys, one
//! resolved display cell each.
//!
//! A row is a learned LANE -- the `provider_entry#upstream` key the learned
//! store, `capability purge`, and `provider probe` all share -- not a model
//! nickname: two nicknames for one upstream on one provider entry share
//! their learned history, so the matrix shows that history once and lists
//! the nicknames that map to it. Rows are the router's learned-lane
//! projection of the config (a pooled model contributes one lane per member
//! entry), then every learned key the config no longer maps, surfaced
//! unrouted rather than dropped.
//!
//! Each cell resolves through the shared lane-cell helper
//! (`crate::lane_cell`), which runs the router's precedence seam in display
//! mode once per nickname on the lane: the operator wire pin, the lane's
//! representability, overrides, the learned registry, the shipped beta seed
//! and catalog priors, in the order dispatch applies them. When the
//! nicknames on a lane disagree on the shown verdict, the cell's verdict
//! reads `mixed` with no lane-wide source or layer; when they disagree on the
//! action, the action reads `mixed`; either way the cell carries each
//! nickname's own verdict, layer and action. `overrode_layer` names the tier
//! the winner beat, when every nickname agrees on it.
//! Ages, timestamps, and stale flags are layered on top here (a display
//! concern the seam deliberately omits).
//!
//! The beta seed is lane-wide: it covers a cell when the lane's provider kind
//! is the seed's and the column is a seeded flag's key, whether or not the
//! replay left an entry there, and a seed-clear marker the replay recorded for
//! the cell turns it into a cleared seed.

use std::collections::BTreeSet;

use routectl_core::capability::WELL_KNOWN_CAPABILITY_KEYS;
use routectl_router::{
    ACTION_MIXED, BetaSeedScope, CapabilityMatrixPanel, Layer, LearnedRegistryEntry,
    MatrixAvailability, MatrixCell, MatrixLane, MatrixNicknameAction, MatrixSource,
    OverrideRegistry, VERDICT_MIXED, beta_override_key, is_stale_days,
};

use super::sections::staleness_threshold_days;
use super::{CapabilityMatrixSource, DoctorContext, MatrixOrigin, PriorCell};
use crate::lane_cell::{
    CellSources, LaneCell, LaneMeta, LearnedSnapshot, NicknameCell, entry_stamps, lane_metas,
    resolve_cell,
};

/// Milliseconds per day, for converting a cell's last-seen age to whole days
/// against the operator staleness hint.
const MS_PER_DAY: i64 = 86_400_000;

/// The layer tags the display-only age and stale flag key on.
const LAYER_LEARNED: &str = "learned";
const LAYER_PRIOR: &str = "prior";

/// The staleness inputs every cell reads.
struct Staleness<'a> {
    priors: &'a [PriorCell],
    today: i64,
    threshold: i64,
}

/// Build the capability matrix panel from the read-only doctor context. The
/// learned matrix source supplies the availability tri-state, the learned
/// cells, and the replay tally; the parsed config supplies lanes, overrides,
/// and priors. Every cell resolves through the shared lane-cell helper.
pub(super) fn build_capability_matrix_panel(ctx: &DoctorContext) -> CapabilityMatrixPanel {
    let (availability, learned, replay) = match &ctx.capability_matrix {
        CapabilityMatrixSource::Available {
            entries,
            now,
            now_ms,
            replay,
            seed_clears,
        } => (
            MatrixAvailability::Available,
            LearnedSnapshot {
                entries: entries.as_slice(),
                now: Some((*now, *now_ms)),
                seed_clears: seed_clears.as_slice(),
            },
            *replay,
        ),
        CapabilityMatrixSource::Empty {
            replay,
            seed_clears,
        } => (
            MatrixAvailability::Empty,
            LearnedSnapshot {
                entries: &[],
                now: None,
                seed_clears: seed_clears.as_slice(),
            },
            *replay,
        ),
        CapabilityMatrixSource::Unavailable(code) => (
            MatrixAvailability::Unavailable { code },
            LearnedSnapshot {
                entries: &[],
                now: None,
                seed_clears: &[],
            },
            None,
        ),
    };

    let overrides = OverrideRegistry::build(&ctx.config);
    let priors: &[PriorCell] = ctx.capability.config.as_ref().map_or(&[], |c| &c.priors);
    let lanes_meta = lane_metas(&ctx.config, learned.entries);
    let seeded = seeded_columns(ctx.beta_seed, &lanes_meta);
    let columns = columns_for(learned.entries, priors, &overrides, &seeded);
    let prior = |nickname: &str, capability: &str| {
        nickname_prior(nickname, capability, priors).map(|(supported, _)| supported)
    };
    let sources = CellSources {
        config: &ctx.config,
        overrides: &overrides,
        learned,
        beta_seed: ctx.beta_seed,
        prior: &prior,
    };
    let staleness = Staleness {
        priors,
        today: ctx.freshness.today_epoch_day,
        threshold: staleness_threshold_days(ctx.freshness.staleness_hint_days),
    };

    let lanes = lanes_meta
        .iter()
        .map(|meta| MatrixLane {
            lane: meta.lane.clone(),
            nicknames: meta.models.iter().map(|m| m.nickname.to_string()).collect(),
            provider_kind: meta.provider_kind,
            routed: meta.routed,
            cells: columns
                .iter()
                .map(|cap| build_cell(meta, cap, &sources, &staleness))
                .collect(),
        })
        .collect();

    let (source, warm) = match &ctx.matrix_origin {
        MatrixOrigin::LedgerReplay => (MatrixSource::LedgerReplay, None),
        MatrixOrigin::Resident(warm) => (MatrixSource::Resident, Some(warm.clone())),
    };
    CapabilityMatrixPanel {
        availability,
        source,
        columns,
        lanes,
        replay,
        warm,
    }
}

/// The seeded flags' keys when any lane is of the kind the seed applies to,
/// else none: a seed column on a matrix with no such lane would be all blank.
fn seeded_columns(seed: BetaSeedScope, lanes: &[LaneMeta]) -> Vec<String> {
    if lanes
        .iter()
        .any(|lane| lane.provider_kind == seed.provider_kind())
    {
        seed.seeded_keys()
    } else {
        Vec::new()
    }
}

/// The column keys: the well-known keys, then the observed keys outside that
/// set (from learned entries, priors, overrides, and the seed), sorted.
///
/// Override keys are stored canonicalized while the other layers keep the
/// client's spelling, so an override that canonicalizes onto an existing
/// column renders in that column: its cells resolve the override through the
/// registry's own canonicalizing lookup either way.
fn columns_for(
    entries: &[LearnedRegistryEntry],
    priors: &[PriorCell],
    overrides: &OverrideRegistry,
    seeded: &[String],
) -> Vec<String> {
    let mut others: BTreeSet<String> = BTreeSet::new();
    for key in seeded {
        insert_other(&mut others, key);
    }
    for entry in entries {
        insert_other(&mut others, &entry.feature_key);
    }
    for prior in priors {
        for (key, _) in &prior.capabilities {
            insert_other(&mut others, key);
        }
    }
    let observed: BTreeSet<String> = others.iter().map(|key| beta_override_key(key)).collect();
    for row in overrides.snapshot() {
        if !observed.contains(&row.capability_key) {
            insert_other(&mut others, &row.capability_key);
        }
    }

    let mut columns: Vec<String> = WELL_KNOWN_CAPABILITY_KEYS
        .iter()
        .map(|key| (*key).to_string())
        .collect();
    columns.extend(others);
    columns
}

fn insert_other(set: &mut BTreeSet<String>, key: &str) {
    if !WELL_KNOWN_CAPABILITY_KEYS.contains(&key) {
        set.insert(key.to_string());
    }
}

/// Resolve one `(lane, capability)` cell through the shared lane-cell
/// helper, then layer on the display-only age, timestamps, and stale flag.
fn build_cell(
    meta: &LaneMeta,
    capability: &str,
    sources: &CellSources,
    staleness: &Staleness,
) -> MatrixCell {
    let cell = resolve_cell(meta, capability, sources);
    let shown = cell.shown();
    let action = cell.action();
    let nickname_actions = if cell.nicknames_agree() {
        Vec::new()
    } else {
        cell.resolved.iter().filter_map(nickname_action).collect()
    };

    let layer = cell.layer();
    let (age_ms, stale) = match layer {
        Some(LAYER_LEARNED) => learned_age(
            &cell,
            sources.learned.now,
            shown.and_then(|s| s.supported),
            staleness.threshold,
        ),
        Some(LAYER_PRIOR) => (None, lane_prior_is_stale(meta, capability, staleness)),
        _ => (None, false),
    };
    let stamps = cell
        .entry
        .zip(sources.learned.now)
        .map(|(entry, anchor)| entry_stamps(entry, anchor));

    MatrixCell {
        verdict: shown.map_or(VERDICT_MIXED, |s| s.verdict),
        supported: shown.and_then(|s| s.supported),
        source: shown.and_then(|s| s.source),
        layer,
        overrode_layer: cell.overrode_layer(),
        action: action.unwrap_or(ACTION_MIXED),
        age_ms,
        stale,
        first_seen_ms: stamps.map(|s| s.first_seen),
        last_seen_ms: stamps.map(|s| s.last_seen),
        expires_at_ms: stamps.and_then(|s| s.expires_at),
        nickname_actions,
    }
}

fn nickname_action(resolved: &NicknameCell) -> Option<MatrixNicknameAction> {
    let resolution = &resolved.resolution;
    resolved
        .nickname
        .as_ref()
        .map(|nickname| MatrixNicknameAction {
            nickname: nickname.clone(),
            verdict: resolution.display_verdict(),
            layer: resolution.display_layer().map(Layer::as_str),
            overrode_layer: resolution.overridden().map(Layer::as_str),
            action: resolution.as_str(),
        })
}

/// Whether any mapped nickname whose catalog prior carries the capability has
/// a `verified_at` stamp past the staleness threshold.
fn lane_prior_is_stale(meta: &LaneMeta, capability: &str, staleness: &Staleness) -> bool {
    meta.models
        .iter()
        .filter_map(|m| nickname_prior(m.nickname, capability, staleness.priors))
        .any(|(_, verified_at)| is_stale_days(verified_at, staleness.today, staleness.threshold))
}

/// One nickname's catalog prior for a capability, with its `verified_at` stamp.
fn nickname_prior<'a>(
    nickname: &str,
    capability: &str,
    priors: &'a [PriorCell],
) -> Option<(bool, &'a str)> {
    priors
        .iter()
        .find(|p| p.nickname == nickname)
        .and_then(|p| {
            p.capabilities
                .iter()
                .find(|(key, _)| key == capability)
                .map(|(_, supported)| (*supported, p.verified_at.as_str()))
        })
}

/// The age and stale flag for a cell whose shown layer is learned.
fn learned_age(
    cell: &LaneCell,
    now: Option<(std::time::Instant, i64)>,
    supported: Option<bool>,
    threshold: i64,
) -> (Option<i64>, bool) {
    let age = cell.entry.zip(now).map(|(entry, (now, _))| {
        let elapsed = now.saturating_duration_since(entry.last_seen).as_millis();
        i64::try_from(elapsed).unwrap_or(i64::MAX)
    });
    // Only a verified positive (supported) carries a staleness flag; a
    // learned negative's freshness is governed by its decay window.
    let stale = supported == Some(true) && age.is_some_and(|a| a / MS_PER_DAY > threshold);
    (age, stale)
}
