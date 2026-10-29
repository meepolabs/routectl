//! The Chat ingress declares exactly the edits its system-message lift
//! makes to the body the openai-compat egress sends back: the egress lowers
//! the lifted prompt into one leading system message.

use axum::http::HeaderMap;
use routectl_core::wire_check::{EditKind, EditSite};
use serde_json::{Value, json};

use crate::alloc_probe::count_allocs;
use crate::ingress::IngressAdapter;
use crate::ingress::openai::{OpenAiIngress, system_lift_edits};

fn lift_edits(body: Value) -> Vec<(String, EditKind)> {
    OpenAiIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("parses")
        .routectl_internal
        .declared_edits
        .iter()
        .filter(|e| e.site() == EditSite::SystemMessageLift)
        .map(|e| (e.path().to_string(), e.kind()))
        .collect()
}

fn system(text: &str) -> Value {
    json!({"role": "system", "content": text})
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

/// One row: its name, the client's messages, and the edits expected.
type LiftRow = (&'static str, Vec<Value>, Vec<(&'static str, EditKind)>);

#[test]
fn the_system_lift_declares_exactly_the_edit_the_round_trip_makes() {
    use EditKind::{Add, Remove, Rewrite};

    let parts_system = json!({"role": "system",
                              "content": [{"type": "text", "text": "be brief"}]});
    let rows: [LiftRow; 6] = [
        (
            "one leading text system",
            vec![system("a"), user("hi")],
            vec![],
        ),
        (
            "one leading parts system",
            vec![parts_system, user("hi")],
            vec![("/messages/0/content", Rewrite)],
        ),
        (
            "one system mid-conversation",
            vec![user("hi"), system("a"), user("again")],
            vec![("/messages/0", Add), ("/messages/1", Remove)],
        ),
        (
            "two systems, first leading",
            vec![system("a"), user("hi"), system("b")],
            vec![("/messages/0/content", Rewrite), ("/messages/2", Remove)],
        ),
        (
            "two systems, none leading",
            vec![user("hi"), system("a"), system("b")],
            vec![
                ("/messages/0", Add),
                ("/messages/1", Remove),
                ("/messages/2", Remove),
            ],
        ),
        ("no system", vec![user("hi")], vec![]),
    ];

    for (name, messages, want) in rows {
        let body = json!({"model": "m", "messages": messages});
        let want: Vec<(String, EditKind)> =
            want.into_iter().map(|(p, k)| (p.to_string(), k)).collect();
        assert_eq!(lift_edits(body), want, "{name}");
    }
}

#[test]
fn a_body_with_a_top_level_system_declares_no_lift() {
    let body = json!({"model": "m", "system": "primary",
                      "messages": [system("a"), user("hi")]});

    assert_eq!(lift_edits(body), []);
}

/// Declaring the lift runs on every Chat request before admission is known,
/// so a request whose lift declares nothing allocates nothing for it. The
/// mid-conversation row is the control: it declares an owned path, so the
/// probe sees its allocations.
#[test]
fn a_lift_that_declares_nothing_allocates_nothing() {
    let rows: [(&str, Vec<Value>, bool); 3] = [
        ("no system", vec![user("hi"), user("again")], false),
        (
            "one leading text system",
            vec![system("a"), user("hi")],
            false,
        ),
        (
            "one system mid-conversation",
            vec![user("hi"), system("a"), user("again")],
            true,
        ),
    ];
    for (name, messages, allocates) in rows {
        let body = json!({"model": "m", "messages": messages});
        let mut req: routectl_core::ChatRequest = serde_json::from_value(body).expect("parses");
        let lifted = crate::ingress::lift_system_messages(&mut req);

        let (edits, allocs) = count_allocs(|| system_lift_edits(false, &lifted));

        assert_eq!(edits.is_empty(), !allocates, "{name}");
        assert_eq!(allocs > 0, allocates, "{name}: {allocs} allocations");
    }
}

fn image_part() -> Value {
    json!({"type": "image_url", "image_url": {"url": "https://example.com/a.png"}})
}

fn unknown_part() -> Value {
    json!({"type": "future_block", "payload": {"k": 1}})
}

fn parse(body: Value) -> routectl_core::ChatRequest {
    OpenAiIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("parses")
}

fn role_edits(req: &routectl_core::ChatRequest) -> Vec<(String, EditKind)> {
    req.routectl_internal
        .declared_edits
        .iter()
        .filter(|e| e.site() == EditSite::SystemMessageLift)
        .map(|e| (e.path().to_string(), e.kind()))
        .collect()
}

/// A system-voice turn carrying a non-text part is not lifted: `system`
/// holds text only, so the whole turn stays in place with every part, for
/// each egress to forward or count.
#[test]
fn a_system_turn_with_a_non_text_part_stays_in_place_with_every_part() {
    let rows = [
        ("system, image part", "system", image_part()),
        ("system, unknown part", "system", unknown_part()),
        ("developer, image part", "developer", image_part()),
        ("developer, unknown part", "developer", unknown_part()),
    ];
    for (name, role, part) in rows {
        let turn = json!({"role": role,
                          "content": [{"type": "text", "text": "look"}, part.clone()]});
        let body = json!({"model": "m", "messages": [user("hi"), turn]});

        let req = parse(body);

        assert!(req.system.is_none(), "{name}: nothing lifted");
        assert_eq!(req.messages.len(), 2, "{name}");
        let kept = serde_json::to_value(&req.messages[1]).expect("serializes");
        assert_eq!(kept["role"], "system", "{name}");
        assert_eq!(
            kept["content"],
            json!([{"type": "text", "text": "look"}, part]),
            "{name}: every part kept, in order"
        );
    }
}

/// Text-only system turns still lift beside a kept one, and the declaration
/// names only what moved: the kept turn is not an edit, and a kept developer
/// turn declares the role rewrite the egress will send.
#[test]
fn the_lift_declares_only_the_turns_it_moved() {
    use EditKind::{Add, Remove, Rewrite};

    let image_turn = |role: &str| json!({"role": role, "content": [image_part()]});
    let rows: [LiftRow; 3] = [
        (
            "kept system mid-conversation",
            vec![user("hi"), image_turn("system")],
            vec![],
        ),
        (
            "kept developer mid-conversation",
            vec![user("hi"), image_turn("developer")],
            vec![("/messages/1/role", Rewrite)],
        ),
        (
            "lifted text system beside a kept one",
            vec![user("hi"), system("a"), image_turn("system"), system("b")],
            vec![
                ("/messages/0", Add),
                ("/messages/1", Remove),
                ("/messages/3", Remove),
            ],
        ),
    ];
    for (name, messages, want) in rows {
        let req = parse(json!({"model": "m", "messages": messages}));
        let want: Vec<(String, EditKind)> =
            want.into_iter().map(|(p, k)| (p.to_string(), k)).collect();
        assert_eq!(role_edits(&req), want, "{name}");
    }
}
