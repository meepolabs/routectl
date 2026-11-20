//! Which scoped usage windows bind a conversation on a given upstream model.
//!
//! A vendor reports some windows per model family (Anthropic's
//! `seven_day_opus`, `seven_day_sonnet`). Such a window limits only the
//! conversations on that family, so it counts for placement only when the
//! conversation's upstream model id is known to belong to it. The mapping is a
//! closed table of exact scope tags to model id prefixes, grounded in captured
//! usage bodies; a scope not in the table is ignored, never matched by
//! substring, and named once in the log so a new vendor scope is noticed.

use std::collections::HashSet;
use std::sync::LazyLock;

use parking_lot::Mutex;

/// Scope tag -> the upstream model id prefix of the family it limits.
const SCOPED_FAMILIES: &[(&str, &str)] = &[("opus", "claude-opus-"), ("sonnet", "claude-sonnet-")];

/// How many distinct unknown scope tags one process logs. Scope tags are
/// vendor-chosen, so the set of tags logged is bounded rather than trusted to
/// stay small.
const LOGGED_SCOPE_CAPACITY: usize = 64;

/// Whether the window scoped `scope` binds a conversation on upstream model
/// `model`. An unknown scope never binds, and is logged once per tag.
pub fn binds(scope: &str, model: &str) -> bool {
    match SCOPED_FAMILIES.iter().find(|(tag, _)| *tag == scope) {
        Some((_, prefix)) => model.starts_with(prefix),
        None => {
            if IGNORED_SCOPES.claim(scope) {
                tracing::info!(
                    event = "placement_scope_ignored",
                    scope = %routectl_core::sanitize_for_log(scope),
                    "a usage window with an unrecognized scope does not count for placement",
                );
            }
            false
        }
    }
}

static IGNORED_SCOPES: LazyLock<ScopeLog> =
    LazyLock::new(|| ScopeLog::with_capacity(LOGGED_SCOPE_CAPACITY));

/// The scope tags already logged, up to a fixed capacity.
#[derive(Debug)]
pub(super) struct ScopeLog {
    seen: Mutex<HashSet<String>>,
    capacity: usize,
}

impl ScopeLog {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            seen: Mutex::new(HashSet::new()),
            capacity,
        }
    }

    /// Claim the one log line for `scope`: true the first time a tag is seen
    /// while the set has room, false otherwise.
    pub(super) fn claim(&self, scope: &str) -> bool {
        let mut seen = self.seen.lock();
        if seen.len() >= self.capacity || seen.contains(scope) {
            return false;
        }
        seen.insert(scope.to_owned())
    }
}

#[cfg(test)]
#[path = "model_scope_tests.rs"]
mod model_scope_tests;
