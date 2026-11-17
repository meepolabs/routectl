//! The RFC 3339 text every panel's `as_of` carries, pinned per instant.

use super::*;

/// UTC, whole seconds (sub-second digits truncated), `Z` suffix. Rows were
/// recorded from the formatter this module used before.
#[test]
fn utc_rfc3339_renders_each_pinned_instant() {
    let rows: &[(&str, i64, &str)] = &[
        ("epoch", 0, "1970-01-01T00:00:00Z"),
        ("before the epoch", -1, "1969-12-31T23:59:59Z"),
        (
            "milliseconds truncate",
            1_779_000_000_123,
            "2026-05-17T06:40:00Z",
        ),
        (
            "milliseconds never round up",
            1_800_000_000_999,
            "2027-01-15T08:00:00Z",
        ),
        ("mid-year", 1_783_134_309_000, "2026-07-04T03:05:09Z"),
        (
            "last four-digit year",
            253_402_300_799_000,
            "9999-12-31T23:59:59Z",
        ),
        ("year zero", -62_167_219_200_000, "0000-01-01T00:00:00Z"),
        (
            "negative year",
            -62_198_755_200_000,
            "-0001-01-01T00:00:00Z",
        ),
        (
            "first five-digit year",
            253_402_300_800_000,
            "+10000-01-01T00:00:00Z",
        ),
        (
            "five-digit negative year",
            -377_705_116_800_001,
            "-10000-12-31T23:59:59Z",
        ),
        (
            "last representable",
            8_210_266_876_799_000,
            "+262142-12-31T23:59:59Z",
        ),
        (
            "first representable",
            -8_334_601_228_800_000,
            "-262143-01-01T00:00:00Z",
        ),
        (
            "one past the last",
            8_210_266_876_800_000,
            "epoch ms 8210266876800000",
        ),
        (
            "one before the first",
            -8_334_601_228_800_001,
            "epoch ms -8334601228800001",
        ),
        ("unrepresentable", i64::MAX, "epoch ms 9223372036854775807"),
    ];
    for (name, ms, expected) in rows {
        assert_eq!(utc_rfc3339(*ms), *expected, "{name}");
    }
}
