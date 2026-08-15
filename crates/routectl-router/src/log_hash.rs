//! Per-process salted hash for correlating a caller-controlled value
//! across log lines without ever logging the value itself.
//!
//! Distinct from `context_trim::fnv1a_hash`, which must stay stable ACROSS
//! processes because it fingerprints a trimmed prefix that is compared
//! turn-to-turn. A log-correlation hash needs the opposite property.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::OnceLock;

/// Process-lifetime hash seed. `RandomState` seeds each instance randomly,
/// so holding exactly one for the whole process makes every derived hash
/// comparable within a run and unpredictable across runs.
static LOG_HASH_SEED: OnceLock<RandomState> = OnceLock::new();

/// Hash a caller-controlled value for LOG CORRELATION only.
///
/// Stable for the life of the process, so an operator can group the log
/// lines carrying the same value; unpredictable across processes, so a
/// hash that reaches a log sink cannot be matched against a precomputed
/// dictionary of guessable inputs. A client is free to set its session
/// identity to a stable per-user string such as an email address, and an
/// unsalted 64-bit hash of one is recoverable offline in seconds -- which
/// would put the very identifier the never-log-raw contract exists to
/// exclude back into a default-level log line.
///
/// Never usable as a fingerprint: nothing may persist one of these values
/// or compare it across restarts.
///
/// Scope of the guarantee: this defeats OFFLINE inversion, which is the
/// whole point. It does not stop someone who can both reach the ingress and
/// read the logs from hashing a guessed value under the live seed and
/// matching it within the same run. That is out of scope while a deployment
/// serves a single tenant, where confirming your own identifier discloses
/// nothing, and log-read access already implies the stronger header-tracing
/// capability that prints the value outright. A future multi-seat
/// deployment should re-examine it deliberately rather than inherit this
/// reasoning silently.
pub fn salted_log_hash(value: &str) -> u64 {
    LOG_HASH_SEED.get_or_init(RandomState::new).hash_one(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Correlation is the whole point of the hash: the same value logged
    /// from two different lines in one run must render identically.
    #[test]
    fn same_value_hashes_identically_within_one_process() {
        let first = salted_log_hash("sess-correlation-probe");
        let second = salted_log_hash("sess-correlation-probe");
        assert_eq!(first, second);
    }

    #[test]
    fn different_values_hash_differently() {
        assert_ne!(salted_log_hash("sess-a"), salted_log_hash("sess-b"));
    }

    /// The salt must not be the identity function on the input: an unsalted
    /// hash of a guessable value is what this module exists to avoid, so
    /// pin that the output is not the plain FNV-1a fingerprint hash.
    #[test]
    fn salted_hash_differs_from_the_unsalted_fingerprint_hash() {
        let value = "sess-dictionary-candidate";
        assert_ne!(
            salted_log_hash(value),
            crate::context_trim::fnv1a_hash(value.as_bytes()),
        );
    }
}
