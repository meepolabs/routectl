//! Test-only resolvable secret refs: `file_ref(value)` is a `file://` ref to
//! an owner-only file holding exactly `value`, standing in for the rejected
//! `literal:` scheme. The files live under the shared testkit temp reaper.

pub use routectl_testkit::secret_file_ref as file_ref;
