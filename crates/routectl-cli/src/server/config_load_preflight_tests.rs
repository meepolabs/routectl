//! The loader's raw-config preflight, one row per refusal class.
//!
//! The load sentinel pins `parse_config_only` to the router driver: each row's
//! error must equal `preflight_raw_config`'s own Display for the same text, so
//! dropping the driver call from the loader turns it red.
//!
//! The reload table is a safety test, not attribution: it proves a refused
//! candidate never replaces the published router, whichever layer refuses it.
//! Several rows (the legacy mitm key, the retired keys, the paid probe caps)
//! are still refused without the driver by the typed `deny_unknown_fields`
//! deserialize, so the load sentinel, not this table, pins the driver call.

use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{
    CURRENT_CONFIG_VERSION, Config, ConfigVersionError, RawConfigPreflightError as Refusal,
    preflight_raw_config,
};
use routectl_testkit::ScopedEnv;

use super::*;
use crate::server::build_router_from_config_with_overlay;
use crate::server::reload::{ReloadTrigger, handle_config_reload};
use crate::server::reload_failure::ReloadFailure;
use crate::server::serve::build_usage_writer;
use crate::server::test_support::{drain_usage_writer_strict, isolate_usage_db, never_shutdown};

type ClassMatcher = fn(&Refusal) -> bool;

fn at_version(version: u32, body: &str) -> String {
    format!("version = {version}\n[server]\nhost = \"127.0.0.1\"\nport = 0\n{body}")
}

fn clean_config() -> String {
    at_version(CURRENT_CONFIG_VERSION, "")
}

/// One fixture per raw refusal class, with the driver variant it must map to.
fn refusal_rows() -> [(&'static str, String, ClassMatcher); 7] {
    let current = CURRENT_CONFIG_VERSION;
    [
        (
            "version missing",
            "[server]\nhost = \"127.0.0.1\"\nport = 0\n".to_string(),
            |e| matches!(e, Refusal::Version(ConfigVersionError::Missing { .. })),
        ),
        ("too old", at_version(current - 1, ""), |e| {
            matches!(e, Refusal::Version(ConfigVersionError::TooOld { .. }))
        }),
        ("too new", at_version(current + 1, ""), |e| {
            matches!(e, Refusal::Version(_))
        }),
        (
            "legacy mitm credential_source",
            at_version(current, "[mitm]\ncredential_source = \"forwarded\"\n"),
            |e| matches!(e, Refusal::LegacyMitmCredentialSource(_)),
        ),
        (
            "retired capability key",
            at_version(current, "[bedrock]\n"),
            |e| matches!(e, Refusal::RetiredCapabilityKeys(_)),
        ),
        (
            "paid probe caps",
            at_version(current, "[fidelity.paid_probe_daily_caps]\nfast = 5\n"),
            |e| matches!(e, Refusal::RemovedPaidProbeCaps(_)),
        ),
        (
            "removed model key",
            at_version(current, "[models.m]\neffort_levels = [\"low\"]\n"),
            |e| matches!(e, Refusal::RetiredModelKnobs(_)),
        ),
    ]
}

/// The driver's refusal for `raw`, asserted to be the row's class so a
/// mis-built fixture cannot silently duplicate another row.
fn driver_refusal(row: &str, raw: &str, is_class: ClassMatcher) -> Refusal {
    let refusal = preflight_raw_config(raw)
        .expect_err(&format!("row `{row}`: the driver must refuse the fixture"));
    assert!(is_class(&refusal), "row `{row}`: wrong class {refusal:?}");
    refusal
}

#[test]
#[serial_test::serial]
fn parse_config_only_refuses_each_raw_class_with_the_drivers_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let path = dir.path().join("config.toml");

    for (row, raw, is_class) in refusal_rows() {
        let refusal = driver_refusal(row, &raw, is_class);
        std::fs::write(&path, &raw).expect("write config.toml");

        let err = match parse_config_only(&path) {
            Ok(_) => panic!("row `{row}`: the loader must refuse the fixture"),
            Err(e) => e,
        };

        assert_eq!(err, refusal.to_string(), "row `{row}`");
    }
}

#[test]
#[serial_test::serial]
fn parse_config_only_accepts_the_clean_control() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let path = dir.path().join("config.toml");
    std::fs::write(&path, clean_config()).expect("write config.toml");

    if let Err(e) = parse_config_only(&path) {
        panic!("the clean control must parse: {e}");
    }
}

/// For every raw refusal class, the real reload path declines with
/// `ConfigLoadFailed` and the published router stays the same allocation.
/// A clean candidate run last through the same harness must swap, so the
/// pointer-identity assertions above it can observe a swap when one happens.
///
/// `#[serial]`: the loader re-reads the ambient `catalog_overlay.json` via
/// `routectl_router::overlay_default_path()`, so XDG is pinned per test.
#[tokio::test]
#[serial_test::serial]
async fn a_reload_refused_by_any_raw_class_keeps_the_published_router() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = build_usage_writer(&config);
    let router =
        build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
            .await
            .expect("build the live router");
    let swap = Arc::new(ArcSwap::from_pointee(router));

    for (row, raw, is_class) in refusal_rows() {
        driver_refusal(row, &raw, is_class);
        std::fs::write(&cfg_path, &raw).expect("write config.toml");
        let router_before = swap.load_full();

        let result = Box::pin(handle_config_reload(
            Some(&cfg_path),
            &config,
            secrets.clone(),
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
            &mut never_shutdown(),
        ))
        .await;

        assert_eq!(
            result.err(),
            Some(ReloadFailure::ConfigLoadFailed),
            "row `{row}`: the reload must be declined"
        );
        assert!(
            Arc::ptr_eq(&router_before, &swap.load_full()),
            "row `{row}`: the prior router must stay published"
        );
    }

    std::fs::write(&cfg_path, clean_config()).expect("write config.toml");
    let router_before = swap.load_full();
    let applied = Box::pin(handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
        &mut never_shutdown(),
    ))
    .await;
    assert!(
        matches!(applied, Ok(Some(_))),
        "the clean control must apply"
    );
    assert!(
        !Arc::ptr_eq(&router_before, &swap.load_full()),
        "the clean control must publish a new router"
    );

    drop(usage);
    drain_usage_writer_strict(writer).await;
}
