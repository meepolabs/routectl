//! Request-scoped walk state: the same-target retries one client request has
//! spent, the targets its failures have excluded, and the recognized upstream
//! pattern that excluded them.
//!
//! Shaped like [`super::repair_budget::RepairBudget`]: constructed once per
//! client request above the chain loop and threaded by `&mut`, never reset by
//! a fallback hop, so a retry count and an exclusion outlive the target that
//! produced them. Exclusions last only for the request; cross-request memory
//! stays with the breaker, the credential park and the capability filter.
//!
//! An exclusion keys on one of four identities: an endpoint (derived by
//! [`endpoint_key`], the one derivation every reader uses, and only when an
//! endpoint exclusion is held), a `[models]` entry, a chain run, or one
//! target's runtime-state key.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use routectl_core::ScopeRow;

use super::DispatchTarget;
use super::class_observe::DispatchSurface;
use crate::config::{Config, ProviderEntry};

/// The identity of an upstream endpoint: a normalized origin
/// (`scheme://host:port`, host lowercased with any trailing dot folded, port
/// explicit or the scheme default), or `provider:<name>` when the provider
/// entry names no URL the router can classify. Never carries userinfo, a
/// path or a query, so printing it prints nothing beyond the origin.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct EndpointKey(String);

impl EndpointKey {
    #[cfg(test)]
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EndpointKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for EndpointKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The endpoint key for provider entry `provider_name` (`entry` is its
/// `[providers]` row, `None` when the name resolves to none).
///
/// Reads the entry's effective egress base ([`egress_base_url`]); when it
/// answers none, or the URL fails the origin guard or has no known port,
/// falls back to `provider:<name>`, the narrowest key, which no other entry
/// shares.
pub(super) fn endpoint_key(provider_name: &str, entry: Option<&ProviderEntry>) -> EndpointKey {
    entry
        .and_then(egress_base_url)
        .and_then(|base| normalized_origin(&base))
        .map_or_else(
            || EndpointKey(format!("provider:{provider_name}")),
            EndpointKey,
        )
}

/// The base URL `entry` dials: [`ProviderEntry::classifiable_egress_base_url`]
/// (which answers a forwarded entry's own base too), else the Gemini base the
/// factory leaves in place, the same one the effective-config view reports.
fn egress_base_url(entry: &ProviderEntry) -> Option<Cow<'_, str>> {
    let classified = entry.classifiable_egress_base_url();
    #[cfg(feature = "gemini")]
    let classified = classified
        .or_else(|| crate::config_effective::gemini_effective_base_url(entry).map(Cow::Borrowed));
    classified
}

/// `scheme://host:port` for `base_url`. The origin comes from
/// [`crate::config_effective::endpoint_origin`], whose guard withholds any
/// URL whose written authority could put credential bytes in the host slot,
/// so the key never carries userinfo.
fn normalized_origin(base_url: &str) -> Option<String> {
    let origin = crate::config_effective::endpoint_origin(base_url)?;
    let url = url::Url::parse(&origin).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let port = url.port_or_known_default()?;
    Some(format!("{}://{host}:{port}", url.scheme()))
}

/// What the walk needs to know about one chain target to decide whether an
/// exclusion covers it. Built only by [`WalkTarget::at`], so the provider
/// entry, the `[models]` entry and the chain run always describe one target.
#[derive(Clone, Copy)]
pub(super) struct WalkTarget<'a> {
    /// The target's `[providers]` key.
    provider_name: &'a str,
    /// The target's runtime-state key: one per (`[models]` entry, seat), so
    /// two chain entries naming the same seat of the same entry share it.
    state_key: &'a str,
    /// That key's provider entry, when it resolves.
    entry: Option<&'a ProviderEntry>,
    /// The `[models]` entry the target dispatches to; every seat of one pool
    /// shares it.
    model: &'a str,
    /// The chain index where the contiguous run of targets dispatching to
    /// this target's `[models]` entry starts.
    run: usize,
}

impl<'a> WalkTarget<'a> {
    /// The walk view of `chain[index]`, its provider entry read from
    /// `config`; `None` when `index` is past the chain.
    pub(super) fn at(
        chain: &'a [DispatchTarget],
        index: usize,
        config: &'a Config,
    ) -> Option<Self> {
        let target = chain.get(index)?;
        Some(Self {
            provider_name: &target.provider_name,
            state_key: &target.state_key,
            entry: config.providers.get(&target.provider_name),
            model: &target.model.nickname,
            run: run_start(chain, index, target),
        })
    }

    /// A walk target whose state key is `model` itself, the key of a
    /// target that is not a pool seat.
    #[cfg(test)]
    pub(super) const fn fixture(
        provider_name: &'a str,
        entry: Option<&'a ProviderEntry>,
        model: &'a str,
        run: usize,
    ) -> Self {
        Self {
            provider_name,
            state_key: model,
            entry,
            model,
            run,
        }
    }

    pub(super) fn endpoint_key(&self) -> EndpointKey {
        endpoint_key(self.provider_name, self.entry)
    }

    pub(super) const fn model(&self) -> &'a str {
        self.model
    }

    pub(super) const fn run(&self) -> ChainRun {
        ChainRun(self.run)
    }

    pub(super) const fn state_key(&self) -> &'a str {
        self.state_key
    }
}

/// Prints the provider name, the `[models]` entry and the chain run only:
/// the provider entry's own Debug prints its base URL verbatim, userinfo
/// included.
impl fmt::Debug for WalkTarget<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalkTarget")
            .field("provider_name", &self.provider_name)
            .field("model", &self.model)
            .field("run", &self.run)
            .finish_non_exhaustive()
    }
}

/// The first index of the contiguous run of chain entries before `target`
/// (at `index`) that dispatch to the same resolved `[models]` entry.
fn run_start(chain: &[DispatchTarget], index: usize, target: &DispatchTarget) -> usize {
    let earlier_in_run = chain
        .get(..index)
        .unwrap_or_default()
        .iter()
        .rev()
        .take_while(|other| Arc::ptr_eq(&other.model, &target.model))
        .count();
    index - earlier_in_run
}

/// One contiguous run of chain targets sharing a `[models]` entry, named by
/// the chain index it starts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ChainRun(usize);

/// One request-scoped exclusion: the scope it covers and the key that
/// identifies the covered targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Exclusion {
    /// Every target served by this endpoint.
    Endpoint(EndpointKey),
    /// Every target dispatching to this `[models]` entry. Another entry
    /// sending the same upstream id (a larger-window variant, say) stays
    /// admitted.
    Model(String),
    /// Every target outside this chain run: an availability probe stays in
    /// the run of seats it started in.
    OutsideRun(ChainRun),
    /// Every chain entry with this runtime-state key: the one seat that
    /// failed, wherever the chain names it again.
    Target(String),
}

/// The scope an [`Exclusion`] covers, as tallied by [`RequestWalk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExclusionScope {
    Endpoint,
    Model,
    ProbeRun,
    Target,
}

impl Exclusion {
    pub(super) const fn scope(&self) -> ExclusionScope {
        match self {
            Self::Endpoint(_) => ExclusionScope::Endpoint,
            Self::Model(_) => ExclusionScope::Model,
            Self::OutsideRun(_) => ExclusionScope::ProbeRun,
            Self::Target(_) => ExclusionScope::Target,
        }
    }

    /// The fence a failed availability probe on `target` sets: the target
    /// itself, under every chain entry naming it, and every target outside
    /// its chain run. The run alone would admit the same seat again when the
    /// chain names it twice in a row (a repeated nickname, a nested alias,
    /// or an entry between them dropped before the walk).
    pub(super) fn probe_fence(target: &WalkTarget<'_>) -> [Self; 2] {
        [
            Self::Target(target.state_key().to_owned()),
            Self::OutsideRun(target.run()),
        ]
    }

    fn covers(&self, target: &WalkTarget<'_>, endpoint: &mut LazyEndpoint) -> bool {
        match self {
            Self::Endpoint(key) => endpoint.get(target) == key,
            Self::Model(model) => target.model == model,
            Self::OutsideRun(run) => target.run() != *run,
            Self::Target(state_key) => target.state_key == state_key,
        }
    }
}

#[derive(Default)]
struct LazyEndpoint(Option<EndpointKey>);

impl LazyEndpoint {
    fn get(&mut self, target: &WalkTarget<'_>) -> &EndpointKey {
        self.0.get_or_insert_with(|| target.endpoint_key())
    }
}

/// Targets skipped because an exclusion covered them, per scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ExcludedBy {
    pub(super) endpoint: u32,
    pub(super) model: u32,
    pub(super) probe_run: u32,
    pub(super) target: u32,
}

impl ExcludedBy {
    const fn tally(&mut self, scope: ExclusionScope) {
        let slot = match scope {
            ExclusionScope::Endpoint => &mut self.endpoint,
            ExclusionScope::Model => &mut self.model,
            ExclusionScope::ProbeRun => &mut self.probe_run,
            ExclusionScope::Target => &mut self.target,
        };
        *slot = slot.saturating_add(1);
    }
}

/// The walk state of one client request.
#[derive(Debug, Default)]
pub(super) struct RequestWalk {
    retries_used: u32,
    exclusions: Vec<Exclusion>,
    recognized: Option<ScopeRow>,
    excluded_by: ExcludedBy,
}

impl RequestWalk {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Same-target retries spent so far, across every target of the walk.
    pub(super) const fn retries_used(&self) -> u32 {
        self.retries_used
    }

    /// The most recent recognized upstream pattern, if any failure carried one.
    pub(super) const fn recognized(&self) -> Option<ScopeRow> {
        self.recognized
    }

    pub(super) const fn excluded_by(&self) -> ExcludedBy {
        self.excluded_by
    }

    #[cfg(test)]
    pub(super) fn exclusions(&self) -> &[Exclusion] {
        &self.exclusions
    }

    /// Whether the walk retried a target, passed over one an exclusion
    /// covered, or recognized an upstream pattern.
    fn took_detours(&self) -> bool {
        self.retries_used > 0
            || self.excluded_by != ExcludedBy::default()
            || self.recognized.is_some()
    }

    /// Log the walk's one summary line, when it took a detour. Counts and
    /// closed tokens only: `attempts` is the upstream calls the walk made,
    /// and nothing from a request, a response, a credential or an endpoint
    /// reaches a field.
    pub(super) fn log_summary(&self, surface: DispatchSurface, attempts: u32) {
        if !self.took_detours() {
            return;
        }
        let excluded = self.excluded_by();
        tracing::info!(
            surface = surface.as_str(),
            attempts,
            retries = self.retries_used(),
            excluded_endpoint = excluded.endpoint,
            excluded_model = excluded.model,
            excluded_target = excluded.target,
            excluded_probe_run = excluded.probe_run,
            recognizer = self.recognized().map_or("none", ScopeRow::as_str),
            "walk summary",
        );
    }

    /// Record a same-target retry the walk is about to make.
    pub(super) const fn spend_retry(&mut self) {
        self.retries_used = self.retries_used.saturating_add(1);
    }

    /// Record a recognized upstream pattern.
    pub(super) const fn recognize(&mut self, row: ScopeRow) {
        self.recognized = Some(row);
    }

    /// Add `exclusion` for the rest of the request; an exclusion already held
    /// is kept once.
    pub(super) fn exclude(&mut self, exclusion: Exclusion) {
        if !self.exclusions.contains(&exclusion) {
            self.exclusions.push(exclusion);
        }
    }

    /// Whether `target` may be dialed. An excluded target is tallied under
    /// the scope of the first exclusion covering it. Derives the target's
    /// endpoint key only when an endpoint exclusion is held.
    pub(super) fn admits(&mut self, target: &WalkTarget<'_>) -> bool {
        let mut endpoint = LazyEndpoint::default();
        let covering = self
            .exclusions
            .iter()
            .find(|exclusion| exclusion.covers(target, &mut endpoint))
            .map(Exclusion::scope);
        match covering {
            Some(scope) => {
                self.excluded_by.tally(scope);
                false
            }
            None => true,
        }
    }
}

#[cfg(test)]
#[path = "walk_state_tests.rs"]
mod walk_state_tests;

#[cfg(test)]
#[path = "walk_summary_tests.rs"]
mod walk_summary_tests;
