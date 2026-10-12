//! The Chat ingress declares exactly the edits its system-message lift
//! makes to the body the openai-compat egress sends back: the egress lowers
//! the lifted prompt into one leading system message.

use axum::http::HeaderMap;
use routectl_core::wire_check::{EditKind, EditSite};
use serde_json::{Value, json};

use crate::ingress::IngressAdapter;
use crate::ingress::openai::OpenAiIngress;

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
    use EditKind::{Add, Relocate, Remove, Rewrite};

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
            vec![("/messages/1", Relocate)],
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
