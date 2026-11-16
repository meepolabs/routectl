//! The factory gives the codex client identity, and its session and
//! installation ids, only to a managed codex login on its own host.
//!
//! A managed codex entry can only target chatgpt.com, so this binary never
//! sends a request: it reads the body each built provider would send
//! (`normalize_request`), where the installation id is stamped exactly when
//! the provider presents the identity. The header and User-Agent side of the
//! same flag is pinned on the wire by the providers crate's
//! `client_identity_wire_tests`.
//!
//! The installation id lives under the config dir named by
//! `XDG_CONFIG_HOME`, a process-global, so every test here is serial.

#![cfg(feature = "openai-responses")]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use routectl_auth::{CredentialProbeFailure, SecretRef, SecretStore};
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_router::{BuildOptions, Config, build_provider_with_options};
use routectl_testkit::ScopedEnv;
use serde_json::Value;

const INSTALLATION_ID_KEY: &str = "x-codex-installation-id";

/// Answers every read with a fixed value and counts session id reads.
#[derive(Default)]
struct SeatStore {
    session_reads: AtomicUsize,
}

#[async_trait]
impl SecretStore for SeatStore {
    async fn get(&self, _: &SecretRef) -> routectl_core::Result<String> {
        Ok("seat-token".into())
    }
    async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn probe_credential(&self, _: &SecretRef) -> Result<(), CredentialProbeFailure> {
        Ok(())
    }
    async fn account_id(&self, _: &SecretRef) -> routectl_core::Result<Option<String>> {
        Ok(Some("seat-account".into()))
    }
    async fn peek_session_id(&self, _: &SecretRef) -> Option<String> {
        self.session_reads.fetch_add(1, Ordering::SeqCst);
        Some("seat-session".into())
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5-codex".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

/// What one factory build did: the body its provider would send and how
/// many times it read a session id.
struct Built {
    body: Value,
    session_reads: usize,
}

impl Built {
    fn stamped_installation_id(&self) -> Option<&str> {
        self.body
            .get("client_metadata")
            .and_then(|m| m.get(INSTALLATION_ID_KEY))
            .and_then(Value::as_str)
    }
}

async fn build(provider_toml: &str) -> Built {
    let config: Config =
        toml::from_str(&format!("[providers.p]\n{provider_toml}")).expect("fixture parses");
    let entry = config.providers.get("p").expect("provider p");
    let store = Arc::new(SeatStore::default());
    let dyn_store: Arc<dyn SecretStore> = store.clone();
    let provider = build_provider_with_options("p", entry, dyn_store, BuildOptions::default())
        .await
        .expect("provider builds");
    Built {
        body: provider.normalize_request(&request()).expect("body"),
        session_reads: store.session_reads.load(Ordering::SeqCst),
    }
}

fn installation_file(xdg: &Path) -> std::path::PathBuf {
    xdg.join("routectl").join("installation_id")
}

const MANAGED_CODEX: &str = "kind = \"openai-responses\"\n\
     api_key_ref = \"oauth://codex#seat-b\"\n\
     base_url = \"https://chatgpt.com/backend-api/codex\"\n";

#[tokio::test]
#[serial_test::serial]
async fn a_managed_codex_login_mints_its_installation_id_once_and_keeps_it() {
    // Arrange
    let xdg = tempfile::tempdir().expect("temp xdg");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
    let file = installation_file(xdg.path());
    assert!(!file.exists(), "the temp config dir starts empty");

    // Act: two builds over one config dir, as across a daemon restart.
    let first = build(MANAGED_CODEX).await;
    let minted = std::fs::read_to_string(&file).expect("boot 1 mints the id");
    let mtime = std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .expect("mtime");
    let second = build(MANAGED_CODEX).await;

    // Assert
    assert_eq!(first.stamped_installation_id(), Some(minted.trim()));
    assert_eq!(
        second.stamped_installation_id(),
        first.stamped_installation_id(),
        "the id is stable across builds (adopt, never re-mint)"
    );
    assert_eq!(
        std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .expect("mtime"),
        mtime,
        "adoption leaves the file untouched"
    );
    assert_eq!(first.session_reads, 1, "the seat's session id is read");
}

#[tokio::test]
#[serial_test::serial]
async fn no_other_responses_lane_reads_or_mints_codex_ids() {
    let xdg = tempfile::tempdir().expect("temp xdg");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
    let _key = ScopedEnv::set("ROUTECTL_CONTAINED_BUILD_KEY", "static-key");
    #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
    let mut lanes = vec![
        (
            "chatgpt dialect with an env bearer on chatgpt.com",
            "kind = \"openai-responses\"\n\
             auth_kind = \"chatgpt-oauth\"\n\
             api_key_ref = \"env://ROUTECTL_CONTAINED_BUILD_KEY\"\n\
             account_id_ref = \"env://ROUTECTL_CONTAINED_BUILD_KEY\"\n\
             base_url = \"https://chatgpt.com/backend-api/codex\"\n",
        ),
        (
            "responses api-key",
            "kind = \"openai-responses\"\n\
             auth_kind = \"api-key\"\n\
             api_key_ref = \"env://ROUTECTL_CONTAINED_BUILD_KEY\"\n",
        ),
    ];
    #[cfg(feature = "bedrock")]
    lanes.push((
        "responses mantle",
        "kind = \"openai-responses\"\n\
         api_key_ref = \"\"\n\
         bedrock_mantle = { region = \"us-east-1\", creds = { kind = \"bearer-key\", \
         key_ref = \"env://ROUTECTL_CONTAINED_BUILD_KEY\" } }\n",
    ));

    for (case, provider_toml) in lanes {
        let built = build(provider_toml).await;

        assert_eq!(
            built.stamped_installation_id(),
            None,
            "{case}: {}",
            built.body
        );
        assert_eq!(built.session_reads, 0, "{case}: no session id read");
        assert!(
            !installation_file(xdg.path()).exists(),
            "{case}: no installation id minted"
        );
    }

    // Positive control: the same dir and store do mint for the managed login.
    let managed = build(MANAGED_CODEX).await;
    assert!(managed.stamped_installation_id().is_some());
    assert!(installation_file(xdg.path()).exists());
}
