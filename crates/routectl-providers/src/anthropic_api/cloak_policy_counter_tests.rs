//! Telemetry contract of the four losses the OAuth-egress cloak chooses:
//! the discarded client system prompt, the collapsed cache breakpoints, the
//! dropped non-text system block, and the tool-sort stand-down.
//!
//! Declared on `cloak.rs` via `#[cfg(test)] #[path = ...]` so the orchestrator
//! stays under the file-length ceiling.
//!
//! Each class gets THREE assertions in one place, because the acceptance shape
//! for this instrumentation is a log AND a counter at the deciding arm:
//!
//!   1. the class's counter delta is exactly 1 and every sibling class's is 0;
//!   2. its log fired ONCE, at the level its severity earns, carrying no
//!      fixture content (read through `routectl_testkit::capture_events`, so
//!      the assertion inspects structured events rather than rendered text);
//!   3. the wire body really did take the loss, with a surviving sibling where
//!      one is available so assertion 2 cannot pass by everything vanishing.
//!
//! SINGLE-SOURCE FIXTURES: each test drives exactly ONE of the four classes and
//! suppresses the other three. A fixture tripping two classes could not pin
//! either one -- deleting one record would leave the other's assertion green.
//!
//! SERIAL GUARDS: the registry is process-global and the runner is threaded,
//! so every test that can bump ANY of the four keys carries
//! `anthropic_api_cloak_policy_actions` -- the ones asserting a delta AND the
//! ones that trip a class incidentally while asserting something else, wherever
//! they live. One guard name for the group because each assertion here reads all
//! four keys, so a per-class name would exclude nothing.
//!
//! DELTAS ONLY, never a rate: these tests call the cloak directly, which is
//! downstream of the lane denominator, so any rate computed here divides by a
//! count this fixture never bumped.

use super::*;

use routectl_testkit::CapturedEvent;
use serde_json::json;

/// The four classes this module pins, in the order the delta arrays below
/// report them.
const CLASSES: [&str; 4] = [
    "cloak_client_system_prompt_discarded",
    "cloak_client_cache_breakpoints_collapsed",
    "cloak_non_text_system_block_dropped",
    "cloak_tool_sort_stood_down",
];

/// The message each class's deciding arm logs, and the level it logs at. The
/// message text is the operator-facing grep target, so it is what the log
/// assertions pin.
const PROMPT_DISCARD_LOG: &str = "discarding the client system prompt";
const NON_TEXT_LOG: &str = "blocks that carry no text content";
const COLLAPSE_LOG: &str = "collapsing client system cache breakpoints";
const TOOL_SORT_LOG: &str = "standing down the whole tool sort";

/// A fixture prompt that shares no substring with any of the static log
/// messages, so a leak assertion against it cannot be satisfied by the
/// message's own wording. A sentinel spelled like real client content would
/// make the check vacuous.
const SENTINEL_PROMPT: &str = "zzsentinel-directive-body";

/// The four counters' current values, in `CLASSES` order. Zero for a class
/// whose key has never been bumped.
fn policy_counts() -> [u64; 4] {
    let snapshot = crate::translation_drop_metrics::translation_policy_action_snapshot();
    CLASSES.map(|class| {
        snapshot
            .iter()
            .find(|e| e.lane == super::super::LANE && e.policy_class == class)
            .map_or(0, |e| e.action_count)
    })
}

fn deltas(before: [u64; 4], after: [u64; 4]) -> [u64; 4] {
    std::array::from_fn(|i| after[i] - before[i])
}

fn test_identity() -> ClaudeCodeIdentity {
    ClaudeCodeIdentity::mint(Some("policy-counter-session"))
}

/// Run the full cloak with a default config and report the four counter deltas
/// alongside every event it emitted.
fn cloak_deltas(body: &mut Value, is_non_cc: bool) -> ([u64; 4], Vec<CapturedEvent>) {
    let id = test_identity();
    let req = ChatRequest::default();

    let before = policy_counts();
    let events = routectl_testkit::capture_events(|| {
        cloak_oauth_egress(body, &req, &id, is_non_cc, &CloakConfig::default());
    });
    let after = policy_counts();

    (deltas(before, after), events)
}

/// The non-CC branch, which is where all four classes live.
fn non_cc_deltas(body: &mut Value) -> ([u64; 4], Vec<CapturedEvent>) {
    cloak_deltas(body, true)
}

/// Events whose message names the given loss.
fn matching<'a>(events: &'a [CapturedEvent], needle: &str) -> Vec<&'a CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message.contains(needle))
        .collect()
}

/// Assert the named loss was logged EXACTLY ONCE, at `level`, carrying none of
/// `forbidden` anywhere in its message or its structured fields.
///
/// The once-ness is load-bearing rather than incidental: an arm logging per
/// block turns one policy action into unbounded log volume on a request whose
/// block array is large, and a request may only ever be one such action.
fn assert_logged_once(
    events: &[CapturedEvent],
    needle: &str,
    level: tracing::Level,
    forbidden: &[&str],
) {
    let hits = matching(events, needle);
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one {needle:?} log; got {}: {:?}",
        hits.len(),
        events
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
    );
    let event = hits[0];
    assert_eq!(event.level, level, "wrong level on {needle:?}");
    let rendered = format!("{} {:?}", event.message, event.fields);
    for secret in forbidden {
        assert!(
            !rendered.contains(secret),
            "the log must carry no request content; {secret:?} leaked into {rendered:?}"
        );
    }
}

/// Assert no event names the given loss. Every zero-delta class needs this: a
/// counter that stayed put while its log fired means the two disagree about
/// what happened.
fn assert_not_logged(events: &[CapturedEvent], needle: &str) {
    assert!(
        matching(events, needle).is_empty(),
        "{needle:?} must not be logged by this fixture: {:?}",
        events
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// The whole-prompt discard.
// ---------------------------------------------------------------------------

/// The whole-prompt loss, driven through BOTH of its exits in one test because
/// the register names one covering test for the class and each exit must be
/// independently pinned: deleting either `return false` reds this test.
///
/// Subcase A has no `messages` key at all; subcase B has a messages array with
/// no `role: "user"` entry. Each carries one string system block (no
/// cache_control, so no collapse), no non-text block, and no `tools` array, so
/// the other three classes cannot fire from either body.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_cloaked_body_with_no_user_message_counts_the_discarded_system_prompt() {
    const PROMPT: &str = SENTINEL_PROMPT;

    // Arrange -- A: no messages key whatsoever.
    let mut no_messages = json!({"system": PROMPT});

    // Act
    let (deltas_a, events_a) = non_cc_deltas(&mut no_messages);

    // Assert
    assert_eq!(
        deltas_a,
        [1, 0, 0, 0],
        "no message array must count the discard alone; classes: {CLASSES:?}"
    );
    assert_logged_once(
        &events_a,
        PROMPT_DISCARD_LOG,
        tracing::Level::WARN,
        &[PROMPT],
    );
    assert_not_logged(&events_a, COLLAPSE_LOG);
    assert_not_logged(&events_a, NON_TEXT_LOG);
    assert_not_logged(&events_a, TOOL_SORT_LOG);
    assert!(
        !serde_json::to_string(&no_messages)
            .unwrap()
            .contains(PROMPT),
        "the fixture must actually lose the prompt: {no_messages}"
    );

    // Arrange -- B: a messages array carrying no user turn.
    let mut no_user = json!({
        "system": PROMPT,
        "messages": [{"role": "assistant", "content": "prior"}]
    });

    // Act
    let (deltas_b, events_b) = non_cc_deltas(&mut no_user);

    // Assert
    assert_eq!(
        deltas_b,
        [1, 0, 0, 0],
        "no user message must count the discard alone; classes: {CLASSES:?}"
    );
    assert_logged_once(
        &events_b,
        PROMPT_DISCARD_LOG,
        tracing::Level::WARN,
        &[PROMPT],
    );
    assert_not_logged(&events_b, COLLAPSE_LOG);
    assert_not_logged(&events_b, NON_TEXT_LOG);
    assert_not_logged(&events_b, TOOL_SORT_LOG);
    let serialized = serde_json::to_string(&no_user).unwrap();
    assert!(
        !serialized.contains(PROMPT),
        "the fixture must actually lose the prompt: {no_user}"
    );
    // Positive control on that negative: the untouched assistant turn is still
    // there, so the prompt's absence is not the whole array having vanished.
    assert!(serialized.contains("prior"), "got: {no_user}");
}

/// A discarded prompt takes its breakpoints with it, so the collapse class must
/// NOT also fire: that would count one loss twice under two labels, and the
/// operator would read a breakpoint collapse on a request that lost everything.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_discarded_prompt_carrying_two_breakpoints_counts_no_collapse() {
    // Arrange -- two breakpoints AND no user message to relocate into.
    let mut body = json!({
        "system": [
            {"type": "text", "text": "first", "cache_control": {"type": "ephemeral", "ttl": "5m"}},
            {"type": "text", "text": "second", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
        ],
        "messages": [{"role": "assistant", "content": "prior"}]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert
    assert_eq!(deltas, [1, 0, 0, 0], "classes: {CLASSES:?}");
    assert_logged_once(
        &events,
        PROMPT_DISCARD_LOG,
        tracing::Level::WARN,
        &["first"],
    );
    assert_not_logged(&events, COLLAPSE_LOG);
}

// ---------------------------------------------------------------------------
// The cache-breakpoint collapse.
// ---------------------------------------------------------------------------

/// Two captured breakpoints reduced to the last. A user message is present (no
/// discard), every block is text (no non-text drop), and there is no `tools`
/// array (no stand-down).
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn two_client_cache_breakpoints_count_one_collapse() {
    // Arrange
    let mut body = json!({
        "system": [
            {"type": "text", "text": "first", "cache_control": {"type": "ephemeral", "ttl": "5m"}},
            {"type": "text", "text": "second", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
        ],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert -- one collapse counted and logged at DEBUG, and the surviving
    // breakpoint is still the last captured one (instrumented, not repaired).
    assert_eq!(deltas, [0, 1, 0, 0], "classes: {CLASSES:?}");
    assert_logged_once(
        &events,
        COLLAPSE_LOG,
        tracing::Level::DEBUG,
        &["first", "second", "5m", "1h"],
    );
    assert_not_logged(&events, PROMPT_DISCARD_LOG);
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["ttl"], "1h",
        "the last-wins collapse must be unchanged: {body}"
    );
}

/// A breakpoint the client placed on a block the relocation CANNOT carry is
/// still a breakpoint that collapsed: the client asked for two cache boundaries
/// and the wire body gets one. Counting only the retained blocks' breakpoints
/// would read this request as lossless on the cache axis.
///
/// Drives the production carrier for a non-text block (a forwarded system turn),
/// so this fixture legitimately trips TWO classes and is therefore not a pin for
/// either -- it pins the INTERACTION, which no single-source fixture can.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_breakpoint_on_a_dropped_non_text_block_still_counts_the_collapse() {
    // Arrange -- one breakpoint on a non-text block, one on retained text.
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [
                {"type": "image", "source": {"type": "base64", "data": "AAAA"},
                 "cache_control": {"type": "ephemeral", "ttl": "5m"}},
                {"type": "text", "text": "retained directive",
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
        ]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert -- both classes fire once each, and the wire output is the
    // unchanged last-wins shape.
    assert_eq!(deltas, [0, 1, 1, 0], "classes: {CLASSES:?}");
    assert_logged_once(&events, COLLAPSE_LOG, tracing::Level::DEBUG, &["1h"]);
    assert_logged_once(&events, NON_TEXT_LOG, tracing::Level::WARN, &["AAAA"]);
    let reminder = &body["messages"][0]["content"][0];
    assert_eq!(
        reminder["cache_control"]["ttl"], "1h",
        "the surviving breakpoint must still be the last captured one: {body}"
    );
    assert!(
        reminder["text"]
            .as_str()
            .is_some_and(|t| t.contains("retained directive")),
        "the retained text must still relocate: {body}"
    );
}

// ---------------------------------------------------------------------------
// The non-text block drop.
// ---------------------------------------------------------------------------

/// The non-text block loss, driven through the live carrier that can reach it:
/// a forwarded `role: "system"` turn whose content array holds non-text blocks.
/// The canonical top-level `system` reaches this module as string text or text
/// blocks, so a top-level fixture would pin a shape production cannot produce.
///
/// TWO non-text blocks in one request, deliberately: the class is per request,
/// so two dropped blocks are still one action and ONE log line. A per-block log
/// or record would read 2 here -- and on a large client block array it would
/// turn one policy action into unbounded log volume.
///
/// A text block rides alongside so the reminder still builds and lands in the
/// user message (no discard), no block carries a cache_control (no collapse),
/// and there is no `tools` array (no stand-down).
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_forwarded_system_turn_with_a_non_text_block_counts_one_drop() {
    // Arrange
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [
                {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
                {"type": "text", "text": "mid-conversation directive"},
                {"type": "image", "source": {"type": "base64", "data": "BBBB"}},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
        ]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert -- ONE action and ONE log for two dropped blocks.
    assert_eq!(
        deltas,
        [0, 0, 1, 0],
        "two dropped blocks are one action; classes: {CLASSES:?}"
    );
    assert_logged_once(
        &events,
        NON_TEXT_LOG,
        tracing::Level::WARN,
        &["AAAA", "BBBB", "mid-conversation"],
    );
    assert_not_logged(&events, PROMPT_DISCARD_LOG);
    // The text sibling still relocated, and both non-text payloads really are
    // gone -- the negative assertion cannot pass by the turn having vanished.
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(
        serialized.contains("mid-conversation directive"),
        "the text sibling must still relocate: {body}"
    );
    assert!(
        !serialized.contains("AAAA") && !serialized.contains("BBBB"),
        "the fixture must actually drop both non-text blocks: {body}"
    );
}

/// A conversation carrying SEVERAL forwarded system turns, only the first of
/// which loses a block. Each turn is captured by its own pass, and the passes
/// are folded together -- so a fold that OVERWRITES rather than accumulates
/// would let the later clean turn erase the earlier turn's loss, reporting a
/// request that dropped content as lossless. One turn's fixture cannot catch
/// that; this one can.
///
/// Still one class delta and one WARN: the class is per request, not per turn.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_clean_later_system_turn_does_not_erase_an_earlier_turn_s_dropped_block() {
    // Arrange -- turn one loses a non-text block beside its text; turn two is
    // text-only and loses nothing.
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [
                {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
                {"type": "text", "text": "first directive"},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "system", "content": [
                {"type": "text", "text": "second directive"},
            ]},
        ]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert -- the earlier turn's loss survives the fold, once.
    assert_eq!(
        deltas,
        [0, 0, 1, 0],
        "the later clean turn must not erase the earlier loss; classes: {CLASSES:?}"
    );
    assert_logged_once(
        &events,
        NON_TEXT_LOG,
        tracing::Level::WARN,
        &["AAAA", "first directive", "second directive"],
    );
    assert_not_logged(&events, PROMPT_DISCARD_LOG);
    assert_not_logged(&events, COLLAPSE_LOG);
    assert_not_logged(&events, TOOL_SORT_LOG);
    // Both turns' text relocated into the one reminder, and the dropped payload
    // is gone -- so the negative above cannot pass by the content vanishing.
    let reminder = body["messages"][0]["content"][0]["text"]
        .as_str()
        .expect("the reminder is a text block")
        .to_string();
    assert!(
        reminder.contains("first directive") && reminder.contains("second directive"),
        "both directives must relocate: {body}"
    );
    assert!(
        !serde_json::to_string(&body).unwrap().contains("AAAA"),
        "the fixture must actually drop the non-text block: {body}"
    );
}

// ---------------------------------------------------------------------------
// The tool-sort stand-down.
// ---------------------------------------------------------------------------

/// Duplicate tool names stand the whole sort down. No `system` field and no
/// forwarded system turn, so nothing is captured and none of the three
/// relocation classes can fire.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn duplicate_tool_names_count_one_tool_sort_stand_down() {
    // Arrange
    let mut body = json!({
        "tools": [{"name": "mcp__dup"}, {"name": "mcp__alpha"}, {"name": "mcp__dup"}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert -- one stand-down counted, logged once at DEBUG with no tool name
    // in it, and the verbatim order preserved.
    assert_eq!(deltas, [0, 0, 0, 1], "classes: {CLASSES:?}");
    assert_logged_once(
        &events,
        TOOL_SORT_LOG,
        tracing::Level::DEBUG,
        &["mcp__dup", "mcp__alpha"],
    );
    assert_eq!(
        body["tools"][0]["name"], "mcp__dup",
        "the stand-down must preserve verbatim order: {body}"
    );
    assert_eq!(body["tools"][1]["name"], "mcp__alpha");
    assert_eq!(body["tools"][2]["name"], "mcp__dup");
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

/// The no-loss control. The four fixtures above prove each counter CAN fire;
/// this one proves the conditions are not always true -- without it, a record
/// wired to fire unconditionally would satisfy every one of them.
///
/// Same transforms as those fixtures -- relocation into a user message, one
/// breakpoint, two unique named custom tools -- and nothing lost.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_lossless_cloaked_request_counts_no_policy_action() {
    // Arrange
    let mut body = json!({
        "system": [{"type": "text", "text": "client system prompt",
                    "cache_control": {"type": "ephemeral"}}],
        "tools": [{"name": "mcp__zebra"}, {"name": "mcp__alpha"}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert -- nothing counted, nothing logged, and the transforms did run:
    // the prompt relocated, the single breakpoint survived, the tools sorted.
    assert_eq!(deltas, [0, 0, 0, 0], "classes: {CLASSES:?}");
    for needle in [
        PROMPT_DISCARD_LOG,
        COLLAPSE_LOG,
        NON_TEXT_LOG,
        TOOL_SORT_LOG,
    ] {
        assert_not_logged(&events, needle);
    }
    let reminder = &body["messages"][0]["content"][0];
    assert!(
        reminder["text"]
            .as_str()
            .is_some_and(|t| t.contains("client system prompt")),
        "the control must relocate the prompt: {body}"
    );
    assert_eq!(reminder["cache_control"]["type"], "ephemeral");
    assert_eq!(body["tools"][0]["name"], "mcp__alpha");
}

/// The genuine-CC branch runs neither the relocation nor the tool sort, so a
/// body that WOULD trip two classes on the non-CC branch counts nothing here.
/// Pins the gate the four records sit behind.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_genuine_cc_request_counts_no_cloak_policy_action() {
    // Arrange -- no user message to relocate into AND duplicate tool names.
    let mut body = json!({
        "system": "client system prompt",
        "tools": [{"name": "mcp__dup"}, {"name": "mcp__dup"}],
        "messages": [{"role": "assistant", "content": "prior"}]
    });

    // Act
    let (deltas, events) = cloak_deltas(&mut body, false);

    // Assert
    assert_eq!(deltas, [0, 0, 0, 0], "classes: {CLASSES:?}");
    for needle in [
        PROMPT_DISCARD_LOG,
        COLLAPSE_LOG,
        NON_TEXT_LOG,
        TOOL_SORT_LOG,
    ] {
        assert_not_logged(&events, needle);
    }
    assert_eq!(
        body["system"], "client system prompt",
        "the genuine-CC branch must leave the client system in place: {body}"
    );
}

/// `strict_mode` is the operator asking for the client system to be DROPPED
/// rather than relocated, so the resulting loss is a configured choice and none
/// of the three relocation classes covers it. A body that would trip all three
/// under the default config counts and logs nothing under strict mode.
///
/// Whether configured discards deserve telemetry of their own is a separate
/// product question; this test pins only that they do not ride these four.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn strict_mode_counts_no_relocation_policy_action() {
    // Arrange -- two breakpoints, a non-text block, and no user message.
    let id = test_identity();
    let req = ChatRequest::default();
    let cfg = CloakConfig {
        strict_mode: true,
        ..CloakConfig::default()
    };
    let mut body = json!({
        "system": [
            {"type": "text", "text": "first", "cache_control": {"type": "ephemeral", "ttl": "5m"}},
            {"type": "text", "text": "second", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
        ],
        "messages": [
            {"role": "system", "content": [
                {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
            ]},
            {"role": "assistant", "content": "prior"},
        ]
    });

    // Act
    let before = policy_counts();
    let events = routectl_testkit::capture_events(|| {
        cloak_oauth_egress(&mut body, &req, &id, true, &cfg);
    });
    let after = policy_counts();

    // Assert -- nothing counted, nothing logged, and strict mode really did
    // discard (identity-only system, no reminder anywhere).
    assert_eq!(deltas(before, after), [0, 0, 0, 0], "classes: {CLASSES:?}");
    for needle in [
        PROMPT_DISCARD_LOG,
        COLLAPSE_LOG,
        NON_TEXT_LOG,
        TOOL_SORT_LOG,
    ] {
        assert_not_logged(&events, needle);
    }
    let sys = body["system"].as_array().expect("system is an array");
    assert_eq!(sys.len(), 1, "strict mode must leave identity only: {body}");
    assert!(
        !serde_json::to_string(&body)
            .unwrap()
            .contains(SYSTEM_REMINDER_OPEN),
        "strict mode must add no reminder: {body}"
    );
}

/// `normalize_tools = false` is the operator turning the canonicalization off,
/// so the pass never runs and the un-stabilized order is a configured choice
/// rather than a stand-down routectl took. Counterpart to the strict-mode
/// control, on the other operator switch.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_tool_sort_kill_switch_counts_no_stand_down() {
    // Arrange -- duplicate names, which WOULD stand the sort down if it ran.
    let id = test_identity();
    let req = ChatRequest::default();
    let cfg = CloakConfig {
        normalize_tools: false,
        ..CloakConfig::default()
    };
    let mut body = json!({
        "tools": [{"name": "mcp__dup"}, {"name": "mcp__dup"}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    let before = policy_counts();
    let events = routectl_testkit::capture_events(|| {
        cloak_oauth_egress(&mut body, &req, &id, true, &cfg);
    });
    let after = policy_counts();

    // Assert
    assert_eq!(deltas(before, after), [0, 0, 0, 0], "classes: {CLASSES:?}");
    assert_not_logged(&events, TOOL_SORT_LOG);
}
