// A rejection's typed value against the row it names: a rewrite edit reads
// only a shape, and a value of the other slot makes no edit.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

/// The ceiling a value-slot test names. Any nonzero `u32` would do.
const NAMED_CEILING: u32 = 4096;

fn named_on(
    path: &str,
    value: Option<super::super::rejection_parser::RejectionValue>,
) -> super::super::rejection_parser::NamedRejection {
    let row = super::closed_table()
        .iter()
        .find(|row| row.path == path)
        .unwrap_or_else(|| panic!("{path}: a closed-table row"));
    super::super::rejection_parser::NamedRejection {
        row,
        tier: routectl_core::capability::SignalTier::SelfIdentifying,
        kind: super::super::rejection_parser::RejectionKind::InvalidValue,
        value,
    }
}

/// Mutation check: make the rewrite arm accept a ceiling value -> red on
/// `rewrite_row_ceiling_value`.
#[test]
fn a_value_edits_only_the_row_whose_slot_it_fills() {
    use super::super::rejection_parser::RejectionValue::{Ceiling, Shape};
    let member = super::TEST_REWRITE_ACCEPTED[0];
    let cases = [
        (
            "rewrite_row_shape_value",
            super::TEST_REWRITE_PATH,
            Some(Shape(member)),
            Some(super::FieldEdit::Rewrite(member)),
        ),
        (
            "rewrite_row_ceiling_value",
            super::TEST_REWRITE_PATH,
            Some(Ceiling(NAMED_CEILING)),
            None,
        ),
        ("rewrite_row_no_value", super::TEST_REWRITE_PATH, None, None),
        (
            "ceiling_row_shape_value",
            super::TEST_CEILING_PATH,
            Some(Shape(member)),
            None,
        ),
        (
            "ceiling_row_ceiling_value",
            super::TEST_CEILING_PATH,
            Some(Ceiling(NAMED_CEILING)),
            Some(super::FieldEdit::Ceiling(NAMED_CEILING)),
        ),
        ("ceiling_row_no_value", super::TEST_CEILING_PATH, None, None),
    ];

    for (name, path, value, expected) in cases {
        let named = named_on(path, value);

        let edit = super::FieldEdit::for_rejection(named.row.transform, &named, 0);

        assert_eq!(edit, expected, "{name}");
    }
}

/// Mutation check: make the rewrite arm accept a ceiling value -> red (the
/// ceiling-valued rejection rewrites the field and the retry serves).
#[tokio::test]
async fn an_injected_ceiling_on_the_rewrite_row_repairs_nothing() {
    let (router, seats) = rewrite_chain(&[RewriteAnswer::Name(&super::TEST_REWRITE_ACCEPTED[..1])]);
    let _injection = provisional::inject_value(
        super::TEST_REWRITE_PATH,
        super::super::rejection_parser::RejectionValue::Ceiling(NAMED_CEILING),
    );

    let dispatched = router
        .complete_with_options(req_with_rewrite_field(), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_err(),
        "no repair, so the rejection reaches the client",
    );
    assert_eq!(seats[0].calls(), 1, "no repaired retry was dispatched");
    assert!(dispatched.meta.field_repairs.is_empty());
    assert!(dispatched.meta.learned_capabilities.is_empty());
}

/// The positive control for the ceiling case above: the same seat and seam,
/// carrying a shape, rewrite the field to it.
#[tokio::test]
async fn an_injected_shape_on_the_rewrite_row_rewrites_to_it() {
    let member = super::TEST_REWRITE_ACCEPTED[0];
    let (router, seats) = rewrite_chain(&[RewriteAnswer::Name(&super::TEST_REWRITE_ACCEPTED[..1])]);
    let _injection = provisional::inject_value(
        super::TEST_REWRITE_PATH,
        super::super::rejection_parser::RejectionValue::Shape(member),
    );

    let dispatched = router
        .complete_with_options(req_with_rewrite_field(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(
        seats[0].values(),
        vec![
            Some(CLIENT_REWRITE_VALUE.to_string()),
            Some(member.to_string()),
        ],
        "one carried attempt, then one retry carrying the injected shape",
    );
}
