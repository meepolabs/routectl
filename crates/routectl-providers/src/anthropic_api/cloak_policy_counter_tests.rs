//! Telemetry contract of the three losses the OAuth-egress cloak chooses:
//! the collapsed cache breakpoints, the dropped system block a user turn
//! cannot carry, and the tool-sort stand-down.
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
//! SINGLE-SOURCE FIXTURES: each test drives exactly ONE of the three classes and
//! suppresses the other two. A fixture tripping two classes could not pin
//! either one -- deleting one record would leave the other's assertion green.
//!
//! SERIAL GUARDS: the registry is process-global and the runner is threaded,
//! so every test that can bump ANY of the three keys carries
//! `anthropic_api_cloak_policy_actions` -- the ones asserting a delta AND the
//! ones that trip a class incidentally while asserting something else, wherever
//! they live. One guard name for the group because each assertion here reads all
//! three keys, so a per-class name would exclude nothing.
//!
//! DELTAS ONLY, never a rate: these tests call the cloak directly, which is
//! downstream of the lane denominator, so any rate computed here divides by a
//! count this fixture never bumped.

use super::*;

use routectl_testkit::CapturedEvent;
use serde_json::json;

/// The three classes this module pins, in the order the delta arrays below
/// report them.
const CLASSES: [&str; 3] = [
    "cloak_client_cache_breakpoints_collapsed",
    "cloak_unrepresentable_system_block_dropped",
    "cloak_tool_sort_stood_down",
];

/// The two relocation losses that no longer exist and must never be recorded
/// again: a zero delta on each is asserted wherever a fixture reaches the
/// shape that used to trip them.
const RETIRED_CLASSES: [&str; 2] = [
    "cloak_client_system_prompt_discarded",
    "cloak_non_text_system_block_dropped",
];

/// The message each class's deciding arm logs, and the level it logs at. The
/// message text is the operator-facing grep target, so it is what the log
/// assertions pin.
const REFUSAL_LOG: &str = "cloak system relocation refused";
const UNREPRESENTABLE_LOG: &str = "blocks a user turn cannot carry";
const COLLAPSE_LOG: &str = "collapsing client system cache breakpoints";
const TOOL_SORT_LOG: &str = "standing down the whole tool sort";

/// A fixture prompt that shares no substring with any of the static log
/// messages, so a leak assertion against it cannot be satisfied by the
/// message's own wording. A sentinel spelled like real client content would
/// make the check vacuous.
const SENTINEL_PROMPT: &str = "zzsentinel-directive-body";

/// The counters' current values for `classes`, in order. Zero for a class
/// whose key has never been bumped.
fn counts_of<const N: usize>(classes: [&str; N]) -> [u64; N] {
    let snapshot = crate::translation_drop_metrics::translation_policy_action_snapshot();
    classes.map(|class| {
        snapshot
            .iter()
            .find(|e| e.lane == super::super::LANE && e.policy_class == class)
            .map_or(0, |e| e.action_count)
    })
}

fn policy_counts() -> [u64; 3] {
    counts_of(CLASSES)
}

fn deltas<const N: usize>(before: [u64; N], after: [u64; N]) -> [u64; N] {
    std::array::from_fn(|i| after[i] - before[i])
}

fn test_identity() -> ClaudeCodeIdentity {
    ClaudeCodeIdentity::mint(Some("policy-counter-session"))
}

/// Run the full cloak with a default config and report the three counter
/// deltas alongside every event it emitted. Also asserts the retired classes
/// stayed at zero, so no fixture here can resurrect one unnoticed.
fn cloak_deltas(body: &mut Value, is_non_cc: bool) -> ([u64; 3], Vec<CapturedEvent>) {
    let id = test_identity();
    let req = ChatRequest::default();

    let before = policy_counts();
    let retired_before = counts_of(RETIRED_CLASSES);
    let events = routectl_testkit::capture_events(|| {
        cloak_oauth_egress(body, &req, &id, is_non_cc, &CloakConfig::default())
            .expect("cloak applies");
    });
    let after = policy_counts();
    assert_eq!(
        deltas(retired_before, counts_of(RETIRED_CLASSES)),
        [0, 0],
        "retired classes must never be recorded: {RETIRED_CLASSES:?}"
    );

    (deltas(before, after), events)
}

/// The non-CC branch, which is where all three classes live.
fn non_cc_deltas(body: &mut Value) -> ([u64; 3], Vec<CapturedEvent>) {
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
// The former whole-prompt discard: now relocated, or refused, never counted.
// ---------------------------------------------------------------------------

/// Assistant-only history no longer loses the prompt: it lands in one
/// synthetic leading user turn, and nothing is counted or logged as a loss.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn assistant_only_history_relocates_the_prompt_and_counts_nothing() {
    // Arrange
    let mut body = json!({
        "system": SENTINEL_PROMPT,
        "messages": [{"role": "assistant", "content": "prior"}]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert
    assert_eq!(deltas, [0, 0, 0], "classes: {CLASSES:?}");
    assert_not_logged(&events, UNREPRESENTABLE_LOG);
    assert_not_logged(&events, REFUSAL_LOG);
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(serialized.contains(SENTINEL_PROMPT), "got: {body}");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "prior");
}

/// A refused relocation fails the request rather than dispatching it, so it
/// counts no policy action: the request never reaches the wire. It WARNs once,
/// carrying the shape and no content, and leaves the body untouched.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_refused_relocation_counts_nothing_and_warns_once_without_content() {
    for mut body in [
        json!({"system": SENTINEL_PROMPT}),
        json!({"system": SENTINEL_PROMPT, "messages": []}),
        json!({"system": SENTINEL_PROMPT, "messages": {"role": "user"}}),
    ] {
        // Arrange
        let before_body = body.clone();
        let id = test_identity();
        let req = ChatRequest::default();
        let before = policy_counts();
        let retired_before = counts_of(RETIRED_CLASSES);

        // Act
        let mut result = None;
        let events = routectl_testkit::capture_events(|| {
            result = Some(cloak_oauth_egress(
                &mut body,
                &req,
                &id,
                true,
                &CloakConfig::default(),
            ));
        });

        // Assert
        let refusal = result
            .expect("ran")
            .expect_err("a body with nowhere to land must be refused");
        assert!(!refusal.detail.contains(SENTINEL_PROMPT), "{refusal:?}");
        assert_eq!(deltas(before, policy_counts()), [0, 0, 0]);
        assert_eq!(deltas(retired_before, counts_of(RETIRED_CLASSES)), [0, 0]);
        assert_logged_once(
            &events,
            REFUSAL_LOG,
            tracing::Level::WARN,
            &[SENTINEL_PROMPT],
        );
        assert_eq!(body, before_body, "a refusal must leave the body untouched");
    }
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
    assert_eq!(deltas, [1, 0, 0], "classes: {CLASSES:?}");
    assert_logged_once(
        &events,
        COLLAPSE_LOG,
        tracing::Level::DEBUG,
        &["first", "second", "5m", "1h"],
    );
    assert_not_logged(&events, UNREPRESENTABLE_LOG);
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
/// Drives the production carrier for such a block (a forwarded system turn),
/// so this fixture legitimately trips TWO classes and is therefore not a pin for
/// either -- it pins the INTERACTION, which no single-source fixture can.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_breakpoint_on_an_unrepresentable_block_still_counts_the_collapse() {
    // Arrange -- one breakpoint on a thinking block, one on retained text.
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [
                {"type": "thinking", "thinking": "AAAA", "signature": "sig",
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
    assert_eq!(deltas, [1, 1, 0], "classes: {CLASSES:?}");
    assert_logged_once(&events, COLLAPSE_LOG, tracing::Level::DEBUG, &["1h"]);
    assert_logged_once(
        &events,
        UNREPRESENTABLE_LOG,
        tracing::Level::WARN,
        &["AAAA"],
    );
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

/// A carried image keeps its OWN breakpoint, so a text breakpoint beside it is
/// not a collapse: two boundaries asked for, two on the wire.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_carried_block_s_breakpoint_is_not_a_collapse() {
    // Arrange
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [
                {"type": "text", "text": "directive",
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                {"type": "image", "source": {"type": "base64", "data": "AAAA"},
                 "cache_control": {"type": "ephemeral", "ttl": "5m"}},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
        ]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert
    assert_eq!(deltas, [0, 0, 0], "classes: {CLASSES:?}");
    assert_not_logged(&events, COLLAPSE_LOG);
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0]["cache_control"]["ttl"], "1h");
    assert_eq!(content[1]["cache_control"]["ttl"], "5m");
}

// ---------------------------------------------------------------------------
// The unrepresentable-block drop.
// ---------------------------------------------------------------------------

/// The unrepresentable-block loss, driven through the live carrier that can
/// reach it: a forwarded `role: "system"` turn whose content array holds blocks
/// a user turn cannot carry. Image and document blocks ride along and are NOT
/// losses, so this fixture also proves they are not counted.
///
/// TWO unrepresentable blocks in one request, deliberately: the class is per
/// request, so two dropped blocks are still one action and ONE log line.
///
/// A text block rides alongside so the reminder still builds (no refusal), no
/// block carries a cache_control (no collapse), and there is no `tools` array
/// (no stand-down).
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_forwarded_system_turn_with_unrepresentable_blocks_counts_one_drop() {
    // Arrange
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [
                {"type": "tool_use", "id": "toolu_AAAA", "name": "x", "input": {}},
                {"type": "text", "text": "mid-conversation directive"},
                {"type": "image", "source": {"type": "base64", "data": "CCCC"}},
                {"type": "redacted_thinking", "data": "BBBB"},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
        ]
    });

    // Act
    let (deltas, events) = non_cc_deltas(&mut body);

    // Assert -- ONE action and ONE log for two dropped blocks.
    assert_eq!(
        deltas,
        [0, 1, 0],
        "two dropped blocks are one action; classes: {CLASSES:?}"
    );
    assert_logged_once(
        &events,
        UNREPRESENTABLE_LOG,
        tracing::Level::WARN,
        &["AAAA", "BBBB", "CCCC", "mid-conversation"],
    );
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(
        serialized.contains("mid-conversation directive") && serialized.contains("CCCC"),
        "the text and the carried image must still relocate: {body}"
    );
    assert!(
        !serialized.contains("toolu_AAAA") && !serialized.contains("BBBB"),
        "the fixture must actually drop both unrepresentable blocks: {body}"
    );
}

/// Several forwarded system turns, only the first of which loses a block. The
/// passes are folded together, so a fold that OVERWRITES rather than
/// accumulates would let the later clean turn erase the earlier turn's loss.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_clean_later_system_turn_does_not_erase_an_earlier_turn_s_dropped_block() {
    // Arrange
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [
                {"type": "thinking", "thinking": "AAAA", "signature": "sig"},
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

    // Assert
    assert_eq!(
        deltas,
        [0, 1, 0],
        "the later clean turn must not erase the earlier loss; classes: {CLASSES:?}"
    );
    assert_logged_once(
        &events,
        UNREPRESENTABLE_LOG,
        tracing::Level::WARN,
        &["AAAA", "first directive", "second directive"],
    );
    assert_not_logged(&events, COLLAPSE_LOG);
    assert_not_logged(&events, TOOL_SORT_LOG);
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
        "the fixture must actually drop the thinking block: {body}"
    );
}

// ---------------------------------------------------------------------------
// The tool-sort stand-down.
// ---------------------------------------------------------------------------

/// Duplicate tool names stand the whole sort down. No `system` field and no
/// forwarded system turn, so nothing is captured and neither relocation class
/// can fire.
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
    assert_eq!(deltas, [0, 0, 1], "classes: {CLASSES:?}");
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

/// The no-loss control. The fixtures above prove each counter CAN fire;
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
    assert_eq!(deltas, [0, 0, 0], "classes: {CLASSES:?}");
    for needle in [
        REFUSAL_LOG,
        COLLAPSE_LOG,
        UNREPRESENTABLE_LOG,
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
    assert_eq!(deltas, [0, 0, 0], "classes: {CLASSES:?}");
    for needle in [
        REFUSAL_LOG,
        COLLAPSE_LOG,
        UNREPRESENTABLE_LOG,
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
/// rather than relocated, so the resulting loss is a configured choice and no
/// relocation class covers it. A body that would trip both relocation classes
/// under the default config counts and logs nothing under strict mode.
///
/// Whether configured discards deserve telemetry of their own is a separate
/// product question; this test pins only that they do not ride these classes.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn strict_mode_counts_no_relocation_policy_action() {
    // Arrange -- two breakpoints, an unrepresentable block, and no user
    // message.
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
                {"type": "tool_use", "id": "toolu_AAAA", "name": "x", "input": {}},
            ]},
            {"role": "assistant", "content": "prior"},
        ]
    });

    // Act
    let before = policy_counts();
    let events = routectl_testkit::capture_events(|| {
        cloak_oauth_egress(&mut body, &req, &id, true, &cfg).expect("cloak applies");
    });
    let after = policy_counts();

    // Assert -- nothing counted, nothing logged, and strict mode really did
    // discard (identity-only system, no reminder anywhere).
    assert_eq!(deltas(before, after), [0, 0, 0], "classes: {CLASSES:?}");
    for needle in [
        REFUSAL_LOG,
        COLLAPSE_LOG,
        UNREPRESENTABLE_LOG,
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
        cloak_oauth_egress(&mut body, &req, &id, true, &cfg).expect("cloak applies");
    });
    let after = policy_counts();

    // Assert
    assert_eq!(deltas(before, after), [0, 0, 0], "classes: {CLASSES:?}");
    assert_not_logged(&events, TOOL_SORT_LOG);
}
