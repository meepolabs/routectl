use routectl_auth::{KeyringStore, MemoryStore, SecretRef, SecretStore};

// --- SecretRef::parse ---

#[test]
fn parse_keychain_valid() {
    let r = SecretRef::parse("keychain://my-service/my-account").unwrap();
    assert_eq!(
        r,
        SecretRef::Keychain {
            service: "my-service".into(),
            account: "my-account".into(),
        }
    );
}

#[test]
fn parse_keychain_missing_account_is_error() {
    let err = SecretRef::parse("keychain://my-service").unwrap_err();
    assert!(err.to_string().contains("invalid keychain URI"));
}

#[test]
fn parse_env_valid() {
    let r = SecretRef::parse("env://MY_VAR").unwrap();
    assert_eq!(r, SecretRef::Env("MY_VAR".into()));
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
    assert!(err.to_string().contains("unrecognized secret URI scheme"));
}

// --- Display for SecretRef ---

#[test]
fn display_keychain() {
    let r = SecretRef::Keychain {
        service: "svc".into(),
        account: "acc".into(),
    };
    assert_eq!(r.to_string(), "keychain://svc/acc");
}

#[test]
fn display_env() {
    assert_eq!(SecretRef::Env("FOO".into()).to_string(), "env://FOO");
}

#[test]
fn display_literal() {
    assert_eq!(
        SecretRef::Literal("val".into()).to_string(),
        "literal:val"
    );
}

// --- MemoryStore ---

#[tokio::test]
async fn memory_store_set_then_get_roundtrip() {
    let store = MemoryStore::new();
    let r = SecretRef::Keychain {
        service: "svc".into(),
        account: "acc".into(),
    };
    store.set(&r, "secret-value").await.unwrap();
    let got = store.get(&r).await.unwrap();
    assert_eq!(got, "secret-value");
}

#[tokio::test]
async fn memory_store_get_unset_key_is_error() {
    let store = MemoryStore::new();
    let r = SecretRef::Keychain {
        service: "missing".into(),
        account: "nobody".into(),
    };
    let err = store.get(&r).await.unwrap_err();
    assert!(err.to_string().contains("no entry"));
}

#[tokio::test]
async fn memory_store_delete_removes_entry() {
    let store = MemoryStore::new();
    let r = SecretRef::Keychain {
        service: "svc".into(),
        account: "acc".into(),
    };
    store.set(&r, "v").await.unwrap();
    store.delete(&r).await.unwrap();
    let err = store.get(&r).await.unwrap_err();
    assert!(err.to_string().contains("no entry"));
}

#[tokio::test]
async fn memory_store_delete_missing_is_error() {
    let store = MemoryStore::new();
    let r = SecretRef::Keychain {
        service: "ghost".into(),
        account: "ghost".into(),
    };
    let err = store.delete(&r).await.unwrap_err();
    assert!(err.to_string().contains("no entry"));
}

#[tokio::test]
async fn memory_store_env_variant_resolves_via_env() {
    std::env::set_var("ROUTECTL_TEST_VAR_MEM", "env-value");
    let store = MemoryStore::new();
    let r = SecretRef::Env("ROUTECTL_TEST_VAR_MEM".into());
    let got = store.get(&r).await.unwrap();
    assert_eq!(got, "env-value");
    std::env::remove_var("ROUTECTL_TEST_VAR_MEM");
}

#[tokio::test]
async fn memory_store_env_variant_missing_is_error() {
    std::env::remove_var("ROUTECTL_TEST_VAR_MISSING");
    let store = MemoryStore::new();
    let r = SecretRef::Env("ROUTECTL_TEST_VAR_MISSING".into());
    let err = store.get(&r).await.unwrap_err();
    assert!(err.to_string().contains("not set"));
}

#[tokio::test]
async fn memory_store_env_set_is_error() {
    let store = MemoryStore::new();
    let r = SecretRef::Env("ANY".into());
    let err = store.set(&r, "v").await.unwrap_err();
    assert!(err.to_string().contains("read-only"));
}

#[tokio::test]
async fn memory_store_literal_get_returns_value() {
    let store = MemoryStore::new();
    let r = SecretRef::Literal("my-secret".into());
    let got = store.get(&r).await.unwrap();
    assert_eq!(got, "my-secret");
}

#[tokio::test]
async fn memory_store_literal_set_is_error() {
    let store = MemoryStore::new();
    let r = SecretRef::Literal("x".into());
    let err = store.set(&r, "y").await.unwrap_err();
    assert!(err.to_string().contains("read-only"));
}

#[tokio::test]
async fn memory_store_literal_delete_is_error() {
    let store = MemoryStore::new();
    let r = SecretRef::Literal("x".into());
    let err = store.delete(&r).await.unwrap_err();
    assert!(err.to_string().contains("read-only"));
}

// --- KeyringStore: env + literal paths (no OS keychain) ---

#[tokio::test]
async fn keyring_store_env_get_resolves_via_env() {
    std::env::set_var("ROUTECTL_TEST_VAR_KR", "keyring-env-value");
    let store = KeyringStore::new();
    let r = SecretRef::Env("ROUTECTL_TEST_VAR_KR".into());
    let got = store.get(&r).await.unwrap();
    assert_eq!(got, "keyring-env-value");
    std::env::remove_var("ROUTECTL_TEST_VAR_KR");
}

#[tokio::test]
async fn keyring_store_env_set_is_error() {
    let store = KeyringStore::new();
    let r = SecretRef::Env("ANY".into());
    let err = store.set(&r, "v").await.unwrap_err();
    assert!(err.to_string().contains("read-only"));
}

#[tokio::test]
async fn keyring_store_env_delete_is_error() {
    let store = KeyringStore::new();
    let r = SecretRef::Env("ANY".into());
    let err = store.delete(&r).await.unwrap_err();
    assert!(err.to_string().contains("read-only"));
}

#[tokio::test]
async fn keyring_store_literal_get_returns_value() {
    let store = KeyringStore::new();
    let r = SecretRef::Literal("direct-val".into());
    let got = store.get(&r).await.unwrap();
    assert_eq!(got, "direct-val");
}

#[tokio::test]
async fn keyring_store_literal_set_is_error() {
    let store = KeyringStore::new();
    let r = SecretRef::Literal("x".into());
    let err = store.set(&r, "y").await.unwrap_err();
    assert!(err.to_string().contains("read-only"));
}

#[tokio::test]
async fn keyring_store_literal_delete_is_error() {
    let store = KeyringStore::new();
    let r = SecretRef::Literal("x".into());
    let err = store.delete(&r).await.unwrap_err();
    assert!(err.to_string().contains("read-only"));
}

// --- Live keyring test (gated behind RUN_KEYRING_TESTS=1) ---

#[tokio::test]
async fn live_keyring_roundtrip() {
    if std::env::var("RUN_KEYRING_TESTS").as_deref() != Ok("1") {
        return;
    }

    use uuid::Uuid;
    let service = "routectl-tests";
    let account = format!("test-{}", Uuid::new_v4());

    let store = KeyringStore::new();
    let r = SecretRef::Keychain {
        service: service.to_string(),
        account: account.clone(),
    };

    // set then get
    store.set(&r, "live-test-value").await.unwrap();
    let got = store.get(&r).await.unwrap();
    assert_eq!(got, "live-test-value");

    // delete then get fails
    store.delete(&r).await.unwrap();
    let err = store.get(&r).await.unwrap_err();
    assert!(err.to_string().contains("keyring:"));
}
