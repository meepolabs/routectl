use super::{CalibrationConfig, Config};

/// A config omitting `[calibration]` must parse with the correction ENABLED:
/// the switch defaults on, and no existing config needs a migration to keep
/// the shipped behavior (which is what a correction-less lane already gives).
#[test]
fn absent_block_leaves_the_correction_enabled() {
    // Arrange / Act: a config with no [calibration] table at all.
    let config: Config =
        toml::from_str("version = 3\n[server]\nhost = \"127.0.0.1\"\n").expect("parse");

    // Assert
    assert_eq!(config.calibration, CalibrationConfig::default());
    assert!(config.calibration.enabled);
}

/// The kill switch round-trips: an explicit `false` reaches the field and
/// survives one serialize/deserialize loop. Pins the field-name spelling
/// that becomes an operator-facing contract once written into a TOML.
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
/// instead of being silently ignored, which would leave an operator
/// believing the correction is off when it is on.
#[test]
fn unknown_key_is_rejected() {
    let err = toml::from_str::<Config>("version = 3\n[calibration]\nenable = false\n")
        .expect_err("unknown [calibration] key must be rejected");
    assert!(err.to_string().contains("enable"), "err: {err}");
}
