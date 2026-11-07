//! A hot reload whose candidate config carries a pool member that refuses the
//! config is rejected with a WARN: the running router keeps serving and the
//! daemon meta records the rejection's class. Asserted per refusal surface,
//! the validation gate (a member with no credential reference) and the member
//! build (a Codex member whose login reads but carries no account id, so its
//! provider cannot be built).

use super::*;
use routectl_auth::oauth::testing::{ScriptedRefresh, ScriptedRefreshFlow};
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_router::RouterOptions;
use std::time::{SystemTime, UNIX_EPOCH};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Pool `alpha` of one seat behind model `grok`, dialing `upstream`, plus
/// `extra` appended verbatim.
fn config_text(upstream: &str, members: &str, extra: &str) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [retry]\nmax_attempts = 1\n\n\
         [providers.seat-a]\nkind = \"anthropic-api\"\nbase_url = \"{upstream}\"\n\
         api_key_ref = \"oauth://test-loopback#seat-a\"\nauth_kind = \"oauth-bearer\"\n\n\
         {extra}\
         [pools.alpha]\nmembers = [{members}]\n\n\
         [models.grok]\nprovider = \"alpha\"\nupstream = \"claude-sonnet-4-6\"\n"
    )
}

/// Each refused candidate, with the class its rejection records.
fn refused_candidates(upstream: &str) -> Vec<(&'static str, String, ReloadFailure)> {
    vec![
        (
            "member with no credential ref",
            config_text(
                upstream,
                "\"seat-a\", \"seat-b\"",
                "[providers.seat-b]\nkind = \"anthropic-api\"\napi_key_ref = \"\"\n\n",
            ),
            ReloadFailure::ConfigLoadFailed,
        ),
        (
            "member whose provider cannot be built",
            config_text(
                upstream,
                "\"seat-a\"",
                "[providers.seat-c]\nkind = \"openai-responses\"\n\
                 api_key_ref = \"oauth://codex#seat-c\"\n\n\
                 [providers.seat-d]\nkind = \"openai-responses\"\n\
                 api_key_ref = \"oauth://codex#seat-d\"\n\n\
                 [pools.beta]\nmembers = [\"seat-c\", \"seat-d\"]\n\n\
                 [models.gpt]\nprovider = \"beta\"\nupstream = \"gpt-5-codex\"\n\n",
            ),
            ReloadFailure::RouterBuildFailed,
        ),
    ]
}

fn write_credentials(creds: &Path) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut providers = serde_json::Map::new();
    // `codex#seat-c` is logged in but carries no account id, so its probe
    // passes and its provider cannot be built. Its sibling `codex#seat-d`
    // builds, so the pool is refused for seat-c alone, not for having no
    // usable member.
    for (seat, account_id) in [
        ("test-loopback#seat-a", None),
        ("codex#seat-c", None),
        ("codex#seat-d", Some("acct-seat-d")),
    ] {
        providers.insert(
            seat.to_string(),
            serde_json::json!({
                "access_token": "tok",
                "refresh_token": "seeded-refresh-token",
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": [],
                "account": { "email": null, "account_id": account_id },
                "obtained_at_unix": now
            }),
        );
    }
    let doc = serde_json::json!({ "schema_version": 1, "providers": providers });
    std::fs::create_dir_all(creds.parent().expect("creds parent")).expect("mkdir");
    std::fs::write(creds, serde_json::to_vec_pretty(&doc).expect("json")).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(creds, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "grok".into(),
        max_tokens: Some(16),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    }
}

async fn mount_ok(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "model": "claude-sonnet-4-6",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(upstream)
        .await;
}

#[tokio::test]
#[serial_test::serial]
async fn a_reload_carrying_a_refused_pool_member_keeps_the_running_router_serving() {
    for (name, candidate, expected_class) in refused_candidates("http://placeholder") {
        // Arrange: a running router over a healthy one-seat pool.
        let dir = tempfile::tempdir().unwrap();
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let creds = dir.path().join("routectl").join("credentials.json");
        write_credentials(&creds);
        let flow = Arc::new(ScriptedRefreshFlow::new(ScriptedRefresh::RefreshExpired));
        let oauth = Arc::new(
            routectl_auth::OAuthStore::open_with_scripted_refresh_for_tests(&creds, flow)
                .await
                .expect("open the credentials store"),
        );
        let secrets: Arc<dyn SecretStore> = Arc::new((*oauth).clone());
        let upstream = MockServer::start().await;
        mount_ok(&upstream).await;
        let good = config_text(&upstream.uri(), "\"seat-a\"", "");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, &good).unwrap();
        let mut config = routectl_router::parse_config(&good).expect("fixture parses");
        let _usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let router =
            build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
                .await
                .expect("initial router build");
        let swap = Arc::new(ArcSwap::from_pointee(router));
        let before = swap.load_full();
        let daemon_meta = crate::handlers::status::DaemonMeta::for_test();
        let status = crate::handlers::status::StatusState::from_app(
            &crate::server::AppState::for_test(swap.clone()),
            None,
            daemon_meta.clone(),
        );
        let ctx = ReloadContext {
            config_path: Some(cfg_path.clone()),
            oauth_store: None,
            secrets,
            router_swap: swap.clone(),
            activation_swap: Arc::new(ArcSwap::from_pointee(ActivationState::default())),
            usage: routectl_usage::handle_with_closed_channel(),
            daemon_meta: daemon_meta.clone(),
        };
        let mut current_config = config;
        let mut current_overlay: Arc<CatalogOverlay> = Arc::default();
        let candidate = candidate.replace("http://placeholder", &upstream.uri());
        std::fs::write(&cfg_path, &candidate).unwrap();
        let check_lines = if expected_class == ReloadFailure::RouterBuildFailed {
            let candidate_config =
                routectl_router::parse_config(&candidate).expect("candidate parses");
            let offline: Arc<dyn SecretStore> = Arc::new(
                crate::commands::config_member_build::OfflineCheckStore::over(Some(
                    (*oauth).clone(),
                )),
            );
            crate::commands::config_member_build::member_build_errors(&candidate_config, offline)
                .await
        } else {
            Vec::new()
        };

        // Act
        let ((), events) = routectl_testkit::with_capture(Box::pin(apply_config_reload(
            &ctx,
            &mut current_config,
            &mut current_overlay,
            ReloadTrigger::ConfigFile,
            &mut never_shutdown(),
        )))
        .await;
        let served = swap
            .load()
            .complete_with_options(request(), RouterOptions::new())
            .await;

        // Assert
        assert!(
            Arc::ptr_eq(&before, &swap.load_full()),
            "`{name}`: the running router must stay installed"
        );
        assert_eq!(
            status
                .daemon_meta
                .reload_failure(chrono::Utc::now().timestamp_millis())
                .map(|f| f.class),
            Some(expected_class),
            "`{name}`: the rejection is recorded"
        );
        // The validation-gate WARN is emitted on the blocking-pool thread the
        // loader runs on, outside this thread's capture; the loader's own WARN
        // for the same rows is pinned by
        // `a_hot_reload_carrying_any_bad_pool_is_declined`.
        if expected_class == ReloadFailure::RouterBuildFailed {
            let warned: Vec<Option<&str>> = events
                .iter()
                .filter(|e| {
                    e.level == tracing::Level::WARN && e.message.contains("config reload failed")
                })
                .map(|e| e.field("error"))
                .collect();
            assert_eq!(check_lines.len(), 1, "`{name}`: {check_lines:?}");
            assert_eq!(
                warned,
                vec![Some(check_lines[0].as_str())],
                "`{name}`: the reload WARN carries the refusal `config check` reports"
            );
        }
        assert!(
            served.result.is_ok(),
            "`{name}`: the running router still serves: {:?}",
            served.result.err()
        );
        assert_eq!(
            served.meta.served_provider.as_deref(),
            Some("seat-a"),
            "`{name}`"
        );
    }
}
