//! Provider entry names reach every doctor line through the operator-name
//! renderer: the probe and secret findings, the validator lines, and the
//! old-folder listing.

use routectl_core::MAX_PROVIDER_NAME_BYTES;

use crate::config_folder::LEGACY_LIBRARY_DISPLAY;

use super::gather::gather_secret_checks;
use super::render::render_human;
use super::tests::ctx;
use super::*;

/// The human doctor render for a config holding one provider written under
/// the TOML key `key`, probed reachable, with `key` also listed in the old
/// config folder.
fn rendered(key: &str) -> Vec<String> {
    let text = format!(
        "version = {}\n[providers.\"{key}\"]\nformat = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\ncredential = \"env://ROUTECTL_DOCTOR_NAME_UNSET\"\n\
         auth = \"x-api-key\"\n",
        routectl_router::CURRENT_CONFIG_VERSION
    );
    let config: Config =
        toml::from_str(&text).unwrap_or_else(|e| panic!("fixture parses: {e}\n{text}"));
    let name = config
        .providers
        .keys()
        .next()
        .expect("one provider")
        .clone();
    let secret_checks = gather_secret_checks(&config, &[]);
    let context = DoctorContext {
        secret_checks,
        probe_results: vec![(name.clone(), ProbeOutcome::Reachable)],
        legacy_folders: vec![LegacyFolder::Entries {
            display: LEGACY_LIBRARY_DISPLAY,
            names: vec![name],
            more: 0,
            has_config: false,
        }],
        ..ctx(config, Some(&text), Vec::new(), Vec::new())
    };
    render_human(&build_report(&context))
}

/// The lines naming the provider: the probe and secret findings and the
/// old-folder line.
fn naming_lines<'a>(lines: &'a [String], shown: &str) -> Vec<&'a String> {
    lines.iter().filter(|line| line.contains(shown)).collect()
}

#[test]
fn a_name_over_the_byte_ceiling_appears_on_no_doctor_line() {
    let over = "p".repeat(MAX_PROVIDER_NAME_BYTES + 1);

    let lines = rendered(&over);

    let placeholder = format!("<{}-byte name withheld>", over.len());
    let named = naming_lines(&lines, &placeholder);
    for marker in ["reachable", "env:// reference", "still holds"] {
        assert!(
            named.iter().any(|l| l.contains(marker)),
            "`{marker}` line shows the placeholder: {lines:#?}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|l| l.contains("a provider name is 129 bytes")),
        "positive control: the validator refuses the name: {lines:#?}"
    );
    for line in &lines {
        assert!(!line.contains(&over), "{line}");
    }
}

#[test]
fn a_name_at_the_byte_ceiling_is_shown_on_its_doctor_lines() {
    let at = "p".repeat(MAX_PROVIDER_NAME_BYTES);

    let lines = rendered(&at);

    let named = naming_lines(&lines, &at);
    for marker in ["reachable", "env:// reference", "still holds"] {
        assert!(
            named.iter().any(|l| l.contains(marker)),
            "`{marker}` line names the provider: {lines:#?}"
        );
    }
}

#[test]
fn a_name_with_esc_and_newline_renders_stripped_on_each_doctor_line() {
    let lines = rendered("bad\\u001b[31m\\nname");

    let named = naming_lines(&lines, "badname");
    for marker in ["reachable", "env:// reference", "still holds"] {
        assert!(
            named.iter().any(|l| l.contains(marker)),
            "`{marker}` line names the stripped provider: {lines:#?}"
        );
    }
    for line in &lines {
        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert!(!line.contains("[31m"), "{line:?}");
    }
}
