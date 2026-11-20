//! The attention list's memory across reads: what was on the list before, so a
//! condition that clears shows as recovered for a while instead of vanishing,
//! a condition with no native start keeps the time it was first seen, and a
//! request-row loss counter's rise is told apart from a total that is merely
//! nonzero.
//!
//! The memory is reconciled on every read and nowhere else, so recovery time is
//! the first read that saw the condition gone. It lives in process memory only
//! and is bounded by entry count and by age. Time arrives as the read's
//! `now_ms`; the module reads no clock.

use std::collections::{BTreeMap, BTreeSet};

use parking_lot::Mutex;

use crate::commands::doctor::attention::{
    Attention, AttentionInputs, AttentionItem, AttentionKind, RecordLoss,
    active_items_with_subsumed,
};

/// How long a recovered item stays on the list after its first observed
/// recovery, and how long a doctor item is held unobserved while the doctor
/// report is unavailable. Both boundaries are inclusive.
pub const FORGET_AFTER_MS: i64 = 60 * 60_000;

/// The most entries the memory stores. Items active in the current read are
/// always returned, stored or not.
pub const MAX_ENTRIES: usize = 256;

/// Whether this read's doctor findings describe the doctor's current verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorCoverage {
    /// The findings are this read's report.
    Current,
    /// No report could be built; the findings are empty, not passing.
    Unavailable,
}

/// The per-daemon memory behind the attention list.
#[derive(Debug, Default)]
pub struct AttentionMemory {
    store: Mutex<Store>,
}

impl AttentionMemory {
    /// An empty memory, as at daemon start.
    pub fn new() -> Self {
        Self::default()
    }

    /// The list for one read: the conditions active in `inputs`, plus the
    /// recovered ones still within their hold, folded into the payload.
    ///
    /// The whole reconcile runs under one lock, so concurrent reads agree on
    /// each item's first-seen time and on each counter rise.
    pub fn build(&self, inputs: AttentionInputs, coverage: DoctorCoverage) -> Attention {
        let items = self.store.lock().reconcile(inputs, coverage);
        Attention::from_items(items)
    }

    #[cfg(test)]
    fn stored_len(&self) -> usize {
        self.store.lock().entries.len()
    }
}

#[derive(Debug, Default)]
struct Store {
    entries: BTreeMap<String, Entry>,
    loss: LossTrack,
}

#[derive(Debug, Clone)]
struct Entry {
    /// The item as last observed active, `since_ms` already filled.
    item: AttentionItem,
    first_seen_ms: i64,
    last_seen_ms: i64,
    recovered_at_ms: Option<i64>,
}

/// The highest loss totals seen so far and when one of them last rose. The
/// counters start at zero with the process, as the memory does.
#[derive(Debug, Default, Clone, Copy)]
struct LossTrack {
    dropped_full: u64,
    dropped_closed: u64,
    write_errors: u64,
    last_rise_ms: Option<i64>,
}

impl LossTrack {
    /// Records this read's totals and returns them with the last rise. A total
    /// below the highest seen comes from a read that sampled the counters
    /// earlier; it is neither a rise nor a new baseline.
    fn observe(&mut self, loss: RecordLoss, now_ms: i64) -> RecordLoss {
        let rose = loss.dropped_full > self.dropped_full
            || loss.dropped_closed > self.dropped_closed
            || loss.write_errors > self.write_errors;
        self.dropped_full = self.dropped_full.max(loss.dropped_full);
        self.dropped_closed = self.dropped_closed.max(loss.dropped_closed);
        self.write_errors = self.write_errors.max(loss.write_errors);
        if rose {
            self.last_rise_ms = Some(now_ms);
        }
        RecordLoss {
            last_rise_ms: self.last_rise_ms,
            ..loss
        }
    }
}

impl Store {
    fn reconcile(
        &mut self,
        mut inputs: AttentionInputs,
        coverage: DoctorCoverage,
    ) -> Vec<AttentionItem> {
        let now = inputs.now_ms;
        inputs.record_loss = self.loss.observe(inputs.record_loss, now);
        let built = active_items_with_subsumed(&inputs);
        let active: Vec<AttentionItem> = built
            .items
            .into_iter()
            .map(|item| self.remember_active(item, now))
            .collect();
        let current: BTreeSet<String> = active.iter().map(|item| item.key.clone()).collect();
        self.forget_subsumed(&built.subsumed);
        self.settle_absent(&current, coverage, now);
        self.evict_past_bound(&current);
        let mut items = active;
        items.extend(
            self.entries
                .iter()
                .filter(|(key, _)| !current.contains(*key))
                .map(|(_, entry)| entry.shown()),
        );
        items
    }

    /// Stores an active item, keeping its first-seen time across reads while
    /// every other field follows this read, and returns it with `since_ms`.
    fn remember_active(&mut self, mut item: AttentionItem, now: i64) -> AttentionItem {
        let first_seen_ms = self
            .entries
            .get(&item.key)
            .filter(|entry| entry.recovered_at_ms.is_none())
            .map_or(now, |entry| entry.first_seen_ms);
        item.since_ms = if has_native_since(item.kind) {
            item.since_ms.or(Some(first_seen_ms))
        } else {
            Some(first_seen_ms)
        };
        self.entries.insert(
            item.key.clone(),
            Entry {
                item: item.clone(),
                first_seen_ms,
                last_seen_ms: now,
                recovered_at_ms: None,
            },
        );
        item
    }

    /// Forgets the items this read left out because a worse item names them.
    /// Their condition still holds, so they are never marked recovered; the
    /// worse item carries them until it clears.
    fn forget_subsumed(&mut self, subsumed: &BTreeSet<String>) {
        self.entries.retain(|key, _| !subsumed.contains(key));
    }

    /// Marks stored items missing from this read as recovered, holds doctor
    /// items while the doctor report is unavailable, and forgets what has
    /// outlived its hold.
    fn settle_absent(&mut self, current: &BTreeSet<String>, coverage: DoctorCoverage, now: i64) {
        self.entries.retain(|key, entry| {
            if current.contains(key) {
                return true;
            }
            let held = coverage == DoctorCoverage::Unavailable
                && entry.item.kind == AttentionKind::DoctorFailure
                && entry.recovered_at_ms.is_none();
            if held {
                return now - entry.last_seen_ms <= FORGET_AFTER_MS;
            }
            let recovered_at = *entry.recovered_at_ms.get_or_insert(now);
            now - recovered_at <= FORGET_AFTER_MS
        });
    }

    /// Drops the stalest entries past `MAX_ENTRIES`: entries not active in
    /// this read go first, oldest last change first.
    fn evict_past_bound(&mut self, current: &BTreeSet<String>) {
        let excess = self.entries.len().saturating_sub(MAX_ENTRIES);
        if excess == 0 {
            return;
        }
        let mut order: Vec<(bool, i64, String)> = self
            .entries
            .iter()
            .map(|(key, entry)| {
                let changed = entry.recovered_at_ms.unwrap_or(entry.last_seen_ms);
                (current.contains(key), changed, key.clone())
            })
            .collect();
        order.sort_unstable();
        for (_, _, key) in order.into_iter().take(excess) {
            self.entries.remove(&key);
        }
    }
}

impl Entry {
    fn shown(&self) -> AttentionItem {
        match self.recovered_at_ms {
            Some(at_ms) => self.item.clone().into_recovered(at_ms),
            None => self.item.clone(),
        }
    }
}

/// Whether the builder's `since_ms` is the condition's own start. A loss
/// item's builder since is the latest rise, so a run of rises would otherwise
/// look newer on every rise.
const fn has_native_since(kind: AttentionKind) -> bool {
    !matches!(kind, AttentionKind::RecordsDropped)
}

#[cfg(test)]
#[path = "attention_memory_tests.rs"]
mod tests;
