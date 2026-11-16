use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_router::{CURRENT_CONFIG_VERSION, Config, RouterOptions};
use routectl_testkit::{CapturedEvent, ScopedEnv};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::server::build_router_from_config;
use crate::server::reload::handle_config_reload;
use crate::server::reload_failure::ReloadFailure;
use crate::server::serve::build_usage_writer;
use crate::server::test_support::{drain_usage_writer_strict, isolate_usage_db, never_shutdown};

const RELOAD_FAILED: &str = "config reload failed; keeping previous config";

fn config_body(extra: &str) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\n\
         host = \"127.0.0.1\"\n\
         port = 0\n\
         [providers.fast]\n\
         format = \"openai-chat\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         credential = \"env://ROUTECTL_TEST_UNSET_KEY\"\n\
         auth = \"bearer\"\n\
         [models.gpt]\n\
         provider = \"fast\"\n\
         upstream = \"gpt-4o\"\n\
         [aliases]\n\
         default = \"gpt\"\n\
         {extra}"
    )
}

fn with_one_retired_key() -> String {
    config_body("[fidelity]\nprefix_impact_opt_in = [\"fast\"]\n")
}

/// Three retired keys from three different rows.
fn with_three_retired_keys() -> String {
    config_body(
        "[mitm]\ncredential_source = \"forwarded\"\n\
         [fidelity]\nprefix_impact_opt_in = [\"fast\"]\n\
         [bedrock]\nallowed_betas = [\"x\"]\n",
    )
}

/// The router driver's own wording for `body`, which every load path must
/// surface unchanged.
fn driver_message(body: &str) -> String {
    routectl_router::preflight_raw_config(body)
        .expect_err("premise: the driver refuses the fixture")
        .to_string()
}

/// Writes `body` to a fresh config dir; the guard scopes XDG_CONFIG_HOME so
/// the loader never sees the operator's real config.
fn write_config(body: &str) -> (tempfile::TempDir, ScopedEnv, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let path = dir.path().join("config.toml");
    std::fs::write(&path, body).expect("write config.toml");
    (dir, xdg, path)
}

fn assert_names_retired_key(err: &str, expected: &str) {
    assert_eq!(err, expected);
    assert!(
        !err.contains("unknown field"),
        "the key is refused by name, not as an unknown field: {err}"
    );
}

#[test]
#[serial_test::serial]
fn cold_start_and_config_check_refuse_a_retired_key_with_the_drivers_wording() {
    let body = with_one_retired_key();
    let expected = driver_message(&body);
    let (_dir, _xdg, path) = write_config(&body);

    let cold = match load_effective_config(&path) {
        Ok(_) => panic!("a config carrying a retired key must not load"),
        Err(e) => e,
    };
    let check = match load_effective_config_unvalidated(&path) {
        Ok(_) => panic!("config check must refuse a retired key"),
        Err(e) => e,
    };

    assert_names_retired_key(&cold, &expected);
    assert_names_retired_key(&check, &expected);
}

/// A hot reload carrying three retired keys is declined across the real
/// reload boundary: router A stays published with the config it was built
/// from, and the reload's own loader emits one WARN naming all three keys.
#[test]
#[serial_test::serial]
fn a_hot_reload_carrying_three_retired_keys_keeps_the_running_router() {
    let ((), events) = routectl_testkit::block_on_with_capture(hot_reload_three_retired_keys());
    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message == RELOAD_FAILED)
        .collect();
    let expected = driver_message(&with_three_retired_keys());
    assert_eq!(warns.len(), 1, "{events:?}");
    assert_eq!(warns[0].field("reasons"), Some("3"));
    assert_eq!(
        warns[0].field("error"),
        Some(expected.lines().collect::<Vec<_>>().join("; ").as_str())
    );
}

/// Drives the reload; the caller asserts the WARN it logged from the
/// blocking pool.
async fn hot_reload_three_retired_keys() {
    // Arrange
    let body = with_three_retired_keys();
    let expected = driver_message(&body);
    assert_eq!(expected.lines().count(), 3, "premise: {expected}");
    let (_dir, _xdg, path) = write_config(&body);

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = build_usage_writer(&config);
    let router_a = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("build router A");
    let swap = Arc::new(ArcSwap::from_pointee(router_a));
    let published = swap.load_full();

    // Act
    let result = Box::pin(handle_config_reload(
        Some(&path),
        &config,
        secrets,
        &swap,
        &usage,
        &mut never_shutdown(),
    ))
    .await;

    // Assert
    assert_eq!(result.err(), Some(ReloadFailure::ConfigLoadFailed));
    let after = swap.load_full();
    assert!(
        Arc::ptr_eq(&published, &after),
        "router A must stay published"
    );
    assert!(
        Arc::ptr_eq(&after.config, &config),
        "the previous config must stay installed"
    );

    drop(usage);
    drain_usage_writer_strict(writer).await;
}

/// Positive control: the same fixture without a retired key loads on every
/// path, so each refusal above is the retired keys' doing.
#[test]
#[serial_test::serial]
fn a_config_without_a_retired_key_loads_on_every_path() {
    let body = config_body("[capability]\ndecay_hours = 24\n");
    let (_dir, _xdg, path) = write_config(&body);

    if let Err(e) = load_effective_config(&path) {
        panic!("cold start must load the clean fixture: {e}");
    }
    if let Err(e) = load_effective_config_unvalidated(&path) {
        panic!("config check must load the clean fixture: {e}");
    }
    assert!(read_parse_validate_config(&path).is_ok());
}

/// A value written under an old provider key. It must never reach a log.
const VALUE_SENTINEL: &str = "OLD-SHAPE-VALUE-SENTINEL";

/// Every key the flat provider entry retired, at the path the old-shape
/// fixture below writes it.
const OLD_PROVIDER_KEYS: &[&str] = &[
    "providers.fast.kind",
    "providers.fast.api_key_ref",
    "providers.claude.auth_kind",
    "providers.gem.auth_mode",
    "providers.br.api_shape",
    "providers.br.region",
    "providers.br.creds",
    "providers.mantle.bedrock_mantle",
    "providers.fast.credential_source",
    "providers.claude.cloak",
    "providers.claude.cloak_sensitive_words",
    "providers.claude.cloak_tool_rename",
];

/// The new-shape file the running router serves from: one openai-chat entry
/// dialing `origin`.
fn new_shape_config(origin: &str, credential: &str) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [retry]\nmax_attempts = 1\n\
         [providers.fast]\n\
         format = \"openai-chat\"\n\
         base_url = \"{origin}\"\n\
         credential = \"{credential}\"\n\
         auth = \"bearer\"\n\
         [models.gpt]\n\
         provider = \"fast\"\n\
         upstream = \"gpt-4o\"\n\
         [aliases]\n\
         default = \"gpt\"\n"
    )
}

/// The same deployment written in the old provider shape, carrying every
/// retired provider key, each holding the value sentinel where it takes a
/// string.
fn old_shape_config(origin: &str) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.fast]\n\
         kind = \"openai-compat\"\n\
         base_url = \"{origin}\"\n\
         api_key_ref = \"env://{VALUE_SENTINEL}\"\n\
         credential_source = \"own\"\n\
         [providers.claude]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic#{VALUE_SENTINEL}\"\n\
         cloak_sensitive_words = [\"{VALUE_SENTINEL}\"]\n\
         cloak_tool_rename = true\n\
         [providers.claude.cloak]\n\
         mode = \"always\"\n\
         [providers.gem]\n\
         kind = \"gemini\"\n\
         auth_mode = \"api-key\"\n\
         api_key_ref = \"env://{VALUE_SENTINEL}\"\n\
         [providers.br]\n\
         kind = \"bedrock\"\n\
         api_shape = \"converse\"\n\
         region = \"{VALUE_SENTINEL}\"\n\
         creds = {{ profile = \"{VALUE_SENTINEL}\" }}\n\
         [providers.mantle]\n\
         kind = \"anthropic-api\"\n\
         [providers.mantle.bedrock_mantle]\n\
         region = \"{VALUE_SENTINEL}\"\n\
         [models.gpt]\n\
         provider = \"fast\"\n\
         upstream = \"gpt-4o\"\n\
         [aliases]\n\
         default = \"gpt\"\n"
    )
}

fn chat_request() -> ChatRequest {
    ChatRequest {
        model: "gpt".into(),
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
            extras: serde_json::Map::new(),
        }]
        .into(),
        ..Default::default()
    }
}

async fn mount_chat_completion(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
            "model": "gpt-4o",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        })))
        .mount(upstream)
        .await;
}

/// What the reload boundary observed: the old-shape text the reload read,
/// whether each request before and after it was served, and the upstream's
/// request count.
struct OldShapeReload {
    old_text: String,
    served_before: bool,
    served_after: bool,
    upstream_hits: usize,
}

/// A router started on the new shape serves a request; the file is then
/// replaced with the old shape and reloaded; the same published router
/// serves a second request.
async fn reload_an_old_shape_file_over_a_serving_router() -> OldShapeReload {
    // Arrange: a running router built from the new-shape file.
    let upstream = MockServer::start().await;
    mount_chat_completion(&upstream).await;
    let credential = routectl_testkit::secret_file_ref("upstream-key");
    let new_text = new_shape_config(&upstream.uri(), &credential);
    let (_dir, _xdg, path) = write_config(&new_text);
    let loaded = load_effective_config(&path).expect("the new-shape file loads");
    let mut config = loaded.config;
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = build_usage_writer(&config);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let router = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("build the running router");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let published = swap.load_full();
    let served_before = swap
        .load()
        .complete_with_options(chat_request(), RouterOptions::new())
        .await
        .result
        .is_ok();

    // Act: the operator saves an old-shape file and the daemon reloads it.
    let old_text = old_shape_config(&upstream.uri());
    std::fs::write(&path, &old_text).expect("write the old-shape file");
    let result = Box::pin(handle_config_reload(
        Some(&path),
        &config,
        secrets,
        &swap,
        &usage,
        &mut never_shutdown(),
    ))
    .await;
    let served_after = swap
        .load()
        .complete_with_options(chat_request(), RouterOptions::new())
        .await;

    // Assert the boundary itself; the caller asserts the WARN.
    assert_eq!(result.err(), Some(ReloadFailure::ConfigLoadFailed));
    assert!(
        Arc::ptr_eq(&published, &swap.load_full()),
        "the running router must stay published"
    );
    assert_eq!(served_after.meta.served_provider.as_deref(), Some("fast"));
    let upstream_hits = upstream
        .received_requests()
        .await
        .map_or(0, |requests| requests.len());

    drop(usage);
    drain_usage_writer_strict(writer).await;
    OldShapeReload {
        old_text,
        served_before,
        served_after: served_after.result.is_ok(),
        upstream_hits,
    }
}

fn reload_failure_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message == RELOAD_FAILED)
        .collect()
}

/// Every rendered field of every captured event, for the no-echo check.
fn rendered(events: &[CapturedEvent]) -> String {
    events
        .iter()
        .flat_map(|e| {
            std::iter::once(e.message.clone())
                .chain(e.fields.iter().map(|(k, v)| format!("{k}={v}")))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
#[serial_test::serial]
fn reloading_an_old_shape_file_warns_once_naming_every_key_and_keeps_serving() {
    // Act
    let (reload, events) =
        routectl_testkit::block_on_with_capture(reload_an_old_shape_file_over_a_serving_router());

    // Assert: the prior router served on both sides of the failed reload.
    assert!(reload.served_before, "the router serves before the reload");
    assert!(
        reload.served_after,
        "the prior router still serves after it"
    );
    assert_eq!(
        reload.upstream_hits, 2,
        "both requests reached the upstream"
    );

    // Assert: exactly one WARN, carrying the driver's line for every key.
    let warns = reload_failure_warns(&events);
    assert_eq!(warns.len(), 1, "{events:#?}");
    let expected = driver_message(&reload.old_text);
    let error = warns[0].field("error").expect("the WARN carries `error`");
    assert_eq!(error, expected.lines().collect::<Vec<_>>().join("; "));
    for key in OLD_PROVIDER_KEYS {
        let line = routectl_router::retired_key_message(key)
            .unwrap_or_else(|| panic!("premise: `{key}` has a retired row"));
        assert!(
            line.contains(" is retired: ") && !line.ends_with("is retired: ; delete it"),
            "premise: the line names a replacement: {line}"
        );
        assert!(
            error.contains(&line),
            "`{key}` is named with its replacement: {error}"
        );
    }
    assert_eq!(
        warns[0].field("reasons"),
        Some(expected.lines().count().to_string().as_str()),
        "one reason per retired key occurrence"
    );

    // Assert: no value written under an old key is echoed anywhere.
    assert!(
        reload.old_text.contains(VALUE_SENTINEL),
        "premise: the old-shape file carries the sentinel"
    );
    let rendered = rendered(&events);
    assert!(
        rendered.contains("api_key_ref` is retired: "),
        "positive control: the rendered capture holds the WARN's error text"
    );
    assert!(
        !rendered.contains(VALUE_SENTINEL),
        "no log line echoes a value from the old-shape file"
    );
}
