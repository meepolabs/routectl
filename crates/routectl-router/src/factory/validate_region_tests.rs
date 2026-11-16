use super::validate_aws_regions;
use crate::config::Config;
use crate::factory::collect_config_validation;

/// One provider of each format that rides the AWS cloud, all naming
/// `region`.
fn provider_blocks(region: &str) -> Vec<(&'static str, String)> {
    let mut formats = vec![
        "bedrock-invoke",
        "bedrock-converse",
        "anthropic-messages",
        "openai-chat",
    ];
    if cfg!(feature = "openai-responses") {
        formats.push("openai-responses");
    }
    formats
        .into_iter()
        .map(|format| {
            (
                format,
                format!(
                    "[providers.lane]\nformat = \"{format}\"\n\
                     cloud = {{ name = \"aws\", region = {region:?} }}\n\
                     credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n"
                ),
            )
        })
        .collect()
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
        "[providers.plain]\nformat = \"openai-chat\"\n\
         base_url = \"https://api.example.com/v1\"\ncredential = \"env://KEY\"\nauth = \"bearer\"\n",
    );

    assert!(validate_aws_regions(&cfg).is_ok());
}
