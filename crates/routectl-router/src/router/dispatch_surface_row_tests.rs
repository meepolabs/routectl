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
