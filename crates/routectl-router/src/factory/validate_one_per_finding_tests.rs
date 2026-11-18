//! `collect_config_validation` returns one entry per finding, including the
//! findings a validator reports together as one multi-line message.

use super::collect_config_validation;
use crate::config::Config;
use routectl_core::MAX_PROVIDER_NAME_BYTES;

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("fixture must parse")
}

fn errors_of(toml_text: &str) -> Vec<String> {
    collect_config_validation(&parse(toml_text)).errors
}

fn provider(name: &str) -> String {
    format!(
        "[providers.\"{name}\"]\nformat = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\ncredential = \"env://K\"\n\
         auth = \"x-api-key\"\n"
    )
}

fn model(nickname: &str, extra: &str) -> String {
    format!("[models.{nickname}]\nprovider = \"p\"\nupstream = \"u\"\n{extra}")
}

/// Two findings of one validator are two entries, each one line, each
/// naming its own key.
fn assert_two_single_line_entries(label: &str, errors: &[String], names: [&str; 2]) {
    assert_eq!(errors.len(), 2, "{label}: {errors:?}");
    for error in errors {
        assert!(!error.contains('\n'), "{label}: {error:?}");
    }
    for (error, name) in errors.iter().zip(names) {
        assert!(error.contains(name), "{label}: `{name}` in {error:?}");
    }
}

#[test]
fn two_bad_provider_names_are_two_entries() {
    let long_a = "a".repeat(MAX_PROVIDER_NAME_BYTES + 1);
    let long_b = "b".repeat(MAX_PROVIDER_NAME_BYTES + 2);
    let over_long = format!("{}{}", provider(&long_a), provider(&long_b));
    let separator = format!("{}{}", provider("x#1"), provider("y#2"));
    let rows = [
        ("over-long", over_long, ["is 129 bytes", "is 130 bytes"]),
        ("separator-bearing", separator, ["`x#1`", "`y#2`"]),
    ];
    for (label, text, names) in rows {
        assert_two_single_line_entries(label, &errors_of(&text), names);
    }
}

#[test]
fn two_bad_pools_are_two_entries() {
    let text = "[pools.east]\nmembers = []\n[pools.west]\nmembers = []\n";

    let errors = errors_of(text);

    assert_two_single_line_entries("empty pools", &errors, ["`east`", "`west`"]);
}

/// One row per remaining validator that joins its findings: each row trips
/// that validator alone, twice.
#[test]
fn each_joining_validator_yields_one_entry_per_finding() {
    let p = provider("p");
    let rows = [
        (
            "alias chain targets",
            format!("{p}[aliases]\nfast = \"ghost-1\"\nslow = \"ghost-2\"\n"),
            ["`ghost-1`", "`ghost-2`"],
        ),
        (
            "model token limits",
            format!(
                "{p}{}{}",
                model("one", "context_window = 0\n"),
                model("two", "context_window = 0\n"),
            ),
            ["models.one.", "models.two."],
        ),
        (
            "model pricing",
            format!(
                "{p}{}{}",
                model("one", "[models.one.pricing]\ninput_per_mtok = -1.0\n"),
                model("two", "[models.two.pricing]\ninput_per_mtok = -1.0\n"),
            ),
            ["models.one.", "models.two."],
        ),
    ];
    for (label, text, names) in rows {
        assert_two_single_line_entries(label, &errors_of(&text), names);
    }
}

#[cfg(feature = "bedrock")]
#[test]
fn the_bedrock_invoke_family_gate_yields_one_entry_per_finding() {
    let text = "[providers.p]\nformat = \"bedrock-invoke\"\n\
         cloud = { name = \"aws\", region = \"us-west-2\" }\n\
         credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n\
         [models.one]\nprovider = \"p\"\nupstream = \"gpt-4o\"\n\
         [models.two]\nprovider = \"p\"\nupstream = \"gpt-4o\"\n";

    let errors = errors_of(text);

    assert_two_single_line_entries("bedrock invoke", &errors, ["[models.one]", "[models.two]"]);
}

/// The split does not touch a lone finding: its entry is the exact message
/// the validator has always produced.
#[test]
fn a_single_finding_keeps_its_exact_wording() {
    let errors = errors_of(&provider("x#1"));

    assert_eq!(
        errors,
        vec![
            "provider name `x#1` must not contain `#` (reserved as the seat-pool \
             state-key separator)"
                .to_string()
        ]
    );
}

/// Text a hostile key injects after its newline, as if it were a finding of
/// its own.
const INJECTED: &str = "config: forged line";

/// A key holding a newline stays inside its own finding: the count is the
/// number of real findings, and no entry starts with the injected text.
#[test]
fn a_newline_bearing_key_is_one_finding() {
    let hostile = format!("a\\u001b[31mRED\\n{INJECTED}");
    let separator_hostile = format!("p#\\n{INJECTED}");
    let rows = [
        (
            "alias name, empty chain",
            format!("{}[aliases]\n\"{hostile}\" = []\n", provider("p")),
            1,
        ),
        (
            "alias name and target, unknown target",
            format!(
                "{}[aliases]\n\"{hostile}\" = \"{hostile}-target\"\n",
                provider("p")
            ),
            1,
        ),
        (
            "alias names, cycle",
            format!(
                "{}[aliases]\n\"{hostile}\" = \"b\"\nb = \"{hostile}\"\n",
                provider("p")
            ),
            1,
        ),
        (
            "provider name, separator-bearing",
            provider(&separator_hostile),
            1,
        ),
        (
            "model nickname equal to another provider's name",
            format!(
                "{}{}[models.\"{separator_hostile}\"]\nprovider = \"p\"\nupstream = \"u\"\n",
                provider("p"),
                provider(&separator_hostile),
            ),
            3,
        ),
    ];
    for (label, text, count) in rows {
        let errors = errors_of(&text);

        assert_eq!(errors.len(), count, "{label}: {errors:?}");
        for error in &errors {
            assert!(!error.contains('\n'), "{label}: {error:?}");
            assert!(!error.starts_with(INJECTED), "{label}: {error:?}");
            assert!(!error.contains('\u{1b}'), "{label}: {error:?}");
        }
        assert!(
            errors.iter().any(|e| e.contains(INJECTED)),
            "{label}: the key is still named: {errors:?}"
        );
    }
}

#[cfg(feature = "bedrock")]
#[test]
fn a_newline_bearing_name_in_the_bedrock_invoke_gate_is_one_finding() {
    let text = format!(
        "[providers.p]\nformat = \"bedrock-invoke\"\n\
         cloud = {{ name = \"aws\", region = \"us-west-2\" }}\n\
         credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n\
         [models.\"m\\n{INJECTED}\"]\nprovider = \"p\"\nupstream = \"gpt\\n{INJECTED}\"\n"
    );

    let errors = errors_of(&text);

    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(!errors[0].contains('\n'), "{errors:?}");
    assert!(errors[0].contains(INJECTED), "{errors:?}");
}
