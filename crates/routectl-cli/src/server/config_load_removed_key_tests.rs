use routectl_router::CURRENT_CONFIG_VERSION;
use routectl_testkit::ScopedEnv;

use super::*;
use crate::server::reload_failure::ReloadFailure;

const REMOVED_KEY_MESSAGE: &str =
    "config carries `[fidelity] paid_probe_daily_caps`: paid probes were removed; delete this key";

fn config_body(fidelity: &str) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.fast]\n\
         kind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         api_key_ref = \"env://ROUTECTL_TEST_UNSET_KEY\"\n\
         [models.gpt]\n\
         provider = \"fast\"\n\
         upstream = \"gpt-4o\"\n\
         [aliases]\n\
         default = \"gpt\"\n\
         {fidelity}"
    )
}

fn with_removed_key() -> String {
    config_body("[fidelity.paid_probe_daily_caps]\nfast = 5\n")
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

fn assert_names_removed_key(err: &str) {
    assert!(err.contains(REMOVED_KEY_MESSAGE), "err: {err}");
    assert!(
        !err.contains("unknown field"),
        "the key is refused by name, not as an unknown field: {err}"
    );
}

#[test]
#[serial_test::serial]
fn cold_start_refuses_the_removed_paid_probe_caps_key() {
    let (_dir, _xdg, path) = write_config(&with_removed_key());

    let err = match load_effective_config(&path) {
        Ok(_) => panic!("a config carrying the removed key must not load"),
        Err(e) => e,
    };

    assert_names_removed_key(&err);
}

#[test]
#[serial_test::serial]
fn config_check_refuses_the_removed_paid_probe_caps_key() {
    let (_dir, _xdg, path) = write_config(&with_removed_key());

    let err = match load_effective_config_unvalidated(&path) {
        Ok(_) => panic!("config check must refuse the removed key"),
        Err(e) => e,
    };

    assert_names_removed_key(&err);
}

#[test]
#[serial_test::serial]
fn a_hot_reload_carrying_the_removed_paid_probe_caps_key_is_declined() {
    let (_dir, _xdg, path) = write_config(&with_removed_key());

    let mut loaded = None;
    let events = routectl_testkit::capture_events(|| {
        loaded = Some(read_parse_validate_config(&path));
    });

    assert!(
        matches!(
            loaded.expect("the loader ran"),
            Err(ReloadFailure::ConfigLoadFailed)
        ),
        "a reload carrying the removed key must be declined so the running router stays live"
    );
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN
            && e.field("error")
                .is_some_and(|v| v.contains(REMOVED_KEY_MESSAGE))),
        "the declined reload must log the named refusal"
    );
}

/// Positive control: the same fixture with a `[fidelity]` block that omits
/// the removed key loads, so each refusal above is that key's doing.
#[test]
#[serial_test::serial]
fn a_config_without_the_removed_key_loads_on_every_path() {
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
