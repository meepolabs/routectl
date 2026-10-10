#!/usr/bin/env bash
# Turn ONE upstream rejection, as routectl already logged it, into a
# committed rejection fixture.
#
#   scripts/capture-rejection.sh --log <file> --request-id <id> \
#                                --dialect <dialect> --case <case> \
#                                --note <text> [--out-root <dir>] [--force]
#
#   --log         a daemon log written by a file sink with ROUTECTL_LOG
#                 enabling `routectl_core::log_safe=debug` (the body) and
#                 `routectl_router::router::dispatch=debug` (the lifted
#                 type).
#                 The source line is the DEBUG `upstream error body` event:
#                 already redacted and capped in-process, so this script
#                 never sees a raw upstream body.
#   --request-id  the request whose rejection to capture. Matched only in
#                 the span prefix of a line, never inside a logged body.
#   --dialect     the egress dialect the rejection came from. Closed set:
#                 anthropic-api, bedrock, openai-compat, openai-responses.
#                 Also selects the line: its `provider_kind` must map to
#                 this dialect, so a request that fell back across lanes
#                 cannot land one lane's envelope under another's name.
#   --case        the fixture name: lowercase letters, digits, `.`, `_`,
#                 `-`, starting with a letter or digit.
#   --note        the fixture's `note`: what was captured, on what, when,
#                 and its evidence tier. Required -- it is the one part of
#                 the fixture the log cannot supply.
#   --out-root    the rejection corpus root. Defaults to
#                 crates/routectl-router/tests/fixtures/rejections.
#   --force       replace an existing fixture. Without it an existing file
#                 is a refusal.
#
# Writes `<out-root>/<dialect>/<case>.json`:
#
#   {"note": ..., "status": <int>, "lifted_type": ..., "body": ...}
#
# `lifted_type` is present only when the same request logged a non-empty
# `upstream_type` for that attempt (the router's class-decision events),
# i.e. the discriminator routectl lifted onto Error::Upstream. `body` is
# the logged body byte-for-byte; for a JSON envelope that is serde_json's
# compact re-serialization, so key order may differ from the wire.
#
# REFUSES, writing nothing, when: the dialect is unknown; no line for that
# request id and dialect exists; two lines disagree; the body carries a cap
# or elision marker (`[truncated at`, `... [truncated]`,
# `(json body, N bytes, not excerpted)`, `<html error page, N bytes>`) --
# a capped body is not the envelope; the fixture already exists without
# --force; or `scrub-fixture.sh --check` finds residual personal data.
# The scrub gate runs on the staged file, so what it scans is byte-for-byte
# what lands.
#
# Exit codes:
#   0  fixture written
#   1  refused: no usable line, a marker, a scrub finding, or an existing
#      fixture
#   2  usage error, or a prerequisite (python3, the scrub gate) missing
#
# Requires python3.
# --- END USAGE ---

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRUB="$ROOT/scripts/scrub-fixture.sh"
DEFAULT_OUT_ROOT="$ROOT/crates/routectl-router/tests/fixtures/rejections"
DIALECTS=("anthropic-api" "bedrock" "openai-compat" "openai-responses")

usage() {
  sed -n '2,/^# --- END USAGE ---$/p' "$0" | sed '$d'
}

fatal() {
  echo "capture-rejection: $1" >&2
  exit 2
}

refuse() {
  echo "capture-rejection: REFUSED $1" >&2
  exit 1
}

dialect_is_known() {
  local d
  for d in "${DIALECTS[@]}"; do
    [ "$d" = "$1" ] && return 0
  done
  return 1
}

LOG=""
REQUEST_ID=""
DIALECT=""
CASE_NAME=""
NOTE=""
OUT_ROOT="$DEFAULT_OUT_ROOT"
FORCE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --log) [ $# -ge 2 ] || fatal "--log requires a value"; LOG="$2"; shift 2 ;;
    --request-id) [ $# -ge 2 ] || fatal "--request-id requires a value"; REQUEST_ID="$2"; shift 2 ;;
    --dialect) [ $# -ge 2 ] || fatal "--dialect requires a value"; DIALECT="$2"; shift 2 ;;
    --case) [ $# -ge 2 ] || fatal "--case requires a value"; CASE_NAME="$2"; shift 2 ;;
    --note) [ $# -ge 2 ] || fatal "--note requires a value"; NOTE="$2"; shift 2 ;;
    --out-root) [ $# -ge 2 ] || fatal "--out-root requires a value"; OUT_ROOT="$2"; shift 2 ;;
    --force) FORCE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) fatal "unknown arg: $1" ;;
  esac
done

[ -n "$LOG" ] || fatal "--log is required (see --help)"
[ -n "$REQUEST_ID" ] || fatal "--request-id is required (see --help)"
[ -n "$DIALECT" ] || fatal "--dialect is required (one of: ${DIALECTS[*]})"
[ -n "$CASE_NAME" ] || fatal "--case is required (see --help)"
[ -n "$NOTE" ] || fatal "--note is required (see --help)"
[ -n "$OUT_ROOT" ] || fatal "--out-root cannot be empty"
dialect_is_known "$DIALECT" || fatal "unknown --dialect '$DIALECT' (one of: ${DIALECTS[*]})"
# The same charset the daemon accepts for an inbound x-request-id, so any
# id it could have logged is expressible and nothing else is.
[[ "$REQUEST_ID" =~ ^[A-Za-z0-9._:-]{1,128}$ ]] ||
  fatal "--request-id must be 1-128 chars of [A-Za-z0-9._:-]"
[[ "$CASE_NAME" =~ ^[a-z0-9][a-z0-9._-]*$ ]] ||
  fatal "--case must match [a-z0-9][a-z0-9._-]*"
[ -r "$LOG" ] || fatal "log not readable: $LOG"
command -v python3 >/dev/null 2>&1 || fatal "python3 is required"
[ -r "$SCRUB" ] || fatal "scrub gate not found at $SCRUB; refusing to capture"

DEST="$OUT_ROOT/$DIALECT/$CASE_NAME.json"
if [ -e "$DEST" ] && [ "$FORCE" -eq 0 ]; then
  refuse "fixture already exists: $DEST (pass --force to replace it)"
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
STAGED="$STAGE/$CASE_NAME.json"

# Exit 0 staged, 3 refused (reason on stderr). Anything else is a script
# failure, never a verdict.
extract_fixture() {
  python3 - "$LOG" "$REQUEST_ID" "$DIALECT" "$NOTE" "$STAGED" <<'PY'
import json
import re
import sys

log_path, request_id, dialect, note, out_path = sys.argv[1:6]

TARGET = "routectl_core::log_safe: "
MESSAGE = "upstream error body "
KIND_TO_DIALECT = {
    "anthropic": "anthropic-api",
    "bedrock-invoke": "bedrock",
    "bedrock-converse": "bedrock",
    "openai-compat": "openai-compat",
    "openai-responses": "openai-responses",
}
ANSI = re.compile(r"\x1b\[[0-9;]*m")
ID_RE = re.compile(r"request_id=" + re.escape(request_id) + r"(?![A-Za-z0-9._:-])")
FIELDS = re.compile(
    r'provider_kind="([^"\\]*)" provider=(?:"(?:[^"\\]|\\.)*"|\S+) status=(\d+) body=(.*)$'
)
TYPE_RE = re.compile(r'(?<![A-Za-z0-9_])upstream_type="([^"\\]*)"')
JSON_ELIDED = re.compile(r"\(json body, \d+ bytes, not excerpted\)")
HTML_ELIDED = re.compile(r"<html error page, \d+ bytes>")


def refuse(reason):
    print("capture-rejection: REFUSED " + reason, file=sys.stderr)
    sys.exit(3)


def body_record(line):
    p = line.find(TARGET)
    if p < 0 or not ID_RE.search(line[:p]):
        return None
    rest = line[p + len(TARGET):]
    if not rest.startswith(MESSAGE):
        return None
    m = FIELDS.match(rest[len(MESSAGE):])
    if not m:
        refuse("an `upstream error body` line for this request id has an unexpected field layout")
    return m.group(1), int(m.group(2)), m.group(3)


def marker_in(body):
    if "[truncated at " in body:
        return "a `[truncated at N bytes]` cap marker"
    if body.endswith("... [truncated]"):
        return "a `... [truncated]` cap marker"
    if JSON_ELIDED.search(body):
        return "a `(json body, N bytes, not excerpted)` marker"
    if HTML_ELIDED.search(body):
        return "an `<html error page, N bytes>` marker"
    return None


def lifted_type(lines, start):
    for line in lines[start + 1:]:
        if not ID_RE.search(line):
            continue
        if body_record(line) is not None:
            return None
        m = TYPE_RE.search(line)
        if m:
            return m.group(1) or None
    return None


with open(log_path, encoding="utf-8", errors="replace") as fh:
    lines = [ANSI.sub("", raw.rstrip("\r\n")) for raw in fh]

records = []
other_kinds = set()
for idx, line in enumerate(lines):
    rec = body_record(line)
    if rec is None:
        continue
    kind, status, body = rec
    if KIND_TO_DIALECT.get(kind) == dialect:
        records.append((idx, status, body))
    else:
        other_kinds.add(kind)

if not records:
    seen = (" (seen provider_kind: " + ", ".join(sorted(other_kinds)) + ")") if other_kinds else ""
    refuse("no DEBUG `upstream error body` line for request id " + request_id
           + " on dialect " + dialect + seen)
if len({(s, b) for _, s, b in records}) > 1:
    refuse(str(len(records)) + " differing `upstream error body` lines for request id "
           + request_id + " on dialect " + dialect + "; capture a request that hit one lane")

idx, status, body = records[0]
if not body:
    refuse("the logged body is empty")
marker = marker_in(body)
if marker:
    refuse("the logged body carries " + marker + "; it is not the whole envelope")

fixture = {"note": note, "status": status}
lifted = lifted_type(lines, idx)
if lifted:
    fixture["lifted_type"] = lifted
fixture["body"] = body
with open(out_path, "w", encoding="ascii") as out:
    out.write(json.dumps(fixture, indent=2, ensure_ascii=True))
    out.write("\n")
PY
}

rc=0
extract_fixture || rc=$?
case "$rc" in
  0) ;;
  3) exit 1 ;;
  *) fatal "extraction failed (status $rc)" ;;
esac

rc=0
bash "$SCRUB" --check "$STAGED" >&2 || rc=$?
case "$rc" in
  0) ;;
  1) refuse "scrub-fixture.sh --check found residual personal data; nothing written" ;;
  *) fatal "scrub gate could not run (status $rc); nothing written" ;;
esac

mkdir -p "$OUT_ROOT/$DIALECT"
if [ -e "$DEST" ] && [ "$FORCE" -eq 0 ]; then
  refuse "fixture appeared during capture: $DEST (pass --force to replace it)"
fi
mv -f "$STAGED" "$DEST"
echo "capture-rejection: wrote $DEST"
