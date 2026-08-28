#!/usr/bin/env bash
# Capture per-request body+summary fixtures from the routectl TRACE log.
#
# Reads /tmp/routectl-trace.log (or `--log <path>`), finds completed
# request_ids since the last capture, and writes per-request fixture
# directories under `crates/routectl-cli/tests/fixtures/captured/<id>/`.
#
# A request counts as "complete" when its trace carries either an
# `upstream success body` line (non-stream path) OR a `stream summary`
# line (stream path). The capture covers any ingress dialect and any
# egress provider routectl emits a provider_kind for. The captured
# meta.json records both `ingress_kind` and `provider_kind` so a
# downstream consumer can dispatch on either.
#
# meta.json carries NO file-presence flags: which optional files a
# fixture directory holds is read from the directory listing by the
# replay loader. The tmp-then-rename promotion below already makes a
# fixture directory all-or-nothing, so a second copy of that fact in
# meta.json could only ever disagree with the filesystem -- and did.
# The one schema is documented in docs/REPLAY-FIXTURES.md.
#
# State: the script writes the timestamp of the last seen completion
# to `crates/routectl-cli/tests/fixtures/captured/.last_capture_ts`
# and resumes from there on the next run, so periodic invocations
# don't re-capture the same requests.
#
# Usage:
#   scripts/capture_fixtures.sh [--log /tmp/routectl-trace.log] \
#                               [--out crates/routectl-cli/tests/fixtures/captured] \
#                               [--limit 4] [--force] [--allow-unsafe-out] \
#                               [--driver-mode]
#
# `--limit N` caps the number of NEW requests captured this run
# (periodic runs typically pass a small limit).
# `--force` ignores the resume marker and re-captures from the start
# of the log.
# `--out` is confined to the default captured dir (which is gitignored)
# because fixtures carry RAW headers -- auth included when the daemon
# runs with ROUTECTL_TRACE_HEADERS. `--allow-unsafe-out` lifts that
# guard for a deliberate out-of-tree capture.
#
# Environment pins recorded verbatim into meta.json when set, empty when
# not (a live-box capture cannot observe them from the trace):
#   ROUTECTL_FIXTURE_CASE_ID          scenario identity for rerun diffs
#   ROUTECTL_FIXTURE_CONFIG_SHA       hash of the config in force
#   ROUTECTL_FIXTURE_CONNECTION_MODE  how the client reached routectl
#   ROUTECTL_FIXTURE_WIRE_PATTERN     wire shape the case claims to cover
#
# TWO CAPTURE MODES, TWO POLICIES.
#
# Default (live-box): a trace drained from a real session. It genuinely
# cannot observe the four pins above, so an empty pin is honest; the
# landing directory is keyed on `request_id`; a missing outgoing
# structural summary warns; scrubbing is write-only.
#
# `--driver-mode`: a hermetic capture produced by a driver that KNOWS
# every pin, so an empty pin is a bug rather than a fact:
#
#   * All four pins are MANDATORY. An unset one aborts the run naming
#     the variable, because an empty case id collapses every case in a
#     lane onto one landing directory and the corpus silently
#     overwrites itself.
#   * Landing directories key on `(lane, case_id)` --
#     `<out>/<lane>/<case_id>/` -- so a rerun of the same case RE-LANDS
#     on the same path and produces a DIFF instead of a fresh sibling.
#     The rerun REPLACES the previous directory: the old one is renamed
#     aside into a `.tmp.stale.*` name, the new one moves into place,
#     then the old one is deleted, so a reader either sees the whole
#     previous fixture or the whole new one. `request_id` stays in
#     meta.json for traceability, it just no longer names the directory.
#     A case id therefore has to be a path-safe SCENARIO name
#     (`tools-multiturn-01`) and must never be derived from the
#     environment -- a hostname or a real path in it is personal data
#     that the scrub gate below refuses, since `--check` scans
#     meta.json too.
#   * A missing structural summary on either direction FAILS that
#     fixture: half the structural evidence is not a canonical fixture.
#   * `scrub-fixture.sh --check` runs after `--write` and a non-zero
#     exit refuses the promotion. A driver fixture is canonical by
#     construction or it is not landed.
#   * The lane must be CLASSIFIED by the scrub gate --
#     `scrub-fixture.sh --lane-known <lane>` exits non-zero and the
#     fixture does not land. A `--check` pass on a lane whose credential
#     shape nobody has classified proves nothing, so an unclassified
#     lane fails closed. A lane the gate lists as having no
#     prefix-detectable shape (with the reason recorded) counts as
#     classified: that is a verdict, not ignorance.
#
# Exit codes:
#   0  the run completed; `captured=<n>` on stdout
#   1  a pin was unset (driver mode), the trace log was unreadable, or a
#      fixture was refused -- a refusal aborts the RUN, so a driver
#      runner reads any non-zero exit as "this case produced no fixture"
#   2  usage error, or an --out outside the captured tree
#   3  DRIVER MODE ONLY: the run completed, refused nothing, and landed
#      zero fixtures -- the driven case produced no completed request (a
#      429, an upstream that returned no success body, a client that died
#      before sending). Deliberately NOT folded into 1: this is retryable,
#      while "we refused a fixture we produced" never is. In LIVE-BOX mode
#      zero captures is the normal quiet-window answer and exits 0.
# --- END USAGE ---

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
ALLOW_UNSAFE_OUT=0
DRIVER_MODE=0

# Landing paths promoted by this run, space-delimited. Only driver mode
# reads it: two requests in one driver trace share the run's single case
# id, so without this the second would overwrite the first.
DRIVER_LANDED=""

# Print the header block as usage. Delimited by a sentinel rather than a
# line count: a magic `1,NNp` range silently starts cutting content the
# moment the header grows, and the driver-mode policy is exactly the part
# a caller needs to read.
usage() {
  sed -n '2,/^# --- END USAGE ---$/p' "$0" | sed '$d'
}

# Workspace package version, stamped into every meta.json + manifest
# entry for forward-compat. Pulled once at startup from the workspace
# Cargo.toml so a version bump mid-run cannot mix versions across one
# capture batch.
ROUTECTL_VERSION="$(grep -E '^version = ' "$ROOT/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')"

# Fixture-format major written into every meta.json. Must match
# FIXTURE_SCHEMA_VERSION in
# crates/routectl-cli/tests/common/replay/loader.rs, which refuses any
# other major outright.
SCHEMA_VERSION=1

# The single owner of fixture scrubbing. Every fixture is passed through
# `--write` before promotion; a driver landing a fixture runs `--check`
# afterwards to refuse anything the write pass could not safely rewrite.
# Absent script is a hard failure, never an unscrubbed capture.
SCRUB="$ROOT/scripts/scrub-fixture.sh"
if [ ! -r "$SCRUB" ]; then
  echo "capture_fixtures: scrub script not found at $SCRUB; refusing to capture" >&2
  exit 1
fi

# The single owner of --out path confinement, shared with every other
# script that writes capture output to a caller-supplied directory.
# Absent library is a hard failure, never an unconfined write.
CONFINE_LIB="$ROOT/scripts/drivers/lib/confine.sh"
if [ ! -r "$CONFINE_LIB" ]; then
  echo "capture_fixtures: confinement library not found at $CONFINE_LIB; refusing to capture" >&2
  exit 1
fi
# shellcheck source=scripts/drivers/lib/confine.sh
. "$CONFINE_LIB"

# Escape a value for use inside a JSON string literal. The meta.json and
# manifest lines below are emitted by hand (no jq dependency), so every
# interpolated STRING value must come through here or a value carrying a
# quote silently produces invalid JSON -- the rig would exit 0, promote
# the fixture, and the loader would skip it forever. Both reachable
# sources are real: the environment pins are set programmatically by a
# driver, and the client name / version are parsed out of a
# CLIENT-CONTROLLED `user-agent` header.
#
# Backslash FIRST, then quote, so the backslash pass cannot double-escape
# the backslash the quote pass just introduced. Control characters are
# not handled: the values here come from single trace-log lines and from
# environment variables, neither of which can carry a raw newline into a
# field without breaking the extraction upstream of this point.
json_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

# Normalize a traced `provider_kind` into the LANE vocabulary --
# `ProviderEntry::kind_str()` in
# crates/routectl-router/src/config/schema.rs, the vocabulary a lane's
# class is derived from. Three spellings exist for one concept: the
# providers-crate PROVIDER_KIND consts (`anthropic`), the two Bedrock
# api_shape kinds (`bedrock-invoke` / `bedrock-converse`, which
# kind_str() does not split), and the config tokens. Normalizing HERE
# means no consumer needs a mapping table.
#
# An unmapped token prints a warning and yields the EMPTY string rather
# than passing an unknown spelling through: a lane-gated consumer
# refuses a fixture with no lane, so the fixture is still captured but
# cannot be mistaken for a lane it was not verified to be.
#
# The two Bedrock spellings collapse to one lane here, and that is the
# ONLY place the api-shape distinction is lost -- `meta.provider_kind`
# keeps the raw token, so a consumer that needs to tell an invoke capture
# from a converse one reads it there. There is deliberately no separate
# api_shape field: it would duplicate a token already on disk.
normalize_lane() {
  case "$1" in
    anthropic)                        printf 'anthropic-api\n' ;;
    openai-compat)                    printf 'openai-compat\n' ;;
    openai-responses)                 printf 'openai-responses\n' ;;
    gemini)                           printf 'gemini\n' ;;
    bedrock-invoke|bedrock-converse)  printf 'bedrock\n' ;;
    *)
      echo "capture_fixtures: unmapped provider_kind '$1'; leaving lane empty" >&2
      printf '\n'
      ;;
  esac
}

while [ $# -gt 0 ]; do
  case "$1" in
    --log) [ $# -ge 2 ] || { echo "--log requires a value" >&2; exit 2; }; LOG="$2"; shift 2 ;;
    --out) [ $# -ge 2 ] || { echo "--out requires a value" >&2; exit 2; }; OUT="$2"; shift 2 ;;
    --limit) [ $# -ge 2 ] || { echo "--limit requires a value" >&2; exit 2; }; LIMIT="$2"; shift 2 ;;
    --force) FORCE=1; shift ;;
    --allow-unsafe-out) ALLOW_UNSAFE_OUT=1; shift ;;
    --driver-mode) DRIVER_MODE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# Driver mode fails closed on the four pins. `:?` aborts under `set -u`
# naming the variable, which is the whole point: an unset pin in a driver
# run is a bug in the driver, and an empty case_id would collapse every
# case in the lane onto one landing directory before anyone noticed. The
# live-box path below never reaches this block and keeps today's
# tolerance.
if [ "$DRIVER_MODE" = 1 ]; then
  : "${ROUTECTL_FIXTURE_CASE_ID:?driver mode requires a case id (ROUTECTL_FIXTURE_CASE_ID)}"
  : "${ROUTECTL_FIXTURE_CONFIG_SHA:?driver mode requires a config sha (ROUTECTL_FIXTURE_CONFIG_SHA)}"
  : "${ROUTECTL_FIXTURE_CONNECTION_MODE:?driver mode requires a connection mode (ROUTECTL_FIXTURE_CONNECTION_MODE)}"
  : "${ROUTECTL_FIXTURE_WIRE_PATTERN:?driver mode requires a wire pattern (ROUTECTL_FIXTURE_WIRE_PATTERN)}"

  # The case id becomes two path components' worth of directory name, so
  # a separator or a traversal segment in it would land the fixture
  # outside the lane directory -- past the --out confinement check, which
  # ran on OUT alone.
  case "$ROUTECTL_FIXTURE_CASE_ID" in
    */*|.|..)
      echo "capture_fixtures: refusing case id '$ROUTECTL_FIXTURE_CASE_ID': it names the landing directory," >&2
      echo "so it must be a single path-safe scenario name (e.g. tools-multiturn-01)." >&2
      exit 2
      ;;
  esac
fi

# Confine --out to the default captured dir unless the operator
# explicitly opts out. Fixtures carry RAW headers (auth included when
# the daemon runs with ROUTECTL_TRACE_HEADERS) and the default tree is
# gitignored; writing them into an arbitrary -- possibly git-tracked --
# path risks committing secrets. OUT keeps its lexically-collapsed form
# for the write path; the containment test itself lives in
# scripts/drivers/lib/confine.sh, which is the only copy.
OUT="$(abspath_lexical "$OUT")"
DEFAULT_OUT_ABS="$(abspath_lexical "$ROOT/crates/routectl-cli/tests/fixtures/captured")"
if [ "$ALLOW_UNSAFE_OUT" = 0 ]; then
  confine_out_under "$OUT" "$DEFAULT_OUT_ABS"
fi

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

# Collect request_ids that have a completion marker after `since` and
# whose trace also carries an `ingress request body` and a
# `provider_kind=` field. Any ingress dialect and any provider_kind
# value is in scope. The completion line carries `request_id=...` and
# a timestamp at the start of the line.
in_scope_ids() {
  awk -v since="$since" '
    /ingress request body ingress="[^"]+"/ {
      if (match($0, /request_id=[0-9a-f-]+/)) {
        id = substr($0, RSTART+11, RLENGTH-11)
        ingress_seen[id] = 1
        if (match($0, /ingress="[^"]+"/)) {
          ingress_kind[id] = substr($0, RSTART+9, RLENGTH-10)
        }
      }
    }
    /provider_kind="[^"]+"/ {
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
        if (ingress_seen[id] && provider_kind[id]) {
          # An unextractable ingress token stays EMPTY rather than
          # becoming a sentinel word: empty means unpinned everywhere
          # else in the schema, and `unknown` would be a value in
          # neither the IngressAdapter::id() vocabulary nor the match
          # arms of any consumer. No apostrophes in this comment: the
          # awk program is single-quoted.
          print completed[id] "\t" id "\t" provider_kind[id] "\t" ingress_kind[id]
        }
      }
    }
  ' "$stripped" | sort -k1,1
}

# For one request_id, write its fixture directory.
#
# Atomicity contract: writes go to a per-request tmp directory
# `$OUT/.tmp.<id>.XXXXXX` and are promoted to the landing path via a
# single mv(1) only after every file lands. A crash between the
# first file write and the mv leaves a `.tmp.*` directory behind,
# which the startup sweep prunes on the next run -- the landing
# directory is never half-populated, so the
# `[ -d "$OUT/$id" ] && continue` idempotency guard in the caller
# can be trusted. The manifest append happens AFTER the mv so a
# dangling manifest entry never points to a missing directory.
#
# The landing path differs by mode: `$OUT/<request_id>` for a live-box
# capture, `$OUT/<lane>/<case_id>` in driver mode (see the header). It is
# resolved late, after the lane is normalized, because the lane is
# derived from the trace rather than passed in.
write_fixture() {
  local id="$1" ts="$2" pkind="$3" ikind="${4:-}"
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

  # Body extraction. The tracing line layout is
  #
  #   TS LEVEL spans: routectl_core::log_safe: <message> <fields...> body=<JSON> redact_prompts_enabled=<bool>
  #
  # We ANCHOR on the structural message position: find the first
  # `routectl_core::log_safe: ` (the tracing target, emitted exactly once
  # per line) and require the message immediately after it to START with
  # the needle. This is what keeps capture correct even when the request
  # BODY itself contains the needle text or a quoted routectl log line
  # (e.g. a coding session about routectl's own logging): content copies
  # live after `body=`, past the first target occurrence, so they can
  # never select the wrong line. Within the matched message we take the
  # FIRST `body=` (the real field -- the message text and the fields
  # before it carry no `body=`) to end-of-line, then strip the trailing
  # `redact_prompts_enabled=<bool>` field.
  extract_body() {
    local needle="$1"
    echo "$lines" | awk -v needle="$needle" -v target="routectl_core::log_safe: " '
      {
        p = index($0, target)
        if (p == 0) next
        mstart = p + length(target)
        if (substr($0, mstart, length(needle)) != needle) next
        rest = substr($0, mstart)
        i = index(rest, "body=")
        if (i == 0) next
        val = substr(rest, i + 5)
        sub(/ redact_prompts_enabled=(true|false)$/, "", val)
        print val
        exit
      }
    '
  }

  # Header extraction. Reciprocal of the parsing contract documented in
  # crates/routectl-core/src/log_safe.rs (the header-trace section): the
  # four canonical needles are the HDR_MSG_* consts there, and `headers`
  # is the LAST field on the line. Same structural anchoring as
  # extract_body -- match the needle only at the message position right
  # after the first `routectl_core::log_safe: `, so a header needle never
  # selects a body line (or a body-content copy of the needle) -- then
  # take everything after the first `headers=` to end-of-line (only a
  # trailing space to strip; header lines carry no `redact_prompts_enabled`).
  extract_headers() {
    local needle="$1"
    echo "$lines" | awk -v needle="$needle" -v target="routectl_core::log_safe: " '
      {
        p = index($0, target)
        if (p == 0) next
        mstart = p + length(target)
        if (substr($0, mstart, length(needle)) != needle) next
        rest = substr($0, mstart)
        i = index(rest, "headers=")
        if (i == 0) next
        val = substr(rest, i + 8)
        sub(/[[:space:]]+$/, "", val)
        print val
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

  # Header fixtures (opt-in via ROUTECTL_TRACE_HEADERS on the daemon).
  # Each is written only when its trace line was present, mirroring
  # the body files above.
  local h_ing h_out h_uok h_egr
  h_ing="$(extract_headers 'ingress request headers')"
  h_out="$(extract_headers 'outgoing request headers')"
  h_uok="$(extract_headers 'upstream response headers')"
  h_egr="$(extract_headers 'egress response headers')"

  [ -n "$h_ing" ] && echo "$h_ing" > "$tmp/ingress_request.headers.json"
  [ -n "$h_out" ] && echo "$h_out" > "$tmp/outgoing_request.headers.json"
  [ -n "$h_uok" ] && echo "$h_uok" > "$tmp/upstream_response.headers.json"
  [ -n "$h_egr" ] && echo "$h_egr" > "$tmp/egress_response.headers.json"

  # Client identity. Name + version come from the captured ingress
  # `user-agent` (`claude-cli/2.1.167 (external, cli)` -> `claude-cli` +
  # `2.1.167`), the only client self-report on the wire. A live-box
  # capture cannot observe its own connection mode, case identity, or
  # config hash, so those come from the environment and stay EMPTY when
  # unset -- an empty pin is honest, a guessed one reads as drift later.
  local client_name client_version
  client_name="$(printf '%s' "$h_ing" | grep -oE '\["user-agent","[^"/]+' | head -1 | sed 's/.*,"//')"
  client_version="$(printf '%s' "$h_ing" | grep -oE '\["user-agent","[^"/]+/[0-9][^ "]*' | head -1 | sed 's#.*/##')"
  local lane
  lane="$(normalize_lane "$pkind")"

  # Structural summary lines, ONE PER REQUEST-SIDE DIRECTION.
  #
  # Anchored the same way as extract_body: the needle must sit at the
  # message position right after the first `routectl_core::log_safe: `,
  # and the direction is matched in the FIELDS of that same line (the
  # `direction=` precedent is the stream-summary selection below). An
  # unanchored `grep 'structural summary' | head -2` selected an ingress
  # request BODY line whose JSON content happened to carry the phrase and
  # then dropped the real outgoing summary off the end of `head -2` --
  # measurably, on 15% of an existing corpus. Body text rides a different
  # event under `body=`, so anchoring on the emitter plus the event name
  # makes content copies unselectable, and picking each direction
  # explicitly makes the selection independent of line order and count.
  extract_structural() {
    local direction="$1"
    echo "$lines" | awk -v direction="$direction" \
                        -v needle="structural summary" \
                        -v target="routectl_core::log_safe: " '
      {
        p = index($0, target)
        if (p == 0) next
        mstart = p + length(target)
        if (substr($0, mstart, length(needle)) != needle) next
        rest = substr($0, mstart + length(needle))
        if (index(rest, "direction=\"" direction "\"") == 0) next
        print
        exit
      }
    '
  }

  local s_ing s_out missing
  s_ing="$(extract_structural ingress)"
  s_out="$(extract_structural outgoing)"
  missing=""
  [ -z "$s_ing" ] && missing="ingress"
  [ -z "$s_out" ] && missing="${missing:+$missing and }outgoing"

  # A fixture missing a direction's structural summary cannot have that
  # direction's wire shape checked against anything. In driver mode that
  # is a failed capture -- a canonical fixture carries both halves of its
  # structural evidence or it is not landed. Live-box mode warns and
  # keeps the fixture: a drained session's log is whatever the daemon
  # happened to emit, and a partial trace is still evidence.
  if [ -n "$missing" ]; then
    if [ "$DRIVER_MODE" = 1 ]; then
      echo "capture_fixtures: no $missing structural summary for $id; discarding the fixture" >&2
      rm -rf "$tmp"
      return 1
    fi
    echo "capture_fixtures: WARN no $missing structural summary for $id" >&2
  fi

  {
    [ -n "$s_ing" ] && printf '%s\n' "$s_ing"
    [ -n "$s_out" ] && printf '%s\n' "$s_out"
    :
  } > "$tmp/structural.txt"

  # Stream summary lines (two: upstream + egress direction).
  echo "$lines" | grep 'stream summary' > "$tmp/stream.txt" || true

  # meta.json -- emit by hand to avoid jq dependency. ONE schema: every
  # key here is a key the replay loader's FixtureMeta reads, apart from
  # the triage-only fields noted in docs/REPLAY-FIXTURES.md. No
  # file-presence flags (the directory listing is the only record).
  #
  # EVERY string value goes through json_escape. The three token counts,
  # schema_version, and stream are numeric / boolean and must NOT be
  # quoted or escaped.
  local j_id j_ts j_version j_alias j_model j_case j_sha j_wire
  local j_cname j_cversion j_cmode j_ikind j_pkind j_lane j_finish
  j_id="$(json_escape "$id")"
  j_ts="$(json_escape "$ts")"
  j_version="$(json_escape "$ROUTECTL_VERSION")"
  j_alias="$(json_escape "${alias:-}")"
  j_model="$(json_escape "${model:-}")"
  j_case="$(json_escape "${ROUTECTL_FIXTURE_CASE_ID:-}")"
  j_sha="$(json_escape "${ROUTECTL_FIXTURE_CONFIG_SHA:-}")"
  j_wire="$(json_escape "${ROUTECTL_FIXTURE_WIRE_PATTERN:-}")"
  j_cname="$(json_escape "${client_name:-}")"
  j_cversion="$(json_escape "${client_version:-}")"
  j_cmode="$(json_escape "${ROUTECTL_FIXTURE_CONNECTION_MODE:-}")"
  j_ikind="$(json_escape "$ikind")"
  j_pkind="$(json_escape "$pkind")"
  j_lane="$(json_escape "$lane")"
  j_finish="$(json_escape "${finish:-}")"

  cat > "$tmp/meta.json" <<META
{
  "schema_version": $SCHEMA_VERSION,
  "request_id": "$j_id",
  "captured_at_ts": "$j_ts",
  "routectl_version": "$j_version",
  "alias": "$j_alias",
  "model": "$j_model",
  "case_id": "$j_case",
  "config_sha": "$j_sha",
  "wire_pattern": "$j_wire",
  "client": {
    "name": "$j_cname",
    "version": "$j_cversion",
    "connection_mode": "$j_cmode"
  },
  "ingress_kind": "$j_ikind",
  "provider_kind": "$j_pkind",
  "lane": "$j_lane",
  "stream": $stream_flag,
  "finish_reason": "$j_finish",
  "input_tokens": ${input_tokens:-0},
  "output_tokens": ${output_tokens:-0},
  "total_tokens": ${total_tokens:-0}
}
META

  # Scrub before promoting. One owner for scrubbing:
  # scripts/scrub-fixture.sh --write rewrites the contributor's own home
  # path (captured bodies echo system-reminder text embedding it, in both
  # the literal form and the dash-encoded `.claude/projects/-home-...`
  # dir-name form) and redacts the value of every credential-shaped header
  # while keeping its name. Auth redaction happens HERE, at write time,
  # because a corpus that ever held a live bearer token is uncommittable in
  # practice no matter what a later scan says.
  #
  # A scrub failure aborts the capture: the tmp directory is removed rather
  # than promoted, so an unscrubbed fixture never reaches the corpus. It is
  # `return 1` and not a bare failure so `set -e` cannot promote it.
  if ! bash "$SCRUB" --write "$tmp"; then
    echo "capture_fixtures: scrub failed for $id; discarding the fixture" >&2
    rm -rf "$tmp"
    return 1
  fi

  # Driver mode additionally REFUSES to promote what the write pass could
  # not fully clean. `--check` exits 1 on residual personal data and 2 on
  # a usage / prerequisite problem; either way the fixture does not land,
  # because a driver corpus is canonical by construction. The live-box
  # path stays write-only: its corpus is local-only by policy, and a
  # refusal there would throw away evidence that cannot be recaptured.
  #
  # `--check` scans EVERY file under the fixture, meta.json included, so a
  # case id carrying a hostname or a real home path is refused by this
  # gate. Case ids are scenario names for exactly that reason.
  if [ "$DRIVER_MODE" = 1 ] && ! bash "$SCRUB" --check "$tmp"; then
    echo "capture_fixtures: scrub check refused $id; not promoting the fixture" >&2
    rm -rf "$tmp"
    return 1
  fi

  # ORDERING CONTRACT: `--write`, then `--check` on the FULL bytes, then
  # any size reduction, then promote. Nothing reduces a fixture today. If a
  # reduction is ever added -- a padding descriptor, an elision sentinel, a
  # truncation of a large block -- it belongs HERE, after the `--check`
  # above, and never before it.
  #
  # A reduction applied before the gate would hide a credential inside the
  # region it replaced: the gate proves only what it read, so bytes dropped
  # first are bytes nothing ever scanned. The regions a reduction would
  # target are the large `tool_result` blocks, and those carry the client's
  # own framing -- `file_path` values, line-numbered file content,
  # directory listings -- so a third party's home path sits deep inside
  # exactly the bytes a reduction would drop. That is what the
  # `home-prefix` and `home-prefix-encoded` classes exist to catch, and
  # they catch it only by scanning the whole file.
  #
  # The order is load-bearing in the other direction too: `--write`
  # neutralizes the contributor's own home path, so `--check` verifies the
  # rewritten bytes rather than the raw capture.

  # Resolve the landing path. Driver mode keys on `(lane, case_id)` so a
  # rerun of the same case re-lands on the same path and diffs; live-box
  # mode keys on request_id. An empty lane means normalize_lane could not
  # map the traced provider_kind, which in driver mode would collapse the
  # path to `$OUT/<case_id>` and put a fixture nothing can gate into the
  # canonical corpus.
  local dst
  if [ "$DRIVER_MODE" = 1 ]; then
    if [ -z "$lane" ]; then
      echo "capture_fixtures: no lane for $id (provider_kind '$pkind'); not promoting the fixture" >&2
      rm -rf "$tmp"
      return 1
    fi
    # A lane the scrub gate holds no credential-shape classification for
    # cannot promote. `--check` proved this fixture carries no residue of
    # the shapes the gate KNOWS; on an unclassified lane that proof is
    # vacuous, because nobody has yet said what a credential on it even
    # looks like. The gate owns the table and answers the question, so
    # there is exactly one place the vocabulary lives (a lane in
    # PROVIDER_SHAPE_EXCLUDED answers "classified" -- its shape is absent
    # by a written verdict, not by omission).
    if ! bash "$SCRUB" --lane-known "$lane"; then
      echo "capture_fixtures: lane '$lane' has no credential-shape classification in the" >&2
      echo "scrub gate; not promoting $id -- classify the lane first." >&2
      rm -rf "$tmp"
      return 1
    fi
    dst="$OUT/$lane/$ROUTECTL_FIXTURE_CASE_ID"
    # One case id pins ONE interaction, so two completed requests in one
    # driver trace both key on this path and the second would silently
    # overwrite the first. Refuse instead: the driver is capturing a case
    # it did not isolate, and a corpus entry that depends on which request
    # finished last is not evidence of anything.
    case " $DRIVER_LANDED " in
      *" $dst "*)
        echo "capture_fixtures: case '$ROUTECTL_FIXTURE_CASE_ID' on lane '$lane' already landed this run;" >&2
        echo "refusing $id -- one driver run captures one case." >&2
        rm -rf "$tmp"
        return 1
        ;;
    esac
    mkdir -p "$OUT/$lane"
  else
    dst="$OUT/$id"
  fi

  # Atomically promote the tmp directory into place. Until this rename
  # succeeds the landing directory holds its previous contents (or does
  # not exist); an interrupted run leaves a `.tmp.*` dir that the next
  # startup sweep removes.
  #
  # A driver rerun REPLACES the previous fixture for that case rather than
  # merging into it: the old directory is renamed aside under a `.tmp.`
  # name, the new one moves into place, and only then is the old one
  # deleted. A merge would leave files from the previous run that this
  # capture never observed -- e.g. an `upstream_response.json` from a
  # non-stream run surviving into a stream rerun -- and file presence IS
  # the schema, so the drift signal would be read off a directory no
  # single capture ever produced.
  if [ -d "$dst" ]; then
    local stale="$OUT/.tmp.stale.$id.$$"
    mv "$dst" "$stale"
    mv "$tmp" "$dst"
    rm -rf "$stale"
  else
    mv "$tmp" "$dst"
  fi
  DRIVER_LANDED="$DRIVER_LANDED $dst"

  # Append to manifest.jsonl AFTER the rename so a dangling manifest
  # entry never points to a missing directory. One JSONL line per
  # request, append-only. Same escaping contract as meta.json above --
  # an unescaped quote here corrupts one line of an append-only file
  # that has no rewrite path.
  cat >> "$OUT/manifest.jsonl" <<MANIFEST_LINE
{"request_id":"$j_id","captured_at":"$j_ts","routectl_version":"$j_version","alias":"$j_alias","model":"$j_model","case_id":"$j_case","ingress_kind":"$j_ikind","provider_kind":"$j_pkind","lane":"$j_lane","stream":$stream_flag,"finish_reason":"$j_finish","input_tokens":${input_tokens:-0},"output_tokens":${output_tokens:-0},"total_tokens":${total_tokens:-0}}
MANIFEST_LINE

  echo "$id"
}

# Iterate over completions in chronological order, apply --limit if set.
captured=0
latest_ts=""
while IFS=$'\t' read -r ts id pkind ikind; do
  [ -z "$id" ] && continue
  [ -d "$OUT/$id" ] && continue          # already captured (defensive idempotency)
  write_fixture "$id" "$ts" "$pkind" "$ikind" >/dev/null
  captured=$((captured + 1))
  latest_ts="$ts"
  if [ "$LIMIT" -gt 0 ] && [ "$captured" -ge "$LIMIT" ]; then
    break
  fi
done < <(in_scope_ids)

# Update resume marker only if we captured something. Write to a temp
# file inside $OUT and rename over the marker so an interrupt mid-write
# cannot leave an empty/partial marker that would force a full rescan
# (mirrors the fixture-dir tmp -> dst promotion in write_fixture).
if [ -n "$latest_ts" ]; then
  marker_tmp="$(mktemp "$OUT/.last_capture_ts.XXXXXX")"
  echo "$latest_ts" > "$marker_tmp"
  mv "$marker_tmp" "$MARKER"
fi

echo "captured=$captured since=$since latest=$latest_ts out=$OUT"

# Driver mode only: landing zero is a failed run, not a quiet window.
# Reaching this line already proves NO fixture was refused -- the script
# runs under `set -e` and write_fixture's refusal `return 1` inside the
# loop body aborts before here -- so `captured=0` here means unambiguously
# that the trace held no completed request. Exit 3 keeps that retryable
# verdict distinct from exit 1 ("we refused a fixture"), which never is.
# Only the case id is nameable here: `lane` is local to write_fixture and
# there is no traced provider_kind to normalize when nothing landed.
if [ "$DRIVER_MODE" = 1 ] && [ "$captured" -eq 0 ]; then
  echo "capture_fixtures: case '$ROUTECTL_FIXTURE_CASE_ID' landed no fixture; the trace at $LOG holds no completed request" >&2
  exit 3
fi
