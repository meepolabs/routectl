#!/usr/bin/env bash
# Boot a HERMETIC routectl, let a driver client talk to it, and hand the
# resulting trace to scripts/capture_fixtures.sh in driver mode.
#
# The trace sink is `serve 2> "$RUN/trace.log"`. That redirect IS the
# capture sink: `init_tracing` writes to stderr, the driven daemon is not
# a service unit so its stderr never reaches a journal, and the rig's
# `--log` already consumes a file path. There is deliberately no new env
# var and no new subcommand for this.
#
# Usage:
#   scripts/capture_driver.sh --lane <lane> --case <case-id> \
#                             --expected-ingress <dialect> \
#                             [--connection-mode <mode>] \
#                             [--out <dir>] [--out-root <dir>] \
#                             [--work <dir>] [--timeout <seconds>] \
#                             [--keep] \
#                             -- <driver-command> [args...]
#
#   --lane             which committed lane config to boot under; reads
#                      scripts/drivers/config/<lane>.toml
#   --case             scenario id, e.g. `tools-multiturn-01`. Names the
#                      landing directory, so it must be a path-safe
#                      scenario name and must never be derived from the
#                      environment (a hostname or a real path in it is
#                      personal data the scrub gate refuses).
#   --expected-ingress which ingress dialect the driven client is expected
#                      to reach routectl on -- `anthropic`, `openai`, or
#                      `openai-responses` (the vocabulary in
#                      scripts/drivers/lib/ingress_kinds.sh). REQUIRED,
#                      with no default, and enforced against the TRACED
#                      dialect at both promotion boundaries. See "WHY THIS
#                      ONE ARRIVES ON ARGV" below.
#   --connection-mode  how the driver reaches routectl. Default
#                      `base-url`; pass `front-proxy` for a MITM run.
#                      The mode and the lane config must agree, both
#                      directions, before any daemon boots: front-proxy
#                      requires the lane config to carry a `[mitm]`
#                      block, and base-url refuses a config that carries
#                      one -- a `[mitm]` block on a base-url boot binds a
#                      listener nobody drives, on a port nobody probed.
#   --out              where the fixture lands. Default:
#                      `.routectl-driver-scratch/` at the repo root,
#                      which is gitignored. Deliberately NOT the
#                      committed driver corpus -- an exploratory rerun of
#                      a case would replace a reviewed fixture in place.
#                      Promotion into the corpus is
#                      scripts/promote_fixture.sh.
#   --out-root         the confinement root --out must live under, chosen
#                      from a CLOSED SET (the gitignored scratch root, or
#                      ROUTECTL_DRIVER_OUT_ROOT when a run sets it). It is
#                      a parameter rather than a constant because a run
#                      that mounts this repo read-only cannot land
#                      fixtures inside it, so no repo-relative constant
#                      can name the destination. It is confined rather
#                      than trusted: a root accepted from argv unchecked
#                      would leave --out compared against a path the same
#                      caller chose, which confines nothing.

#   --work             parent for the run workspace. Default: mktemp.
#   --timeout          seconds to wait for /health. Default 20.
#   --keep             keep the run workspace (trace log included) for
#                      debugging instead of removing it on exit.
#
# THE DRIVER CONTRACT. The command after `--` runs with cwd set to the
# run's throwaway git repo and with these variables exported:
#
#   ROUTECTL_BASE_URL                 http://127.0.0.1:<port>
#   ROUTECTL_DRIVER_PORT              <port>
#   ROUTECTL_DRIVER_RUN               the run workspace root
#   ROUTECTL_DRIVER_WORK              the throwaway git repo (also cwd)
#   HOME                              throwaway home, empty at boot
#   XDG_CONFIG_HOME                   throwaway config root
#   ROUTECTL_FIXTURE_CASE_ID          the five driver-mode pins, so a
#   ROUTECTL_FIXTURE_CONFIG_SHA       driver can echo them into its own
#   ROUTECTL_FIXTURE_CONNECTION_MODE  logs and read them back
#   ROUTECTL_FIXTURE_WIRE_PATTERN
#   ROUTECTL_FIXTURE_EXPECTED_INGRESS
#
# In front-proxy mode only, two more:
#
#   ROUTECTL_DRIVER_PROXY_URL         http://127.0.0.1:<mitm-port>
#   ROUTECTL_DRIVER_PROXY_CA          the CA pem the client must trust
#
# The MITM port is a SECOND probed port, distinct from the HTTP one
# (`validate_mitm_config` refuses a collision at startup), passed as
# `serve --mitm-port` so the committed config bytes -- and their sha --
# stay untouched. The CA path points into the run's throwaway XDG root:
# the daemon mints that CA itself at listener start, so no pre-mint step
# exists and no host CA is ever consumed.
#
# The wire pattern is DERIVED from the case file, never taken on argv: a
# flag would let a caller declare a pattern the case does not claim, and
# the recorded claim is the only on-disk evidence of which wire shape a
# fixture was captured for.
#
# WHY THIS ONE ARRIVES ON ARGV. `--expected-ingress` is the exception to
# the derive-rather-than-accept rule above, because nothing this script
# reads names the value:
#
#   * the lane config declares the EGRESS provider (`kind`,
#     `api_key_ref`, `auth_kind`) and says nothing about which dialect a
#     client speaks INBOUND -- a translation lane exists precisely so the
#     two differ;
#   * a case describes an INTERACTION and is deliberately
#     dialect-agnostic: the same turns and knobs are driven through
#     different clients on different dialects, which is what makes two
#     lanes' fixtures comparable at all;
#   * the DRIVER does know its client's dialect, but it runs downstream of
#     the pin export -- the rig reads the pins from this script's
#     environment, and there is no channel back up from a driver -- so a
#     driver-owned pin would arrive after the only reader needed it.
#
# So the pin is a property of the (driver, lane) PAIRING the caller
# chooses, and the caller is the only layer that holds it. It is required
# rather than defaulted: a default of `anthropic` would be silently wrong
# for exactly the non-Anthropic client this check exists to catch, which
# is the empty claim wearing a plausible value.
#
# A driver maps ROUTECTL_BASE_URL onto whatever variable its client
# reads; this script stays client-agnostic on purpose.
#
# HERMETICITY IS THE POINT, not tidiness. A driven client runs tools and
# reads their output back into its own request bodies, so anything
# personal reachable from its cwd or its HOME lands in a fixture. Fresh
# HOME, fresh cwd, and a synthetic git identity mean there is nothing
# personal to read in the first place; the scrub gate is the proof half
# of the same story, not a substitute for it.
#
# The daemon this script starts is killed by the pid captured from its
# own `$!` and nothing else -- never by name. It is never launched under
# `setsid`, because the captured pid would then be the wrapper and the
# real daemon would outlive the run.
#
# Exit codes:
#   0  the driver ran and the rig landed the fixture
#   2  usage error (unknown flag, missing lane config, no driver command,
#      a mode/config mismatch, an expected-ingress dialect outside the
#      vocabulary, or a `--out` this script refuses to write to)
#   3  the hermetic daemon never became healthy
#   4  the driver command exited non-zero
#   5  the capture rig refused the fixture (see its own message)
#   6  no free port in the search window
#   7  the rig ran clean but landed NO fixture -- the case produced no
#      completed request. Distinct from 5 on purpose: this one is
#      retryable, a refusal is a defect in what the case produced
#   8  front-proxy only: the daemon is healthy but its MITM front-proxy
#      listener never became ready -- no structured listening line for
#      the selected MITM port in the trace within the timeout, a proxy
#      startup-error line in the trace, or a CA unreadable at the path
#      this script exports. Distinct from 3 on purpose: MITM startup
#      failure is non-fatal to the daemon, so /health stays green while
#      the proxied CONNECT has nothing to hit, and an operator sent to
#      the health layer would debug the wrong thing
#
# `ROUTECTL_BIN` overrides the daemon binary (default `routectl`).
# `ROUTECTL_DRIVER_PORT_MIN` / `ROUTECTL_DRIVER_PORT_MAX` narrow the port
# search window. Both exist so the self-test can drive the boot path
# without a real daemon and without a real credential.
# --- END USAGE ---

# set -e so a failed boot step aborts instead of driving a daemon that
# is not there. set -u catches a typo'd variable before it silently
# empties a path. pipefail is off for the same reason the rig has it off:
# `... | head -1` pipelines terminate their producer on purpose.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RIG="$ROOT/scripts/capture_fixtures.sh"
CONFIG_DIR="$ROOT/scripts/drivers/config"
CASES_DIR="$ROOT/scripts/drivers/cases"
VALIDATE_CASE="$ROOT/scripts/drivers/lib/validate_case.py"

# The single owner of path confinement, shared with the rig and the
# promote script. An absent library is a hard failure, never an
# unconfined run: `--out` is caller-supplied and fixtures carry RAW
# headers (auth included, since the daemon here runs with
# ROUTECTL_TRACE_HEADERS on), so a run whose destination went unchecked
# would be a write primitive aimed at whatever the caller named.
CONFINE_LIB="$ROOT/scripts/drivers/lib/confine.sh"
if [ ! -r "$CONFINE_LIB" ]; then
  echo "capture_driver: confinement library not found at $CONFINE_LIB; refusing to run" >&2
  exit 2
fi
# shellcheck source=scripts/drivers/lib/confine.sh
. "$CONFINE_LIB"

# The single owner of the ingress-dialect vocabulary, shared with the rig
# and the promote script. Absent library is a hard failure, never an
# unvalidated `--expected-ingress`: the pin is enforced against a traced
# token downstream, so an unchecked typo would fail every capture with a
# dialect-mismatch message instead of naming the flag that was wrong.
INGRESS_KINDS_LIB="$ROOT/scripts/drivers/lib/ingress_kinds.sh"
if [ ! -r "$INGRESS_KINDS_LIB" ]; then
  echo "capture_driver: ingress vocabulary not found at $INGRESS_KINDS_LIB; refusing to run" >&2
  exit 2
fi
# shellcheck source=scripts/drivers/lib/ingress_kinds.sh
. "$INGRESS_KINDS_LIB"

# The default landing root: a gitignored scratch tree, NOT the committed
# driver corpus. A rerun of a case overwrites that case's fixture, so a
# corpus default means every exploratory run replaces a reviewed fixture
# in place; promotion into the corpus is a separate, scrub-gated step
# (scripts/promote_fixture.sh).
DEFAULT_SCRATCH="$ROOT/.routectl-driver-scratch"

# Roots a caller may widen `--out` to, as a closed set. A run that mounts
# the repo read-only cannot land fixtures inside it, so the set cannot be
# "under the repo" -- but it must not be "anywhere either", or the
# containment check below degenerates into comparing two caller-chosen
# paths. `ROUTECTL_DRIVER_OUT_ROOT` names the one env seam a mounted run
# needs; it is read here rather than accepted as a second free path so the
# allowed set stays enumerable by reading this file.
ALLOWED_OUT_ROOTS="$DEFAULT_SCRATCH${ROUTECTL_DRIVER_OUT_ROOT:+
$ROUTECTL_DRIVER_OUT_ROOT}"

# Refuse an `--out-root` outside the closed set. Exact match only: a
# PREFIX test would accept `<allowed>-evil` beside `<allowed>`, and a
# suffix-stripped compare would accept a parent. The candidate is already
# lexically absolute here, and `confine_out_under` below still walks it
# for symlink components, so this check owns membership and nothing else.
confine_out_root() {
  local _cand="$1" _allowed
  while IFS= read -r _allowed; do
    [ -n "$_allowed" ] || continue
    [ "$_cand" = "$(abspath_lexical "$_allowed")" ] && return 0
  done <<EOF
$ALLOWED_OUT_ROOTS
EOF
  echo "capture_driver: refusing --out-root '$_cand': not an allowed landing root." >&2
  echo "allowed: the gitignored scratch root, or the value of" >&2
  echo "ROUTECTL_DRIVER_OUT_ROOT when the run sets it." >&2
  echo "a root taken on trust would make the --out check compare two" >&2
  echo "caller-chosen paths, which confines nothing." >&2
  exit 2
}


# Daemon binary, overridable so the self-test can inject a stub. A real
# boot needs credentials and CI has none.
ROUTECTL_BIN="${ROUTECTL_BIN:-routectl}"

# Ephemeral-range window the run picks its port from. High and fixed so a
# driven daemon never lands on a port a conventionally-configured local
# service would be listening on; the `ss` probe below is what actually
# decides, this only bounds the search.
PORT_MIN="${ROUTECTL_DRIVER_PORT_MIN:-19000}"
PORT_MAX="${ROUTECTL_DRIVER_PORT_MAX:-19999}"
PORT_TRIES=64

LANE=""
CASE_ID=""
CONNECTION_MODE="base-url"
EXPECTED_INGRESS=""
DRIVER_OUT=""
OUT_ROOT=""
WORK_PARENT=""
HEALTH_TIMEOUT=20
KEEP=0

# Print the header block as usage, sentinel-delimited: a magic line range
# starts silently cutting content the moment the header grows, and the
# driver contract is exactly the part a caller needs to read.
usage() {
  sed -n '2,/^# --- END USAGE ---$/p' "$0" | sed '$d'
}

die() {
  echo "capture_driver: $1" >&2
  exit "$2"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --lane) [ $# -ge 2 ] || die "--lane requires a value" 2; LANE="$2"; shift 2 ;;
    --case) [ $# -ge 2 ] || die "--case requires a value" 2; CASE_ID="$2"; shift 2 ;;
    --connection-mode)
      [ $# -ge 2 ] || die "--connection-mode requires a value" 2
      CONNECTION_MODE="$2"; shift 2 ;;
    --expected-ingress)
      [ $# -ge 2 ] || die "--expected-ingress requires a value" 2
      EXPECTED_INGRESS="$2"; shift 2 ;;
    --out) [ $# -ge 2 ] || die "--out requires a value" 2; DRIVER_OUT="$2"; shift 2 ;;
    --out-root)
      [ $# -ge 2 ] || die "--out-root requires a value" 2
      OUT_ROOT="$2"; shift 2 ;;
    --work) [ $# -ge 2 ] || die "--work requires a value" 2; WORK_PARENT="$2"; shift 2 ;;
    --timeout) [ $# -ge 2 ] || die "--timeout requires a value" 2; HEALTH_TIMEOUT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) usage; exit 0 ;;
    --) shift; break ;;
    *) die "unknown arg: $1" 2 ;;
  esac
done

[ -n "$LANE" ] || die "--lane is required" 2
[ -n "$CASE_ID" ] || die "--case is required" 2

# Required with NO default, and validated against the closed vocabulary
# before any daemon boots. A default would be silently wrong for exactly
# the non-Anthropic client the downstream check exists to catch, and an
# out-of-vocabulary value can never equal a traced token -- so an
# unvalidated one would turn every run into a dialect-mismatch refusal
# that names the wrong fault. See "WHY THIS ONE ARRIVES ON ARGV" above.
[ -n "$EXPECTED_INGRESS" ] ||
  die "--expected-ingress is required (one of: $(ingress_kinds_list)); it is the pin the promotion gates check the traced dialect against" 2
ingress_kind_is_known "$EXPECTED_INGRESS" ||
  die "unsupported --expected-ingress '$EXPECTED_INGRESS' (one of: $(ingress_kinds_list))" 2

[ $# -ge 1 ] || die "no driver command given (pass it after \`--\`)" 2

# An inverted window makes `RANDOM % (MAX - MIN + 1)` a modulo by a
# non-positive number, which yields a candidate OUTSIDE the window -- so
# the caller gets a port it did not ask for instead of an error.
[ "$PORT_MAX" -ge "$PORT_MIN" ] ||
  die "port window $PORT_MIN-$PORT_MAX is inverted" 2

LANE_CONFIG="$CONFIG_DIR/$LANE.toml"
[ -r "$LANE_CONFIG" ] || die "no committed config for lane '$LANE' at $LANE_CONFIG" 2

# MODE/CONFIG COHERENCE, both directions, before any daemon boots. Each
# committed config means exactly one mode: front-proxy without `[mitm]`
# has no listener for the client's CONNECT, and base-url WITH `[mitm]`
# binds a listener nobody drives on the config's placeholder port --
# which this script never probed, so it can collide with anything. The
# match is anchored to the table header alone; the twin configs are
# hand-reviewed TOML where `[mitm]` starts its own line.
lane_has_mitm=0
grep -q '^\[mitm\]$' "$LANE_CONFIG" && lane_has_mitm=1
case "$CONNECTION_MODE" in
  front-proxy)
    [ "$lane_has_mitm" = 1 ] ||
      die "connection mode front-proxy requires a [mitm] block in $LANE_CONFIG, which has none" 2
    ;;
  base-url)
    [ "$lane_has_mitm" = 0 ] ||
      die "connection mode base-url refuses $LANE_CONFIG: it carries a [mitm] block, so it is a front-proxy lane" 2
    ;;
  *)
    die "unsupported --connection-mode '$CONNECTION_MODE' (base-url or front-proxy)" 2
    ;;
esac

# The sha is of the COMMITTED lane config, not of the copy the run boots
# from. The two differ only by the port the run selected, and hashing the
# patched copy would give every run a unique sha -- which would make
# `meta.config_sha` unable to tell a config change from a fresh run, the
# one question it exists to answer. (This build passes the port on the
# serve command line instead of patching, so the bytes happen to match
# today; hashing the committed file keeps that true if that ever changes.)
CONFIG_SHA="$(sha256sum "$LANE_CONFIG" | cut -d' ' -f1)"

# The wire pattern the fixture will CLAIM, read out of the case file the
# drivers read. Same shape as the lane-config check above: fail closed,
# before any daemon boots, because a run that cannot name its pattern
# would land a fixture whose claim is empty -- and an empty claim is
# worse than none, since nothing downstream can tell it from a pattern
# nobody recorded. `validate_case.py` validates the field against its
# closed set on the way out, so a value that arrives here is a member of
# that set; the exit status is what proves it arrived.
CASE_FILE="$CASES_DIR/$CASE_ID.json"
[ -r "$CASE_FILE" ] || die "no case file for '$CASE_ID' at $CASE_FILE" 2
WIRE_PATTERN="$(python3 "$VALIDATE_CASE" --field wire_pattern "$CASE_FILE")" ||
  die "case '$CASE_ID' declares no valid wire_pattern (see $CASE_FILE)" 2
[ -n "$WIRE_PATTERN" ] ||
  die "case '$CASE_ID' declares an empty wire_pattern (see $CASE_FILE)" 2

# ---------------------------------------------------------------------
# Landing root
# ---------------------------------------------------------------------

# `--out` is CALLER-SUPPLIED, so this script owns a confinement check of
# its own -- it does not inherit one from the rig. It hands the rig
# `--allow-unsafe-out` below (the driver landing root is not the rig's
# `captured/` tree, so the rig's own check would refuse every driver run),
# which means the rig performs NO containment on this path. Without the
# check here the runner would be an unconfined write primitive that lands
# raw fixture headers -- auth included, since the daemon boots with
# ROUTECTL_TRACE_HEADERS -- wherever the caller pointed it.
#
# `--out-root` WIDENS the allowed root, which is why it is itself confined
# to ALLOWED_OUT_ROOTS rather than taken on trust. A root accepted from
# argv unchecked would reduce the containment below to "the path is under
# a path the same caller also chose", which is no containment at all --
# `--out /anywhere/x --out-root /anywhere` would pass. The promote script
# looks similar but is not: the caller-supplied root there governs only
# what it READS, while its write destination is confined to a constant.
#
# Both paths resolve LEXICALLY first and are quoted at every use:
# `$(...)` strips trailing newlines, so an unquoted substitution used as a
# path can word-split into arguments a later command would act on.
[ -n "$DRIVER_OUT" ] || DRIVER_OUT="$DEFAULT_SCRATCH"
[ -n "$OUT_ROOT" ] || OUT_ROOT="$DEFAULT_SCRATCH"
DRIVER_OUT="$(abspath_lexical "$DRIVER_OUT")"
OUT_ROOT="$(abspath_lexical "$OUT_ROOT")"
confine_out_root "$OUT_ROOT"
confine_out_under "$DRIVER_OUT" "$OUT_ROOT"


# ---------------------------------------------------------------------
# Run workspace
# ---------------------------------------------------------------------

if [ -n "$WORK_PARENT" ]; then
  mkdir -p "$WORK_PARENT"
  RUN="$(mktemp -d "$WORK_PARENT/routectl-driver.XXXXXX")"
else
  RUN="$(mktemp -d)"
fi

# `client-home`, not `home`: the driven client reads $HOME back into its
# request bodies (its memory-dir path lands in the system prompt), and a
# segment literally named `home` makes that echoed path match the scrub
# gate's `/home/<name>` deny class mid-string. The gate is deliberately
# over-approximate; the runner's job is to keep the string out of the
# fixture, not to carve an exception through the gate.
RUN_HOME="$RUN/client-home"
RUN_XDG="$RUN/xdg"
RUN_WORK="$RUN/work"
TRACE="$RUN/trace.log"

DAEMON_PID=""

# Kill ONLY the pid this run captured, then take the workspace down.
# Registered before the daemon starts so every exit path -- a failed
# health poll, a driver crash, a SIGINT from the terminal -- runs it; an
# aborted run that leaked a daemon would hold its port and its hermetic
# state directory for the rest of the session.
cleanup() {
  local rc=$?
  trap - EXIT
  if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    local i=0
    while [ "$i" -lt 40 ] && kill -0 "$DAEMON_PID" 2>/dev/null; do
      sleep 0.1
      i=$((i + 1))
    done
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
      kill -9 "$DAEMON_PID" 2>/dev/null || true
      wait "$DAEMON_PID" 2>/dev/null || true
    fi
  fi
  if [ "$KEEP" = 1 ]; then
    echo "capture_driver: run workspace kept at $RUN" >&2
  else
    rm -rf "$RUN"
  fi
  exit "$rc"
}
trap cleanup EXIT INT TERM

mkdir -p "$RUN_HOME" "$RUN_WORK" "$RUN_XDG/routectl"

# The config path the daemon resolves is
# `$XDG_CONFIG_HOME/routectl/config.toml` -- the `routectl/` SUBDIR is
# part of it, and an XDG root without it boots against the caller's real
# config instead of the lane's.
cp "$LANE_CONFIG" "$RUN_XDG/routectl/config.toml"

# A synthetic git identity, in a throwaway repo, under a throwaway HOME:
# a client that shells out to `git` sees this and nothing else. The values
# are obviously fake by design -- `.invalid` is reserved and can never
# resolve.
#
# The init itself runs under the throwaway HOME too. Otherwise `git init`
# reads the CALLER's global config, and an `include` directive or a
# templatedir in it would seed the throwaway repo with the caller's own
# settings -- the one thing this repo exists not to have.
(
  cd "$RUN_WORK"
  export HOME="$RUN_HOME"
  export XDG_CONFIG_HOME="$RUN_XDG"
  git init -q -b main .
  git config user.name "Fixture Driver"
  git config user.email "driver@fixtures.invalid"
  git config commit.gpgsign false
)

# ---------------------------------------------------------------------
# Port selection
# ---------------------------------------------------------------------

# True when something is already listening on the candidate port. `ss`
# reports the local address as `<addr>:<port>` in its fourth column, so
# the match is anchored on the trailing `:<port>` to keep 19001 from
# matching 190010.
port_in_use() {
  ss -ltn 2>/dev/null | awk -v p=":$1" 'NR > 1 && index($4, p) == length($4) - length(p) + 1 { found = 1 } END { exit !found }'
}

# Pick a port AFTER probing, never by assumption. A fixed "probably free"
# port collides with a previous run's daemon or with another checkout's,
# and a bind failure on an occupied port is the worst outcome: the other
# listener keeps answering /health, so the run proceeds against someone
# else's daemon.
pick_port() {
  local i=0 candidate
  while [ "$i" -lt "$PORT_TRIES" ]; do
    candidate=$((PORT_MIN + RANDOM % (PORT_MAX - PORT_MIN + 1)))
    if ! port_in_use "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
    i=$((i + 1))
  done
  return 1
}

PORT="$(pick_port)" || die "no free port found in $PORT_MIN-$PORT_MAX after $PORT_TRIES tries" 6
BASE_URL="http://127.0.0.1:$PORT"

# front-proxy needs a SECOND port for the MITM listener, drawn through
# the same probe and additionally rejecting equality with the first:
# `validate_mitm_config` refuses a listen_port that collides with the
# HTTP port, so handing it a collision would turn a random draw into a
# startup failure. Exhausting the window stays the same exit code as the
# first draw -- it is the same scarcity.
MITM_PORT=""
PROXY_URL=""
PROXY_CA=""
if [ "$CONNECTION_MODE" = "front-proxy" ]; then
  pick_distinct_port() {
    local i=0 candidate
    while [ "$i" -lt "$PORT_TRIES" ]; do
      candidate="$(pick_port)" || return 1
      if [ "$candidate" != "$PORT" ]; then
        printf '%s\n' "$candidate"
        return 0
      fi
      i=$((i + 1))
    done
    return 1
  }
  MITM_PORT="$(pick_distinct_port)" ||
    die "no free MITM port distinct from $PORT found in $PORT_MIN-$PORT_MAX after $PORT_TRIES tries" 6
  [ "$MITM_PORT" != "$PORT" ] ||
    die "internal error: MITM port draw returned the HTTP port $PORT" 6
  PROXY_URL="http://127.0.0.1:$MITM_PORT"
  # The daemon mints this CA itself at listener start, under the run's
  # fresh XDG root -- there is no pre-mint step and no host CA is ever
  # consumed. The `current` segment is the generation symlink the cert
  # store swaps atomically on re-mint.
  PROXY_CA="$RUN_XDG/routectl/mitm-certs/current/mitm-ca-cert.pem"
fi

# ---------------------------------------------------------------------
# Boot
# ---------------------------------------------------------------------

# The trace knobs the rig needs: body traces at TRACE on the log_safe
# target, header tracing on, and a body cap large enough that a real
# multi-turn body is not truncated mid-escape (the replay loader refuses
# a cap-truncated body).
#
# Both ports arrive on the command line rather than as a rewrite of the
# copied config: the committed bytes stay the identity the sha names, and
# there is one less file mutation between the hash and the boot.
serve_args=(serve --host 127.0.0.1 --port "$PORT")
if [ -n "$MITM_PORT" ]; then
  serve_args+=(--mitm-port "$MITM_PORT")
fi
(
  cd "$RUN_WORK"
  HOME="$RUN_HOME" \
  XDG_CONFIG_HOME="$RUN_XDG" \
  ROUTECTL_LOG="routectl=info,routectl_core::log_safe=trace" \
  ROUTECTL_TRACE_HEADERS=1 \
  ROUTECTL_TRACE_BODY_BYTES=2097152 \
    exec "$ROUTECTL_BIN" "${serve_args[@]}"
) 2> "$TRACE" &
DAEMON_PID=$!

# /health is a PRECONDITION, and "something answers" is not the
# condition: an occupied port leaves a stale listener answering while our
# own process is already dead. Both halves are required on every poll --
# the pid this run captured is alive AND the endpoint answers.
health_ok() {
  kill -0 "$DAEMON_PID" 2>/dev/null || return 1
  curl -fsS -m 2 "$BASE_URL/health" >/dev/null 2>&1
}

deadline_polls=$((HEALTH_TIMEOUT * 4))
healthy=0
i=0
while [ "$i" -lt "$deadline_polls" ]; do
  if health_ok; then
    healthy=1
    break
  fi
  sleep 0.25
  i=$((i + 1))
done

if [ "$healthy" = 0 ]; then
  echo "capture_driver: hermetic daemon on port $PORT never became healthy within ${HEALTH_TIMEOUT}s" >&2
  echo "--- tail of $TRACE ---" >&2
  tail -n 20 "$TRACE" >&2 || true
  exit 3
fi

# ---------------------------------------------------------------------
# MITM readiness (front-proxy only)
# ---------------------------------------------------------------------

# /health proves the DAEMON, not the proxy: MITM startup failure is
# non-fatal in serve.rs (the daemon logs and keeps serving), so a healthy
# daemon can carry a dead front-proxy listener -- and the driven client's
# proxied CONNECT would hit nothing, surfacing as a client error nobody
# can attribute. The gate is the structured listening line for the
# SELECTED MITM port in the trace plus a readable CA at the path this run
# exports; a startup-error line in the trace is already a terminal
# verdict, so it fails immediately instead of waiting out the deadline.
#
# Both matches anchor on the emitter and the event, never on a phrase a
# request body could contain, and the trace lines carry ANSI escapes --
# stripped here before any anchored match, because the escapes sit inside
# the `addr=` field the port anchor reads.
if [ "$CONNECTION_MODE" = "front-proxy" ]; then
  MITM_EMITTER="routectl_cli::server::serve"
  mitm_trace() {
    sed 's/\x1b\[[0-9;]*m//g' "$TRACE" 2>/dev/null || true
  }
  mitm_listening() {
    mitm_trace | grep -Eq \
      "${MITM_EMITTER}.*MITM front-proxy listening.*addr=127\.0\.0\.1:${MITM_PORT}([^0-9]|$)"
  }
  mitm_startup_error() {
    mitm_trace | grep -Eq "${MITM_EMITTER}.*MITM proxy failed to start"
  }
  mitm_fail() {
    echo "capture_driver: $1" >&2
    echo "--- tail of $TRACE ---" >&2
    tail -n 20 "$TRACE" >&2 || true
    exit 8
  }

  mitm_ready=0
  i=0
  while [ "$i" -lt "$deadline_polls" ]; do
    if mitm_startup_error; then
      mitm_fail "MITM front-proxy listener failed to start (startup error in the trace); the daemon is healthy on port $PORT, the proxy layer on port $MITM_PORT is down"
    fi
    if mitm_listening; then
      mitm_ready=1
      break
    fi
    sleep 0.25
    i=$((i + 1))
  done
  if [ "$mitm_ready" = 0 ]; then
    mitm_fail "MITM front-proxy listener on port $MITM_PORT never became ready within ${HEALTH_TIMEOUT}s; the daemon is healthy, the proxy layer is not"
  fi
  [ -r "$PROXY_CA" ] ||
    mitm_fail "MITM front-proxy CA is not readable at $PROXY_CA; the listener is up but a client cannot trust it"
fi

# ---------------------------------------------------------------------
# Drive
# ---------------------------------------------------------------------

driver_rc=0
(
  cd "$RUN_WORK"
  # The proxy carriers exist only in front-proxy mode: a base-url driver
  # that saw them could route through a listener this run never started,
  # and the drivers' own mode check treats their presence as intent. The
  # unset side matters as much as the export side -- the runner forwards
  # the caller's environment, so an inherited carrier would survive a
  # base-url run without it.
  if [ "$CONNECTION_MODE" = "front-proxy" ]; then
    export ROUTECTL_DRIVER_PROXY_URL="$PROXY_URL"
    export ROUTECTL_DRIVER_PROXY_CA="$PROXY_CA"
  else
    unset ROUTECTL_DRIVER_PROXY_URL ROUTECTL_DRIVER_PROXY_CA
  fi
  HOME="$RUN_HOME" \
  XDG_CONFIG_HOME="$RUN_XDG" \
  ROUTECTL_BASE_URL="$BASE_URL" \
  ROUTECTL_DRIVER_PORT="$PORT" \
  ROUTECTL_DRIVER_RUN="$RUN" \
  ROUTECTL_DRIVER_WORK="$RUN_WORK" \
  ROUTECTL_FIXTURE_CASE_ID="$CASE_ID" \
  ROUTECTL_FIXTURE_CONFIG_SHA="$CONFIG_SHA" \
  ROUTECTL_FIXTURE_CONNECTION_MODE="$CONNECTION_MODE" \
  ROUTECTL_FIXTURE_WIRE_PATTERN="$WIRE_PATTERN" \
  ROUTECTL_FIXTURE_EXPECTED_INGRESS="$EXPECTED_INGRESS" \
    exec "$@"
) >"$RUN/driver.log" 2>&1 || driver_rc=$?

if [ "$driver_rc" != 0 ]; then
  echo "capture_driver: driver command exited $driver_rc" >&2
  echo "--- tail of driver output ---" >&2
  tail -n 20 "$RUN/driver.log" >&2 || true
  exit 4
fi

# ---------------------------------------------------------------------
# Capture
# ---------------------------------------------------------------------

# Stop the daemon BEFORE the rig reads the trace: a still-running daemon
# can append mid-read, and the shutdown path is also where the last
# response's trace lines are flushed.
if kill -0 "$DAEMON_PID" 2>/dev/null; then
  kill "$DAEMON_PID" 2>/dev/null || true
  wait "$DAEMON_PID" 2>/dev/null || true
fi
DAEMON_PID=""

# `--force`: the run's trace is a fresh file, so the rig's resume marker
# (which lives in the landing root, across runs) would otherwise skip a
# rerun of the same case whose timestamps predate the marker -- and a
# rerun landing no fixture is exactly the drift signal this whole path
# exists to produce.
#
# `--allow-unsafe-out`: the rig confines a caller-supplied `--out` to its
# OWN `captured/` tree, and every driver landing root is outside it, so
# the rig's check would refuse every driver run. The flag lifts the rig's
# check only -- it does not lift this script's, which ran above against
# `--out-root` before any daemon booted. Deleting that check would make
# this line an unconfined write.
rig_rc=0
ROUTECTL_FIXTURE_CASE_ID="$CASE_ID" \
ROUTECTL_FIXTURE_CONFIG_SHA="$CONFIG_SHA" \
ROUTECTL_FIXTURE_CONNECTION_MODE="$CONNECTION_MODE" \
ROUTECTL_FIXTURE_WIRE_PATTERN="$WIRE_PATTERN" \
ROUTECTL_FIXTURE_EXPECTED_INGRESS="$EXPECTED_INGRESS" \
  bash "$RIG" --driver-mode --force \
    --log "$TRACE" \
    --out "$DRIVER_OUT" \
    --allow-unsafe-out || rig_rc=$?

# The mapping is explicit rather than a blanket non-zero -> 5, because the
# rig distinguishes a REFUSAL (exit 1: it produced a fixture and rejected
# it -- never retry) from a ZERO LANDING (exit 3: the case produced no
# completed request -- retryable). Collapsing them here would make the
# rig's distinction unobservable to any caller of this script.
if [ "$rig_rc" = 3 ]; then
  echo "capture_driver: case '$CASE_ID' on lane '$LANE' landed no fixture (see the rig's message)" >&2
  exit 7
fi
if [ "$rig_rc" != 0 ]; then
  echo "capture_driver: capture rig refused the fixture for case '$CASE_ID' on lane '$LANE'" >&2
  exit 5
fi

echo "capture_driver: lane=$LANE case=$CASE_ID port=$PORT${MITM_PORT:+ mitm_port=$MITM_PORT} config_sha=$CONFIG_SHA expected_ingress=$EXPECTED_INGRESS out=$DRIVER_OUT"
