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
         format = \"openai-chat\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         credential = \"env://ROUTECTL_TEST_UNSET_KEY\"\n\
         auth = \"bearer\"\n\
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

/// Two providers whose names carry the seat separator: the shared validator
/// suite reports two findings for one config.
fn two_validator_findings_config() -> String {
    let provider = |name: &str| {
        format!(
            "[providers.\"{name}\"]\nformat = \"anthropic-messages\"\n\
             base_url = \"https://api.anthropic.com\"\ncredential = \"env://K\"\n\
             auth = \"x-api-key\"\n"
        )
    };
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n{}{}",
        provider("x#1"),
        provider("y#2")
    )
}

#[test]
#[serial_test::serial]
fn a_reload_declined_for_two_validator_findings_names_both() {
    let warn = reload_failure_warn(&two_validator_findings_config());

    let error = warn.field("error").expect("the WARN carries `error`");
    assert!(!error.contains('\n'), "error must be one line: {error:?}");
    assert!(error.contains("`x#1`"), "{error}");
    assert!(error.contains("`y#2`"), "{error}");
    assert_eq!(warn.field("reasons"), Some("2"));
}

#[test]
#[serial_test::serial]
fn cold_start_reports_every_validator_finding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let path = dir.path().join("config.toml");
    std::fs::write(&path, two_validator_findings_config()).expect("write config.toml");

    let err = match load_effective_config(&path) {
        Ok(_) => panic!("the fixture must not load"),
        Err(e) => e,
    };

    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 2, "{err}");
    assert!(lines[0].starts_with("config: provider name `x#1`"), "{err}");
    assert!(lines[1].starts_with("config: provider name `y#2`"), "{err}");
}

/// A provider entry under the TOML key `key` whose `base_url` scheme the
/// shape rule refuses, so the refusal quotes the provider name.
fn refused_provider_config(key: &str) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n[providers.\"{key}\"]\n\
         format = \"anthropic-messages\"\nbase_url = \"ftp://api.anthropic.com\"\n\
         credential = \"env://K\"\nauth = \"x-api-key\"\n"
    )
}

#[test]
#[serial_test::serial]
fn a_reload_warn_withholds_a_name_over_the_byte_ceiling_and_shows_one_at_it() {
    let over = "p".repeat(routectl_core::MAX_PROVIDER_NAME_BYTES + 1);
    let at = "q".repeat(routectl_core::MAX_PROVIDER_NAME_BYTES);

    let over_warn = reload_failure_warn(&refused_provider_config(&over));
    let at_warn = reload_failure_warn(&refused_provider_config(&at));

    let over_error = over_warn.field("error").expect("the WARN carries `error`");
    assert!(!over_error.contains(&over), "{over_error}");
    assert!(
        over_error.contains(&format!("<{}-byte name withheld>", over.len())),
        "{over_error}"
    );
    let at_error = at_warn.field("error").expect("the WARN carries `error`");
    assert!(at_error.contains(&format!("provider `{at}`")), "{at_error}");
}

#[test]
#[serial_test::serial]
fn a_reload_warn_strips_esc_and_newline_from_a_provider_name() {
    let warn = reload_failure_warn(&refused_provider_config("bad\\u001b[31m\\nname"));

    let error = warn.field("error").expect("the WARN carries `error`");
    assert!(error.contains("provider `badname`"), "{error}");
    assert!(!error.chars().any(char::is_control), "{error:?}");
}
