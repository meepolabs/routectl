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

use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::IngressAdapter;

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
