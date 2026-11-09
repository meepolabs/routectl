use super::{Config, SeatQuotaConfig};

/// A config omitting `[seat_quota]` parses to the default block.
#[test]
fn absent_block_parses_to_the_default() {
    // Arrange / Act: a config with no [seat_quota] table at all.
    let config: Config =
        toml::from_str("version = 3\n[server]\nhost = \"127.0.0.1\"\n").expect("parse");

    // Assert
    assert_eq!(config.seat_quota, SeatQuotaConfig::default());
    assert!(config.seat_quota.enabled);
}

/// An existing config that sets `enabled = false` still loads, and the value
/// survives one serialize/deserialize loop. The key has no effect; it is
/// kept so such configs are not refused.
#[test]
fn explicit_disable_round_trips() {
    // Arrange / Act
    let config: Config =
        toml::from_str("version = 3\n[seat_quota]\nenabled = false\n").expect("parse");

    // Assert
    assert!(!config.seat_quota.enabled);

    let serialized = toml::to_string(&config).expect("serialize");
    let reparsed: Config = toml::from_str(&serialized).expect("re-parse");
    assert!(!reparsed.seat_quota.enabled);
}

/// `deny_unknown_fields`: a typo'd key inside the block surfaces at load
/// instead of being silently ignored.
#[test]
fn unknown_key_is_rejected() {
    let err = toml::from_str::<Config>("version = 3\n[seat_quota]\nenable = false\n")
        .expect_err("unknown [seat_quota] key must be rejected");
    assert!(err.to_string().contains("enable"), "err: {err}");
}

/// The block carries EXACTLY one knob. The curated caps, the long-window
/// guard and the freshness bounds are deliberately not operator surface, and
/// this pins that: a second field here would also have to be classified for
/// reload and re-rendered into the committed schema.
#[test]
fn the_block_exposes_only_the_enabled_key() {
    let rendered = toml::to_string(&SeatQuotaConfig::default()).expect("serialize");
    let keys: Vec<&str> = rendered
        .lines()
        .filter_map(|line| line.split('=').next())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .collect();
    assert_eq!(keys, vec!["enabled"]);
}
