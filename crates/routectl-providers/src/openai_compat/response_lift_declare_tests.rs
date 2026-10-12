//! The openai-compat response normalization declares, on a tap that checks
//! the response, exactly the edits it makes: the `reasoning_content` rename
//! and lift, and `cache_read_input_tokens` lifted from a usage sub-bag. A
//! tap that does not check the response records nothing.

use routectl_core::wire_check::{EditKind, EditSite, IngressDialect, Leg, WireTap};
use serde_json::{Value, json};

use super::declare_response_lifts;
use crate::openai_compat::ReasoningDialect;

fn declared(tap: &WireTap) -> Vec<(EditSite, String, EditKind)> {
    tap.take_response()
        .edits(Leg::Response)
        .iter()
        .map(|e| (e.site(), e.path().to_string(), e.kind()))
        .collect()
}

fn body(message: Value, usage: Value) -> Value {
    json!({"choices": [{"index": 0, "message": message}], "usage": usage})
}

fn chunk(delta: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": delta}]})
}

/// One row: its name, the upstream body or chunk, the dialect, whether it
/// streamed, and the edits expected.
type LiftRow = (
    &'static str,
    Value,
    ReasoningDialect,
    bool,
    Vec<(EditSite, &'static str, EditKind)>,
);

#[test]
fn each_response_lift_is_declared_only_when_it_fires() {
    use EditKind::{Add, Remove};
    use EditSite::{CachedTokensAdd, ReasoningContentLift};

    let reasoning = json!({"role": "assistant", "content": "x", "reasoning_content": "why"});
    let plain = json!({"role": "assistant", "content": "x"});
    let no_usage = json!({"prompt_tokens": 1});
    let rc = "/choices/*/message/reasoning_content";
    let details = "/choices/*/message/reasoning_details";
    let r = "/choices/*/message/reasoning";
    let cache = "/usage/cache_read_input_tokens";
    let rows: [LiftRow; 7] = [
        (
            "lifting dialect, complete body",
            body(reasoning.clone(), no_usage.clone()),
            ReasoningDialect::DeepSeek,
            false,
            vec![
                (ReasoningContentLift, rc, Remove),
                (ReasoningContentLift, details, Add),
            ],
        ),
        (
            "lifting dialect, stream chunk",
            chunk(json!({"reasoning_content": "why"})),
            ReasoningDialect::DeepSeek,
            true,
            vec![
                (ReasoningContentLift, rc, Remove),
                (ReasoningContentLift, details, Add),
                (ReasoningContentLift, r, Add),
            ],
        ),
        (
            "non-lifting dialect renames only",
            body(reasoning.clone(), no_usage.clone()),
            ReasoningDialect::OpenAi,
            false,
            vec![
                (ReasoningContentLift, rc, Remove),
                (ReasoningContentLift, r, Add),
            ],
        ),
        (
            "reasoning already set keeps it",
            body(
                json!({"role": "assistant", "reasoning": "a", "reasoning_content": "b"}),
                no_usage.clone(),
            ),
            ReasoningDialect::OpenAi,
            false,
            vec![],
        ),
        (
            "cached tokens lifted from the OpenAI sub-bag",
            body(
                plain.clone(),
                json!({"prompt_tokens_details": {"cached_tokens": 4}}),
            ),
            ReasoningDialect::OpenAi,
            false,
            vec![(CachedTokensAdd, cache, Add)],
        ),
        (
            "cached tokens already top-level",
            body(
                plain.clone(),
                json!({"cache_read_input_tokens": 4,
                       "prompt_tokens_details": {"cached_tokens": 4}}),
            ),
            ReasoningDialect::OpenAi,
            false,
            vec![],
        ),
        (
            "nothing to lift",
            body(plain, no_usage),
            ReasoningDialect::DeepSeek,
            false,
            vec![],
        ),
    ];

    for (name, raw, dialect, streamed, want) in rows {
        let tap = WireTap::checking_response(IngressDialect::Chat);
        declare_response_lifts(Some(&tap), &raw, dialect, streamed);
        let want: Vec<(EditSite, String, EditKind)> = want
            .into_iter()
            .map(|(site, path, kind)| (site, path.to_string(), kind))
            .collect();
        assert_eq!(declared(&tap), want, "{name}");
    }
}

#[test]
fn a_tap_that_does_not_check_the_response_records_no_lift() {
    let tap = WireTap::admitted();
    let raw = body(
        json!({"role": "assistant", "reasoning_content": "why"}),
        json!({"prompt_tokens_details": {"cached_tokens": 4}}),
    );

    declare_response_lifts(Some(&tap), &raw, ReasoningDialect::DeepSeek, false);

    assert_eq!(declared(&tap), []);
}
