//! Provider selection: which table provider prices a config target.

use std::collections::BTreeSet;

use routectl_core::provider_kind::config as kind;

use super::format::ApiBase;
use super::{PriceTable, ProviderPrices};
use crate::field_verdict::loopback_target_suppresses_minting;

/// The table provider a kind falls back to when no `api_host` matches: the
/// vendor that kind always speaks to.
const FIRST_PARTY: &[(&str, &str)] = &[
    (kind::ANTHROPIC_API, "anthropic"),
    (kind::OPENAI_RESPONSES, "openai"),
    (kind::GEMINI, "google"),
    (kind::BEDROCK, "amazon-bedrock"),
];

/// The outcome of the origin match.
pub(super) enum HostMatch<'a> {
    One(&'a ProviderPrices),
    Ambiguous,
    None,
}

impl PriceTable {
    /// The table provider for a target of `kind` egressing to `base_url`,
    /// looked up for `upstream`, or `None` when selection is unknown.
    pub(super) fn select_provider(
        &self,
        kind: &str,
        base_url: Option<&str>,
        upstream: &str,
    ) -> Option<&ProviderPrices> {
        if let Some(target) = base_url.and_then(remote_base) {
            match self.match_host(&target, upstream) {
                HostMatch::One(provider) => return Some(provider),
                HostMatch::Ambiguous => return None,
                HostMatch::None => {}
            }
        }
        FIRST_PARTY
            .iter()
            .find(|(k, _)| *k == kind)
            .and_then(|(_, id)| self.provider(id))
    }

    /// The same-origin provider that best fits `target`, ranked by the
    /// longest common path prefix, then the count of path segments both
    /// share in any position, then whether it lists `upstream`. Ambiguous
    /// only when the best candidates tie on all three.
    pub(super) fn match_host(&self, target: &ApiBase, upstream: &str) -> HostMatch<'_> {
        let mut best: Option<(PathFit, &ProviderPrices)> = None;
        let mut tied = false;
        for provider in &self.providers {
            let Some(api) = provider.api.as_ref().filter(|a| a.origin == target.origin) else {
                continue;
            };
            let fit = PathFit {
                prefix: common_prefix(&api.segments, &target.segments),
                shared: shared_segments(&api.segments, &target.segments),
                lists_upstream: provider.models.contains_key(upstream),
            };
            match best {
                Some((top, _)) if fit < top => {}
                Some((top, _)) if fit == top => tied = true,
                _ => {
                    best = Some((fit, provider));
                    tied = false;
                }
            }
        }
        match best {
            Some(_) if tied => HostMatch::Ambiguous,
            Some((_, provider)) => HostMatch::One(provider),
            None => HostMatch::None,
        }
    }
}

/// How closely one same-origin provider's API path fits a target's, ordered
/// field by field.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PathFit {
    prefix: usize,
    shared: usize,
    lists_upstream: bool,
}

/// The comparable form of a target base URL; `None` for a loopback or local
/// target, which never matches a table host.
fn remote_base(base_url: &str) -> Option<ApiBase> {
    // Shared with field-verdict minting on purpose: one notion of "local".
    if loopback_target_suppresses_minting(base_url) {
        return None;
    }
    ApiBase::parse(base_url)
}

fn common_prefix(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// How many distinct segments of `target` also appear anywhere in `api`.
fn shared_segments(api: &[String], target: &[String]) -> usize {
    let api: BTreeSet<&str> = api.iter().map(String::as_str).collect();
    let target: BTreeSet<&str> = target.iter().map(String::as_str).collect();
    api.intersection(&target).count()
}
