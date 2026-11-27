//! The Bedrock mantle lane of the openai-compat egress withholds the
//! Anthropic client's user id: the canonical `user` the Anthropic ingress
//! copies from `metadata.user_id` stays off the AWS wire, is declared at
//! `/user`, and counts once on the fingerprint tally.

use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use routectl_core::{ChatRequest, Provider, RequestProvenance};
use serde_json::{Value, json};

use super::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::bedrock::auth::ResolvedCreds;
use crate::mantle::MantleAuth;

const CLIENT_USER: &str = "u-client-1";

fn first_party() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig::new(
        "compat-identity",
        "http://127.0.0.1:9",
        "test-key",
    ))
}

fn mantle() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        mantle: Some(MantleAuth {
            region: "us-west-2".into(),
            creds: ResolvedCreds::Bearer {
                key: "mantle-bearer-key".into(),
            },
        }),
        ..OpenAiCompatConfig::new(
            "mantle-identity",
            "https://bedrock-mantle.us-west-2.api.aws/openai/v1",
            "first-party-key",
        )
    })
}

/// A request carrying `user` and, when given, the extras bag, as `provenance`
/// leaves it.
fn identified(provenance: RequestProvenance, extras: Option<Value>) -> ChatRequest {
    let mut req: ChatRequest = serde_json::from_value(json!({
        "model": "anthropic.claude-haiku-4-5",
        "messages": [{"role": "user", "content": "hi"}],
    }))
    .expect("fixture is a canonical request");
    req.user = Some(CLIENT_USER.into());
    req.provider_extras = extras;
    req.routectl_internal.provenance = provenance;
    req
}

/// The Anthropic ingress's shape: `user` mirrors `metadata.user_id`.
fn anthropic_ingress() -> ChatRequest {
    identified(
        RequestProvenance::AnthropicIngress,
        Some(json!({"metadata": {"user_id": CLIENT_USER}})),
    )
}

fn fingerprint_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == super::LANE && e.policy_class == "client_fingerprint_stripped")
        .map_or(0, |e| e.action_count)
}

/// The wire body, the `/user` removals declared, and the fingerprint delta.
fn send(provider: &OpenAiCompatProvider, mut req: ChatRequest) -> (Value, usize, u64) {
    let tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(tap.clone());
    let before = fingerprint_count();

    let body = provider.normalize_request(&req).expect("normalize");

    let delta = fingerprint_count() - before;
    let declared = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|e| {
            e.site() == EditSite::ClientFingerprintStrip
                && e.path() == "/user"
                && e.kind() == EditKind::Remove
        })
        .count();
    (body, declared, delta)
}

#[test]
fn the_client_user_id_reaches_the_wire_only_off_the_mantle_anthropic_ingress_path() {
    let rows: [(&str, OpenAiCompatProvider, ChatRequest, Option<Value>); 3] = [
        (
            "mantle, Anthropic ingress",
            mantle(),
            anthropic_ingress(),
            None,
        ),
        (
            "first-party upstream, Anthropic ingress",
            first_party(),
            anthropic_ingress(),
            Some(json!(CLIENT_USER)),
        ),
        (
            "mantle, a Chat client's own user",
            mantle(),
            identified(RequestProvenance::OpenaiIngress, None),
            Some(json!(CLIENT_USER)),
        ),
    ];
    for (name, provider, req, want) in rows {
        let (body, _, _) = send(&provider, req);

        assert_eq!(body.get("user").cloned(), want, "{name}: {body}");
        if want.is_none() {
            assert!(!body.to_string().contains(CLIENT_USER), "{name}: {body}");
        }
    }
}

#[test]
fn an_anthropic_ingress_user_alone_is_withheld_on_the_mantle_lane_and_counted_once() {
    let rows = [
        (
            "user alone",
            identified(RequestProvenance::AnthropicIngress, None),
        ),
        ("user beside its metadata block", anthropic_ingress()),
    ];
    for (name, req) in rows {
        let (body, declared, delta) = send(&mantle(), req);

        assert!(body.get("user").is_none(), "{name}: {body}");
        assert_eq!(declared, 1, "{name}");
        assert_eq!(delta, 1, "{name}");
    }
}

#[test]
fn a_user_the_mantle_lane_keeps_is_neither_declared_nor_counted() {
    let rows: [(&str, OpenAiCompatProvider, ChatRequest); 2] = [
        (
            "first-party upstream, Anthropic ingress",
            first_party(),
            identified(RequestProvenance::AnthropicIngress, None),
        ),
        (
            "mantle, a Chat client's own user",
            mantle(),
            identified(RequestProvenance::OpenaiIngress, None),
        ),
    ];
    for (name, provider, req) in rows {
        let (_, declared, delta) = send(&provider, req);

        assert_eq!((declared, delta), (0, 0), "{name}");
    }
}
