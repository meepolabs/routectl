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
#                             [--connection-mode <mode>] \
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
#   --connection-mode  how the driver reaches routectl. Default
#                      `base-url`; pass `front-proxy` for a MITM run.
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
#   ROUTECTL_FIXTURE_CASE_ID          the three driver-mode pins, so a
#   ROUTECTL_FIXTURE_CONFIG_SHA       driver can echo them into its own
#   ROUTECTL_FIXTURE_CONNECTION_MODE  logs and read them back
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
#   2  usage error (unknown flag, missing lane config, no driver command)
#   3  the hermetic daemon never became healthy
#   4  the driver command exited non-zero
#   5  the capture rig refused the fixture (see its own message)
#   6  no free port in the search window
#   7  the rig ran clean but landed NO fixture -- the case produced no
#      completed request. Distinct from 5 on purpose: this one is
#      retryable, a refusal is a defect in what the case produced
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

# The driver corpus root, resolved from this script's own location the
# same way the rig resolves its default. It is NOT caller-supplied: a
# fixed repo-relative constant has no traversal or symlink surface to
# guard, which is why passing the rig's `--allow-unsafe-out` below is
# safe here. (The rig confines a caller-supplied `--out` to the live-box
# `captured/` tree; the driver root is its gitignored sibling, so driver
# mode's own documented landing root needs that flag.)
DRIVER_OUT="$ROOT/crates/routectl-cli/tests/fixtures/driver"

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
[ $# -ge 1 ] || die "no driver command given (pass it after \`--\`)" 2

# An inverted window makes `RANDOM % (MAX - MIN + 1)` a modulo by a
# non-positive number, which yields a candidate OUTSIDE the window -- so
# the caller gets a port it did not ask for instead of an error.
[ "$PORT_MAX" -ge "$PORT_MIN" ] ||
  die "port window $PORT_MIN-$PORT_MAX is inverted" 2

LANE_CONFIG="$CONFIG_DIR/$LANE.toml"
[ -r "$LANE_CONFIG" ] || die "no committed config for lane '$LANE' at $LANE_CONFIG" 2

# The sha is of the COMMITTED lane config, not of the copy the run boots
# from. The two differ only by the port the run selected, and hashing the
# patched copy would give every run a unique sha -- which would make
# `meta.config_sha` unable to tell a config change from a fresh run, the
# one question it exists to answer. (This build passes the port on the
# serve command line instead of patching, so the bytes happen to match
# today; hashing the committed file keeps that true if that ever changes.)
CONFIG_SHA="$(sha256sum "$LANE_CONFIG" | cut -d' ' -f1)"

# ---------------------------------------------------------------------
# Run workspace
# ---------------------------------------------------------------------

if [ -n "$WORK_PARENT" ]; then
  mkdir -p "$WORK_PARENT"
  RUN="$(mktemp -d "$WORK_PARENT/routectl-driver.XXXXXX")"
else
  RUN="$(mktemp -d)"
fi

RUN_HOME="$RUN/home"
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

# ---------------------------------------------------------------------
# Boot
# ---------------------------------------------------------------------

# The trace knobs the rig needs: body traces at TRACE on the log_safe
# target, header tracing on, and a body cap large enough that a real
# multi-turn body is not truncated mid-escape (the replay loader refuses
# a cap-truncated body).
#
# The port arrives on the command line rather than as a rewrite of the
# copied config: the committed bytes stay the identity the sha names, and
# there is one less file mutation between the hash and the boot.
(
  cd "$RUN_WORK"
  HOME="$RUN_HOME" \
  XDG_CONFIG_HOME="$RUN_XDG" \
  ROUTECTL_LOG="routectl=info,routectl_core::log_safe=trace" \
  ROUTECTL_TRACE_HEADERS=1 \
  ROUTECTL_TRACE_BODY_BYTES=2097152 \
    exec "$ROUTECTL_BIN" serve --host 127.0.0.1 --port "$PORT"
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
# Drive
# ---------------------------------------------------------------------

driver_rc=0
(
  cd "$RUN_WORK"
  HOME="$RUN_HOME" \
  XDG_CONFIG_HOME="$RUN_XDG" \
  ROUTECTL_BASE_URL="$BASE_URL" \
  ROUTECTL_DRIVER_PORT="$PORT" \
  ROUTECTL_DRIVER_RUN="$RUN" \
  ROUTECTL_DRIVER_WORK="$RUN_WORK" \
  ROUTECTL_FIXTURE_CASE_ID="$CASE_ID" \
  ROUTECTL_FIXTURE_CONFIG_SHA="$CONFIG_SHA" \
  ROUTECTL_FIXTURE_CONNECTION_MODE="$CONNECTION_MODE" \
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
# (which lives in the corpus, across runs) would otherwise skip a rerun
# of the same case whose timestamps predate the marker -- and a rerun
# landing no fixture is exactly the drift signal this whole path exists
# to produce.
rig_rc=0
ROUTECTL_FIXTURE_CASE_ID="$CASE_ID" \
ROUTECTL_FIXTURE_CONFIG_SHA="$CONFIG_SHA" \
ROUTECTL_FIXTURE_CONNECTION_MODE="$CONNECTION_MODE" \
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

echo "capture_driver: lane=$LANE case=$CASE_ID port=$PORT config_sha=$CONFIG_SHA"
