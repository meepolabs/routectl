//! The boot-time unresolved-entries WARN names provider entries through the
//! operator-name renderer.

use routectl_core::MAX_PROVIDER_NAME_BYTES;
use routectl_testkit::{CapturedEvent, capture_events};

use super::{ACTIVATION_EVENT_MSG, warn_unresolved_at_startup};

fn the_warn(entries: &[&str]) -> CapturedEvent {
    let events = capture_events(|| warn_unresolved_at_startup(entries));
    let mut warns = events
        .into_iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message.starts_with(ACTIVATION_EVENT_MSG));
    let warn = warns.next().expect("the unresolved WARN is emitted");
    assert!(warns.next().is_none());
    warn
}

#[test]
fn an_entry_name_over_the_byte_ceiling_is_withheld_from_the_warn() {
    let over = "p".repeat(MAX_PROVIDER_NAME_BYTES + 1);
    let at = "q".repeat(MAX_PROVIDER_NAME_BYTES);

    let warn = the_warn(&[&over, &at]);

    let entries = warn.field("entries").expect("the WARN names its entries");
    assert!(!entries.contains(&over), "{entries}");
    assert!(
        entries.contains(&format!("<{}-byte name withheld>", over.len())),
        "{entries}"
    );
    assert!(entries.contains(&at), "positive control: {entries}");
}

#[test]
fn an_entry_name_with_esc_and_newline_is_stripped_in_the_warn() {
    let warn = the_warn(&["bad\u{1b}[31m\nname"]);

    let entries = warn.field("entries").expect("the WARN names its entries");
    assert!(entries.contains("\"badname\""), "{entries}");
    assert!(!entries.chars().any(char::is_control), "{entries:?}");
}
