use bytes::Bytes;

use super::{AttemptRecord, IngressWire, WireTap};
use crate::schema::ChatRequest;
use crate::wire_check::edits::{DeclaredEdit, EditKind, EditSite};
use crate::wire_check::lane::IngressDialect;

const SENTINEL: &str = "SENTINEL-body-must-not-render-7f3a";

fn body() -> Bytes {
    Bytes::from(format!(
        r#"{{"model":"m","messages":[{{"content":"{SENTINEL}"}}]}}"#
    ))
}

fn stream_strip() -> DeclaredEdit {
    DeclaredEdit::new(EditSite::StreamStrip, "/stream", EditKind::Remove)
        .unwrap_or_else(|e| panic!("narrow pattern refused: {e}"))
}

fn ingress() -> IngressWire {
    IngressWire {
        dialect: IngressDialect::Anthropic,
        client_version: Some("2.1.0".to_string()),
        bytes: body(),
    }
}

#[test]
fn clone_observes_the_originals_deposit_and_declarations() {
    let router_copy = WireTap::default();
    let provider_copy = router_copy.clone();

    provider_copy.deposit(body());
    provider_copy.declare(stream_strip());
    let seen = router_copy.take();

    assert_eq!(seen.egress, Some(body()));
    assert_eq!(seen.edits, vec![stream_strip()]);
    let drained = provider_copy.take();
    assert!(drained.egress.is_none());
    assert!(drained.edits.is_empty());
}

#[test]
fn a_later_deposit_replaces_an_earlier_one() {
    let tap = WireTap::default();

    tap.deposit(Bytes::from_static(b"{\"first\":1}"));
    tap.deposit(Bytes::from_static(b"{\"second\":2}"));

    assert_eq!(
        tap.take().egress,
        Some(Bytes::from_static(b"{\"second\":2}"))
    );
}

#[test]
fn debug_never_renders_body_bytes() {
    let tap = WireTap::default();
    tap.deposit(body());
    tap.declare(stream_strip());
    let wire = ingress();
    let canonical = ChatRequest::default();
    let egress = body();
    let edits = [stream_strip()];
    let record = AttemptRecord {
        ingress: &wire,
        canonical: &canonical,
        egress: Some(&egress),
        edits: &edits,
        rejection: None,
    };

    let rendered = [
        ("tap", format!("{tap:?}")),
        ("ingress wire", format!("{wire:?}")),
        ("tap contents", format!("{:?}", tap.take())),
        ("attempt record", format!("{record:?}")),
    ];

    for (name, text) in rendered {
        assert!(!text.contains(SENTINEL), "{name} leaked the body: {text}");
    }
}

#[test]
fn drop_hook_removes_one_member_from_the_deposit() {
    let rows: [(&str, &'static str, &[u8], &[u8]); 4] = [
        (
            "top-level key",
            "/model",
            br#"{"model":"m","max_tokens":1}"#,
            br#"{"max_tokens":1}"#,
        ),
        (
            "nested key",
            "/messages/0/name",
            br#"{"messages":[{"name":"n","role":"user"}]}"#,
            br#"{"messages":[{"role":"user"}]}"#,
        ),
        (
            "absent key",
            "/missing",
            br#"{"model":"m"}"#,
            br#"{"model":"m"}"#,
        ),
        ("not json", "/model", b"not json", b"not json"),
    ];

    for (name, pointer, sent, expected) in rows {
        let tap = WireTap::dropping_on_deposit(pointer);
        let observer = tap.clone();

        tap.deposit(Bytes::copy_from_slice(sent));

        assert_eq!(
            observer.take().egress.as_deref(),
            Some(expected),
            "row {name}"
        );
    }
}
