//! Shared fixtures for the probe-scheduler test sidecars.
//!
//! One definition each, so a fixture change cannot leave one sidecar
//! describing a scheduler the others no longer build.

use super::ProbePayload;
use crate::field_verdict::FieldVerdictKey;

/// The bounded payload every scheduler test activates with. The scheduler
/// only carries it; the worker-level tests pin its content.
pub(super) fn payload() -> ProbePayload {
    // Through the CONSTRUCTOR so this fixture is held to the same retention
    // bound as every other payload in the tree. A fixture exempt from it could
    // carry a value the production path would have refused, and the tests
    // relying on that bound would be asserting against a shape that cannot
    // occur.
    ProbePayload::new(
        "thinking.enabled.display",
        "summarized".to_string(),
        &[],
        &[],
        false,
    )
    .expect("a modeled display token is within the retention bound")
}

/// A distinct identity per `n`, minted through the production constructor
/// so no test can spell a permanent key the grammar would refuse.
pub(super) fn key(n: usize) -> FieldVerdictKey {
    FieldVerdictKey::new(
        &format!("anthropic:model-{n}"),
        "thinking.enabled.display",
        "anthropic-api",
    )
    .expect("the grounded path mints an identity on this lane")
}

/// The one grounded path, on one lane, under two distinct capabilities.
pub(super) fn key_with_path(path: &str) -> FieldVerdictKey {
    FieldVerdictKey::new("anthropic:model-0", path, "anthropic-api")
        .expect("a well-formed dotted path mints an identity")
}
