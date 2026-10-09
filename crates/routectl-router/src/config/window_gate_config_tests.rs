use super::{Config, WindowGateConfig};

/// A config omitting `[window_gate]` must parse with the gate DISABLED:
/// the gate is opt-in, so an untouched config never reorders a chain.
#[test]
fn absent_block_leaves_the_gate_disabled() {
    // Arrange / Act: a config with no [window_gate] table at all.
    let config: Config =
        toml::from_str("version = 3\n[server]\nhost = \"127.0.0.1\"\n").expect("parse");

    // Assert
    assert_eq!(config.window_gate, WindowGateConfig::default());
    assert!(!config.window_gate.enabled);
}

/// An empty `[window_gate]` table takes the field-level default, which must
/// agree with the struct-level one: present-but-empty is still off.
#[test]
fn empty_block_leaves_the_gate_disabled() {
    // Arrange / Act
    let config: Config = toml::from_str("version = 3\n[window_gate]\n").expect("parse");

    // Assert
    assert!(!config.window_gate.enabled);
}

/// An explicit `true` reaches the field: the opt-in spelling an operator
/// writes is honored.
#[test]
fn explicit_enable_is_honored() {
    // Arrange / Act
    let config: Config =
        toml::from_str("version = 3\n[window_gate]\nenabled = true\n").expect("parse");

    // Assert
    assert!(config.window_gate.enabled);
}

/// The kill switch round-trips: an explicit `false` reaches the field and
/// survives one serialize/deserialize loop. Pins the field-name spelling
/// that becomes an operator-facing contract once written into a TOML.
#[test]
fn explicit_disable_round_trips() {
    // Arrange / Act
    let config: Config =
        toml::from_str("version = 3\n[window_gate]\nenabled = false\n").expect("parse");

    // Assert
    assert!(!config.window_gate.enabled);

    let serialized = toml::to_string(&config).expect("serialize");
    let reparsed: Config = toml::from_str(&serialized).expect("re-parse");
    assert!(!reparsed.window_gate.enabled);
}

/// `deny_unknown_fields`: a typo'd key inside the block surfaces at load
/// instead of being silently ignored, which would leave an operator
/// believing a gate is off when it is on.
#[test]
fn unknown_key_is_rejected() {
    let err = toml::from_str::<Config>("version = 3\n[window_gate]\nenable = false\n")
        .expect_err("unknown [window_gate] key must be rejected");
    assert!(err.to_string().contains("enable"), "err: {err}");
}
