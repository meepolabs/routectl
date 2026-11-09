//! The embedded release table: it validates, it is already in canonical
//! form, and the accessor parses it once.

use super::shipped::SHIPPED_JSON;
use super::{parse_and_validate, shipped_table};

#[test]
fn the_shipped_table_validates_and_prices_at_least_one_model() {
    let table = parse_and_validate(SHIPPED_JSON.as_bytes()).expect("shipped table validates");

    assert!(table.provider_count() >= 1);
    assert!(table.model_count() >= 1);
}

#[test]
fn the_shipped_table_is_byte_identical_to_its_canonical_form() {
    let table = parse_and_validate(SHIPPED_JSON.as_bytes()).expect("shipped table validates");

    let canonical = table.to_canonical_json().expect("serializes");

    assert!(
        canonical == SHIPPED_JSON.as_bytes(),
        "the shipped file is not the pretty-printed, sorted output of the refresh"
    );
}

#[test]
fn the_accessor_returns_one_parse_of_the_embedded_table() {
    let expected = parse_and_validate(SHIPPED_JSON.as_bytes()).expect("shipped table validates");

    let first = shipped_table().expect("accessor yields the table");
    let second = shipped_table().expect("accessor yields the table");

    assert_eq!(first, &expected);
    assert!(std::ptr::eq(first, second), "the table was parsed twice");
}
