use super::*;

// -----------------------------------------------------------------------
// Legacy sidecar: read side only (round-trips a manually-written file --
// the write side that used to produce it is gone).
// -----------------------------------------------------------------------

#[test]
fn load_verifications_reads_a_manually_written_sidecar_file() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing_verifications.json");
    std::fs::write(
        &path,
        r#"{"verified":{"openai-compat:grok-*":"2026-06-30"}}"#,
    )
    .unwrap();

    // Act
    let loaded = load_verifications(&path).unwrap();

    // Assert
    assert_eq!(
        loaded
            .verified
            .get("openai-compat:grok-*")
            .map(String::as_str),
        Some("2026-06-30")
    );
}

#[test]
fn load_verifications_missing_path_returns_empty_default() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does_not_exist.json");

    // Act
    let result = load_verifications(&path);

    // Assert -- not an error; the map is empty
    assert!(result.is_ok());
    assert!(result.unwrap().verified.is_empty());
}

#[test]
fn load_verifications_malformed_json_returns_error() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, b"not valid json {{{").unwrap();

    // Act
    let result = load_verifications(&path);

    // Assert
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("malformed"), "expected 'malformed' in: {msg}");
}

// -----------------------------------------------------------------------
// merge_verifications_into: additive and config wins
// -----------------------------------------------------------------------

#[test]
fn merge_adds_new_selector_with_verified_at_only() {
    // Arrange
    let mut config = minimal_config();
    let mut v = PricingVerifications::default();
    v.verified
        .insert("openai-compat:grok-*".to_string(), "2026-06-30".to_string());

    // Act
    let skipped = merge_verifications_into(&mut config, &v);

    // Assert: the key was inserted as a pure verification override
    assert!(skipped.is_empty(), "no entries should be skipped");
    let ov = config
        .cache_pricing
        .get("openai-compat:grok-*")
        .expect("key should be inserted");
    assert_eq!(
        ov.verified_at.as_deref(),
        Some("2026-06-30"),
        "verified_at should be set"
    );
    assert!(ov.wm.is_none(), "wm should be None (pure verification)");
    assert!(ov.rm.is_none(), "rm should be None");
    assert!(ov.ttl_seconds.is_none(), "ttl_seconds should be None");
    assert!(
        ov.min_prefix_tokens.is_none(),
        "min_prefix_tokens should be None"
    );
}

#[test]
fn merge_does_not_overwrite_existing_config_key() {
    // Arrange: the config already has an entry for this selector
    let mut config = minimal_config();
    let existing = CachePricingOverride {
        wm: Some(1.5),
        verified_at: Some("2025-01-01".to_string()),
        override_acknowledges_cost_risk: true,
        ..Default::default()
    };
    config
        .cache_pricing
        .insert("openai-compat:grok-*".to_string(), existing);

    let mut v = PricingVerifications::default();
    v.verified
        .insert("openai-compat:grok-*".to_string(), "2026-06-30".to_string());

    // Act
    let skipped = merge_verifications_into(&mut config, &v);

    // Assert: the config entry is unchanged; existing key not in skipped
    assert!(
        skipped.is_empty(),
        "config-key wins should not appear in skipped"
    );
    let ov = config
        .cache_pricing
        .get("openai-compat:grok-*")
        .expect("key should still be present");
    assert_eq!(
        ov.verified_at.as_deref(),
        Some("2025-01-01"),
        "config entry should not be overwritten by sidecar"
    );
    assert_eq!(ov.wm, Some(1.5), "wm should be unchanged");
}

#[test]
fn merge_skips_malformed_date_and_inserts_valid_sibling() {
    // Arrange: one bad date, one good date
    let mut config = minimal_config();
    let mut v = PricingVerifications::default();
    v.verified
        .insert("openai-compat:grok-*".to_string(), "2026-13-99".to_string());
    v.verified.insert(
        "openai-compat:mistral-*".to_string(),
        "2026-06-30".to_string(),
    );

    // Act
    let skipped = merge_verifications_into(&mut config, &v);

    // Assert: malformed-date entry is skipped and reported
    assert_eq!(skipped, vec!["openai-compat:grok-*".to_string()]);
    assert!(
        !config.cache_pricing.contains_key("openai-compat:grok-*"),
        "malformed entry should not be inserted"
    );
    assert!(
        config.cache_pricing.contains_key("openai-compat:mistral-*"),
        "valid sibling should be inserted"
    );
}

fn minimal_config() -> Config {
    let toml = r#"
[server]
host = "127.0.0.1"
port = 4000
"#;
    toml::from_str(toml).expect("minimal config should parse")
}
