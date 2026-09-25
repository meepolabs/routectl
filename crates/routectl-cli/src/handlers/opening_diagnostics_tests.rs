//! The persisted opening vocabulary: which keys a row gets, their JSON
//! types, and the terminal-provenance labels and vendor rule.

use std::time::{Duration, Instant};

use routectl_core::UsageInputSource;
use serde_json::{Map, Value, json};

use super::{OpeningDiagnostics, OpeningFacts, TerminalReport, key, terminal};

fn facts() -> OpeningFacts {
    OpeningFacts {
        source: "anchor",
        reason: "anchor_hit",
        input: Some(41_317),
        provisional: true,
        lane_switched: Some(false),
    }
}

fn entries_of(
    diagnostics: &OpeningDiagnostics,
    body_after: Option<Duration>,
) -> Map<String, Value> {
    let start = Instant::now();
    diagnostics
        .extra_entries(start, body_after.map(|after| start + after))
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

#[test]
fn an_opening_sent_to_the_client_records_typed_numeric_and_boolean_values() {
    // Arrange
    let start = Instant::now();
    let diagnostics = OpeningDiagnostics {
        opening: Some(facts()),
        upstream_first_event_at: Some(start + Duration::from_millis(40)),
        terminal: TerminalReport::Reported {
            input: 41_329,
            source: Some(UsageInputSource::ExplicitFinal),
            from_vendor: Some(false),
        },
    };

    // Act
    let entries: Map<String, Value> = diagnostics
        .extra_entries(start, Some(start + Duration::from_millis(2_600)))
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

    // Assert
    assert_eq!(
        Value::Object(entries),
        json!({
            "opening_present": true,
            "opening_source": "anchor",
            "opening_reason": "anchor_hit",
            "opening_input": 41_317,
            "opening_provisional": true,
            "opening_lane_switched": false,
            "opening_first_event_ms": 40,
            "opening_first_enqueue_ms": 2_600,
            "terminal_source": "explicit_final",
            "terminal_input": 41_329,
            "terminal_vendor_verified": false,
        })
    );
}

#[test]
fn no_chosen_opening_or_no_body_byte_is_explicitly_no_opening() {
    let chosen = OpeningDiagnostics {
        opening: Some(facts()),
        upstream_first_event_at: None,
        terminal: TerminalReport::Missing,
    };
    let unchosen = OpeningDiagnostics {
        opening: None,
        ..chosen
    };
    let only_marker = json!({"opening_present": false});

    assert_eq!(Value::Object(entries_of(&chosen, None)), only_marker);
    assert_eq!(
        Value::Object(entries_of(&unchosen, Some(Duration::from_millis(5)))),
        only_marker
    );
    // Positive control: the same opening with a body byte is present.
    assert_eq!(
        entries_of(&chosen, Some(Duration::ZERO))[key::OPENING_PRESENT],
        true
    );
}

#[test]
fn optional_facts_are_omitted_rather_than_written_as_null_or_zero() {
    let diagnostics = OpeningDiagnostics {
        opening: Some(OpeningFacts {
            input: None,
            lane_switched: None,
            ..facts()
        }),
        upstream_first_event_at: None,
        terminal: TerminalReport::Missing,
    };

    let entries = entries_of(&diagnostics, Some(Duration::ZERO));

    for absent in [
        key::OPENING_INPUT,
        key::OPENING_LANE_SWITCHED,
        key::OPENING_FIRST_EVENT_MS,
        key::TERMINAL_INPUT,
    ] {
        assert!(!entries.contains_key(absent), "{absent}: {entries:?}");
    }
    assert_eq!(entries[key::TERMINAL_SOURCE], terminal::MISSING);
}

#[test]
fn every_terminal_provenance_has_its_own_label() {
    let reported = |source| TerminalReport::Reported {
        input: 1,
        source,
        from_vendor: None,
    };
    let cases = [
        (TerminalReport::Missing, "missing"),
        (reported(None), "unmarked"),
        (
            reported(Some(UsageInputSource::ExplicitFinal)),
            "explicit_final",
        ),
        (
            reported(Some(UsageInputSource::VendorOpening)),
            "vendor_opening",
        ),
        (
            reported(Some(UsageInputSource::ProxyOpening)),
            "proxy_opening",
        ),
        (
            reported(Some(UsageInputSource::InterimCarry)),
            "interim_carry",
        ),
        (
            reported(Some(UsageInputSource::PartialFinal)),
            "partial_final",
        ),
    ];

    for (report, label) in cases {
        assert_eq!(report.label(), label);
    }
}

#[test]
fn only_a_vendor_measurement_is_verified_and_an_explicit_final_needs_a_vendor_endpoint() {
    let reported = |source, from_vendor| TerminalReport::Reported {
        input: 1,
        source: Some(source),
        from_vendor,
    };

    assert!(reported(UsageInputSource::VendorOpening, None).vendor_verified());
    assert!(reported(UsageInputSource::ExplicitFinal, Some(true)).vendor_verified());
    // An explicit report relayed by a compatible endpoint, or from a parser
    // that did not say where it read it, is not established.
    assert!(!reported(UsageInputSource::ExplicitFinal, Some(false)).vendor_verified());
    assert!(!reported(UsageInputSource::ExplicitFinal, None).vendor_verified());
    for source in [
        UsageInputSource::ProxyOpening,
        UsageInputSource::InterimCarry,
        UsageInputSource::PartialFinal,
    ] {
        assert!(
            !reported(source, Some(true)).vendor_verified(),
            "{source:?}"
        );
    }
    let unmarked = TerminalReport::Reported {
        input: 1,
        source: None,
        from_vendor: Some(true),
    };
    assert!(!unmarked.vendor_verified());
    assert!(!TerminalReport::Missing.vendor_verified());
}

#[test]
fn a_terminal_label_is_evidence_exactly_when_its_provenance_is() {
    let label_of = |source| {
        TerminalReport::Reported {
            input: 1,
            source,
            from_vendor: None,
        }
        .label()
    };
    for source in [
        UsageInputSource::ExplicitFinal,
        UsageInputSource::VendorOpening,
        UsageInputSource::ProxyOpening,
        UsageInputSource::InterimCarry,
        UsageInputSource::PartialFinal,
    ] {
        assert_eq!(
            super::is_terminal_evidence_label(label_of(Some(source))),
            source.is_terminal_evidence(),
            "{source:?}"
        );
    }
    // Positive control: the evidence set is not empty.
    assert!(super::is_terminal_evidence_label(terminal::EXPLICIT_FINAL));
    for label in [
        terminal::MISSING,
        terminal::UNMARKED,
        terminal::UNRECOGNIZED,
        "",
        "Explicit_Final",
    ] {
        assert!(!super::is_terminal_evidence_label(label), "{label}");
    }
}

#[test]
fn the_closed_label_sets_name_exactly_what_the_producer_writes() {
    use crate::ingress::anthropic::context_anchor::{MissReason, OpeningReason, OpeningSource};

    // Arrange: every label each producer path can write.
    let sources: Vec<&str> = [
        OpeningSource::Anchor.as_str(),
        OpeningSource::Calibrated.as_str(),
        OpeningSource::Raw.as_str(),
        super::source::UPSTREAM_WIRE,
        super::source::UPSTREAM_WIRE_UNVERIFIED,
    ]
    .to_vec();
    let reasons: Vec<&str> = [
        OpeningReason::AnchorHit.as_str(),
        OpeningReason::Unanchored.as_str(),
        OpeningReason::LaneUnresolved.as_str(),
        OpeningReason::AnchorMiss(MissReason::Cold).as_str(),
        OpeningReason::AnchorMiss(MissReason::LaneChanged).as_str(),
        OpeningReason::AnchorMiss(MissReason::GenerationChanged).as_str(),
        OpeningReason::AnchorMiss(MissReason::HistoryShrank).as_str(),
        OpeningReason::AnchorMiss(MissReason::PrefixChanged).as_str(),
        super::UPSTREAM_OPENER_REASON,
    ]
    .to_vec();
    let reported = |source| {
        TerminalReport::Reported {
            input: 1,
            source,
            from_vendor: None,
        }
        .label()
    };
    let terminals: Vec<&str> = vec![
        TerminalReport::Missing.label(),
        reported(None),
        reported(Some(UsageInputSource::ExplicitFinal)),
        reported(Some(UsageInputSource::VendorOpening)),
        reported(Some(UsageInputSource::ProxyOpening)),
        reported(Some(UsageInputSource::InterimCarry)),
        reported(Some(UsageInputSource::PartialFinal)),
        terminal::UNRECOGNIZED,
    ];

    // Assert: same members, no extras, no duplicates.
    fn set<'a>(v: &[&'a str]) -> std::collections::BTreeSet<&'a str> {
        v.iter().copied().collect()
    }
    assert_eq!(set(super::OPENING_SOURCE_LABELS), set(&sources));
    assert_eq!(set(super::OPENING_REASON_LABELS), set(&reasons));
    assert_eq!(set(super::TERMINAL_SOURCE_LABELS), set(&terminals));
    assert_eq!(super::OPENING_SOURCE_LABELS.len(), sources.len());
    assert_eq!(super::OPENING_REASON_LABELS.len(), reasons.len());
    assert_eq!(super::TERMINAL_SOURCE_LABELS.len(), terminals.len());
}

#[test]
fn every_key_the_producer_writes_is_in_the_key_list() {
    // Arrange: the fullest row the producer writes.
    let start = Instant::now();
    let diagnostics = OpeningDiagnostics {
        opening: Some(facts()),
        upstream_first_event_at: Some(start),
        terminal: TerminalReport::Reported {
            input: 1,
            source: Some(UsageInputSource::ExplicitFinal),
            from_vendor: Some(true),
        },
    };

    // Act
    let written: std::collections::BTreeSet<&str> = diagnostics
        .extra_entries(start, Some(start))
        .into_iter()
        .map(|(k, _)| k)
        .collect();

    // Assert
    let listed: std::collections::BTreeSet<&str> = key::ALL.iter().copied().collect();
    assert_eq!(written, listed);
}
