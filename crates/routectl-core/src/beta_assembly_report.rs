//! Per-attempt slot through which an anthropic-vocabulary egress records the
//! two beta sets it computed while assembling the attempt's beta carrier (the
//! `anthropic-beta` header, the Invoke body or the Converse bag).
//!
//! - The floor: the flags the egress itself asserts and never withholds
//!   (operator pins, and on the cloak lane the OAuth gate and minted floor).
//! - The body-implied set: the flags the shipped body's own fields require,
//!   which the egress re-adds after any withhold.
//!
//! A flag in either set would ship again on a retry that withheld it, so a
//! reader must not treat it as a client flag it can remove. The router reads
//! the slot after the call; an attempt whose egress recorded nothing has no
//! assembly, and no flag of it is removable.

use std::sync::{Arc, Mutex, PoisonError};

/// The floor and body-implied beta sets of one assembled attempt, each
/// deduplicated in the order the egress composed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BetaAssembly {
    floor: Vec<String>,
    body_implied: Vec<String>,
}

impl BetaAssembly {
    /// The assembly of `floor` and `body_implied`, each keeping only the
    /// first occurrence of a flag.
    pub fn new(floor: Vec<String>, body_implied: Vec<String>) -> Self {
        Self {
            floor: dedup_in_order(floor),
            body_implied: dedup_in_order(body_implied),
        }
    }

    /// The flags the egress asserts and never withholds.
    pub fn floor(&self) -> &[String] {
        &self.floor
    }

    /// The flags the shipped body's own fields require.
    pub fn body_implied(&self) -> &[String] {
        &self.body_implied
    }
}

fn dedup_in_order(flags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(flags.len());
    for flag in flags {
        if !out.contains(&flag) {
            out.push(flag);
        }
    }
    out
}

/// Shared, clone-cheap slot holding at most one [`BetaAssembly`]. Every clone
/// shares one slot, so the copy the router keeps observes what the egress
/// records on the copy that rode the request.
#[derive(Debug, Clone, Default)]
pub struct BetaAssemblyReport {
    assembly: Arc<Mutex<Option<BetaAssembly>>>,
}

impl BetaAssemblyReport {
    /// Store `assembly`, replacing any earlier one: an egress that assembles
    /// the same attempt twice ships only the last body, so only its sets
    /// describe the wire.
    pub fn record(&self, assembly: BetaAssembly) {
        let mut slot = self.assembly.lock().unwrap_or_else(PoisonError::into_inner);
        *slot = Some(assembly);
    }

    /// Return the recorded assembly, if any, and empty the slot.
    pub fn take(&self) -> Option<BetaAssembly> {
        let mut slot = self.assembly.lock().unwrap_or_else(PoisonError::into_inner);
        slot.take()
    }
}

#[cfg(test)]
mod tests {
    use super::{BetaAssembly, BetaAssemblyReport};

    fn owned(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|f| (*f).to_string()).collect()
    }

    #[test]
    fn new_dedupes_each_set_keeping_first_seen_order() {
        let assembly = BetaAssembly::new(owned(&["b", "a", "b"]), owned(&["c", "c", "a"]));

        assert_eq!(assembly.floor(), owned(&["b", "a"]));
        assert_eq!(assembly.body_implied(), owned(&["c", "a"]));
    }

    #[test]
    fn record_replaces_an_earlier_assembly() {
        let report = BetaAssemblyReport::default();

        report.record(BetaAssembly::new(owned(&["first"]), owned(&["first"])));
        report.record(BetaAssembly::new(owned(&["second"]), Vec::new()));

        assert_eq!(
            report.take(),
            Some(BetaAssembly::new(owned(&["second"]), Vec::new()))
        );
    }

    #[test]
    fn take_empties_the_slot() {
        let report = BetaAssemblyReport::default();
        report.record(BetaAssembly::new(owned(&["a"]), Vec::new()));

        assert!(report.take().is_some());
        assert!(report.take().is_none());
    }

    #[test]
    fn clones_share_one_slot() {
        let router_copy = BetaAssemblyReport::default();
        let provider_copy = router_copy.clone();

        provider_copy.record(BetaAssembly::new(owned(&["a"]), owned(&["b"])));

        assert_eq!(
            router_copy.take(),
            Some(BetaAssembly::new(owned(&["a"]), owned(&["b"])))
        );
        assert!(provider_copy.take().is_none());
    }

    #[test]
    fn take_recovers_from_a_poisoned_lock() {
        let report = BetaAssemblyReport::default();
        report.record(BetaAssembly::new(owned(&["a"]), Vec::new()));
        let poisoner = report.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.assembly.lock().unwrap();
            panic!("poison the slot");
        })
        .join();

        assert_eq!(
            report.take(),
            Some(BetaAssembly::new(owned(&["a"]), Vec::new()))
        );
    }
}
