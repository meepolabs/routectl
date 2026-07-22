//! Tests for the `[registry.*]` pricing table: parsing, the
//! `deny_unknown_fields` guard inside `[pricing]`, and the
//! `Config::pricing_for` glob resolver.

use super::Config;

#[test]
fn registry_pricing_block_parses() {
    // Arrange
    let toml_text = r#"
[registry."deepseek-*"]

[registry."deepseek-*".pricing]
input_per_mtok = 0.27
output_per_mtok = 1.1
cache_read_per_mtok = 0.07
cache_write_5m_per_mtok = 0.5
cache_write_1h_per_mtok = 0.9
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse registry block");

    // Assert
    let entry = cfg.registry.get("deepseek-*").expect("entry present");
    let pricing = entry.pricing.as_ref().expect("pricing present");
    assert_eq!(pricing.input_per_mtok, Some(0.27));
    assert_eq!(pricing.output_per_mtok, Some(1.1));
    assert_eq!(pricing.cache_read_per_mtok, Some(0.07));
    assert_eq!(pricing.cache_write_5m_per_mtok, Some(0.5));
    assert_eq!(pricing.cache_write_1h_per_mtok, Some(0.9));
    assert!(entry.provider.is_none());
}

#[test]
fn registry_pricing_rejects_unknown_field() {
    // Arrange: typo'd `inputs_per_mtok` inside [pricing].
    let toml_text = r#"
[registry."deepseek-*".pricing]
inputs_per_mtok = 0.27
"#;
    // Act
    let result = toml::from_str::<Config>(toml_text);

    // Assert
    assert!(
        result.is_err(),
        "unknown [registry.*.pricing] key must reject; got Ok"
    );
}

fn priced(input: f64) -> super::PricingConfig {
    super::PricingConfig {
        input_per_mtok: Some(input),
        ..super::PricingConfig::default()
    }
}

fn config_with_registry(entries: Vec<(&str, Option<&str>, super::PricingConfig)>) -> Config {
    let mut cfg = Config::default();
    for (key, provider, pricing) in entries {
        cfg.registry.insert(
            key.to_string(),
            super::RegistryEntry {
                pricing: Some(pricing),
                provider: provider.map(str::to_string),
            },
        );
    }
    cfg
}

#[test]
fn pricing_for_exact_beats_prefix() {
    // Arrange
    let cfg = config_with_registry(vec![
        ("deepseek-*", None, priced(1.0)),
        ("deepseek-chat", None, priced(2.0)),
    ]);

    // Act
    let pricing = cfg.pricing_for("deepseek-chat", "any").expect("match");

    // Assert: the exact key wins over the prefix.
    assert_eq!(pricing.input_per_mtok, Some(2.0));
}

/// Equal-length Exact-vs-Prefix tie: key `"deepseek*"` parses to a
/// Prefix with stored prefix "deepseek" (len 8) and key `"deepseek"`
/// parses to an Exact (len 8). Both match upstream "deepseek" with an
/// IDENTICAL prefix_len, so scope and length cannot break the tie --
/// the Exact entry must win on the is_exact tie-break.
#[test]
fn pricing_for_exact_beats_equal_length_prefix() {
    // Arrange
    let cfg = config_with_registry(vec![
        ("deepseek*", None, priced(1.0)),
        ("deepseek", None, priced(2.0)),
    ]);

    // Act
    let pricing = cfg.pricing_for("deepseek", "any").expect("match");

    // Assert: the exact entry wins the equal-length tie.
    assert_eq!(pricing.input_per_mtok, Some(2.0));
}

#[test]
fn pricing_for_longer_prefix_beats_shorter() {
    // Arrange
    let cfg = config_with_registry(vec![
        ("deep*", None, priced(1.0)),
        ("deepseek-*", None, priced(2.0)),
    ]);

    // Act
    let pricing = cfg.pricing_for("deepseek-chat", "any").expect("match");

    // Assert
    assert_eq!(pricing.input_per_mtok, Some(2.0));
}

#[test]
fn pricing_for_provider_scoped_preferred_over_agnostic() {
    // Arrange: two entries match the same upstream -- one agnostic,
    // one scoped to `vendor-a`. They use distinct glob keys (the
    // table is keyed by pattern string, so a same-pattern collision
    // would dedupe; provider scoping rides on distinct keys).
    let cfg = config_with_registry(vec![
        ("deepseek-*", None, priced(1.0)),
        ("deepseek-c*", Some("vendor-a"), priced(2.0)),
    ]);

    // Act + Assert: matching provider gets the scoped price even
    // though it is the SHORTER-matching... here the scoped key is
    // longer, but scope is the primary key so verify scope wins by
    // making the agnostic key at least as long.
    let scoped = cfg
        .pricing_for("deepseek-chat", "vendor-a")
        .expect("scoped match");
    assert_eq!(scoped.input_per_mtok, Some(2.0));

    // A different provider falls back to the agnostic entry; the
    // entry scoped to vendor-a is NOT eligible for vendor-b.
    let agnostic = cfg
        .pricing_for("deepseek-chat", "vendor-b")
        .expect("agnostic match");
    assert_eq!(agnostic.input_per_mtok, Some(1.0));
}

#[test]
fn pricing_for_scope_beats_longer_agnostic_prefix() {
    // Arrange: the agnostic entry has the LONGER prefix; scope must
    // still win because provider-scope is the primary sort key.
    let cfg = config_with_registry(vec![
        ("deepseek-chat-v3", None, priced(1.0)),
        ("deepseek-*", Some("vendor-a"), priced(2.0)),
    ]);

    // Act
    let scoped = cfg
        .pricing_for("deepseek-chat-v3", "vendor-a")
        .expect("scoped match");

    // Assert: scope beats the longer agnostic prefix.
    assert_eq!(scoped.input_per_mtok, Some(2.0));
}

#[test]
fn pricing_for_no_match_returns_none() {
    // Arrange
    let cfg = config_with_registry(vec![("deepseek-*", None, priced(1.0))]);

    // Act
    let result = cfg.pricing_for("gpt-4o", "any");

    // Assert
    assert!(result.is_none(), "no glob matches => None");
}
