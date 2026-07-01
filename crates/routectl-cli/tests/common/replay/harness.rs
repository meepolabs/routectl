//! Shared bits across the two replay test drivers (`replay_egress.rs`
//! and `replay_ingress.rs`): the captured root locator, the
//! loader-vector to `HeaderMap` bridge, the per-fixture outcome enum,
//! and the Phase 1 model-denylist + skip-reason helper.
//!
//! These were duplicated verbatim across both test files until they
//! grew in lockstep one too many times. Hoisting them removes the
//! "edit one, forget the other" failure mode without introducing a
//! provider-registry abstraction (over-engineering for three
//! providers; see the cross-reference comments in the egress / ingress
//! match arms).

use std::path::PathBuf;

use axum::http::{HeaderMap, HeaderName, HeaderValue};

use super::loader::Fixture;

/// Default replay-fixture root. Per-contributor, local, gitignored at
/// the repo policy level. Populated by `scripts/capture_fixtures.sh`.
/// `discover_fixtures` returns an empty vector when the directory is
/// empty, which keeps the replay tests passing on a fresh checkout
/// before any fixtures have been captured.
pub fn captured_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/captured")
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
        let parsed_name = if let Ok(n) = HeaderName::from_bytes(name.as_bytes()) {
            n
        } else {
            eprintln!(
                "[replay] dropping malformed header name `{name}` (header value not echoed; \
                 fix the fixture's *.headers.json)",
            );
            continue;
        };
        let parsed_value = if let Ok(v) = HeaderValue::from_str(value) {
            v
        } else {
            eprintln!(
                "[replay] dropping malformed header value on `{name}` \
                 (value not echoed; fix the fixture's *.headers.json)",
            );
            continue;
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

/// Substrings flagged as "needs router enrichment not yet wired into
/// replay". A fixture whose `meta.model` contains any of these is
/// skipped on both replay drivers; the constraint is documented in
/// `docs/REPLAY-FIXTURES.md` "Phase 1 corpus scope". Matching is
/// substring + case-insensitive so capture-rig variants
/// (`claude-opus-4-7-...`, `deepseek-v4`, ...) all hit.
pub const PHASE1_MODEL_DENYLIST: &[&str] = &["opus-4", "deepseek"];

/// Phase-one denylist filter: drop fixtures whose model requires the
/// router-side enrichment (adaptive thinking, DeepSeek
/// `history_reasoning`) that the bare ingress -> egress path does not
/// yet replay.
pub fn phase1_skip_reason(fixture: &Fixture) -> Option<String> {
    let model = fixture.meta.model.as_deref()?;
    let lc = model.to_ascii_lowercase();
    for needle in PHASE1_MODEL_DENYLIST {
        if lc.contains(needle) {
            return Some(format!(
                "model `{model}` matches phase-one denylist substring `{needle}`; \
                 needs router enrichment not yet wired into replay",
            ));
        }
    }
    None
}
