//! `/status/models` panel: one row per learned lane the live router
//! dispatches to, with its live state, the cause the needs-attention list
//! names it under, what it has learned to refuse, its resolved capability
//! cells and its changed-in-transit counts.
//!
//! Every input is a read of one pinned router snapshot through the
//! read-only facade: the dispatch targets and their gate eligibility (read
//! by the health panel's own reading), each account's credential state, the
//! routed lanes' capability cells (resolved by the shared lane-cell
//! resolver) and the wire check's transit tally. No usage ledger is opened
//! and no doctor report is gathered, so a read costs memory reads only.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use routectl_core::EligibilityReason;
use routectl_router::router::RouteTargetStatus;
use routectl_router::wire_check_state::{LaneTransitRow, TransitLane, WireTransitSnapshot};
use serde::Serialize;

use super::attention::route_cause_keys;
use super::health::eligibility_reading;
use super::router_view::{AccountCredential, StatusRouterView};
use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, utc_rfc3339};
use crate::lane_cell::{ResolvedCell, ResolvedLane, encoded_value};

/// Wire-shape version of the models panel payload. A removed or renamed
/// field, or a field or token whose meaning changes, bumps it; an added field
/// does not.
pub const SCHEMA_VERSION: u32 = 1;

/// The `top_paths` entry that sums every path the transit tally counted past
/// its per-lane path cap. A real path is a JSON pointer, empty or starting
/// with `/`, so it never reads as this token.
pub const OTHER_PATHS: &str = "other";

/// One row per learned lane, problems first, then by lane.
///
/// `started_ms` is when the daemon's wire-check state was built; a reload
/// carries that state, so it is the daemon start and every `transit` count
/// is since then.
#[derive(Debug, Clone, Serialize)]
pub(super) struct ModelsPanel {
    started_ms: u64,
    rows: Vec<ModelRow>,
}

/// One learned lane: a `[providers]` entry and the upstream model id it
/// sends, which is the key the learned capability store, `capability purge`
/// and the transit tally share.
///
/// For a lane whose entry uses its own credential, `provider` and `upstream`
/// equal the usage ledger's `provider` and `upstream` columns for a request
/// served on it. A lane whose entry forwards the client's credential is the
/// exception: dispatch sends the client's requested model unchanged there,
/// so the ledger records that model as `upstream` while the lane carries the
/// configured one.
///
/// A pooled model is one row per pool member entry it dispatches through,
/// each listing the model's nickname; two nicknames on one entry and
/// upstream share one row.
#[derive(Debug, Clone, Serialize)]
struct ModelRow {
    /// `provider#upstream`.
    lane: String,
    /// The `[providers]` entry the lane egresses through (the member entry
    /// for a pool seat).
    provider: String,
    /// The upstream model id the lane sends.
    upstream: String,
    /// The configured model nicknames that dispatch to the lane, sorted.
    nicknames: Vec<String>,
    /// The credential account the lane's park and unusable mark cover, or
    /// `None` for an entry that forwards the client's credential.
    account: Option<String>,
    /// The login label of an `oauth://<provider>#<label>` credential, else
    /// `None`.
    login_label: Option<String>,
    /// The lane's live state, a [`ModelState`] token: the worst state among
    /// the lane's dispatch targets, ranked as admission ranks refusals.
    state: &'static str,
    /// Epoch-ms the state began, where the router knows it (an unusable
    /// credential's first mark, a breaker's opening); `None` otherwise.
    since_ms: Option<i64>,
    /// The key of the `/status/attention` item that names this lane's
    /// problem, so rows sharing one cause join one item; `None` when the
    /// list names none of the lane's targets.
    cause_key: Option<String>,
    /// The capabilities the lane has learned to refuse: every resolved cell
    /// whose shown signal comes from the learned tier and says unsupported.
    refusals: Vec<Refusal>,
    /// Every capability cell the doctor matrix shows for the lane, resolved
    /// the same way.
    capabilities: Vec<CapabilityCell>,
    transit: Transit,
}

/// One learned refusal, as the lane's resolved cell shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Refusal {
    capability: String,
    /// The display verdict token (`broken`, `suspect`, ... or `mixed`).
    verdict: &'static str,
    /// The routing action dispatch takes on the cell, or `mixed`.
    action: &'static str,
    /// The value the learned verdict applies (`shape:<v>` / `ceiling:<n>`),
    /// or `None` when it writes none.
    value: Option<String>,
}

/// One resolved capability cell's lane-wide view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CapabilityCell {
    capability: String,
    /// The display verdict token, or `mixed` when the nicknames differ.
    verdict: &'static str,
    /// The shown signal's polarity; `None` with no signal or when mixed.
    supported: Option<bool>,
    /// The shown signal's source tag; `None` with no signal or when mixed.
    source: Option<&'static str>,
    /// The tier that decides the shown signal; `None` with no signal or when
    /// mixed.
    layer: Option<&'static str>,
    /// The routing action dispatch takes on the cell, or `mixed`.
    action: &'static str,
}

/// The lane's response-leg changed-in-transit counts since `started_ms`.
/// `checked == 0` means nothing was measured: the wire check never sampled
/// the lane, or the lane was first seen after the tally had named its
/// lane cap and counts only in the tally's overflow bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Transit {
    /// Response comparisons that finished.
    checked: u64,
    /// The subset of `checked` with at least one changed field.
    changed: u64,
    /// Changed fields by sanitized path, most first, at most the tally's
    /// per-lane path cap, then one [`OTHER_PATHS`] entry summing the paths
    /// past it when there were any.
    top_paths: Vec<PathCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PathCount {
    path: String,
    count: u64,
}

/// A lane's live state. Closed: a new refusal reason is a compile error in
/// [`Self::of`], never a silent new token. Each refusal state spells the
/// eligibility reason's own token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModelState {
    /// Dispatchable.
    Ok,
    CredentialUnusable,
    AccountParked,
    BreakerOpen,
    RateLimited,
}

impl ModelState {
    /// Every state, in token order of the wire contract.
    #[cfg(test)]
    pub(super) const ALL: [Self; 5] = [
        Self::Ok,
        Self::CredentialUnusable,
        Self::AccountParked,
        Self::BreakerOpen,
        Self::RateLimited,
    ];

    /// The state a gate eligibility reading names. A request-only reason
    /// describes one request's needs, not the lane, and a gate reading
    /// never carries one, so it reads as `Ok`.
    pub(super) const fn of(reason: Option<EligibilityReason>) -> Self {
        match reason {
            None | Some(EligibilityReason::Capability | EligibilityReason::ContextWindow) => {
                Self::Ok
            }
            Some(EligibilityReason::CredentialUnusable) => Self::CredentialUnusable,
            Some(EligibilityReason::AccountParked) => Self::AccountParked,
            Some(EligibilityReason::BreakerOpen) => Self::BreakerOpen,
            Some(EligibilityReason::RateLimited) => Self::RateLimited,
        }
    }

    /// The wire token.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::CredentialUnusable => "credential_unusable",
            Self::AccountParked => "account_parked",
            Self::BreakerOpen => "breaker_open",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Everything one read is built from, copied out of one pinned router
/// snapshot at one instant.
pub(super) struct ModelInputs {
    pub(super) now_ms: i64,
    pub(super) targets: Vec<RouteTargetStatus>,
    /// The credential state of each account a target names.
    pub(super) credentials: BTreeMap<String, AccountCredential>,
    pub(super) lanes: Vec<ResolvedLane>,
    pub(super) transit: WireTransitSnapshot,
}

/// Build the panel from ONE router snapshot at the pinned `now` (a
/// monotonic instant and its epoch-ms anchor).
pub(super) fn build_from_view(view: &StatusRouterView, now: (Instant, i64)) -> ModelsPanel {
    let (instant, now_ms) = now;
    models_panel(ModelInputs {
        now_ms,
        targets: view.route_targets(instant),
        credentials: view.account_credentials(now),
        lanes: view.routed_lanes(now),
        transit: view.wire_transit_snapshot(),
    })
}

/// The panel for `inputs`. A target with no learned lane (a provider entry
/// carrying the reserved separator, which config validation refuses) has no
/// row.
pub(super) fn models_panel(inputs: ModelInputs) -> ModelsPanel {
    let causes = route_cause_keys(inputs.now_ms, &inputs.targets, &inputs.credentials);
    let mut by_lane: BTreeMap<String, Vec<RouteTargetStatus>> = BTreeMap::new();
    for target in inputs.targets {
        if let Some(lane) = target.learned_lane.as_ref() {
            by_lane
                .entry(lane.as_lane_key().to_string())
                .or_default()
                .push(target);
        }
    }
    let mut cells: BTreeMap<String, Vec<ResolvedCell>> = inputs
        .lanes
        .into_iter()
        .map(|lane| (lane.lane, lane.cells))
        .collect();
    let mut transit = transit_by_lane(inputs.transit.lanes);
    let mut rows: Vec<ModelRow> = by_lane
        .into_iter()
        .map(|(lane, targets)| {
            let parts = LaneParts {
                cells: cells.remove(&lane).unwrap_or_default(),
                transit: transit.remove(&lane).unwrap_or_else(unmeasured),
            };
            lane_row(lane, targets, parts, &causes, inputs.now_ms)
        })
        .collect();
    rows.sort_by(|a, b| {
        is_problem(b)
            .cmp(&is_problem(a))
            .then_with(|| a.lane.cmp(&b.lane))
    });
    ModelsPanel {
        started_ms: inputs.transit.started_ms,
        rows,
    }
}

fn is_problem(row: &ModelRow) -> bool {
    row.state != ModelState::Ok.as_str() || row.cause_key.is_some()
}

/// A lane's resolved cells and transit counts, joined on its key.
struct LaneParts {
    cells: Vec<ResolvedCell>,
    transit: Transit,
}

/// One lane's row from its dispatch targets. The lane's state, its start and
/// its cause come from its worst target (highest refusal priority, then
/// state key); when that target is named by no item, the cause is the first
/// one another target on the lane is named under.
fn lane_row(
    lane: String,
    mut targets: Vec<RouteTargetStatus>,
    parts: LaneParts,
    causes: &BTreeMap<String, String>,
    now_ms: i64,
) -> ModelRow {
    targets.sort_by(|a, b| {
        priority(b)
            .cmp(&priority(a))
            .then_with(|| a.state_key.cmp(&b.state_key))
    });
    let worst = &targets[0];
    let reading = eligibility_reading(worst, now_ms);
    let cause_key = targets
        .iter()
        .find_map(|target| causes.get(&target.state_key).cloned());
    let mut nicknames: Vec<String> = targets.iter().map(|t| t.nickname.clone()).collect();
    nicknames.sort_unstable();
    nicknames.dedup();
    let (provider, upstream) = worst.learned_lane.as_ref().map_or_else(
        || (String::new(), String::new()),
        |key| (key.provider_entry().to_string(), key.upstream().to_string()),
    );
    ModelRow {
        lane,
        provider,
        upstream,
        nicknames,
        account: worst.account.clone(),
        login_label: worst.login_label.clone(),
        state: ModelState::of(reading.reason).as_str(),
        since_ms: reading.since_ms,
        cause_key,
        refusals: parts.cells.iter().filter_map(refusal).collect(),
        capabilities: parts.cells.iter().map(capability_cell).collect(),
        transit: parts.transit,
    }
}

fn priority(target: &RouteTargetStatus) -> u8 {
    target.gate.reason.map_or(0, EligibilityReason::priority)
}

/// The cell as a learned refusal, when its shown signal is the learned
/// tier's and says unsupported.
fn refusal(cell: &ResolvedCell) -> Option<Refusal> {
    let shown = cell.shown?;
    (cell.layer == Some(LEARNED_LAYER) && shown.supported == Some(false)).then(|| Refusal {
        capability: cell.capability.clone(),
        verdict: shown.verdict,
        action: cell.action,
        value: cell.entry.as_ref().and_then(encoded_value),
    })
}

/// The display layer token of the learned capability registry.
const LEARNED_LAYER: &str = routectl_router::Layer::Learned.as_str();

fn capability_cell(cell: &ResolvedCell) -> CapabilityCell {
    CapabilityCell {
        capability: cell.capability.clone(),
        verdict: cell
            .shown
            .map_or(routectl_router::VERDICT_MIXED, |s| s.verdict),
        supported: cell.shown.and_then(|s| s.supported),
        source: cell.shown.and_then(|s| s.source),
        layer: cell.layer,
        action: cell.action,
    }
}

/// The tally's named lanes by lane key. The overflow bucket names no lane,
/// so it joins no row.
fn transit_by_lane(rows: Vec<LaneTransitRow>) -> BTreeMap<String, Transit> {
    rows.into_iter()
        .filter_map(|row| match &row.lane {
            TransitLane::Lane(key) => Some((key.as_lane_key().to_string(), transit_of(&row))),
            TransitLane::Other => None,
        })
        .collect()
}

fn transit_of(row: &LaneTransitRow) -> Transit {
    let mut top_paths: Vec<PathCount> = row
        .paths
        .iter()
        .map(|(path, count)| PathCount {
            path: path.as_str().to_string(),
            count: *count,
        })
        .collect();
    if row.other_paths > 0 {
        top_paths.push(PathCount {
            path: OTHER_PATHS.to_string(),
            count: row.other_paths,
        });
    }
    Transit {
        checked: row.checked,
        changed: row.changed,
        top_paths,
    }
}

const fn unmeasured() -> Transit {
    Transit {
        checked: 0,
        changed: 0,
        top_paths: Vec::new(),
    }
}

pub(super) async fn build(state: &StatusState) -> Panel<ModelsPanel> {
    let view = state.router.view();
    let panel = guard_panel(
        &state.builder_capacity,
        SCHEMA_VERSION,
        codes::DB_UNAVAILABLE,
        move || {
            let now = (
                Instant::now(),
                crate::local_time::unix_ms(time::OffsetDateTime::now_utc()),
            );
            let dto = build_from_view(&view, now);
            Panel::available(SCHEMA_VERSION, utc_rfc3339(now.1), dto)
        },
    )
    .await;
    state.observability.models.record(&panel);
    panel
}

pub(super) async fn handler(State(state): State<Arc<StatusState>>) -> Json<Panel<ModelsPanel>> {
    Json(build(&state).await)
}

#[cfg(test)]
pub(super) mod fixture;

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
