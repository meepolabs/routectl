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
//! An exclusion keys on an endpoint identity derived by [`endpoint_key`], the
//! one derivation every reader uses. It is computed lazily: a request that
//! holds no exclusion never derives a key.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the dispatch walk adopts the request walk in a follow-up change"
    )
)]

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use routectl_core::ScopeRow;

use super::DispatchTarget;
use crate::config::{Config, ProviderEntry};

/// The identity of an upstream endpoint: a normalized origin
/// (`scheme://host:port`, host lowercased with any trailing dot folded, port
/// explicit or the scheme default), or `provider:<name>` when the provider
/// entry names no URL the router can classify. Never carries userinfo, a
/// path or a query, so printing it prints nothing beyond the origin.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct EndpointKey(String);

impl EndpointKey {
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
#[derive(Debug, Clone, Copy)]
pub(super) struct WalkTarget<'a> {
    /// The target's `[providers]` key.
    provider_name: &'a str,
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
            entry: config.providers.get(&target.provider_name),
            model: &target.model.nickname,
            run: run_start(chain, index, target),
        })
    }

    #[cfg(test)]
    pub(super) const fn fixture(
        provider_name: &'a str,
        entry: Option<&'a ProviderEntry>,
        model: &'a str,
        run: usize,
    ) -> Self {
        Self {
            provider_name,
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
}

/// The scope an [`Exclusion`] covers, as tallied by [`RequestWalk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExclusionScope {
    Endpoint,
    Model,
    ProbePool,
}

impl Exclusion {
    pub(super) const fn scope(&self) -> ExclusionScope {
        match self {
            Self::Endpoint(_) => ExclusionScope::Endpoint,
            Self::Model(_) => ExclusionScope::Model,
            Self::OutsideRun(_) => ExclusionScope::ProbePool,
        }
    }

    fn covers(&self, target: &WalkTarget<'_>, endpoint: &mut LazyEndpoint) -> bool {
        match self {
            Self::Endpoint(key) => endpoint.get(target) == key,
            Self::Model(model) => target.model == model,
            Self::OutsideRun(run) => target.run() != *run,
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
    pub(super) probe_pool: u32,
}

impl ExcludedBy {
    const fn tally(&mut self, scope: ExclusionScope) {
        let slot = match scope {
            ExclusionScope::Endpoint => &mut self.endpoint,
            ExclusionScope::Model => &mut self.model,
            ExclusionScope::ProbePool => &mut self.probe_pool,
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

    pub(super) fn exclusions(&self) -> &[Exclusion] {
        &self.exclusions
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
