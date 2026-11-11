// What the Bedrock Mantle surface withholds of the request members this hub
// does not model -- preserved input items, unmodeled tools, namespace
// containers and stamps, and members carried on messages and parts -- and
// what the first-party surfaces still forward.
//
// `include!`d into `request_tests.rs`; all top-level imports live there.

const WITHHELD_CLASS: &str = "unknown_member_withheld";

fn s3_url() -> String {
    ["s3:", "", "planted-bucket", "key.png"].join("/")
}

fn responses_ingress(mut req: ChatRequest) -> ChatRequest {
    req.routectl_internal.provenance = routectl_core::RequestProvenance::ResponsesIngress;
    req
}

fn with_passthrough(items: Vec<(usize, Value)>) -> ChatRequest {
    let mut req = responses_ingress(req_with(vec![user_text("hi"), user_text("again")]));
    req.routectl_internal.responses_input_passthrough = items
        .into_iter()
        .map(|(modeled_prefix, item)| ResponsesPassthroughItem {
            modeled_prefix,
            item,
        })
        .collect();
    req
}

fn agent_message_with_s3_source() -> Value {
    json!({"type": "agent_message", "author": "a", "recipient": "b",
           "content": [{"type": "input_text", "text": "hi"}],
           "source": {"type": "s3", "uri": s3_url()}})
}

fn computer_output_with_upper_case_image_url() -> Value {
    json!({"type": "computer_call_output", "call_id": "call_c",
           "output": {"type": "computer_screenshot", "IMAGE_URL": s3_url()}})
}

fn reviewed_agent_message() -> Value {
    json!({"type": "agent_message", "id": "am_1", "author": "a", "recipient": "b",
           "content": [{"type": "input_text", "text": "hand-off"}]})
}

fn remove(path: &str) -> DeclaredRequestEdit {
    (
        routectl_core::wire_check::EditSite::AuthSurfaceGate,
        path.to_owned(),
        routectl_core::wire_check::EditKind::Remove,
    )
}

fn add(path: &str) -> DeclaredRequestEdit {
    (
        routectl_core::wire_check::EditSite::AuthSurfaceGate,
        path.to_owned(),
        routectl_core::wire_check::EditKind::Add,
    )
}

/// Each unreviewed preserved item is absent from the Mantle wire, declared
/// as a Remove at its inbound `input[]` index, and the request counts once
/// however many it withheld; a reviewed item beside them still forwards.
/// The first-party surfaces forward every item as written.
#[test]
fn mantle_withholds_unreviewed_passthrough_items_declares_each_and_counts_once() {
    // Arrange
    let req = with_passthrough(vec![
        (1, agent_message_with_s3_source()),
        (1, reviewed_agent_message()),
        (2, computer_output_with_upper_case_image_url()),
    ]);
    let before = responses_drop_count(WITHHELD_CLASS);

    // Act
    let (wire, edits) = translate_declaring(&cfg_bedrock_mantle(), &req);

    // Assert
    assert_eq!(responses_drop_count(WITHHELD_CLASS) - before, 1, "{wire}");
    let input = wire["input"].as_array().expect("input array");
    assert_eq!(input.len(), 3, "{wire}");
    assert_eq!(input[1], reviewed_agent_message(), "{wire}");
    assert!(!wire.to_string().contains("planted-bucket"), "{wire}");
    for path in ["/input/1", "/input/4"] {
        assert!(edits.contains(&remove(path)), "{path}: {edits:?}");
    }
    assert!(
        !edits
            .iter()
            .any(|(_, _, kind)| *kind == routectl_core::wire_check::EditKind::Relocate),
        "a shift by withheld items alone is no move: {edits:?}"
    );
    for (surface, cfg) in [("api-key", cfg_api_key()), ("chatgpt-oauth", cfg())] {
        let wire = translate_to_json(&cfg, &req);
        let input = wire["input"].as_array().expect("input array");
        assert_eq!(input.len(), 5, "{surface}: {wire}");
        assert_eq!(input[1], agent_message_with_s3_source(), "{surface}");
        assert_eq!(
            input[4],
            computer_output_with_upper_case_image_url(),
            "{surface}"
        );
    }
}

/// Positive control: a request whose preserved items are all reviewed
/// forwards them on Mantle and counts nothing.
#[test]
fn mantle_forwards_reviewed_passthrough_items_uncounted() {
    // Arrange
    let shell = json!({"type": "local_shell_call", "call_id": "c1", "status": "completed",
                       "action": {"type": "exec", "command": ["ls"]}});
    let req = with_passthrough(vec![(1, reviewed_agent_message()), (2, shell.clone())]);

    // Act
    let (wire, edits) = crate::translation_drop_metrics::assert_lane_ran_uncounted(
        "openai-responses",
        WITHHELD_CLASS,
        || translate_declaring(&cfg_bedrock_mantle(), &req),
    );

    // Assert
    assert_eq!(wire["input"][1], reviewed_agent_message(), "{wire}");
    assert_eq!(wire["input"][3], shell, "{wire}");
    assert!(
        !edits.iter().any(|(_, path, _)| path == "/input/1"),
        "{edits:?}"
    );
}

/// Mantle receives the function tools alone: a `ToolDef::Other` and a
/// top-level namespace container are absent and declared at their client
/// index, the container's function member ships flat and is declared as an
/// Add, and the request counts once. The first-party surfaces send both.
#[test]
fn mantle_withholds_unmodeled_tools_and_namespace_containers_and_counts_once() {
    // Arrange
    let (req, container) = req_with_top_level_namespace();
    let req = responses_ingress(req);
    let before = responses_drop_count(WITHHELD_CLASS);

    // Act
    let (wire, edits) = translate_declaring(&cfg_bedrock_mantle(), &req);

    // Assert
    assert_eq!(responses_drop_count(WITHHELD_CLASS) - before, 1, "{wire}");
    let names: Vec<&str> = wire["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("function name"))
        .collect();
    assert_eq!(names, ["flat_one", "alpha_wait", "flat_two"], "{wire}");
    assert!(edits.contains(&remove("/tools/1")), "{edits:?}");
    assert!(edits.contains(&add("/tools/1")), "{edits:?}");
    let first_party = translate_to_json(&cfg_api_key(), &req);
    assert_eq!(first_party["tools"][1], container, "{first_party}");
}

/// A client `ToolDef::Other` beside a function tool: absent on Mantle and
/// declared at its own index, forwarded as written on the first-party
/// surfaces.
#[test]
fn mantle_withholds_a_client_unmodeled_tool_at_its_index() {
    // Arrange
    let hosted = json!({"type": "mcp", "server_label": "s", "server_url": s3_url()});
    let mut req = responses_ingress(req_with(vec![user_text("hi")]));
    req.tools = Some(vec![ToolDef::Other(hosted.clone()), ns_custom("calc")]);
    let before = responses_drop_count(WITHHELD_CLASS);

    // Act
    let (wire, edits) = translate_declaring(&cfg_bedrock_mantle(), &req);

    // Assert
    assert_eq!(responses_drop_count(WITHHELD_CLASS) - before, 1, "{wire}");
    assert_eq!(wire["tools"].as_array().map(Vec::len), Some(1), "{wire}");
    assert_eq!(wire["tools"][0]["name"], json!("calc"), "{wire}");
    assert!(edits.contains(&remove("/tools/0")), "{edits:?}");
    assert!(!edits.contains(&remove("/tools/1")), "{edits:?}");
    let first_party = translate_to_json(&cfg(), &req);
    assert_eq!(first_party["tools"][0], hosted, "{first_party}");
}

/// A call to a namespace member keeps no `namespace` on Mantle, which got no
/// container to resolve it against: declared and counted. The first-party
/// surfaces stamp it back.
#[test]
fn mantle_withholds_the_namespace_of_a_call_to_a_flattened_member() {
    // Arrange
    let (mut req, _) = req_with_top_level_namespace();
    req.tools = Some(vec![ns_custom("alpha_wait")]);
    req.routectl_internal.responses_namespaces.containers = Vec::new();
    let call = json!({"id": "call_1", "type": "function",
                      "function": {"name": "alpha_wait", "arguments": "{}"}});
    let mut assistant = assistant_text("");
    assistant.tool_calls = Some(vec![call]);
    req.messages = vec![user_text("hi"), assistant].into();
    let req = responses_ingress(req);
    let before = responses_drop_count(WITHHELD_CLASS);

    // Act
    let (wire, edits) = translate_declaring(&cfg_bedrock_mantle(), &req);

    // Assert
    assert_eq!(responses_drop_count(WITHHELD_CLASS) - before, 1, "{wire}");
    assert_eq!(wire["input"][1]["type"], json!("function_call"), "{wire}");
    assert!(wire["input"][1].get("namespace").is_none(), "{wire}");
    assert!(edits.contains(&remove("/input/*/namespace")), "{edits:?}");
    let first_party = translate_to_json(&cfg_api_key(), &req);
    assert_eq!(
        first_party["input"][1]["namespace"],
        json!("ns_alpha"),
        "{first_party}"
    );
}

/// Members carried on a message item and on its parts, including a
/// non-https url nested inside a part member, are absent from the Mantle
/// wire, each declared as a Remove, and the request counts once.
#[test]
fn mantle_withholds_message_and_part_members_declares_each_and_counts_once() {
    // Arrange
    let part = carried_text_part(
        "hi",
        members(&[(
            "annotations",
            json!([{"type": "url_citation", "url": s3_url()}]),
        )]),
    );
    let user = with_members(user_parts(vec![part]), members(&[("id", json!("msg_1"))]));
    let req = responses_request(vec![user]);
    let before = responses_drop_count(WITHHELD_CLASS);

    // Act
    let (wire, edits) = translate_declaring(&cfg_bedrock_mantle(), &req);

    // Assert
    assert_eq!(responses_drop_count(WITHHELD_CLASS) - before, 1, "{wire}");
    assert!(!wire.to_string().contains("planted-bucket"), "{wire}");
    assert!(wire["input"][0].get("id").is_none(), "{wire}");
    assert!(edits.contains(&remove("/input/*/id")), "{edits:?}");
    assert!(
        edits.contains(&remove("/input/*/content/*/annotations")),
        "{edits:?}"
    );
    let first_party = translate_to_json(&cfg(), &req);
    assert_eq!(
        first_party["input"][0]["content"][0]["annotations"][0]["url"],
        json!(s3_url()),
        "{first_party}"
    );
}

/// Every Mantle withhold in one request -- a top-level key, a preserved
/// item, a tool and a carried member -- counts the request once.
#[test]
fn every_mantle_withhold_in_one_request_counts_once() {
    // Arrange
    let mut req = with_passthrough(vec![(1, agent_message_with_s3_source())]);
    req.messages = vec![carried_user_turn()].into();
    req.tools = Some(vec![ToolDef::Other(json!({"type": "web_search"}))]);
    req.provider_extras = Some(json!({"safety_identifier": "u"}));
    let drops_before = responses_drop_count(WITHHELD_CLASS);
    let seen_before = responses_lane_seen_count();

    // Act
    let wire = translate_to_json(&cfg_bedrock_mantle(), &req);

    // Assert
    assert_eq!(
        responses_drop_count(WITHHELD_CLASS) - drops_before,
        1,
        "{wire}"
    );
    assert_eq!(responses_lane_seen_count() - seen_before, 1, "{wire}");
}
