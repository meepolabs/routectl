#!/usr/bin/env python3
"""Per-cell evidence a driver run leaves next to its fixture.

Called by scripts/capture_driver.sh from its cleanup trap, after the daemon
has stopped and before the run workspace (trace log included) is removed.
The layout it writes is documented in docs/DEVELOPMENT.md "Per-cell
evidence"; the snapshot files beside it (health.json, usage.json) are
fetched by the runner itself while the daemon is still up.

Modes:
  extract-lines <kind> <trace>   print what one extractor reads out of a
                                 trace, one record per line (self-test seam;
                                 kind is fidelity, ingress or errors)
  finalize [--trace-live] --dir D --trace T [--snapshot NAME=STATUS]... --exit N
           [--driver-rc N] [--rig-rc N] --lane L --case C
           --expected-ingress E --connection-mode M --turns N
                                 write the trace-derived artifacts and
                                 client_result.json into D; with
                                 --trace-live (the daemon could not be
                                 stopped) the trace is not read and the
                                 trace artifacts are reported missing

Every trace match is anchored on the event boundary the daemon's formatter
draws: with styling on, an event's target is written dimmed and followed by
a dimmed `:` (ESC[2m<target>ESC[0mESC[2m:ESC[0m), and the sink renders any
ESC inside span or event field text as the visible text `\x1b`. So the
first styled target on the raw line is the event's own, and neither a span
value written before it (the client-chosen model name is one) nor a body
written after it can spell one. A line without a styled target contributes
nothing: unstyled text has no boundary a field value cannot forge, which is
why the runner forces styling on for its daemon. The text after the
boundary is ANSI-stripped and must begin with the event message.

Exit codes: 0 written, 2 usage error.
"""

import json
import os
import re
import signal
import subprocess
import sys
import tempfile
from collections.abc import Iterable, Iterator

SCHEMA = "routectl-cell-evidence/1"

# Replicas of emitter spellings: the rig's runtime trees carry scripts/ and
# no crates/, so they cannot be read from source here. capture_driver.test.sh
# welds each one to the Rust source where that tree exists.
LOG_SAFE_TARGET = "routectl_core::log_safe"
INGRESS_MESSAGE = "ingress request body "
ERROR_MESSAGE = "upstream error body "
FIDELITY_TARGET = "routectl_cli::handlers::status::field_verdict_log"
FIDELITY_MESSAGE = "envelope field verdict snapshot"

ANSI = re.compile(r"\x1b\[[0-9;]*m")
# The formatter's dimmed target and dimmed separator. The timestamp is also
# dimmed but starts with a digit, and span names are bold, so the first
# match is the event target.
STYLED_TARGET = re.compile(
    r"\x1b\[2m([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*)\x1b\[0m\x1b\[2m:\x1b\[0m "
)
INGRESS_FIELD = re.compile(r'ingress="([a-z0-9_-]{1,64})"(?: |$)')
ERROR_FIELDS = re.compile(
    r'provider_kind="([^"\\]{1,64})" provider=(?:"(?:[^"\\]|\\.)*"|\S+) status=(\d{3}) body=(.*)$'
)

EXCERPT_MAX_BYTES = 512
# Bounds on what one cell can write. A daemon that logged far more than a
# driver case produces is itself the finding; the cap keeps a runaway cell
# from filling the landing root, and the truncation is recorded so an
# aggregator reads the cell as incomplete rather than as clean.
FIDELITY_MAX_BYTES = 1 << 20
ERROR_MAX_ROWS = 1000
INGRESS_MAX_TOKENS = 16

# An excerpt is written to disk beside the fixture, and an upstream can echo
# a credential into free text no name-keyed redaction sees. Every body is
# therefore run through the fixture scrubber's own checker -- one pattern set,
# not a copy -- and an excerpt whose body it flags, or that could not be
# checked, is replaced whole by this marker.
EXCERPT_OMITTED = "[omitted: credential-shaped content]"
SCRUB_CHECKER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "scrub-fixture.sh")
SCRUB_TIMEOUT_SECONDS = 20
# A dirty batch is re-checked one body per file to find the dirty rows; past
# this many rows every excerpt is omitted instead, keeping the cost bounded.
SCRUB_PER_ROW_MAX = 64
SCRUB_FINDING = re.compile(r"^  (\S+)  \S+$")

# A credential no shape rule knows (a prefix-less key, a new vendor format)
# is still known by VALUE to the run that holds it: the driver credentials
# the daemon resolves through `env://` refs. Account ids are included: the
# ChatGPT account id names the operator's account, and the fixture scrubber
# already refuses it by header name for that reason. Values are read from
# this process's own inherited environment, never from argv, and never
# printed. Values shorter than the floor are ignored: real keys and ids are
# far longer, and a short value would omit excerpts on ordinary text.
HELD_CREDENTIAL_NAME = re.compile(r"^ROUTECTL_DRIVER_[A-Z0-9_]+_(?:API_KEY|TOKEN|ACCOUNT_ID)$")
HELD_CREDENTIAL_MIN_CHARS = 16


def _held_credentials() -> list[str]:
    return [
        value
        for name, value in os.environ.items()
        if HELD_CREDENTIAL_NAME.match(name) and len(value) >= HELD_CREDENTIAL_MIN_CHARS
    ]

TRACE_ARTIFACTS = ("fidelity.log", "ingress_kinds.txt", "upstream_errors.jsonl")
SNAPSHOT_ARTIFACTS = ("health.json", "usage.json")


def _after_target(line: str, target: str, message: str) -> str | None:
    """The ANSI-stripped text after `message` when the event target the
    formatter drew on the raw line is `target` and introduces it, else None."""
    m = STYLED_TARGET.search(line)
    if m is None or m.group(1) != target:
        return None
    rest = ANSI.sub("", line[m.end():])
    if not rest.startswith(message):
        return None
    return rest[len(message):]


def fidelity_line(line: str) -> str | None:
    rest = _after_target(line, FIDELITY_TARGET, FIDELITY_MESSAGE)
    if rest is None or (rest and not rest.startswith(" ")):
        return None
    return ANSI.sub("", line)


def ingress_token(line: str) -> str | None:
    rest = _after_target(line, LOG_SAFE_TARGET, INGRESS_MESSAGE)
    if rest is None:
        return None
    m = INGRESS_FIELD.match(rest)
    return m.group(1) if m else None


def error_row(line: str) -> tuple[dict, str] | None:
    """(row, full body) for an error line; the row's excerpt is not yet
    checked for credentials (see `scrub_excerpts`)."""
    rest = _after_target(line, LOG_SAFE_TARGET, ERROR_MESSAGE)
    if rest is None:
        return None
    m = ERROR_FIELDS.match(rest)
    if not m:
        return None
    body = m.group(3)
    excerpt = body.encode("utf-8")[:EXCERPT_MAX_BYTES].decode("utf-8", "ignore")
    return {"provider_kind": m.group(1), "status": int(m.group(2)), "excerpt": excerpt}, body


# Process group of the scrub checker while one runs. The checker forks grep
# and python3 and runs in its own session so one kill takes the whole group;
# that also means it does not die with this process, so whatever ends this
# process -- its own timeout, or the runner's outer bound -- kills it first.
_checker_group: int | None = None


def _kill_checker_group() -> None:
    if _checker_group is None:
        return
    try:
        os.killpg(_checker_group, signal.SIGKILL)
    except ProcessLookupError:
        pass


def _on_terminate(signum: int, _frame: object) -> None:
    _kill_checker_group()
    sys.exit(128 + signum)


def _scrub_check(directory: str) -> set[str] | None:
    """Names of the files under `directory` the scrubber flags, or None when
    the check could not give a verdict."""
    global _checker_group
    try:
        proc = subprocess.Popen(
            ["bash", SCRUB_CHECKER, "--check", directory],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
            start_new_session=True,
        )
    except OSError:
        return None
    _checker_group = proc.pid
    try:
        _, stderr = proc.communicate(timeout=SCRUB_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        # The group may already be gone (the checker exited after the
        # timeout fired); either way it is reaped, and no verdict is given.
        _kill_checker_group()
        proc.communicate()
        return None
    finally:
        _checker_group = None
    if proc.returncode == 0:
        return set()
    if proc.returncode != 1:
        return None
    flagged = {
        os.path.basename(m.group(1))
        for m in map(SCRUB_FINDING.match, stderr.splitlines())
        if m
    }
    return flagged or None


def _flagged_rows(bodies: list[str], scratch_parent: str) -> set[int] | None:
    """Indexes of the bodies the scrubber flags, or None when it cannot say.

    The bodies are copied under `scratch_parent` -- the trace's own
    directory, inside the run workspace the runner removes -- so a writer
    killed mid-check leaves them only where the trace already was."""
    with tempfile.TemporaryDirectory(prefix=".evidence-scrub.", dir=scratch_parent) as scratch:
        batch = os.path.join(scratch, "batch")
        os.mkdir(batch)
        with open(os.path.join(batch, "bodies.txt"), "w", encoding="utf-8") as fh:
            fh.write(_lines(bodies))
        verdict = _scrub_check(batch)
        if verdict is not None and not verdict:
            return set()
        if verdict is None or len(bodies) > SCRUB_PER_ROW_MAX:
            return None
        rows = os.path.join(scratch, "rows")
        os.mkdir(rows)
        for i, body in enumerate(bodies):
            with open(os.path.join(rows, f"{i}.txt"), "w", encoding="utf-8") as fh:
                fh.write(body + "\n")
        verdict = _scrub_check(rows)
    if not verdict:
        return None
    return {int(name.split(".", 1)[0]) for name in verdict}


def scrub_excerpts(found: list[tuple[dict, str]], scratch_parent: str) -> list[dict]:
    """The rows with every excerpt whose body is credential-shaped, carries a
    credential value this run holds, or could not be checked, replaced by
    EXCERPT_OMITTED."""
    if not found:
        return []
    try:
        flagged = _flagged_rows([body for _, body in found], scratch_parent)
    except OSError:
        flagged = None
    held = _held_credentials()

    def omitted(i: int, body: str) -> bool:
        return flagged is None or i in flagged or any(value in body for value in held)

    return [
        {**row, "excerpt": EXCERPT_OMITTED} if omitted(i, body) else row
        for i, (row, body) in enumerate(found)
    ]


def _trace_dir(trace: str) -> str:
    return os.path.dirname(os.path.abspath(trace))


def _trace_lines(path: str) -> Iterator[str]:
    with open(path, encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            yield raw.rstrip("\r\n")


def scan(path: str) -> tuple[list[str], list[str], list[tuple[dict, str]], list[str]]:
    """Read the trace once; return (fidelity lines, ingress tokens, unscrubbed
    error rows with their bodies, names of truncated artifacts)."""
    fidelity, fidelity_bytes = [], 0
    tokens, rows, truncated = set(), [], set()
    for line in _trace_lines(path):
        kept = fidelity_line(line)
        if kept is not None:
            size = len(kept.encode("utf-8")) + 1
            if fidelity_bytes + size > FIDELITY_MAX_BYTES:
                truncated.add("fidelity.log")
            else:
                fidelity.append(kept)
                fidelity_bytes += size
            continue
        token = ingress_token(line)
        if token is not None:
            if token in tokens or len(tokens) < INGRESS_MAX_TOKENS:
                tokens.add(token)
            else:
                truncated.add("ingress_kinds.txt")
            continue
        row = error_row(line)
        if row is not None:
            if len(rows) < ERROR_MAX_ROWS:
                rows.append(row)
            else:
                truncated.add("upstream_errors.jsonl")
    return fidelity, sorted(tokens), rows, sorted(truncated)


def _write_atomic(directory: str, name: str, text: str) -> None:
    tmp = os.path.join(directory, "." + name + ".tmp")
    with open(tmp, "w", encoding="utf-8") as fh:
        fh.write(text)
    os.replace(tmp, os.path.join(directory, name))


def _lines(items: Iterable[str]) -> str:
    return "".join(item + "\n" for item in items)


def write_trace_artifacts(directory: str, trace: str) -> tuple[dict[str, str], list[str]]:
    """Write the three trace-derived files; return (missing, truncated)."""
    try:
        fidelity, tokens, rows, truncated = scan(trace)
    except OSError as exc:
        reason = "trace unreadable: " + exc.__class__.__name__
        return {name: reason for name in TRACE_ARTIFACTS}, []
    _write_atomic(directory, "fidelity.log", _lines(fidelity))
    _write_atomic(directory, "ingress_kinds.txt", _lines(tokens))
    _write_atomic(
        directory,
        "upstream_errors.jsonl",
        _lines(json.dumps(row, sort_keys=True) for row in scrub_excerpts(rows, _trace_dir(trace))),
    )
    return {}, truncated


def _int_or_none(value: str | None) -> int | None:
    if value is None or value == "":
        return None
    try:
        return int(value)
    except ValueError:
        return None


def _parse_finalize(argv: list[str]) -> tuple[dict[str, str], dict[str, str]]:
    """Return (single-valued options, snapshot name -> fetch status)."""
    opts: dict[str, str] = {}
    snapshots: dict[str, str] = {}
    singles = {
        "--dir", "--trace", "--exit", "--driver-rc", "--rig-rc", "--lane",
        "--case", "--expected-ingress", "--connection-mode", "--turns",
    }
    i = 0
    while i < len(argv):
        flag = argv[i]
        if flag == "--trace-live":
            opts["trace_live"] = "1"
            i += 1
            continue
        if i + 1 >= len(argv):
            raise ValueError(flag + " requires a value")
        value = argv[i + 1]
        if flag == "--snapshot":
            name, sep, status = value.partition("=")
            if not sep or name not in ("health", "usage"):
                raise ValueError("--snapshot takes health=STATUS or usage=STATUS")
            snapshots[name] = status
        elif flag in singles:
            opts[flag[2:].replace("-", "_")] = value
        else:
            raise ValueError("unknown flag " + flag)
        i += 2
    for required in ("dir", "trace", "exit"):
        if required not in opts:
            raise ValueError("--" + required + " is required")
    return opts, snapshots


def finalize(argv: list[str]) -> None:
    opts, statuses = _parse_finalize(argv)
    directory = opts["dir"]
    if opts.get("trace_live"):
        missing, truncated = {name: "daemon still running" for name in TRACE_ARTIFACTS}, []
    else:
        missing, truncated = write_trace_artifacts(directory, opts["trace"])
    for name in ("health", "usage"):
        status = statuses.get(name, "not fetched")
        if status != "ok" or not os.path.isfile(os.path.join(directory, name + ".json")):
            missing[name + ".json"] = status if status != "ok" else "absent after fetch"
    result = {
        "schema": SCHEMA,
        "lane": opts.get("lane", ""),
        "case": opts.get("case", ""),
        "expected_ingress": opts.get("expected_ingress", ""),
        "connection_mode": opts.get("connection_mode", ""),
        "turns": _int_or_none(opts.get("turns")),
        "driver_rc": _int_or_none(opts.get("driver_rc")),
        "rig_rc": _int_or_none(opts.get("rig_rc")),
        "exit": _int_or_none(opts["exit"]),
        "missing": dict(sorted(missing.items())),
        "truncated": truncated,
    }
    _write_atomic(directory, "client_result.json", json.dumps(result, indent=2, sort_keys=True) + "\n")


def extract_lines(kind: str, trace: str) -> None:
    fidelity, tokens, rows, _ = scan(trace)
    if kind == "fidelity":
        out = fidelity
    elif kind == "ingress":
        out = tokens
    elif kind == "errors":
        out = [json.dumps(row, sort_keys=True) for row in scrub_excerpts(rows, _trace_dir(trace))]
    else:
        raise ValueError("unknown extractor " + kind)
    sys.stdout.write(_lines(out))


def main(argv: list[str]) -> int:
    # Set explicitly: a runner in cleanup has INT/TERM/HUP ignored, and that
    # disposition is inherited, so without a handler the outer bound's
    # SIGTERM would be lost and only its SIGKILL would land -- which cannot
    # take the checker group with it.
    for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(signum, _on_terminate)
    try:
        if len(argv) == 3 and argv[0] == "extract-lines":
            extract_lines(argv[1], argv[2])
        elif argv and argv[0] == "finalize":
            finalize(argv[1:])
        else:
            raise ValueError("usage: see the module docstring")
    except ValueError as exc:
        print("cell_evidence: " + str(exc), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
