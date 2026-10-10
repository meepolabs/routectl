//! The per-kind attributability decision every field stage shares, driven over
//! each kind field repair admits and each refusal it owns.

use super::{AttributableLane, attributable_field_lane};
use crate::config::Config;

const PROVIDER: &str = "p0";

const ANTHROPIC_REMOTE: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";
const ANTHROPIC_LOOPBACK: &str =
    "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\nbase_url = \"http://127.0.0.1:8889\"\n";
const COMPAT_REMOTE: &str = "kind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";
const COMPAT_LOOPBACK: &str = "kind = \"openai-compat\"\nbase_url = \"http://localhost:11434/v1\"\napi_key_ref = \"literal:k\"\n";
#[cfg(feature = "bedrock")]
const BEDROCK_NATIVE: &str =
    "kind = \"bedrock\"\nregion = \"us-west-2\"\ncreds = { kind = \"default-chain\" }\n";
#[cfg(feature = "bedrock")]
const BEDROCK_UNADMITTED_REGION: &str =
    "kind = \"bedrock\"\nregion = \"not-a-region\"\ncreds = { kind = \"default-chain\" }\n";
#[cfg(feature = "bedrock")]
const ANTHROPIC_MANTLE: &str = "kind = \"anthropic-api\"\napi_key_ref = \"\"\n\
     bedrock_mantle = { region = \"us-west-2\", creds = { kind = \"default-chain\" } }\n";
#[cfg(feature = "bedrock")]
const COMPAT_MANTLE: &str = "kind = \"openai-compat\"\napi_key_ref = \"\"\n\
     bedrock_mantle = { region = \"us-west-2\", creds = { kind = \"default-chain\" } }\n";
#[cfg(feature = "openai-responses")]
const RESPONSES_REMOTE: &str =
    "kind = \"openai-responses\"\napi_key_ref = \"literal:k\"\nauth_kind = \"api-key\"\n";
#[cfg(feature = "gemini")]
const GEMINI_REMOTE: &str = "kind = \"gemini\"\napi_key_ref = \"literal:k\"\n";

/// One row: the `[providers.p0]` body (`None` = no entry), the caller's
/// forwarded fact, and the expected `(kind, base_url)` or refusal.
struct Row {
    name: &'static str,
    provider_toml: Option<&'static str>,
    forwarded: bool,
    expect: Option<(&'static str, &'static str)>,
}

fn rows() -> Vec<Row> {
    #[cfg_attr(
        not(any(feature = "bedrock", feature = "openai-responses", feature = "gemini")),
        allow(unused_mut)
    )]
    let mut rows = vec![
        Row {
            name: "anthropic-api, own credential",
            provider_toml: Some(ANTHROPIC_REMOTE),
            forwarded: false,
            expect: Some(("anthropic-api", "https://api.anthropic.com")),
        },
        Row {
            name: "anthropic-api, forwarded",
            provider_toml: Some(ANTHROPIC_REMOTE),
            forwarded: true,
            expect: None,
        },
        Row {
            name: "anthropic-api, loopback base",
            provider_toml: Some(ANTHROPIC_LOOPBACK),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "openai-compat, own credential",
            provider_toml: Some(COMPAT_REMOTE),
            forwarded: false,
            expect: Some(("openai-compat", "https://example.test/v1")),
        },
        Row {
            name: "openai-compat, forwarded",
            provider_toml: Some(COMPAT_REMOTE),
            forwarded: true,
            expect: None,
        },
        Row {
            name: "openai-compat, loopback base",
            provider_toml: Some(COMPAT_LOOPBACK),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "missing entry",
            provider_toml: None,
            forwarded: false,
            expect: None,
        },
    ];
    #[cfg(feature = "bedrock")]
    rows.extend([
        Row {
            name: "bedrock, own credential",
            provider_toml: Some(BEDROCK_NATIVE),
            forwarded: false,
            expect: Some(("bedrock", "https://bedrock-runtime.us-west-2.amazonaws.com")),
        },
        Row {
            name: "bedrock, forwarded",
            provider_toml: Some(BEDROCK_NATIVE),
            forwarded: true,
            expect: None,
        },
        Row {
            name: "bedrock, a region that names no endpoint",
            provider_toml: Some(BEDROCK_UNADMITTED_REGION),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "anthropic-api, Mantle sub-lane",
            provider_toml: Some(ANTHROPIC_MANTLE),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "openai-compat, Mantle sub-lane",
            provider_toml: Some(COMPAT_MANTLE),
            forwarded: false,
            expect: None,
        },
    ]);
    #[cfg(feature = "openai-responses")]
    rows.push(Row {
        name: "openai-responses, own credential",
        provider_toml: Some(RESPONSES_REMOTE),
        forwarded: false,
        expect: None,
    });
    #[cfg(feature = "gemini")]
    rows.push(Row {
        name: "gemini, own credential",
        provider_toml: Some(GEMINI_REMOTE),
        forwarded: false,
        expect: None,
    });
    rows
}

fn config_for(provider_toml: Option<&str>) -> Config {
    provider_toml.map_or_else(Config::default, |body| {
        toml::from_str(&format!("\n[providers.{PROVIDER}]\n{body}")).expect("valid test toml")
    })
}

#[test]
fn the_attributability_decision_matches_the_table_for_every_kind() {
    for row in rows() {
        // Arrange
        let config = config_for(row.provider_toml);

        // Act
        let verdict = attributable_field_lane(&config, PROVIDER, row.forwarded);

        // Assert
        let observed = verdict.as_ref().map(
            |AttributableLane {
                 provider_kind,
                 base_url,
             }| (*provider_kind, base_url.as_ref()),
        );
        assert_eq!(observed, row.expect, "{}", row.name);
    }
}

/// A refusing row is evidence only if its entry parsed into the kind it
/// names: a body that silently deserialized as another kind would pass the
/// table above for the wrong reason.
#[test]
fn each_refusing_row_parses_into_the_kind_it_names() {
    let premises: Vec<(&str, &str)> = {
        #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
        let mut premises = vec![
            (ANTHROPIC_LOOPBACK, "anthropic-api"),
            (COMPAT_LOOPBACK, "openai-compat"),
        ];
        #[cfg(feature = "bedrock")]
        premises.extend([
            (ANTHROPIC_MANTLE, "anthropic-api"),
            (COMPAT_MANTLE, "openai-compat"),
            (BEDROCK_UNADMITTED_REGION, "bedrock"),
        ]);
        premises
    };
    for (provider_toml, kind) in premises {
        let config = config_for(Some(provider_toml));
        let entry = config.providers.get(PROVIDER).expect("the entry parsed");
        assert_eq!(entry.kind_str(), kind, "premise: {provider_toml}");
    }
}
