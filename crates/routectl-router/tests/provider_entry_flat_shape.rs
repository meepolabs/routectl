//! The flat `[providers.X]` entry: unknown keys are refused even though the
//! runtime policy is flattened into the entry, the flattened keys are
//! accepted, the `cloud` table refuses unknown keys of its own, and an entry
//! naming both carriers is refused.
//!
//! Self-contained on purpose: it parses inline TOML through the public parse
//! funnel and validation suite only, so it pins the serde shape without any
//! other fixture in the crate.

use routectl_router::{Config, ProviderEntry, collect_config_validation, parse_config};

const CLEAN_ENTRY: &str = r#"
[providers.p]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "env://ANTHROPIC_API_KEY"
auth = "x-api-key"
"#;

fn parse(text: &str) -> Result<Config, String> {
    parse_config(text)
}

fn only_entry(config: &Config) -> &ProviderEntry {
    config.providers.get("p").expect("provider p parsed")
}

#[test]
fn an_unknown_key_on_the_entry_is_refused_and_the_clean_entry_parses() {
    let with_unknown = format!("{CLEAN_ENTRY}bogus_key = 1\n");

    let clean = parse(CLEAN_ENTRY);
    let refused = parse(&with_unknown);

    assert!(clean.is_ok(), "clean control must parse: {clean:?}");
    let err = refused.expect_err("an unknown key must be refused");
    assert!(err.contains("unknown field `bogus_key`"), "{err}");
}

#[test]
fn a_flattened_runtime_key_is_accepted_on_the_entry() {
    let text = format!("{CLEAN_ENTRY}rpm_limit = 7\ncircuit_failures = 3\n");

    let config = parse(&text).expect("flattened runtime keys parse");

    let runtime = only_entry(&config).runtime();
    assert_eq!(runtime.rpm_limit, Some(7));
    assert_eq!(runtime.circuit_failures, Some(3));
}

#[test]
fn an_unknown_key_inside_cloud_is_refused_and_a_clean_cloud_parses() {
    let entry = |cloud: &str| {
        format!(
            "[providers.p]\nformat = \"bedrock-converse\"\ncloud = {cloud}\n\
             credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n"
        )
    };
    let clean = parse(&entry(r#"{ name = "aws", region = "us-east-1" }"#));
    let refused = parse(&entry(
        r#"{ name = "aws", region = "us-east-1", endpoint = "x" }"#,
    ));

    assert!(clean.is_ok(), "clean control must parse: {clean:?}");
    let err = refused.expect_err("an unknown key inside cloud must be refused");
    assert!(err.contains("unknown field `endpoint`"), "{err}");
}

#[test]
fn an_entry_naming_both_base_url_and_cloud_is_refused_by_key_name() {
    const SENTINEL_HOST: &str = "sentinel-carrier-host.example";
    let both = format!(
        "[providers.p]\nformat = \"anthropic-messages\"\n\
         base_url = \"https://{SENTINEL_HOST}\"\n\
         cloud = {{ name = \"aws\", region = \"us-east-1\" }}\n\
         credential = \"env://ANTHROPIC_API_KEY\"\nauth = \"x-api-key\"\n"
    );
    let clean = parse(CLEAN_ENTRY).expect("clean control parses");
    let refused = parse(&both).expect("both carriers parse; validation refuses");

    let clean_errors = collect_config_validation(&clean).errors;
    let errors = collect_config_validation(&refused).errors;

    assert!(clean_errors.is_empty(), "clean control: {clean_errors:?}");
    let carrier_errors: Vec<&String> = errors
        .iter()
        .filter(|e| e.contains("provider `p`") && e.contains("`base_url`") && e.contains("`cloud`"))
        .collect();
    assert_eq!(carrier_errors.len(), 1, "{errors:?}");
    assert!(
        errors.iter().all(|e| !e.contains(SENTINEL_HOST)),
        "the base_url value is withheld: {errors:?}"
    );
}
