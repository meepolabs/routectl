use super::{CapabilityConfig, Config, OverrideEntry};

#[test]
fn absent_block_uses_defaults() {
    // Arrange / Act: a full config that omits [capability] entirely.
    let config: Config =
        toml::from_str("version = 3\n[server]\nhost = \"127.0.0.1\"\n").expect("parse");

    // Assert: the omitted block resolves to the documented defaults.
    assert_eq!(config.capability, CapabilityConfig::default());
    assert!(config.capability.enabled);
    assert_eq!(config.capability.decay_hours, 48);
    assert_eq!(config.capability.inferred_window_hours, 1);
}

#[test]
fn explicit_values_are_honored() {
    // Arrange / Act
    let config: Config = toml::from_str(
        "version = 3\n[capability]\nenabled = false\ndecay_hours = 12\n\
             inferred_window_hours = 3\n",
    )
    .expect("parse");

    // Assert
    assert!(!config.capability.enabled);
    assert_eq!(config.capability.decay_hours, 12);
    assert_eq!(config.capability.inferred_window_hours, 3);
}

#[test]
fn partial_block_defaults_the_omitted_keys() {
    // Only the kill switch set; the two tempo knobs keep their defaults.
    let config: Config =
        toml::from_str("version = 3\n[capability]\nenabled = false\n").expect("parse");

    assert!(!config.capability.enabled);
    assert_eq!(config.capability.decay_hours, 48);
    assert_eq!(config.capability.inferred_window_hours, 1);
}

#[test]
fn unknown_key_is_rejected() {
    // deny_unknown_fields: a typo'd key surfaces at load, never silent.
    let err = toml::from_str::<Config>("version = 3\n[capability]\ndecay_hrs = 5\n")
        .expect_err("unknown [capability] key must be rejected");
    assert!(err.to_string().contains("decay_hrs"), "err: {err}");
}

/// `config example` prints `examples/config.toml` verbatim; that shipped
/// text must render a `[capability]` block, and it must parse with the
/// documented defaults.
#[test]
fn shipped_example_renders_capability_block() {
    let example = include_str!("../../../../examples/config.toml");
    assert!(
        example.contains("[capability]"),
        "shipped example must render a [capability] block"
    );

    let config: Config = toml::from_str(example).expect("example parses as Config");
    assert!(config.capability.enabled);
    assert_eq!(config.capability.decay_hours, 48);
    assert_eq!(config.capability.inferred_window_hours, 1);
}

#[test]
fn absent_overrides_table_yields_empty_map() {
    // Omitting [capability.overrides] leaves the map empty and keeps
    // Default equality intact for a config that sets only tempo knobs.
    let config: Config =
        toml::from_str("version = 3\n[capability]\nenabled = true\n").expect("parse");

    assert!(config.capability.overrides.is_empty());
    assert_eq!(config.capability, CapabilityConfig::default());
}

#[test]
fn two_tier_overrides_deserialize_to_expected_map() {
    // Arrange / Act: provider-wide and provider:nickname targets.
    let config: Config = toml::from_str(
        "version = 3\n\
             [capability.overrides.anthropic]\n\
             unsupported = [\"computer_use\"]\n\
             force_supported = [\"structured_output\"]\n\
             [capability.overrides.\"anthropic:sonnet\"]\n\
             unsupported = [\"prefill\"]\n",
    )
    .expect("parse");

    // Assert: both keys present with the documented values.
    assert_eq!(config.capability.overrides.len(), 2);
    assert_eq!(
        config.capability.overrides.get("anthropic"),
        Some(&OverrideEntry {
            unsupported: vec!["computer_use".to_string()],
            force_supported: vec!["structured_output".to_string()],
        })
    );
    assert_eq!(
        config.capability.overrides.get("anthropic:sonnet"),
        Some(&OverrideEntry {
            unsupported: vec!["prefill".to_string()],
            force_supported: Vec::new(),
        })
    );
}

#[test]
fn override_entry_omitted_lists_default_empty() {
    // An entry naming neither list is valid; both lists default empty.
    let config: Config =
        toml::from_str("version = 3\n[capability.overrides.openai]\n").expect("parse");

    assert_eq!(
        config.capability.overrides.get("openai"),
        Some(&OverrideEntry::default())
    );
}

#[test]
fn unknown_override_entry_key_is_rejected() {
    // deny_unknown_fields on OverrideEntry: a typo'd inner key surfaces
    // at load rather than being silently dropped.
    let err = toml::from_str::<Config>(
        "version = 3\n[capability.overrides.anthropic]\nunsuported = [\"x\"]\n",
    )
    .expect_err("unknown OverrideEntry key must be rejected");
    assert!(err.to_string().contains("unsuported"), "err: {err}");
}

#[test]
fn old_and_new_shape_configs_both_parse() {
    // Old shape: no [capability.overrides] at all.
    let old_shape: Config =
        toml::from_str("version = 3\n[capability]\nenabled = true\ndecay_hours = 48\n")
            .expect("legacy-shape config parses");
    assert!(old_shape.capability.overrides.is_empty());

    // New shape: the overrides table present.
    let new_shape: Config = toml::from_str(
        "version = 3\n[capability.overrides.anthropic]\nunsupported = [\"computer_use\"]\n",
    )
    .expect("new-shape config parses");
    assert_eq!(new_shape.capability.overrides.len(), 1);
}
