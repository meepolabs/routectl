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
#                               [--limit 4] [--force] [--allow-unsafe-out]
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
    -h|--help) sed -n '1,45p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# Lexically resolve a path to absolute, collapsing `.` and `..` without
# touching the filesystem. Portable (no realpath / -m dependency) and
# works for a not-yet-created directory. Symlinks are NOT followed: the
# default captured tree has none and confinement only needs lexical
# containment.
abspath_lexical() {
  case "$1" in
    /*) _p="$1" ;;
    *)  _p="$PWD/$1" ;;
  esac
  printf '%s\n' "$_p" | awk -F/ '
    { n = 0
      for (i = 1; i <= NF; i++) {
        if ($i == "" || $i == ".") continue
        if ($i == "..") { if (n > 0) n--; continue }
        seg[++n] = $i
      }
      out = ""
      for (i = 1; i <= n; i++) out = out "/" seg[i]
      print (out == "" ? "/" : out)
    }'
}

# Physically resolve a path to absolute, FOLLOWING symlinks, so a
# symlinked component cannot disguise an out-of-tree destination as an
# in-tree one. The path need not exist yet: walk up the RAW path (no
# lexical `..` collapse first -- collapsing before symlink resolution is
# unsafe, since `link/..` must resolve through the link, not cancel its
# name) to the nearest EXISTING ancestor, resolve THAT with
# `cd -P` / `pwd -P` (portable; no `realpath -m` dependency), then
# re-append the non-existing tail. Tail components do not exist, so they
# cannot be symlinks; a final lexical collapse of the combined path is
# therefore physically faithful.
abspath_physical() {
  case "$1" in
    /*) _p="$1" ;;
    *)  _p="$PWD/$1" ;;
  esac
  _tail=""
  while [ ! -e "$_p" ] && [ "$_p" != "/" ]; do
    _tail="$(basename "$_p")${_tail:+/$_tail}"
    _p="$(dirname "$_p")"
  done
  _phys="$(cd -P "$_p" 2>/dev/null && pwd -P)" || {
    echo "cannot physically resolve path ancestor: $_p" >&2
    exit 2
  }
  if [ -n "$_tail" ]; then
    abspath_lexical "$_phys/$_tail"
  else
    printf '%s\n' "$_phys"
  fi
}

# Confine --out to the default captured dir unless the operator
# explicitly opts out. Fixtures carry RAW headers (auth included when
# the daemon runs with ROUTECTL_TRACE_HEADERS) and the default tree is
# gitignored; writing them into an arbitrary -- possibly git-tracked --
# path risks committing secrets. OUT keeps its lexically-collapsed form
# for the write path; the confinement test compares the PHYSICALLY
# resolved (symlink-following) OUT against the physically resolved
# default root. A purely lexical compare cannot see through a symlinked
# subdir under the default tree, so `<default>/<symlink>/x` could escape
# confinement -- resolving both sides physically closes that hole while
# still normalizing `..` traversals such as `<default>/../../src`.
OUT="$(abspath_lexical "$OUT")"
DEFAULT_OUT_ABS="$(abspath_lexical "$ROOT/crates/routectl-cli/tests/fixtures/captured")"
if [ "$ALLOW_UNSAFE_OUT" = 0 ]; then
  # Belt-and-suspenders: walk every OUT component UNDER the captured
  # root and reject any symlink, even a DANGLING one (target does not
  # exist). The physical resolution further down walks up to the
  # nearest EXISTING ancestor with `cd -P`, so a broken symlink under
  # the captured tree (e.g. `<captured>/<dangling-link>/<leaf>` where
  # leaf does not yet exist) slips past it; `mkdir -p` below also
  # cannot reify a dangling symlink as a directory. `[ -L ]` is the
  # POSIX symlink test -- true for any symlink regardless of whether
  # its target resolves -- and it is run BEFORE physical resolution
  # because resolution loses the per-component symlink information.
  # Out-of-tree paths skip this loop and are rejected by the physical
  # confinement test below.
  case "$OUT" in
    "$DEFAULT_OUT_ABS" | "$DEFAULT_OUT_ABS"/*)
      _check="$DEFAULT_OUT_ABS"
      _remaining="${OUT:${#DEFAULT_OUT_ABS}}"
      _remaining="${_remaining#/}"
      while [ -n "$_remaining" ]; do
        case "$_remaining" in
          */*) _seg="${_remaining%%/*}"; _remaining="${_remaining#*/}" ;;
          *)   _seg="$_remaining"; _remaining="" ;;
        esac
        [ -z "$_seg" ] && continue
        _check="$_check/$_seg"
        if [ -L "$_check" ]; then
          echo "refusing --out '$OUT': symlink component at '$_check'." >&2
          echo "fixtures contain raw headers (auth when ROUTECTL_TRACE_HEADERS is on);" >&2
          echo "a symlink under the captured tree could redirect writes outside it." >&2
          echo "pass --allow-unsafe-out to override." >&2
          exit 2
        fi
      done
      ;;
  esac

  out_phys="$(abspath_physical "$OUT")"
  default_phys="$(abspath_physical "$DEFAULT_OUT_ABS")"
  case "$out_phys" in
    "$default_phys" | "$default_phys"/*) : ;;
    *)
      echo "refusing --out '$OUT': outside the default captured dir '$DEFAULT_OUT_ABS'." >&2
      echo "fixtures contain raw headers (auth when ROUTECTL_TRACE_HEADERS is on)." >&2
      echo "pass --allow-unsafe-out to write outside the default tree on purpose." >&2
      exit 2
      ;;
  esac
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
# `$OUT/.tmp.<id>.XXXXXX` and are promoted to `$OUT/<id>` via a
# single mv(1) only after every file lands. A crash between the
# first file write and the mv leaves a `.tmp.*` directory behind,
# which the startup sweep prunes on the next run -- the final
# `$OUT/<id>` directory is never half-populated, so the
# `[ -d "$OUT/$id" ] && continue` idempotency guard in the caller
# can be trusted. The manifest append happens AFTER the mv so a
# dangling manifest entry never points to a missing directory.
write_fixture() {
  local id="$1" ts="$2" pkind="$3" ikind="${4:-}"
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

  # Structural summary lines (two: ingress + outgoing direction).
  echo "$lines" | grep 'structural summary' | head -2 > "$tmp/structural.txt" || true
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
  local j_id j_ts j_version j_alias j_model j_case j_sha
  local j_cname j_cversion j_cmode j_ikind j_pkind j_lane j_finish
  j_id="$(json_escape "$id")"
  j_ts="$(json_escape "$ts")"
  j_version="$(json_escape "$ROUTECTL_VERSION")"
  j_alias="$(json_escape "${alias:-}")"
  j_model="$(json_escape "${model:-}")"
  j_case="$(json_escape "${ROUTECTL_FIXTURE_CASE_ID:-}")"
  j_sha="$(json_escape "${ROUTECTL_FIXTURE_CONFIG_SHA:-}")"
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

  # Atomically promote the tmp directory into place. Until this
  # rename succeeds the final `$OUT/<id>` directory does not exist;
  # an interrupted run leaves a `.tmp.*` dir that the next startup
  # sweep removes.
  mv "$tmp" "$dst"

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

# === Symlink-component check sanity test (manual) ===
#
# Verifies the per-component [-L] walk catches a dangling symlink in the
# OUT path. The walk runs lexically before any filesystem touch, so a
# synthetic captured/ tree exercises it without invoking the script.
# Expected: the loop emits `PASS: detected <symlink path>`.
#
#   tmp="$(mktemp -d)"
#   trap 'rm -rf "$tmp"' EXIT
#   captured="$tmp/captured"
#   mkdir -p "$captured"
#   # Dangling symlink: target deliberately does not exist.
#   ln -s "$tmp/no-such-target" "$captured/dangling"
#   # Mirror the script's lexical walk in-process:
#
#   DEFAULT_OUT_ABS="$captured"
#   OUT="$captured/dangling/leaf"
#   _check="$DEFAULT_OUT_ABS"
#   _remaining="${OUT:${#DEFAULT_OUT_ABS}}"
#   _remaining="${_remaining#/}"
#   hit=""
#   while [ -n "$_remaining" ]; do
#     case "$_remaining" in
#       */*) _seg="${_remaining%%/*}"; _remaining="${_remaining#*/}" ;;
#       *)   _seg="$_remaining"; _remaining="" ;;
#     esac
#     [ -z "$_seg" ] && continue
#     _check="$_check/$_seg"
#     if [ -L "$_check" ]; then hit="$_check"; break; fi
#   done
#   if [ -n "$hit" ]; then echo "PASS: detected $hit"; else echo "FAIL"; fi
