//! The store's log lines name the seat key escaped, so a credential label
//! carrying control bytes cannot forge or recolor a log line.

use super::*;

use crate::quota::key::seat_key_for_served_identity;
use crate::quota::meter::Correction;
use crate::quota::reading::WindowKey;

/// One line-emitting call: its event name and how to make the store emit it.
type LogRow<'a> = (&'static str, Box<dyn Fn(&QuotaStore) + 'a>);

const RAW_KEY: &str = "anthropic#a\u{1b}[31m\nforged=1";

/// The `seat` field of the one `event` line `act` emits.
fn logged_seat(event: &str, act: impl FnOnce()) -> String {
    let events = routectl_testkit::capture_events(act);
    let lines: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some(event))
        .collect();
    let [line] = lines.as_slice() else {
        panic!("one {event} line: {events:?}");
    };
    line.field("seat").expect("a seat field").to_string()
}

#[test]
fn each_seat_naming_line_escapes_control_bytes_in_the_key() {
    let key = seat_key_for_served_identity(RAW_KEY);
    let correction = Correction {
        key: WindowKey::new(18_000, None).expect("a curated length"),
        old_used: 0.9,
        new_used: 0.3,
    };
    let rows: [LogRow<'_>; 2] = [
        (
            "quota_correction_confirmed",
            Box::new(|store| store.record_correction(&key, &correction)),
        ),
        (
            "quota_observation_rejected",
            Box::new(|store| store.record_rejection(&key, RejectionReason::Contradictory)),
        ),
    ];
    for (event, act) in rows {
        let store = QuotaStore::default();

        let seat = logged_seat(event, || act(&store));

        assert_eq!(
            seat,
            routectl_core::sanitize_for_log(RAW_KEY),
            "row: {event}"
        );
        assert!(!seat.contains(['\u{1b}', '\n']), "row: {event}: {seat:?}");
    }
}
