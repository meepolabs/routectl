//! Installation-id stability across two factory boots -- the
//! process-restart invariant, pinned at the wire. A temp XDG config dir
//! persists the `installation_id` file across two independent
//! `build_resolved_models` passes: the first boot MINTS the id, the second
//! ADOPTS it from disk, and the SAME id reaches the egress wire on both
//! boots. The adopt-existing-file and mint-atomic paths are unit-tested in
//! the factory; this is the boot-level end-to-end that additionally pins the
//! resolved id onto the outbound request header.
//!
//! `resolved_identity` is a set-once process-global, so this lives in its own
//! test binary with a SINGLE test that walks boot -> boot in order.

use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{BuildOptions, Config, build_resolved_models};
use routectl_testkit::ScopedEnv;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const INSTALLATION_ID_HEADER: &str = "x-codex-installation-id";

fn completed_sse() -> String {
    let completed = serde_json::json!({
        "id": "resp_iid",
        "object": "response",
        "status": "completed",
        "model": "gpt-5-codex",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "pong"}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed).unwrap()
    )
}

fn base_req() -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: "gpt-5-codex".into(),
        messages: vec![routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

/// Boot the factory against `base_url`, drive one completion, and return the
/// `x-codex-installation-id` the egress stamped on the wire.
async fn boot_and_capture_installation_id(base_url: &str, server: &MockServer) -> String {
    let config: Config = toml::from_str(&format!(
        "[providers.cx]\nkind = \"openai-responses\"\nauth_kind = \"chatgpt-oauth\"\n\
         api_key_ref = \"env://ROUTECTL_IID_TOKEN\"\n\
         account_id_ref = \"env://ROUTECTL_IID_ACCT\"\n\
         base_url = \"{base_url}\"\n\
         [models.m]\nprovider = \"cx\"\nupstream = \"gpt-5-codex\"\n",
    ))
    .expect("fixture parses");
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    let (models, failed) = build_resolved_models(&config, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(
        failed.is_empty(),
        "no model should fail to build: {failed:?}"
    );
    let resolved = models.get("m").expect("model m resolved");
    resolved
        .provider
        .complete(base_req())
        .await
        .expect("complete against wiremock");

    let received = server.received_requests().await.expect("captured requests");
    let latest = received.last().expect("at least one egress request");
    latest
        .headers
        .get(INSTALLATION_ID_HEADER)
        .unwrap_or_else(|| panic!("egress must stamp {INSTALLATION_ID_HEADER}"))
        .to_str()
        .expect("installation-id header is ASCII")
        .to_string()
}

#[tokio::test]
#[serial_test::serial]
async fn installation_id_is_stable_across_two_serve_boots() {
    // One temp XDG config dir shared by both boots: it plays the role of the
    // persistent config dir that survives a process restart. Never the real
    // ~/.config/routectl.
    let xdg = tempfile::tempdir().expect("temp xdg");
    let _xdg_guard = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
    // Static bearer + account id via env refs so the factory resolves
    // without an OAuthStore; auth_kind stays chatgpt-oauth so the egress
    // stamps the installation-id header.
    let _tok = ScopedEnv::set("ROUTECTL_IID_TOKEN", "test-jwt");
    let _acct = ScopedEnv::set("ROUTECTL_IID_ACCT", "acct-uuid");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(completed_sse()),
        )
        .mount(&server)
        .await;

    // Boot 1: the config dir has no installation_id file, so the factory
    // mints one and the egress stamps it.
    let id_file = xdg.path().join("routectl").join("installation_id");
    assert!(
        !id_file.exists(),
        "the temp config dir must start without an installation_id file",
    );
    let first = boot_and_capture_installation_id(&server.uri(), &server).await;
    assert!(
        id_file.exists(),
        "boot 1 must mint the installation_id file"
    );
    let on_disk = std::fs::read_to_string(&id_file)
        .expect("read minted id")
        .trim()
        .to_string();
    assert_eq!(
        first, on_disk,
        "the id on the wire must equal the id persisted to disk",
    );
    // Record the persisted file's mtime so boot 2 can prove it read the file
    // rather than re-writing it.
    let mtime_after_boot1 = std::fs::metadata(&id_file)
        .expect("stat minted id")
        .modified()
        .expect("mtime available");

    // Boot 2: a fresh factory pass against the SAME config dir adopts the
    // persisted file rather than minting a new id -- the same value reaches
    // the wire, exactly as it would across a real daemon restart.
    let second = boot_and_capture_installation_id(&server.uri(), &server).await;
    // A minted id is a random UUIDv4, so an equal value across two boots can
    // only come from reading the persisted file -- a re-mint would produce a
    // different random id. This is the core adoption proof.
    assert_eq!(
        first, second,
        "the installation-id must be stable across boots (adopt, never re-mint)",
    );
    // Adoption is a pure read: the file's content and mtime are unchanged, so
    // boot 2 did not rewrite (or overwrite-with-an-equal-value) the file.
    let on_disk_after_boot2 = std::fs::read_to_string(&id_file)
        .expect("read adopted id")
        .trim()
        .to_string();
    assert_eq!(
        on_disk, on_disk_after_boot2,
        "boot 2 must not rewrite the installation_id file",
    );
    let mtime_after_boot2 = std::fs::metadata(&id_file)
        .expect("stat adopted id")
        .modified()
        .expect("mtime available");
    assert_eq!(
        mtime_after_boot1, mtime_after_boot2,
        "adoption is a read-only path: boot 2 must not touch the file's mtime",
    );

    let received = server.received_requests().await.expect("captured requests");
    assert_eq!(received.len(), 2, "one egress request per boot");
}
