//! Shared fixtures for routectl-router integration tests.

/// A `file://` secret ref that resolves to `value`; see
/// `routectl_testkit::secret_file_ref`.
#[allow(unused_imports)]
pub use routectl_testkit::secret_file_ref as file_ref;
