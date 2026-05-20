#!/usr/bin/env bash
# Capture per-request body+summary fixtures from the routectl TRACE log.
#
# Reads /tmp/routectl-trace.log (or `--log <path>`), finds completed
# request_ids since the last capture, and writes per-request fixture
# directories under `crates/routectl-cli/tests/fixtures/captured/<id>/`.
#
# A request counts as "complete" when its trace carries either an
# `upstream success body` line (non-stream path) OR a `stream summary`
# line (stream path). The capture filters to the Anthropic ingress
# only; egress provider is anthropic-api, openai-compat, or
# openai-responses.
#
# State: the script writes the timestamp of the last seen completion
# to `crates/routectl-cli/tests/fixtures/captured/.last_capture_ts`
# and resumes from there on the next run, so periodic invocations
# from the 3-min heartbeat don't re-capture the same requests.
#
# Usage:
#   scripts/capture_fixtures.sh [--log /tmp/routectl-trace.log] \
#                               [--out crates/routectl-cli/tests/fixtures/captured] \
#                               [--limit 4] [--force]
#
# `--limit N` caps the number of NEW requests captured this run
# (the periodic hook passes 4 to mirror the heartbeat's window).
# `--force` ignores the resume marker and re-captures from the start
# of the log.

# set -e so a partial-fixture mid-write failure aborts the script
# instead of silently writing a half-poisoned directory. set -u
# catches typo'd variable references early. pipefail is NOT used:
# several pipelines below intentionally early-terminate the
# producer (`grep ... | head -1`), which leaves the producer with
# a SIGPIPE-induced non-zero exit under pipefail and triggers
# set -e abort on a successful capture path.
set -eu

LOG="/tmp/routectl-trace.log"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/crates/routectl-cli/tests/fixtures/captured"
LIMIT=0
FORCE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --log) LOG="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --limit) LIMIT="$2"; shift 2 ;;
    --force) FORCE=1; shift ;;
    -h|--help) sed -n '1,32p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

mkdir -p "$OUT"
MARKER="$OUT/.last_capture_ts"

# Sweep any stale per-request tmp dirs from a prior crashed run.
# We rename atomically (mv tmp -> $OUT/$id) at the end of each
# write_fixture, so any `.tmp.*` left behind here is poison from
# a previous abort. Naming uses `.tmp.<id>.XXXXXX` so the directory
# scan `[ -d "$OUT/$id" ]` (no dot prefix) cannot collide with
# legitimate captures.
find "$OUT" -maxdepth 1 -name '.tmp.*' -type d -exec rm -rf {} + 2>/dev/null || true

if [ ! -r "$LOG" ]; then
  echo "trace log not readable: $LOG" >&2
  exit 1
fi

# Sed pattern that strips ANSI color escapes.
strip_ansi() { sed -E 's/\x1b\[[0-9;]*m//g'; }

# Resume from the last captured timestamp (lexicographic compare works
# on ISO-8601 with fixed-width timestamps).
since="1970-01-01T00:00:00Z"
if [ "$FORCE" = 0 ] && [ -r "$MARKER" ]; then
  since="$(cat "$MARKER" 2>/dev/null)"
  [ -z "$since" ] && since="1970-01-01T00:00:00Z"
fi

stripped="$(mktemp)"
trap 'rm -f "$stripped"' EXIT
strip_ansi < "$LOG" > "$stripped"

# Collect request_ids that have a completion marker after `since` AND
# whose ingress span is anthropic AND whose provider_kind is one of
# the three in scope. The completion line carries `request_id=...`
# and a timestamp at the start of the line.
in_scope_ids() {
  awk -v since="$since" '
    /ingress request body ingress="anthropic"/ {
      if (match($0, /request_id=[0-9a-f-]+/)) {
        id = substr($0, RSTART+11, RLENGTH-11)
        anthropic_in[id] = 1
      }
    }
    /(provider_kind="anthropic"|provider_kind="openai-compat"|provider_kind="openai-responses")/ {
      if (match($0, /request_id=[0-9a-f-]+/)) {
        id = substr($0, RSTART+11, RLENGTH-11)
        if (match($0, /provider_kind="[^"]+"/)) {
          k = substr($0, RSTART+15, RLENGTH-16)
          provider_kind[id] = k
        }
      }
    }
    /upstream success body|stream summary/ {
      if (match($0, /request_id=[0-9a-f-]+/)) {
        id = substr($0, RSTART+11, RLENGTH-11)
        ts = substr($0, 1, 27)  # ISO-8601 with millisecond precision
        # `>=` (not `>`) so two requests completing in the same
        # millisecond are not dropped on the next run. The
        # `[ -d "$OUT/$id" ] && continue` check downstream
        # prevents re-capturing the request whose ts matches the
        # marker.
        if (ts >= since) {
          completed[id] = ts
        }
      }
    }
    END {
      for (id in completed) {
        if (anthropic_in[id] && provider_kind[id]) {
          print completed[id] "\t" id "\t" provider_kind[id]
        }
      }
    }
  ' "$stripped" | sort -k1,1
}

# For one request_id, write its fixture directory.
#
# Atomicity contract: writes go to a per-request tmp directory
# `$OUT/.tmp.<id>.XXXXXX` and are promoted to `$OUT/<id>` via a
# single mv(1) only after every file lands. A crash between the
# first file write and the mv leaves a `.tmp.*` directory behind,
# which the startup sweep prunes on the next run -- the final
# `$OUT/<id>` directory is never half-populated, so the
# `[ -d "$OUT/$id" ] && continue` idempotency guard in the caller
# can be trusted. The manifest append happens AFTER the mv so a
# dangling manifest entry never points to a missing directory.
write_fixture() {
  local id="$1" ts="$2" pkind="$3"
  local dst="$OUT/$id"
  local tmp
  tmp="$(mktemp -d "$OUT/.tmp.$id.XXXXXX")"

  # Pull every line for this request.
  local lines
  lines="$(grep -F "request_id=$id" "$stripped" || true)"
  if [ -z "$lines" ]; then
    rm -rf "$tmp"
    return 0
  fi

  # meta.json: high-level fields.
  local alias model
  alias="$(echo "$lines" | grep -oE '_with_options\{alias=[a-z0-9_-]+' | head -1 | sed 's/.*alias=//')"
  model="$(echo "$lines" | grep -oE '(complete|stream)\{provider=[^ ]+ model=[a-zA-Z0-9._:/-]+' | head -1 | sed 's/.*model=//')"
  local stream_flag finish input_tokens output_tokens total_tokens
  stream_flag="false"
  if echo "$lines" | grep -q 'stream summary'; then
    stream_flag="true"
    local ss
    ss="$(echo "$lines" | grep 'stream summary direction="upstream"' | head -1)"
    finish="$(echo "$ss" | grep -oE 'finish_reason="[^"]+"' | cut -d= -f2 | tr -d '"' || echo unknown)"
    input_tokens="$(echo "$ss" | grep -oE 'prompt_tokens=[0-9]+' | cut -d= -f2 || echo 0)"
    output_tokens="$(echo "$ss" | grep -oE 'completion_tokens=[0-9]+' | cut -d= -f2 || echo 0)"
    total_tokens="$(echo "$ss" | grep -oE 'total_tokens=[0-9]+' | cut -d= -f2 || echo 0)"
  fi

  # Body extraction. The tracing field layout is
  #
  #   TS LEVEL spans: target: <message text> <fields...> body=<JSON> redact_prompts_enabled=<bool>
  #
  # so the structural `body=` marker is always the FIRST `body=` on
  # the line (any earlier "body" in spans/message text lacks the
  # `=`). The previous `sed -E 's/.*body=//'` was greedy and would
  # match the LAST `body=` instead, corrupting any captured body
  # whose JSON content itself contained the literal substring
  # `body=` (e.g. a user prompt about log output). awk + index()
  # gives us first-occurrence semantics. The trailing
  # `redact_prompts_enabled=<bool>` field is stripped so the saved
  # JSON has no log fields appended.
  extract_body() {
    local needle="$1"
    # Pure awk pipeline (no `grep | head`) to avoid SIGPIPE on the
    # producer when set -e is on. awk filters to lines containing
    # the message-name needle, finds the FIRST `body=` field on
    # the matched line, strips the trailing
    # `redact_prompts_enabled=<bool>` field, and exits after the
    # first match so only one body is emitted per needle.
    echo "$lines" | awk -v needle="$needle" '
      index($0, needle) {
        i = index($0, "body=")
        if (i == 0) next
        rest = substr($0, i + 5)
        sub(/ redact_prompts_enabled=(true|false)$/, "", rest)
        print rest
        exit
      }
    '
  }

  local b_ing b_out b_uok b_egr
  b_ing="$(extract_body 'ingress request body')"
  b_out="$(extract_body 'outgoing request body')"
  b_uok="$(extract_body 'upstream success body')"
  b_egr="$(extract_body 'egress response body')"

  [ -n "$b_ing" ] && echo "$b_ing" > "$tmp/ingress_request.json"
  [ -n "$b_out" ] && echo "$b_out" > "$tmp/outgoing_request.json"
  [ -n "$b_uok" ] && echo "$b_uok" > "$tmp/upstream_response.json"
  [ -n "$b_egr" ] && echo "$b_egr" > "$tmp/egress_response.json"

  # Structural summary lines (two: ingress + outgoing direction).
  echo "$lines" | grep 'structural summary' | head -2 > "$tmp/structural.txt" || true
  # Stream summary lines (two: upstream + egress direction).
  echo "$lines" | grep 'stream summary' > "$tmp/stream.txt" || true

  # meta.json -- emit by hand to avoid jq dependency.
  cat > "$tmp/meta.json" <<META
{
  "request_id": "$id",
  "captured_at_ts": "$ts",
  "alias": "${alias:-}",
  "model": "${model:-}",
  "provider_kind": "${pkind}",
  "stream": $stream_flag,
  "finish_reason": "${finish:-}",
  "input_tokens": ${input_tokens:-0},
  "output_tokens": ${output_tokens:-0},
  "total_tokens": ${total_tokens:-0},
  "has_ingress_body": $([ -n "$b_ing" ] && echo true || echo false),
  "has_outgoing_body": $([ -n "$b_out" ] && echo true || echo false),
  "has_upstream_response": $([ -n "$b_uok" ] && echo true || echo false),
  "has_egress_response": $([ -n "$b_egr" ] && echo true || echo false)
}
META

  # Atomically promote the tmp directory into place. Until this
  # rename succeeds the final `$OUT/<id>` directory does not exist;
  # an interrupted run leaves a `.tmp.*` dir that the next startup
  # sweep removes.
  mv "$tmp" "$dst"

  # Append to manifest.jsonl AFTER the rename so a dangling manifest
  # entry never points to a missing directory. One JSONL line per
  # request, append-only.
  cat >> "$OUT/manifest.jsonl" <<MANIFEST_LINE
{"request_id":"$id","captured_at":"$ts","alias":"${alias:-}","model":"${model:-}","provider_kind":"${pkind}","stream":$stream_flag,"finish_reason":"${finish:-}","input_tokens":${input_tokens:-0},"output_tokens":${output_tokens:-0},"total_tokens":${total_tokens:-0}}
MANIFEST_LINE

  echo "$id"
}

# Iterate over completions in chronological order, apply --limit if set.
captured=0
latest_ts=""
while IFS=$'\t' read -r ts id pkind; do
  [ -z "$id" ] && continue
  [ -d "$OUT/$id" ] && continue          # already captured (defensive idempotency)
  write_fixture "$id" "$ts" "$pkind" >/dev/null
  captured=$((captured + 1))
  latest_ts="$ts"
  if [ "$LIMIT" -gt 0 ] && [ "$captured" -ge "$LIMIT" ]; then
    break
  fi
done < <(in_scope_ids)

# Update resume marker only if we captured something.
if [ -n "$latest_ts" ]; then
  echo "$latest_ts" > "$MARKER"
fi

echo "captured=$captured since=$since latest=$latest_ts out=$OUT"
