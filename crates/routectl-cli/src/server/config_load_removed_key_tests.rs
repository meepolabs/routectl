use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{CURRENT_CONFIG_VERSION, Config};
use routectl_testkit::ScopedEnv;

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
         kind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         api_key_ref = \"env://ROUTECTL_TEST_UNSET_KEY\"\n\
         [models.gpt]\n\
         provider = \"fast\"\n\
         upstream = \"gpt-4o\"\n\
         [aliases]\n\
         default = \"gpt\"\n\
         {extra}"
    )
}

fn with_one_retired_key() -> String {
    config_body("[fidelity.paid_probe_daily_caps]\nfast = 5\n")
}

/// Three retired keys from three different rows.
fn with_three_retired_keys() -> String {
    config_body(
        "[mitm]\ncredential_source = \"forwarded\"\n\
         [fidelity.paid_probe_daily_caps]\nfast = 5\n\
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
/// from. The loader runs on `spawn_blocking` inside the reload, where the
/// thread-local capture cannot see it, so its one WARN is captured by
/// driving the same loader synchronously.
#[tokio::test]
#[serial_test::serial]
async fn a_hot_reload_carrying_three_retired_keys_keeps_the_running_router() {
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
    let mut loaded = None;
    let events = routectl_testkit::capture_events(|| {
        loaded = Some(read_parse_validate_config(&path));
    });

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
    assert!(matches!(
        loaded.expect("the loader ran"),
        Err(ReloadFailure::ConfigLoadFailed)
    ));
    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message == RELOAD_FAILED)
        .collect();
    assert_eq!(warns.len(), 1, "{events:?}");
    assert_eq!(warns[0].field("reasons"), Some("3"));
    assert_eq!(
        warns[0].field("error"),
        Some(expected.lines().collect::<Vec<_>>().join("; ").as_str())
    );

    drop(usage);
    drain_usage_writer_strict(writer).await;
}

/// Positive control: the same fixture without a retired key loads on every
/// path, so each refusal above is the retired keys' doing.
#[test]
#[serial_test::serial]
fn a_config_without_a_retired_key_loads_on_every_path() {
    let body = config_body("[fidelity]\nprefix_impact_opt_in = [\"fast\"]\n");
    let (_dir, _xdg, path) = write_config(&body);

    if let Err(e) = load_effective_config(&path) {
        panic!("cold start must load the clean fixture: {e}");
    }
    if let Err(e) = load_effective_config_unvalidated(&path) {
        panic!("config check must load the clean fixture: {e}");
    }
    assert!(read_parse_validate_config(&path).is_ok());
}
