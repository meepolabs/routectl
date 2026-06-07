//! Thin re-export of the shared contract-test fixture builders, plus
//! the cli-only `replay` harness submodule.
//!
//! The single source of truth for the canonical-request /
//! canonical-response builders lives in `routectl_core::test_utils`
//! (gated behind the `test-utils` feature, enabled here as a
//! dev-dependency). This shim exists so the ingress contract tests can
//! keep referring to `common::scenarios::*` and the bare helpers
//! (`common::user_msg`, etc.) unchanged, while `replay` stays local to
//! this crate (it depends on cli-side fixtures and harness code).

pub mod replay;

// Not every test binary uses the scenario builders (e.g. the replay
// binaries touch only `common::replay`); the glob re-export is dead in
// those compilation units, which is expected for a shared module.
#[allow(unused_imports)]
pub use routectl_core::test_utils::*;
