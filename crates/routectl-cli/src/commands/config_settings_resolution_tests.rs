//! Settings rows whose value the runtime takes from another entry, and the
//! environment-only knobs the block leaves out.

use routectl_core::identity::codex::PINNED_CODEX_VERSION;
use routectl_router::Config;
use routectl_testkit::ScopedEnv;

use super::redacted_settings_lines;

fn lines_for(raw_text: &str) -> Vec<String> {
    let config: Config = toml::from_str(raw_text).expect("fixture config parses");
    redacted_settings_lines(&config, raw_text).expect("settings render")
}

fn line_for<'a>(lines: &'a [String], path: &str) -> &'a str {
    let prefix = format!("  {path}=");
    lines
        .iter()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no `{path}` line; lines:\n{lines:#?}"))
}

#[test]
#[serial_test::serial]
fn the_settings_block_prints_no_log_knob_row_whatever_the_environment_sets() {
    let _headers = ScopedEnv::set("ROUTECTL_TRACE_HEADERS", "1");
    let _redact = ScopedEnv::set("ROUTECTL_LOG_REDACT_PROMPTS", "1");
    let _cap = ScopedEnv::set("ROUTECTL_TRACE_BODY_BYTES", "4096");
    let raw = "[retry]\nmax_attempts = 5\n";

    let lines = lines_for(raw);

    assert!(
        lines.iter().any(|l| l == "  retry.max_attempts=5(file)"),
        "control: the block renders; lines:\n{lines:#?}"
    );
    let log_rows: Vec<&String> = lines
        .iter()
        .filter(|l| l.trim_start().starts_with("log.") || l.contains("(env: "))
        .collect();
    assert!(log_rows.is_empty(), "log knob rows: {log_rows:#?}");
}

#[test]
fn an_unset_codex_version_names_the_entry_whose_value_it_runs_under() {
    let entry = |name: &str, version: Option<&str>| {
        let pin = version.map_or_else(String::new, |v| format!("codex_version = \"{v}\"\n"));
        format!("[providers.{name}]\nkind = \"openai-responses\"\napi_key_ref = \"env://K\"\n{pin}")
    };
    let rows = [
        (
            "another entry sets it",
            [entry("alpha", Some("9.9.9")), entry("beta", None)],
            "\"9.9.9\"(from providers.alpha.codex_version)".to_string(),
        ),
        (
            "no entry sets it",
            [entry("alpha", None), entry("beta", None)],
            format!("\"{PINNED_CODEX_VERSION}\"(default)"),
        ),
        (
            "the entry sets its own",
            [entry("alpha", Some("9.9.9")), entry("beta", Some("9.9.9"))],
            "\"9.9.9\"(file)".to_string(),
        ),
    ];
    for (row, entries, expected) in rows {
        let raw = entries.concat();

        let lines = lines_for(&raw);

        assert_eq!(
            line_for(&lines, "providers.beta.codex_version"),
            format!("  providers.beta.codex_version={expected}"),
            "row `{row}`"
        );
    }
}
