#!/usr/bin/env python3
"""Check that a staged fixture's captured bytes exhibit the wire pattern it claims.

`meta.wire_pattern` is otherwise a RECORDED claim: a case asking for tools
only passes a permission flag to the client, so a `tool-use-multiturn`
fixture with zero tool calls lands, scrubs clean, and is then asserted by
the replay harness as evidence of a shape it never carried. This module is
the predicate side of that claim -- one predicate per pattern token in
`validate_case.py`'s `WIRE_PATTERNS`, read off the fixture's own captured
bytes.

Invoked at the two promotion boundaries: `capture_fixtures.sh`'s
`write_fixture` in driver mode (after the scrub `--check`, before the
promoting `mv`) and `promote_fixture.sh` on the staged copy. A refusal
discards the fixture rather than landing it.

Every predicate keys on the INGRESS side only. That is what stops it
becoming a per-provider shape parser: the case controls what the client
sends, not what any provider dialect the request is translated into looks
like.

Ingress side does not mean Anthropic side. A body-census predicate reads
whichever turn list the captured body actually carries -- `messages` for
the Anthropic and chat-completions shapes, `input` for the Responses
shape -- so one census answers for every ingress dialect. `baseline` is
the deliberate exception: it is ANTHROPIC-ONLY and refuses a claim on any
other ingress dialect by name, because the floor request of a
non-Anthropic client carries structurally non-configurable tools and a
per-dialect tool-count floor would pin a client VERSION into a predicate.

Two evidence sources, and no others:
  structural.txt      the ingress structural summary line (baseline,
                      thinking, cache-breakpoints)
  ingress_request.json  the captured inbound body (tool-use-multiturn,
                      large-context)

The structural-line predicates are a port of the Rust reference logic in
crates/routectl-cli/tests/wire_pattern_weld.rs. Two properties of that
port are load-bearing and are asserted in scripts/drivers.test.sh: token
parsing is exact on the `key=value` name, and the ingress line is selected
by its `direction` token rather than by position.

Usage:
  verify_pattern.py <fixture-dir> <pattern>
  verify_pattern.py --structural-line <pattern>   # line on stdin

The second mode exists for the shared classification set that the Rust
reference logic reads from the same file: a classification record is one
structural line, so feeding one through the same code path the fixture mode
uses is what makes a Python/Rust disagreement observable.

Exit codes: 0 the fixture exhibits the pattern, 1 it does not (reason on
stderr), 2 usage error.
"""

import json
import os
import sys

# The ingress structural summary and the captured inbound body. Driver
# mode already refuses a fixture missing either request-side structural
# summary, so an absent structural line here means a fixture that was
# never promotable.
STRUCTURAL_FILE = "structural.txt"
INGRESS_BODY_FILE = "ingress_request.json"

# Both spellings of "thinking off". The real client sends
# `thinking: {"type": "disabled"}`, which the summary renders as the
# explicit token `thinking_shape=disabled` rather than as an absent field,
# so a predicate that only knew the absent form would read a disabled
# block as an active one.
INACTIVE_THINKING_SHAPES = ("", "disabled")

# The ingress dialect token `baseline` is scoped to, as the ingress
# structural line's own `id` field spells it (IngressAdapter::id()). A
# client speaking another dialect offers tools its own runtime requires
# rather than tools a case permitted, so `tools_len == 0` describes a
# request that client cannot send; keying the floor on a measured
# per-client tool count would pin a client VERSION into a predicate and
# lie at that client's next release.
ANTHROPIC_INGRESS_ID = "anthropic"

# The turn-list keys a captured ingress body can carry, and the item /
# block types each dialect spells a tool call and a tool result with. The
# census picks the list by which key the body ACTUALLY carries: the claimed
# dialect lives in meta.json beside the wire_pattern claim, so gating one
# claim on the other would verify nothing.
ANTHROPIC_TURNS_KEY = "messages"
RESPONSES_TURNS_KEY = "input"
TURNS_KEYS = (ANTHROPIC_TURNS_KEY, RESPONSES_TURNS_KEY)
TOOL_CALL_TYPES = ("tool_use", "function_call")
TOOL_RESULT_TYPES = ("tool_result", "function_call_output")

# A `large-context` claim must mean the CONTEXT was large, not that the
# client's own preamble was. Claude Code's floor request -- one short
# prompt, every capability knob off -- already carries ~28 KB of system
# prompt and reminder text, so any floor near that size is satisfied by
# every fixture the corpus will ever hold, including the baseline. This
# floor sits an order of magnitude above that preamble and well under the
# padding a large-context case generates, so it separates the two without
# encoding either exactly.
MIN_LARGE_CONTEXT_BYTES = 256 * 1024

# Pattern tokens deliberately absent from the predicate table: they have a
# closed-set entry and no case, so no fixture can claim one yet. Listed
# rather than silently missing, because a missing predicate is what this
# module exists to prevent.
# --- BEGIN DEFERRED_PATTERNS ---
DEFERRED_PATTERNS = (
    "mcp-tools",
)
# --- END DEFERRED_PATTERNS ---


class PatternError(Exception):
    """A fixture whose captured bytes do not exhibit its claimed pattern."""


def token_value(line, key):
    """Value of the `key=value` token named `key`, or None.

    Token-exact by construction: a substring search for `thinking_shape=`
    also matches `output_thinking_shape=...`, which would let an unrelated
    field satisfy a clause about a missing one.
    """
    for token in line.split():
        name, sep, value = token.partition("=")
        if sep and name == key:
            return value
    return None


def ingress_line(structural):
    """The ingress structural line, selected by its `direction` token
    rather than by position -- the file's line order is a capture
    convention, not a guarantee."""
    for line in structural.splitlines():
        direction = token_value(line, "direction")
        if direction is not None and direction.strip('"') == "ingress":
            return line
    return None


def _read_ingress_line(fixture_dir):
    path = os.path.join(fixture_dir, STRUCTURAL_FILE)
    try:
        with open(path, encoding="utf-8") as handle:
            structural = handle.read()
    except OSError as exc:
        raise PatternError(f"unreadable {STRUCTURAL_FILE}: {exc}") from exc
    line = ingress_line(structural)
    if line is None:
        raise PatternError(f'{STRUCTURAL_FILE} carries no direction="ingress" line')
    return line


def _ingress_body_path(fixture_dir):
    path = os.path.join(fixture_dir, INGRESS_BODY_FILE)
    if not os.path.isfile(path):
        raise PatternError(f"no {INGRESS_BODY_FILE}; the capture recorded no ingress body")
    return path


def _read_ingress_body(fixture_dir):
    path = _ingress_body_path(fixture_dir)
    try:
        with open(path, encoding="utf-8") as handle:
            body = json.load(handle)
    except OSError as exc:
        raise PatternError(f"unreadable {INGRESS_BODY_FILE}: {exc}") from exc
    except UnicodeDecodeError as exc:
        raise PatternError(f"{INGRESS_BODY_FILE} is not valid UTF-8: {exc}") from exc
    except json.JSONDecodeError as exc:
        raise PatternError(f"{INGRESS_BODY_FILE} is not valid JSON: {exc}") from exc
    if not isinstance(body, dict):
        raise PatternError(f"{INGRESS_BODY_FILE} is not a JSON object")
    return body


def _count_token(line, key):
    raw = token_value(line, key)
    if raw is None:
        raise PatternError(f"{key} token absent")
    try:
        return int(raw)
    except ValueError as exc:
        raise PatternError(f"{key}={raw} is not a count") from exc


# ---------------------------------------------------------------------
# Structural-line predicates (ported; see the module docstring)
# ---------------------------------------------------------------------


def line_is_baseline(line):
    # The dialect scope is checked FIRST and by name. A non-Anthropic
    # client's floor request carries tools its own runtime requires, so
    # reporting `tools_len=8, want 0` would describe the refusal as a
    # capture defect when it is a scope statement.
    dialect = token_value(line, "id")
    if dialect is None:
        raise PatternError("id token absent; baseline is Anthropic-only and cannot be scoped")
    dialect = dialect.strip('"')
    if dialect != ANTHROPIC_INGRESS_ID:
        raise PatternError(
            f"ingress dialect {dialect!r} is not {ANTHROPIC_INGRESS_ID!r}; "
            "baseline is Anthropic-only, because another client's floor request "
            "carries tools it does not choose and a per-dialect tool-count floor "
            "would pin that client's version into this predicate"
        )

    tools_len = _count_token(line, "tools_len")
    if tools_len != 0:
        raise PatternError(f"tools_len={tools_len}, want 0")

    shape = token_value(line, "thinking_shape")
    if shape is not None and shape not in INACTIVE_THINKING_SHAPES:
        raise PatternError(f"thinking_shape={shape} is active")

    cache_control_count = _count_token(line, "cache_control_count")
    if cache_control_count != 0:
        raise PatternError(f"cache_control_count={cache_control_count}, want 0")


def line_is_thinking(line):
    shape = token_value(line, "thinking_shape")
    if shape is None:
        raise PatternError("thinking_shape token absent")
    if shape in INACTIVE_THINKING_SHAPES:
        raise PatternError(f"thinking_shape={shape!r} is not an active thinking block")


def line_is_cache_breakpoints(line):
    count = _count_token(line, "cache_control_count")
    if count < 1:
        raise PatternError(f"cache_control_count={count}, want at least 1")


# The three predicates a structural line alone decides. The two body-census
# predicates below need the captured body and so are absent here -- and from
# the shared classification set.
STRUCTURAL_PREDICATES = {
    "baseline": line_is_baseline,
    "thinking": line_is_thinking,
    "cache-breakpoints": line_is_cache_breakpoints,
}


# ---------------------------------------------------------------------
# Ingress-body census predicates
# ---------------------------------------------------------------------


def _content_blocks(turn):
    content = turn.get("content")
    if not isinstance(content, list):
        return []
    return [block for block in content if isinstance(block, dict)]


def _carries_tool_call(turn):
    # A Responses input item IS the call: `{"type": "function_call", ...}`
    # carries no role, so a role gate applied to it would read every
    # Responses tool loop as no tool loop at all.
    if turn.get("type") in TOOL_CALL_TYPES:
        return True
    if turn.get("role") != "assistant":
        return False
    if any(block.get("type") in TOOL_CALL_TYPES for block in _content_blocks(turn)):
        return True
    calls = turn.get("tool_calls")
    return isinstance(calls, list) and bool(calls)


def _carries_tool_result(turn):
    if turn.get("type") in TOOL_RESULT_TYPES:
        return True
    if turn.get("role") == "tool":
        return True
    return any(block.get("type") in TOOL_RESULT_TYPES for block in _content_blocks(turn))


def _turn_list(body):
    """The turn list the captured body carries, and the key that held it.

    Selected by which key is PRESENT rather than by any recorded dialect
    claim: `meta.ingress_kind` sits beside the `wire_pattern` claim this
    module exists to check, so reading one to decide the other would
    verify a body against its own label.

    A body carrying MORE THAN ONE turn-list key is refused rather than
    resolved by precedence. No dialect emits two, so such a body is
    hand-edited or hybrid, and picking one list would let the pattern be
    satisfied by turns the other key contradicts -- a gate that reads
    around ambiguity instead of failing closed on it.
    """
    present = [key for key in TURNS_KEYS if key in body]
    if len(present) > 1:
        raise PatternError(
            "the ingress body carries more than one turn list "
            f"({', '.join(present)}); no dialect emits two, so which one "
            "the claim refers to is ambiguous"
        )
    for key in TURNS_KEYS:
        turns = body.get(key)
        if isinstance(turns, list) and turns:
            return turns, key
    raise PatternError(
        "the ingress body carries no turn list under any of "
        f"{', '.join(TURNS_KEYS)}"
    )


def _tool_use_multiturn(fixture_dir):
    """A turn carrying a tool-call AND a LATER turn carrying its result.

    An offered `tools` array is not this pattern: the client offers its
    tool list on every request once tools are permitted, so a tools-array
    check would be satisfied by the baseline fixture. What no single-turn
    capture can produce is the RESENT pair -- the client only puts a tool
    call and its result on the wire when a later turn replays the earlier
    exchange.

    One census over every ingress dialect: the pair is spelled as an
    assistant `tool_use` block plus a `tool_result` block in the Anthropic
    shape, as a `tool_calls` array plus a `role: "tool"` turn in the
    chat-completions shape, and as `function_call` / `function_call_output`
    input items in the Responses shape. The ORDER clause is the same in all
    three, which is why they share one predicate rather than branching.
    """
    turns, turns_key = _turn_list(_read_ingress_body(fixture_dir))

    for index, turn in enumerate(turns):
        if not isinstance(turn, dict) or not _carries_tool_call(turn):
            continue
        for later in turns[index + 1 :]:
            if isinstance(later, dict) and _carries_tool_result(later):
                return
        raise PatternError(
            f"{turns_key}[{index}] carries a tool call but no later turn carries a tool result"
        )

    raise PatternError(f"no {turns_key} turn carries a tool-call")


def _large_context(fixture_dir):
    # The body is PARSED before it is measured. A byte count alone would
    # let a truncated or non-JSON capture above the floor satisfy the
    # pattern, and the promotion slot cannot distinguish a large body from
    # a large pile of bytes -- the shape claim is about a request, not
    # about a file size.
    _read_ingress_body(fixture_dir)
    size = os.path.getsize(_ingress_body_path(fixture_dir))
    if size < MIN_LARGE_CONTEXT_BYTES:
        raise PatternError(
            f"the ingress body is {size} bytes, under the {MIN_LARGE_CONTEXT_BYTES} byte floor"
        )


def _on_ingress_line(line_predicate):
    """Adapt a structural-line predicate to a fixture directory."""

    def check(fixture_dir):
        line_predicate(_read_ingress_line(fixture_dir))

    return check


# One entry per token in validate_case.py's WIRE_PATTERNS. A token with no
# entry here is refused rather than waved through, so adding a pattern to
# the closed set without a predicate cannot promote an unverified fixture.
#
# The sentinels bound the block that the cross-language weld in
# crates/routectl-cli/tests/wire_pattern_weld.rs parses as text; that weld
# is what makes an omission here red rather than merely unverified.
# --- BEGIN PREDICATES ---
PREDICATES = {
    "baseline": _on_ingress_line(line_is_baseline),
    "thinking": _on_ingress_line(line_is_thinking),
    "cache-breakpoints": _on_ingress_line(line_is_cache_breakpoints),
    "tool-use-multiturn": _tool_use_multiturn,
    "large-context": _large_context,
}
# --- END PREDICATES ---


def verify(fixture_dir, pattern):
    """Raise PatternError unless the fixture at `fixture_dir` exhibits
    `pattern`."""
    predicate = PREDICATES.get(pattern)
    if predicate is None:
        raise PatternError(
            f"no predicate for wire_pattern {pattern!r} "
            f"(deferred: {', '.join(DEFERRED_PATTERNS)})"
        )
    if not os.path.isdir(fixture_dir):
        raise PatternError("not a fixture directory")
    predicate(fixture_dir)


def verify_structural_line(line, pattern):
    """Raise PatternError unless `line` exhibits `pattern`.

    Refuses a body-census pattern outright: a structural line cannot decide
    one, and answering "no" would read as a classification rather than as
    the wrong question.
    """
    predicate = STRUCTURAL_PREDICATES.get(pattern)
    if predicate is None:
        raise PatternError(
            f"{pattern!r} is not decided by a structural line "
            f"(structural: {', '.join(sorted(STRUCTURAL_PREDICATES))})"
        )
    predicate(line)


def main(argv):
    structural_mode = len(argv) == 2 and argv[0] == "--structural-line"
    if not structural_mode and len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2

    if structural_mode:
        pattern = argv[1]
        target = "the structural line on stdin"
    else:
        target, pattern = argv

    try:
        if structural_mode:
            verify_structural_line(sys.stdin.read(), pattern)
        else:
            verify(target, pattern)
    except PatternError as exc:
        print(f"verify_pattern: {target}: {pattern}: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
