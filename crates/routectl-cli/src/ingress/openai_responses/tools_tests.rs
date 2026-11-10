//! Namespace flattening at the Responses ingress, through the real request
//! parse, for both tool carriers.

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use routectl_core::schema::ResponsesNamespaceCollision;
use routectl_core::{ChatRequest, CustomTool, RequestObservation, ToolDef};
use serde_json::{Value, json};
use serial_test::serial;

use super::note_collisions_on_observation;

const COLLISION_CLASS: &str = "namespace_tool_name_collision";

fn parse(body: Value) -> ChatRequest {
    super::super::parse::translate_request(&HeaderMap::new(), body).expect("request parses")
}

fn collision_count() -> u64 {
    routectl_providers::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "openai-responses" && e.drop_class == COLLISION_CLASS)
        .map_or(0, |e| e.drop_count)
}

fn function(name: &str) -> Value {
    json!({
        "type": "function",
        "name": name,
        "description": format!("{name} description"),
        "parameters": {"type": "object", "properties": {"arg": {"type": "string"}}},
        "strict": false
    })
}

/// Shaped like the containers a Codex client sends: one namespace holding a
/// freeform grammar tool beside functions, and a second holding functions.
fn codex_shaped_containers() -> Vec<Value> {
    vec![
        json!({
            "type": "namespace",
            "name": "ns_alpha",
            "description": "alpha tools",
            "tools": [
                {"type": "custom", "name": "exec", "description": "run", "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}},
                function("alpha_wait"),
                function("alpha_send")
            ]
        }),
        json!({
            "type": "namespace",
            "name": "ns_beta",
            "description": "beta tools",
            "tools": [function("beta_read"), function("beta_list")]
        }),
    ]
}

#[derive(Clone, Copy, Debug)]
enum Carrier {
    TopLevel,
    AdditionalTools,
}

fn body_with(carrier: Carrier, tools: Vec<Value>) -> Value {
    let user = json!({"type": "message", "role": "user", "content": "hi"});
    match carrier {
        Carrier::TopLevel => json!({"model": "m", "tools": tools, "input": [user]}),
        Carrier::AdditionalTools => json!({
            "model": "m",
            "input": [{"type": "additional_tools", "role": "developer", "tools": tools}, user]
        }),
    }
}

fn names(tools: &[ToolDef]) -> Vec<String> {
    tools
        .iter()
        .map(|t| match t {
            ToolDef::Custom(c) => c.name.clone(),
            ToolDef::Other(v) => v["name"].as_str().unwrap_or("<unnamed>").to_owned(),
        })
        .collect()
}

#[test]
fn codex_shaped_namespaces_flatten_to_bare_members_through_both_carriers() {
    for carrier in [Carrier::TopLevel, Carrier::AdditionalTools] {
        // Arrange
        let body = body_with(carrier, codex_shaped_containers());

        // Act
        let req = parse(body);

        // Assert
        let tools = req.tools.expect("members are canonical");
        assert_eq!(
            names(&tools),
            ["exec", "alpha_wait", "alpha_send", "beta_read", "beta_list"],
            "{carrier:?}"
        );
        assert!(
            matches!(&tools[0], ToolDef::Other(v) if v["type"] == "custom"),
            "{carrier:?}: the grammar tool rides as Other: {tools:?}"
        );
        for tool in &tools[1..] {
            let ToolDef::Custom(c) = tool else {
                panic!("{carrier:?}: a nested function must be Custom: {tool:?}");
            };
            let raw = function(&c.name);
            let expected = CustomTool::from_responses_function(&raw).expect("normalizes");
            assert_eq!(
                serde_json::to_value(c).unwrap(),
                serde_json::to_value(&expected).unwrap(),
                "{carrier:?}"
            );
        }
        let expected_map: BTreeMap<String, String> = [
            ("exec", "ns_alpha"),
            ("alpha_wait", "ns_alpha"),
            ("alpha_send", "ns_alpha"),
            ("beta_read", "ns_beta"),
            ("beta_list", "ns_beta"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let namespaces = &req.routectl_internal.responses_namespaces;
        assert_eq!(namespaces.members, expected_map, "{carrier:?}");
        assert!(namespaces.collisions.is_empty(), "{carrier:?}");
        let rendered = serde_json::to_string(&tools).unwrap();
        assert!(!rendered.contains("namespace"), "{carrier:?}: {rendered}");
        assert!(!rendered.contains("ns_alpha"), "{carrier:?}: {rendered}");
    }
}

#[test]
fn only_top_level_containers_are_recorded_for_the_splice_with_their_index() {
    // Arrange
    let containers = codex_shaped_containers();
    let top = json!({
        "model": "m",
        "tools": [function("flat_one"), containers[0].clone(), function("flat_two")],
        "input": [{"type": "additional_tools", "role": "developer", "tools": [containers[1].clone()]}]
    });

    // Act
    let req = parse(top);

    // Assert
    let recorded = &req.routectl_internal.responses_namespaces.containers;
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].index, 1);
    assert_eq!(recorded[0].raw, containers[0]);
    assert_eq!(
        names(req.tools.as_deref().expect("tools")),
        [
            "flat_one",
            "exec",
            "alpha_wait",
            "alpha_send",
            "flat_two",
            "beta_read",
            "beta_list"
        ]
    );
}

#[test]
fn flat_tools_without_a_namespace_parse_exactly_as_the_flat_rule_does() {
    // Arrange: positive control; every entry takes the pre-existing flat path.
    let flat = vec![
        function("flat_one"),
        json!({"type": "web_search"}),
        json!({"type": "function", "name": "flat_bad", "description": 42}),
        function("flat_two"),
    ];
    let expected: Vec<ToolDef> = flat
        .iter()
        .map(|t| {
            CustomTool::from_responses_function(t)
                .map_or_else(|| ToolDef::Other(t.clone()), ToolDef::Custom)
        })
        .collect();

    // Act
    let req = parse(body_with(Carrier::TopLevel, flat));

    // Assert
    assert_eq!(
        serde_json::to_vec(&req.tools.expect("tools")).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    assert_eq!(
        req.routectl_internal.responses_namespaces,
        routectl_core::schema::ResponsesNamespaces::default()
    );
}

#[test]
fn nested_non_functions_follow_their_carriers_credential_rule() {
    // Arrange: hosted, MCP and unknown members beside one function.
    let container = json!({
        "type": "namespace",
        "name": "ns_hosted",
        "tools": [
            function("hosted_sibling"),
            {"type": "web_search"},
            {"type": "mcp", "server_label": "nested_label", "headers": {"Authorization": "Bearer nested-credential"}},
            {"type": "future_kind", "name": "future_named"}
        ]
    });

    // Act
    let top = parse(body_with(Carrier::TopLevel, vec![container.clone()]));
    let inline = parse(body_with(Carrier::AdditionalTools, vec![container]));

    // Assert
    let top_tools = top.tools.expect("top-level members");
    assert_eq!(
        names(&top_tools),
        ["hosted_sibling", "<unnamed>", "<unnamed>", "future_named"]
    );
    assert_eq!(
        top.routectl_internal
            .responses_namespaces
            .unnamed_members
            .len(),
        2
    );
    let inline_tools = inline.tools.expect("the function member");
    assert_eq!(names(&inline_tools), ["hosted_sibling"]);
    let rendered = serde_json::to_string(&inline_tools).unwrap();
    assert!(!rendered.contains("nested-credential"), "{rendered}");
    assert!(
        inline
            .routectl_internal
            .responses_namespaces
            .unnamed_members
            .is_empty()
    );
}

#[test]
fn a_namespace_nested_in_a_namespace_is_not_walked() {
    // Arrange
    let container = json!({
        "type": "namespace",
        "name": "ns_outer",
        "tools": [
            function("outer_fn"),
            {"type": "namespace", "name": "ns_inner", "tools": [function("inner_fn")]}
        ]
    });

    // Act
    let req = parse(body_with(Carrier::TopLevel, vec![container]));

    // Assert
    assert_eq!(names(req.tools.as_deref().expect("tools")), ["outer_fn"]);
    assert!(
        !req.routectl_internal
            .responses_namespaces
            .members
            .contains_key("inner_fn")
    );
}

struct CollisionRow {
    name: &'static str,
    body: Value,
    /// The clashing name; at most one canonical tool may carry it.
    clashing: &'static str,
    /// Whether a flat declaration of the clashing name is kept.
    flat_kept: bool,
    expected: Vec<ResponsesNamespaceCollision>,
}

fn collision(namespace: &str, tool: &str) -> ResponsesNamespaceCollision {
    ResponsesNamespaceCollision {
        namespace: namespace.to_owned(),
        tool: tool.to_owned(),
    }
}

fn collision_rows() -> Vec<CollisionRow> {
    vec![
        CollisionRow {
            name: "flat vs nested",
            body: body_with(
                Carrier::TopLevel,
                vec![
                    function("shared"),
                    json!({"type": "namespace", "name": "ns_one", "tools": [function("shared"), function("one_only")]}),
                ],
            ),
            clashing: "shared",
            flat_kept: true,
            expected: vec![collision("ns_one", "shared")],
        },
        CollisionRow {
            name: "nested vs nested across two namespaces",
            body: body_with(
                Carrier::AdditionalTools,
                vec![
                    json!({"type": "namespace", "name": "ns_one", "tools": [function("shared"), function("one_only")]}),
                    json!({"type": "namespace", "name": "ns_two", "tools": [function("shared")]}),
                ],
            ),
            clashing: "shared",
            flat_kept: false,
            expected: vec![collision("ns_one", "shared"), collision("ns_two", "shared")],
        },
    ]
}

#[test]
#[serial]
fn a_colliding_member_is_excluded_warned_once_and_counted_once() {
    for row in collision_rows() {
        // Arrange
        let before = collision_count();

        // Act
        let mut req = None;
        let events = routectl_testkit::capture_events(|| req = Some(parse(row.body.clone())));

        // Assert
        let req = req.expect("parsed");
        let name = row.name;
        let tools = req.tools.expect("tools");
        let carrying: Vec<&ToolDef> = tools
            .iter()
            .filter(|t| matches!(t, ToolDef::Custom(c) if c.name == row.clashing))
            .collect();
        assert_eq!(
            carrying.len(),
            usize::from(row.flat_kept),
            "{name}: {tools:?}"
        );
        assert!(names(&tools).contains(&"one_only".to_owned()), "{name}");
        let namespaces = &req.routectl_internal.responses_namespaces;
        assert!(!namespaces.members.contains_key(row.clashing), "{name}");
        assert_eq!(
            namespaces.members.get("one_only").map(String::as_str),
            Some("ns_one")
        );
        assert_eq!(namespaces.collisions, row.expected, "{name}");
        let warns: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN && e.message.contains("namespaced tools"))
            .collect();
        assert_eq!(warns.len(), 1, "{name}: {events:?}");
        assert_eq!(collision_count() - before, 1, "{name}");
    }
}

#[test]
#[serial]
fn a_request_without_a_collision_neither_warns_nor_counts() {
    // Arrange: control for the row above.
    let before = collision_count();

    // Act
    let events = routectl_testkit::capture_events(|| {
        parse(body_with(Carrier::TopLevel, codex_shaped_containers()));
    });

    // Assert
    assert!(
        events.iter().all(|e| e.level != tracing::Level::WARN),
        "{events:?}"
    );
    assert_eq!(collision_count(), before);
}

#[test]
#[serial]
fn a_collision_is_noted_on_the_request_observation_slot() {
    // Arrange
    let mut colliding = parse(collision_rows().remove(0).body);
    let mut clean = parse(body_with(Carrier::TopLevel, codex_shaped_containers()));
    for req in [&mut colliding, &mut clean] {
        req.routectl_internal.observation = Some(RequestObservation::new());
    }

    // Act
    note_collisions_on_observation(&colliding);
    note_collisions_on_observation(&clean);

    // Assert
    let drops = |req: &ChatRequest| {
        let slot = req.routectl_internal.observation.as_ref().expect("slot");
        slot.snapshot().translation_drops
    };
    assert_eq!(drops(&colliding), 1);
    assert_eq!(drops(&clean), 0);
    let slot = colliding
        .routectl_internal
        .observation
        .as_ref()
        .expect("slot");
    assert!(
        !slot.note_translation_drop("openai-responses", "namespace_tool_name_collision"),
        "the pair is already noted on the slot"
    );
}
