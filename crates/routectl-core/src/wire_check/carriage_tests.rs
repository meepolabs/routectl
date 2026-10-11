use bytes::Bytes;

use super::{AttemptRecord, IngressWire, WireTap};
use crate::error::Error;
use crate::schema::ChatRequest;
use crate::wire_check::edits::{DeclaredEdit, EditKind, EditSite};
use crate::wire_check::lane::{IngressDialect, Leg};

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

fn finish_reason_rewrite() -> DeclaredEdit {
    DeclaredEdit::new(
        EditSite::FinishReasonRewrite,
        "/choices/*/finish_reason",
        EditKind::Rewrite,
    )
    .unwrap_or_else(|e| panic!("narrow pattern refused: {e}"))
}

fn response_body() -> Bytes {
    Bytes::from(format!(r#"{{"choices":[{{"text":"{SENTINEL}"}}]}}"#))
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
    let seen = router_copy.take_request();

    assert_eq!(seen.request(), Some(&body()));
    assert_eq!(seen.edits(Leg::Request), [stream_strip()]);
    let drained = provider_copy.take_request();
    assert!(drained.request().is_none());
    assert!(drained.edits(Leg::Request).is_empty());
}

#[test]
fn each_direction_lands_in_its_own_slot() {
    let request_only = WireTap::default();
    request_only.deposit(body());
    request_only.declare(stream_strip());
    let response_only = WireTap::default();
    response_only.deposit_response(response_body());
    response_only.declare_response(finish_reason_rewrite());

    let request_seen = request_only.take_request();
    let response_seen = response_only.take_response();

    let rows = [
        (
            "request deposit",
            request_seen.request().cloned(),
            Some(body()),
        ),
        (
            "request deposit leaves response empty",
            request_seen.response().cloned(),
            None,
        ),
        (
            "response deposit",
            response_seen.response().cloned(),
            Some(response_body()),
        ),
        (
            "response deposit leaves request empty",
            response_seen.request().cloned(),
            None,
        ),
    ];
    for (name, got, expected) in rows {
        assert_eq!(got, expected, "row {name}");
    }
    let edit_rows = [
        (
            "request edit on request leg",
            request_seen.edits(Leg::Request).to_vec(),
            vec![stream_strip()],
        ),
        (
            "request edit not on response leg",
            request_seen.edits(Leg::Response).to_vec(),
            vec![],
        ),
        (
            "response edit on response leg",
            response_seen.edits(Leg::Response).to_vec(),
            vec![finish_reason_rewrite()],
        ),
        (
            "response edit not on request leg",
            response_seen.edits(Leg::Request).to_vec(),
            vec![],
        ),
    ];
    for (name, got, expected) in edit_rows {
        assert_eq!(got, expected, "row {name}");
    }
}

#[test]
fn deposit_moves_the_buffer_without_copying() {
    let tap = WireTap::default();
    let sent = body();
    let sent_ptr = sent.as_ptr();
    let received = response_body();
    let received_ptr = received.as_ptr();

    tap.deposit(sent);
    tap.deposit_response(received);
    let request_side = tap.take_request();
    let response_side = tap.take_response();

    assert_eq!(request_side.request().map(|b| b.as_ptr()), Some(sent_ptr));
    assert_eq!(
        response_side.response().map(|b| b.as_ptr()),
        Some(received_ptr)
    );
}

#[test]
fn attempt_record_exposes_both_directions() {
    let wire = ingress();
    let canonical = ChatRequest::default();
    let egress = body();
    let upstream = response_body();
    let request_edits = [stream_strip()];
    let response_edits = [finish_reason_rewrite()];
    let rejection = Error::Internal("refused".to_string());

    let request_only = AttemptRecord::new(&wire, &canonical, Some(&egress), &request_edits);
    let full = request_only
        .with_response(Some(&upstream), &response_edits)
        .with_rejection(&rejection);

    assert_eq!(request_only.upstream_response(), None);
    assert!(request_only.edits(Leg::Response).is_empty());
    assert!(request_only.rejection().is_none());
    assert_eq!(full.egress(), Some(&egress));
    assert_eq!(full.upstream_response(), Some(&upstream));
    assert_eq!(full.edits(Leg::Request), request_edits);
    assert_eq!(full.edits(Leg::Response), response_edits);
    assert!(full.rejection().is_some());
    assert!(std::ptr::eq(full.ingress(), &raw const wire));
    assert!(std::ptr::eq(full.canonical(), &raw const canonical));
}

#[test]
fn a_later_deposit_replaces_an_earlier_one() {
    let tap = WireTap::default();

    tap.deposit(Bytes::from_static(b"{\"first\":1}"));
    tap.deposit(Bytes::from_static(b"{\"second\":2}"));

    assert_eq!(
        tap.take_request().request(),
        Some(&Bytes::from_static(b"{\"second\":2}"))
    );
}

#[test]
fn debug_never_renders_body_bytes() {
    let tap = WireTap::default();
    tap.deposit(body());
    tap.deposit_response(response_body());
    tap.declare(stream_strip());
    tap.declare_response(finish_reason_rewrite());
    let wire = ingress();
    let canonical = ChatRequest::default();
    let egress = body();
    let upstream = response_body();
    let request_edits = [stream_strip()];
    let response_edits = [finish_reason_rewrite()];
    let record = AttemptRecord::new(&wire, &canonical, Some(&egress), &request_edits)
        .with_response(Some(&upstream), &response_edits);

    let rendered = [
        ("tap", format!("{tap:?}")),
        ("ingress wire", format!("{wire:?}")),
        ("request side", format!("{:?}", tap.take_request())),
        ("response side", format!("{:?}", tap.take_response())),
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
            observer.take_request().request().map(Bytes::as_ref),
            Some(expected),
            "row {name}"
        );
    }
}

#[test]
fn drop_hook_leaves_the_response_deposit_untouched() {
    let tap = WireTap::dropping_on_deposit("/model");
    let upstream = Bytes::from_static(br#"{"model":"m","id":"x"}"#);

    tap.deposit_response(upstream.clone());

    assert_eq!(tap.take_response().response(), Some(&upstream));
}

#[test]
fn draining_the_request_side_leaves_the_response_side_for_its_own_check() {
    let tap = WireTap::default();
    tap.deposit_response(response_body());
    tap.declare_response(finish_reason_rewrite());
    tap.deposit(body());
    tap.declare(stream_strip());

    let request_side = tap.take_request();
    let response_side = tap.take_response();

    assert_eq!(request_side.request(), Some(&body()));
    assert_eq!(request_side.edits(Leg::Request), [stream_strip()]);
    assert!(request_side.response().is_none());
    assert!(request_side.edits(Leg::Response).is_empty());
    assert_eq!(response_side.response(), Some(&response_body()));
    assert_eq!(
        response_side.edits(Leg::Response),
        [finish_reason_rewrite()]
    );
    assert!(response_side.request().is_none());
    assert!(response_side.edits(Leg::Request).is_empty());
}

#[test]
fn draining_the_response_side_leaves_the_request_side_for_its_own_check() {
    let tap = WireTap::default();
    tap.deposit(body());
    tap.declare(stream_strip());
    tap.deposit_response(response_body());

    let response_side = tap.take_response();
    let request_side = tap.take_request();

    assert_eq!(response_side.response(), Some(&response_body()));
    assert!(response_side.request().is_none());
    assert_eq!(request_side.request(), Some(&body()));
    assert_eq!(request_side.edits(Leg::Request), [stream_strip()]);
}

#[test]
fn admission_is_fixed_at_creation_and_shared_by_every_clone() {
    let rows = [
        ("default tap", WireTap::default(), false),
        ("admitted tap", WireTap::admitted(), true),
    ];

    for (name, tap, expected) in rows {
        let provider_copy = tap.clone();
        provider_copy.deposit(body());
        let _ = provider_copy.take_request();

        assert_eq!(tap.is_admitted(), expected, "row {name}");
        assert_eq!(provider_copy.is_admitted(), expected, "row {name} clone");
    }
}
