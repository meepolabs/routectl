//! Several validator findings reach `config check` and `doctor` as one line
//! each: no finding is folded into another's line, where its newline would
//! render as `?`.

use routectl_router::{CURRENT_CONFIG_VERSION, Status};
use routectl_testkit::ScopedEnv;

use super::{safe_line, validation_report};
use crate::commands::doctor::{GatherSources, build_report_no_network, gather_context_no_network};

fn provider(name: &str) -> String {
    format!(
        "[providers.\"{name}\"]\nformat = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\ncredential = \"env://K\"\n\
         auth = \"x-api-key\"\n"
    )
}

/// Two providers whose names carry the seat separator: two findings of one
/// validator.
fn two_finding_config() -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n{}{}",
        provider("x#1"),
        provider("y#2")
    )
}

fn assert_one_clean_line_per_finding(surface: &str, lines: &[String]) {
    assert_eq!(lines.len(), 2, "{surface}: {lines:?}");
    for line in lines {
        assert!(
            !line.contains('?') && !line.contains('\n'),
            "{surface}: {line:?}"
        );
    }
    assert!(lines[0].contains("`x#1`"), "{surface}: {lines:?}");
    assert!(lines[1].contains("`y#2`"), "{surface}: {lines:?}");
}

#[test]
fn config_check_renders_one_line_per_finding() {
    let text = two_finding_config();
    let config = routectl_router::parse_config(&text).expect("fixture parses");

    let lines: Vec<String> = validation_report(&config, Some(&text))
        .errors
        .iter()
        .map(|e| safe_line(e))
        .collect();

    assert_one_clean_line_per_finding("config check", &lines);
}

#[tokio::test]
#[serial_test::serial]
async fn doctor_renders_one_finding_per_validator_finding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, two_finding_config()).expect("write config.toml");

    let ctx = gather_context_no_network(&config_path, GatherSources::Disk, &[]).await;
    let report = build_report_no_network(&ctx);

    let details: Vec<String> = report
        .findings
        .iter()
        .filter(|f| f.section == "config" && f.name == "validation" && f.status == Status::Fail)
        .map(|f| f.detail.clone())
        .collect();
    assert_one_clean_line_per_finding("doctor", &details);
}
