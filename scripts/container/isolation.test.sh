#!/usr/bin/env bash
# Verify that a DEFAULT-BRIDGE container cannot reach the host's live
# routectl daemon -- and that the probe which says so can reach anything
# at all.
#
# WHY THE NEGATIVE ALONE IS NOT THE TEST. "The container could not reach
# the daemon" is what a broken probe reports too: a wrong gateway
# address, a curl that is not in the image, a typo in the port. Both
# halves therefore go through ONE probe function differing only in
# host:port:
#
#   the NEGATIVE  -- the live daemon's port at the host-gateway address
#                    must be UNREACHABLE.
#   the CONTROL   -- a throwaway host listener bound on 0.0.0.0:<free
#                    port> must be REACHABLE at that SAME gateway
#                    address, from that SAME container, in that same run.
#
# The control is the load-bearing half. Without it every assertion below
# is satisfiable by a probe that reaches nothing.
#
# THE LOOPBACK PROBE IS THE WEAK HALF and is asserted only for
# completeness. `127.0.0.1` inside a container is the CONTAINER'S OWN
# loopback, so that probe fails however broken the host isolation is --
# it can never distinguish a contained container from an escaped one. Do
# not read it as the proof; the gateway pair above is the proof.
#
# WHAT THIS TEST IS ALLOWED TO TOUCH ON THE HOST. The daemon under test
# is a LIVE service on this box, not a stub: it is the thing whose
# unreachability is being asserted, so it has to be up. This test
# therefore reads `/health` and lists the ledger directory, and does
# nothing else to it -- no signal of any kind, and no database client.
# Both restrictions are asserted lexically against this file at the end,
# with a positive control, because an absence is what review is worst at
# enforcing.
#
# The ledger check is by `ls`, deliberately: a missing `-shm` beside a
# WAL-mode database IS the fault, and a database client opens read-write
# by default and CHECKPOINTS on clean exit, which deletes the very
# sidecars whose presence is the assertion. Opening it to check it is
# what breaks it.
#
# A REAL ACCEPTING LISTENER, not a bound socket. Binding without
# listening refuses every dial, and binding-and-listening-without-reading
# lets a TCP connect SUCCEED -- so a half-built listener can invert
# either half of the pair. The control serves real responses, and the
# negative wants nothing bound at all.
#
# Requires docker, the built image, python3, and the live daemon:
#   docker absent      -> the container legs SKIP BY NAME.
#   image not built    -> same; build it with scripts/container/build.sh.
#   daemon not healthy -> the live-daemon legs SKIP BY NAME. With nothing
#                         bound on the host there is no isolation to
#                         assert, and a green negative would be vacuous.
#                         This is the state CI runs in.
#   python3 absent     -> FAILS. It picks the free port and serves the
#                         control listener, so a skip would remove the
#                         load-bearing half.
#
# Run it from anywhere:
#   bash scripts/container/isolation.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

IMAGE="routectl-capture:default"

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/routectl"
LEDGER="$CONFIG_DIR/usage.db"

# Scratch on REAL disk outside the repo. Not $TMPDIR: /tmp here is a
# tmpfs small enough that unrelated gates already hit StorageFull on it.
WORK_PARENT="${TMPDIR:-/var/tmp}"
case "$WORK_PARENT" in
    /tmp | /tmp/*) WORK_PARENT="/var/tmp" ;;
esac

fails=0

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- expected '$expected', got '$actual'"
        fails=$((fails + 1))
    fi
}

fail() {
    echo "FAIL: $1"
    fails=$((fails + 1))
}

# A named skip: the caller must be able to read WHAT was not verified off
# the log, which is the whole difference between a skip and a pass.
skip() {
    echo "SKIP: $1"
}

if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 not found; there would be no free-port pick and no"
    echo "      control listener, so the load-bearing half of this suite"
    echo "      cannot run"
    exit 1
fi

# ---------------------------------------------------------------------
# The live daemon's port, resolved rather than written down
# ---------------------------------------------------------------------
#
# ONE PLACE holds the number, and it is a last-resort fallback: the port
# is taken from the environment first, then from the daemon's own config,
# so a box that moved its daemon is probed at the port it actually
# listens on rather than at a stale constant. The final guard in this
# file asserts the literal appears exactly once, on the line below.
LIVE_PORT_FALLBACK=9100

live_port_from_config() {
    [ -r "$CONFIG_DIR/config.toml" ] || return 0
    sed -n 's/^[[:space:]]*port[[:space:]]*=[[:space:]]*\([0-9]\{1,\}\).*/\1/p' \
        "$CONFIG_DIR/config.toml" | head -n 1
}

LIVE_PORT="${ROUTECTL_LIVE_PORT:-$(live_port_from_config)}"
LIVE_PORT="${LIVE_PORT:-$LIVE_PORT_FALLBACK}"

case "$LIVE_PORT" in
    '' | *[!0-9]*)
        echo "FAIL: the live daemon's port did not resolve to a number"
        exit 1
        ;;
esac
echo "PASS: the live daemon's port resolved to a number ($LIVE_PORT)"

# ---------------------------------------------------------------------
# Is the daemon actually up? The negative is vacuous if it is not
# ---------------------------------------------------------------------

host_health_code() {
    curl -s -o /dev/null -w '%{http_code}' --max-time 4 --connect-timeout 4 \
        "http://127.0.0.1:$LIVE_PORT/health" 2>/dev/null || printf '000'
}

DAEMON_UP=no
if [ "$(host_health_code)" = "200" ]; then
    DAEMON_UP=yes
fi

WORK="$(mktemp -d "$WORK_PARENT/routectl-isolation-test.XXXXXX")" || exit 1

LISTENER_PID=""

# shellcheck disable=SC2329 # invoked indirectly, by the EXIT trap below
cleanup() {
    # BY ITS OWN CAPTURED PID, never by name or pattern. A pattern-based
    # signal on a box running the live daemon is how the daemon gets
    # taken down by a test that was only meant to tidy up after itself.
    if [ -n "${LISTENER_PID:-}" ]; then
        kill "$LISTENER_PID" 2>/dev/null || true
        wait "$LISTENER_PID" 2>/dev/null || true
        LISTENER_PID=""
    fi
    # Validated non-empty before use: a command substitution strips
    # trailing newlines and an empty one turns `rm -rf "$x"` into a
    # deletion of the wrong tree.
    if [ -n "${WORK:-}" ] && [ -d "$WORK" ]; then
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------
# The control listener
# ---------------------------------------------------------------------

write_listener() {
    cat >"$1" <<'PY'
import http.server
import socketserver
import sys


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"status":"control"}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


# 0.0.0.0, not loopback: the whole point of the control is to be
# reachable at the host-gateway address the container dials.
socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("0.0.0.0", int(sys.argv[1])), Handler) as srv:
    srv.serve_forever()
PY
}

# An ephemeral port the kernel says is free. Asked of the kernel rather
# than picked, so the suite does not collide with whatever else this box
# is running.
free_port() {
    python3 - <<'PY'
import socket

s = socket.socket()
s.bind(("", 0))
print(s.getsockname()[1])
s.close()
PY
}

# The probe both halves of the pair run, byte-identical apart from
# host:port. Emits `<label>=<verdict>` and `<label>_rc=<code>`.
write_probe() {
    cat >"$1" <<'SH'
set -u

# The host as seen from a default-bridge container. RESOLVED, never
# written down: the bridge subnet differs between boxes and between
# docker networks, and a hardcoded address would make the negative pass
# by dialling nothing at all.
gw="$(ip route show default 2>/dev/null | awk '/^default/ { print $3; exit }')"
printf 'gateway=%s\n' "$gw"

probe() {
    local label="$1" host="$2" port="$3" rc=0
    curl -s -o /dev/null --max-time 5 --connect-timeout 5 \
        "http://$host:$port/health" || rc=$?
    if [ "$rc" -eq 0 ]; then
        printf '%s=reachable\n' "$label"
    else
        printf '%s=unreachable\n' "$label"
    fi
    printf '%s_rc=%s\n' "$label" "$rc"
}

probe gateway_live "$gw" "$PROBE_LIVE_PORT"
probe loopback_live 127.0.0.1 "$PROBE_LIVE_PORT"
probe gateway_control "$gw" "$PROBE_CONTROL_PORT"
SH
}

probe_get() {
    sed -n "s/^$2=//p" "$1" | head -1
}

# ---------------------------------------------------------------------
# The container legs
# ---------------------------------------------------------------------

run_container_legs() {
    local probe="$WORK/probe.sh" listener="$WORK/listener.py"
    local out="$WORK/probe.out" control_port ready=no tries=10
    write_probe "$probe"
    write_listener "$listener"

    control_port="$(free_port)"
    case "$control_port" in
        '' | *[!0-9]*)
            fail "a free port was obtained for the control listener"
            return
            ;;
    esac

    # WELL AWAY from the daemon's port, asserted rather than assumed: a
    # control bound next door to the live port would make an
    # off-by-a-little probe look like a working pair.
    if [ "$control_port" -gt $((LIVE_PORT + 1000)) ] ||
        [ "$control_port" -lt $((LIVE_PORT - 1000)) ]; then
        echo "PASS: the control port is well away from the daemon's ($control_port)"
    else
        fail "the control port is well away from the daemon's -- got $control_port"
        return
    fi

    python3 "$listener" "$control_port" &
    LISTENER_PID=$!

    # Readiness is polled, not slept for: the container leg has one shot
    # at the control, and a listener that had not finished binding would
    # redden the load-bearing half for a reason that is not isolation.
    while [ "$tries" -gt 0 ]; do
        tries=$((tries - 1))
        if curl -s -o /dev/null --max-time 2 --connect-timeout 2 \
            "http://127.0.0.1:$control_port/health"; then
            ready=yes
            break
        fi
        sleep 0.5
    done
    check "the control listener is accepting connections before the probe runs" \
        "yes" "$ready"
    if [ "$ready" != yes ]; then
        return
    fi

    # DEFAULT BRIDGE, stated explicitly. `--network host` would give the
    # container the host's own loopback and make every assertion below
    # meaningless, so the network mode is not left to a daemon default
    # that a later edit could change.
    if ! docker run --rm --network bridge \
        -e PROBE_LIVE_PORT="$LIVE_PORT" \
        -e PROBE_CONTROL_PORT="$control_port" \
        -v "$probe:/probe.sh:ro" \
        "$IMAGE" bash /probe.sh >"$out" 2>"$WORK/probe.err"; then
        fail "the probe ran inside the container"
        sed -n '1,10p' "$WORK/probe.err"
        return
    fi
    echo "PASS: the probe ran inside a default-bridge container"

    local gateway
    gateway="$(probe_get "$out" gateway)"
    if [ -n "$gateway" ]; then
        echo "PASS: the container resolved a host-gateway address ($gateway)"
    else
        fail "the container resolved a host-gateway address -- got nothing, so every gateway probe below dialled an empty host"
        return
    fi

    # The gateway must not BE the loopback address, or the gateway leg
    # collapses into the weak loopback leg and the pair proves nothing
    # about the host.
    if [ "$gateway" = "127.0.0.1" ]; then
        fail "the host-gateway address is distinct from the container's loopback -- got 127.0.0.1"
    else
        echo "PASS: the host-gateway address is distinct from the container's loopback"
    fi

    # --- THE PAIR ----------------------------------------------------
    # The control FIRST. Read in this order, a failure here says "the
    # probe is broken" before the negative below can be misread as
    # "isolation holds".
    check "POSITIVE CONTROL: a host listener IS reachable at the gateway" \
        "reachable" "$(probe_get "$out" gateway_control)"
    check "the control probe's own exit code is success" "0" \
        "$(probe_get "$out" gateway_control_rc)"

    check "the live daemon's port is UNREACHABLE at the host-gateway address" \
        "unreachable" "$(probe_get "$out" gateway_live)"

    # The weak half, asserted for completeness. See the header: this
    # probe fails however broken the host isolation is.
    check "the live daemon's port is unreachable at the container's own loopback" \
        "unreachable" "$(probe_get "$out" loopback_live)"

    # --- take the control listener down ------------------------------
    cleanup_listener_and_assert "$control_port"
}

# The listener is taken down, and the takedown is ASSERTED rather than
# assumed: a leaked listener bound on 0.0.0.0 outlives the test as an
# open port on the box.
cleanup_listener_and_assert() {
    local control_port="$1"
    local port_gone=no tries=6
    if [ -n "${LISTENER_PID:-}" ]; then
        kill "$LISTENER_PID" 2>/dev/null || true
        wait "$LISTENER_PID" 2>/dev/null || true
        LISTENER_PID=""
    fi
    while [ "$tries" -gt 0 ]; do
        tries=$((tries - 1))
        if ! curl -s -o /dev/null --max-time 2 --connect-timeout 2 \
            "http://127.0.0.1:$control_port/health"; then
            port_gone=yes
            break
        fi
        sleep 0.5
    done
    check "the control listener is down after the run" "yes" "$port_gone"
}

if ! command -v docker >/dev/null 2>&1; then
    skip "the container legs -- docker is not installed or not on PATH. Nothing"
    skip "  verified that a bridge container is refused at the live daemon's"
    skip "  port, and nothing verified that the probe saying so can reach"
    skip "  anything at all."
elif ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    skip "the container legs -- $IMAGE is not built locally, so no probe ran"
    skip "  inside it. Build it with scripts/container/build.sh."
elif [ "$DAEMON_UP" != yes ]; then
    skip "the container legs -- no daemon answers /health on this host, so"
    skip "  nothing is bound at the port whose unreachability is the"
    skip "  assertion, and a green negative would be vacuous."
else
    run_container_legs
fi

# ---------------------------------------------------------------------
# The post-run host check
# ---------------------------------------------------------------------
#
# The daemon this box routes through was live throughout. Asserting it is
# STILL live, with its ledger sidecars intact, is what distinguishes a
# test that observed the isolation from one that disturbed the thing it
# was observing.

if [ "$DAEMON_UP" = yes ]; then
    check "the live daemon still answers /health after the run" "200" \
        "$(host_health_code)"

    # By `ls`, never by opening the database. See the header.
    check "the live ledger is still present" "present" \
        "$(ls "$LEDGER" >/dev/null 2>&1 && echo present || echo ABSENT)"
    check "the ledger's shared-memory sidecar is still present" "present" \
        "$(ls "$LEDGER-shm" >/dev/null 2>&1 && echo present || echo ABSENT)"
    check "the ledger's write-ahead log sidecar is still present" "present" \
        "$(ls "$LEDGER-wal" >/dev/null 2>&1 && echo present || echo ABSENT)"
else
    skip "the post-run host check -- no daemon answered /health before the run"
    skip "  either, so there was nothing to disturb and nothing to re-verify."
fi

# ---------------------------------------------------------------------
# This file does nothing to the live install but read it
# ---------------------------------------------------------------------
#
# The three rules are read off this file's own source. The banned words
# are ASSEMBLED FROM FRAGMENTS rather than written whole, because a guard
# that spells its own forbidden literal fires on itself -- the patterns
# and the control below would then be indistinguishable from the
# regression they exist to catch.
SELF="${BASH_SOURCE[0]}"
frag() { printf '%s%s' "$1" "$2"; }

SIGNAL_BY_NAME="$(frag 'pk' 'ill')"
SIGNAL_ALL="$(frag 'kill' 'all')"
DB_CLIENT="$(frag 'sqli' 'te3')"

SIGNAL_RE="$SIGNAL_BY_NAME|$SIGNAL_ALL"
DB_CLIENT_RE="(^|[;&|[:space:]])(/usr/bin/)?$DB_CLIENT([[:space:]]|$)"

check "this file signals nothing by name or pattern" "0" \
    "$(grep -cE -- "$SIGNAL_RE" "$SELF" || true)"
check "this file invokes no database client against the live ledger" "0" \
    "$(grep -cE -- "$DB_CLIENT_RE" "$SELF" || true)"

# EXACTLY ONE: the fallback that resolves the port. More than one means a
# probe went back to a written-down number and stopped following the
# daemon; zero means the fallback was deleted and this guard no longer
# has anything to hold. Asserted against the FALLBACK rather than the
# resolved value: an override moves the resolved port to a number this
# file never spells, which would make the guard pass by finding nothing.
check "this file names the live port in exactly one place" "1" \
    "$(grep -cF -- "$LIVE_PORT_FALLBACK" "$SELF" || true)"

# Positive control for the three greps above: every pattern MUST fire on
# a file that does contain what it looks for, in the placements a real
# regression would use -- a bare command, a piped command, and the port
# written twice. Without it, a mistyped pattern would read as three
# passes. Built at RUNTIME so no committed file in this repo carries the
# literals, which is also what keeps this file's own guard honest.
control="$(mktemp "$WORK/hygiene-control.XXXXXX")"
{
    printf '%s routectl\n' "$SIGNAL_BY_NAME"
    printf 'ps -e | %s -r routectl\n' "$SIGNAL_ALL"
    printf '%s %s\n' "$DB_CLIENT" "$LEDGER"
    printf 'curl 127.0.0.1:%s/health\n' "$LIVE_PORT_FALLBACK"
    printf 'nc -z 127.0.0.1 %s\n' "$LIVE_PORT_FALLBACK"
} >"$control"
control_hits=0
for pattern in "$SIGNAL_RE" "$DB_CLIENT_RE"; do
    if grep -qE -- "$pattern" "$control"; then
        control_hits=$((control_hits + 1))
    fi
done
if [ "$(grep -cF -- "$LIVE_PORT_FALLBACK" "$control")" -gt 1 ]; then
    control_hits=$((control_hits + 1))
fi
check "the live-install greps fire on a file that does contain them" "3" \
    "$control_hits"
rm -f "$control"

# The assembly is itself a fault surface: an empty fragment would make
# every pattern above match nothing and all three guards pass vacuously.
check "the assembled guard words are non-empty" "yes" \
    "$([ -n "$SIGNAL_BY_NAME" ] && [ -n "$SIGNAL_ALL" ] && [ -n "$DB_CLIENT" ] &&
        echo yes || echo no)"

# The sibling self-tests are the reason this one can be narrow: it
# asserts network isolation only, and says so where a reader looking for
# the mount and credential rules will find the pointer.
check "the wrapper self-test that owns the mount and refusal rules exists" "yes" \
    "$([ -r "$HERE/run_capture.test.sh" ] && echo yes || echo no)"

echo
if [ "$fails" -eq 0 ]; then
    echo "capture cell isolation: all assertions passed"
    exit 0
fi
echo "capture cell isolation: $fails assertion(s) failed"
exit 1
