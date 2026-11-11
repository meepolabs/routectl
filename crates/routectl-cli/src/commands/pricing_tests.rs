//! The per-lifetime rate rules: the diagnostic's missing dimensions and the
//! carry onto the usage crate's `Rates`.

use std::collections::BTreeMap;

use super::*;

fn priced_base() -> PricingConfig {
    PricingConfig {
        input_per_mtok: Some(3.0),
        output_per_mtok: Some(15.0),
        cache_read_per_mtok: Some(0.3),
        cache_write_5m_per_mtok: Some(3.75),
        cache_write_1h_per_mtok: Some(6.0),
        cache_write_per_mtok: None,
    }
}

fn with_map(entries: &[(&str, f64)]) -> PricingConfig {
    PricingConfig {
        cache_write_per_mtok: Some(
            entries
                .iter()
                .map(|(key, rate)| ((*key).to_string(), *rate))
                .collect(),
        ),
        ..priced_base()
    }
}

#[test]
fn missing_equivalence_dimensions_names_each_unresolved_lifetime() {
    // (name, pricing, expected missing labels)
    let cases: [(&str, PricingConfig, &[&str]); 4] = [
        ("every price resolves", priced_base(), &[]),
        (
            "5m and 1h unpriced",
            PricingConfig {
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
                ..priced_base()
            },
            &["cache write 5m", "cache write 1h"],
        ),
        (
            "an extra priced lifetime resolves",
            with_map(&[("30m", 5.0)]),
            &[],
        ),
        (
            "an extra lifetime priced below zero",
            with_map(&[("30m", -1.0)]),
            &["cache write 30m"],
        ),
    ];
    for (name, pricing, expected) in cases {
        // Act
        let missing = missing_equivalence_dimensions(&pricing);

        // Assert
        assert_eq!(missing, expected, "{name}");
    }
}

#[test]
fn rates_from_pricing_carries_every_priced_lifetime() {
    // Arrange
    let pricing = with_map(&[("30m", 5.0)]);

    // Act
    let rates = rates_from_pricing(&pricing);

    // Assert
    assert_eq!(
        rates.cache_write_per_mtok,
        BTreeMap::from([(300, 3.75), (1800, 5.0), (3600, 6.0)])
    );
}
