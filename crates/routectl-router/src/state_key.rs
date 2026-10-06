//! The lane identity every learned acceptance fact keys on.
//!
//! A [`StateKey`] names the `[providers]` entry that egresses a request and
//! the upstream model id it sends. The model nickname is deliberately absent:
//! two nicknames for one upstream on one provider entry share their learned
//! history, a nickname rename keeps it, and a different provider entry or
//! upstream starts fresh by construction. For a pooled seat the provider entry
//! is the seat's member entry, so the key is already per seat.
//!
//! It is distinct from the runtime breaker / RPM `state_key`
//! (`nickname[#member]`), which stays model-scoped.
//!
//! # Serialization
//!
//! One constructor (`StateKey::new`, crate-private: dispatch mints every
//! lane, and operator surfaces read lanes through the router's learned-lane
//! projection) and one public parser ([`StateKey::parse`], for operator
//! input). The serialized form is `provider_entry#upstream`, split at the
//! FIRST `#`.
//! That is injective because `#` is reserved in every provider name
//! (`seat_pool::check_state_key_name`), and the constructor refuses one that
//! carries it; an upstream may contain `#` and still round-trips. The usage
//! ledger stores the serialized form as an opaque `lane_key` string.
//!
//! There is no `Display`: an upstream id is operator configuration that may
//! carry arbitrary bytes, so the only rendering for a log is the sanitized
//! [`StateKey::for_log`].

use crate::seat_pool::SEAT_KEY_SEPARATOR;

/// The learned-store lane: (provider entry, upstream model id).
///
/// Stored as the serialized form plus the separator position, so the
/// persisted key and both halves are borrowed views of one allocation.
///
/// A raw render does not compile; log through [`StateKey::for_log`]:
///
/// ```compile_fail,E0277
/// let key = routectl_router::StateKey::parse("provider#upstream").unwrap();
/// let _ = format!("{key}");
/// ```
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct StateKey {
    serialized: String,
    split: usize,
}

impl StateKey {
    /// Mint the lane for `provider_entry` egressing `upstream`.
    ///
    /// `None` when `provider_entry` contains the reserved separator: config
    /// validation refuses such a name, so only an unvalidated construction
    /// path can reach this, and keying it would make the serialized form
    /// ambiguous.
    #[must_use]
    pub(crate) fn new(provider_entry: &str, upstream: &str) -> Option<Self> {
        if provider_entry.contains(SEAT_KEY_SEPARATOR) {
            return None;
        }
        let mut serialized = String::with_capacity(provider_entry.len() + 1 + upstream.len());
        serialized.push_str(provider_entry);
        serialized.push(SEAT_KEY_SEPARATOR);
        serialized.push_str(upstream);
        Some(Self {
            serialized,
            split: provider_entry.len(),
        })
    }

    /// Parse a serialized lane, splitting at the first separator. `None` when
    /// the string carries no separator at all.
    #[must_use]
    pub fn parse(serialized: &str) -> Option<Self> {
        let (provider_entry, _upstream) = serialized.split_once(SEAT_KEY_SEPARATOR)?;
        Some(Self {
            serialized: serialized.to_string(),
            split: provider_entry.len(),
        })
    }

    /// The `[providers]` entry half.
    #[must_use]
    pub fn provider_entry(&self) -> &str {
        &self.serialized[..self.split]
    }

    /// The upstream model id half, verbatim.
    #[must_use]
    pub fn upstream(&self) -> &str {
        &self.serialized[self.split + SEAT_KEY_SEPARATOR.len_utf8()..]
    }

    /// The serialized form the learned registry and the usage ledger key on.
    /// Persistence only: render through [`Self::for_log`] instead.
    #[must_use]
    pub fn as_lane_key(&self) -> &str {
        &self.serialized
    }

    /// The sanitized rendering for a log field or an operator surface.
    #[must_use]
    pub fn for_log(&self) -> String {
        routectl_core::sanitize_for_log(&self.serialized)
    }
}

#[cfg(test)]
impl StateKey {
    /// A lane for a fixture whose identity only needs to be distinct: the
    /// named provider entry over one fixed upstream.
    pub(crate) fn fixture(provider_entry: &str) -> Self {
        Self::new(provider_entry, "fixture-upstream")
            .expect("a fixture provider entry carries no separator")
    }
}

impl std::fmt::Debug for StateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StateKey").field(&self.for_log()).finish()
    }
}

#[cfg(test)]
#[path = "state_key_tests.rs"]
mod tests;
