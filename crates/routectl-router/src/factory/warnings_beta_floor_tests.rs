//! Tests for `beta_floor_override_warnings`: an `unsupported` `beta:` override
//! on an anthropic-api provider naming a flag the egress floor re-adds is
//! inert, and earns an advisory WARN naming the flag and the floor class.

use super::beta_floor_override_warnings;
use crate::config::Config;
use routectl_core::identity::anthropic::{
    EFFORT_BETA, OAUTH_ANTHROPIC_BETA, STRUCTURED_OUTPUTS_BETA, default_claude_code_anthropic_betas,
};

const PROVIDER_PINNED_BETA: &str = "provider-pinned-2026-01-01";
const MODEL_PINNED_BETA: &str = "model-pinned-2026-01-01";

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("fixture must parse")
}

/// One provider `p` of `kind` (with `provider_body` lines), one model `m`
/// bound to it carrying `model_beta` as its `anthropic-beta` header, and one
/// override cell `spec` whose `list` (`unsupported` or `force_supported`)
/// holds `beta:<flag>`.
fn config_with_override(
    kind: &str,
    provider_body: &str,
    spec: &str,
    list: &str,
    flag: &str,
) -> Config {
    parse(&format!(
        "version = 5\n\
         [providers.p]\n\
         kind = \"{kind}\"\n\
         {provider_body}\
         [providers.p.header_extras]\n\
         anthropic-beta = \"{PROVIDER_PINNED_BETA}\"\n\
         [models.m]\n\
         provider = \"p\"\n\
         upstream = \"some-model\"\n\
         [models.m.header_extras]\n\
         anthropic-beta = \"{MODEL_PINNED_BETA}\"\n\
         [capability.overrides.\"{spec}\"]\n\
         {list} = [\"beta:{flag}\"]\n"
    ))
}

fn anthropic_unsupported(spec: &str, flag: &str) -> Config {
    config_with_override(
        "anthropic-api",
        "api_key_ref = \"env://KEY\"\n",
        spec,
        "unsupported",
        flag,
    )
}

/// A Claude Code floor flag that sits in no other floor class, so its row
/// pins the Claude Code class alone.
fn claude_code_only_flag() -> &'static str {
    default_claude_code_anthropic_betas()
        .iter()
        .copied()
        .find(|flag| *flag != OAUTH_ANTHROPIC_BETA && *flag != STRUCTURED_OUTPUTS_BETA)
        .expect("the Claude Code floor carries a flag outside the other classes")
}

#[test]
fn an_unsupported_floor_flag_on_anthropic_api_warns_once_naming_its_class() {
    let rows: [(&str, &str, &str, &str); 6] = [
        ("oauth gate", "p", OAUTH_ANTHROPIC_BETA, "OAuth gate"),
        (
            "claude code floor",
            "p",
            claude_code_only_flag(),
            "Claude Code floor",
        ),
        (
            "structured outputs",
            "p",
            STRUCTURED_OUTPUTS_BETA,
            "structured outputs",
        ),
        ("effort", "p", EFFORT_BETA, "effort:"),
        (
            "provider header_extras",
            "p",
            PROVIDER_PINNED_BETA,
            "provider header_extras",
        ),
        (
            "model header_extras",
            "p:m",
            MODEL_PINNED_BETA,
            "model header_extras",
        ),
    ];

    for (row, spec, flag, class) in rows {
        let warnings = beta_floor_override_warnings(&anthropic_unsupported(spec, flag));

        assert_eq!(warnings.len(), 1, "row `{row}`: {warnings:?}");
        assert!(
            warnings[0].contains(flag),
            "row `{row}` must name the flag: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains(class),
            "row `{row}` must name the class `{class}`: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains(spec),
            "row `{row}` must name the spec: {}",
            warnings[0]
        );
    }
}

#[test]
fn a_model_header_flag_does_not_warn_on_a_provider_scoped_spec() {
    let warnings = beta_floor_override_warnings(&anthropic_unsupported("p", MODEL_PINNED_BETA));

    assert!(
        warnings.is_empty(),
        "a provider-wide spec covers models without that pin: {warnings:?}"
    );
}

#[test]
fn a_floor_flag_override_on_openai_compat_is_silent() {
    let config = config_with_override(
        "openai-compat",
        "base_url = \"https://example.test/v1\"\napi_key_ref = \"env://KEY\"\n",
        "p",
        "unsupported",
        EFFORT_BETA,
    );

    assert!(beta_floor_override_warnings(&config).is_empty());
}

#[test]
fn a_non_floor_flag_on_anthropic_api_is_silent() {
    let warnings =
        beta_floor_override_warnings(&anthropic_unsupported("p", "context-1m-2025-08-07"));

    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn a_force_supported_floor_flag_is_silent() {
    let config = config_with_override(
        "anthropic-api",
        "api_key_ref = \"env://KEY\"\n",
        "p",
        "force_supported",
        EFFORT_BETA,
    );

    assert!(beta_floor_override_warnings(&config).is_empty());
}

#[test]
fn a_case_variant_floor_flag_warns() {
    let variant = EFFORT_BETA.to_ascii_uppercase();

    let warnings = beta_floor_override_warnings(&anthropic_unsupported("p", &variant));

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(&variant), "{}", warnings[0]);
    assert!(warnings[0].contains("effort:"), "{}", warnings[0]);
}

#[test]
fn a_flag_in_two_floor_classes_warns_once_naming_both() {
    let warnings = beta_floor_override_warnings(&anthropic_unsupported("p", OAUTH_ANTHROPIC_BETA));

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("OAuth gate"), "{}", warnings[0]);
    assert!(warnings[0].contains("Claude Code floor"), "{}", warnings[0]);
}
