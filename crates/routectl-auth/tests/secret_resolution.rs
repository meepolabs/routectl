use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use routectl_auth::{MemoryStore, SecretRef, SecretStore};
use tempfile::NamedTempFile;

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
    assert_eq!(
        r,
        SecretRef::File("/etc/secrets/openrouter".into())
    );
}

#[test]
fn parse_file_empty_path_is_error() {
    let err = SecretRef::parse("file://").unwrap_err();
    assert!(err.to_string().contains("missing path"));
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
    assert!(msg.contains("env://"), "error should hint at env:// : {msg}");
    assert!(msg.contains("file://"), "error should hint at file:// : {msg}");
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
fn display_literal() {
    assert_eq!(
        SecretRef::Literal("val".into()).to_string(),
        "literal:val"
    );
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

fn write_secret_file(contents: &str, mode: u32) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("tempfile");
    f.write_all(contents.as_bytes()).expect("write");
    let mut perms = f.as_file().metadata().expect("meta").permissions();
    perms.set_mode(mode);
    f.as_file().set_permissions(perms).expect("chmod");
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
async fn file_get_missing_is_error() {
    let store = MemoryStore::new();
    let r = SecretRef::File("/nonexistent/path/to/key".into());
    let err = store.get(&r).await.unwrap_err();
    assert!(err.to_string().contains("failed to read secret file"));
}

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

#[tokio::test]
async fn file_get_group_readable_is_refused() {
    let file = write_secret_file("leaky-group-secret", 0o640);
    let store = MemoryStore::new();
    let r = SecretRef::File(file.path().to_path_buf());
    let err = store.get(&r).await.unwrap_err();
    assert!(err.to_string().contains("permissions"));
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
