use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use routectl_auth::{MemoryStore, SecretRef, SecretStore};
use tempfile::NamedTempFile;

// Note on env-mutating tests below: each test uses a UNIQUE
// `ROUTECTL_TEST_*` key name so no two tests race on the same
// value. The libc `setenv` / `unsetenv` functions are not
// thread-safe at the C level (Rust 1.81 marked the wrappers
// `unsafe` for this reason), but as long as the keys are
// disjoint AND no other code in this process reads/writes the
// same keys, the parallel test runner is safe in practice.
// Adding a `Mutex<()>` here ran into a `clippy::await_holding_lock`
// warning because the surrounding tests are async; dropping the
// guard before `.await` reintroduces the race window. The
// `unique-key` invariant is what makes the current tests correct.
// If a future test needs to share a key, switch to
// `#[serial_test::serial]` + dropping `tokio::test`.

// --- SecretRef::parse ---

#[test]
fn parse_env_valid() {
    let r = SecretRef::parse("env://MY_VAR").unwrap();
    assert_eq!(r, SecretRef::Env("MY_VAR".into()));
}

#[test]
fn parse_env_empty_var_is_error() {
    let err = SecretRef::parse("env://").unwrap_err();
    assert!(err.to_string().contains("missing variable name"));
}

#[test]
fn parse_file_valid_absolute_path() {
    let r = SecretRef::parse("file:///etc/secrets/openrouter").unwrap();
    assert_eq!(r, SecretRef::File("/etc/secrets/openrouter".into()));
}

#[test]
fn parse_file_empty_path_is_error() {
    let err = SecretRef::parse("file://").unwrap_err();
    assert!(err.to_string().contains("missing path"));
}

#[test]
fn parse_file_relative_path_is_error() {
    let err = SecretRef::parse("file://relative/key").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("absolute"), "got: {msg}");
    assert!(msg.contains("file:///abs/path"), "got: {msg}");
}

#[test]
fn parse_file_dot_relative_path_is_error() {
    let err = SecretRef::parse("file://./key").unwrap_err();
    assert!(err.to_string().contains("absolute"));
}

#[test]
fn parse_literal_valid() {
    let r = SecretRef::parse("literal:hello-world").unwrap();
    assert_eq!(r, SecretRef::Literal("hello-world".into()));
}

#[test]
fn parse_literal_empty_value() {
    let r = SecretRef::parse("literal:").unwrap();
    assert_eq!(r, SecretRef::Literal("".into()));
}

#[test]
fn parse_unknown_scheme_is_error() {
    let err = SecretRef::parse("http://example.com").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unrecognized secret URI scheme"));
    assert!(
        msg.contains("env://"),
        "error should hint at env:// : {msg}"
    );
    assert!(
        msg.contains("file://"),
        "error should hint at file:// : {msg}"
    );
}

// --- Display for SecretRef ---

#[test]
fn display_env() {
    assert_eq!(SecretRef::Env("FOO".into()).to_string(), "env://FOO");
}

#[test]
fn display_file() {
    assert_eq!(
        SecretRef::File("/tmp/key".into()).to_string(),
        "file:///tmp/key"
    );
}

#[test]
fn display_literal_redacts_value() {
    // SecretRef::Literal is the secret material in-line; Display
    // must NOT echo the value. Any caller that `format!`s a
    // SecretRef would otherwise leak the literal payload into
    // logs / shell history / CI output.
    assert_eq!(
        SecretRef::Literal("hunter2".into()).to_string(),
        "literal:[REDACTED]"
    );
}

#[test]
fn display_literal_empty_distinguishable() {
    // An empty literal stays distinguishable from a redacted one
    // so operators can spot a placeholder (literal:) vs a real
    // secret in error/check output without guessing.
    assert_eq!(SecretRef::Literal(String::new()).to_string(), "literal:");
}

// --- MemoryStore: env path ---

#[tokio::test]
async fn env_get_resolves_via_process_env() {
    std::env::set_var("ROUTECTL_TEST_VAR_RESOLVE", "env-value");
    let store = MemoryStore::new();
    let r = SecretRef::Env("ROUTECTL_TEST_VAR_RESOLVE".into());
    let got = store.get(&r).await.unwrap();
    assert_eq!(got, "env-value");
    std::env::remove_var("ROUTECTL_TEST_VAR_RESOLVE");
}

#[tokio::test]
async fn env_get_missing_is_error() {
    std::env::remove_var("ROUTECTL_TEST_VAR_MISSING");
    let store = MemoryStore::new();
    let r = SecretRef::Env("ROUTECTL_TEST_VAR_MISSING".into());
    let err = store.get(&r).await.unwrap_err();
    assert!(err.to_string().contains("not set"));
}

// --- MemoryStore: literal path ---

#[tokio::test]
async fn literal_get_returns_inline_value() {
    let store = MemoryStore::new();
    let r = SecretRef::Literal("my-secret".into());
    assert_eq!(store.get(&r).await.unwrap(), "my-secret");
}

#[tokio::test]
async fn literal_empty_returns_empty_string() {
    let store = MemoryStore::new();
    let r = SecretRef::Literal("".into());
    assert_eq!(store.get(&r).await.unwrap(), "");
}

// --- MemoryStore: file path ---
//
// File-permission enforcement is Unix-only (POSIX mode bits via
// PermissionsExt); the tests below that depend on it are gated with
// `#[cfg(unix)]` so the suite still compiles + runs cleanly on
// Windows.

#[cfg(unix)]
fn write_secret_file(contents: &str, mode: u32) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("tempfile");
    f.write_all(contents.as_bytes()).expect("write");
    let mut perms = f.as_file().metadata().expect("meta").permissions();
    perms.set_mode(mode);
    f.as_file().set_permissions(perms).expect("chmod");
    f
}

#[cfg(not(unix))]
fn write_secret_file(contents: &str, _mode: u32) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("tempfile");
    f.write_all(contents.as_bytes()).expect("write");
    f
}

#[tokio::test]
async fn file_get_returns_trimmed_contents() {
    let file = write_secret_file("sk-abc-12345\n", 0o600);
    let store = MemoryStore::new();
    let r = SecretRef::File(file.path().to_path_buf());
    let got = store.get(&r).await.unwrap();
    assert_eq!(got, "sk-abc-12345");
}

#[tokio::test]
async fn file_get_handles_no_trailing_newline() {
    let file = write_secret_file("sk-no-newline", 0o400);
    let store = MemoryStore::new();
    let r = SecretRef::File(file.path().to_path_buf());
    assert_eq!(store.get(&r).await.unwrap(), "sk-no-newline");
}

#[tokio::test]
async fn file_get_oversized_is_refused() {
    // 1 MiB + 1 byte. Real secrets are bytes-to-kilobytes; a misconfigured
    // path (e.g. /var/log/syslog) must not drive the loader toward OOM.
    let oversized = "a".repeat((1 << 20) + 1);
    let file = write_secret_file(&oversized, 0o600);
    let store = MemoryStore::new();
    let r = SecretRef::File(file.path().to_path_buf());
    let err = store.get(&r).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("refusing to load"), "got: {msg}");
    assert!(msg.contains("cap is"), "got: {msg}");
}

#[tokio::test]
async fn file_get_at_cap_size_succeeds() {
    // Exactly 1 MiB must succeed. Boundary check on the cap.
    let at_cap = "b".repeat(1 << 20);
    let file = write_secret_file(&at_cap, 0o600);
    let store = MemoryStore::new();
    let r = SecretRef::File(file.path().to_path_buf());
    let got = store.get(&r).await.unwrap();
    assert_eq!(got.len(), 1 << 20);
}

#[tokio::test]
async fn file_get_missing_is_error() {
    let store = MemoryStore::new();
    let r = SecretRef::File("/nonexistent/path/to/key".into());
    let err = store.get(&r).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("failed to open secret file"), "got: {msg}");
}

#[cfg(unix)]
#[tokio::test]
async fn file_get_world_readable_is_refused() {
    let file = write_secret_file("leaky-secret", 0o644);
    let store = MemoryStore::new();
    let r = SecretRef::File(file.path().to_path_buf());
    let err = store.get(&r).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("permissions"), "got: {msg}");
    assert!(msg.contains("chmod 600 or 400"), "got: {msg}");
}

#[cfg(unix)]
#[tokio::test]
async fn file_get_group_readable_is_refused() {
    let file = write_secret_file("leaky-group-secret", 0o640);
    let store = MemoryStore::new();
    let r = SecretRef::File(file.path().to_path_buf());
    let err = store.get(&r).await.unwrap_err();
    assert!(err.to_string().contains("permissions"));
}

#[tokio::test]
async fn file_get_relative_path_is_refused() {
    // Defense in depth: parse() rejects relative `file://` URIs, but
    // the SecretRef::File variant is publicly constructible. Calling
    // `read_secret_file` with a relative path must still refuse it
    // rather than resolving against process CWD.
    let store = MemoryStore::new();
    let r = SecretRef::File("relative/path/to/key".into());
    let err = store.get(&r).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("absolute"), "got: {msg}");
}

#[tokio::test]
async fn file_get_dot_relative_path_is_refused() {
    let store = MemoryStore::new();
    let r = SecretRef::File("./key".into());
    let err = store.get(&r).await.unwrap_err();
    assert!(err.to_string().contains("absolute"));
}

#[tokio::test]
async fn file_get_directory_path_is_refused() {
    // A path pointing at a directory must be refused as "not a
    // regular file" (catches symlink-to-directory swaps).
    let dir = tempfile::tempdir().expect("tempdir");
    let store = MemoryStore::new();
    let r = SecretRef::File(dir.path().to_path_buf());
    let err = store.get(&r).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not a regular file"), "got: {msg}");
}

// --- MemoryStore: set/delete are read-only ---

#[tokio::test]
async fn set_is_read_only_error() {
    let store = MemoryStore::new();
    let r = SecretRef::Env("ANY".into());
    let err = store.set(&r, "v").await.unwrap_err();
    assert!(err.to_string().contains("read-only"));
}

#[tokio::test]
async fn delete_is_read_only_error() {
    let store = MemoryStore::new();
    let r = SecretRef::Literal("x".into());
    let err = store.delete(&r).await.unwrap_err();
    assert!(err.to_string().contains("read-only"));
}
