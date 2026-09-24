//! The anchor-only normalized estimate and the anchored-delta arithmetic.

use std::sync::Arc;

use routectl_core::test_utils::{assistant_text_msg, user_msg};
use routectl_core::{CacheControl, ChatRequest, Message, Role, SystemContent};
use serde_json::{Value, json};

use super::tests::{
    anchor_then_check, assert_miss, billing_block, grown, history, history_ending_with, lane,
    parts_msg, request, serialized_len, text_block, text_part, tool_result, tool_use,
};
use super::{AnchorVerdict, MissReason, RequestIdentity, anchored_input};

// ------------------------------------------------- normalized estimate

fn opening(prior: &ChatRequest, next: &ChatRequest) -> u64 {
    match anchor_then_check(prior, 10_000, next, &lane()) {
        AnchorVerdict::Hit { opening_input } => opening_input,
        miss => panic!("expected a hit, got {miss:?}"),
    }
}

fn marked(req: ChatRequest) -> ChatRequest {
    let mut messages = req.messages.to_vec();
    let last = messages.len() - 1;
    messages[last] = parts_msg(
        Role::User,
        vec![text_part(
            "Now list its dependencies.",
            Some(CacheControl::ephemeral_1h()),
        )],
    );
    ChatRequest {
        cache_control: Some(CacheControl::ephemeral_5m()),
        messages: Arc::from(messages),
        ..req
    }
}

fn with_last_user_text(req: ChatRequest) -> ChatRequest {
    let mut messages = req.messages.to_vec();
    let last = messages.len() - 1;
    messages[last] = parts_msg(
        Role::User,
        vec![text_part("Now list its dependencies.", None)],
    );
    ChatRequest {
        messages: Arc::from(messages),
        ..req
    }
}

#[test]
fn cache_markers_do_not_change_the_anchored_opening() {
    // Arrange
    let prior = request(history());
    let plain = with_last_user_text(request(grown(&history())));
    let with_markers = marked(request(grown(&history())));

    // Act
    let plain_opening = opening(&prior, &plain);
    let marked_opening = opening(&prior, &with_markers);

    // Assert
    assert!(serialized_len(&with_markers) > serialized_len(&plain));
    assert_eq!(marked_opening, plain_opening);
}

#[test]
fn a_much_larger_billing_block_does_not_change_the_opening() {
    // Arrange
    let prior = request(history());
    let plain = request(grown(&history()));
    let padded = ChatRequest {
        system: Some(SystemContent::Blocks(vec![
            billing_block(&"f".repeat(4_000)),
            text_block("You are a careful coding agent."),
        ])),
        ..request(grown(&history()))
    };

    // Act
    let plain_opening = opening(&prior, &plain);
    let padded_opening = opening(&prior, &padded);

    // Assert
    assert!(serialized_len(&padded) > serialized_len(&plain) + 4_000);
    assert_eq!(padded_opening, plain_opening);
}

#[test]
fn a_billing_only_system_matches_an_absent_system() {
    // The egress drops every billing block, so a system that is nothing but
    // billing reaches the upstream exactly like no system at all.
    let without = |messages| ChatRequest {
        system: None,
        ..request(messages)
    };
    let billing_blocks = |messages| ChatRequest {
        system: Some(SystemContent::Blocks(vec![billing_block("00001")])),
        ..request(messages)
    };
    let billing_text = |messages| ChatRequest {
        system: Some(SystemContent::Text(
            "x-anthropic-billing-header: cc_version=2.1.0; cch=00002;".into(),
        )),
        ..request(messages)
    };

    let absent = RequestIdentity::measure(&without(history()), None);
    let blocks = RequestIdentity::measure(&billing_blocks(history()), None);
    let text = RequestIdentity::measure(&billing_text(history()), None);

    assert_eq!(blocks.digest(), absent.digest());
    assert_eq!(text.digest(), absent.digest());
    assert_eq!(blocks.normalized_bytes(), absent.normalized_bytes());
    assert_eq!(
        opening(&without(history()), &billing_blocks(grown(&history()))),
        opening(&without(history()), &without(grown(&history())))
    );
}

#[test]
fn a_system_with_real_text_beside_billing_is_not_absent() {
    let absent = ChatRequest {
        system: None,
        ..request(history())
    };
    let with_text = ChatRequest {
        system: Some(SystemContent::Blocks(vec![
            billing_block("00001"),
            text_block(" "),
        ])),
        ..request(history())
    };

    let verdict = anchor_then_check(&absent, 1_000, &with_text, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn metadata_bytes_count_toward_the_normalized_size() {
    // Hashed fields and counted bytes are the same stream: metadata the
    // digest covers must also grow the normalized size.
    let small = request(grown(&history()));
    let large = ChatRequest {
        provider_extras: Some(json!({
            "metadata": {"user_id": "u".repeat(4_000)},
            "context_management": {"edits": [{"type": "clear_thinking"}]},
        })),
        ..request(grown(&history()))
    };

    let small_id = RequestIdentity::measure(&small, None);
    let large_id = RequestIdentity::measure(&large, None);

    let added = "u".repeat(4_000).len() - "u-1".len();
    assert_eq!(
        large_id.normalized_bytes() - small_id.normalized_bytes(),
        added as u64
    );
    assert_ne!(large_id.digest(), small_id.digest());
}

#[test]
fn normalized_estimate_counts_exactly_the_hashed_stream() {
    // Arrange
    let appended = "x".repeat(4_000);
    let base = request(history());
    let longer = request(history_ending_with(user_msg(&appended)));

    // Act
    let base_id = RequestIdentity::measure(&base, None);
    let longer_id = RequestIdentity::measure(&longer, None);

    // Assert
    assert_eq!(
        base_id.normalized_estimate(),
        base_id.normalized_bytes() / 4
    );
    assert!(longer_id.normalized_bytes() >= base_id.normalized_bytes() + appended.len() as u64);
    assert!(longer_id.normalized_bytes() < base_id.normalized_bytes() + appended.len() as u64 + 64);
}

#[test]
fn a_real_prompt_change_of_the_same_size_as_a_marker_still_misses() {
    // Arrange: the new request edits prior text instead of moving a marker.
    let prior = request(history());
    let mut edited = grown(&history());
    edited[0] = user_msg("Summarize the repository LAYOUT.");

    // Act
    let verdict = anchor_then_check(&prior, 1_000, &request(edited), &lane());

    // Assert
    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn the_persisted_raw_estimate_is_unchanged_by_anchor_measurement() {
    let req = marked(request(grown(&history())));
    let before = routectl_router::estimate_total_tokens(&req);

    let identity = RequestIdentity::measure(&req, None);

    assert_eq!(routectl_router::estimate_total_tokens(&req), before);
    assert_eq!(before, serialized_len(&req) as u64 / 4);
    assert_ne!(identity.normalized_estimate(), before);
}

// ------------------------------------------------------------ arithmetic

#[test]
fn anchored_input_adds_only_the_growth() {
    assert_eq!(anchored_input(10_000, 8_000, 8_500), 10_500);
}

#[test]
fn anchored_input_subtracts_a_shrink() {
    assert_eq!(anchored_input(10_000, 8_000, 7_900), 9_900);
}

#[test]
fn anchored_input_saturates_at_zero() {
    assert_eq!(anchored_input(50, 8_000, 10), 0);
}

#[test]
fn anchored_input_saturates_at_the_maximum() {
    assert_eq!(anchored_input(u64::MAX - 1, 0, 10), u64::MAX);
}

#[test]
fn anchored_input_with_no_raw_change_is_the_prior_actual() {
    assert_eq!(anchored_input(u64::MAX, u64::MAX, u64::MAX), u64::MAX);
}

// ------------------------------------------------ synthetic arithmetic

/// SYNTHETIC: a hand-written token count, not a tokenizer. It exists only
/// to give the arithmetic tests below a prior actual that differs from the
/// bytes/4 estimate; it says nothing about how close any estimate is to a
/// real provider count. Real accuracy is measured on live traffic.
fn synthetic_prior_actual(prior: &ChatRequest) -> u64 {
    RequestIdentity::measure(prior, None).normalized_estimate() * 8 / 5
}

fn json_heavy_history(rounds: usize) -> Vec<Message> {
    let mut messages = vec![user_msg("Audit every manifest in the repository.")];
    for i in 0..rounds {
        let id = format!("toolu_{i:03}");
        let rows: Vec<Value> = (0..40)
            .map(|j| json!({"crate": format!("c{i}-{j}"), "version": "1.0.0", "features": ["std", "derive"]}))
            .collect();
        let payload = serde_json::to_string_pretty(&rows).expect("fixture serializes");
        messages.push(tool_use(&id, json!({"path": format!("m{i}.json")})));
        messages.push(tool_result(&id, Value::String(payload)));
    }
    messages
}

#[test]
fn synthetic_anchored_opening_carries_the_prior_actual_and_adds_only_new_bytes() {
    // Arrange: a large prior prompt whose (synthetic) actual is far from its
    // raw estimate, then two small appended messages.
    let prior = request(json_heavy_history(30));
    let mut next_messages = json_heavy_history(30);
    next_messages.push(assistant_text_msg("All manifests pin 1.0.0."));
    next_messages.push(user_msg("Check the lockfile next."));
    let next = request(next_messages);
    let prior_actual = synthetic_prior_actual(&prior);
    let growth = RequestIdentity::measure(&next, None).normalized_estimate()
        - RequestIdentity::measure(&prior, None).normalized_estimate();

    // Act
    let verdict = anchor_then_check(&prior, prior_actual, &next, &lane());

    // Assert: the old prompt enters once (as the actual), never again as
    // estimate; the raw estimate of the whole request is not the answer.
    assert_eq!(
        verdict,
        AnchorVerdict::Hit {
            opening_input: prior_actual + growth
        }
    );
    assert!(
        growth > 0 && growth < 100,
        "growth is the two appended messages only"
    );
    assert_ne!(
        prior_actual + growth,
        routectl_router::estimate_total_tokens(&next)
    );
}
