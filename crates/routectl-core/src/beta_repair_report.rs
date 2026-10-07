//! Per-request slot through which a provider reports the client beta flags
//! it stripped and retried without.
//!
//! When an upstream rejects a request by naming a client-supplied
//! `anthropic-beta` flag, a provider may drop that flag and retry. It records
//! the dropped flags here; the router reads them after the call returns so it
//! can learn that this provider must not be sent those flags again.

use std::sync::{Arc, Mutex, PoisonError};

/// Shared, clone-cheap slot of stripped beta flags. Every clone shares one
/// slot, so the copy the router keeps observes what the provider records on
/// the copy that rode the request.
#[derive(Debug, Clone, Default)]
pub struct BetaRepairReport {
    flags: Arc<Mutex<Vec<String>>>,
}

impl BetaRepairReport {
    /// Append each flag not already recorded, preserving first-seen order.
    pub fn record(&self, flags: &[String]) {
        let mut slot = self.flags.lock().unwrap_or_else(PoisonError::into_inner);
        for flag in flags {
            if !slot.contains(flag) {
                slot.push(flag.clone());
            }
        }
    }

    /// Return the recorded flags in first-seen order and empty the slot.
    pub fn take(&self) -> Vec<String> {
        let mut slot = self.flags.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *slot)
    }
}

#[cfg(test)]
mod tests {
    use super::BetaRepairReport;

    fn owned(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|f| (*f).to_string()).collect()
    }

    #[test]
    fn record_dedupes_and_keeps_first_seen_order() {
        let report = BetaRepairReport::default();

        report.record(&owned(&["b-flag", "a-flag", "b-flag"]));
        report.record(&owned(&["a-flag", "c-flag"]));

        assert_eq!(report.take(), owned(&["b-flag", "a-flag", "c-flag"]));
    }

    #[test]
    fn take_empties_the_slot() {
        let report = BetaRepairReport::default();
        report.record(&owned(&["a-flag"]));

        let first = report.take();
        let second = report.take();

        assert_eq!(first, owned(&["a-flag"]));
        assert!(second.is_empty());
    }

    #[test]
    fn clones_share_one_slot() {
        let router_copy = BetaRepairReport::default();
        let provider_copy = router_copy.clone();

        provider_copy.record(&owned(&["a-flag"]));
        router_copy.record(&owned(&["a-flag", "b-flag"]));

        assert_eq!(router_copy.take(), owned(&["a-flag", "b-flag"]));
        assert!(provider_copy.take().is_empty());
    }

    #[test]
    fn take_recovers_from_a_poisoned_lock() {
        let report = BetaRepairReport::default();
        report.record(&owned(&["a-flag"]));
        let poisoner = report.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.flags.lock().unwrap();
            panic!("poison the slot");
        })
        .join();

        assert_eq!(report.take(), owned(&["a-flag"]));
    }
}
