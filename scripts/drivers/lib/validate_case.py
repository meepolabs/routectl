#!/usr/bin/env python3
"""Validate and read one canonical interaction case file.

The single enforcement point for the case schema documented in
scripts/drivers/cases/README.md: the drivers read their case through this
module, and scripts/drivers.test.sh checks every committed case against
it. A second copy of the rules inside a driver would drift from the
README the moment either changed, and a driver is the last place a bad
case should be caught -- by then a daemon is booted and a client is
mid-session.

Modes:
  --check <case>            validate only; print nothing on success
  --field <name> <case>     print one scalar top-level or knobs field
  --turns <case>            print each turn's prompt, one per line

Exit codes: 0 valid, 1 invalid (reason on stderr), 2 usage error.
"""

import json
import os
import re
import sys

SCHEMA_VERSION = 1

# The id charset also makes a path separator and a `..` segment
# unrepresentable, which matters because the id NAMES a landing directory
# under the corpus root.
ID_RE = re.compile(r"^[a-z0-9]+(-[a-z0-9]+)*$")

WIRE_PATTERNS = frozenset(
    {
        "baseline",
        "tool-use-multiturn",
        "cache-breakpoints",
        "thinking",
        "large-context",
    }
)

TOP_KEYS = frozenset(
    {
        "schema_version",
        "case_id",
        "title",
        "wire_pattern",
        "lane",
        "turns",
        "knobs",
        "notes",
    }
)
REQUIRED_TOP_KEYS = TOP_KEYS - {"notes"}

BOOL_KNOBS = ("tools", "thinking", "cache_breakpoints")
KNOB_KEYS = frozenset(BOOL_KNOBS) | {"context_padding_bytes"}

# One megabyte of context padding already produces a large-context shape.
# The cap exists so a typo'd knob cannot make a run generate a filler tree
# that fills the temp filesystem the hermetic workspace lives on.
MAX_PADDING_BYTES = 8 * 1024 * 1024


class CaseError(Exception):
    """A case file that does not conform to the documented schema."""


def _require_clean_str(value, label):
    if not isinstance(value, str):
        raise CaseError(f"{label} must be a string")
    if not value.strip():
        raise CaseError(f"{label} must not be empty")
    # A prompt crosses a shell argv and lands in a trace log where a
    # newline is a record separator, so a control character in one
    # corrupts the very fixture the case exists to produce.
    if any(ord(ch) < 0x20 or ord(ch) == 0x7F for ch in value):
        raise CaseError(f"{label} must be single-line and free of control characters")
    return value


def _validate_knobs(knobs):
    if not isinstance(knobs, dict):
        raise CaseError("knobs must be an object")
    extra = set(knobs) - KNOB_KEYS
    if extra:
        raise CaseError(f"unknown knobs: {', '.join(sorted(extra))}")
    missing = KNOB_KEYS - set(knobs)
    if missing:
        raise CaseError(f"missing knobs: {', '.join(sorted(missing))}")
    for key in BOOL_KNOBS:
        if not isinstance(knobs[key], bool):
            raise CaseError(f"knobs.{key} must be a boolean")
    padding = knobs["context_padding_bytes"]
    # `isinstance(True, int)` is true in Python, so a boolean here would
    # otherwise pass as 0 or 1 bytes of padding.
    if not isinstance(padding, int) or isinstance(padding, bool):
        raise CaseError("knobs.context_padding_bytes must be an integer")
    if padding < 0:
        raise CaseError("knobs.context_padding_bytes must not be negative")
    if padding > MAX_PADDING_BYTES:
        raise CaseError(
            f"knobs.context_padding_bytes exceeds the {MAX_PADDING_BYTES} byte cap"
        )


def _validate_turns(turns):
    if not isinstance(turns, list) or not turns:
        raise CaseError("turns must be a non-empty array")
    for index, turn in enumerate(turns):
        if not isinstance(turn, dict):
            raise CaseError(f"turns[{index}] must be an object")
        if set(turn) != {"prompt"}:
            raise CaseError(f"turns[{index}] must carry exactly one key, prompt")
        _require_clean_str(turn["prompt"], f"turns[{index}].prompt")


def _lane_config_dir():
    return os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "config")


def validate(path):
    """Return the parsed case at `path`, or raise CaseError."""
    try:
        with open(path, encoding="utf-8") as handle:
            case = json.load(handle)
    except OSError as exc:
        raise CaseError(f"unreadable: {exc}") from exc
    except json.JSONDecodeError as exc:
        raise CaseError(f"not valid JSON: {exc}") from exc

    if not isinstance(case, dict):
        raise CaseError("the case must be a JSON object")

    extra = set(case) - TOP_KEYS
    if extra:
        raise CaseError(f"unknown keys: {', '.join(sorted(extra))}")
    missing = REQUIRED_TOP_KEYS - set(case)
    if missing:
        raise CaseError(f"missing keys: {', '.join(sorted(missing))}")

    if case["schema_version"] != SCHEMA_VERSION:
        raise CaseError(f"schema_version must be {SCHEMA_VERSION}")

    case_id = case["case_id"]
    if not isinstance(case_id, str) or not ID_RE.match(case_id):
        raise CaseError(
            f"case_id {case_id!r} must match {ID_RE.pattern} -- it names the landing directory"
        )
    stem = os.path.basename(path)[: -len(".json")] if path.endswith(".json") else None
    if stem is not None and stem != case_id:
        raise CaseError(f"case_id {case_id!r} does not match the filename stem {stem!r}")

    _require_clean_str(case["title"], "title")
    if "notes" in case:
        _require_clean_str(case["notes"], "notes")

    if case["wire_pattern"] not in WIRE_PATTERNS:
        raise CaseError(
            f"wire_pattern must be one of: {', '.join(sorted(WIRE_PATTERNS))}"
        )

    lane = case["lane"]
    if not isinstance(lane, str) or not ID_RE.match(lane):
        raise CaseError(f"lane {lane!r} must match {ID_RE.pattern}")
    lane_config = os.path.join(_lane_config_dir(), lane + ".toml")
    if not os.path.isfile(lane_config):
        raise CaseError(f"lane {lane!r} has no committed config at {lane_config}")

    _validate_turns(case["turns"])
    _validate_knobs(case["knobs"])
    return case


def _print_field(case, name):
    if name in case and not isinstance(case[name], (dict, list)):
        value = case[name]
    elif name in case["knobs"]:
        value = case["knobs"][name]
    else:
        raise CaseError(f"no scalar field named {name!r}")
    if isinstance(value, bool):
        print("true" if value else "false")
    else:
        print(value)


def main(argv):
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2

    mode = argv[0]
    try:
        if mode == "--check" and len(argv) == 2:
            validate(argv[1])
        elif mode == "--turns" and len(argv) == 2:
            for turn in validate(argv[1])["turns"]:
                print(turn["prompt"])
        elif mode == "--field" and len(argv) == 3:
            _print_field(validate(argv[2]), argv[1])
        else:
            print(__doc__, file=sys.stderr)
            return 2
    except CaseError as exc:
        target = argv[-1]
        print(f"validate_case: {target}: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
