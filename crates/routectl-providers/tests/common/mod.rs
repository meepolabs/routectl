//! Thin re-export of the shared contract-test fixture builders.
//!
//! The single source of truth lives in
//! `routectl_core::test_utils` (gated behind the `test-utils` feature,
//! enabled here as a dev-dependency). This shim exists only so the
//! egress contract tests can keep referring to `common::scenarios::*`
//! and the bare helpers (`common::user_msg`, etc.) unchanged.

// Not every egress test binary uses every helper; the glob re-export is
// dead in some compilation units, which is expected for a shared module.
#[allow(unused_imports)]
pub use routectl_core::test_utils::*;
