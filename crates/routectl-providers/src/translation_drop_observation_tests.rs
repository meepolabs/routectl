//! Every lane's synchronous request entry counts a client request once,
//! however many times that request is normalized, when the request carries an
//! observation slot, and once per call when it carries none.
//!
//! The unit-test registry is per thread, and every entry below runs on the
//! test's own thread, so each delta is exactly what that test caused.

use std::collections::BTreeSet;

use routectl_core::{
    AnthropicThinking, ChatRequest, ContentPart, KnownContentPart, Message, MessageContent,
    Provider, RequestObservation, Role, ThinkingObject,
};

use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
use crate::openai_compat::{HistoryReasoning, ReasoningDialect};
use crate::translation_drop_metrics::{translation_drop_snapshot, translation_lane_seen};

type Normalize = fn(&ChatRequest);

/// One lane: its telemetry token, its real request entry, and the drop
/// classes [`lossy_request`] fires on it, written out by hand so the
/// distinct-pair count is never derived from the counters under test.
struct LaneRow {
    lane: &'static str,
    normalize: Normalize,
    drops: &'static [&'static str],
}

fn bedrock_cfg(api_shape: BedrockApiShape) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:observation-test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-sonnet-4-5".into(),
        api_shape,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    }
}

fn anthropic(req: &ChatRequest) {
    let _ = crate::anthropic_api::request::normalize("test", req, false, None, false, true);
}

fn openai_compat(req: &ChatRequest) {
    let _ = crate::openai_compat::request::normalize(
        "openai-compat:test",
        req,
        ReasoningDialect::OpenAi,
        HistoryReasoning::Auto,
        None,
        false,
    );
}

fn openai_responses(req: &ChatRequest) {
    let cfg = crate::openai_responses::OpenAiResponsesConfig::new("responses:test", "k");
    let _ = crate::openai_responses::request::translate(&cfg, req);
}

fn gemini(req: &ChatRequest) {
    let provider =
        crate::gemini::GeminiProvider::new(crate::gemini::GeminiConfig::new("gemini:test", "k"));
    let _ = provider.normalize_request(req);
}

fn bedrock_invoke(req: &ChatRequest) {
    let _ = crate::bedrock::invoke::normalize_request(&bedrock_cfg(BedrockApiShape::Invoke), req);
}

fn bedrock_converse(req: &ChatRequest) {
    let _ =
        crate::bedrock::converse::normalize_request(&bedrock_cfg(BedrockApiShape::Converse), req);
}

fn lanes() -> [LaneRow; 6] {
    [
        LaneRow {
            lane: "anthropic",
            normalize: anthropic,
            drops: &["system_part_unrepresentable"],
        },
        LaneRow {
            lane: "openai-compat",
            normalize: openai_compat,
            drops: &[
                "anthropic_beta_unsupported",
                "thinking_type_unrepresentable",
            ],
        },
        LaneRow {
            lane: "openai-responses",
            normalize: openai_responses,
            drops: &["thinking_type_unrepresentable"],
        },
        LaneRow {
            lane: "gemini",
            normalize: gemini,
            drops: &[
                "system_part_unrepresentable",
                "thinking_type_unrepresentable",
            ],
        },
        LaneRow {
            lane: "bedrock-invoke",
            normalize: bedrock_invoke,
            drops: &["system_part_unrepresentable"],
        },
        LaneRow {
            lane: "bedrock-converse",
            normalize: bedrock_converse,
            drops: &["system_part_unrepresentable"],
        },
    ]
}

/// A request every lane loses something from: a system-role turn with an
/// image part, an Anthropic beta flag, and a thinking type canonical
/// reasoning cannot express.
fn lossy_request() -> ChatRequest {
    let system_turn = Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Parts(vec![
            ContentPart::Known(KnownContentPart::Text {
                text: "be brief".into(),
                citations: None,
                cache_control: None,
            }),
            ContentPart::Known(KnownContentPart::ImageUrl {
                image_url: serde_json::json!({"url": "https://example.com/a.png"}),
                cache_control: None,
            }),
        ]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    };
    let user_turn = Message {
        role: Role::User,
        content: MessageContent::Text("hello".into()),
        ..system_turn.clone()
    };
    let mut req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: vec![system_turn, user_turn].into(),
        max_tokens: Some(64),
        anthropic_beta: vec!["observation-test-2099-01-01".into()],
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking = AnthropicThinking::Sent(
        ThinkingObject::from_client(serde_json::json!({"type": "future_mode"})).expect("an object"),
    );
    req
}

/// Every drop class with a nonzero count on `lane`, and its count.
fn drop_counts(lane: &str) -> Vec<(&'static str, u64)> {
    translation_drop_snapshot()
        .into_iter()
        .filter(|e| e.lane == lane)
        .map(|e| (e.drop_class, e.drop_count))
        .collect()
}

/// The classes whose count on `lane` moved between `before` and now, with the
/// delta each moved by.
fn drop_deltas(lane: &str, before: &[(&'static str, u64)]) -> Vec<(&'static str, u64)> {
    drop_counts(lane)
        .into_iter()
        .filter_map(|(class, after)| {
            let prior = before
                .iter()
                .find(|(c, _)| *c == class)
                .map_or(0, |(_, n)| *n);
            (after > prior).then_some((class, after - prior))
        })
        .collect()
}

/// The positive control every row leans on: the fixture really fires each
/// row's hand-written drop set on one plain call, so a later "delta 1" is a
/// deduplication, not a fixture that never fired.
#[test]
fn the_lossy_request_fires_exactly_the_listed_drops_on_every_lane() {
    for row in lanes() {
        // Arrange
        let req = lossy_request();
        let before = drop_counts(row.lane);

        // Act
        (row.normalize)(&req);

        // Assert
        let fired: BTreeSet<&str> = drop_deltas(row.lane, &before)
            .into_iter()
            .map(|(class, _)| class)
            .collect();
        let listed: BTreeSet<&str> = row.drops.iter().copied().collect();
        assert_eq!(fired, listed, "lane {}", row.lane);
    }
}

#[test]
fn a_request_normalized_twice_with_a_slot_counts_its_lane_and_each_drop_once() {
    for row in lanes() {
        // Arrange
        let slot = RequestObservation::new();
        let mut req = lossy_request();
        req.routectl_internal.observation = Some(slot.clone());
        let seen_before = translation_lane_seen(row.lane);
        let drops_before = drop_counts(row.lane);

        // Act
        (row.normalize)(&req);
        (row.normalize)(&req);

        // Assert
        assert_eq!(
            translation_lane_seen(row.lane) - seen_before,
            1,
            "lane {}: lane_seen",
            row.lane
        );
        let expected: Vec<(&str, u64)> = row.drops.iter().map(|class| (*class, 1)).collect();
        let mut deltas = drop_deltas(row.lane, &drops_before);
        deltas.sort_unstable();
        assert_eq!(deltas, expected, "lane {}: drop deltas", row.lane);
        assert_eq!(
            slot.snapshot().translation_drops,
            row.drops.len() as u64,
            "lane {}: the slot counts distinct pairs",
            row.lane
        );
    }
}

#[test]
fn a_request_without_a_slot_normalized_twice_counts_twice() {
    for row in lanes() {
        // Arrange
        let req = lossy_request();
        assert!(req.routectl_internal.observation.is_none());
        let seen_before = translation_lane_seen(row.lane);
        let drops_before = drop_counts(row.lane);

        // Act
        (row.normalize)(&req);
        (row.normalize)(&req);

        // Assert
        assert_eq!(
            translation_lane_seen(row.lane) - seen_before,
            2,
            "lane {}: lane_seen",
            row.lane
        );
        let expected: Vec<(&str, u64)> = row.drops.iter().map(|class| (*class, 2)).collect();
        let mut deltas = drop_deltas(row.lane, &drops_before);
        deltas.sort_unstable();
        assert_eq!(deltas, expected, "lane {}: drop deltas", row.lane);
    }
}

#[test]
fn two_requests_with_their_own_slots_each_count_once() {
    for row in lanes() {
        // Arrange
        let first = RequestObservation::new();
        let second = RequestObservation::new();
        let mut req_a = lossy_request();
        req_a.routectl_internal.observation = Some(first.clone());
        let mut req_b = lossy_request();
        req_b.routectl_internal.observation = Some(second.clone());
        let seen_before = translation_lane_seen(row.lane);

        // Act: interleaved, so a scope leaking from one call into the next
        // would dedup the second request against the first.
        (row.normalize)(&req_a);
        (row.normalize)(&req_b);
        (row.normalize)(&req_a);
        (row.normalize)(&req_b);

        // Assert
        assert_eq!(
            translation_lane_seen(row.lane) - seen_before,
            2,
            "lane {}",
            row.lane
        );
        assert_eq!(first.snapshot().translation_drops, row.drops.len() as u64);
        assert_eq!(second.snapshot().translation_drops, row.drops.len() as u64);
    }
}

#[test]
fn a_panic_inside_a_scope_leaves_no_slot_behind() {
    // Arrange
    let lane = "unit-test-lane-scope-panic";
    let dead = RequestObservation::new();
    let mut req = lossy_request();
    req.routectl_internal.observation = Some(dead.clone());

    // Act: a translation that panics while its scope is open.
    let unwound = std::panic::catch_unwind(|| {
        let _scope = crate::translation_drop_metrics::ObservationScope::enter(&req);
        crate::translation_drop_metrics::record_translation_lane_seen(lane);
        panic!("translation failed mid-scope");
    });
    let before = translation_lane_seen(lane);
    crate::translation_drop_metrics::record_translation_lane_seen(lane);
    crate::translation_drop_metrics::record_translation_lane_seen(lane);

    // Assert: the later calls ran with no slot, so both counted and the dead
    // request's slot saw nothing more.
    assert!(unwound.is_err(), "the closure must actually panic");
    assert_eq!(translation_lane_seen(lane) - before, 2);
    assert!(
        !dead.note_lane_seen(lane),
        "the dead slot saw the first call only"
    );
}
