#!/usr/bin/env python3
"""Check that a staged fixture's captured bytes exhibit the wire pattern it claims.

`meta.wire_pattern` is otherwise a RECORDED claim: a case asking for tools
only passes a permission flag to the client, so a `tool-use-multiturn`
fixture with zero tool calls lands, scrubs clean, and is then asserted by
the replay harness as evidence of a shape it never carried. This module is
the predicate side of that claim -- one predicate per pattern token in
`validate_case.py`'s `WIRE_PATTERNS`, read off the fixture's own captured
bytes.

Every predicate keys on the INGRESS side only. That is what stops it
becoming a per-provider shape parser: the case controls what the client
sends, not what any provider dialect the request is translated into looks
like.

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
    if turn.get("role") != "assistant":
        return False
    if any(block.get("type") == "tool_use" for block in _content_blocks(turn)):
        return True
    calls = turn.get("tool_calls")
    return isinstance(calls, list) and bool(calls)


def _carries_tool_result(turn):
    if turn.get("role") == "tool":
        return True
    return any(block.get("type") == "tool_result" for block in _content_blocks(turn))


def _tool_use_multiturn(fixture_dir):
    """An assistant turn carrying a tool-call block AND a LATER turn
    carrying its result.

    An offered `tools` array is not this pattern: the client offers its
    tool list on every request once tools are permitted, so a tools-array
    check would be satisfied by the baseline fixture. What no single-turn
    capture can produce is the RESENT pair -- the client only puts a
    tool_use block and its tool_result on the wire when a later turn
    replays the earlier exchange.
    """
    turns = _read_ingress_body(fixture_dir).get("messages")
    if not isinstance(turns, list) or not turns:
        raise PatternError("the ingress body carries no turn list")

    for index, turn in enumerate(turns):
        if not isinstance(turn, dict) or not _carries_tool_call(turn):
            continue
        for later in turns[index + 1 :]:
            if isinstance(later, dict) and _carries_tool_result(later):
                return
        raise PatternError(
            f"turn {index} carries a tool call but no later turn carries a tool result"
        )

    raise PatternError("no assistant turn carries a tool-call block")


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
