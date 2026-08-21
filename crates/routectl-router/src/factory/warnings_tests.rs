//! Unit tests for the non-fatal config warning collectors in
//! `factory/warnings.rs`.

#![cfg(feature = "gemini")]

use super::cloudcode_host_warnings;
use crate::config::Config;

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("fixture must parse")
}

/// A `cloud-code` Gemini entry, optionally pinning `base_url`. `None`
/// leaves the key unset, which is the daily-default shape.
fn cloud_code_config(base_url: Option<&str>) -> Config {
    let pin = base_url.map_or_else(String::new, |b| format!("base_url = \"{b}\"\n"));
    parse(&format!(
        "version = 2\n\
         [providers.g]\n\
         kind = \"gemini\"\n\
         api_key_ref = \"oauth://antigravity\"\n\
         auth_mode = \"cloud-code\"\n\
         {pin}"
    ))
}

#[test]
fn warns_when_a_cloud_code_entry_pins_the_production_host() {
    // Arrange
    let config = cloud_code_config(Some(routectl_providers::gemini::PROD_BASE_URL));

    // Act
    let warnings = cloudcode_host_warnings(&config);

    // Assert
    assert_eq!(
        warnings.len(),
        1,
        "an explicit production pin must warn exactly once: {warnings:?}"
    );
    assert!(
        warnings[0].contains("[providers.g]"),
        "the warning must name the provider key: {}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("base_url"),
        "the warning must name the recovery knob: {}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("daily"),
        "the warning must name the daily lane default: {}",
        warnings[0]
    );
}

#[test]
fn warns_on_a_production_pin_carrying_a_path_port_or_credentials() {
    // POSITIVE CONTROL for the negative cases below: the predicate matches
    // the parsed host, so these three real production pins must all fire --
    // otherwise the lookalike test below would pass vacuously.
    for pin in [
        "https://cloudcode-pa.googleapis.com/v1internal",
        "https://cloudcode-pa.googleapis.com:443",
        "https://CloudCode-PA.GoogleAPIs.Com",
        "https://user:pass@cloudcode-pa.googleapis.com",
    ] {
        let warnings = cloudcode_host_warnings(&cloud_code_config(Some(pin)));
        assert_eq!(
            warnings.len(),
            1,
            "pin {pin} egresses to the production host and must warn: {warnings:?}"
        );
    }
}

#[test]
fn silent_for_a_lookalike_host() {
    // A sibling-domain suffix, the host inside a path, and a
    // credentials-suffix smuggle all egress somewhere other than the
    // production host: a substring test would false-positive on each.
    for pin in [
        "https://cloudcode-pa.googleapis.com.example.test",
        "https://proxy.example.test/cloudcode-pa.googleapis.com",
        "https://cloudcode-pa.googleapis.com@other.example.test",
    ] {
        let warnings = cloudcode_host_warnings(&cloud_code_config(Some(pin)));
        assert!(
            warnings.is_empty(),
            "pin {pin} is not the production host and must stay silent: {warnings:?}"
        );
    }
}

#[test]
fn silent_for_an_unset_base_url_and_an_explicit_daily_pin() {
    assert!(
        cloudcode_host_warnings(&cloud_code_config(None)).is_empty(),
        "an unset base_url takes the daily lane default"
    );
    assert!(
        cloudcode_host_warnings(&cloud_code_config(Some(
            routectl_providers::gemini::DAILY_BASE_URL
        )))
        .is_empty(),
        "an explicit daily pin is the default host, not a divergence"
    );
}

#[test]
fn silent_for_an_api_key_gemini_entry_even_pinned_at_the_production_host() {
    // The Cloud Code hosts are not on the api-key lane at all, so a pin
    // there says nothing about cloud-code lane serving.
    let config = parse(&format!(
        "version = 2\n\
         [providers.g]\n\
         kind = \"gemini\"\n\
         api_key_ref = \"env://GEMINI_API_KEY\"\n\
         base_url = \"{}\"\n",
        routectl_providers::gemini::PROD_BASE_URL
    ));
    assert!(
        cloudcode_host_warnings(&config).is_empty(),
        "an api-key entry must never produce a cloud-code host warning"
    );
}

#[test]
fn collect_config_validation_surfaces_the_pin_as_a_warning_not_an_error() {
    // Arrange
    let config = cloud_code_config(Some(routectl_providers::gemini::PROD_BASE_URL));

    // Act
    let validation = super::super::validate::collect_config_validation(&config);

    // Assert
    assert!(
        validation
            .warnings
            .iter()
            .any(|w| w.contains("[providers.g]") && w.contains("production cloud-code host")),
        "the host pin line must reach .warnings: {:?}",
        validation.warnings
    );
    assert!(
        validation.errors.is_empty(),
        "a production pin is advisory and must never fail the load: {:?}",
        validation.errors
    );
}
