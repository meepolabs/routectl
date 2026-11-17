//! Terminal-safe rendering of operator-written names.

use std::borrow::Cow;

use super::{MAX_PROVIDER_NAME_BYTES, render_operator_name};

fn has_terminal_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

#[test]
fn a_name_at_the_byte_ceiling_renders_unchanged() {
    let name = "p".repeat(MAX_PROVIDER_NAME_BYTES);

    let rendered = render_operator_name(&name);

    assert_eq!(rendered, name);
}

#[test]
fn a_name_over_the_byte_ceiling_renders_only_its_byte_count() {
    // Arrange: one byte over, built from a letter absent from the placeholder
    // wording so any echoed byte of the name shows up.
    let name = "q".repeat(MAX_PROVIDER_NAME_BYTES + 1);

    // Act
    let rendered = render_operator_name(&name);

    // Assert
    assert!(rendered.contains("129"), "{rendered}");
    assert!(!rendered.contains('q'), "{rendered}");
}

#[test]
fn the_byte_ceiling_counts_bytes_not_characters() {
    // 65 two-byte characters: 130 bytes, fewer than 128 characters.
    let name = "\u{e9}".repeat(MAX_PROVIDER_NAME_BYTES / 2 + 1);
    assert!(name.chars().count() <= MAX_PROVIDER_NAME_BYTES);

    let rendered = render_operator_name(&name);

    assert!(rendered.contains("130"), "{rendered}");
    assert!(!rendered.contains('\u{e9}'), "{rendered}");
}

#[test]
fn control_characters_and_escape_sequences_leave_no_control_byte() {
    let rows: &[(&str, &str, &str)] = &[
        ("newline", "work\nfake line", "workfake line"),
        ("carriage return", "work\rover", "workover"),
        ("tab", "work\tname", "workname"),
        ("delete", "work\u{7f}name", "workname"),
        ("c1 next-line", "work\u{85}name", "workname"),
        ("ansi color", "\u{1b}[31mwork\u{1b}[0m", "work"),
        ("c1 csi", "\u{9b}2Jwork", "work"),
        ("bare escape", "work\u{1b}name", "workname"),
    ];
    for (label, input, expected) in rows {
        let rendered = render_operator_name(input);

        assert!(!has_terminal_control(&rendered), "{label}: {rendered:?}");
        assert_eq!(rendered, *expected, "{label}");
    }
}

#[test]
fn printable_names_pass_through_unchanged() {
    let rows: &[(&str, &str)] = &[
        ("ascii with punctuation", "anthropic-work#seat_1 (eu)"),
        ("non-ascii", "caf\u{e9}-\u{65e5}\u{672c}"),
    ];
    for (label, input) in rows {
        let rendered = render_operator_name(input);

        assert_eq!(rendered, *input, "{label}");
    }
}

#[test]
fn a_clean_name_is_borrowed_and_a_dirty_one_is_owned() {
    assert!(matches!(
        render_operator_name("clean-name"),
        Cow::Borrowed(_)
    ));
    assert!(matches!(render_operator_name("dirty\nname"), Cow::Owned(_)));
}
