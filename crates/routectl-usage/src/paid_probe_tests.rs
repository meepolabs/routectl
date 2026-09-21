//! Tests for the paid-probe reservation key and unit-count codec.

use super::*;

/// Epoch milliseconds exactly at the start of UTC day 20348.
const DAY_20348_START_MS: i64 = 20_348 * MS_PER_UTC_DAY;

#[test]
fn key_for_a_plain_provider_matches_the_durable_form() {
    // Arrange
    let day = utc_day_from_epoch_ms(DAY_20348_START_MS);

    // Act
    let key = reservation_key(day, "anthropic");

    // Assert
    assert_eq!(key, "paid_probe_reservation:v1:20348:616e7468726f706963");
}

#[test]
fn key_hex_encodes_every_provider_byte_as_two_lowercase_digits() {
    // Arrange
    let provider = "aB:0~";

    // Act
    let key = reservation_key(0, provider);
    let hex = key.rsplit(':').next().expect("provider field");

    // Assert
    assert_eq!(hex, "61423a307e");
    assert_eq!(hex.len(), provider.len() * 2);
    assert!(
        hex.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "provider field must be lowercase hex: {hex}"
    );
}

#[test]
fn key_for_a_provider_carrying_the_separator_keeps_four_fields() {
    // Arrange
    let provider = "a:b";

    // Act
    let key = reservation_key(7, provider);

    // Assert
    assert_eq!(key.split(':').count(), 4);
    assert_eq!(key, "paid_probe_reservation:v1:7:613a62");
}

#[test]
fn provider_names_cannot_forge_another_providers_key() {
    // Arrange: a name whose plain form would reproduce the key structure
    // of provider "b" on day 1.
    let forging = "x:paid_probe_reservation:v1:1:b";

    // Act
    let forged = reservation_key(9, forging);
    let honest = reservation_key(1, "b");

    // Assert
    assert_ne!(forged, honest);
    assert_eq!(forged.split(':').count(), 4);
    assert!(
        !forged.ends_with(":1:62"),
        "hex encoding must absorb the embedded structure: {forged}"
    );
}

#[test]
fn key_for_a_non_ascii_provider_encodes_its_utf8_bytes() {
    // Arrange: two-byte UTF-8 scalar, written as an ASCII source escape.
    let provider = "caf\u{e9}";

    // Act
    let key = reservation_key(0, provider);

    // Assert
    assert_eq!(key, "paid_probe_reservation:v1:0:636166c3a9");
}

#[test]
fn every_byte_takes_two_digits_so_short_bytes_cannot_merge() {
    // Arrange: two providers written with ASCII source escapes whose UTF-8
    // bytes only stay distinguishable at a fixed two-digit width. Padded,
    // they are c2 88 and 0c 28 08; unpadded, both render as "c288" and the
    // two providers would share one budget bucket.
    let two_byte = "\u{88}";
    let three_byte = "\u{c}(\u{8}";

    // Act
    let two_byte_key = reservation_key(11, two_byte);
    let three_byte_key = reservation_key(11, three_byte);

    // Assert: the premise -- each name's bytes do collapse to one string
    // without padding, so this fixture can expose a width defect.
    let unpadded = |s: &str| -> String {
        s.as_bytes()
            .iter()
            .map(|b| format!("{b:x}"))
            .collect::<String>()
    };
    assert_eq!(unpadded(two_byte), unpadded(three_byte));

    assert_eq!(two_byte_key, "paid_probe_reservation:v1:11:c288");
    assert_eq!(three_byte_key, "paid_probe_reservation:v1:11:0c2808");
    assert_ne!(two_byte_key, three_byte_key);
}

#[test]
fn distinct_providers_never_share_a_key_on_one_day() {
    // Arrange
    let names = [
        "a",
        "b",
        "a:b",
        "ab",
        "caf\u{e9}",
        "",
        "\u{88}",
        "\u{c}(\u{8}",
    ];

    // Act
    let keys: Vec<String> = names.iter().map(|n| reservation_key(5, n)).collect();

    // Assert
    for (i, left) in keys.iter().enumerate() {
        for right in keys.iter().skip(i + 1) {
            assert_ne!(left, right, "distinct providers produced one key");
        }
    }
}

#[test]
fn an_empty_provider_yields_an_empty_hex_field() {
    // Arrange / Act
    let key = reservation_key(3, "");

    // Assert
    assert_eq!(key, "paid_probe_reservation:v1:3:");
}

#[test]
fn the_last_millisecond_of_a_day_shares_that_days_key() {
    // Arrange
    let start = DAY_20348_START_MS;
    let end = DAY_20348_START_MS + MS_PER_UTC_DAY - 1;

    // Act
    let start_day = utc_day_from_epoch_ms(start);
    let end_day = utc_day_from_epoch_ms(end);

    // Assert
    assert_eq!(start_day, 20_348);
    assert_eq!(end_day, 20_348);
    assert_eq!(
        reservation_key(start_day, "anthropic"),
        reservation_key(end_day, "anthropic")
    );
}

#[test]
fn adjacent_days_produce_different_keys() {
    // Arrange
    let previous = utc_day_from_epoch_ms(DAY_20348_START_MS - 1);
    let current = utc_day_from_epoch_ms(DAY_20348_START_MS);
    let next = utc_day_from_epoch_ms(DAY_20348_START_MS + MS_PER_UTC_DAY);

    // Assert
    assert_eq!(previous, 20_347);
    assert_eq!(next, 20_349);
    assert_ne!(
        reservation_key(previous, "anthropic"),
        reservation_key(current, "anthropic")
    );
    assert_ne!(
        reservation_key(current, "anthropic"),
        reservation_key(next, "anthropic")
    );
}

#[test]
fn pre_epoch_timestamps_floor_to_earlier_days() {
    // Arrange / Act / Assert
    assert_eq!(utc_day_from_epoch_ms(0), 0);
    assert_eq!(utc_day_from_epoch_ms(-1), -1);
    assert_eq!(utc_day_from_epoch_ms(-MS_PER_UTC_DAY), -1);
    assert_eq!(utc_day_from_epoch_ms(-MS_PER_UTC_DAY - 1), -2);
}

#[test]
fn extreme_timestamps_stay_within_the_day_index() {
    // Arrange / Act / Assert
    assert_eq!(utc_day_from_epoch_ms(i64::MAX), i64::MAX / MS_PER_UTC_DAY);
    assert_eq!(
        utc_day_from_epoch_ms(i64::MIN),
        i64::MIN.div_euclid(MS_PER_UTC_DAY)
    );
}

#[test]
fn unit_counts_round_trip_through_the_canonical_form() {
    // Arrange
    let cases = [0_u32, 1, 9, 10, 4_294_967_294, u32::MAX];

    // Act / Assert
    for units in cases {
        let rendered = format_units(units);
        assert_eq!(parse_units(&rendered), Ok(units), "round trip of {units}");
    }
}

#[test]
fn the_formatter_emits_only_canonical_decimal() {
    // Arrange / Act / Assert
    assert_eq!(format_units(0), "0");
    assert_eq!(format_units(1), "1");
    assert_eq!(format_units(u32::MAX), "4294967295");
}

#[test]
fn an_empty_value_is_malformed_rather_than_zero() {
    // Arrange / Act / Assert
    assert_eq!(parse_units(""), Err(MalformedUnits::Empty));
}

#[test]
fn leading_zeros_are_refused() {
    // Arrange
    let cases = ["00", "01", "000", "0000000001"];

    // Act / Assert
    for raw in cases {
        assert_eq!(
            parse_units(raw),
            Err(MalformedUnits::LeadingZero),
            "leading-zero value {raw}"
        );
    }
}

#[test]
fn signs_whitespace_and_fractions_are_refused() {
    // Arrange
    let cases = [
        "+1", "-1", " 1", "1 ", "\t1", "1\n", "1.0", "1,0", "0x1", "1e3", "one", "1_0", "\u{661}",
    ];

    // Act / Assert
    for raw in cases {
        assert_eq!(
            parse_units(raw),
            Err(MalformedUnits::NonDigit),
            "non-digit value {raw:?}"
        );
    }
}

#[test]
fn values_above_the_unit_ceiling_are_refused_as_overflow() {
    // Arrange
    let cases = ["4294967296", "4294967300", "99999999999999999999"];

    // Act / Assert
    for raw in cases {
        assert_eq!(
            parse_units(raw),
            Err(MalformedUnits::Overflow),
            "overflowing value {raw}"
        );
    }
}

#[test]
fn no_refusal_class_reads_as_a_committed_count() {
    // Arrange
    let malformed = ["", "00", "-1", " 1", "1.0", "4294967296", "nan"];

    // Act / Assert
    for raw in malformed {
        assert!(
            parse_units(raw).is_err(),
            "malformed value {raw:?} must not parse to a count"
        );
    }
}

include!("paid_probe_reserve_tests.rs");
