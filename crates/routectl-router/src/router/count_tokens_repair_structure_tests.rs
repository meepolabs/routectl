//! Structural guards over the token-count walk's repair wiring.
//!
//! These exist because the arm they cover cannot be reached end-to-end
//! today: `count_tokens` admits only `anthropic-api` and Anthropic-family
//! `bedrock` seats, and the classifier's replay-rejection lift is closed
//! over the kinds with a captured envelope (`openai-responses`), so no seat
//! the walk can dispatch to can produce the class the repair arm fires on.
//! `repair_budget_cross_walk_tests` pins that disjointness; a behavioral
//! test of this arm would pass with the arm deleted, which is worse than no
//! test.
//!
//! So the arm's load-bearing properties are asserted against the module's
//! own SOURCE instead. THIS FILE is the sole owner of the wiring claims --
//! that the budget is threaded rather than constructed per seat, that the
//! arm's conditions are ordered so the budget is drawn last, that the
//! repaired request goes through the recalibrating strip wrapper before the
//! `continue`, that the probe slot is released and disarmed in that order
//! without a breaker debit, that the classified rejection is rebuilt
//! body-free and ASSIGNED back, and that no unsinked settlement runs.
//! Behavioral tests elsewhere claim only what they can observe. Each guard
//! names what a violating edit looks like, so whoever makes it gets a
//! failure that says why.
//!
//! The whole file is scanned: its tests are SIDECARS, so none of their text
//! is in this source and there is nothing to cut (a `#[cfg(test)]` cut here
//! would silently shrink the scanned region).
//!
//! When a repair kind lands whose lane a count_tokens-capable seat can
//! reach, these guards are superseded by a behavioral test in
//! `repair_budget_cross_walk_tests` and should be deleted rather than kept
//! alongside it.

use super::REPAIR_ARM_MARKER;

/// This module's production source, scanned whole (sidecar tests).
const SOURCE: &str = include_str!("count_tokens.rs");

/// The source of one named `fn` body: from its signature to the start of
/// the next top-level `impl`-level `fn`, or end of file.
///
/// # Panics
///
/// If the signature is absent or appears more than once -- either way the
/// region a guard scans would silently change.
fn fn_region<'a>(source: &'a str, signature: &str) -> &'a str {
    let occurrences = source.matches(signature).count();
    assert_eq!(
        occurrences, 1,
        "`{signature}` occurs {occurrences} times, so the scanned region is ambiguous; \
         update these guards deliberately rather than letting them cover a different span",
    );
    let start = source.find(signature).expect("checked above");
    let rest = &source[start + signature.len()..];
    match rest
        .find("\n    async fn ")
        .or_else(|| rest.find("\n    fn "))
    {
        Some(end) => &rest[..end],
        None => rest,
    }
}

const WALK_SIGNATURE: &str = "async fn count_tokens_walk(";
const SEAT_SIGNATURE: &str = "async fn count_tokens_try_seat(";
const BUDGET_CONSTRUCTOR: &str = "RepairBudget::per_request()";

/// Offset of the strip-repair arm inside a seat-function region.
///
/// # Panics
///
/// If the arm's marker is absent -- the token-count walk would then have no
/// repair position at all, which is the defect these guards exist to catch.
fn repair_arm_start(seat: &str) -> usize {
    seat.find(REPAIR_ARM_MARKER).unwrap_or_else(|| {
        panic!(
            "the seat's repair arm (marked `{REPAIR_ARM_MARKER}`) is gone; the token-count \
             walk has no repair position left"
        )
    })
}

/// The CONDITION text of the repair arm's `if`: everything between the `if`
/// keyword and the `{` that opens its body.
///
/// Found by brace depth from the `if`, not by matching an indented literal:
/// an indentation-sensitive needle silently stops matching when the arm is
/// reindented (a nesting change, a rustfmt rewrap), and a guard that then
/// falls back to "the whole arm" reads every statement in the BODY as if it
/// were a condition -- which is exactly the mutation it exists to catch. So
/// this fails CLOSED instead: no locatable body brace panics rather than
/// returning a span.
///
/// `arm` must start at the repair arm's marker comment.
///
/// # Panics
///
/// If the arm has no `if`, or no `{` at depth zero after it. Both mean the
/// arm's shape changed enough that these guards can no longer say what they
/// claim, and a loud failure is the only honest outcome.
fn repair_arm_conditions(arm: &str) -> &str {
    let if_at = arm
        .find("if ")
        .expect("the repair arm must be an `if` whose conditions these guards can read");
    let after_if = &arm[if_at + "if ".len()..];

    // Depth over the bracket kinds a condition can legitimately contain, so
    // a `(`-nested call or an index inside the condition cannot be mistaken
    // for the body opener. The first `{` seen at depth zero IS the body.
    let mut depth = 0i32;
    for (offset, ch) in after_if.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '{' if depth == 0 => return &after_if[..offset],
            _ => {}
        }
    }
    panic!(
        "the repair arm's `if` has no body-opening `{{` at bracket depth zero, so its \
         conditions cannot be delimited; these guards must be updated deliberately rather \
         than left scanning a span that no longer means what they assert"
    );
}

/// A condition expression's TOP-LEVEL conjuncts, in source order, split on
/// the `&&` that sit at bracket depth zero.
///
/// Splitting on depth-zero `&&` only is what makes an order or arity
/// assertion mean what it says: an `&&` nested inside a call argument or a
/// parenthesized sub-expression is part of ONE conjunct, not a separator, so
/// a condition that grows an internal `&&` cannot silently renumber the
/// sequence a guard reads.
fn top_level_conjuncts(conditions: &str) -> Vec<String> {
    let bytes = conditions.as_bytes();
    let mut conjuncts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'&' if depth == 0 && bytes.get(idx + 1) == Some(&b'&') => {
                conjuncts.push(conditions[start..idx].trim().to_string());
                idx += 2;
                start = idx;
                continue;
            }
            _ => {}
        }
        idx += 1;
    }
    conjuncts.push(conditions[start..].trim().to_string());
    conjuncts.retain(|c| !c.is_empty());
    conjuncts
}

/// The repair arm's top-level conjuncts.
///
/// `arm` must start at the repair arm's marker comment.
fn repair_arm_conjuncts(arm: &str) -> Vec<String> {
    top_level_conjuncts(repair_arm_conditions(arm))
}

/// `source` with comment text blanked out but string literals preserved,
/// for the guards that forbid a CALL rather than a mention.
///
/// Needed because the honest way to document why a call is absent is to name
/// it, and a bare `contains` cannot tell the explanation from the thing
/// explained. Masking prose first means a guard can say "this call must not
/// appear" without the doc comment that justifies its absence tripping it --
/// and without weakening the needle to a receiver-specific spelling that a
/// rename would slip past.
///
/// STRING-AWARE, which the naive line-comment version was not: a `//` inside
/// a string literal (a URL in a `tracing` field, a doc example, a format
/// string) is NOT a comment start, and truncating the line there silently
/// discarded every character after it. Real code following such a string --
/// including a forbidden settlement call on the same statement -- vanished
/// from the scanned text, so the guard passed while the call was live. That
/// shape is reproduced in `the_masker_keeps_code_after_a_string_holding_a_
/// double_slash` and mutation-verified.
///
/// Comment bytes are replaced with spaces rather than removed so byte offsets
/// are preserved: the positional guards can compare offsets from masked and
/// raw text interchangeably.
fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut idx = 0usize;
    // Only the states this source shape can be in. Raw strings and char
    // literals do not appear in the scanned region; a `"` inside a comment is
    // ignored because comment scanning consumes to end-of-line first.
    while idx < bytes.len() {
        let rest = &source[idx..];
        if rest.starts_with("//") {
            // Comment: blank to end of line, keeping the newline.
            let end = rest.find('\n').map_or(source.len(), |n| idx + n);
            out.extend(std::iter::repeat_n(' ', end - idx));
            idx = end;
            continue;
        }
        if bytes[idx] == b'"' {
            // String literal: copy verbatim, honoring backslash escapes, so a
            // `//` or a `"` inside it cannot be read as syntax.
            out.push('"');
            idx += 1;
            while idx < bytes.len() {
                match bytes[idx] {
                    b'\\' => {
                        out.push('\\');
                        if let Some(&next) = bytes.get(idx + 1) {
                            out.push(char::from(next));
                        }
                        idx += 2;
                    }
                    b'"' => {
                        out.push('"');
                        idx += 1;
                        break;
                    }
                    _ => {
                        let ch = source[idx..].chars().next().expect("valid boundary");
                        out.push(ch);
                        idx += ch.len_utf8();
                    }
                }
            }
            continue;
        }
        let ch = rest.chars().next().expect("valid boundary");
        out.push(ch);
        idx += ch.len_utf8();
    }
    out
}

/// The repair arm's BODY: from the marker to the `continue;` that
/// re-dispatches the repaired request.
///
/// # Panics
///
/// If the arm does not end in a `continue;` -- without it the repaired
/// request is never re-dispatched, so there is no repair to guard.
fn repair_arm_body(seat: &str) -> &str {
    let arm = &seat[repair_arm_start(seat)..];
    let end = arm.find("continue;").unwrap_or_else(|| {
        panic!(
            "the repair arm must end in a `continue;` so the repaired request is \
             re-dispatched through the loop's gate; without it nothing retries"
        )
    });
    &arm[..end]
}

#[test]
fn the_condition_parser_is_indentation_independent() {
    // The property that makes the draw-order guard trustworthy after a
    // reindent: the condition span is delimited by brace depth, so the same
    // arm at two different indentation levels yields the same conditions.
    let flat = "// m\nif a && budget.draw()\n{\n    body();\n}";
    let nested = "// m\n            if a && budget.draw()\n            {\n                body();\n            }";
    assert_eq!(
        repair_arm_conditions(flat).trim(),
        repair_arm_conditions(nested).trim(),
        "the condition span must not depend on how deeply the arm is indented",
    );
    assert!(
        !repair_arm_conditions(flat).contains("body()"),
        "the body must never fall inside the condition span",
    );
}

#[test]
fn the_condition_parser_looks_past_brackets_inside_the_condition() {
    // A `{`-free condition containing calls and indexing must not have its
    // span cut early, or the guard would read a real condition as absent.
    let arm = "// m\nif f(g(x)) && list[0] && budget.draw()\n{\n    body();\n}";
    let conditions = repair_arm_conditions(arm);
    assert!(conditions.contains("budget.draw()"));
    assert!(!conditions.contains("body()"));
}

#[test]
fn the_conjunct_splitter_ignores_nested_ands() {
    // The order guard counts POSITIONS, so a nested `&&` that split into its
    // own conjunct would renumber the sequence and make the order assertion
    // read a different arm than the one it names.
    let arm = "// m\nif f(a && b) && second && third\n{\n    body();\n}";
    let conjuncts = repair_arm_conjuncts(arm);
    assert_eq!(
        conjuncts,
        vec!["f(a && b)", "second", "third"],
        "an `&&` inside brackets belongs to its conjunct, never to the top-level sequence",
    );
}

#[test]
#[should_panic(expected = "no body-opening")]
fn the_condition_parser_fails_closed_when_the_body_brace_is_unlocatable() {
    // THE fail-closed proof. The predecessor of this parser fell back to
    // "the whole arm" when its indented needle missed, which silently turned
    // every body statement into an apparent condition -- the guard would then
    // pass on exactly the mutation it exists to catch. This asserts the
    // replacement panics instead. Verified as a real behavior rather than by
    // inspection: an unparseable arm cannot be produced in the live source
    // (it would not compile), so the parser is driven directly.
    let unparseable = "// m\nif a && budget.draw()";
    let _ = repair_arm_conditions(unparseable);
}

#[test]
#[should_panic(expected = "repair position left")]
fn the_arm_locator_fails_closed_when_the_marker_is_absent() {
    // Same discipline for the marker: a missing arm must panic rather than
    // let the other guards scan a span that no longer contains the arm.
    let _ = repair_arm_start("fn seat() { no_marker_here(); }");
}

#[test]
#[should_panic(expected = "must end in a `continue;`")]
fn the_body_delimiter_fails_closed_without_a_continue() {
    let _ = repair_arm_body("// count_tokens_strip_repair_arm\nif a { body(); }\n");
}

#[test]
fn the_repair_budget_is_constructed_once_per_request_and_never_per_seat() {
    // A budget constructed inside the walk or inside a seat is exactly the
    // per-target reset the shared ceiling exists to remove, and no
    // behavioral test of this walk can catch it (see the module docs).
    let constructions = SOURCE.matches(BUDGET_CONSTRUCTOR).count();
    assert_eq!(
        constructions, 1,
        "`{BUDGET_CONSTRUCTOR}` must appear exactly once in this module (above the seat \
         walk); {constructions} occurrences means a seat or the walk mints its own \
         allowance",
    );
    for (label, signature) in [
        ("the seat walk", WALK_SIGNATURE),
        ("a seat", SEAT_SIGNATURE),
    ] {
        assert!(
            !fn_region(SOURCE, signature).contains(BUDGET_CONSTRUCTOR),
            "{label} must receive the request's budget by reference, never construct one",
        );
    }
}

#[test]
fn the_seat_walk_threads_the_budget_it_was_given() {
    // The EXACT argument list, over MASKED source. Two gaps this closes over
    // a bare `contains("repair_budget,")`:
    //
    // - A comment anywhere in the walk carrying the old needle satisfied it
    //   while the real call passed something else entirely, so the guard
    //   passed on a walk that no longer threads anything.
    // - `repair_budget,` alone does not say WHICH expression is passed. A
    //   fresh helper-produced budget (`&mut fresh_budget()`, a per-seat
    //   `RepairBudget::per_request()` behind a function, a clone) reads as a
    //   threaded budget at every other assertion while restoring exactly the
    //   per-seat reset the shared ceiling exists to remove -- and no
    //   behavioral test of this walk can see it (the arm is unreachable).
    //
    // So the call is pinned whole: the walk's own `repair_budget` binding,
    // by name, in position.
    let walk = code_only(fn_region(SOURCE, WALK_SIGNATURE));
    let call = ".count_tokens_try_seat(req, candidate, repair_budget, meta)";
    assert!(
        walk.contains(call),
        "the seat walk must call `{call}` verbatim -- its own `repair_budget` binding, not a \
         freshly constructed or helper-produced one -- or every seat it visits gets its own \
         allowance. Masked walk source: {walk}",
    );
}

#[test]
fn the_repair_arm_orders_its_conditions_with_the_budget_draw_last() {
    // The FULL top-level conjunct order, not just "class before draw". Each
    // earlier conjunct is cheaper and side-effect-free; `draw()` mutates. So
    // any reorder that moves `draw()` ahead of a condition charges requests
    // that then decline to repair, and the charge is invisible -- no test of
    // observable behavior can see a budget spent on a request that never
    // repaired. Hence the whole sequence is pinned here.
    let seat = fn_region(SOURCE, SEAT_SIGNATURE);
    let arm = &seat[repair_arm_start(seat)..];
    let conjuncts = repair_arm_conjuncts(arm);

    // Each conjunct is identified by a needle that cannot match a sibling.
    let expected: [(&str, &str); 4] = [
        (
            "the per-seat already-repaired check",
            "replay_repair_attempted",
        ),
        ("the replay-plan presence binding", "replay_plan.as_ref()"),
        ("the repairable-class check", "is_replay_rejection_class"),
        ("the shared budget draw", "repair_budget.draw()"),
    ];
    assert_eq!(
        conjuncts.len(),
        expected.len(),
        "the repair arm must have exactly {} top-level conditions, found {}: {conjuncts:?} -- \
         a new condition must be placed deliberately relative to the mutating `draw()`, so \
         this guard is updated rather than loosened",
        expected.len(),
        conjuncts.len(),
    );
    for (position, (label, needle)) in expected.iter().enumerate() {
        assert!(
            conjuncts[position].contains(needle),
            "top-level condition {position} must be {label} (matching `{needle}`), found \
             `{}`; the conditions are ordered cheapest-and-pure first so the mutating draw \
             runs last",
            conjuncts[position],
        );
    }

    // Stated separately from the positional loop: the draw is the FINAL
    // top-level conjunct. A trailing condition appended after it would keep
    // every index above intact while still charging a request that then
    // declines.
    let last = conjuncts
        .last()
        .expect("the arm has conditions, asserted above");
    assert!(
        last.contains("repair_budget.draw()"),
        "`repair_budget.draw()` must be the LAST top-level conjunct; found `{last}` after it, \
         so a request can be charged and then decline to repair",
    );
    // And it appears exactly once, so "last" cannot be satisfied by a second
    // copy while an earlier one still charges first.
    let draws = conjuncts
        .iter()
        .filter(|c| c.contains("repair_budget.draw()"))
        .count();
    assert_eq!(
        draws, 1,
        "the budget must be drawn by exactly one condition; {draws} draws means a request \
         can be charged more than once for one repair decision",
    );
}

#[test]
fn the_repaired_request_is_stripped_through_the_recalibrating_wrapper_inside_the_arm() {
    // Scoped to the marked ARM BODY, not the whole seat: a wrapper call that
    // drifted outside the arm (into the success path, or below the scrub)
    // strips a request no repair is retrying, and a seat-wide `contains`
    // would keep passing. The exact argument list is pinned because the
    // wrapper's whole purpose is re-stamping THIS attempt's estimate -- a
    // call on some other request, lane, or meta re-stamps the wrong thing.
    let seat = fn_region(SOURCE, SEAT_SIGNATURE);
    let body = repair_arm_body(seat);
    let call = "strip_replay_artifacts_recalibrating(&mut attempt_req, lane, meta)";

    let at = body.find(call).unwrap_or_else(|| {
        panic!(
            "the repair arm's body must call `{call}` -- with exactly those arguments -- so \
             the dispatched body and the calibration estimate describe the same payload"
        )
    });
    // `repair_arm_body` already ends at the `continue;`, so being inside the
    // body IS being before the continue; assert the ordering explicitly
    // anyway, since that is the property a reader needs.
    assert!(
        at < body.len(),
        "the strip must precede the `continue;` that re-dispatches, or the retry sends the \
         unrepaired request",
    );
    assert_eq!(
        SOURCE.matches(call).count(),
        1,
        "exactly one call site: a second copy elsewhere in the module would strip a request \
         no repair is retrying",
    );
}

#[test]
fn the_repair_arm_releases_then_disarms_then_continues_without_debiting_the_breaker() {
    // Order is the property, not mere presence. Releasing the half-open slot
    // must precede disarming the cancellation backstop (disarm-then-release
    // leaves a window where neither owns the slot), and both must precede the
    // `continue` that re-gates -- the re-gate sees `half_open_in_flight` and
    // returns CircuitOpen if the slot is still held, locking the breaker
    // until restart.
    let seat = fn_region(SOURCE, SEAT_SIGNATURE);
    let body = repair_arm_body(seat);

    let release = body
        .find("self.release_probe_slot(&target.state_key)")
        .expect(
            "the repair arm must release the half-open probe slot before re-gating, exactly \
             as the 401 recovery does, or the breaker stays locked open until restart",
        );
    let disarm = body
        .find("probe_guard.disarm()")
        .expect("the repair arm must disarm the cancellation backstop");
    assert!(
        release < disarm,
        "the slot release must precede `probe_guard.disarm()`: disarming first drops the \
         backstop while the slot is still claimed",
    );
    // Both sit inside `repair_arm_body`, which is delimited at the
    // `continue;`, so both provably precede the re-dispatch.

    for debit in [
        "self.record_failure(",
        "self.park_provider(",
        "class_debits(",
    ] {
        assert!(
            !body.contains(debit),
            "the repair arm must not reach `{debit}`: a repairable rejection is not this \
             seat's health signal and must never debit breaker state",
        );
    }
}

#[test]
fn the_seat_learns_nothing_while_its_settlement_has_no_ledger_sink() {
    // This walk's `DispatchMeta` is request-local: nothing drains its
    // learned/cleared rows to the capability-event ledger. Calling the
    // two-phase settlement here would mutate the SHARED learned registry
    // while its event row went nowhere -- the precise state a warm rebuild
    // resurrects a negative from. So the settlement is deliberately absent
    // until it has a sink, and its absence is pinned rather than left to be
    // "fixed" back in by a reader who assumes parity with the messages walks.
    //
    // Needles are RECEIVER-INDEPENDENT: `plan.commit(` would miss the same
    // call through any other binding name (`carry.commit(`, a field, a
    // temporary), which is a rename away from silently re-enabling the
    // mutation this guard forbids. Scanned over CODE only, so the doc comment
    // that explains why these calls are absent does not trip its own guard.
    let seat = code_only(fn_region(SOURCE, SEAT_SIGNATURE));
    for settlement in [".commit(", "settle_success"] {
        assert!(
            !seat.contains(settlement),
            "`{settlement}` must not appear on the token-count path under ANY receiver name: \
             it mutates the shared learned registry and returns a row this walk has nowhere \
             to drain, so the mutation would persist while the event is dropped",
        );
    }
    // BOTH spellings: `learned = true` is the assignment form (mutating a
    // resident `ReplayDegradation`), `learned: true` is the struct-literal
    // form used where the summary is first built. Forbidding only one leaves
    // the other as a one-character path back to a false persistence claim.
    for claim in ["learned = true", "learned: true"] {
        assert!(
            !seat.contains(claim),
            "the degradation summary must not claim a negative was learned on a path that \
             persists nothing; `{claim}` is a false claim in either the assignment or the \
             struct-literal form",
        );
    }
}

#[test]
fn the_masker_keeps_code_and_blanks_prose() {
    // The no-settlement guard's needles would match their own justifying
    // prose, so it scans masked source. If the masker dropped code, that
    // guard would go vacuous -- pinned here rather than trusted.
    let src =
        "let x = plan.commit(1); // mentions .commit( in prose\n// settle_success here\nlet y = 2;";
    let masked = code_only(src);
    assert!(
        masked.contains("plan.commit(1)"),
        "real code must survive masking, or every forbidding guard goes vacuous",
    );
    assert!(
        masked.contains("let y = 2;"),
        "code on later lines must survive",
    );
    assert!(
        !masked.contains("in prose"),
        "trailing comments must be blanked",
    );
    assert!(
        !masked.contains("settle_success"),
        "whole-line comments must be blanked",
    );
}

#[test]
fn the_masker_keeps_code_after_a_string_holding_a_double_slash() {
    // THE gap the naive line-comment stripper had. A `//` inside a string
    // literal is not a comment start, but a `line.find("//")` truncation read
    // it as one and discarded the entire rest of the line -- so a forbidden
    // call sitting after such a string was invisible to every guard scanning
    // masked text, and the guard passed while the call was live. The shape is
    // realistic: a `tracing` field carrying a URL, on the same statement as
    // further code.
    let src = r#"tracing::debug!(url = "https://example.com/x", n = carry.settle_success());"#;
    let masked = code_only(src);
    assert!(
        masked.contains("settle_success"),
        "code AFTER a string containing `//` must survive masking, or a forbidden call hides \
         behind any URL-bearing literal on the same line; masked: {masked}",
    );
    assert!(
        masked.contains("https://example.com/x"),
        "the string literal's own bytes are preserved, so nothing inside it is read as syntax",
    );

    // Positive control for the comment case on the SAME shape: a real comment
    // after that string still gets blanked, so the test above is not passing
    // merely because masking stopped happening.
    let with_comment = format!("{src} // and settle_success_in_prose");
    let masked_with_comment = code_only(&with_comment);
    assert!(
        !masked_with_comment.contains("settle_success_in_prose"),
        "a genuine trailing comment after a string-bearing statement is still blanked",
    );
}

#[test]
fn the_masker_preserves_byte_offsets() {
    // The positional guards compare offsets taken from masked and raw text, so
    // masking must not shift them. Blanking rather than deleting is what keeps
    // that true.
    let src = "let a = 1; // note\nlet b = 2;";
    assert_eq!(
        code_only(src).len(),
        src.len(),
        "masking must preserve length so offsets stay comparable",
    );
}

#[test]
fn a_replay_rejection_is_scrubbed_and_assigned_back_before_it_can_reach_the_caller() {
    // Every `return CountSeatOutcome::Terminal(e)` below the repair arm hands
    // `e` back to the walk, which hands it to the client. A replay
    // rejection's body echoes the reasoning artifact it objected to, so the
    // error must be rebuilt body-free once the repair arm has declined --
    // before any of those returns.
    let seat = fn_region(SOURCE, SEAT_SIGNATURE);
    let call = "replay_rejection_body_free(&e, &cf.class, provider_name)";
    let scrub = seat.find(call).unwrap_or_else(|| {
        panic!(
            "the seat must rebuild a classified replay rejection body-free via `{call}`, the \
             same helper the messages walks use"
        )
    });

    // The scrub's `if let` must be UNCONDITIONAL beyond the binding itself:
    // exactly one top-level conjunct. An added conjunct (`&& attempts_made > 1`,
    // `&& !is_capability_error(&e)`, any narrowing predicate) makes the
    // ASSIGNMENT conditional, so some paths still return the rejection body
    // while the call, the assignment, and their ordering all still look
    // correct to every other assertion here. Arity is the only thing that
    // catches it.
    let if_let_start = seat[..scrub]
        .rfind("if let ")
        .expect("the scrub must be an `if let` whose condition this guard can read");
    let condition_span = &seat[if_let_start + "if let ".len()..];
    let mut depth = 0i32;
    let mut condition_end = condition_span.len();
    for (offset, ch) in condition_span.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '{' if depth == 0 => {
                condition_end = offset;
                break;
            }
            _ => {}
        }
    }
    let scrub_conditions = &condition_span[..condition_end];
    let scrub_conjuncts = top_level_conjuncts(scrub_conditions);
    assert_eq!(
        scrub_conjuncts.len(),
        1,
        "the scrub's `if let` must carry exactly one top-level condition (the binding); found \
         {}: {scrub_conjuncts:?} -- an added conjunct makes the assignment conditional, so \
         some paths return the unscrubbed rejection body while every other assertion here \
         still passes",
        scrub_conjuncts.len(),
    );
    assert!(
        scrub_conjuncts[0].contains(call),
        "the scrub's single condition must be the body-free rebuild itself, found `{}`",
        scrub_conjuncts[0],
    );

    // The RESULT must be assigned back to `e`. Calling the helper and
    // dropping its value compiles (it is not `#[must_use]`) and reads as a
    // scrub while leaving the body on the error every return below hands
    // out -- so the assignment, not the call, is the property.
    let after_call = &seat[scrub + call.len()..];
    let block_end = after_call
        .find('}')
        .expect("the scrub's `if let` block must close");
    let scrub_block = &after_call[..block_end];
    assert!(
        scrub_block.contains("e = body_free"),
        "the scrub's result must be ASSIGNED back to `e` inside the scrub block; calling \
         `{call}` and discarding the value compiles and looks like a scrub while every \
         return below still carries the rejection body. Block was: `{scrub_block}`",
    );

    let arm_end = repair_arm_start(seat) + repair_arm_body(seat).len();
    assert!(
        scrub > arm_end,
        "the scrub must come AFTER the repair arm: a rejection the arm consumed is retried, \
         not returned, and scrubbing earlier would rebuild an error the arm still reads",
    );
    // No terminal return of `e` may precede the scrub. The forwarded-auth
    // return above it is exempt: `forwarded_terminal_status` matches only
    // 401/403/429, which the replay lift (400/422 only) can never produce.
    let after_scrub = &seat[scrub..];
    let terminal_returns_after = after_scrub.matches("CountSeatOutcome::Terminal(e)").count();
    let terminal_returns_total = seat.matches("CountSeatOutcome::Terminal(e)").count();
    assert_eq!(
        terminal_returns_after,
        terminal_returns_total - 1,
        "every terminal return of the upstream error except the forwarded-auth one (401/403/\
         429, a status the replay lift cannot produce) must sit after the scrub, or that path \
         returns the unscrubbed body",
    );
}
