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
//! Each cell merges the three capability signal layers -- operator
//! overrides, the learned ledger-replay registry, and catalog priors --
//! through the shared pure resolvers (`resolve_display_verdict`,
//! `resolve_display_action`), so the panel cannot drift from the router's
//! precedence order. The action is resolved per nickname, because an
//! override or a pinned beta can be nickname-scoped; when the nicknames on a
//! lane disagree the cell reads `mixed` and carries each nickname's action.
//! Ages, timestamps, and stale flags are layered on top here (a display
//! concern the pure resolvers deliberately omit).

use std::collections::BTreeSet;
use std::time::Instant;

use routectl_core::capability::WELL_KNOWN_CAPABILITY_KEYS;
use routectl_router::{
    ACTION_MIXED, ActionInputs, CapabilityMatrixPanel, DisplayVerdict, LearnedActing,
    LearnedLaneProjection, LearnedRegistryEntry, MatrixAvailability, MatrixCell, MatrixLane,
    MatrixNicknameAction, ModelEntry, OverrideProvenance, OverrideRegistry, OverrideVerdict,
    ProviderEntry, StateKey, is_stale_days, lane_strips_capability, resolve_display_action,
    resolve_display_verdict,
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
/// clock anchors.
struct Learned<'a> {
    entries: &'a [LearnedRegistryEntry],
    now: Option<(Instant, i64)>,
}

/// The config-derived inputs every cell reads.
struct CellInputs<'a> {
    ctx: &'a DoctorContext,
    overrides: &'a OverrideRegistry,
    priors: &'a [PriorCell],
    learned: Learned<'a>,
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
        } => (
            MatrixAvailability::Available,
            Learned {
                entries: entries.as_slice(),
                now: Some((*now, *now_ms)),
            },
            Some(*replay),
        ),
        CapabilityMatrixSource::Empty { replay } => (
            MatrixAvailability::Empty,
            Learned {
                entries: &[],
                now: None,
            },
            Some(*replay),
        ),
        CapabilityMatrixSource::Unavailable(code) => (
            MatrixAvailability::Unavailable { code },
            Learned {
                entries: &[],
                now: None,
            },
            None,
        ),
    };

    let overrides = OverrideRegistry::build(&ctx.config);
    let priors: &[PriorCell] = ctx.capability.config.as_ref().map_or(&[], |c| &c.priors);
    let (columns, other_overflow) = columns_for(learned.entries, priors, &overrides);
    let lanes_meta = lane_metas(ctx, learned.entries);
    let inputs = CellInputs {
        ctx,
        overrides: &overrides,
        priors,
        learned,
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

/// The column keys: the well-known keys, then the observed keys outside that
/// set (from learned entries, priors, and overrides), sorted and capped at
/// [`OTHER_COLUMN_CAP`]. The second return value is the count of observed
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
/// shared display resolvers, then layer on the display-only age, timestamps,
/// and stale flag.
fn build_cell(meta: &LaneMeta, capability: &str, inputs: &CellInputs) -> MatrixCell {
    let override_cell = lane_override(meta, capability, inputs.overrides);
    let learned_entry = inputs
        .learned
        .entries
        .iter()
        .find(|e| e.state_key == meta.lane && e.feature_key == capability);
    let prior_stamp = lane_prior(meta, capability, inputs.priors);
    let prior = prior_stamp.map(|(supported, _)| supported);

    let display = resolve_display_verdict(
        override_cell,
        learned_entry.map(|e| (e.verdict, e.source)),
        prior,
    );
    let learned_acting = learned_entry
        .zip(inputs.learned.now)
        .map(|(entry, (now, _))| LearnedActing::from_entry(entry, now));
    let (action, nickname_actions) = cell_action(
        meta,
        capability,
        inputs,
        CellSignals {
            display,
            learned: learned_entry.map(|e| (e.verdict, e.source)),
            learned_acting,
            prior,
        },
    );

    let layer = display.source.map(|source| match source {
        "override" => LAYER_OVERRIDE,
        "prior" => LAYER_PRIOR,
        _ => LAYER_LEARNED,
    });
    let (age_ms, stale) = match layer {
        Some(LAYER_LEARNED) => {
            learned_age(learned_entry, &inputs.learned, display.supported, inputs)
        }
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
        verdict: display.verdict,
        supported: display.supported,
        source: display.source,
        layer,
        action,
        age_ms,
        stale,
        first_seen_ms: stamps.map(|s| s.first_seen),
        last_seen_ms: stamps.map(|s| s.last_seen),
        expires_at_ms: stamps.and_then(|s| s.expires_at),
        nickname_actions,
    }
}

/// The learned and prior signals one cell resolves against, shared by every
/// nickname on the lane.
#[derive(Clone, Copy)]
struct CellSignals {
    display: DisplayVerdict,
    learned: Option<(
        routectl_core::capability::Verdict,
        routectl_core::capability::EvidenceSource,
    )>,
    learned_acting: Option<LearnedActing>,
    prior: Option<bool>,
}

/// The cell's action, resolved for each nickname on the lane through its own
/// override and its own beta pins. Agreement yields that action and no
/// per-nickname list; disagreement yields [`ACTION_MIXED`] and every
/// nickname's action. A lane no model maps resolves once, against the bare
/// provider-entry override.
fn cell_action(
    meta: &LaneMeta,
    capability: &str,
    inputs: &CellInputs,
    signals: CellSignals,
) -> (&'static str, Vec<MatrixNicknameAction>) {
    let (Some(provider), false) = (meta.provider_entry.as_deref(), meta.models.is_empty()) else {
        let action = action_for(
            signals.display,
            signals,
            lane_strips(meta, capability, inputs),
            inputs,
        );
        return (action, Vec::new());
    };
    let per_nickname: Vec<MatrixNicknameAction> = meta
        .models
        .iter()
        .map(|model| {
            let override_cell =
                inputs
                    .overrides
                    .resolve(provider, model.nickname, capability, meta.provider_kind);
            let display = resolve_display_verdict(override_cell, signals.learned, signals.prior);
            let strips =
                lane_strips_capability(&inputs.ctx.config, provider, &[model.entry], capability);
            MatrixNicknameAction {
                nickname: model.nickname.to_string(),
                action: action_for(display, signals, strips, inputs),
            }
        })
        .collect();
    let first = per_nickname[0].action;
    if per_nickname.iter().all(|n| n.action == first) {
        (first, Vec::new())
    } else {
        (ACTION_MIXED, per_nickname)
    }
}

fn action_for(
    display: DisplayVerdict,
    signals: CellSignals,
    strip_applies: bool,
    inputs: &CellInputs,
) -> &'static str {
    resolve_display_action(ActionInputs {
        display,
        learned: signals.learned_acting,
        prior: signals.prior,
        strip_applies,
        capability_enabled: inputs.ctx.config.capability.enabled,
    })
}

/// The override resolution for a lane: the first model on it whose
/// `provider:nickname` or bare `provider` cell carries one, or the bare
/// provider-entry cell for an unrouted lane.
fn lane_override(
    meta: &LaneMeta,
    capability: &str,
    overrides: &OverrideRegistry,
) -> Option<(OverrideVerdict, OverrideProvenance)> {
    let provider = meta.provider_entry.as_deref()?;
    if meta.models.is_empty() {
        return overrides.resolve(provider, "", capability, meta.provider_kind);
    }
    meta.models
        .iter()
        .find_map(|m| overrides.resolve(provider, m.nickname, capability, meta.provider_kind))
}

/// The catalog prior for a lane: the first mapped nickname whose prior cell
/// carries the capability, with its `verified_at` stamp.
fn lane_prior<'a>(
    meta: &LaneMeta,
    capability: &str,
    priors: &'a [PriorCell],
) -> Option<(bool, &'a str)> {
    meta.models.iter().find_map(|m| {
        priors
            .iter()
            .find(|p| p.nickname == m.nickname)
            .and_then(|p| {
                p.capabilities
                    .iter()
                    .find(|(key, _)| key == capability)
                    .map(|(_, supported)| (*supported, p.verified_at.as_str()))
            })
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
