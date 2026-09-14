//! `CappedWarnSet`: the bounded warn-once dedup primitive both drift /
//! anomaly warnings in this crate are built on.
//!
//! The shape it factors out. A warning keyed on request-derived data must
//! fire once per distinct key rather than once per request, or it trains an
//! operator to ignore the level. But the key space is client-controlled, so
//! remembering every key seen would grow without bound for the life of the
//! process under a runaway or hostile client. So: a set with a cap, one
//! notice when the cap is first hit, and never-warn past it -- never
//! warn-every-request past it, which would turn the degradation into a
//! second unbounded log-volume problem.
//!
//! What this owns and what it does not. It owns the decision: is this key
//! new, already recorded, or arriving past the cap. It owns the atomicity
//! of that check-then-act, the bound, the one-shot cap notice, and poisoned
//! lock recovery. It emits NOTHING -- every log target, field set, and
//! message stays at the call site, because those are what make two
//! different warnings distinguishable to whoever reads the logs. Callers
//! decide under this type and log after it returns, so no lock is held
//! across a `tracing` call.

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// What one key's arrival means for the caller's warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarnDecision {
    /// First sighting, recorded: the caller should emit its warning.
    Emit,
    /// Already recorded: stay silent.
    AlreadyWarned,
    /// The cap is full and this key is not in the set. The caller should
    /// stay silent about the KEY, and emit its one-time cap notice iff
    /// `notice` is true.
    CapReached { notice: bool },
}

/// A bounded set of keys already warned about, plus the one-shot flag for
/// the cap notice. `cap` is explicit per instance: the two current callers
/// bound different key spaces for different reasons, and one shared
/// constant would tie a change in either to the other.
#[derive(Debug)]
pub struct CappedWarnSet<K: Eq + Hash> {
    seen: Mutex<HashSet<K>>,
    cap: usize,
    cap_noted: AtomicBool,
}

impl<K: Eq + Hash> CappedWarnSet<K> {
    pub fn new(cap: usize) -> Self {
        Self {
            seen: Mutex::new(HashSet::new()),
            cap,
            cap_noted: AtomicBool::new(false),
        }
    }

    /// Record `key` if it is new and the set has room, and say what the
    /// caller should do. The decision is taken while the lock is held, so
    /// concurrent arrivals of one key produce exactly one [`Emit`].
    ///
    /// Never panics: a poisoned lock (only reachable if a prior holder
    /// panicked while holding it, which nothing here does) is recovered
    /// rather than propagated onto a request path.
    ///
    /// [`Emit`]: WarnDecision::Emit
    pub fn admit(&self, key: K) -> WarnDecision {
        let decision = match self.seen.lock() {
            Ok(mut seen) => Self::decide(&mut seen, key, self.cap),
            Err(poisoned) => Self::decide(&mut poisoned.into_inner(), key, self.cap),
        };
        match decision {
            RawDecision::Emit => WarnDecision::Emit,
            RawDecision::AlreadyWarned => WarnDecision::AlreadyWarned,
            RawDecision::CapReached => WarnDecision::CapReached {
                notice: !self.cap_noted.swap(true, Ordering::Relaxed),
            },
        }
    }

    fn decide(seen: &mut HashSet<K>, key: K, cap: usize) -> RawDecision {
        if seen.contains(&key) {
            return RawDecision::AlreadyWarned;
        }
        if seen.len() >= cap {
            return RawDecision::CapReached;
        }
        seen.insert(key);
        RawDecision::Emit
    }

    /// The configured bound, so a caller can name it in its cap notice
    /// without holding a second copy of the number.
    pub const fn cap(&self) -> usize {
        self.cap
    }

    /// Keys currently recorded.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        match self.seen.lock() {
            Ok(seen) => seen.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Whether the one-shot cap notice has already been handed out.
    #[cfg(test)]
    pub fn cap_noted(&self) -> bool {
        self.cap_noted.load(Ordering::Relaxed)
    }

    /// Poison the lock so callers' recovery paths are reachable in tests.
    #[cfg(test)]
    pub fn poison_for_test(&self) {
        let _unused = self.seen.lock();
        panic!("poisoning the dedup lock on purpose");
    }
}

/// The decision before the cap-notice flag is consulted. Split out so the
/// one-shot swap happens OUTSIDE the lock.
enum RawDecision {
    Emit,
    AlreadyWarned,
    CapReached,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_key_emits_and_a_repeat_stays_silent() {
        let set: CappedWarnSet<String> = CappedWarnSet::new(8);

        assert_eq!(set.admit("a".to_string()), WarnDecision::Emit);

        assert_eq!(set.admit("a".to_string()), WarnDecision::AlreadyWarned);
    }

    #[test]
    fn distinct_keys_each_emit_once_under_alternation() {
        let set: CappedWarnSet<String> = CappedWarnSet::new(8);

        assert_eq!(set.admit("a".to_string()), WarnDecision::Emit);
        assert_eq!(set.admit("b".to_string()), WarnDecision::Emit);

        for key in ["a", "b", "a", "b"] {
            assert_eq!(
                set.admit(key.to_string()),
                WarnDecision::AlreadyWarned,
                "alternating keys must each be remembered, not just the last"
            );
        }
    }

    #[test]
    fn a_tuple_key_is_admitted_by_its_whole_value() {
        let set: CappedWarnSet<(String, String)> = CappedWarnSet::new(8);

        assert_eq!(
            set.admit(("GET".to_string(), "/a".to_string())),
            WarnDecision::Emit
        );

        assert_eq!(
            set.admit(("GET".to_string(), "/b".to_string())),
            WarnDecision::Emit,
            "one component differing makes a different key"
        );
        assert_eq!(
            set.admit(("GET".to_string(), "/a".to_string())),
            WarnDecision::AlreadyWarned
        );
    }

    #[test]
    fn the_first_key_past_the_cap_carries_the_notice_and_later_ones_do_not() {
        let set: CappedWarnSet<String> = CappedWarnSet::new(2);
        assert_eq!(set.admit("a".to_string()), WarnDecision::Emit);
        assert_eq!(set.admit("b".to_string()), WarnDecision::Emit);

        assert_eq!(
            set.admit("c".to_string()),
            WarnDecision::CapReached { notice: true }
        );

        assert_eq!(
            set.admit("d".to_string()),
            WarnDecision::CapReached { notice: false },
            "the cap notice is one-shot"
        );
        assert_eq!(
            set.admit("c".to_string()),
            WarnDecision::CapReached { notice: false },
            "a key refused at the cap was never recorded, so it stays refused"
        );
    }

    #[test]
    fn a_key_recorded_before_the_cap_still_dedups_after_it() {
        let set: CappedWarnSet<String> = CappedWarnSet::new(2);
        set.admit("a".to_string());
        set.admit("b".to_string());
        set.admit("overflow".to_string());

        assert_eq!(
            set.admit("a".to_string()),
            WarnDecision::AlreadyWarned,
            "reaching the cap must not turn a known key into a re-warn"
        );
    }

    #[test]
    fn the_set_stops_growing_at_the_cap() {
        let set: CappedWarnSet<String> = CappedWarnSet::new(4);

        for n in 0..64 {
            set.admit(format!("k{n}"));
        }

        assert_eq!(set.len(), 4);
    }

    #[test]
    fn a_poisoned_lock_is_recovered_rather_than_propagated() {
        let set: std::sync::Arc<CappedWarnSet<String>> = std::sync::Arc::new(CappedWarnSet::new(8));
        let poisoner = std::sync::Arc::clone(&set);
        let _ = std::thread::spawn(move || poisoner.poison_for_test()).join();

        assert_eq!(set.admit("a".to_string()), WarnDecision::Emit);
        assert_eq!(set.admit("a".to_string()), WarnDecision::AlreadyWarned);
    }

    #[test]
    fn concurrent_arrivals_of_one_key_emit_exactly_once() {
        let set: std::sync::Arc<CappedWarnSet<String>> =
            std::sync::Arc::new(CappedWarnSet::new(64));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let set = std::sync::Arc::clone(&set);
            handles.push(std::thread::spawn(move || {
                usize::from(set.admit("a".to_string()) == WarnDecision::Emit)
            }));
        }

        let emitted: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();

        assert_eq!(emitted, 1, "the check-then-act must be atomic");
    }

    #[test]
    fn concurrent_arrivals_past_the_cap_produce_exactly_one_notice() {
        let set: std::sync::Arc<CappedWarnSet<String>> = std::sync::Arc::new(CappedWarnSet::new(1));
        set.admit("first".to_string());
        let mut handles = Vec::new();
        for n in 0..16 {
            let set = std::sync::Arc::clone(&set);
            handles.push(std::thread::spawn(move || {
                usize::from(set.admit(format!("k{n}")) == WarnDecision::CapReached { notice: true })
            }));
        }

        let notices: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();

        assert_eq!(notices, 1);
    }

    #[test]
    fn the_cap_is_readable_for_a_callers_notice_field() {
        let set: CappedWarnSet<String> = CappedWarnSet::new(17);

        assert_eq!(set.cap(), 17);
    }

    /// A zero cap admits nothing. Pinned because it is the degenerate edge
    /// of an explicit-cap design: it must refuse cleanly, never divide or
    /// underflow.
    #[test]
    fn a_zero_cap_refuses_the_very_first_key() {
        let set: CappedWarnSet<String> = CappedWarnSet::new(0);

        assert_eq!(
            set.admit("a".to_string()),
            WarnDecision::CapReached { notice: true }
        );
        assert_eq!(set.len(), 0);
    }
}
