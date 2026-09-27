use super::validate_aws_regions;
use crate::config::Config;
use crate::factory::collect_config_validation;

/// One provider of each region-deriving shape, all naming `region`.
fn provider_blocks(region: &str) -> Vec<(&'static str, String)> {
    let mut blocks = vec![
        (
            "bedrock",
            format!(
                "[providers.lane]\nkind = \"bedrock\"\nregion = {region:?}\n\
                 creds = {{ kind = \"default-chain\" }}\n"
            ),
        ),
        (
            "anthropic-api mantle",
            format!(
                "[providers.lane]\nkind = \"anthropic-api\"\n\
                 bedrock_mantle = {{ region = {region:?}, creds = {{ kind = \"default-chain\" }} }}\n"
            ),
        ),
        (
            "openai-compat mantle",
            format!(
                "[providers.lane]\nkind = \"openai-compat\"\napi_key_ref = \"\"\n\
                 bedrock_mantle = {{ region = {region:?}, creds = {{ kind = \"default-chain\" }} }}\n"
            ),
        ),
    ];
    if cfg!(feature = "openai-responses") {
        blocks.push((
            "openai-responses mantle",
            format!(
                "[providers.lane]\nkind = \"openai-responses\"\napi_key_ref = \"\"\n\
                 bedrock_mantle = {{ region = {region:?}, creds = {{ kind = \"default-chain\" }} }}\n"
            ),
        ));
    }
    blocks
}

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).unwrap_or_else(|e| panic!("must parse: {e}\n{toml_text}"))
}

#[test]
fn accepts_commercial_govcloud_and_china_regions_on_every_lane() {
    for region in ["us-west-2", "eu-central-1", "us-gov-west-1", "cn-north-1"] {
        for (lane, toml_text) in provider_blocks(region) {
            let cfg = parse(&toml_text);

            let result = validate_aws_regions(&cfg);
            let suite = collect_config_validation(&cfg).errors;

            assert!(result.is_ok(), "{lane} {region}: {result:?}");
            assert!(suite.is_empty(), "{lane} {region}: {suite:?}");
        }
    }
}

#[test]
fn rejects_a_host_altering_region_on_every_lane() {
    let hostile = [
        "",
        "   ",
        "us-west-2.evil.example",
        "x@127.0.0.1:9/",
        "evil.example#",
        "evil.example?",
        "us-west-2/",
        "US-WEST-2",
        "us-west-2 ",
        "us-west-2\n",
    ];

    for region in hostile {
        for (lane, toml_text) in provider_blocks(region) {
            let cfg = parse(&toml_text);

            let err = validate_aws_regions(&cfg).unwrap_err();

            let msg = err.to_string();
            assert!(msg.contains("provider `lane`"), "{lane} {region:?}: {msg}");
            assert!(msg.contains("region"), "{lane} {region:?}: {msg}");
        }
    }
}

#[test]
fn the_whole_validation_suite_reports_a_bad_region_exactly_once() {
    for region in ["x@127.0.0.1:9/", "", "   "] {
        for (lane, toml_text) in provider_blocks(region) {
            let cfg = parse(&toml_text);

            let errors = collect_config_validation(&cfg).errors;

            let region_errors: Vec<_> = errors.iter().filter(|e| e.contains("region")).collect();
            assert_eq!(region_errors.len(), 1, "{lane} {region:?}: {errors:?}");
        }
    }
}

#[test]
fn rejection_message_does_not_echo_the_region_value() {
    let toml_text = &provider_blocks("us-west-2.evil.example")[0].1;
    let cfg = parse(toml_text);

    let msg = validate_aws_regions(&cfg).unwrap_err().to_string();

    assert!(!msg.contains("evil"), "must not echo the value: {msg}");
}

#[test]
fn ignores_providers_that_derive_no_endpoint_from_a_region() {
    let cfg = parse(
        "[providers.plain]\nkind = \"openai-compat\"\napi_key_ref = \"env://KEY\"\n\
         base_url = \"https://api.example.com/v1\"\n",
    );

    assert!(validate_aws_regions(&cfg).is_ok());
}
