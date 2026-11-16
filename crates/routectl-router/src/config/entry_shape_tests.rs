//! Entry-shape rules over entries parsed from TOML, the path an operator's
//! hand-edited file takes: which keys each format takes, the plain-URL rule
//! for `base_url`, and one-pass reporting through the validation suite.

use super::shape_refusals;
use crate::config::{Config, ProviderEntry};
use crate::factory::collect_config_validation;

/// One entry per format, each otherwise well formed for the shape rules.
const FORMAT_ENTRIES: [(&str, &str); 7] = [
    (
        "anthropic-messages",
        "base_url = \"https://api.anthropic.com\"\nauth = \"x-api-key\"\n",
    ),
    (
        "openai-chat",
        "base_url = \"https://api.example.test/v1\"\nauth = \"bearer\"\n",
    ),
    (
        "openai-responses",
        "base_url = \"https://chatgpt.com/backend-api/codex\"\nauth = \"bearer\"\n",
    ),
    (
        "bedrock-invoke",
        "cloud = { name = \"aws\", region = \"us-east-1\" }\nauth = \"bearer\"\n",
    ),
    (
        "bedrock-converse",
        "cloud = { name = \"aws\", region = \"us-east-1\" }\nauth = \"bearer\"\n",
    ),
    (
        "gemini",
        "base_url = \"https://generativelanguage.googleapis.com/v1beta\"\n\
         auth = \"x-goog-api-key\"\n",
    ),
    (
        "gemini-cloud-code",
        "base_url = \"https://cloudcode-pa.googleapis.com\"\nauth = \"bearer\"\n",
    ),
];

/// Each format-specific key, the one format that takes it, and the values
/// written for it: an explicit empty or default value first, then an
/// ordinary one. The empty or default value still counts as set.
const FORMAT_KEYS: [(&str, &str, [&str; 2]); 8] = [
    (
        "anthropic_version",
        "anthropic-messages",
        ["\"\"", "\"2023-06-01\""],
    ),
    (
        "forward_client_headers",
        "anthropic-messages",
        ["[]", "[\"x-claude-code-session-id\"]"],
    ),
    (
        "context_management",
        "anthropic-messages",
        ["false", "true"],
    ),
    (
        "cloak_mode",
        "anthropic-messages",
        ["\"auto\"", "\"never\""],
    ),
    ("cloak_strict_mode", "anthropic-messages", ["false", "true"]),
    ("codex_version", "openai-responses", ["\"\"", "\"0.130.0\""]),
    (
        "account_id_ref",
        "openai-responses",
        ["\"\"", "\"env://ACCOUNT\""],
    ),
    (
        "cloud_project_id",
        "gemini-cloud-code",
        ["\"\"", "\"my-project-1234\""],
    ),
];

fn entry_text(format: &str, body: &str, extra: &str) -> String {
    format!("[providers.p]\nformat = \"{format}\"\ncredential = \"env://KEY\"\n{body}{extra}")
}

fn parse_entry(text: &str) -> ProviderEntry {
    let cfg: Config = toml::from_str(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    cfg.providers.into_values().next().expect("one provider")
}

#[test]
fn every_format_entry_is_shape_clean_without_format_keys() {
    for (format, body) in FORMAT_ENTRIES {
        let entry = parse_entry(&entry_text(format, body, ""));

        assert_eq!(
            shape_refusals("p", &entry),
            Vec::<String>::new(),
            "{format}"
        );
    }
}

#[test]
fn a_format_key_is_refused_on_every_format_that_does_not_take_it() {
    let mut refused = 0;
    let mut accepted = 0;
    for (key, takes, values) in FORMAT_KEYS {
        for value in values {
            for (format, body) in FORMAT_ENTRIES {
                let row = format!("{key} = {value} on {format}");
                let entry = parse_entry(&entry_text(format, body, &format!("{key} = {value}\n")));

                let refusals = shape_refusals("p", &entry);

                if format == takes {
                    assert_eq!(refusals, Vec::<String>::new(), "{row}");
                    accepted += 1;
                } else {
                    assert_eq!(
                        refusals,
                        vec![format!(
                            "provider `p`: `{key}` does not apply to format `{format}`; delete it"
                        )],
                        "{row}"
                    );
                    refused += 1;
                }
            }
        }
    }
    assert_eq!((refused, accepted), (8 * 2 * 6, 8 * 2));
}

#[test]
fn every_misplaced_key_of_one_entry_is_reported() {
    let entry = parse_entry(&entry_text(
        "openai-chat",
        "base_url = \"https://api.example.test/v1\"\nauth = \"bearer\"\n",
        "cloak_mode = \"never\"\ncodex_version = \"0.130.0\"\ncloud_project_id = \"x\"\n",
    ));

    let refusals = shape_refusals("p", &entry);

    assert_eq!(refusals.len(), 3, "{refusals:?}");
    for key in ["cloak_mode", "codex_version", "cloud_project_id"] {
        assert!(
            refusals.iter().any(|r| r.contains(&format!("`{key}`"))),
            "{key}: {refusals:?}"
        );
    }
}

const SENTINEL: &str = "sk-SENTINEL";

fn base_url_refusals(base_url: &str) -> Vec<String> {
    let text = entry_text(
        "openai-chat",
        &format!(
            "base_url = {}\nauth = \"bearer\"\n",
            toml::Value::String(base_url.to_string())
        ),
        "",
    );
    shape_refusals("p", &parse_entry(&text))
}

#[test]
fn a_base_url_that_is_not_a_plain_url_is_refused_without_echoing_it() {
    let rows = [
        ("query", "https://gw.example/v1?key=sk-SENTINEL", "a query"),
        ("empty query", "https://gw.example/v1?", "a query"),
        (
            "fragment",
            "https://gw.example/v1#sk-SENTINEL",
            "a fragment",
        ),
        (
            "userinfo",
            "https://user:sk-SENTINEL@gw.example/v1",
            "userinfo",
        ),
        ("user only", "https://sk-SENTINEL@gw.example/v1", "userinfo"),
        ("no host", "https://:443/sk-SENTINEL", "not a valid URL"),
        (
            "mailto",
            "mailto:sk-SENTINEL@gw.example",
            "scheme is not allowed",
        ),
        (
            "other scheme",
            "ftp://gw.example/sk-SENTINEL",
            "scheme is not allowed",
        ),
        ("not a url", "sk-SENTINEL", "not a valid URL"),
        ("blank", "   ", "set but empty"),
    ];
    for (label, base_url, rule) in rows {
        let refusals = base_url_refusals(base_url);

        assert_eq!(refusals.len(), 1, "{label}: {refusals:?}");
        let refusal = &refusals[0];
        assert!(
            refusal.starts_with("provider `p`: base_url "),
            "{label}: {refusal}"
        );
        assert!(refusal.contains(rule), "{label}: {refusal}");
        assert!(
            !refusal.contains(SENTINEL),
            "{label} echoes the value: {refusal}"
        );
        assert!(
            !refusal.contains("gw.example"),
            "{label} echoes the host: {refusal}"
        );
    }
}

#[test]
fn a_plain_base_url_is_accepted() {
    for base_url in [
        "https://gw.example",
        "https://gw.example/v1/openai",
        "https://gw.example/v1/",
        "https://gw.example:8443/v1",
        "http://127.0.0.1:8080/v1",
        "http://localhost:11434",
        "http://[::1]:8080/",
    ] {
        assert_eq!(
            base_url_refusals(base_url),
            Vec::<String>::new(),
            "{base_url}"
        );
    }
}

fn validation_errors(text: &str) -> Vec<String> {
    let cfg: Config = toml::from_str(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    collect_config_validation(&cfg).errors
}

/// The ordinary value of each format-specific key loads on an entry of the
/// format that takes it, through the whole validation suite: the
/// applicability rule refuses nothing an applicable entry legitimately sets.
#[test]
fn each_format_key_loads_on_its_own_format() {
    #[cfg_attr(
        not(any(feature = "openai-responses", feature = "gemini")),
        allow(unused_mut)
    )]
    let mut rows = vec![(
        "anthropic-messages",
        "base_url = \"https://api.anthropic.com\"\ncredential = \"env://KEY\"\n\
         auth = \"x-api-key\"\nanthropic_version = \"2023-06-01\"\n\
         forward_client_headers = [\"x-claude-code-session-id\"]\n\
         context_management = true\ncloak_mode = \"never\"\ncloak_strict_mode = true\n",
    )];
    #[cfg(feature = "openai-responses")]
    rows.push((
        "openai-responses",
        "base_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"env://KEY\"\n\
         auth = \"bearer\"\ncodex_version = \"0.130.0\"\naccount_id_ref = \"env://ACCOUNT\"\n",
    ));
    #[cfg(feature = "gemini")]
    rows.push((
        "gemini-cloud-code",
        "base_url = \"https://cloudcode-pa.googleapis.com\"\n\
         credential = \"oauth://antigravity\"\nauth = \"bearer\"\n\
         cloud_project_id = \"my-project-1234\"\n",
    ));
    for (format, body) in rows {
        let errors = validation_errors(&format!("[providers.p]\nformat = \"{format}\"\n{body}"));

        assert_eq!(errors, Vec::<String>::new(), "{format}");
    }
}

/// A file with several broken entries is reported in one pass: every
/// entry's shape problems together, each problem once.
#[test]
fn every_entry_shape_problem_of_a_file_is_reported_together() {
    let text = "\
[providers.queried]
format = \"openai-chat\"
base_url = \"https://gw.example/v1?key=sk-SENTINEL\"
credential = \"env://KEY\"
auth = \"bearer\"

[providers.misplaced]
format = \"openai-chat\"
base_url = \"https://gw.example/v1\"
credential = \"env://KEY\"
auth = \"bearer\"
cloak_mode = \"never\"

[providers.carrierless]
format = \"anthropic-messages\"
credential = \"env://KEY\"
auth = \"x-api-key\"
codex_version = \"0.130.0\"
";

    let errors = validation_errors(text);

    let expected = [
        ("queried", "a query"),
        (
            "misplaced",
            "`cloak_mode` does not apply to format `openai-chat`",
        ),
        ("carrierless", "set exactly one of `base_url` or `cloud`"),
        (
            "carrierless",
            "`codex_version` does not apply to format `anthropic-messages`",
        ),
    ];
    for (provider, rule) in expected {
        let hits = errors
            .iter()
            .filter(|e| e.contains(&format!("provider `{provider}`")) && e.contains(rule))
            .count();
        assert_eq!(hits, 1, "{provider} {rule}: {errors:?}");
    }
    assert_eq!(errors.len(), expected.len(), "{errors:?}");
    assert!(errors.iter().all(|e| !e.contains(SENTINEL)), "{errors:?}");
}

/// The carrier rule is reported once, whether the entry sets both carriers
/// or neither, although the plan checks it too.
#[test]
fn the_carrier_refusal_is_reported_once() {
    for (label, carrier) in [
        ("neither", ""),
        (
            "both",
            "base_url = \"https://gw.example/v1\"\ncloud = { name = \"aws\", region = \"us-east-1\" }\n",
        ),
    ] {
        let errors = validation_errors(&format!(
            "[providers.p]\nformat = \"openai-chat\"\n{carrier}credential = \"env://KEY\"\n\
             auth = \"bearer\"\n"
        ));

        let carrier_refusals = errors
            .iter()
            .filter(|e| e.contains("exactly one of `base_url` or `cloud`"))
            .count();
        assert_eq!(carrier_refusals, 1, "{label}: {errors:?}");
    }
}

/// An entry the plan refuses for an unrelated reason still has its shape
/// problems reported beside the plan's refusal.
#[test]
fn a_shape_problem_is_reported_beside_a_plan_refusal() {
    let errors = validation_errors(
        "[providers.p]\nformat = \"openai-chat\"\nbase_url = \"https://gw.example/v1#frag\"\n\
         credential = \"literal:sk-SENTINEL\"\nauth = \"bearer\"\n",
    );

    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(
        errors.iter().any(|e| e.contains("a fragment")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("`credential`")),
        "{errors:?}"
    );
}
