//! The config key a missing-price label points at: every rate dimension maps
//! to a path the config editor accepts, and a label or nickname that cannot be
//! written as a dotted path maps to nothing.

use routectl_router::{PathShape, validate_config_path};

use super::*;
use crate::commands::usage::{UNKNOWN_LIFETIME_LABEL, UNKNOWN_REASONING_LABEL};

const EVERY_RATE_DIMENSION: [PriceDimension; 6] = [
    PriceDimension::Input,
    PriceDimension::Output,
    PriceDimension::CacheRead,
    PriceDimension::CacheWrite(300),
    PriceDimension::CacheWrite(1800),
    PriceDimension::CacheWrite(3600),
];

#[test]
fn every_rate_dimension_label_maps_to_its_config_key_as_a_valid_scalar() {
    for dimension in EVERY_RATE_DIMENSION {
        let label = dimension_label(dimension);

        let key = price_config_key("sonnet", &label).expect("a rate label has a key");

        assert_eq!(
            validate_config_path(&key),
            Ok(PathShape::Scalar),
            "label {label:?} -> {key}"
        );
        assert_eq!(
            key,
            format!("models.sonnet.pricing.{}", dimension.config_key()),
            "label {label:?}"
        );
    }
}

#[test]
fn labels_that_name_no_rate_map_to_no_key() {
    // The control: the same model yields a key for a rate label.
    assert!(price_config_key("sonnet", "input").is_some());

    for label in [
        UNKNOWN_LIFETIME_LABEL,
        UNKNOWN_REASONING_LABEL,
        "",
        "cache write",
        "cache write 0m",
        "cache write 60m",
        "cache write thirty",
        "Input",
    ] {
        assert_eq!(price_config_key("sonnet", label), None, "label {label:?}");
    }
}

#[test]
fn nickname_that_needs_quoting_maps_to_no_key_never_an_invalid_one() {
    let bare = ["opus-4_7", "Haiku"];
    for model in bare {
        let key = price_config_key(model, "output").expect("a bare nickname has a key");
        assert_eq!(
            validate_config_path(&key),
            Ok(PathShape::Scalar),
            "model {model:?}"
        );
    }

    let quoted = ["claude.opus", "my model", "\"q\"", "it's", "caf\u{e9}", ""];
    for model in quoted {
        assert_eq!(price_config_key(model, "output"), None, "model {model:?}");
    }
}
