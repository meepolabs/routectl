//! One `(learned lane, capability)` cell as the read-only surfaces show it.
//!
//! The read-only surfaces that describe learned lanes (the doctor capability
//! matrix today, the status learned projection next) key them by the
//! `provider_entry#upstream` lane the learned store shares with `capability
//! purge` and `provider probe`. This module builds the lane rows from the
//! config and resolves a cell through the router's precedence seam in display
//! mode, so every such surface reads the same tiers dispatch applies: the operator wire pin, the lane's representability,
//! overrides, the learned registry, the shipped beta seed, and catalog priors.
//!
//! A cell resolves once per nickname on the lane, because an override or a
//! model `header_extras` pin can be nickname-scoped. The cell's lane-wide view
//! holds a value only when every nickname agrees on it; each nickname's own
//! resolution stays available for the disagreeing case.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use routectl_core::capability::Verdict;
use routectl_router::{
    BetaSeedScope, CellContext, Config, DisplayInputs, Layer, LearnedInput, LearnedLaneProjection,
    LearnedRegistryEntry, ModelEntry, OverrideRegistry, ProviderEntry, Resolution, SeedCell,
    SeedClearMarker, StateKey, beta_seed_state_for_key, operator_pins_capability, resolve_display,
};

/// One configured model that dispatches to a lane.
pub struct LaneModel<'a> {
    pub nickname: &'a str,
    pub entry: &'a ModelEntry,
}

/// A lane row: its identity plus the config bindings a cell consults.
pub struct LaneMeta<'a> {
    /// The serialized lane key, or a legacy key verbatim.
    pub lane: String,
    /// The lane's provider entry; `None` for a legacy key with no separator.
    pub provider_entry: Option<String>,
    /// The lane's upstream model id; empty for a legacy key.
    pub upstream: String,
    /// The provider kind (`kind_str`), empty when the entry is not configured.
    pub provider_kind: &'static str,
    /// The configured models that map to the lane, sorted by nickname.
    pub models: Vec<LaneModel<'a>>,
    /// Whether the loaded config still maps the lane.
    pub routed: bool,
}

/// The lane rows, sorted by lane key: the router's learned-lane projection of
/// `config` (a pooled model contributes one lane per member entry), then every
/// learned key in `entries` the config no longer maps, surfaced unrouted
/// rather than dropped.
pub fn lane_metas<'a>(config: &'a Config, entries: &[LearnedRegistryEntry]) -> Vec<LaneMeta<'a>> {
    let projection = LearnedLaneProjection::from_config(config);
    let mut metas: Vec<LaneMeta<'a>> = projection
        .lanes()
        .iter()
        .map(|projected| LaneMeta {
            lane: projected.lane.as_lane_key().to_string(),
            provider_entry: Some(projected.lane.provider_entry().to_string()),
            upstream: projected.lane.upstream().to_string(),
            provider_kind: projected.provider_kind,
            models: projected
                .nicknames
                .iter()
                .filter_map(|nickname| config.models.get_key_value(nickname))
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
    metas.extend(unrouted.into_iter().map(|key| unrouted_meta(config, key)));
    metas
}

fn unrouted_meta<'a>(config: &Config, key: &str) -> LaneMeta<'a> {
    let parsed = StateKey::parse(key);
    let provider_entry = parsed
        .as_ref()
        .map(|lane| lane.provider_entry().to_string());
    let provider_kind = provider_entry
        .as_deref()
        .map_or("", |entry| provider_kind_for(config, entry));
    LaneMeta {
        lane: key.to_string(),
        provider_entry,
        upstream: parsed.map_or_else(String::new, |lane| lane.upstream().to_string()),
        provider_kind,
        models: Vec::new(),
        routed: false,
    }
}

/// Provider kind (`kind_str`) for a configured provider, or `""` when the
/// provider is absent: an empty kind normalizes as a pass-through, exactly
/// as the override registry treats an unconfigured provider.
fn provider_kind_for(config: &Config, provider: &str) -> &'static str {
    config
        .providers
        .get(provider)
        .map_or("", ProviderEntry::kind_str)
}

/// The learned registry snapshot a cell reads: the entries, the pinned
/// monotonic and epoch-ms clock anchors they were read against (`None` when
/// nothing is resident), and the seed-clear markers the registry holds.
#[derive(Clone, Copy)]
pub struct LearnedSnapshot<'a> {
    pub entries: &'a [LearnedRegistryEntry],
    pub now: Option<(Instant, i64)>,
    pub seed_clears: &'a [SeedClearMarker],
}

/// Everything a cell resolves against beyond its lane and capability.
/// `prior` answers a nickname's catalog prior for a capability.
pub struct CellSources<'a> {
    pub config: &'a Config,
    pub overrides: &'a OverrideRegistry,
    pub learned: LearnedSnapshot<'a>,
    pub beta_seed: BetaSeedScope,
    pub prior: &'a dyn Fn(&str, &str) -> Option<bool>,
}

/// One nickname's resolution of a cell, or the lane's own for a lane no
/// model maps (`nickname` is then `None`).
pub struct NicknameCell {
    pub nickname: Option<String>,
    pub resolution: Resolution,
}

/// A resolved `(lane, capability)` cell: the resident learned entry, when
/// one exists, and the per-nickname resolutions.
pub struct LaneCell<'a> {
    pub entry: Option<&'a LearnedRegistryEntry>,
    pub resolved: Vec<NicknameCell>,
}

/// The display signal a nickname's resolution shows. Two nicknames show the
/// same cell exactly when these agree.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Shown {
    pub verdict: &'static str,
    pub supported: Option<bool>,
    pub source: Option<&'static str>,
}

impl Shown {
    const fn of(resolution: &Resolution) -> Self {
        Self {
            verdict: resolution.display_verdict(),
            supported: resolution.display_supported(),
            source: resolution.display_source(),
        }
    }
}

impl LaneCell<'_> {
    /// The signal every nickname shows, or `None` when any two differ.
    pub fn shown(&self) -> Option<Shown> {
        self.agreed(Shown::of)
    }

    /// The display layer token of the signal every nickname shows; `None`
    /// when the shown signals differ or no tier holds one.
    pub fn layer(&self) -> Option<&'static str> {
        self.shown()
            .and(self.agreed(Resolution::display_layer))
            .flatten()
            .map(Layer::as_str)
    }

    /// The action token every nickname resolves, or `None` when any two
    /// differ.
    pub fn action(&self) -> Option<&'static str> {
        self.agreed(Resolution::as_str)
    }

    /// The tier every nickname's winner beat; `None` when they disagree or
    /// no lower tier had an opinion.
    pub fn overrode_layer(&self) -> Option<&'static str> {
        self.agreed(Resolution::overridden)
            .flatten()
            .map(Layer::as_str)
    }

    /// Whether every nickname agrees on the shown signal and the action. The
    /// overridden tier alone does not split a cell: it explains the winner,
    /// and [`Self::overrode_layer`] is simply absent when it differs.
    pub fn nicknames_agree(&self) -> bool {
        self.shown().is_some() && self.action().is_some()
    }

    fn agreed<T: PartialEq + Copy>(&self, field: impl Fn(&Resolution) -> T) -> Option<T> {
        let first = field(&self.resolved.first()?.resolution);
        self.resolved
            .iter()
            .all(|r| field(&r.resolution) == first)
            .then_some(first)
    }
}

/// Resolve `capability` on the lane through the seam's display mode, once per
/// nickname through its own override, prior and `header_extras` pin. A lane
/// no model maps resolves once, against the bare provider-entry override and
/// floor (neither exists when the entry is not configured).
pub fn resolve_cell<'a>(
    meta: &LaneMeta,
    capability: &str,
    sources: &CellSources<'a>,
) -> LaneCell<'a> {
    let entry = sources
        .learned
        .entries
        .iter()
        .find(|e| e.state_key == meta.lane && e.feature_key == capability);
    let lane = LaneInputs {
        learned: entry
            .zip(sources.learned.now)
            .and_then(|(entry, (now, _))| LearnedInput::from_entry(entry, now)),
        seed: seed_cell(meta, capability, sources),
        ctx: cell_context(meta, capability, sources.config),
    };
    let resolved = if meta.models.is_empty() {
        vec![NicknameCell {
            nickname: None,
            resolution: resolve_unmapped(meta, capability, sources, lane),
        }]
    } else {
        meta.models
            .iter()
            .map(|model| NicknameCell {
                nickname: Some(model.nickname.to_string()),
                resolution: resolve_nickname(meta, model, capability, sources, lane),
            })
            .collect()
    };
    LaneCell { entry, resolved }
}

/// The lane-wide tier inputs every nickname on the lane shares.
#[derive(Clone, Copy)]
struct LaneInputs {
    learned: Option<LearnedInput>,
    seed: Option<SeedCell>,
    ctx: CellContext,
}

fn resolve_nickname(
    meta: &LaneMeta,
    model: &LaneModel,
    capability: &str,
    sources: &CellSources,
    lane: LaneInputs,
) -> Resolution {
    let provider = meta.provider_entry.as_deref().unwrap_or("");
    resolve_display(DisplayInputs {
        pinned: operator_pins_capability(
            sources.config,
            provider,
            &model.entry.header_extras,
            capability,
        ),
        override_verdict: sources.overrides.resolve(
            provider,
            model.nickname,
            capability,
            meta.provider_kind,
        ),
        learned: lane.learned,
        seed: lane.seed,
        prior: (sources.prior)(model.nickname, capability),
        ctx: lane.ctx,
    })
}

fn resolve_unmapped(
    meta: &LaneMeta,
    capability: &str,
    sources: &CellSources,
    lane: LaneInputs,
) -> Resolution {
    let provider = meta.provider_entry.as_deref();
    resolve_display(DisplayInputs {
        pinned: provider.is_some_and(|provider| {
            operator_pins_capability(sources.config, provider, &BTreeMap::new(), capability)
        }),
        override_verdict: provider.and_then(|provider| {
            sources
                .overrides
                .resolve(provider, "", capability, meta.provider_kind)
        }),
        learned: lane.learned,
        seed: lane.seed,
        prior: None,
        ctx: lane.ctx,
    })
}

/// The cell's context on the lane's own wire. A legacy key with no provider
/// entry has no egress kind, so nothing on it is unrepresentable.
fn cell_context(meta: &LaneMeta, capability: &str, config: &Config) -> CellContext {
    match meta.provider_entry.as_deref() {
        Some(provider) => CellContext::for_lane(config, provider, &meta.upstream, capability),
        None => CellContext::new(capability, &config.capability, None, &meta.upstream),
    }
}

/// The seed's state on a cell, under the lane's kind: cleared when the
/// registry holds a seed-clear marker for the cell under that kind.
fn seed_cell(meta: &LaneMeta, capability: &str, sources: &CellSources) -> Option<SeedCell> {
    beta_seed_state_for_key(sources.beta_seed, meta.provider_kind, capability, || {
        sources.learned.seed_clears.iter().any(|marker| {
            marker.state_key == meta.lane
                && marker.feature_key == capability
                && marker.provider_kind == meta.provider_kind
        })
    })
}

/// A learned entry's instants as epoch milliseconds.
#[derive(Clone, Copy)]
pub struct EntryStamps {
    pub first_seen: i64,
    pub last_seen: i64,
    /// A negative's decay deadline; `None` for a positive, which never decays.
    pub expires_at: Option<i64>,
}

/// Map an entry's instants onto the wall clock through the pinned
/// `(now, now_ms)` anchor pair.
pub fn entry_stamps(entry: &LearnedRegistryEntry, (now, now_ms): (Instant, i64)) -> EntryStamps {
    let negative = !matches!(entry.verdict, Verdict::VerifiedWorking);
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
