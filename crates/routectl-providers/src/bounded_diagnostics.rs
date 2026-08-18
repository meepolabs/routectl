//! Collection-time bounding for the diagnostic samples the provider
//! egresses attach to aggregated WARN records.
//!
//! Dependency-free and provider-agnostic on purpose: it is declared
//! without a feature gate so a lean anthropic build, an all-features
//! Bedrock build, and the OpenAI-Responses egress all reach the same
//! primitive without any module depending on another provider's gate.
//! Nothing here carries provider vocabulary or wire-translation logic.

/// Cap on how many items of any one diagnostic sample reach a log
/// record. A single reply can carry arbitrarily many skipped reasoning
/// details, foreign format tags, or affected message indices, so an
/// uncapped sample turns one WARN into an unbounded log record. The cap
/// applies as the sample is COLLECTED, not at format time, so the
/// diagnostic path never allocates the full list either.
pub const MAX_LOGGED_DIAGNOSTIC_ITEMS: usize = 8;

/// A diagnostic sample that stops growing at `MAX_LOGGED_DIAGNOSTIC_ITEMS`
/// and remembers whether anything was dropped to stay within that bound.
///
/// Only the bounding is shared. The per-request tallies that own these
/// samples stay defined per translator, because they are not the same
/// type in disguise: their category sets differ (one egress splits
/// unsigned-signature drops from foreign-format drops, another has a
/// single category), and their WARN message strings are grep targets that
/// operators and the wire-behavior docs reference separately. Unifying
/// them would require passing the message text in as a constructor
/// argument and carrying fields most call sites never set.
pub struct BoundedLogSample<T> {
    items: Vec<T>,
    truncated: bool,
}

impl<T> Default for BoundedLogSample<T> {
    fn default() -> Self {
        Self {
            items: Vec::with_capacity(MAX_LOGGED_DIAGNOSTIC_ITEMS),
            truncated: false,
        }
    }
}

impl<T> BoundedLogSample<T> {
    /// An empty sample, pre-sized to the cap.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `value` if the sample has room; otherwise drop it and mark
    /// the sample truncated.
    pub fn push(&mut self, value: T) {
        if self.items.len() < MAX_LOGGED_DIAGNOSTIC_ITEMS {
            self.items.push(value);
        } else {
            self.truncated = true;
        }
    }

    /// The stored items, in first-seen order. Never longer than
    /// `MAX_LOGGED_DIAGNOSTIC_ITEMS`.
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// How many items are stored -- NOT how many were offered. Callers
    /// that need the exact magnitude keep their own count.
    ///
    /// No collection site reads this: an exact count is always its own
    /// counter, which is the whole point of the type. It exists for the
    /// unit tests that pin the bounding behavior, so the lib target sees
    /// it (and its `is_empty` pair, which clippy requires alongside it)
    /// unused. This allowance is scoped to the two accessors rather than
    /// the impl block so a collection method that lost its last caller
    /// still surfaces.
    #[allow(dead_code)]
    pub const fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the sample has stored nothing at all.
    #[allow(dead_code)]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether the sample has reached the cap and can store nothing more.
    ///
    /// Exists so a collection site whose item is EXPENSIVE to materialize
    /// (a rendered path, a formatted fragment) can skip the work entirely
    /// once no further item can be stored -- see
    /// [`Self::push_distinct_lazily`].
    pub const fn is_full(&self) -> bool {
        self.items.len() >= MAX_LOGGED_DIAGNOSTIC_ITEMS
    }

    /// Whether at least one item was dropped because the sample was
    /// already full.
    ///
    /// This is never derived from an offered-vs-stored comparison: for a
    /// deduplicating sample, repeats of an already-stored value are fully
    /// represented in the rendered sample, so a run of duplicates leaves
    /// this `false` however long it is.
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

impl<T: PartialEq> BoundedLogSample<T> {
    /// Store `value` unless an equal value is already stored. A linear
    /// scan over at most `MAX_LOGGED_DIAGNOSTIC_ITEMS` entries is cheaper
    /// than hashing and keeps first-seen order in the rendered sample.
    pub fn push_distinct(&mut self, value: T) {
        if self.items.contains(&value) {
            return;
        }
        self.push(value);
    }

    /// Fold `other`'s items into this sample, keeping the same cap and
    /// carrying its truncation flag across.
    ///
    /// For a tally that runs the same collection pass twice over one request
    /// (a post-assembly rewrite re-scanned by the seam that made it) so the
    /// two runs still yield ONE aggregated record.
    pub fn absorb(&mut self, other: Self) {
        let Self { items, truncated } = other;
        for item in items {
            self.push_distinct(item);
        }
        self.truncated |= truncated;
    }

    /// [`Self::push_distinct`] for a value that costs real work to
    /// materialize: `materialize` is NOT called once the sample is full, so
    /// the collection site pays only for the items that can actually reach
    /// the log record.
    ///
    /// The one semantic difference from `push_distinct`: at capacity this
    /// marks the sample truncated without knowing whether the value would
    /// have been a repeat of a stored one, because deciding that would
    /// require the very work being skipped. A collection site whose values
    /// are already distinct by construction loses nothing; one that expects
    /// heavy duplication and needs exact truncation should keep calling
    /// `push_distinct`.
    pub fn push_distinct_lazily(&mut self, materialize: impl FnOnce() -> T) {
        if self.is_full() {
            self.truncated = true;
            return;
        }
        self.push_distinct(materialize());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bounding happens as items are collected, so far more offers than
    /// the cap must still leave the stored sample at the cap -- the
    /// diagnostic path never holds the full list to truncate later.
    #[test]
    fn push_stores_at_most_the_cap_however_many_items_arrive() {
        // Arrange
        let mut sample: BoundedLogSample<u32> = BoundedLogSample::new();

        // Act
        for i in 0..(MAX_LOGGED_DIAGNOSTIC_ITEMS as u32 * 5) {
            sample.push(i);
            assert!(sample.len() <= MAX_LOGGED_DIAGNOSTIC_ITEMS);
        }

        // Assert
        assert_eq!(sample.len(), MAX_LOGGED_DIAGNOSTIC_ITEMS);
        assert_eq!(sample.items(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert!(sample.truncated());
    }

    #[test]
    fn push_leaves_a_sample_within_the_cap_untruncated() {
        // Arrange
        let mut sample: BoundedLogSample<u32> = BoundedLogSample::new();
        assert!(sample.is_empty());

        // Act
        for i in 0..3 {
            sample.push(i);
        }

        // Assert
        assert_eq!(sample.items(), &[0, 1, 2]);
        assert!(!sample.truncated());
    }

    #[test]
    fn push_distinct_keeps_first_seen_order_and_skips_repeats() {
        // Arrange
        let mut sample: BoundedLogSample<&str> = BoundedLogSample::new();

        // Act
        for value in ["zeta", "alpha", "zeta", "beta", "alpha"] {
            sample.push_distinct(value);
        }

        // Assert
        assert_eq!(sample.items(), &["zeta", "alpha", "beta"]);
        assert!(!sample.truncated());
    }

    /// The defect this guards: deriving truncation from offered-vs-stored
    /// counts. Ten offers of one value store one item and drop nothing,
    /// so the sample renders the whole truth and is not truncated.
    #[test]
    fn push_distinct_all_duplicates_never_reports_truncation() {
        // Arrange
        let mut sample: BoundedLogSample<&str> = BoundedLogSample::new();

        // Act
        for _ in 0..10 {
            sample.push_distinct("foreign-format");
        }

        // Assert
        assert_eq!(sample.len(), 1);
        assert!(
            !sample.truncated(),
            "duplicates are fully represented by the stored value"
        );
    }

    #[test]
    fn push_distinct_reports_truncation_only_when_a_new_value_is_rejected() {
        // Arrange: exactly at capacity, nothing dropped yet.
        let mut sample: BoundedLogSample<u32> = BoundedLogSample::new();
        for i in 0..(MAX_LOGGED_DIAGNOSTIC_ITEMS as u32) {
            sample.push_distinct(i);
        }
        assert_eq!(sample.len(), MAX_LOGGED_DIAGNOSTIC_ITEMS);
        assert!(!sample.truncated());

        // Act: a repeat of a stored value at capacity is not a rejection.
        sample.push_distinct(0);

        // Assert
        assert!(!sample.truncated());
        assert_eq!(sample.len(), MAX_LOGGED_DIAGNOSTIC_ITEMS);

        // Act: a distinct value at capacity is.
        sample.push_distinct(u32::MAX);

        // Assert
        assert!(sample.truncated());
        assert_eq!(sample.len(), MAX_LOGGED_DIAGNOSTIC_ITEMS);
        assert!(!sample.items().contains(&u32::MAX));
    }

    #[test]
    fn push_distinct_lazily_stops_materializing_once_the_sample_is_full() {
        // Arrange
        let mut sample: BoundedLogSample<u32> = BoundedLogSample::new();
        let mut materializations = 0usize;

        // Act
        for i in 0..(MAX_LOGGED_DIAGNOSTIC_ITEMS as u32 * 5) {
            sample.push_distinct_lazily(|| {
                materializations += 1;
                i
            });
        }

        // Assert
        assert_eq!(
            materializations, MAX_LOGGED_DIAGNOSTIC_ITEMS,
            "a full sample must not pay to build a value it cannot store"
        );
        assert_eq!(sample.len(), MAX_LOGGED_DIAGNOSTIC_ITEMS);
        assert!(
            sample.truncated(),
            "the dropped items must still be visible"
        );
        assert!(sample.is_full());
    }

    /// Below capacity the lazy path is the eager path: it materializes and it
    /// deduplicates, so repeats neither grow the sample nor claim truncation.
    #[test]
    fn push_distinct_lazily_deduplicates_below_capacity() {
        // Arrange
        let mut sample: BoundedLogSample<&str> = BoundedLogSample::new();

        // Act
        for value in ["alpha", "beta", "alpha"] {
            sample.push_distinct_lazily(|| value);
        }

        // Assert
        assert_eq!(sample.items(), &["alpha", "beta"]);
        assert!(!sample.truncated());
        assert!(!sample.is_full());
    }
}
