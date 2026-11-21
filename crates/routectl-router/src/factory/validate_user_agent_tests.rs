//! An entry `user_agent` the HTTP client builder cannot install is refused
//! by the shared validation suite and by the build plan, naming the entry
//! and the key and never the value.

use super::collect_config_validation;
use crate::config::Config;
use crate::factory::plan::plan_provider;

/// A distinctive token carried by every rejected value, so an echo of the
/// value in a message is detectable.
const MARKER: &str = "zq-echo-marker";

fn config_with_user_agent(user_agent: &str) -> Config {
    let mut config: Config = toml::from_str(
        "[providers.edge]\nformat = \"openai-chat\"\nbase_url = \"https://example.com/v1\"\n\
         credential = \"env://ROUTECTL_UA_TEST_KEY\"\nauth = \"bearer\"\n",
    )
    .expect("fixture parses");
    config
        .providers
        .get_mut("edge")
        .expect("provider present")
        .user_agent = Some(user_agent.to_string());
    config
}

fn user_agent_errors(config: &Config) -> Vec<String> {
    collect_config_validation(config)
        .errors
        .into_iter()
        .filter(|e| e.contains("user_agent"))
        .collect()
}

const REJECT_ROWS: &[(&str, &str)] = &[
    ("CR", "probe/1.0\rzq-echo-marker"),
    ("LF", "probe/1.0\nzq-echo-marker"),
    (
        "CRLF header injection",
        "probe/1.0\r\nX-Injected: zq-echo-marker",
    ),
    ("NUL", "probe/1.0\0zq-echo-marker"),
    ("DEL", "probe/1.0\u{7f}zq-echo-marker"),
    ("SOH", "probe/1.0\u{1}zq-echo-marker"),
    ("ESC", "probe/1.0\u{1b}[31mzq-echo-marker"),
    ("unit separator", "probe/1.0\u{1f}zq-echo-marker"),
];

/// Values the client builder installs: realistic CLI shapes, plus the
/// edges its header-value parse accepts (tab, obs-text, empty).
const ACCEPT_ROWS: &[(&str, &str)] = &[
    ("claude cli", "claude-cli/2.1.3 (external, cli)"),
    ("codex cli", "codex_cli_rs/0.5.0 (Linux; x86_64) xterm"),
    ("plain product token", "routectl/0.9.0"),
    ("tab", "routectl/0.9.0\t(test)"),
    ("non-ascii obs-text", "routectl/0.9.0 (Jos\u{e9})"),
    ("empty", ""),
];

#[test]
fn a_user_agent_with_a_non_header_byte_is_refused_naming_the_entry_not_the_value() {
    for (label, user_agent) in REJECT_ROWS {
        assert!(user_agent.contains(MARKER), "{label}: fixture lacks marker");

        let errors = user_agent_errors(&config_with_user_agent(user_agent));

        assert_eq!(errors.len(), 1, "{label}: {errors:?}");
        let message = &errors[0];
        assert!(
            message.starts_with("provider `edge`: user_agent "),
            "{label}: {message}"
        );
        assert!(
            !message.contains(MARKER),
            "{label}: value echoed: {message}"
        );
        assert!(
            !message.chars().any(char::is_control),
            "{label}: {message:?}"
        );
    }
}

#[test]
fn a_user_agent_the_client_builder_installs_is_accepted() {
    for (label, user_agent) in ACCEPT_ROWS {
        let config = config_with_user_agent(user_agent);

        assert_eq!(user_agent_errors(&config), Vec::<String>::new(), "{label}");
        assert!(
            collect_config_validation(&config).errors.is_empty(),
            "{label}"
        );
    }
}

#[test]
fn the_build_plan_refuses_a_user_agent_with_a_non_header_byte() {
    for (label, user_agent) in REJECT_ROWS {
        let config = config_with_user_agent(user_agent);
        let entry = config.providers.get("edge").expect("provider present");

        let message = plan_provider("edge", entry).expect_err(label).to_string();

        assert!(
            message.contains("provider `edge`: user_agent "),
            "{label}: {message}"
        );
        assert!(
            !message.contains(MARKER),
            "{label}: value echoed: {message}"
        );
    }
    for (label, user_agent) in ACCEPT_ROWS {
        let config = config_with_user_agent(user_agent);
        let entry = config.providers.get("edge").expect("provider present");

        assert!(plan_provider("edge", entry).is_ok(), "{label}");
    }
}
