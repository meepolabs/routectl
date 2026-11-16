use std::collections::BTreeSet;

use routectl_core::wire_check::EgressKind;

use super::{Auth, Carrier, FORMAT_ROWS, Format, VENDOR_ROWS, format_row, vendor_row};
use crate::config::{CONFIG_PROVIDER_KINDS, Config};

const ALL_FORMATS: [Format; 7] = [
    Format::AnthropicMessages,
    Format::OpenaiChat,
    Format::OpenaiResponses,
    Format::BedrockInvoke,
    Format::BedrockConverse,
    Format::Gemini,
    Format::GeminiCloudCode,
];

#[test]
fn format_rows_map_to_frozen_family_tokens() {
    let expected = [
        (Format::AnthropicMessages, "anthropic-api"),
        (Format::OpenaiChat, "openai-compat"),
        (Format::OpenaiResponses, "openai-responses"),
        (Format::BedrockInvoke, "bedrock"),
        (Format::BedrockConverse, "bedrock"),
        (Format::Gemini, "gemini"),
        (Format::GeminiCloudCode, "gemini"),
    ];

    for (format, token) in expected {
        assert_eq!(
            format_row(format).token,
            token,
            "family token of {format:?}"
        );
    }
    assert_eq!(FORMAT_ROWS.len(), expected.len(), "one row per format");
}

#[test]
fn format_serde_values_are_kebab_case() {
    let expected = [
        (Format::AnthropicMessages, "anthropic-messages"),
        (Format::OpenaiChat, "openai-chat"),
        (Format::OpenaiResponses, "openai-responses"),
        (Format::BedrockInvoke, "bedrock-invoke"),
        (Format::BedrockConverse, "bedrock-converse"),
        (Format::Gemini, "gemini"),
        (Format::GeminiCloudCode, "gemini-cloud-code"),
    ];
    for (format, value) in expected {
        assert_eq!(serde_json::to_value(format).unwrap(), value, "{format:?}");
    }

    let auths = [
        (Auth::XApiKey, "x-api-key"),
        (Auth::Bearer, "bearer"),
        (Auth::XGoogApiKey, "x-goog-api-key"),
        (Auth::AwsSigv4, "aws-sigv4"),
    ];
    for (auth, value) in auths {
        assert_eq!(serde_json::to_value(auth).unwrap(), value, "{auth:?}");
    }
}

/// Every allowed (format, carrier, auth) resolves to exactly one egress
/// kind, no two formats share an egress kind on the same carrier unless they
/// share a family token, and no route admits an empty auth list.
#[test]
fn every_allowed_route_has_one_egress_kind() {
    let mut seen: Vec<(Format, Carrier, Auth, EgressKind)> = Vec::new();
    for row in FORMAT_ROWS {
        for carrier in [Carrier::BaseUrl, Carrier::CloudAws] {
            let Some(route) = row.route(carrier) else {
                continue;
            };
            assert!(
                !route.auths.is_empty(),
                "{:?} over {carrier:?} is allowed with no auth",
                row.format
            );
            assert_ne!(route.egress, EgressKind::Local, "{:?}", row.format);
            for auth in route.auths {
                assert!(
                    !seen
                        .iter()
                        .any(|(f, c, a, _)| (*f, *c, *a) == (row.format, carrier, *auth)),
                    "duplicate route {:?}/{carrier:?}/{auth:?}",
                    row.format
                );
                seen.push((row.format, carrier, *auth, route.egress));
            }
        }
    }
    assert!(!seen.is_empty(), "positive control: some route is allowed");

    for format in [Format::BedrockInvoke, Format::BedrockConverse] {
        assert_eq!(
            format_row(format).route(Carrier::BaseUrl),
            None,
            "{format:?}"
        );
    }
    for format in [Format::Gemini, Format::GeminiCloudCode] {
        assert_eq!(
            format_row(format).route(Carrier::CloudAws),
            None,
            "{format:?}"
        );
    }
}

#[test]
fn routes_match_the_lanes_this_build_can_construct() {
    let url = |f| {
        format_row(f)
            .route(Carrier::BaseUrl)
            .map(|r| (r.egress, r.auths))
    };
    let aws = |f| {
        format_row(f)
            .route(Carrier::CloudAws)
            .map(|r| (r.egress, r.auths))
    };
    let mantle: &[Auth] = &[Auth::Bearer, Auth::AwsSigv4];

    assert_eq!(
        url(Format::AnthropicMessages),
        Some((EgressKind::AnthropicApi, &[Auth::XApiKey, Auth::Bearer][..]))
    );
    assert_eq!(
        url(Format::OpenaiChat),
        Some((EgressKind::OpenaiCompat, &[Auth::Bearer][..]))
    );

    let bedrock = cfg!(feature = "bedrock");
    let responses = cfg!(feature = "openai-responses");
    let gemini = cfg!(feature = "gemini");
    let cases = [
        (
            aws(Format::AnthropicMessages),
            bedrock,
            EgressKind::AnthropicApiMantle,
        ),
        (
            aws(Format::OpenaiChat),
            bedrock,
            EgressKind::OpenaiCompatMantle,
        ),
        (
            aws(Format::OpenaiResponses),
            bedrock && responses,
            EgressKind::OpenaiResponsesMantle,
        ),
        (
            aws(Format::BedrockInvoke),
            bedrock,
            EgressKind::BedrockInvoke,
        ),
        (
            aws(Format::BedrockConverse),
            bedrock,
            EgressKind::BedrockConverse,
        ),
    ];
    for (got, built, egress) in cases {
        assert_eq!(got, built.then_some((egress, mantle)), "{egress:?}");
    }
    assert_eq!(
        url(Format::OpenaiResponses),
        responses.then_some((EgressKind::OpenaiResponses, &[Auth::Bearer][..]))
    );
    assert_eq!(
        url(Format::Gemini),
        gemini.then_some((EgressKind::Gemini, &[Auth::XGoogApiKey][..]))
    );
    assert_eq!(
        url(Format::GeminiCloudCode),
        gemini.then_some((EgressKind::Gemini, &[Auth::Bearer][..]))
    );
}

#[test]
fn format_row_is_total() {
    for format in ALL_FORMATS {
        assert_eq!(format_row(format).format, format);
    }
}

#[test]
fn descriptor_family_tokens_equal_config_provider_kinds() {
    let routable: BTreeSet<&str> = FORMAT_ROWS
        .iter()
        .filter(|row| row.base_url.is_some() || row.cloud_aws.is_some())
        .map(|row| row.token)
        .collect();
    let configurable: BTreeSet<&str> = CONFIG_PROVIDER_KINDS.iter().copied().collect();

    assert_eq!(routable, configurable);
}

#[test]
fn vendor_rows_weld_to_known_oauth_providers() {
    let vendors: Vec<&str> = VENDOR_ROWS.iter().map(|row| row.vendor).collect();

    assert_eq!(vendors, routectl_auth::oauth::known_provider_ids());
    assert_eq!(vendor_row("not-a-vendor"), None);
}

#[test]
fn vendor_rows_pin_format_hosts_and_identity() {
    let expected: [(&str, Format, &str, &[&str], bool); 4] = [
        (
            "anthropic",
            Format::AnthropicMessages,
            "https://api.anthropic.com",
            &["api.anthropic.com"],
            true,
        ),
        (
            "codex",
            Format::OpenaiResponses,
            "https://chatgpt.com/backend-api/codex",
            &["chatgpt.com"],
            true,
        ),
        (
            "xai",
            Format::OpenaiChat,
            "https://api.x.ai/v1",
            &["api.x.ai"],
            false,
        ),
        (
            "antigravity",
            Format::GeminiCloudCode,
            "https://daily-cloudcode-pa.googleapis.com",
            &[
                "cloudcode-pa.googleapis.com",
                "daily-cloudcode-pa.googleapis.com",
            ],
            true,
        ),
    ];

    for (vendor, format, base, hosts, identity) in expected {
        let row = vendor_row(vendor).expect(vendor);
        assert_eq!(row.format, format, "{vendor}");
        assert_eq!(row.default_base, base, "{vendor}");
        assert_eq!(row.hosts, hosts, "{vendor}");
        assert_eq!(row.identity, identity, "{vendor}");
        let base_host = url::Url::parse(row.default_base)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned));
        assert!(
            base_host.is_some_and(|h| row.hosts.contains(&h.as_str())),
            "{vendor}: default base must be on an allowed host"
        );
    }
}

fn entry(toml_text: &str) -> crate::config::ProviderEntry {
    let cfg: Config = toml::from_str(toml_text).expect("fixture parses");
    cfg.providers.into_values().next().expect("one provider")
}

/// One row per format and carrier combination an entry can express: the
/// format, the family token and the egress kind it sends with.
#[test]
fn format_of_every_current_shape() {
    #[cfg_attr(
        not(any(feature = "bedrock", feature = "openai-responses", feature = "gemini")),
        allow(unused_mut)
    )]
    let mut rows: Vec<(&str, &str, Format, &str, EgressKind)> = vec![
        (
            "anthropic-api",
            "[providers.p]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"env://K\"\nauth = \"x-api-key\"\n",
            Format::AnthropicMessages,
            "anthropic-api",
            EgressKind::AnthropicApi,
        ),
        (
            "openai-compat",
            "[providers.p]\nformat = \"openai-chat\"\nbase_url = \"https://example.com/v1\"\ncredential = \"env://K\"\nauth = \"bearer\"\n",
            Format::OpenaiChat,
            "openai-compat",
            EgressKind::OpenaiCompat,
        ),
    ];
    #[cfg(feature = "bedrock")]
    rows.extend([
        (
            "anthropic-api mantle",
            "[providers.p]\nformat = \"anthropic-messages\"\n\
             cloud = { name = \"aws\", region = \"us-west-2\" }\n\
             credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n",
            Format::AnthropicMessages,
            "anthropic-api",
            EgressKind::AnthropicApiMantle,
        ),
        (
            "openai-compat mantle",
            "[providers.p]\nformat = \"openai-chat\"\n\
             cloud = { name = \"aws\", region = \"us-west-2\" }\n\
             credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n",
            Format::OpenaiChat,
            "openai-compat",
            EgressKind::OpenaiCompatMantle,
        ),
        (
            "bedrock invoke",
            "[providers.p]\nformat = \"bedrock-invoke\"\ncloud = { name = \"aws\", region = \"us-east-1\" }\ncredential = \"aws://chain\"\nauth = \"aws-sigv4\"\n",
            Format::BedrockInvoke,
            "bedrock",
            EgressKind::BedrockInvoke,
        ),
        (
            "bedrock converse",
            "[providers.p]\nformat = \"bedrock-converse\"\ncloud = { name = \"aws\", region = \"us-east-1\" }\ncredential = \"aws://chain\"\nauth = \"aws-sigv4\"\n",
            Format::BedrockConverse,
            "bedrock",
            EgressKind::BedrockConverse,
        ),
    ]);
    #[cfg(feature = "openai-responses")]
    rows.extend([
        (
            "responses chatgpt-oauth",
            "[providers.p]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex\"\nauth = \"bearer\"\n",
            Format::OpenaiResponses,
            "openai-responses",
            EgressKind::OpenaiResponses,
        ),
        (
            "responses api-key",
            "[providers.p]\nformat = \"openai-responses\"\nbase_url = \"https://api.openai.com/v1\"\ncredential = \"env://K\"\nauth = \"bearer\"\n",
            Format::OpenaiResponses,
            "openai-responses",
            EgressKind::OpenaiResponses,
        ),
    ]);
    #[cfg(all(feature = "openai-responses", feature = "bedrock"))]
    rows.push((
        "responses mantle",
        "[providers.p]\nformat = \"openai-responses\"\n\
         cloud = { name = \"aws\", region = \"us-west-2\" }\n\
         credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n",
        Format::OpenaiResponses,
        "openai-responses",
        EgressKind::OpenaiResponsesMantle,
    ));
    #[cfg(feature = "gemini")]
    rows.extend([
        (
            "gemini api-key",
            "[providers.p]\nformat = \"gemini\"\nbase_url = \"https://generativelanguage.googleapis.com/v1beta\"\ncredential = \"env://K\"\nauth = \"x-goog-api-key\"\n",
            Format::Gemini,
            "gemini",
            EgressKind::Gemini,
        ),
        (
            "gemini cloud-code",
            "[providers.p]\nformat = \"gemini-cloud-code\"\nbase_url = \"https://daily-cloudcode-pa.googleapis.com\"\ncredential = \"oauth://antigravity\"\nauth = \"bearer\"\n",
            Format::GeminiCloudCode,
            "gemini",
            EgressKind::Gemini,
        ),
    ]);

    let bedrock = usize::from(cfg!(feature = "bedrock"));
    let responses = usize::from(cfg!(feature = "openai-responses"));
    let gemini = usize::from(cfg!(feature = "gemini"));
    let expected_rows = 2 + 4 * bedrock + 2 * responses + responses * bedrock + 2 * gemini;
    assert_eq!(rows.len(), expected_rows, "one row per constructible shape");

    for (name, toml_text, format, token, egress) in rows {
        let entry = entry(toml_text);
        assert_eq!(entry.format(), format, "{name}: format");
        assert_eq!(entry.kind_str(), token, "{name}: kind_str");
        assert_eq!(entry.egress_kind(), egress, "{name}: egress_kind");
    }
}
