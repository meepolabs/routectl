//! A declined hot reload logs its loader error as one line: a log shipper
//! splits a field carrying a newline into separate records.

use routectl_router::CURRENT_CONFIG_VERSION;
use routectl_testkit::{CapturedEvent, ScopedEnv};

use super::*;

const RELOAD_FAILED: &str = "config reload failed; keeping previous config";

fn config_with_model_keys(model_keys: &str) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.fast]\n\
         kind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         api_key_ref = \"env://ROUTECTL_TEST_UNSET_KEY\"\n\
         [models.gpt]\n\
         provider = \"fast\"\n\
         upstream = \"gpt-4o\"\n\
         {model_keys}\
         [aliases]\n\
         default = \"gpt\"\n"
    )
}

/// The reload-failure WARN `read_parse_validate_config` emits for `body`.
fn reload_failure_warn(body: &str) -> CapturedEvent {
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let path = dir.path().join("config.toml");
    std::fs::write(&path, body).expect("write config.toml");

    let mut loaded = None;
    let events = routectl_testkit::capture_events(|| {
        loaded = Some(read_parse_validate_config(&path));
    });

    assert!(
        loaded.expect("the loader ran").is_err(),
        "the fixture must be declined"
    );
    events
        .into_iter()
        .find(|e| e.level == tracing::Level::WARN && e.message == RELOAD_FAILED)
        .expect("a declined reload logs its failure")
}

#[test]
#[serial_test::serial]
fn a_reload_declined_for_two_retired_knobs_logs_one_line_with_a_reason_count() {
    // Arrange: the loader names each retired knob on its own line.
    let body = config_with_model_keys("effort_levels = [\"low\"]\nmax_thinking_budget = 1024\n");
    let loader_error = routectl_router::preflight_raw_config(&body)
        .expect_err("premise: the preflight refuses both knobs")
        .to_string();
    assert_eq!(
        loader_error.lines().count(),
        2,
        "premise: the loader error is multi-line: {loader_error}"
    );

    // Act
    let warn = reload_failure_warn(&body);

    // Assert
    let error = warn.field("error").expect("the WARN carries `error`");
    assert!(!error.contains('\n'), "error must be one line: {error:?}");
    let joined = loader_error.lines().collect::<Vec<_>>().join("; ");
    assert_eq!(error, joined, "both reasons joined with \"; \"");
    assert_eq!(warn.field("reasons"), Some("2"));
}

#[test]
#[serial_test::serial]
fn a_reload_declined_for_one_reason_counts_one() {
    let body = config_with_model_keys("effort_levels = [\"low\"]\n");
    let loader_error = routectl_router::preflight_raw_config(&body)
        .expect_err("premise: the preflight refuses the knob")
        .to_string();

    let warn = reload_failure_warn(&body);

    assert_eq!(warn.field("error"), Some(loader_error.as_str()));
    assert_eq!(warn.field("reasons"), Some("1"));
}

#[test]
#[serial_test::serial]
fn cold_start_keeps_the_multi_line_error() {
    let body = config_with_model_keys("effort_levels = [\"low\"]\nmax_thinking_budget = 1024\n");
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let path = dir.path().join("config.toml");
    std::fs::write(&path, &body).expect("write config.toml");

    let err = match load_effective_config(&path) {
        Ok(_) => panic!("the fixture must not load"),
        Err(e) => e,
    };

    assert_eq!(err.lines().count(), 2, "{err}");
}
