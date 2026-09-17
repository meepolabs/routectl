use super::{Config, FidelityConfig};

#[test]
fn absent_block_uses_defaults() {
    // Arrange / Act: a full config that omits [fidelity] entirely.
    let config: Config =
        toml::from_str("version = 3\n[server]\nhost = \"127.0.0.1\"\n").expect("parse");

    // Assert: the omitted block resolves to the documented defaults --
    // no prefix-impact opt-in, no paid-probe budget.
    assert_eq!(config.fidelity, FidelityConfig::default());
    assert!(config.fidelity.prefix_impact_opt_in.is_empty());
    assert!(config.fidelity.paid_probe_daily_caps.is_empty());
}

#[test]
fn explicit_values_are_honored() {
    // Arrange / Act
    let config: Config = toml::from_str(
        "version = 3\n[fidelity]\nprefix_impact_opt_in = [\"anthropic\", \"anthropic:sonnet\"]\n\
             [fidelity.paid_probe_daily_caps]\nanthropic = 5\n",
    )
    .expect("parse");

    // Assert
    assert_eq!(
        config.fidelity.prefix_impact_opt_in,
        vec!["anthropic".to_string(), "anthropic:sonnet".to_string()]
    );
    assert_eq!(
        config.fidelity.paid_probe_daily_caps.get("anthropic"),
        Some(&5)
    );
}

#[test]
fn partial_block_defaults_the_omitted_keys() {
    // Only the opt-in list set; the paid-probe cap map keeps its default.
    let config: Config =
        toml::from_str("version = 3\n[fidelity]\nprefix_impact_opt_in = [\"anthropic\"]\n")
            .expect("parse");

    assert_eq!(
        config.fidelity.prefix_impact_opt_in,
        vec!["anthropic".to_string()]
    );
    assert!(config.fidelity.paid_probe_daily_caps.is_empty());
}

#[test]
fn unknown_key_is_rejected() {
    // deny_unknown_fields: a typo'd key surfaces at load, never silent.
    let err = toml::from_str::<Config>("version = 3\n[fidelity]\nprefix_impct_opt_in = []\n")
        .expect_err("unknown [fidelity] key must be rejected");
    assert!(
        err.to_string().contains("prefix_impct_opt_in"),
        "err: {err}"
    );
}

#[test]
fn unknown_paid_probe_daily_caps_value_shape_is_rejected() {
    // The cap value is a u32; a non-numeric value must fail to parse.
    let err = toml::from_str::<Config>(
        "version = 3\n[fidelity.paid_probe_daily_caps]\nanthropic = \"five\"\n",
    )
    .expect_err("non-numeric cap value must be rejected");
    assert!(err.to_string().contains("integer") || err.to_string().contains("invalid type"));
}

/// `config example` prints `examples/config.toml` verbatim; that shipped
/// text documents `[fidelity]` as a commented-out example, so it must not
/// render an active `[fidelity]` block, and the file must still parse.
#[test]
fn shipped_example_documents_fidelity_without_activating_it() {
    let example = include_str!("../../../../examples/config.toml");
    assert!(
        example.contains("[fidelity]"),
        "shipped example must document the [fidelity] block"
    );

    #[cfg(all(feature = "bedrock", feature = "openai-responses"))]
    {
        let config: Config = toml::from_str(example).expect("example parses as Config");
        assert_eq!(config.fidelity, FidelityConfig::default());
    }
}

/// Lean-build counterpart: the lean example carries no `[fidelity]`
/// mention and still parses to the inert default.
#[test]
fn lean_example_parses_with_fidelity_defaults() {
    let example = crate::config::LEAN_EXAMPLE_CONFIG;
    let config: Config = toml::from_str(example).expect("lean example parses as Config");
    assert_eq!(config.fidelity, FidelityConfig::default());
}

#[test]
fn two_tier_prefix_impact_opt_in_deserializes_to_expected_list() {
    // Arrange / Act: a provider-wide target and a provider:nickname target.
    let config: Config = toml::from_str(
        "version = 3\n[fidelity]\nprefix_impact_opt_in = [\"anthropic\", \"anthropic:sonnet\"]\n",
    )
    .expect("parse");

    // Assert: both grammar shapes are preserved verbatim.
    assert_eq!(
        config.fidelity.prefix_impact_opt_in,
        vec!["anthropic".to_string(), "anthropic:sonnet".to_string()]
    );
}

#[test]
fn paid_probe_daily_caps_table_deserializes_to_expected_map() {
    let config: Config = toml::from_str(
        "version = 3\n[fidelity.paid_probe_daily_caps]\nanthropic = 3\nopenai = 10\n",
    )
    .expect("parse");

    assert_eq!(config.fidelity.paid_probe_daily_caps.len(), 2);
    assert_eq!(
        config.fidelity.paid_probe_daily_caps.get("anthropic"),
        Some(&3)
    );
    assert_eq!(
        config.fidelity.paid_probe_daily_caps.get("openai"),
        Some(&10)
    );
}
