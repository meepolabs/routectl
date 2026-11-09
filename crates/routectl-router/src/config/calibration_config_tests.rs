use super::{CalibrationConfig, Config};

/// A config omitting `[calibration]` parses to the default block.
#[test]
fn absent_block_parses_to_the_default() {
    // Arrange / Act: a config with no [calibration] table at all.
    let config: Config =
        toml::from_str("version = 3\n[server]\nhost = \"127.0.0.1\"\n").expect("parse");

    // Assert
    assert_eq!(config.calibration, CalibrationConfig::default());
    assert!(config.calibration.enabled);
}

/// An existing config that sets `enabled = false` still loads, and the value
/// survives one serialize/deserialize loop. The key has no effect; it is
/// kept so such configs are not refused.
#[test]
fn explicit_disable_round_trips() {
    // Arrange / Act
    let config: Config =
        toml::from_str("version = 3\n[calibration]\nenabled = false\n").expect("parse");

    // Assert
    assert!(!config.calibration.enabled);

    let serialized = toml::to_string(&config).expect("serialize");
    let reparsed: Config = toml::from_str(&serialized).expect("re-parse");
    assert!(!reparsed.calibration.enabled);
}

/// `deny_unknown_fields`: a typo'd key inside the block surfaces at load
/// instead of being silently ignored.
#[test]
fn unknown_key_is_rejected() {
    let err = toml::from_str::<Config>("version = 3\n[calibration]\nenable = false\n")
        .expect_err("unknown [calibration] key must be rejected");
    assert!(err.to_string().contains("enable"), "err: {err}");
}
