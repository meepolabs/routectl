use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

const MEMBERS: &[&str] = &["summarized", "omitted"];
const SHAPE_SLOT: ValueSlot = ValueSlot::Shape(MEMBERS);

#[test]
fn every_valid_value_round_trips_through_its_slot() {
    let cases = [
        (
            VerdictValue::Shape("summarized"),
            SHAPE_SLOT,
            "shape:summarized",
        ),
        (VerdictValue::Shape("omitted"), SHAPE_SLOT, "shape:omitted"),
        (VerdictValue::Ceiling(1), ValueSlot::Ceiling, "ceiling:1"),
        (
            VerdictValue::Ceiling(64_000),
            ValueSlot::Ceiling,
            "ceiling:64000",
        ),
        (
            VerdictValue::Ceiling(u32::MAX),
            ValueSlot::Ceiling,
            "ceiling:4294967295",
        ),
    ];
    for (value, slot, wire) in cases {
        let refused = AtomicU64::new(0);

        let encoded = value.encode();
        let decoded = decode_counted(&encoded, slot, &refused);

        assert_eq!(encoded, wire, "{value:?} encodes to its pinned spelling");
        assert_eq!(decoded, Some(value), "{wire} decodes back to {value:?}");
        assert_eq!(refused.load(Ordering::Relaxed), 0, "{wire} is not counted");
    }
}

/// Mutation check: make a `shape:` token decode to any value regardless of
/// membership -> red on `shape_non_member`.
#[test]
fn every_malformed_token_decodes_to_none_and_is_counted() {
    let cases: &[(&str, &str, ValueSlot)] = &[
        ("unknown_prefix", "width:summarized", SHAPE_SLOT),
        ("no_prefix", "summarized", SHAPE_SLOT),
        ("shape_non_member", "shape:verbose", SHAPE_SLOT),
        ("shape_empty_member", "shape:", SHAPE_SLOT),
        ("shape_member_case", "shape:Summarized", SHAPE_SLOT),
        ("shape_member_padded", "shape: summarized", SHAPE_SLOT),
        (
            "shape_on_ceiling_row",
            "shape:summarized",
            ValueSlot::Ceiling,
        ),
        ("ceiling_on_shape_row", "ceiling:100", SHAPE_SLOT),
        ("ceiling_non_numeric", "ceiling:abc", ValueSlot::Ceiling),
        ("ceiling_empty", "ceiling:", ValueSlot::Ceiling),
        ("ceiling_signed", "ceiling:+5", ValueSlot::Ceiling),
        ("ceiling_negative", "ceiling:-5", ValueSlot::Ceiling),
        ("ceiling_leading_zero", "ceiling:0100", ValueSlot::Ceiling),
        ("ceiling_zero", "ceiling:0", ValueSlot::Ceiling),
        ("ceiling_over_u32", "ceiling:4294967296", ValueSlot::Ceiling),
        (
            "ceiling_far_over",
            "ceiling:99999999999999999999",
            ValueSlot::Ceiling,
        ),
        ("empty", "", SHAPE_SLOT),
        ("non_ascii_shape", "shape:summar\u{0131}zed", SHAPE_SLOT),
        (
            "non_ascii_ceiling",
            "ceiling:\u{0661}\u{0662}",
            ValueSlot::Ceiling,
        ),
    ];
    for &(name, token, slot) in cases {
        let refused = AtomicU64::new(0);

        let decoded = decode_counted(token, slot, &refused);

        assert_eq!(decoded, None, "{name}: {token:?} must not decode");
        assert_eq!(
            refused.load(Ordering::Relaxed),
            1,
            "{name}: a refused token is counted once"
        );
    }
}

#[test]
fn a_decoded_shape_is_the_row_member_not_the_stored_text() {
    let token = String::from("shape:omitted");

    let decoded = VerdictValue::decode(&token, SHAPE_SLOT);

    let Some(VerdictValue::Shape(shape)) = decoded else {
        panic!("a member decodes: {decoded:?}");
    };
    assert!(
        std::ptr::eq(shape, MEMBERS[1]),
        "the decoded value borrows the code-owned member"
    );
}

#[test]
fn the_public_decode_bumps_the_process_counter() {
    let before = VerdictValue::malformed_total();

    let decoded = VerdictValue::decode("ceiling:abc", ValueSlot::Ceiling);

    assert_eq!(decoded, None);
    assert!(
        VerdictValue::malformed_total() > before,
        "the process-wide counter moved"
    );
}
