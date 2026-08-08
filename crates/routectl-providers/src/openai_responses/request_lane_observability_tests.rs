// Multi-turn reasoning replay, the max_tokens lane contract, and the
// drop-observability pins (cache_control and canonical sampling knobs).
// `include!`d into `request_tests.rs`; all top-level imports live there,
// so do not add `use` lines here.
//
// Holds the `max_output_tokens` lane pair --
// `openai_responses_forwards_caller_max_tokens_as_max_output_tokens` (ApiKey,
// via `cfg_api_key()`) against
// `openai_responses_omits_max_output_tokens_on_codex_lane` (ChatgptOauth, via
// `cfg()`). Those two must stay together and adjacent: separated, the codex
// lane's negative assertions no longer read against the lane that does emit
// the field, and the pin goes vacuous.

// ---------------------------------------------------------------------------
// multi-turn reasoning replay round-trip
//
// These tests prove that an assistant turn carrying response-side
// reasoning_details (tagged with the emitting lane's format tag) survives
// the second-turn request translation: the encrypted_content signature
// the upstream issued must reach the next /responses POST verbatim or
// Anthropic/OpenAI reasoning-enabled models return 400.
// ---------------------------------------------------------------------------

#[test]
fn response_reasoning_round_trips_through_canonical_to_replay_request() {
    use crate::openai_responses::response;
    use crate::openai_responses::response_types::ResponsesResponse;

    // Arrange: a fake upstream response carrying a Reasoning output
    // item with a signature + summary + inner text. Drive it through
    // the response translator to a canonical ChatResponse, then build
    // a new request whose message[1] is the translated assistant turn
    // and assert the egress emits a Reasoning input item carrying the
    // original encrypted_content + id.
    let upstream_body = json!({
        "id": "resp_01",
        "status": "completed",
        "model": "gpt-5-codex",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "step"}],
                "content": [{"type": "reasoning_text", "text": "detail"}],
                "encrypted_content": "sig_xyz"
            },
            {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "answer"}]
            }
        ]
    });
    let typed: ResponsesResponse = from_value(upstream_body).unwrap();
    let chat_response = response::translate(
        "test",
        crate::openai_responses::AuthKind::ChatgptOauth,
        typed,
    )
    .unwrap();
    let assistant_msg = chat_response.choices[0].message.clone();

    // Act: build a fresh request whose second message is the assistant
    // turn from the upstream response. The egress must lift
    // reasoning_details back into a Reasoning input item.
    let req = req_with(vec![user_text("ping"), assistant_msg]);
    let v = translate_to_json(&cfg(), &req);

    // Assert: input[1] is a Reasoning item carrying the original
    // signature, id, and summary/content surfaces.
    let reasoning = &v["input"][1];
    assert_eq!(reasoning["type"], "reasoning");
    assert_eq!(reasoning["id"], "rs_1");
    assert_eq!(reasoning["encrypted_content"], "sig_xyz");
    assert_eq!(
        reasoning["summary"],
        json!([{"type": "summary_text", "text": "step"}])
    );
    assert_eq!(
        reasoning["content"],
        json!([{"type": "reasoning_text", "text": "detail"}])
    );
    // input[2] is the assistant message text (no Thinking duplication
    // because reasoning_details produced the Reasoning item).
    let msg = &v["input"][2];
    assert_eq!(msg["type"], "message");
    assert_eq!(msg["role"], "assistant");
    assert_eq!(
        msg["content"],
        json!([{"type": "output_text", "text": "answer"}])
    );
}

#[test]
fn sse_reasoning_round_trips_through_canonical_to_replay_request() {
    use crate::openai_responses::sse::ResponsesStreamState;
    use routectl_core::{ReasoningDetail, ReasoningDetailKind};

    // Arrange: synthesize a streaming session by feeding events
    // through parse_event, then collect the emitted reasoning_details
    // into a synthetic assistant message. The replay request must then
    // carry the same encrypted_content signature.
    let events = vec![
        json!({"type": "response.created", "response": {"id": "r", "model": "m"}}),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_1", "summary": []}
        }),
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "summary_index": 0,
            "delta": "step"
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_1", "summary": [],
                     "encrypted_content": "sig_xyz"}
        }),
    ];
    let mut state = ResponsesStreamState::default();
    let mut all_details: Vec<ReasoningDetail> = Vec::new();
    for ev in events {
        let typed = serde_json::from_value(ev).unwrap();
        for chunk in state.parse_event("test", typed).unwrap() {
            all_details.extend(chunk.choices[0].delta.reasoning_details.clone());
        }
    }
    // Sanity: at least one Encrypted detail with the upstream id.
    let enc = all_details
        .iter()
        .find(|d| matches!(d.kind, ReasoningDetailKind::Encrypted))
        .expect("Encrypted detail emitted");
    assert_eq!(enc.id.as_deref(), Some("rs_1"));
    assert_eq!(enc.payload["encrypted_content"], "sig_xyz");

    // Promote the accumulated details onto a synthetic assistant
    // message and drive translate_request to assert the encrypted_content
    // reaches the egress wire body.
    let assistant = Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text("answer".into()),
        reasoning: None,
        reasoning_details: all_details,
        name: None,
        tool_call_id: None,
        tool_calls: None,
    };
    let req = req_with(vec![user_text("ping"), assistant]);
    let v = translate_to_json(&cfg(), &req);
    let reasoning = &v["input"][1];
    assert_eq!(reasoning["type"], "reasoning");
    assert_eq!(reasoning["id"], "rs_1");
    assert_eq!(reasoning["encrypted_content"], "sig_xyz");
}

// ---------------------------------------------------------------------------
// v0.8 max_tokens contract: forward what the caller sent, inject nothing
// ---------------------------------------------------------------------------

/// A caller-supplied ceiling must reach the wire under the Responses
/// API's own field name on the ApiKey lane, where `max_output_tokens`
/// is a documented top-level field. Forwarding is not injection: the
/// good-translator principle forbids synthesizing a value, not honoring
/// one.
#[test]
fn openai_responses_forwards_caller_max_tokens_as_max_output_tokens() {
    let mut req = req_with(vec![user_text("hi")]);
    req.max_tokens = Some(500);

    let v = translate_to_json(&cfg_api_key(), &req);

    assert_eq!(
        v.get("max_output_tokens").and_then(Value::as_u64),
        Some(500),
        "caller max_tokens must reach the wire as max_output_tokens; got: {v}"
    );
    assert!(
        v.get("max_tokens").is_none(),
        "the Responses wire field is max_output_tokens, not max_tokens; got: {v}"
    );
}

/// The codex OAuth lane MUST NOT emit `max_output_tokens` even when the
/// caller supplies a ceiling: codex's `ResponsesApiRequest` has no such
/// member and the chatgpt.com backend rejects the contract drift. This
/// pins the lane split against `openai_responses_forwards_caller_max_tokens_as_max_output_tokens`
/// so the two lanes can never silently converge again.
#[test]
fn openai_responses_omits_max_output_tokens_on_codex_lane() {
    let mut req = req_with(vec![user_text("hi")]);
    req.max_tokens = Some(500);

    let v = translate_to_json(&cfg(), &req);

    assert!(
        v.get("max_output_tokens").is_none(),
        "codex OAuth lane must not emit max_output_tokens; got: {v}"
    );
    assert!(
        v.get("max_tokens").is_none(),
        "codex OAuth lane must not emit max_tokens either; got: {v}"
    );
}

/// The caller's value wins over the Anthropic-shape carrier, and the
/// carrier never contributes its own number to this lane.
#[test]
fn openai_responses_caller_max_tokens_wins_over_internal_carrier() {
    let mut req = req_with(vec![user_text("hi")]);
    req.max_tokens = Some(500);
    req.routectl_internal.max_output_tokens = 8000;

    let v = translate_to_json(&cfg_api_key(), &req);

    assert_eq!(
        v.get("max_output_tokens").and_then(Value::as_u64),
        Some(500),
        "caller-supplied ceiling must win over the router carrier; got: {v}"
    );
}

/// The openai-responses egress MUST NOT inject `max_tokens` when the
/// caller omits it. Mirrors the openai-compat negative-injection test
/// (`openai_compat_does_not_inject_max_tokens_when_caller_omitted`).
/// The good-translator principle: only inject where the upstream
/// demands it (Anthropic-shape egresses). The
/// `routectl_internal.max_output_tokens` carrier is Anthropic-shape
/// territory and must NOT leak onto the openai-responses wire body
/// regardless of its value.
#[test]
fn openai_responses_does_not_inject_max_tokens_when_caller_omitted() {
    let mut req = req_with(vec![user_text("hi")]);
    req.max_tokens = None;
    // Pin: even when the router carrier carries a non-zero value
    // (e.g. when a `[models.X].max_output_tokens` override sits on a
    // model that happens to route through openai-responses), the
    // egress must NOT lift it onto the wire body.
    req.routectl_internal.max_output_tokens = 8000;
    // Run on the ApiKey lane, where `max_output_tokens` IS an emittable
    // wire field -- so a synthesized baseline would actually surface here.
    // On the codex lane the field is gated off unconditionally, which
    // would make this non-injection assertion vacuous.
    let v = translate_to_json(&cfg_api_key(), &req);
    assert!(
        v.get("max_tokens").is_none(),
        "openai-responses egress must not inject max_tokens; got: {v}"
    );
    assert!(
        v.get("max_output_tokens").is_none(),
        "openai-responses egress must not inject max_output_tokens either; got: {v}"
    );
}

// ---------------------------------------------------------------------------
// dropped cache_control observability
//
// The Responses API has no prompt-cache breakpoint surface, so dropping
// caller `cache_control` markers is CORRECT; these tests pin that the
// drop is OBSERVABLE (a single WARN naming the surfaces) and that the
// wire body is UNCHANGED by the diagnostic. `system`-level markers are
// excluded here -- system.rs already logs that drop at DEBUG.
// ---------------------------------------------------------------------------

#[test]
fn dropped_surfaces_detects_top_level_marker() {
    // Arrange
    let mut req = req_with(vec![user_text("hi")]);
    req.cache_control = Some(CacheControl::ephemeral_5m());

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert_eq!(surfaces, vec!["top-level"]);
}

#[test]
fn dropped_surfaces_detects_per_part_marker() {
    // Arrange
    let req = req_with(vec![user_text_part_with_cc("hi")]);

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert_eq!(surfaces, vec!["messages"]);
}

#[test]
fn dropped_surfaces_detects_per_tool_marker() {
    // Arrange
    let mut req = req_with(vec![user_text("hi")]);
    req.tools = Some(vec![custom_tool_with_cc("calc")]);

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert_eq!(surfaces, vec!["tools"]);
}

#[test]
fn dropped_surfaces_excludes_system_already_logged_at_debug() {
    // Arrange: only a system-block marker. system.rs owns that DEBUG log,
    // so this helper must NOT re-report it (avoids a double-log).
    let mut req = req_with(vec![user_text("hi")]);
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "sys".into(),
        cache_control: Some(CacheControl::ephemeral_5m()),
        citations: None,
    }]));

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert!(
        surfaces.is_empty(),
        "system marker must not be re-reported: {surfaces:?}"
    );
}

#[test]
fn dropped_surfaces_empty_for_clean_request() {
    // Arrange: no markers anywhere.
    let req = req_with(vec![user_text("hi")]);

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert!(surfaces.is_empty());
}

#[traced_test]
#[test]
fn warn_fires_for_top_level_marker_and_wire_is_unchanged() {
    // Arrange: identical requests, one with a top-level marker.
    let clean = req_with(vec![user_text("hi")]);
    let mut hinted = req_with(vec![user_text("hi")]);
    hinted.cache_control = Some(CacheControl::ephemeral_5m());

    // Act
    let clean_wire = translate_to_json(&cfg(), &clean);
    let hinted_wire = translate_to_json(&cfg(), &hinted);

    // Assert: the diagnostic fired, names the surface, and the wire body
    // is byte-identical to the unhinted request (cache_control never rode
    // the Responses wire to begin with).
    assert!(
        logs_contain("cache_control dropped"),
        "drop diagnostic must fire for a top-level marker"
    );
    assert_eq!(clean_wire, hinted_wire);
    assert!(hinted_wire.get("cache_control").is_none());
}

#[traced_test]
#[test]
fn no_warn_for_clean_request() {
    // Arrange
    let req = req_with(vec![user_text("hi")]);

    // Act
    let _ = translate(&cfg(), &req).expect("translate");

    // Assert: a request with no caller markers emits no drop diagnostic.
    assert!(
        !logs_contain("cache_control dropped"),
        "no drop diagnostic should fire when no caller marker is present"
    );
}

// ---------------------------------------------------------------------------
// dropped canonical sampling knobs observability
//
// The Responses API models none of `n / seed / logprobs / top_logprobs /
// logit_bias / presence_penalty / frequency_penalty`, and they cannot ride
// through provider_extras (canonical keys). These tests pin that the drop
// is OBSERVABLE (one WARN naming the fields) and silent when unset.
// ---------------------------------------------------------------------------

#[traced_test]
#[test]
fn sampling_fields_warn_once_naming_dropped_fields() {
    // Arrange
    let mut req = req_with(vec![user_text("hi")]);
    req.n = Some(3);
    req.frequency_penalty = Some(0.7);

    // Act
    let wire = translate_to_json(&cfg(), &req);

    // Assert
    logs_assert(crate::sampling_drop_guard::test_support::exactly_one_sampling_warn);
    assert!(logs_contain("frequency_penalty"));
    assert!(wire.get("n").is_none(), "got: {wire}");
    assert!(wire.get("frequency_penalty").is_none(), "got: {wire}");
}

#[traced_test]
#[test]
fn no_sampling_warn_when_no_sampling_field_set() {
    // Arrange
    let req = req_with(vec![user_text("hi")]);

    // Act
    let _ = translate(&cfg(), &req).expect("translate");

    // Assert
    assert!(!logs_contain("sampling fields dropped"));
}
