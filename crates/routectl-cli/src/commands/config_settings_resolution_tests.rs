//! Settings rows whose value the runtime takes from somewhere other than
//! the file or a code default: the environment, or another entry.

use routectl_core::identity::codex::PINNED_CODEX_VERSION;
use routectl_core::{DEFAULT_REDACT_PROMPTS, DEFAULT_TRACE_HEADERS};
use routectl_router::Config;
use routectl_testkit::ScopedEnv;

use super::{redacted_settings_lines, redacted_settings_lines_with_env};

fn lines_with_env(raw_text: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let config: Config = toml::from_str(raw_text).expect("fixture config parses");
    redacted_settings_lines_with_env(&config, raw_text, env).expect("settings render")
}

fn line_for<'a>(lines: &'a [String], path: &str) -> &'a str {
    let prefix = format!("  {path}=");
    lines
        .iter()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no `{path}` line; lines:\n{lines:#?}"))
}

#[test]
fn a_log_knob_row_names_the_layer_the_runtime_resolves_it_from() {
    let knobs = [
        (
            "trace_headers",
            "ROUTECTL_TRACE_HEADERS",
            DEFAULT_TRACE_HEADERS,
        ),
        (
            "redact_prompts",
            "ROUTECTL_LOG_REDACT_PROMPTS",
            DEFAULT_REDACT_PROMPTS,
        ),
    ];
    for (key, var, default) in knobs {
        let rows: [(&str, Option<&str>, Option<bool>, String); 5] = [
            ("env truthy", Some("1"), None, format!("true(env: {var})")),
            (
                "env truthy beats file",
                Some("yes"),
                Some(false),
                format!("true(env: {var})"),
            ),
            (
                "env falsy beats file",
                Some("0"),
                Some(true),
                format!("false(env: {var})"),
            ),
            (
                "file when env unset",
                None,
                Some(true),
                "true(file)".to_string(),
            ),
            ("neither", None, None, format!("{default}(default)")),
        ];
        for (row, env_value, file_value, expected) in rows {
            let raw = file_value.map_or_else(String::new, |v| format!("[log]\n{key} = {v}\n"));
            let env = |asked: &str| env_value.filter(|_| asked == var).map(str::to_string);

            let lines = lines_with_env(&raw, &env);

            let path = format!("log.{key}");
            assert_eq!(
                line_for(&lines, &path),
                format!("  {path}={expected}"),
                "{key}: row `{row}`"
            );
        }
    }
}

#[test]
#[serial_test::serial]
fn the_settings_block_reads_the_log_knob_env_of_this_process() {
    let _headers = ScopedEnv::set("ROUTECTL_TRACE_HEADERS", "1");
    let _redact = ScopedEnv::set("ROUTECTL_LOG_REDACT_PROMPTS", "off");
    let _cap = ScopedEnv::unset("ROUTECTL_TRACE_BODY_BYTES");
    let raw = "[log]\ntrace_headers = false\nredact_prompts = true\n";
    let config: Config = toml::from_str(raw).expect("fixture config parses");

    let lines = redacted_settings_lines(&config, raw).expect("settings render");

    for expected in [
        "  log.trace_headers=true(env: ROUTECTL_TRACE_HEADERS)",
        "  log.redact_prompts=false(env: ROUTECTL_LOG_REDACT_PROMPTS)",
        "  log.trace_body_bytes=16384(default)",
    ] {
        assert!(
            lines.iter().any(|l| l == expected),
            "`{expected}` missing; lines:\n{lines:#?}"
        );
    }
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

        let lines = lines_with_env(&raw, &|_| None);

        assert_eq!(
            line_for(&lines, "providers.beta.codex_version"),
            format!("  providers.beta.codex_version={expected}"),
            "row `{row}`"
        );
    }
}
