// The Gemini payload-extras source boundary. No HTTP ingress speaks the
// Gemini dialect, so a request that arrived through one forwards only the
// operator's own `payload_extras`; a trusted library caller forwards its
// explicit `provider_extras`. Every assertion is on the serialized body the
// provider would ship, and on the log events the assembly emitted.
//
// Every test here that can move the `ingress_extra_withheld` counter carries
// the `gemini_ingress_extra_withheld` guard, and every one that can move the
// fingerprint counter also carries `gemini_client_fingerprint_stripped`, in
// ONE attribute: stacked attributes guard only one key.
//
// `include!`d into the `tests` module of `request.rs`; imports live there.

const TOKEN_TELL: &str = "tok-mcp-7Qz4";
const FUTURE_TELL: &str = "future-val-8w";
const SAFETY_ID_TELL: &str = "sid-3kd9";
const CACHE_KEY_TELL: &str = "pck-5mn1";
const CLIENT_SUBKEY_TELL: &str = "client-sub-2p";

/// The `(gemini, ingress_extra_withheld)` policy-action counter.
fn gemini_ingress_withheld_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == "gemini" && e.policy_class == "ingress_extra_withheld")
        .map_or(0, |e| e.action_count)
}

fn operator_safety_settings() -> Value {
    json!([{"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE"}])
}

fn mcp_servers_with_token() -> Value {
    json!([{
        "type": "url",
        "url": "https://mcp.example.com/sse",
        "name": "example",
        "authorization_token": TOKEN_TELL
    }])
}

/// A dispatched request as the router hands it to the egress: the full
/// union on `provider_extras`, the operator layer recorded separately.
fn dispatched(provenance: RequestProvenance, union: Value, operator: Option<Value>) -> ChatRequest {
    let mut req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![make_user("hi")].into(),
        provider_extras: Some(union),
        ..Default::default()
    };
    req.routectl_internal.provenance = provenance;
    req.routectl_internal.operator_payload_extras = operator.map(std::sync::Arc::new);
    req
}

/// The provider body, the events its assembly emitted, and both counter
/// deltas as `(ingress_extra_withheld, client_fingerprint_stripped)`.
fn body_events_and_deltas(
    req: &ChatRequest,
) -> (Value, Vec<routectl_testkit::CapturedEvent>, u64, u64) {
    let withheld_before = gemini_ingress_withheld_count();
    let fingerprint_before = gemini_fingerprint_strip_count();
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| {
        body = provider_body("gemini:test", req).expect("provider body builds");
    });
    let withheld = gemini_ingress_withheld_count() - withheld_before;
    let fingerprint = gemini_fingerprint_strip_count() - fingerprint_before;
    (body, events, withheld, fingerprint)
}

fn withheld_warns(
    events: &[routectl_testkit::CapturedEvent],
) -> Vec<&routectl_testkit::CapturedEvent> {
    events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message.contains("ingress extras"))
        .collect()
}

fn field<'a>(event: &'a routectl_testkit::CapturedEvent, name: &str) -> Option<&'a str> {
    event
        .fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn assert_no_event_contains(events: &[routectl_testkit::CapturedEvent], tell: &str) {
    assert!(
        events.iter().all(|e| !format!("{e:?}").contains(tell)),
        "no event may echo a withheld value ({tell}): {events:?}"
    );
}

// ---------------------------------------------------------------------------
// Ingress provenance: operator extras only
// ---------------------------------------------------------------------------

/// Single-source pin for the ingress-extras withhold: one credential-bearing
/// key, no metadata, no system content.
#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn anthropic_ingress_mcp_servers_never_reach_the_gemini_body_and_count_one_withhold() {
    // Arrange
    let req = dispatched(
        RequestProvenance::AnthropicIngress,
        json!({"mcp_servers": mcp_servers_with_token()}),
        None,
    );

    // Act
    let (body, events, withheld, fingerprint) = body_events_and_deltas(&req);

    // Assert
    let wire = rendered(&body);
    assert!(body.get("mcp_servers").is_none(), "{wire}");
    assert!(!wire.contains(TOKEN_TELL), "{wire}");
    assert_eq!(withheld, 1, "one request, one withhold");
    assert_eq!(fingerprint, 0, "no identity block was withheld");
    let warns = withheld_warns(&events);
    assert_eq!(warns.len(), 1, "one WARN per request: {events:?}");
    assert_eq!(field(warns[0], "keys"), Some("mcp_servers"));
    assert_eq!(field(warns[0], "count"), Some("1"));
    assert_no_event_contains(&events, TOKEN_TELL);
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn openai_ingress_future_and_identity_keys_are_withheld_beside_operator_safety_settings() {
    // Arrange
    let operator = json!({"safetySettings": operator_safety_settings()});
    let req = dispatched(
        RequestProvenance::OpenaiIngress,
        json!({
            "safetySettings": operator_safety_settings(),
            "safety_identifier": SAFETY_ID_TELL,
            "prompt_cache_key": CACHE_KEY_TELL,
            "some_future_knob": {"nested": FUTURE_TELL}
        }),
        Some(operator),
    );

    // Act
    let (body, events, withheld, fingerprint) = body_events_and_deltas(&req);

    // Assert
    let wire = rendered(&body);
    assert_eq!(body["safetySettings"], operator_safety_settings(), "{wire}");
    for tell in [SAFETY_ID_TELL, CACHE_KEY_TELL, FUTURE_TELL] {
        assert!(!wire.contains(tell), "{tell} reached the body: {wire}");
        assert_no_event_contains(&events, tell);
    }
    assert_eq!(withheld, 1, "three withheld keys, one request, one action");
    assert_eq!(fingerprint, 0);
    let warns = withheld_warns(&events);
    assert_eq!(warns.len(), 1, "{events:?}");
    assert_eq!(
        field(warns[0], "keys"),
        Some("prompt_cache_key,safety_identifier,some_future_knob")
    );
    assert_eq!(field(warns[0], "count"), Some("3"));
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn a_key_both_layers_set_forwards_only_the_operator_value_and_counts_the_client_part() {
    // Arrange: the dispatch layer deep-merged the operator's object over the
    // client's, so the union holds the client's sub-key too.
    let req = dispatched(
        RequestProvenance::AnthropicIngress,
        json!({"labels": {"team": "operator-team", "leak": CLIENT_SUBKEY_TELL}}),
        Some(json!({"labels": {"team": "operator-team"}})),
    );

    // Act
    let (body, events, withheld, _) = body_events_and_deltas(&req);

    // Assert
    assert_eq!(body["labels"], json!({"team": "operator-team"}));
    assert!(!rendered(&body).contains(CLIENT_SUBKEY_TELL));
    assert_eq!(
        withheld, 1,
        "the client contribution under the key was withheld"
    );
    assert_eq!(field(withheld_warns(&events)[0], "keys"), Some("labels"));
    assert_no_event_contains(&events, CLIENT_SUBKEY_TELL);
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn operator_only_extras_on_an_ingress_request_reach_the_wire_and_count_nothing() {
    // Arrange
    let operator = json!({"safetySettings": operator_safety_settings()});
    let req = dispatched(
        RequestProvenance::OpenaiIngress,
        operator.clone(),
        Some(operator),
    );

    // Act
    let (body, events, withheld, fingerprint) = body_events_and_deltas(&req);

    // Assert
    assert_eq!(body["safetySettings"], operator_safety_settings());
    assert_eq!(withheld, 0, "nothing client-sourced was withheld");
    assert_eq!(fingerprint, 0);
    assert!(withheld_warns(&events).is_empty(), "{events:?}");
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn an_ingress_request_with_no_extras_counts_nothing() {
    // Arrange
    let mut req = dispatched(RequestProvenance::AnthropicIngress, json!({}), None);
    req.provider_extras = None;

    // Act
    let (body, events, withheld, fingerprint) = body_events_and_deltas(&req);

    // Assert
    assert!(body.get("contents").is_some());
    assert_eq!(withheld, 0);
    assert_eq!(fingerprint, 0);
    assert!(withheld_warns(&events).is_empty(), "{events:?}");
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn ingress_metadata_counts_on_the_fingerprint_tally_not_the_extras_withhold() {
    // Arrange
    let req = dispatched(
        RequestProvenance::AnthropicIngress,
        json!({"metadata": {"user_id": METADATA_TELL}}),
        None,
    );

    // Act
    let (body, events, withheld, fingerprint) = body_events_and_deltas(&req);

    // Assert
    assert!(body.get("metadata").is_none());
    assert!(!rendered(&body).contains(METADATA_TELL));
    assert_eq!(
        fingerprint, 1,
        "the identity block is a fingerprint withhold"
    );
    assert_eq!(
        withheld, 0,
        "metadata is not double-counted as an extras withhold"
    );
    assert_no_event_contains(&events, METADATA_TELL);
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn operator_metadata_is_restored_on_an_ingress_request_while_the_client_part_is_withheld() {
    // Arrange
    let req = dispatched(
        RequestProvenance::OpenaiIngress,
        json!({
            "metadata": {"user_id": METADATA_TELL, "trace": "operator-set"},
            "mcp_servers": mcp_servers_with_token()
        }),
        Some(json!({"metadata": {"trace": "operator-set"}})),
    );

    // Act
    let (body, events, withheld, fingerprint) = body_events_and_deltas(&req);

    // Assert
    let wire = rendered(&body);
    assert_eq!(body["metadata"], json!({"trace": "operator-set"}), "{wire}");
    assert!(
        !wire.contains(METADATA_TELL) && !wire.contains(TOKEN_TELL),
        "{wire}"
    );
    assert_eq!(fingerprint, 1);
    assert_eq!(withheld, 1);
    assert_eq!(
        field(withheld_warns(&events)[0], "keys"),
        Some("mcp_servers")
    );
}

#[test]
#[serial_test::serial(
    gemini_client_fingerprint_stripped,
    gemini_ingress_extra_withheld,
    gemini_provider_extra_managed_key_conflict
)]
fn an_operator_managed_key_on_an_ingress_request_is_still_refused_and_counted() {
    // Arrange
    let operator = json!({"generationConfig": {"topK": 40}, "safetySettings": []});
    let req = dispatched(
        RequestProvenance::AnthropicIngress,
        operator.clone(),
        Some(operator),
    );
    let before = gemini_policy_action_count("provider_extra_managed_key_conflict");

    // Act
    let (body, _, withheld, _) = body_events_and_deltas(&req);

    // Assert
    let after = gemini_policy_action_count("provider_extra_managed_key_conflict");
    assert!(!rendered(&body).contains("topK"));
    assert_eq!(body["safetySettings"], json!([]));
    assert_eq!(
        after - before,
        1,
        "the managed-key guard still runs on operator extras"
    );
    assert_eq!(withheld, 0, "an operator key is refused, not withheld");
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn an_operator_key_removed_from_the_dispatched_union_is_not_reintroduced() {
    // Arrange: a dispatch-time strip removed the operator's key from the
    // attempt's union after the operator layer was recorded.
    let req = dispatched(
        RequestProvenance::AnthropicIngress,
        json!({"safetySettings": []}),
        Some(json!({"safetySettings": [], "context_management": {"edits": []}})),
    );

    // Act
    let (body, _, withheld, _) = body_events_and_deltas(&req);

    // Assert
    assert!(
        body.get("context_management").is_none(),
        "{}",
        rendered(&body)
    );
    assert_eq!(body["safetySettings"], json!([]));
    assert_eq!(withheld, 0);
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn withheld_key_names_are_sanitized_before_they_are_logged() {
    // Arrange
    let req = dispatched(
        RequestProvenance::OpenaiIngress,
        json!({"evil\nkey\u{1b}[31m": TOKEN_TELL}),
        None,
    );

    // Act
    let (body, events, withheld, _) = body_events_and_deltas(&req);

    // Assert
    assert!(!rendered(&body).contains(TOKEN_TELL));
    assert_eq!(withheld, 1);
    let keys = field(withheld_warns(&events)[0], "keys")
        .expect("keys field")
        .to_string();
    assert!(
        !keys.contains('\n') && !keys.contains('\u{1b}'),
        "a control character reached the log: {keys:?}"
    );
    assert_no_event_contains(&events, TOKEN_TELL);
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn the_boundary_leaves_the_callers_request_unmodified_across_attempts() {
    // Arrange
    let req = dispatched(
        RequestProvenance::AnthropicIngress,
        json!({"mcp_servers": mcp_servers_with_token(), "safetySettings": []}),
        Some(json!({"safetySettings": []})),
    );
    let before = serde_json::to_value(&req).expect("serialize");
    let operator_before = req.routectl_internal.operator_payload_extras.clone();

    // Act
    let first = provider_body("gemini:test", &req).expect("first attempt");
    let second = provider_body("gemini:test", &req).expect("second attempt");

    // Assert
    assert_eq!(serde_json::to_value(&req).expect("serialize"), before);
    assert_eq!(
        req.routectl_internal.operator_payload_extras,
        operator_before
    );
    assert_eq!(first, second, "a retried attempt emits the same body");
}

// ---------------------------------------------------------------------------
// Library provenance: explicit extras are trusted
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn library_callers_forward_arbitrary_extras_and_count_no_withhold() {
    // Arrange
    let req = dispatched(
        RequestProvenance::Library,
        json!({
            "mcp_servers": mcp_servers_with_token(),
            "some_future_knob": FUTURE_TELL,
            "safetySettings": operator_safety_settings()
        }),
        None,
    );

    // Act
    let (body, events, withheld, fingerprint) = body_events_and_deltas(&req);

    // Assert
    assert_eq!(body["mcp_servers"], mcp_servers_with_token());
    assert_eq!(body["some_future_knob"], json!(FUTURE_TELL));
    assert_eq!(body["safetySettings"], operator_safety_settings());
    assert_eq!(withheld, 0);
    assert_eq!(fingerprint, 0);
    assert!(withheld_warns(&events).is_empty(), "{events:?}");
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped, gemini_ingress_extra_withheld)]
fn library_metadata_is_still_withheld_and_counted() {
    // Arrange
    let req = dispatched(
        RequestProvenance::Library,
        json!({"metadata": {"user_id": METADATA_TELL}, "some_future_knob": FUTURE_TELL}),
        None,
    );

    // Act
    let (body, _, withheld, fingerprint) = body_events_and_deltas(&req);

    // Assert
    assert!(body.get("metadata").is_none());
    assert_eq!(body["some_future_knob"], json!(FUTURE_TELL));
    assert_eq!(fingerprint, 1);
    assert_eq!(withheld, 0);
}
