//! Shared safety bound for alias and credential-seat expansion.

use std::collections::BTreeMap;

use routectl_core::sanitize_for_log;

use crate::config::Config;
use crate::router::ALIAS_MAX_RECURSION_DEPTH;

/// Generous internal safety valve, not a tuning knob: ordinary fallback chains
/// have a handful of targets and pools have at most 32 seats. Allow 4096 final
/// dispatch hops (including repeated targets, which may be deliberate retries),
/// while preventing a small branching alias DAG from allocating millions of
/// request-time targets. Config counts all configured seats, not just the ones
/// whose credentials happen to be usable during this build.
pub const MAX_EXPANDED_TARGETS: usize = 4096;

/// Count a cycle-free alias DAG once per node, retaining multiplicity at each
/// incoming edge. Saturate at limit + 1: the exact oversize count is irrelevant
/// and must not overflow. Height travels with the memo so a shared suffix first
/// seen on a short path cannot hide a later over-depth path.
pub fn validate_expanded_sizes(config: &Config, errors: &mut Vec<String>) {
    let mut memo = BTreeMap::new();
    for alias in config.aliases.keys() {
        let (size, height) = expanded_size(config, alias, 0, &mut memo);
        let alias = sanitize_for_log(alias);
        if size > MAX_EXPANDED_TARGETS {
            errors.push(format!(
                "alias `{alias}`: expanded chain exceeds {MAX_EXPANDED_TARGETS} dispatch \
                 targets (including repeated targets and pool seats); shorten the fallback chain"
            ));
        }
        if height > ALIAS_MAX_RECURSION_DEPTH {
            errors.push(format!(
                "alias `{alias}`: chain recursion exceeds depth {ALIAS_MAX_RECURSION_DEPTH}; \
                 shorten the nested alias chain"
            ));
        }
    }
}

fn expanded_size(
    config: &Config,
    alias: &str,
    depth: usize,
    memo: &mut BTreeMap<String, (usize, usize)>,
) -> (usize, usize) {
    if let Some(&summary) = memo.get(alias) {
        return summary;
    }
    // Cycle validation runs first. Also bound this recursion defensively, even
    // if called on an invalid graph; do not memoize a truncated suffix.
    if depth > ALIAS_MAX_RECURSION_DEPTH {
        return (0, ALIAS_MAX_RECURSION_DEPTH + 1);
    }
    let mut size = 0usize;
    let mut height = 0;
    for entry in config.aliases[alias].nicknames() {
        let (child_size, child_height) = if config.aliases.contains_key(entry) {
            let (size, height) = expanded_size(config, entry, depth + 1, memo);
            (size, height + 1)
        } else {
            let seats = config
                .models
                .get(entry)
                .and_then(|m| config.pools.get(&m.provider));
            (seats.map_or(1, |pool| pool.members.len()), 0)
        };
        size = size
            .saturating_add(child_size)
            .min(MAX_EXPANDED_TARGETS + 1);
        height = height.max(child_height);
    }
    let summary = (size, height);
    memo.insert(alias.to_string(), summary);
    summary
}

pub fn expansion_limit_error() -> routectl_core::Error {
    routectl_core::Error::Config(format!(
        "alias chain expansion exceeds {MAX_EXPANDED_TARGETS} targets \
         (including repeated targets and pool seats); shorten the fallback chain \
         and run `routectl config check`"
    ))
}

#[cfg(test)]
#[path = "alias_limits_tests.rs"]
mod tests;
