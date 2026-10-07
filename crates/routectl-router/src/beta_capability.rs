//! The beta-flag capability namespace: one prefix, one bounded constructor,
//! one accessor.
//!
//! An upstream that rejects a request by naming an `anthropic_beta` flag
//! states a fact about that flag on that lane. Such a fact is recorded in the
//! existing learned-capability key space as `beta:<flag>`, so the registry,
//! the events ledger, the warm rebuild and the doctor surfaces all carry it
//! unchanged -- there is no second store and no schema change.
//!
//! The prefix is permanent: a minted key is written verbatim to an
//! append-only ledger and read back on every later boot. A second copy of the
//! prefix literal anywhere else could drift from this one and re-partition
//! history, so the prefix is private to this module: the only way to obtain a
//! key is [`beta_capability_key`], and the only ways to read one are
//! [`beta_flag_of`] and [`capability_key_is_beta`].
//!
//! Beta keys are catalog-scoped facts: they sit outside the envelope-field
//! namespace, so `field_capability::capability_key_is_catalog_scoped` keeps
//! its default and the invalidation paths discard them on a catalog revision
//! change like any other catalog capability.

use routectl_core::is_safe_token;

/// The permanent prefix of every beta-flag capability key.
///
/// Private on purpose: a sibling module holding the prefix could assemble a
/// key that never passed [`beta_capability_key`]'s grammar, and every such key
/// would be permanent.
const BETA_CAPABILITY_PREFIX: &str = "beta:";

/// Bytes a flag may never contain, on top of the shared token shape.
///
/// `.`: the Bedrock branch of the shared capability-key normalizer keeps only
/// the first dot-separated segment, so a dotted flag would be truncated into a
/// different permanent key on that lane. `:`: the prefix's own separator, which
/// also rejects a key passed back in as a flag.
const FORBIDDEN_FLAG_BYTES: [char; 2] = ['.', ':'];

/// Build the capability key for the beta flag an upstream rejection named, or
/// `None` when `flag` is not a well-formed flag.
///
/// Accepted: a non-empty, bounded token of printable ASCII (the shared
/// `is_safe_token` shape) containing neither `.` nor `:`. An accepted flag is
/// appended to the prefix unchanged, so the key preserves the upstream's
/// spelling byte for byte.
pub fn beta_capability_key(flag: &str) -> Option<String> {
    is_beta_flag(flag).then(|| format!("{BETA_CAPABILITY_PREFIX}{flag}"))
}

/// True when `flag` is a well-formed beta flag. See [`beta_capability_key`].
fn is_beta_flag(flag: &str) -> bool {
    is_safe_token(flag) && !flag.contains(FORBIDDEN_FLAG_BYTES)
}

/// The flag inside a beta capability key, or `None` for a key outside the
/// namespace. The exact inverse of [`beta_capability_key`]'s append, so a
/// round trip is lossless.
///
/// A namespace read, not a validity check: a key persisted by an earlier
/// build is returned as stored even if today's grammar would refuse its flag.
pub fn beta_flag_of(key: &str) -> Option<&str> {
    key.strip_prefix(BETA_CAPABILITY_PREFIX)
}

/// True when `key` sits inside the beta-flag namespace.
pub fn capability_key_is_beta(key: &str) -> bool {
    beta_flag_of(key).is_some()
}

/// The beta flags a shipped seed withholds on one provider kind.
///
/// Only a cell this scope covers can carry a seed-clear marker: a marker on any
/// other cell changes no withholding decision, so storing it would only grow
/// the marker set with flags no seed names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BetaSeedScope {
    provider_kind: &'static str,
    flags: &'static [&'static str],
}

impl BetaSeedScope {
    /// A scope covering no cell.
    pub const EMPTY: Self = Self::new("", &[]);

    /// The scope of `flags` on `provider_kind` lanes.
    pub const fn new(provider_kind: &'static str, flags: &'static [&'static str]) -> Self {
        Self {
            provider_kind,
            flags,
        }
    }

    /// The provider kind the seed applies to.
    pub const fn provider_kind(&self) -> &'static str {
        self.provider_kind
    }

    /// The capability key of every well-formed seeded flag, in seed order.
    pub fn seeded_keys(&self) -> Vec<String> {
        self.flags
            .iter()
            .filter_map(|flag| beta_capability_key(flag))
            .collect()
    }

    /// True when `feature_key` is the beta key of a seeded flag and
    /// `provider_kind` is the kind the seed applies to.
    pub fn covers(&self, provider_kind: &str, feature_key: &str) -> bool {
        provider_kind == self.provider_kind
            && beta_flag_of(feature_key).is_some_and(|flag| self.flags.contains(&flag))
    }
}

#[cfg(test)]
#[path = "beta_capability_tests.rs"]
mod tests;
