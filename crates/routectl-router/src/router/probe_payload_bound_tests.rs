//! The retained probe payload is bounded at capture.
//!
//! A payload is taken at ACTIVATION, so whatever it retains is held for as
//! long as the job is tracked. `ProbePayload::new` enforces every bound: the
//! display value must be one of the three MODELED literals, and each beta
//! source is bounded by count, per-token bytes, and a combined total. A value
//! outside any of those skips activation rather than being truncated, stored,
//! or resent -- so total retained bytes are bounded by queue depth times the
//! largest acceptable payload.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use super::probe_test_support::{
    BodyAssertingProvider, GROUNDED_PATH, OkProvider, grounding_request, remote_router,
};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::{
    PROBE_BETA_MAX_COUNT_PER_SOURCE, PROBE_BETA_MAX_TOKEN_BYTES, PROBE_BETA_MAX_TOTAL_BYTES,
    PROBE_MODELED_DISPLAY_VALUES, PROBE_QUEUE_DEPTH, ProbePayload,
};

/// A display value no build models, spelled so the reason it is refused is the
/// value itself rather than its length. The vocabulary is the whole rule; there
/// is no separate byte ceiling for a long value to reach.
const UNMODELED_DISPLAY_VALUE: &str = "not-a-modeled-display-value";

#[tokio::test]
async fn an_unmodeled_display_value_skips_activation_and_is_never_retained() {
    // A client-supplied display token is caller-controlled text, and a probe
    // carrying an unmodeled value would ask about a shape the closed table
    // cannot repair -- while its settlement is still terminal for the
    // incarnation. Refusing at CAPTURE keeps it off the probe's wire body and
    // out of scheduler memory.
    let provider = Arc::new(BodyAssertingProvider {
        seen: parking_lot::Mutex::new(Vec::new()),
    });
    let router = remote_router(provider.clone());
    let mut admitted = grounding_request();
    admitted.routectl_internal.anthropic_thinking_display =
        Some(UNMODELED_DISPLAY_VALUE.to_string());
    assert!(
        !PROBE_MODELED_DISPLAY_VALUES.contains(&UNMODELED_DISPLAY_VALUE),
        "premise: the fixture value must be outside the modeled vocabulary"
    );

    let _ = router.complete(admitted).await;

    assert_eq!(
        router.probe_scheduler_snapshot().activations_total,
        0,
        "an unmodeled value must skip activation entirely"
    );
    assert_eq!(router.probe_scheduler_snapshot().queued, 0);
    router.run_due_probes().await;
    assert!(
        provider.seen.lock().is_empty(),
        "an unmodeled value must never reach a probe body"
    );
}

#[tokio::test]
async fn every_modeled_display_value_still_activates() {
    // Positive control for the refusal above, driven end to end: each modeled
    // token activates its lane, so the refusal is about the value rather than
    // about activation being broken.
    for token in PROBE_MODELED_DISPLAY_VALUES {
        let provider = Arc::new(OkProvider {
            count_calls: AtomicUsize::new(0),
        });
        let router = remote_router(provider);
        let mut admitted = grounding_request();
        admitted.routectl_internal.anthropic_thinking_display = Some(token.to_string());

        let _ = router.complete(admitted).await;

        assert_eq!(
            router.probe_scheduler_snapshot().activations_total,
            1,
            "the modeled token {token} must activate its lane"
        );
    }
}

fn payload(value: &str, client: &[String], operator: &[String]) -> Option<ProbePayload> {
    ProbePayload::new(GROUNDED_PATH, value.to_string(), client, operator, false)
}

fn tokens(n: usize, len: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("{}{i:04}", "x".repeat(len)))
        .collect()
}

#[test]
fn the_payload_constructor_refuses_every_unmodeled_display_value() {
    // ONE rule, the vocabulary, and the fields are private so no call site can
    // bypass it.
    //
    // It matters because the settlement a probe reaches is terminal for the
    // incarnation, so a probe carrying junk in this client-supplied field would
    // tombstone the lane's identity and stop every other client's traffic from
    // being probed. A SHORT unmodeled value is the case no size ceiling could
    // catch, which is why the vocabulary is the rule rather than a length.
    for junk in [
        "nonsense",
        "SUMMARIZED",
        "summarized ",
        "",
        "x",
        UNMODELED_DISPLAY_VALUE,
    ] {
        assert!(
            payload(junk, &[], &[]).is_none(),
            "an unmodeled display value must be refused: {junk:?}"
        );
    }

    // Each modeled token is a POSITIVE CONTROL: the refusal above must not be
    // "refuse everything".
    for token in PROBE_MODELED_DISPLAY_VALUES {
        assert!(
            payload(token, &[], &[]).is_some(),
            "the modeled token {token} must be accepted"
        );
    }
}

#[test]
fn each_beta_source_is_counted_against_its_own_ceiling() {
    // PER SOURCE, not shared: the two sets are stored separately and reapplied
    // to different carriers, so one must not consume the other's headroom.
    let at_bound = tokens(PROBE_BETA_MAX_COUNT_PER_SOURCE, 1);
    let over = tokens(PROBE_BETA_MAX_COUNT_PER_SOURCE + 1, 1);

    assert!(
        payload("summarized", &at_bound, &at_bound).is_some(),
        "both sources may independently sit AT the per-source count ceiling"
    );
    assert!(payload("summarized", &over, &[]).is_none());
    assert!(payload("summarized", &[], &over).is_none());
}

#[test]
fn the_combined_total_byte_bound_bites_where_the_per_source_bounds_pass() {
    // The combined bound is what makes the product not the real ceiling. Each
    // source here is inside its own count and per-token limits.
    let wide = tokens(
        PROBE_BETA_MAX_COUNT_PER_SOURCE,
        PROBE_BETA_MAX_TOKEN_BYTES - 5,
    );
    assert!(wide.len() <= PROBE_BETA_MAX_COUNT_PER_SOURCE);
    assert!(wide.iter().all(|b| b.len() <= PROBE_BETA_MAX_TOKEN_BYTES));
    let per_source_total: usize = wide.iter().map(String::len).sum();
    assert!(
        per_source_total > PROBE_BETA_MAX_TOTAL_BYTES,
        "premise: this fixture must exceed the COMBINED bound"
    );

    assert!(payload("summarized", &wide, &[]).is_none());
    assert!(payload("summarized", &[], &wide).is_none());
}

#[test]
fn a_per_token_oversize_is_refused_in_either_source() {
    let long = vec!["x".repeat(PROBE_BETA_MAX_TOKEN_BYTES + 1)];
    assert!(payload("summarized", &long, &[]).is_none());
    assert!(payload("summarized", &[], &long).is_none());
    // Positive control at exactly the per-token ceiling.
    let exact = vec!["x".repeat(PROBE_BETA_MAX_TOKEN_BYTES)];
    assert!(payload("summarized", &exact, &[]).is_some());
}

#[test]
fn an_unsafe_beta_token_is_refused_rather_than_sanitized() {
    // The egress joins betas into ONE comma-separated header, so a comma forges
    // additional flags and CR/LF forges a header. All are REFUSED: a rewritten
    // token is a different flag, and sending it would probe a beta context the
    // upstream never saw while attributing the answer to the field.
    for bad in [
        "a,b",
        "a\r\nx-evil: 1",
        "a\rb",
        "a\nb",
        "a\tb",
        "a\u{0}b",
        "",
        "   ",
    ] {
        assert!(
            payload("summarized", &[bad.to_string()], &[]).is_none(),
            "an unsafe client beta must be refused: {bad:?}"
        );
        assert!(
            payload("summarized", &[], &[bad.to_string()]).is_none(),
            "an unsafe operator beta must be refused: {bad:?}"
        );
    }
    // Positive control: surrounding whitespace is TRIMMED, not refused, and the
    // retained token is the trimmed one.
    let p = payload("summarized", &["  ctx-1m  ".to_string()], &[]).expect("trimmable");
    assert_eq!(p.client_betas(), ["ctx-1m"]);
}

#[test]
fn a_realistic_claude_code_beta_floor_is_never_refused() {
    // The bounds exist to reject adversarial shapes, so the pinned real floor
    // must pass. Without this, every bound above could be set to zero and the
    // negative tests would all still be green.
    let real: Vec<String> =
        routectl_core::identity::anthropic::default_claude_code_anthropic_betas()
            .iter()
            .map(|b| (*b).to_string())
            .collect();
    assert!(
        payload("summarized", &real, &real).is_some(),
        "the pinned Claude Code floor must stay within every bound, on both sources"
    );
}

#[test]
fn duplicate_beta_tokens_are_deduplicated_per_source() {
    let dup = vec![
        "ctx-1m".to_string(),
        " ctx-1m ".to_string(),
        "ctx-1m".to_string(),
    ];
    let p = payload("summarized", &dup, &dup).expect("duplicates are collapsed, not refused");
    assert_eq!(p.client_betas(), ["ctx-1m"]);
    assert_eq!(p.operator_betas(), ["ctx-1m"]);
}

#[test]
fn an_oversized_count_is_refused_before_any_per_token_work() {
    // The count bound is read off `len()` BEFORE per-token trimming and
    // validation, so a pathological input is refused after one length read.
    // Driven with a vector far past the bound whose every element would also be
    // individually invalid: the refusal must still be the count's.
    let huge: Vec<String> = (0..PROBE_BETA_MAX_COUNT_PER_SOURCE * 1000)
        .map(|_| "x".repeat(PROBE_BETA_MAX_TOKEN_BYTES + 10))
        .collect();
    assert!(payload("summarized", &huge, &[]).is_none());
}

#[tokio::test]
async fn retained_payload_bytes_are_bounded_by_depth_times_the_largest_payload() {
    // The memory bound as a whole: queue depth x the LARGEST ACCEPTABLE payload.
    // Each payload here carries the longest modeled value AND both beta sources
    // filled toward the combined byte ceiling, so the tracked-job bound is what
    // holds total retained bytes down.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);
    // The LONGEST modeled token. The value half of a payload is bounded by the
    // vocabulary (three fixed literals), so the worst case it can reach is the
    // longest of them -- the beta half is the only part a caller influences, and
    // the bounds on it are pinned by the tests above.
    let max_value = PROBE_MODELED_DISPLAY_VALUES
        .iter()
        .max_by_key(|t| t.len())
        .expect("the modeled vocabulary is non-empty")
        .to_string();
    // Split the combined ceiling across both sources, so the payload is at its
    // largest ACCEPTABLE size rather than one that would be refused.
    let per_token = PROBE_BETA_MAX_TOKEN_BYTES / 2;
    let count = (PROBE_BETA_MAX_TOTAL_BYTES / 2) / per_token;
    let client = tokens(count, per_token - 4);
    let operator = tokens(count, per_token - 4);

    let mut activated = 0usize;
    for n in 0..PROBE_QUEUE_DEPTH * 3 {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        let payload = ProbePayload::new(GROUNDED_PATH, max_value.clone(), &client, &operator, true)
            .expect("the maximum acceptable payload must construct");
        assert!(
            !payload.client_betas().is_empty() && !payload.operator_betas().is_empty(),
            "premise: the fixture must actually carry betas on both sources"
        );
        router.activate_probe_plan_for_tests(&key, payload);
        activated += 1;
    }
    assert!(
        activated > PROBE_QUEUE_DEPTH,
        "premise: more lanes than depth"
    );

    assert!(
        router.probe_scheduler_snapshot().queued <= PROBE_QUEUE_DEPTH,
        "tracked jobs -- and so retained payloads -- stay at the depth bound"
    );
}

#[test]
fn a_test_only_closed_table_row_can_be_neither_captured_nor_applied_by_a_probe() {
    // The probe path resolves against the GROUNDED table on both sides, so a
    // test-only row is unproduceable AND unconsumable. This matters because a
    // TEST build appends `TEST_FIELD_REPAIRS` to `closed_table()`: an
    // application routed through that table would accept a path no production
    // capture could ever have emitted, and the asymmetry would hide a real
    // mis-wiring behind a green suite.
    //
    // CONSUMPTION: a payload naming the test-only path sets nothing.
    let payload = ProbePayload::new(
        super::field_repair::TEST_PREFIX_IMPACTING_PATH,
        "summarized".to_string(),
        &[],
        &[],
        false,
    )
    .expect("the payload constructor bounds VALUES, not paths");
    let mut req = routectl_core::ChatRequest::default();
    let before = serde_json::to_value(&req).expect("serializable");
    super::field_repair::apply_probe_payload(&mut req, &payload);
    assert_eq!(
        serde_json::to_value(&req).expect("serializable"),
        before,
        "a test-only path must set nothing on a probe body"
    );
    // The serialized comparison cannot see `routectl_internal` (the whole
    // carrier is `#[serde(skip)]`), so the two internal carriers a probe could
    // possibly touch are checked directly.
    assert!(req.routectl_internal.anthropic_thinking_display.is_none());
    assert!(req.system.is_none());

    // And the POSITIVE CONTROL on the same call: the grounded path DOES apply,
    // so the silence above is about the path rather than about
    // `apply_probe_payload` being inert.
    let grounded = ProbePayload::new(GROUNDED_PATH, "summarized".to_string(), &[], &[], false)
        .expect("grounded payload");
    let mut req = routectl_core::ChatRequest::default();
    super::field_repair::apply_probe_payload(&mut req, &grounded);
    assert_eq!(
        req.routectl_internal.anthropic_thinking_display.as_deref(),
        Some("summarized"),
        "the grounded path must still apply"
    );

    // CAPTURE: a request carrying ONLY the test-only surface grounds no probe
    // payload, even in this test build where the row is in `closed_table()`.
    let test_row_only = routectl_core::ChatRequest {
        system: Some(routectl_core::SystemContent::Text(
            super::field_repair::TEST_PREFIX_SENTINEL.to_string(),
        )),
        ..Default::default()
    };
    assert!(
        super::field_repair::present_rows(&test_row_only)
            .any(|row| row.path == super::field_repair::TEST_PREFIX_IMPACTING_PATH),
        "premise: the test row must actually be present in a test build, or \
         this proves nothing"
    );
    assert_eq!(
        super::field_repair::grounded_closed_table_payloads(&test_row_only).count(),
        0,
        "a test-only row must ground no probe payload"
    );
}
