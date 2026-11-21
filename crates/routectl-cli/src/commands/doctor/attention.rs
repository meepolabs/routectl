//! The needs-attention list: which current conditions need the operator,
//! how bad each is, what it is in plain words, and what fixes it.
//!
//! A pure builder. Plain snapshots go in; items, a verdict and counts come
//! out. Nothing here reads a clock, a file, the router or the config: the
//! caller pins `now_ms` and copies every input across. The kind list is
//! closed; renderers add no condition, wording or severity of their own.
//!
//! The verdict and the counts are folds of one item list
//! ([`Attention::from_items`]), so a caller that adds recovered items to the
//! active ones folds the merged list and gets a consistent summary.

use std::collections::{BTreeMap, BTreeSet};

use routectl_core::failure_class::LastOutcome;
use routectl_core::{EligibilityReason, UpstreamReason};
use routectl_router::runtime_state::UnusableCause;
use routectl_router::{Billing, Finding, Status};
use serde::{Serialize, Serializer};

use super::sections::POOLS_SECTION;
pub use crate::server::seat_driver::RefreshFailure;

#[cfg(test)]
#[path = "attention_tests.rs"]
mod tests;

/// A usage poll counts as failing once its current failure run is this old.
pub const POLL_FAILING_AFTER_MS: i64 = 15 * MS_PER_MIN;

/// A request-row loss counter rise keeps the list item active this long.
pub const RECORDS_DROPPED_HOLD_MS: i64 = 5 * MS_PER_MIN;

/// A non-stale window used past this fraction puts its account on the list.
pub const NEAR_LIMIT_USED: f64 = 0.90;

/// The longest scope tag shown in plain words.
const MAX_PLAIN_SCOPE_LEN: usize = 16;

const MS_PER_MIN: i64 = 60_000;
const MS_PER_HOUR: i64 = 60 * MS_PER_MIN;
const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;
const TWO_DAYS_MS: i64 = 2 * MS_PER_DAY;
const SECS_PER_HOUR: u64 = 3_600;
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;

/// Every input of one build, copied out of the daemon by the caller.
///
/// Two inputs carry a precondition the builder cannot check:
/// - `pools` must name every configured pool. A doctor Fail in the pools
///   section is left to `pool_empty` only for a pool named here; a Fail for a
///   pool missing from `pools` is listed as a doctor item.
/// - `record_loss.last_rise_ms` is when a loss counter last rose, which only a
///   caller comparing successive reads knows (the status panel's attention
///   memory fills it). A bare total says nothing about a rise, so with `None`
///   no `records_dropped` item is raised.
#[derive(Debug, Clone, Default)]
pub struct AttentionInputs {
    /// The instant the build describes, in epoch ms.
    pub now_ms: i64,
    /// Every configured pool's seat counts.
    pub pools: Vec<PoolSnapshot>,
    /// Every dispatch target's circuit state.
    pub routes: Vec<RouteSnapshot>,
    /// Every configured credential account.
    pub accounts: Vec<AccountSnapshot>,
    /// Whether each background seat driver is running.
    pub drivers: DriverSnapshot,
    /// How many managed (login) accounts the served config names.
    pub managed_accounts: usize,
    /// The request-row loss counters.
    pub record_loss: RecordLoss,
    /// The doctor report's findings; only failures become items.
    pub doctor_findings: Vec<Finding>,
}

/// One pool's seat counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolSnapshot {
    /// The `[pools]` key.
    pub pool: String,
    /// Seats the pool names.
    pub configured: usize,
    /// Seats a request could dispatch to now.
    pub eligible: usize,
}

/// One dispatch target's circuit state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSnapshot {
    /// The runtime state key (`nickname`, or `nickname#member` for a seat).
    pub state_key: String,
    /// The `[providers]` entry the target dispatches through (the member
    /// entry for a pool seat).
    pub provider_entry: String,
    /// The target's credential account, when it has one.
    pub account: Option<String>,
    /// The outcome of the target's last settled dispatch, when one has
    /// settled. Open routes on one provider entry share a cause only when
    /// this agrees.
    pub last_outcome: Option<LastOutcome>,
    /// Whether the target's circuit is open (not half-open).
    pub circuit_open: bool,
    /// When the circuit last opened, in epoch ms.
    pub open_since_ms: Option<i64>,
}

/// One credential account.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountSnapshot {
    /// The account key (`provider#label`).
    pub account: String,
    /// The display name, e.g. `claude-a (seat-a)`.
    pub display: String,
    /// The account's credential state.
    pub credential: CredentialState,
    /// The bare command that readmits the account's credential, when one
    /// exists.
    pub fix: Option<String>,
    /// What readmits the account's credential in plain words, when no bare
    /// command does.
    pub remediation: Option<String>,
    /// The refresh driver's current failure run for the account.
    pub refresh_failure: Option<RefreshFailure>,
    /// The usage poll's current failure run for the account.
    pub poll_failure: Option<PollFailure>,
    /// The account's usage windows.
    pub windows: Vec<WindowSnapshot>,
    /// The budget the upstream last reported billing against.
    pub billing: Billing,
    /// Whether that billing claim is known and recent enough to act on.
    pub billing_fresh: bool,
}

/// Whether an account's credential can be used, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialState {
    /// Nothing is known against the credential.
    Usable,
    /// The credential is known not to work until it is replaced.
    Unusable {
        /// Why.
        cause: UnusableCause,
        /// When the account was first marked, in epoch ms.
        since_ms: i64,
    },
    /// The upstream rejected the credential; the park lapses on its own.
    Parked {
        /// The rejection's reason.
        reason: UpstreamReason,
        /// When the park lapses into a re-test, in epoch ms.
        until_ms: i64,
    },
}

/// A current run of failed usage polls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollFailure {
    /// When the run began, in epoch ms.
    pub since_ms: i64,
    /// The raw result token of the latest poll, when known.
    pub last_result: Option<String>,
}

/// One usage window of an account.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSnapshot {
    /// The window length, in seconds.
    pub length_secs: u64,
    /// The vendor scope tag, `None` for an account-wide window.
    pub scope: Option<String>,
    /// The used fraction, in `[0.0, 1.0]`.
    pub used: f64,
    /// Whether the reading is too old or past its reset to count.
    pub stale: bool,
    /// When the window resets, in epoch ms.
    pub reset_ms: Option<i64>,
}

/// Whether each background seat driver is running.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DriverSnapshot {
    /// The token refresh driver.
    pub refresh_running: bool,
    /// The usage poll driver.
    pub poll_running: bool,
}

/// The request-row loss counters and when one last rose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordLoss {
    /// Rows the full writer channel refused.
    pub dropped_full: u64,
    /// Rows refused because the writer channel was closed.
    pub dropped_closed: u64,
    /// Writes the writer failed to persist.
    pub write_errors: u64,
    /// When any of the three last rose, in epoch ms.
    pub last_rise_ms: Option<i64>,
}

/// The closed list of conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttentionKind {
    /// A pool with seats configured and none usable.
    PoolEmpty,
    /// Several routes on one provider entry with open circuits after the
    /// same last outcome.
    ProviderFailing,
    /// A route whose circuit is open.
    RouteCircuitOpen,
    /// An account whose credential is unusable.
    AccountUnusable,
    /// An account whose credential is parked.
    AccountParked,
    /// An account whose token refresh keeps failing.
    AccountRefreshFailing,
    /// A background seat driver not running while managed accounts exist.
    DriverStopped,
    /// An account with a window used past the near-limit fraction.
    AccountNearLimit,
    /// An account the upstream reports billing beyond its included budget.
    AccountOverage,
    /// An account whose usage poll has failed for a while.
    UsagePollFailing,
    /// A request-row loss counter rose recently.
    RecordsDropped,
    /// A failing doctor finding.
    DoctorFailure,
}

impl AttentionKind {
    /// Every kind, in list order: broken kinds before degraded ones, so
    /// list order is worst first.
    pub const ALL: [Self; 12] = [
        Self::PoolEmpty,
        Self::ProviderFailing,
        Self::RouteCircuitOpen,
        Self::AccountUnusable,
        Self::AccountParked,
        Self::AccountRefreshFailing,
        Self::DriverStopped,
        Self::AccountNearLimit,
        Self::AccountOverage,
        Self::UsagePollFailing,
        Self::RecordsDropped,
        Self::DoctorFailure,
    ];

    /// The wire token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PoolEmpty => "pool_empty",
            Self::ProviderFailing => "provider_failing",
            Self::RouteCircuitOpen => "route_circuit_open",
            Self::AccountUnusable => "account_unusable",
            Self::AccountParked => "account_parked",
            Self::AccountRefreshFailing => "account_refresh_failing",
            Self::DriverStopped => "driver_stopped",
            Self::AccountNearLimit => "account_near_limit",
            Self::AccountOverage => "account_overage",
            Self::UsagePollFailing => "usage_poll_failing",
            Self::RecordsDropped => "records_dropped",
            Self::DoctorFailure => "doctor_failure",
        }
    }

    /// How bad an item of this kind is.
    pub const fn severity(self) -> Severity {
        match self {
            Self::PoolEmpty => Severity::Broken,
            _ => Severity::Degraded,
        }
    }

    /// Whether an item of this kind can move the verdict. Doctor failures and
    /// dropped records are listed and counted but say nothing about routing.
    pub const fn moves_verdict(self) -> bool {
        !matches!(self, Self::RecordsDropped | Self::DoctorFailure)
    }

    /// The view that shows the item's detail.
    pub const fn target(self) -> ViewTarget {
        match self {
            Self::ProviderFailing | Self::RouteCircuitOpen => ViewTarget::Models,
            Self::RecordsDropped => ViewTarget::Traffic,
            Self::DoctorFailure => ViewTarget::Doctor,
            Self::PoolEmpty
            | Self::AccountUnusable
            | Self::AccountParked
            | Self::AccountRefreshFailing
            | Self::DriverStopped
            | Self::AccountNearLimit
            | Self::AccountOverage
            | Self::UsagePollFailing => ViewTarget::Accounts,
        }
    }

    fn list_rank(self) -> usize {
        Self::ALL
            .iter()
            .position(|kind| *kind == self)
            .unwrap_or(usize::MAX)
    }
}

/// How bad an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Something is not working, routing still has a way through.
    Degraded,
    /// Requests have nowhere to go.
    Broken,
}

impl Severity {
    /// The wire token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Degraded => "degraded",
            Self::Broken => "broken",
        }
    }
}

/// The verdict's severity: an item severity, or nothing to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VerdictSeverity {
    /// No active routing or account item.
    Ok,
    /// The worst active routing or account item is degraded.
    Degraded,
    /// The worst active routing or account item is broken.
    Broken,
}

impl VerdictSeverity {
    /// The wire token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Broken => "broken",
        }
    }
}

/// Whether an item's condition holds now or has cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemState {
    /// The condition holds now.
    Active,
    /// The condition has cleared.
    Recovered,
}

impl ItemState {
    /// The wire token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Recovered => "recovered",
        }
    }
}

/// The dashboard view that shows an item's detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewTarget {
    /// Accounts, pools and seat drivers.
    Accounts,
    /// Models and their routes.
    Models,
    /// Traffic read from the usage ledger.
    Traffic,
    /// The doctor report.
    Doctor,
}

impl ViewTarget {
    /// The wire token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accounts => "accounts",
            Self::Models => "models",
            Self::Traffic => "traffic",
            Self::Doctor => "doctor",
        }
    }
}

macro_rules! serialize_as_token {
    ($($ty:ty),+) => {$(
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
    )+};
}

serialize_as_token!(
    AttentionKind,
    Severity,
    VerdictSeverity,
    ItemState,
    ViewTarget
);

/// One entry of the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttentionItem {
    /// Stable identity of the condition: the kind plus its subject's key.
    pub key: String,
    /// The condition.
    pub kind: AttentionKind,
    /// How bad it is.
    pub severity: Severity,
    /// Whether it holds now.
    pub state: ItemState,
    /// What it is about, in display form.
    pub subject: String,
    /// The credential account it is about, when it is about one.
    pub account: Option<String>,
    /// Plain words, with the age where one is known.
    pub cause: String,
    /// The machine form, for grep and copy.
    pub raw_token: String,
    /// When the condition began, in epoch ms, when known.
    pub since_ms: Option<i64>,
    /// When the condition lapses on its own, in epoch ms, when known.
    pub until_ms: Option<i64>,
    /// When the condition was first seen cleared, in epoch ms.
    pub recovered_at_ms: Option<i64>,
    /// The bare command that fixes it, copyable as is, when one exists.
    pub fix: Option<String>,
    /// What fixes it in plain words, when no bare command does.
    pub remediation: Option<String>,
    /// The view that shows its detail.
    pub target: Option<ViewTarget>,
    /// The state keys of the routes the item names, sorted, when it names
    /// several.
    pub targets: Option<Vec<String>>,
}

impl AttentionItem {
    /// The same item, marked cleared at `at_ms`.
    #[must_use]
    pub fn into_recovered(self, at_ms: i64) -> Self {
        Self {
            state: ItemState::Recovered,
            recovered_at_ms: Some(at_ms),
            ..self
        }
    }

    fn is_active(&self) -> bool {
        self.state == ItemState::Active
    }
}

/// The one-line summary of the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// The worst active routing or account item's severity, or `ok`.
    pub severity: VerdictSeverity,
    /// One sentence.
    pub text: String,
    /// The view the worst routing or account item points at; `None` when
    /// no such item is active.
    pub target: Option<ViewTarget>,
}

/// Item tallies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Active broken items.
    pub broken: usize,
    /// Active degraded items.
    pub degraded: usize,
    /// Recovered items.
    pub recovered: usize,
    /// Active doctor failures.
    pub doctor_fail: usize,
}

/// The list with its verdict and counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attention {
    /// The summary.
    pub verdict: Verdict,
    /// The tallies.
    pub counts: Counts,
    /// Every item, active first, worst first.
    pub items: Vec<AttentionItem>,
}

impl Attention {
    /// Order `items` and fold them into the verdict and counts. A caller
    /// holding recovered items passes them in the same list; they are listed
    /// but never counted as active and never move the verdict.
    pub fn from_items(mut items: Vec<AttentionItem>) -> Self {
        items.sort_by(|a, b| {
            (a.state, a.kind.list_rank(), &a.key).cmp(&(b.state, b.kind.list_rank(), &b.key))
        });
        Self {
            verdict: verdict_of(&items),
            counts: counts_of(&items),
            items,
        }
    }
}

fn counts_of(items: &[AttentionItem]) -> Counts {
    let mut counts = Counts::default();
    for item in items {
        let tally = match (item.state, item.severity) {
            (ItemState::Recovered, _) => &mut counts.recovered,
            (ItemState::Active, Severity::Broken) => &mut counts.broken,
            (ItemState::Active, Severity::Degraded) => &mut counts.degraded,
        };
        *tally += 1;
        if item.is_active() && item.kind == AttentionKind::DoctorFailure {
            counts.doctor_fail += 1;
        }
    }
    counts
}

/// Expects `items` sorted active first, worst first.
fn verdict_of(items: &[AttentionItem]) -> Verdict {
    let (routing, other): (Vec<&AttentionItem>, Vec<&AttentionItem>) = items
        .iter()
        .filter(|item| item.is_active())
        .partition(|item| item.kind.moves_verdict());
    let others = others_text(other.len());
    match routing.first() {
        Some(worst) => {
            let more = match routing.len() - 1 {
                0 => String::new(),
                n => format!(", and {n} more"),
            };
            Verdict {
                severity: match worst.severity {
                    Severity::Broken => VerdictSeverity::Broken,
                    Severity::Degraded => VerdictSeverity::Degraded,
                },
                text: format!("{}: {}{more}{others}", worst.subject, worst.cause),
                target: worst.target,
            }
        }
        None => Verdict {
            severity: VerdictSeverity::Ok,
            text: format!("All routes healthy{others}"),
            target: None,
        },
    }
}

fn others_text(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => "; 1 other item needs attention".to_string(),
        n => format!("; {n} other items need attention"),
    }
}

/// The list for `inputs`: every active item, folded.
pub fn build(inputs: &AttentionInputs) -> Attention {
    Attention::from_items(active_items(inputs))
}

/// One build's active items and the keys of the conditions it left out
/// because a worse item names them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveItems {
    /// Every condition that holds, one item per cause, in list order.
    pub items: Vec<AttentionItem>,
    /// The keys of conditions that hold but are named by another item in
    /// `items`: a refresh failing on an account whose credential item is
    /// listed, an open route whose account is unusable or parked, an open
    /// route a `provider_failing` item names, and a pools-section doctor Fail
    /// for a pool a listed `pool_empty` names. None of them has cleared.
    pub subsumed: BTreeSet<String>,
}

/// Every condition that holds at `inputs.now_ms`, one item per cause, in
/// list order. A route blocked by its account's credential is named by the
/// account item, and open routes sharing a provider entry and a last outcome
/// by one `provider_failing` item, not once per route.
pub fn active_items(inputs: &AttentionInputs) -> Vec<AttentionItem> {
    active_items_with_subsumed(inputs).items
}

/// [`active_items`], with the keys it left out because another item names
/// them.
pub fn active_items_with_subsumed(inputs: &AttentionInputs) -> ActiveItems {
    let now = inputs.now_ms;
    let mut out = ActiveItems {
        items: inputs.pools.iter().filter_map(pool_item).collect(),
        subsumed: BTreeSet::new(),
    };
    let pool_empty_keys: BTreeSet<String> = out.items.iter().map(|item| item.key.clone()).collect();
    route_items(inputs, &mut out);
    for account in &inputs.accounts {
        account_items(account, now, &mut out);
    }
    out.items.extend(driver_items(inputs));
    out.items.extend(records_item(&inputs.record_loss, now));
    for finding in &inputs.doctor_findings {
        let Some(item) = doctor_item(finding) else {
            continue;
        };
        if pool_empty_names_it(&pool_empty_keys, finding) {
            out.subsumed.insert(item.key);
        } else {
            out.items.push(item);
        }
    }
    out
}

/// Whether `finding` is a pools-section finding about a pool that one of
/// `pool_empty_keys`, the `pool_empty` items this read lists, names.
fn pool_empty_names_it(pool_empty_keys: &BTreeSet<String>, finding: &Finding) -> bool {
    finding.section == POOLS_SECTION
        && pool_empty_keys.contains(&item_key(AttentionKind::PoolEmpty, &finding.name))
}

/// An item's stable identity: its kind plus its subject's key.
fn item_key(kind: AttentionKind, subject_key: &str) -> String {
    format!("{}:{subject_key}", kind.as_str())
}

/// The key of the item [`active_items`] lists as the cause of route
/// `state_key`: its account's credential item while that credential is
/// unusable or parked (listed whether or not the circuit is open), else,
/// with the circuit open, the `provider_failing` item it folds into or its
/// own `route_circuit_open` item. `None` for a route with nothing listed
/// against it or not in `inputs`.
pub fn cause_key_for_route(inputs: &AttentionInputs, state_key: &str) -> Option<String> {
    let route = inputs
        .routes
        .iter()
        .find(|route| route.state_key == state_key)?;
    if !route.circuit_open && blocking_account(inputs, route.account.as_deref()).is_none() {
        return None;
    }
    Some(route_cause_key(inputs, &provider_folds(inputs), route))
}

/// Open routes on one provider entry with one last outcome, keyed by the
/// entry and the outcome's token.
type ProviderFolds<'a> = BTreeMap<(&'a str, &'static str), Vec<&'a RouteSnapshot>>;

/// The open routes that fold into a `provider_failing` item: two or more on
/// one provider entry, none blocked by its account, with the same last
/// outcome. A route with no settled outcome shows no shared cause, so it
/// never folds.
fn provider_folds(inputs: &AttentionInputs) -> ProviderFolds<'_> {
    let mut folds = ProviderFolds::new();
    for route in &inputs.routes {
        if !route.circuit_open || blocking_account(inputs, route.account.as_deref()).is_some() {
            continue;
        }
        let Some(outcome) = route.last_outcome else {
            continue;
        };
        folds
            .entry((route.provider_entry.as_str(), outcome_token(outcome)))
            .or_default()
            .push(route);
    }
    folds.retain(|_, routes| routes.len() >= 2);
    folds
}

/// The key of the item that names `route`, given this build's provider
/// folds. Expects `route` open or blocked by its account.
fn route_cause_key(
    inputs: &AttentionInputs,
    folds: &ProviderFolds<'_>,
    route: &RouteSnapshot,
) -> String {
    let account_key = blocking_account(inputs, route.account.as_deref()).and_then(|account| {
        credential_kind(&account.credential).map(|kind| item_key(kind, &account.account))
    });
    if let Some(key) = account_key {
        return key;
    }
    let folded = route
        .last_outcome
        .map(|outcome| (route.provider_entry.as_str(), outcome_token(outcome)))
        .filter(|fold| folds.contains_key(fold));
    match folded {
        Some((entry, token)) => item_key(
            AttentionKind::ProviderFailing,
            &fold_subject_key(entry, token),
        ),
        None => item_key(AttentionKind::RouteCircuitOpen, &route.state_key),
    }
}

fn fold_subject_key(entry: &str, token: &str) -> String {
    format!("{entry}/{token}")
}

/// The wire token of a last outcome, as `/status/health` spells it and the
/// core enum serializes it.
const fn outcome_token(outcome: LastOutcome) -> &'static str {
    match outcome {
        LastOutcome::Ok => "ok",
        LastOutcome::RateLimited => "rate_limited",
        LastOutcome::Timeout => "timeout",
        LastOutcome::TransportError => "transport_error",
        LastOutcome::Http4xx => "http_4xx",
        LastOutcome::Http5xx => "http_5xx",
        LastOutcome::CircuitOpen => "circuit_open",
    }
}

const fn outcome_words(outcome: Option<LastOutcome>) -> &'static str {
    match outcome {
        None => "unknown",
        Some(LastOutcome::Ok) => "ok",
        Some(LastOutcome::RateLimited) => "rate limited",
        Some(LastOutcome::Timeout) => "timed out",
        Some(LastOutcome::TransportError) => "connection failed",
        Some(LastOutcome::Http4xx) => "request refused (4xx)",
        Some(LastOutcome::Http5xx) => "server error (5xx)",
        Some(LastOutcome::CircuitOpen) => "circuit open",
    }
}

/// One item per provider fold, then each open route listed under its own
/// item; a route another item names is reported as subsumed.
fn route_items(inputs: &AttentionInputs, out: &mut ActiveItems) {
    let folds = provider_folds(inputs);
    out.items.extend(
        folds
            .iter()
            .map(|((entry, token), routes)| provider_item(entry, token, routes, inputs.now_ms)),
    );
    for route in inputs.routes.iter().filter(|route| route.circuit_open) {
        let item = route_item(route, inputs.now_ms);
        if route_cause_key(inputs, &folds, route) == item.key {
            out.items.push(item);
        } else {
            out.subsumed.insert(item.key);
        }
    }
}

/// The plain-words cause of a credential state with its age, or `None` for
/// a usable credential.
pub fn credential_cause(state: &CredentialState, now_ms: i64) -> Option<String> {
    match *state {
        CredentialState::Usable => None,
        CredentialState::Unusable { cause, since_ms } => Some(format!(
            "{}, for {}",
            unusable_words(cause),
            span(now_ms - since_ms)
        )),
        CredentialState::Parked { until_ms, .. } => Some(format!(
            "Credential refused by the provider, retried in {}",
            span(until_ms - now_ms)
        )),
    }
}

const fn unusable_words(cause: UnusableCause) -> &'static str {
    match cause {
        UnusableCause::NotLoggedIn => "Not logged in",
        UnusableCause::Unreadable => "Credential cannot be read",
        UnusableCause::Expired => "Login expired",
        UnusableCause::Revoked => "Login revoked by the provider",
    }
}

/// A coarse length of time: under a minute, whole minutes, hours or days.
fn span(ms: i64) -> String {
    match ms {
        ..MS_PER_MIN => "under 1 min".to_string(),
        MS_PER_MIN..MS_PER_HOUR => format!("{} min", ms / MS_PER_MIN),
        MS_PER_HOUR..MS_PER_DAY => format!("{} h", ms / MS_PER_HOUR),
        MS_PER_DAY..TWO_DAYS_MS => "1 day".to_string(),
        _ => format!("{} days", ms / MS_PER_DAY),
    }
}

/// An item with the kind's own severity and target, active, not recovered.
struct Draft {
    kind: AttentionKind,
    subject_key: String,
    subject: String,
    account: Option<String>,
    cause: String,
    raw_token: String,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    fix: Option<String>,
    remediation: Option<String>,
    targets: Option<Vec<String>>,
}

impl Draft {
    fn finish(self) -> AttentionItem {
        AttentionItem {
            key: item_key(self.kind, &self.subject_key),
            kind: self.kind,
            severity: self.kind.severity(),
            state: ItemState::Active,
            subject: self.subject,
            account: self.account,
            cause: self.cause,
            raw_token: self.raw_token,
            since_ms: self.since_ms,
            until_ms: self.until_ms,
            recovered_at_ms: None,
            fix: self.fix,
            remediation: self.remediation,
            target: Some(self.kind.target()),
            targets: self.targets,
        }
    }

    fn for_account(kind: AttentionKind, account: &AccountSnapshot) -> Self {
        Self {
            account: Some(account.account.clone()),
            ..Self::for_subject(kind, &account.account, &account.display)
        }
    }

    fn for_subject(kind: AttentionKind, key: &str, subject: &str) -> Self {
        Self {
            kind,
            subject_key: key.to_string(),
            subject: subject.to_string(),
            account: None,
            cause: String::new(),
            raw_token: String::new(),
            since_ms: None,
            until_ms: None,
            fix: None,
            remediation: None,
            targets: None,
        }
    }
}

fn pool_item(pool: &PoolSnapshot) -> Option<AttentionItem> {
    (pool.configured > 0 && pool.eligible == 0).then(|| {
        Draft {
            cause: format!("No usable seat, 0 of {}", pool.configured),
            raw_token: format!(
                "{} (eligible=0, configured={})",
                AttentionKind::PoolEmpty.as_str(),
                pool.configured
            ),
            ..Draft::for_subject(AttentionKind::PoolEmpty, &pool.pool, &pool.pool)
        }
        .finish()
    })
}

/// The account `account` names when its credential is unusable or parked.
fn blocking_account<'a>(
    inputs: &'a AttentionInputs,
    account: Option<&str>,
) -> Option<&'a AccountSnapshot> {
    let key = account?;
    inputs
        .accounts
        .iter()
        .find(|a| a.account == key && credential_kind(&a.credential).is_some())
}

/// The kind of the item a credential state raises, or `None` for a usable
/// credential.
const fn credential_kind(state: &CredentialState) -> Option<AttentionKind> {
    match state {
        CredentialState::Usable => None,
        CredentialState::Unusable { .. } => Some(AttentionKind::AccountUnusable),
        CredentialState::Parked { .. } => Some(AttentionKind::AccountParked),
    }
}

/// "Circuit open", with the age since `open_since_ms` when known.
fn circuit_cause(prefix: &str, open_since_ms: Option<i64>, now: i64) -> String {
    open_since_ms.map_or_else(
        || prefix.to_string(),
        |since| format!("{prefix}, for {}", span(now - since)),
    )
}

/// Expects `route` open.
fn route_item(route: &RouteSnapshot, now: i64) -> AttentionItem {
    Draft {
        account: route.account.clone(),
        cause: circuit_cause("Circuit open", route.open_since_ms, now),
        raw_token: EligibilityReason::BreakerOpen.as_str().to_string(),
        since_ms: route.open_since_ms,
        ..Draft::for_subject(
            AttentionKind::RouteCircuitOpen,
            &route.state_key,
            &route.state_key,
        )
    }
    .finish()
}

/// One item for the open `routes` of provider entry `entry` whose last
/// outcome reads `token`. It carries their account only when they all share
/// it, and dates from the earliest open circuit.
fn provider_item(entry: &str, token: &str, routes: &[&RouteSnapshot], now: i64) -> AttentionItem {
    let mut targets: Vec<String> = routes.iter().map(|route| route.state_key.clone()).collect();
    targets.sort();
    let account = routes[0].account.clone().filter(|first| {
        routes
            .iter()
            .all(|route| route.account.as_ref() == Some(first))
    });
    let since_ms = routes.iter().filter_map(|route| route.open_since_ms).min();
    let prefix = format!(
        "Circuit open on {} routes, last outcome {}",
        routes.len(),
        outcome_words(routes[0].last_outcome)
    );
    Draft {
        account,
        cause: circuit_cause(&prefix, since_ms, now),
        raw_token: format!(
            "{} (last_outcome={token}, routes={})",
            EligibilityReason::BreakerOpen.as_str(),
            routes.len()
        ),
        since_ms,
        targets: Some(targets),
        ..Draft::for_subject(
            AttentionKind::ProviderFailing,
            &fold_subject_key(entry, token),
            entry,
        )
    }
    .finish()
}

fn account_items(account: &AccountSnapshot, now: i64, out: &mut ActiveItems) {
    let credential = credential_item(account, now);
    let refresh = refresh_item(account, now);
    let refresh = match (&credential, refresh) {
        (Some(_), Some(hidden)) => {
            out.subsumed.insert(hidden.key);
            None
        }
        (_, refresh) => refresh,
    };
    out.items.extend(
        [
            credential,
            refresh,
            near_limit_item(account, now),
            overage_item(account),
            poll_item(account, now),
        ]
        .into_iter()
        .flatten(),
    );
}

fn credential_item(account: &AccountSnapshot, now: i64) -> Option<AttentionItem> {
    let cause = credential_cause(&account.credential, now)?;
    let draft = match account.credential {
        CredentialState::Usable => return None,
        CredentialState::Unusable {
            cause: unusable,
            since_ms,
        } => Draft {
            raw_token: with_refresh(
                format!(
                    "{} ({})",
                    EligibilityReason::CredentialUnusable.as_str(),
                    unusable.as_str()
                ),
                account.refresh_failure.as_ref(),
            ),
            since_ms: Some(since_ms),
            ..Draft::for_account(AttentionKind::AccountUnusable, account)
        },
        CredentialState::Parked { reason, until_ms } => Draft {
            raw_token: with_refresh(
                format!(
                    "{} ({})",
                    EligibilityReason::AccountParked.as_str(),
                    reason.as_str()
                ),
                account.refresh_failure.as_ref(),
            ),
            until_ms: Some(until_ms),
            ..Draft::for_account(AttentionKind::AccountParked, account)
        },
    };
    Some(
        Draft {
            cause,
            fix: account.fix.clone(),
            remediation: account.remediation.clone(),
            ..draft
        }
        .finish(),
    )
}

/// A credential item's machine form. A failing refresh on an account whose
/// credential is already listed is the same cause seen from the driver, so it
/// is named here rather than listed again.
fn with_refresh(mark: String, refresh: Option<&RefreshFailure>) -> String {
    match refresh {
        Some(failure) => format!("{mark}; {}", refresh_token(failure)),
        None => mark,
    }
}

fn refresh_token(failure: &RefreshFailure) -> String {
    format!("refresh_failing ({})", failure.token)
}

fn refresh_item(account: &AccountSnapshot, now: i64) -> Option<AttentionItem> {
    let failure = account.refresh_failure.as_ref()?;
    Some(
        Draft {
            cause: format!(
                "Token refresh failing, for {}",
                span(now - failure.first_failure_ms)
            ),
            raw_token: refresh_token(failure),
            since_ms: Some(failure.first_failure_ms),
            fix: account.fix.clone(),
            remediation: account.remediation.clone(),
            ..Draft::for_account(AttentionKind::AccountRefreshFailing, account)
        }
        .finish(),
    )
}

fn near_limit_item(account: &AccountSnapshot, now: i64) -> Option<AttentionItem> {
    let fullest = account
        .windows
        .iter()
        .filter(|w| !w.stale && w.used > NEAR_LIMIT_USED)
        .max_by(|a, b| a.used.total_cmp(&b.used))?;
    let scope_words = fullest
        .scope
        .as_deref()
        .filter(|scope| is_plain_scope(scope))
        .map_or_else(String::new, |scope| format!(" {scope}"));
    let resets = fullest.reset_ms.map_or_else(String::new, |reset| {
        format!(", resets in {}", span(reset - now))
    });
    let scope_token = fullest
        .scope
        .as_deref()
        .map_or_else(String::new, |scope| format!(", scope={scope}"));
    Some(
        Draft {
            cause: format!(
                "{}% of the {}{scope_words} window used{resets}",
                used_percent(fullest.used),
                window_length(fullest.length_secs)
            ),
            raw_token: format!(
                "{} (used={}, length_secs={}{scope_token})",
                AttentionKind::AccountNearLimit.as_str(),
                fullest.used,
                fullest.length_secs
            ),
            until_ms: fullest.reset_ms,
            ..Draft::for_account(AttentionKind::AccountNearLimit, account)
        }
        .finish(),
    )
}

/// Whether a scope tag reads as a plain word (a model family such as
/// `opus`) rather than a vendor limit id; only plain words enter a cause.
fn is_plain_scope(scope: &str) -> bool {
    (1..=MAX_PLAIN_SCOPE_LEN).contains(&scope.len())
        && scope.bytes().all(|b| b.is_ascii_lowercase())
}

/// A used fraction as a whole percent, rounded up so a value past the
/// near-limit line never reads as the line itself, and held below 100 until
/// the window is actually full.
fn used_percent(used: f64) -> f64 {
    let percent = (used * 100.0).ceil();
    if used < 1.0 {
        percent.min(99.0)
    } else {
        percent
    }
}

/// A window length as an adjective: "5 h", "7 day".
fn window_length(secs: u64) -> String {
    if secs >= SECS_PER_DAY && secs.is_multiple_of(SECS_PER_DAY) {
        format!("{} day", secs / SECS_PER_DAY)
    } else if secs >= SECS_PER_HOUR && secs.is_multiple_of(SECS_PER_HOUR) {
        format!("{} h", secs / SECS_PER_HOUR)
    } else {
        format!("{} min", secs.div_ceil(60))
    }
}

/// Overage is read only from a fresh upstream billing claim, never from a
/// full window: a saturated window and an overage seat look alike, and an old
/// claim may describe a billing period that has since reset. It is a cost
/// cause, not a credential one, so it is listed beside any credential item.
fn overage_item(account: &AccountSnapshot) -> Option<AttentionItem> {
    let kind = AttentionKind::AccountOverage;
    (account.billing_fresh && account.billing == Billing::Overage).then(|| {
        Draft {
            cause: "Billing beyond the included plan, at extra-usage rates".to_string(),
            raw_token: format!("{} (billing=overage)", kind.as_str()),
            ..Draft::for_account(kind, account)
        }
        .finish()
    })
}

fn poll_item(account: &AccountSnapshot, now: i64) -> Option<AttentionItem> {
    let failure = account
        .poll_failure
        .as_ref()
        .filter(|failure| failure.since_ms <= now - POLL_FAILING_AFTER_MS)?;
    let kind = AttentionKind::UsagePollFailing;
    let raw_token = failure.last_result.as_deref().map_or_else(
        || kind.as_str().to_string(),
        |result| format!("{} ({result})", kind.as_str()),
    );
    Some(
        Draft {
            cause: format!("Usage poll failing, for {}", span(now - failure.since_ms)),
            raw_token,
            since_ms: Some(failure.since_ms),
            ..Draft::for_account(kind, account)
        }
        .finish(),
    )
}

fn driver_items(inputs: &AttentionInputs) -> Vec<AttentionItem> {
    if inputs.managed_accounts == 0 {
        return Vec::new();
    }
    let drivers = [
        (
            "refresh",
            "token refresh driver",
            inputs.drivers.refresh_running,
        ),
        ("poll", "usage poll driver", inputs.drivers.poll_running),
    ];
    drivers
        .into_iter()
        .filter(|(_, _, running)| !running)
        .map(|(name, subject, _)| {
            let kind = AttentionKind::DriverStopped;
            Draft {
                cause: "Not running".to_string(),
                raw_token: format!("{} ({name})", kind.as_str()),
                ..Draft::for_subject(kind, name, subject)
            }
            .finish()
        })
        .collect()
}

fn records_item(loss: &RecordLoss, now: i64) -> Option<AttentionItem> {
    let rose = loss
        .last_rise_ms
        .filter(|rose| now - rose <= RECORDS_DROPPED_HOLD_MS)?;
    let kind = AttentionKind::RecordsDropped;
    Some(
        Draft {
            cause: format!("Usage records dropped, last {} ago", span(now - rose)),
            raw_token: format!(
                "{} (dropped_full={}, dropped_closed={}, write_errors={})",
                kind.as_str(),
                loss.dropped_full,
                loss.dropped_closed,
                loss.write_errors
            ),
            since_ms: Some(rose),
            ..Draft::for_subject(kind, "ledger", "usage ledger")
        }
        .finish(),
    )
}

/// A doctor remediation is an instruction in prose, often quoting a command
/// among other words, so it is never the bare `fix`.
fn doctor_item(finding: &Finding) -> Option<AttentionItem> {
    if finding.status != Status::Fail {
        return None;
    }
    let path = format!("{}/{}", finding.section, finding.name);
    Some(
        Draft {
            cause: finding.detail.clone(),
            raw_token: format!("doctor_fail ({path})"),
            remediation: finding.remediation.clone(),
            ..Draft::for_subject(AttentionKind::DoctorFailure, &path, &finding.name)
        }
        .finish(),
    )
}
