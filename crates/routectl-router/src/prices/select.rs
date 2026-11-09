//! Provider selection: which table provider prices a config target.

use routectl_core::provider_kind::config as kind;

use super::format::ApiBase;
use super::{PriceTable, ProviderPrices};
use crate::config::{Config, ProviderEntry};
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
    /// The table provider for a target of `kind` egressing to `base_url`, or
    /// `None` when selection is unknown.
    pub(super) fn select_provider(
        &self,
        kind: &str,
        base_url: Option<&str>,
    ) -> Option<&ProviderPrices> {
        if let Some(target) = base_url.and_then(remote_base) {
            match self.match_host(&target) {
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

    pub(super) fn match_host(&self, target: &ApiBase) -> HostMatch<'_> {
        let mut best: Option<(usize, &ProviderPrices)> = None;
        let mut tied = false;
        for provider in &self.providers {
            let Some(api) = provider.api.as_ref().filter(|a| a.origin == target.origin) else {
                continue;
            };
            let shared = common_prefix(&api.segments, &target.segments);
            match best {
                Some((len, _)) if shared < len => {}
                Some((len, _)) if shared == len => tied = true,
                _ => {
                    best = Some((shared, provider));
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

/// The provider entry a `[providers]` or `[pools]` key resolves to: the entry
/// itself, or a pool's first member naming an entry (the member
/// `Config::provider_kind_for_target` reads).
pub(super) fn provider_entry_for_target<'a>(
    config: &'a Config,
    name: &str,
) -> Option<&'a ProviderEntry> {
    config.providers.get(name).or_else(|| {
        config
            .pools
            .get(name)?
            .members
            .iter()
            .find_map(|member| config.providers.get(member))
    })
}
