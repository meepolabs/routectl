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
//! Each cell merges the capability signal layers -- operator overrides, the
//! learned ledger-replay registry, the shipped beta seed, and catalog priors
//! -- through the shared pure resolvers (`resolve_display_verdict`,
//! `resolve_display_action`), so the panel cannot drift from the router's
//! precedence order. Both the verdict and the action are resolved per
//! nickname, because an override or a pinned beta can be nickname-scoped.
//! When the nicknames on a lane disagree on the verdict, the cell's verdict
//! reads `mixed` with no lane-wide source or layer; when they disagree on the
//! action, the action reads `mixed`; either way the cell carries each
//! nickname's own verdict, layer and action.
//! Ages, timestamps, and stale flags are layered on top here (a display
//! concern the pure resolvers deliberately omit).
//!
//! The beta seed is lane-wide: it covers a cell when the lane's provider kind
//! is the seed's and the column is a seeded flag's key, whether or not the
//! replay left an entry there, and a seed-clear marker the replay recorded for
//! the cell turns it into a cleared seed.

use std::collections::BTreeSet;
use std::time::Instant;

use routectl_core::capability::WELL_KNOWN_CAPABILITY_KEYS;
use routectl_router::{
    ACTION_MIXED, ActionInputs, BetaSeedScope, CapabilityMatrixPanel, DisplayVerdict,
    LearnedActing, LearnedLaneProjection, LearnedRegistryEntry, MatrixAvailability, MatrixCell,
    MatrixLane, MatrixNicknameAction, ModelEntry, OverrideRegistry, ProviderEntry, SeedCell,
    SeedClearMarker, StateKey, VERDICT_MIXED, capability_key_is_beta, is_stale_days,
    lane_strips_capability, resolve_display_action, resolve_display_verdict,
};

use super::sections::staleness_threshold_days;
use super::{CapabilityMatrixSource, DoctorContext, PriorCell};

/// How many observed capability keys outside the well-known set are
/// rendered as their own columns before the rest collapse into a
/// `(+N more)` overflow count.
const OTHER_COLUMN_CAP: usize = 10;

/// Milliseconds per day, for converting a cell's last-seen age to whole days
/// against the operator staleness hint.
const MS_PER_DAY: i64 = 86_400_000;

/// Layer tags for a cell's winning layer.
const LAYER_OVERRIDE: &str = "override";
const LAYER_LEARNED: &str = "learned";
const LAYER_PRIOR: &str = "prior";
const LAYER_SEED: &str = "seed";

/// One configured model that dispatches to a lane.
struct LaneModel<'a> {
    nickname: &'a str,
    entry: &'a ModelEntry,
}

/// A lane's identity plus the config bindings needed to consult overrides,
/// priors, and the strip policy for it.
struct LaneMeta<'a> {
    lane: String,
    provider_entry: Option<String>,
    provider_kind: &'static str,
    models: Vec<LaneModel<'a>>,
    routed: bool,
}

/// The learned source as the cells read it: the snapshot plus its pinned
/// clock anchors and the seed-clear markers the replay recorded.
struct Learned<'a> {
    entries: &'a [LearnedRegistryEntry],
    now: Option<(Instant, i64)>,
    seed_clears: &'a [SeedClearMarker],
}

/// The config-derived inputs every cell reads.
struct CellInputs<'a> {
    ctx: &'a DoctorContext,
    overrides: &'a OverrideRegistry,
    priors: &'a [PriorCell],
    learned: Learned<'a>,
    beta_seed: BetaSeedScope,
    today: i64,
    threshold: i64,
}

/// Build the capability matrix panel from the read-only doctor context. The
/// learned matrix source supplies the availability tri-state, the learned
/// cells, and the replay tally; the parsed config supplies lanes, overrides,
/// and priors. All three layers merge per cell through the shared resolvers.
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
            Learned {
                entries: entries.as_slice(),
                now: Some((*now, *now_ms)),
                seed_clears: seed_clears.as_slice(),
            },
            Some(*replay),
        ),
        CapabilityMatrixSource::Empty {
            replay,
            seed_clears,
        } => (
            MatrixAvailability::Empty,
            Learned {
                entries: &[],
                now: None,
                seed_clears: seed_clears.as_slice(),
            },
            Some(*replay),
        ),
        CapabilityMatrixSource::Unavailable(code) => (
            MatrixAvailability::Unavailable { code },
            Learned {
                entries: &[],
                now: None,
                seed_clears: &[],
            },
            None,
        ),
    };

    let overrides = OverrideRegistry::build(&ctx.config);
    let priors: &[PriorCell] = ctx.capability.config.as_ref().map_or(&[], |c| &c.priors);
    let lanes_meta = lane_metas(ctx, learned.entries);
    let seeded = seeded_columns(ctx.beta_seed, &lanes_meta);
    let (columns, other_overflow) = columns_for(learned.entries, priors, &overrides, &seeded);
    let inputs = CellInputs {
        ctx,
        overrides: &overrides,
        priors,
        learned,
        beta_seed: ctx.beta_seed,
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
                .map(|cap| build_cell(meta, cap, &inputs))
                .collect(),
        })
        .collect();

    CapabilityMatrixPanel {
        availability,
        columns,
        other_overflow,
        lanes,
        replay,
    }
}

/// The lane rows, sorted by lane key: the router's learned-lane projection
/// of the config, then every learned key with no configured model on it.
fn lane_metas<'a>(ctx: &'a DoctorContext, entries: &[LearnedRegistryEntry]) -> Vec<LaneMeta<'a>> {
    let projection = LearnedLaneProjection::from_config(&ctx.config);
    let mut metas: Vec<LaneMeta<'a>> = projection
        .lanes()
        .iter()
        .map(|projected| LaneMeta {
            lane: projected.lane.as_lane_key().to_string(),
            provider_entry: Some(projected.lane.provider_entry().to_string()),
            provider_kind: projected.provider_kind,
            models: projected
                .nicknames
                .iter()
                .filter_map(|nickname| ctx.config.models.get_key_value(nickname))
                .map(|(nickname, entry)| LaneModel { nickname, entry })
                .collect(),
            routed: projected.routed,
        })
        .collect();

    let projected: BTreeSet<&str> = projection
        .lanes()
        .iter()
        .map(|lane| lane.lane.as_lane_key())
        .collect();
    let unrouted: BTreeSet<&str> = entries
        .iter()
        .map(|entry| entry.state_key.as_str())
        .filter(|key| !projected.contains(key))
        .collect();
    metas.extend(unrouted.into_iter().map(|key| {
        let provider_entry = StateKey::parse(key).map(|lane| lane.provider_entry().to_string());
        let provider_kind = provider_entry
            .as_deref()
            .map_or("", |entry| provider_kind_for(ctx, entry));
        LaneMeta {
            lane: key.to_string(),
            provider_entry,
            provider_kind,
            models: Vec::new(),
            routed: false,
        }
    }));
    metas
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
/// set (from learned entries, priors, overrides, and the seed), sorted and
/// capped at [`OTHER_COLUMN_CAP`]. The second return value is the count of
/// observed other keys beyond the cap.
fn columns_for(
    entries: &[LearnedRegistryEntry],
    priors: &[PriorCell],
    overrides: &OverrideRegistry,
    seeded: &[String],
) -> (Vec<String>, u32) {
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

/// Resolve one `(lane, capability)` cell: consult the three layers per
/// nickname, run the shared display resolvers, then layer on the display-only
/// age, timestamps, and stale flag.
fn build_cell(meta: &LaneMeta, capability: &str, inputs: &CellInputs) -> MatrixCell {
    let learned_entry = inputs
        .learned
        .entries
        .iter()
        .find(|e| e.state_key == meta.lane && e.feature_key == capability);
    let prior_stamp = lane_prior(meta, capability, inputs.priors);
    let signals = CellSignals {
        learned: learned_entry.map(|e| (e.verdict, e.source)),
        learned_acting: learned_entry
            .zip(inputs.learned.now)
            .map(|(entry, (now, _))| LearnedActing::from_entry(entry, now)),
        seed: seed_cell(meta, capability, inputs),
        prior: prior_stamp.map(|(supported, _)| supported),
    };
    let resolved = resolve_per_nickname(meta, capability, inputs, signals);
    let shared_display = agreed(&resolved, |r| r.display);
    let shared_action = agreed(&resolved, |r| r.action);
    let nickname_actions = if shared_display.is_some() && shared_action.is_some() {
        Vec::new()
    } else {
        resolved
            .iter()
            .filter_map(Resolved::nickname_action)
            .collect()
    };

    let layer = shared_display.and_then(layer_of);
    let (age_ms, stale) = match layer {
        Some(LAYER_LEARNED) => learned_age(
            learned_entry,
            &inputs.learned,
            shared_display.and_then(|d| d.supported),
            inputs,
        ),
        Some(LAYER_PRIOR) => (
            None,
            prior_stamp.is_some_and(|(_, verified_at)| {
                is_stale_days(verified_at, inputs.today, inputs.threshold)
            }),
        ),
        _ => (None, false),
    };
    let stamps = learned_entry
        .zip(inputs.learned.now)
        .map(|(entry, anchor)| entry_stamps(entry, anchor));

    MatrixCell {
        verdict: shared_display.map_or(VERDICT_MIXED, |d| d.verdict),
        supported: shared_display.and_then(|d| d.supported),
        source: shared_display.and_then(|d| d.source),
        layer,
        action: shared_action.unwrap_or(ACTION_MIXED),
        age_ms,
        stale,
        first_seen_ms: stamps.map(|s| s.first_seen),
        last_seen_ms: stamps.map(|s| s.last_seen),
        expires_at_ms: stamps.and_then(|s| s.expires_at),
        nickname_actions,
    }
}

/// The learned, seed and prior signals one cell resolves against. The learned
/// and seed halves are the lane's and shared by every nickname; the prior is
/// per nickname, because the catalog records priors by nickname.
#[derive(Clone, Copy)]
struct CellSignals {
    learned: Option<(
        routectl_core::capability::Verdict,
        routectl_core::capability::EvidenceSource,
    )>,
    learned_acting: Option<LearnedActing>,
    seed: Option<SeedCell>,
    prior: Option<bool>,
}

/// The seed's state on a cell: `None` when the seed does not cover the lane's
/// kind and the column, otherwise cleared when the replay recorded a marker
/// for the cell under the lane's kind, else withheld.
fn seed_cell(meta: &LaneMeta, capability: &str, inputs: &CellInputs) -> Option<SeedCell> {
    if !inputs.beta_seed.covers(meta.provider_kind, capability) {
        return None;
    }
    let cleared = inputs.learned.seed_clears.iter().any(|marker| {
        marker.state_key == meta.lane
            && marker.feature_key == capability
            && marker.provider_kind == meta.provider_kind
    });
    Some(if cleared {
        SeedCell::Cleared
    } else {
        SeedCell::Withhold
    })
}

/// One nickname's resolution of a cell, or the lane's own for a lane no model
/// maps (`nickname` is then `None`).
struct Resolved {
    nickname: Option<String>,
    display: DisplayVerdict,
    action: &'static str,
}

impl Resolved {
    fn nickname_action(&self) -> Option<MatrixNicknameAction> {
        self.nickname.as_ref().map(|nickname| MatrixNicknameAction {
            nickname: nickname.clone(),
            verdict: self.display.verdict,
            layer: layer_of(self.display),
            action: self.action,
        })
    }
}

/// The value every resolution agrees on, or `None` when any two differ.
fn agreed<T: PartialEq + Copy>(resolved: &[Resolved], field: impl Fn(&Resolved) -> T) -> Option<T> {
    let first = field(resolved.first()?);
    resolved.iter().all(|r| field(r) == first).then_some(first)
}

/// Resolve the cell for each nickname on the lane through its own override
/// and its own beta pins. A lane no model maps resolves once, against the
/// bare provider-entry override (none when the entry is not configured).
fn resolve_per_nickname(
    meta: &LaneMeta,
    capability: &str,
    inputs: &CellInputs,
    signals: CellSignals,
) -> Vec<Resolved> {
    let (Some(provider), false) = (meta.provider_entry.as_deref(), meta.models.is_empty()) else {
        let override_cell = meta.provider_entry.as_deref().and_then(|provider| {
            inputs
                .overrides
                .resolve(provider, "", capability, meta.provider_kind)
        });
        let display =
            resolve_display_verdict(override_cell, signals.learned, signals.seed, signals.prior);
        let strips = lane_strips(meta, capability, inputs);
        return vec![Resolved {
            nickname: None,
            display,
            action: action_for(display, signals, strips, capability, inputs),
        }];
    };
    meta.models
        .iter()
        .map(|model| {
            let override_cell =
                inputs
                    .overrides
                    .resolve(provider, model.nickname, capability, meta.provider_kind);
            let signals = CellSignals {
                prior: nickname_prior(model.nickname, capability, inputs.priors)
                    .map(|(supported, _)| supported),
                ..signals
            };
            let display = resolve_display_verdict(
                override_cell,
                signals.learned,
                signals.seed,
                signals.prior,
            );
            let strips =
                lane_strips_capability(&inputs.ctx.config, provider, &[model.entry], capability);
            Resolved {
                nickname: Some(model.nickname.to_string()),
                display,
                action: action_for(display, signals, strips, capability, inputs),
            }
        })
        .collect()
}

/// The layer tag for a resolved display, or `None` for an unknown cell.
fn layer_of(display: DisplayVerdict) -> Option<&'static str> {
    display.source.map(|source| match source {
        "override" => LAYER_OVERRIDE,
        "prior" => LAYER_PRIOR,
        "seed" => LAYER_SEED,
        _ => LAYER_LEARNED,
    })
}

fn action_for(
    display: DisplayVerdict,
    signals: CellSignals,
    strip_applies: bool,
    capability: &str,
    inputs: &CellInputs,
) -> &'static str {
    resolve_display_action(ActionInputs {
        display,
        learned: signals.learned_acting,
        prior: signals.prior,
        strip_applies,
        capability_enabled: inputs.ctx.config.capability.enabled,
        beta_flag: capability_key_is_beta(capability),
        seed: signals.seed,
    })
}

/// The catalog prior stamp for a lane: the first mapped nickname whose prior
/// cell carries the capability, with its `verified_at` stamp.
fn lane_prior<'a>(
    meta: &LaneMeta,
    capability: &str,
    priors: &'a [PriorCell],
) -> Option<(bool, &'a str)> {
    meta.models
        .iter()
        .find_map(|m| nickname_prior(m.nickname, capability, priors))
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

/// Whether an F1 negative on this lane would be stripped in place.
fn lane_strips(meta: &LaneMeta, capability: &str, inputs: &CellInputs) -> bool {
    let Some(provider) = meta.provider_entry.as_deref() else {
        return false;
    };
    let models: Vec<&ModelEntry> = meta.models.iter().map(|m| m.entry).collect();
    lane_strips_capability(&inputs.ctx.config, provider, &models, capability)
}

/// The age and stale flag for a cell a learned entry won.
fn learned_age(
    entry: Option<&LearnedRegistryEntry>,
    learned: &Learned,
    supported: Option<bool>,
    inputs: &CellInputs,
) -> (Option<i64>, bool) {
    let age = entry.zip(learned.now).map(|(entry, (now, _))| {
        let elapsed = now.saturating_duration_since(entry.last_seen).as_millis();
        i64::try_from(elapsed).unwrap_or(i64::MAX)
    });
    // Only a verified positive (supported) carries a staleness flag; a
    // learned negative's freshness is governed by its decay window.
    let stale = supported == Some(true) && age.is_some_and(|a| a / MS_PER_DAY > inputs.threshold);
    (age, stale)
}

/// A learned entry's instants as epoch milliseconds.
#[derive(Clone, Copy)]
struct EntryStamps {
    first_seen: i64,
    last_seen: i64,
    expires_at: Option<i64>,
}

fn entry_stamps(entry: &LearnedRegistryEntry, (now, now_ms): (Instant, i64)) -> EntryStamps {
    let negative = !matches!(
        entry.verdict,
        routectl_core::capability::Verdict::VerifiedWorking
    );
    EntryStamps {
        first_seen: epoch_ms(entry.first_seen, now, now_ms),
        last_seen: epoch_ms(entry.last_seen, now, now_ms),
        expires_at: negative.then(|| epoch_ms(entry.expires_at, now, now_ms)),
    }
}

/// Map a monotonic instant onto the wall clock through the pinned anchor
/// pair, saturating rather than overflowing at either end.
fn epoch_ms(instant: Instant, now: Instant, now_ms: i64) -> i64 {
    if instant >= now {
        let ahead = instant.duration_since(now).as_millis();
        now_ms.saturating_add(i64::try_from(ahead).unwrap_or(i64::MAX))
    } else {
        let behind = now.duration_since(instant).as_millis();
        now_ms.saturating_sub(i64::try_from(behind).unwrap_or(i64::MAX))
    }
}

/// Provider kind (`kind_str`) for a configured provider, or `""` when the
/// provider is absent -- an empty kind normalizes as a pass-through, exactly
/// as the override registry treats an unconfigured provider.
fn provider_kind_for(ctx: &DoctorContext, provider: &str) -> &'static str {
    ctx.config
        .providers
        .get(provider)
        .map_or("", ProviderEntry::kind_str)
}
