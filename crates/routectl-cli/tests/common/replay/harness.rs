//! Shared bits across the two replay test drivers (`replay_egress.rs`
//! and `replay_ingress.rs`): the canon root locator, the loader-vector
//! to `HeaderMap` bridge, and the per-fixture outcome enum.
//!
//! These were duplicated verbatim across both test files until they
//! grew in lockstep one too many times. Hoisting them removes the
//! "edit one, forget the other" failure mode without introducing a
//! provider-registry abstraction (over-engineering for three
//! providers; see the cross-reference comments in the egress / ingress
//! match arms).

use std::path::PathBuf;

use axum::http::{HeaderMap, HeaderName, HeaderValue};

/// Path (relative to the workspace root) to the hand-curated fixture
/// corpus. `discover_fixtures` returns an empty vector when the
/// directory contains only `.gitkeep` / `README.md`, which keeps the
/// replay tests passing before the seed corpus lands.
pub fn canon_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/canon")
}

/// Build a `HeaderMap` from the `(name, value)` pairs persisted in a
/// fixture's `*.headers.json`. A pair the `http` crate refuses to
/// accept is logged to stderr (with the offending name) and skipped --
/// this is a fixture-authoring bug we want to surface, but failing the
/// whole test on it would mask the comparator output that pinpoints
/// the real wire-shape issue.
pub fn headers_from_pairs(pairs: &[(String, String)]) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in pairs {
        let parsed_name = match HeaderName::from_bytes(name.as_bytes()) {
            Ok(n) => n,
            Err(_) => {
                eprintln!(
                    "[replay] dropping malformed header name `{}` (header value not echoed; \
                     fix the fixture's *.headers.json)",
                    name,
                );
                continue;
            }
        };
        let parsed_value = match HeaderValue::from_str(value) {
            Ok(v) => v,
            Err(_) => {
                eprintln!(
                    "[replay] dropping malformed header value on `{}` \
                     (value not echoed; fix the fixture's *.headers.json)",
                    name,
                );
                continue;
            }
        };
        out.insert(parsed_name, parsed_value);
    }
    out
}

/// Outcome of one fixture's run. `Skipped` carries a human-readable
/// reason so the test driver can surface it as an info log rather than
/// a failure. `Asserted` means the fixture was exercised end-to-end.
pub enum FixtureOutcome {
    Asserted,
    Skipped(String),
}
