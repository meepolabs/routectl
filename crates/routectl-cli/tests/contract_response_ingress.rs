//! Contract tests for the response-side ingress layer.
//!
//! Each scenario takes a canonical `ChatResponse` from
//! `common::scenarios` and asserts the wire body that an
//! `IngressAdapter::render_response` implementation produces. This is
//! the response-side analog of the request-side `contract_ingress`
//! tests in the sibling file.
//!
//! See the sibling `contract_response_egress` tests in
//! `routectl-providers` for the upstream-wire-to-canonical half. The
//! per-scenario builders are mirrored across both crates; see
//! `common::mod.rs` for the mirror-sync note.
//!
//! Scope: only the Anthropic ingress's `render_response` is exercised
//! here. The OpenAI ingress's `render_response` is canonical
//! pass-through (the canonical `ChatResponse` IS the OpenAI wire
//! shape) so it carries no translation logic worth pinning.

mod common;

use routectl_cli::ingress::IngressAdapter;
use routectl_cli::ingress::anthropic::AnthropicIngress;

use common::scenarios;

// =====================================================================
// Scenario 4: stop_reason_round_trip
// =====================================================================
//
// Response-side scenario: feed a canonical `ChatResponse` through the
// Anthropic ingress's `render_response` and assert the wire shape
// `stop_reason` survives round-trip for both the OpenAI-mapped value
// (`stop` -> `end_turn`) and the Anthropic-only passthrough
// (`pause_turn`). Only Anthropic ingress is tested because
// openai-compat does not have these stop reasons. Bug class caught:
// Anthropic-only stop reasons clobbered to `end_turn`.

#[test]
fn ingress_anthropic_render_stop_reason_end_turn() {
    let resp = scenarios::scenario_4_response_stop_reason_end_turn();

    let wire = AnthropicIngress
        .render_response(resp)
        .expect("anthropic ingress render");

    // Explicit pin on the bug class (B/K) BEFORE the snapshot so a
    // regression surfaces with a clear failure message rather than
    // a generic snapshot diff. Snapshot below covers the rest of
    // the wire body so additions / drops to unasserted fields also
    // fail loudly.
    assert_eq!(wire["stop_reason"], "end_turn");
    insta::with_settings!({snapshot_path => "snapshots/anthropic"}, {
        insta::assert_json_snapshot!("scenario_4_render_stop_reason_end_turn", wire);
    });
}

#[test]
fn ingress_anthropic_render_stop_reason_pause_turn() {
    let resp = scenarios::scenario_4_response_stop_reason_pause_turn();

    let wire = AnthropicIngress
        .render_response(resp)
        .expect("anthropic ingress render");

    // Anthropic-only stop reasons must passthrough verbatim --
    // they must NOT be clobbered to `end_turn`. Pre-fix the
    // legacy mapping would lose `pause_turn`, breaking
    // claude-code's per-stop-reason error handling.
    assert_eq!(
        wire["stop_reason"], "pause_turn",
        "pause_turn must passthrough verbatim, not clobber to end_turn"
    );
    insta::with_settings!({snapshot_path => "snapshots/anthropic"}, {
        insta::assert_json_snapshot!("scenario_4_render_stop_reason_pause_turn", wire);
    });
}

// =====================================================================
// Scenario 11: matched_stop_sequence_round_trip
// =====================================================================
//
// Response-side: canonical `Choice.matched_stop_sequence` MUST render
// as wire `stop_reason:"stop_sequence"` + `stop_sequence:"<value>"`,
// overriding the lossy `finish_reason -> stop_reason` mapping that
// would otherwise emit `end_turn`. This closes the seam that broke
// claude-code structured-output flows: SDK-configured stop_sequence
// (the JSON fence) hit by a non-Anthropic backend (deepseek-v4-pro
// via openai-compat) rendered with `stop_reason:"end_turn"`, which
// the CLI couldn't reconcile and synthesized a `<synthetic>` wrap-up
// message flagged `is_error: true`. Real $-impact failure
// (2026-05-19 reviewer flow).

#[test]
fn ingress_anthropic_render_matched_stop_sequence() {
    let resp = scenarios::scenario_11_response_matched_stop_sequence();

    let wire = AnthropicIngress
        .render_response(resp)
        .expect("anthropic ingress render");

    // The two assertions pin the wire-shape contract that closes
    // the bug class: stop_reason MUST be "stop_sequence" (not the
    // lossy "end_turn") and stop_sequence MUST carry the matched
    // marker.
    assert_eq!(
        wire["stop_reason"], "stop_sequence",
        "matched_stop_sequence must override the canonical \"stop\" \
         -> wire \"end_turn\" mapping",
    );
    assert_eq!(
        wire["stop_sequence"], "</answer>",
        "the matched marker must surface on the wire so callers can \
         distinguish structured-output termination from natural end_turn",
    );
    insta::with_settings!({snapshot_path => "snapshots/anthropic"}, {
        insta::assert_json_snapshot!("scenario_11_render_matched_stop_sequence", wire);
    });
}
