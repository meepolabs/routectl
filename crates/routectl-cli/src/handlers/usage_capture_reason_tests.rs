//! The terminal error's typed credential reason lands in the row's `extra`
//! JSON as `upstream_reason`; a reasonless error leaves the key absent.

use super::*;
use routectl_core::{ChatRequest, UpstreamReason};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};

fn capture() -> (UsageCapture, UsageWriter) {
    let db_path = crate::test_usage_dir::usage_dir().join("usage.db");
    let (handle, writer) = UsageWriter::start(db_path, CHANNEL_CAPACITY, 0, true);
    let req = ChatRequest {
        model: "m".to_string(),
        ..Default::default()
    };
    let draft = build_usage_draft("openai", &req, "req-reason".to_string());
    (
        UsageCapture::new(draft, handle, "ingress-1".to_string()),
        writer,
    )
}

#[test]
fn a_terminal_error_stamps_its_typed_reason_and_a_reasonless_one_stamps_nothing() {
    let rows: [(&str, Error, Option<&str>); 5] = [
        (
            "upstream expired",
            Error::upstream("p", 401, "expired")
                .with_upstream_reason(Some(UpstreamReason::CredentialExpired)),
            Some("credential-expired"),
        ),
        (
            "upstream invalid",
            Error::upstream("p", 401, "invalid")
                .with_upstream_reason(Some(UpstreamReason::CredentialInvalid)),
            Some("credential-invalid"),
        ),
        (
            "all seats parked",
            Error::Credential {
                detail: "seat credential parked: credential-expired".into(),
                reason: UpstreamReason::CredentialExpired,
                parked_until_ms: Some(1_000),
            },
            Some("credential-expired"),
        ),
        ("reasonless 401", Error::upstream("p", 401, "denied"), None),
        ("auth without reason", Error::Auth("refresh".into()), None),
    ];
    for (case, err, want) in rows {
        let (mut cap, _writer) = capture();

        cap.observe_error(&err);

        let got = cap
            .record
            .extra
            .as_ref()
            .and_then(|extra| extra.get("upstream_reason"))
            .and_then(Value::as_str);
        assert_eq!(got, want, "{case}");
        if want.is_none() {
            assert_eq!(cap.record.extra, None, "{case}: extra stays untouched");
        }
    }
}

#[test]
fn the_reason_stamp_keeps_the_existing_extra_keys() {
    let (mut cap, _writer) = capture();
    cap.mark_stream_stage(StreamStage::PreContentDispatch);

    cap.observe_error(
        &Error::upstream("p", 401, "expired")
            .with_upstream_reason(Some(UpstreamReason::CredentialExpired)),
    );

    assert_eq!(
        cap.record.extra,
        Some(serde_json::json!({
            "stream_stage": "pre_content_dispatch",
            "upstream_reason": "credential-expired",
        })),
    );
}
