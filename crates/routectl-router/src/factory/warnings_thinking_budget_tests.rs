//! Tests for `unread_thinking_budget_warnings`: `[models.X]
//! max_thinking_budget` is projected onto every resolved model regardless
//! of kind, but only the Anthropic-shape thinking builder reads it, so on
//! any other kind the value is inert and earns an advisory WARN.

use super::unread_thinking_budget_warnings;
use crate::config::Config;

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("fixture must parse")
}

/// A single provider entry of `kind`, with one `[models.m]` bound to it.
/// `budget_line` is the `max_thinking_budget` assignment, or empty to leave
/// the key unset.
fn config_for(kind: &str, provider_body: &str, budget_line: &str) -> Config {
    parse(&format!(
        "version = 2\n\
         [providers.p]\n\
         kind = \"{kind}\"\n\
         {provider_body}\
         [models.m]\n\
         provider = \"p\"\n\
         upstream = \"some-model\"\n\
         {budget_line}"
    ))
}

fn openai_compat(budget_line: &str) -> Config {
    config_for(
        "openai-compat",
        "base_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n",
        budget_line,
    )
}

fn anthropic_api(budget_line: &str) -> Config {
    config_for(
        "anthropic-api",
        "api_key_ref = \"literal:k\"\n",
        budget_line,
    )
}

#[test]
fn silent_when_the_knob_is_unset_on_a_non_reading_kind() {
    assert!(
        unread_thinking_budget_warnings(&openai_compat("")).is_empty(),
        "an unset key is not a misconfiguration on any kind"
    );
}

#[test]
fn silent_on_an_anthropic_shape_model_that_sets_the_knob() {
    assert!(
        unread_thinking_budget_warnings(&anthropic_api("max_thinking_budget = 16000\n")).is_empty(),
        "the anthropic-api egress reads the cap, so the key is live there"
    );
}

#[test]
fn warns_on_an_openai_compat_model_that_sets_the_knob() {
    // POSITIVE CONTROL for the silent cases above: the same key on a kind
    // whose egress never reads it must fire, or those assertions would pass
    // vacuously.
    let warnings = unread_thinking_budget_warnings(&openai_compat("max_thinking_budget = 16000\n"));

    assert_eq!(
        warnings.len(),
        1,
        "an inert budget cap must warn exactly once: {warnings:?}"
    );
    assert!(
        warnings[0].contains("[models.m]"),
        "the warning must name the model entry key: {}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("openai-compat"),
        "the warning must name the kind that ignores the key: {}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("max_thinking_budget") && warnings[0].contains("no effect"),
        "the warning must name the knob and state that it is unread: {}",
        warnings[0]
    );
}

#[test]
fn the_warning_names_the_reading_kinds_from_the_provider_side_source() {
    let warnings = unread_thinking_budget_warnings(&openai_compat("max_thinking_budget = 8000\n"));
    for reader in routectl_providers::anthropic_api::MAX_THINKING_BUDGET_READER_KINDS {
        assert!(
            warnings[0].contains(reader),
            "the recovery hint must name reading kind {reader}: {}",
            warnings[0]
        );
    }
}

#[cfg(feature = "gemini")]
#[test]
fn warns_on_a_gemini_model_that_sets_the_knob() {
    let config = config_for(
        "gemini",
        "api_key_ref = \"env://GEMINI_API_KEY\"\n",
        "max_thinking_budget = 16000\n",
    );
    let warnings = unread_thinking_budget_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "the gemini egress never reads the cap: {warnings:?}"
    );
    assert!(
        warnings[0].contains("gemini"),
        "the warning must name the kind: {}",
        warnings[0]
    );
}

#[cfg(feature = "openai-responses")]
#[test]
fn warns_on_an_openai_responses_model_that_sets_the_knob() {
    let config = config_for(
        "openai-responses",
        "api_key_ref = \"literal:k\"\n",
        "max_thinking_budget = 16000\n",
    );
    let warnings = unread_thinking_budget_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "the openai-responses egress never reads the cap: {warnings:?}"
    );
    assert!(
        warnings[0].contains("openai-responses"),
        "the warning must name the kind: {}",
        warnings[0]
    );
}

/// Bedrock reads the cap on BOTH shapes -- Invoke delegates body
/// construction to the anthropic-api normalizer, Converse reuses the same
/// thinking builder -- so neither shape may warn.
#[cfg(feature = "bedrock")]
#[test]
fn silent_on_a_bedrock_model_on_either_api_shape() {
    for api_shape in ["invoke", "converse"] {
        let config = parse(&format!(
            "version = 2\n\
             [providers.b]\n\
             kind = \"bedrock\"\n\
             region = \"us-east-1\"\n\
             api_shape = \"{api_shape}\"\n\
             creds = {{ kind = \"default-chain\" }}\n\
             [models.m]\n\
             provider = \"b\"\n\
             upstream = \"some-model\"\n\
             max_thinking_budget = 16000\n"
        ));
        assert!(
            unread_thinking_budget_warnings(&config).is_empty(),
            "bedrock api_shape = {api_shape} reads the cap"
        );
    }
}

/// A pool-backed model resolves its kind through the pool's members. The
/// knob applies the same way there, and a `[pools]` name is not a
/// `[providers]` name -- a lookup that missed the pool table would report
/// nothing at all.
#[test]
fn warns_on_a_pool_backed_model_whose_members_are_a_non_reading_kind() {
    let config = parse(
        "version = 2\n\
         [providers.a]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n\
         api_key_ref = \"oauth://openai\"\n\
         [providers.b]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n\
         api_key_ref = \"oauth://openai\"\n\
         [pools.team]\n\
         members = [\"a\", \"b\"]\n\
         [models.m]\n\
         provider = \"team\"\n\
         upstream = \"some-model\"\n\
         max_thinking_budget = 16000\n",
    );
    let warnings = unread_thinking_budget_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "a pool of non-reading members carries the same inert key: {warnings:?}"
    );
    assert!(
        warnings[0].contains("openai-compat"),
        "the warning must name the members' kind: {}",
        warnings[0]
    );
}

/// A `provider` value naming neither a provider entry nor a pool is a hard
/// validation error reported elsewhere; this advisory has no kind to reason
/// about and must stay quiet rather than guess one.
#[test]
fn silent_for_a_model_whose_provider_reference_dangles() {
    let config = parse(
        "version = 2\n\
         [models.m]\n\
         provider = \"nope\"\n\
         upstream = \"some-model\"\n\
         max_thinking_budget = 16000\n",
    );
    assert!(unread_thinking_budget_warnings(&config).is_empty());
}

#[test]
fn the_advisory_rides_the_aggregate_validator_as_a_warning_not_an_error() {
    let config = openai_compat("max_thinking_budget = 16000\n");
    let validation = super::super::validate::collect_config_validation(&config);

    assert!(
        validation
            .warnings
            .iter()
            .any(|w| w.contains("[models.m]") && w.contains("max_thinking_budget")),
        "the line must reach .warnings so both serve boot and `config check` show it: {:?}",
        validation.warnings
    );
    assert!(
        validation.errors.is_empty(),
        "an inert knob is advisory and must never fail the load: {:?}",
        validation.errors
    );
}
