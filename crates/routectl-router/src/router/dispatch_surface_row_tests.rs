//! The boolean rows on `DispatchSurface`: one test per row, each pinning the
//! value every surface carries.

use super::class_observe::DispatchSurface;

const SURFACES: [DispatchSurface; 3] = [
    DispatchSurface::Complete,
    DispatchSurface::Stream,
    DispatchSurface::CountTokens,
];

fn row(read: fn(DispatchSurface) -> bool) -> Vec<(&'static str, bool)> {
    SURFACES.iter().map(|s| (s.as_str(), read(*s))).collect()
}

#[test]
fn a_strict_reject_walks_only_on_the_message_surfaces() {
    assert_eq!(
        row(DispatchSurface::strict_reject_walks),
        [
            ("complete", true),
            ("stream", true),
            ("count_tokens", false)
        ],
    );
}

#[test]
fn an_unknown_provider_walks_only_on_the_message_surfaces() {
    assert_eq!(
        row(DispatchSurface::unknown_provider_walks),
        [
            ("complete", true),
            ("stream", true),
            ("count_tokens", false)
        ],
    );
}

#[test]
fn a_health_error_walks_only_on_the_message_surfaces() {
    assert_eq!(
        row(DispatchSurface::health_error_walks),
        [
            ("complete", true),
            ("stream", true),
            ("count_tokens", false)
        ],
    );
}

#[test]
fn only_complete_sleeps_between_attempts() {
    assert_eq!(
        row(DispatchSurface::sleeps_between_attempts),
        [
            ("complete", true),
            ("stream", false),
            ("count_tokens", false)
        ],
    );
}

#[test]
fn every_surface_but_stream_records_success_on_commit() {
    assert_eq!(
        row(DispatchSurface::records_success_on_commit),
        [
            ("complete", true),
            ("stream", false),
            ("count_tokens", true)
        ],
    );
}

#[test]
fn only_the_message_surfaces_shape_an_attempt_as_a_message() {
    assert_eq!(
        row(DispatchSurface::shapes_message),
        [
            ("complete", true),
            ("stream", true),
            ("count_tokens", false)
        ],
    );
}

#[test]
fn only_the_message_surfaces_dedupe_learning_across_the_whole_request() {
    assert_eq!(
        row(DispatchSurface::dedupes_learning_per_request),
        [
            ("complete", true),
            ("stream", true),
            ("count_tokens", false)
        ],
    );
}

#[test]
fn only_the_message_surfaces_hold_a_recovered_error_as_the_last_error() {
    assert_eq!(
        row(DispatchSurface::holds_recovered_errors),
        [
            ("complete", true),
            ("stream", true),
            ("count_tokens", false)
        ],
    );
}

#[test]
fn a_breaker_or_rate_limit_gate_refusal_walks_only_on_the_message_surfaces() {
    assert_eq!(
        row(DispatchSurface::gate_refusal_walks),
        [
            ("complete", true),
            ("stream", true),
            ("count_tokens", false)
        ],
    );
}

#[test]
fn count_logs_its_own_gate_refusal_message() {
    let messages: Vec<_> = SURFACES
        .iter()
        .map(|s| (s.as_str(), s.gate_blocked_message()))
        .collect();

    assert_eq!(
        messages,
        [
            ("complete", "gate blocked"),
            ("stream", "gate blocked"),
            ("count_tokens", "count_tokens gate blocked")
        ],
    );
}
