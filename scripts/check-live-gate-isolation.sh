#!/usr/bin/env bash
# Hostile-environment check for the standard test gate.
#
# Plants a synthetic value in every environment variable the live provider
# tests read, then runs the standard all-features workspace test gate inside
# a private network namespace and asserts it makes ZERO network attempts.
#
# Each leg runs in fresh user + network + pid namespaces. The network
# namespace has only a loopback interface, and every IPv4 / IPv6
# destination is routed to it, so no packet can leave the machine and every
# packet crosses `lo`. The recorder (net-oracle.py) captures on `lo` and
# logs every outgoing packet to a non-loopback destination -- any protocol,
# any port -- plus every packet to a resolver address, and answers DNS so
# clients fail fast. Traffic between loopback addresses is not a network
# attempt and is not logged.
#
# Legs, in order:
#
#   1. recorder control -- a DNS lookup, a connect to the name it resolved,
#      and direct-IP TCP / UDP to non-standard ports; the recorder MUST log
#      each one, or the check could not see such an attempt at all.
#   2. standard gate    -- `cargo test --workspace --all-features`; MUST
#      pass and log nothing.
#   3. live controls    -- each live target named explicitly, one leg per
#      target, under the same planted variables; EACH MUST log at least one
#      attempt, proving the planted values make that target dial out. Their
#      test verdicts are ignored: every live test fails against the recorder.
#
# A leg is valid only if the recorder was still running when the leg's
# command finished, then stopped cleanly on request with no error logged.
# A recorder that dies, raises, or drops packets fails the leg: an empty log
# from a dead recorder is not evidence. Every step has a deadline, and an
# interrupted run kills every process of the running leg.
#
# Dependencies are fetched first, outside the namespace and with no planted
# variable set; the legs then build and run `--offline` inside it, so build
# scripts are covered too. Cargo legs pass `--no-fail-fast` so a failing
# binary cannot stop the run before a later one had its chance to dial.
# Proxy variables are cleared for the namespaced legs.
#
# Needs util-linux `unshare` with unprivileged user namespaces, iproute2
# `ip`, python3, and bash >= 5.1. Without the namespace the check SKIPS BY
# NAME and exits 0, unless --require-netns is given (CI passes it), in which
# case the missing namespace is a failure.
#
# Run from anywhere inside the repo:
#   bash scripts/check-live-gate-isolation.sh [--require-netns]
#
# Exit codes: 0 = pass (or named skip), 1 = a leg failed, 2 = usage.

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
ORACLE="$HERE/net-oracle.py"
SELF="$HERE/$(basename "${BASH_SOURCE[0]}")"

LIVE_SOURCES=(
    crates/routectl-cli/tests/live_*.rs
    crates/routectl-cli/tests/live_matrix/*.rs
)
LIVE_TARGETS=(live_matrix live_anthropic_oauth)
STANDARD_GATE=(cargo test --workspace --all-features --offline --no-fail-fast)
live_command() {
    LIVE_COMMAND=(cargo test -p routectl-cli --features live-integration --offline --no-fail-fast
        --test "$1" -- --test-threads=1)
}
PROXY_VARS=(HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY http_proxy https_proxy all_proxy no_proxy)

# Deadlines, in seconds.
FETCH_DEADLINE=900
CONTROL_DEADLINE=120
STANDARD_DEADLINE=3600
LIVE_DEADLINE=1500
# Inside a leg: recorder start-up, and its clean stop once asked.
ORACLE_START_DEADLINE=10
ORACLE_STOP_DEADLINE=10
# Slack the outer hard kill allows beyond a leg's own deadline.
LEG_KILL_SLACK=60

log() { echo "check-live-gate-isolation: $*" >&2; }

# ---------------------------------------------------------------------------
# Inside the namespace (pid 1 of a fresh pid namespace, so every process of
# the leg dies when this returns). Invoked by the outer half only. Writes the
# leg's verdict to the status file as `leg_rc=<n>` and `recorder=<state>`.
# ---------------------------------------------------------------------------
wait_until_gone() {
    local pid="$1" tenths="$2"
    while kill -0 "$pid" 2>/dev/null; do
        ((tenths-- > 0)) || return 1
        sleep 0.1
    done
}

inside() {
    local log_path="$1" ready="$2" status="$3" deadline="$4"
    shift 4
    ip link set lo up || return 90
    ip route add local 0.0.0.0/0 dev lo || return 90
    # IPv6 may be disabled in the namespace; then it has no route at all.
    if ip -6 route add local ::/0 dev lo 2>/dev/null; then
        : >"$status.ipv6"
    fi

    python3 "$ORACLE" "$log_path" "$ready" &
    local oracle_pid=$! tenths=$((ORACLE_START_DEADLINE * 10))
    while [[ ! -e "$ready" ]]; do
        if ! kill -0 "$oracle_pid" 2>/dev/null || ((tenths-- <= 0)); then
            echo "recorder=did-not-start" >>"$status"
            return 90
        fi
        sleep 0.1
    done

    # The leg runs as the invoking user again, in a nested user namespace:
    # as namespace root, permission-denied tests would pass their writes.
    timeout --kill-after=10 "$deadline" \
        python3 "$ORACLE" --as-user "$OUTER_UID" "$OUTER_GID" -- "$@" &
    local leg_pid=$! first="" rc=0
    wait -n -p first "$oracle_pid" "$leg_pid"
    rc=$?
    if [[ "$first" == "$oracle_pid" ]]; then
        # Also covers the recorder winning a same-instant race with the leg.
        echo "recorder=died-before-leg-finished rc=$rc" >>"$status"
        return 91
    fi
    echo "leg_rc=$rc" >>"$status"
    if ! kill -0 "$oracle_pid" 2>/dev/null; then
        echo "recorder=died-before-leg-finished" >>"$status"
        return 91
    fi
    kill -TERM "$oracle_pid"
    if ! wait_until_gone "$oracle_pid" $((ORACLE_STOP_DEADLINE * 10)); then
        echo "recorder=did-not-stop" >>"$status"
        return 91
    fi
    local oracle_rc=0
    wait "$oracle_pid" || oracle_rc=$?
    echo "recorder=exited rc=$oracle_rc" >>"$status"
    return 0
}

if [[ "${1:-}" == "--inside" ]]; then
    shift
    inside "$@"
    exit $?
fi

# ---------------------------------------------------------------------------
# Outside the namespace.
# ---------------------------------------------------------------------------
require_netns=0
case "${1:-}" in
    "") ;;
    --require-netns) require_netns=1 ;;
    *)
        echo "usage: $0 [--require-netns]" >&2
        exit 2
        ;;
esac

skip_or_fail() {
    if ((require_netns)); then
        log "FAIL: $1"
        exit 1
    fi
    echo "SKIP: $1"
    exit 0
}

if ((BASH_VERSINFO[0] < 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] < 1))); then
    skip_or_fail "bash >= 5.1 required (wait -p), found $BASH_VERSION"
fi
for tool in unshare ip python3 cargo timeout pkill; do
    command -v "$tool" >/dev/null 2>&1 || skip_or_fail "required tool '$tool' not found"
done
UNSHARE=(unshare --user --map-root-user --net --pid --fork --kill-child --mount-proc)
"${UNSHARE[@]}" ip link set lo up 2>/dev/null ||
    skip_or_fail "cannot create unprivileged user + network + pid namespaces"

cd "$REPO_ROOT" || exit 1

WORK="$(mktemp -d)"
RUNNING_PID=""

# Kills whatever step is running -- for a leg, its `timeout` wrapper and the
# `unshare` under it, whose --kill-child takes the whole pid namespace down.
stop_running() {
    [[ -n "$RUNNING_PID" ]] || return 0
    pkill -KILL -P "$RUNNING_PID" 2>/dev/null
    kill -KILL "$RUNNING_PID" 2>/dev/null
    wait "$RUNNING_PID" 2>/dev/null
    RUNNING_PID=""
}
cleanup() {
    stop_running
    rm -rf "$WORK"
}
trap cleanup EXIT
trap 'log "interrupted"; exit 130' INT
trap 'log "terminated"; exit 143' TERM HUP

# Runs "$@" in the background under a hard deadline and waits for it, so a
# signal to this script is handled while the step runs. Sets STEP_RC
# (124 or 137 on a deadline).
run_bounded() {
    local deadline="$1"
    shift
    timeout --kill-after=10 "$deadline" "$@" &
    RUNNING_PID=$!
    STEP_RC=0
    wait "$RUNNING_PID" || STEP_RC=$?
    RUNNING_PID=""
}

# Every SCREAMING_SNAKE_CASE string literal in the live sources: env var
# names are read either inline or through a named const, and both are string
# literals there. A literal that is not an env var name is planted harmlessly.
derive_planted_names() {
    grep -ohE '"[A-Z][A-Z0-9]*(_[A-Z0-9]+)+"' "${LIVE_SOURCES[@]}" | tr -d '"' | sort -u
}

# A JWT-shaped bearer carrying the account claim the OAuth live tests
# require before they dial, so no test skips on token shape alone.
synthetic_jwt() {
    python3 - <<'PY'
import base64, json
def seg(obj):
    return base64.urlsafe_b64encode(json.dumps(obj).encode()).rstrip(b"=").decode()
claims = {"https://api.openai.com/auth": {"chatgpt_account_id": "synthetic-account"},
          "note": "synthetic value planted by a hostile-environment test"}
print(seg({"alg": "none", "typ": "JWT"}) + "." + seg(claims) + ".c3ludGhldGlj")
PY
}

build_planted_env() {
    local names jwt token_file name
    names="$(derive_planted_names)"
    if [[ -z "$names" ]]; then
        log "FAIL: no variable names derived from ${LIVE_SOURCES[*]}"
        return 1
    fi
    jwt="$(synthetic_jwt)" || return 1
    token_file="$WORK/synthetic-token"
    printf '%s\n' "$jwt" >"$token_file"
    PLANTED=()
    while IFS= read -r name; do
        case "$name" in
            *_FILE) PLANTED+=("$name=$token_file") ;;
            *_URL) PLANTED+=("$name=https://synthetic-upstream.example.com/v1") ;;
            *_REGION) PLANTED+=("$name=us-east-1") ;;
            *) PLANTED+=("$name=$jwt") ;;
        esac
    done <<<"$names"
    log "planted ${#PLANTED[@]} synthetic variables: $(tr '\n' ' ' <<<"$names")"
}

# Runs "$@" in fresh namespaces with the planted environment, under
# `deadline` seconds. Sets LEG_LOG (the recorder log) and LEG_ERROR (empty
# when the leg is valid evidence, else why it is not) and LEG_RC (the
# command's exit status; meaningful only when LEG_ERROR is empty).
run_leg() {
    local name="$1" deadline="$2"
    shift 2
    LEG_LOG="$WORK/$name.log"
    LEG_STATUS="$WORK/$name.status"
    : >"$LEG_LOG"
    : >"$LEG_STATUS"
    local unset_args=() var
    for var in "${PROXY_VARS[@]}"; do unset_args+=(-u "$var"); done
    run_bounded $((deadline + LEG_KILL_SLACK)) \
        env "${unset_args[@]}" "${PLANTED[@]}" OUTER_UID="$(id -u)" OUTER_GID="$(id -g)" \
        "${UNSHARE[@]}" \
        bash "$SELF" --inside "$LEG_LOG" "$WORK/$name.ready" "$LEG_STATUS" "$deadline" "$@"
    rm -f "$WORK/$name.ready"
    judge_leg "$STEP_RC"
}

# Sets LEG_ERROR and LEG_RC from a finished leg's status file and log.
judge_leg() {
    local inside_rc="$1" state
    LEG_ERROR=""
    LEG_RC="$(sed -n 's/^leg_rc=//p' "$LEG_STATUS")"
    state="$(sed -n 's/^recorder=//p' "$LEG_STATUS")"
    if [[ "$inside_rc" -eq 124 || "$inside_rc" -eq 137 ]]; then
        LEG_ERROR="leg exceeded its hard deadline and was killed"
    elif [[ "$inside_rc" -eq 90 ]]; then
        LEG_ERROR="namespace or recorder setup failed (${state:-no recorder state})"
    elif [[ "$inside_rc" -ne 0 || "$state" != "exited rc=0" ]]; then
        LEG_ERROR="recorder did not run for the whole leg and stop cleanly (${state:-no recorder state}, harness exit $inside_rc)"
    elif grep -q '^oracle-error' "$LEG_LOG"; then
        LEG_ERROR="recorder reported: $(grep -m1 '^oracle-error' "$LEG_LOG")"
    elif [[ "$(head -n1 "$LEG_LOG")" != "oracle-ready" || "$(tail -n1 "$LEG_LOG")" != "oracle-stopped" ]]; then
        LEG_ERROR="recorder log is missing its ready or stopped sentinel"
    elif [[ "$LEG_RC" -eq 124 || "$LEG_RC" -eq 137 ]]; then
        LEG_ERROR="the leg's command exceeded its deadline"
    fi
}

# Recorder lines that are network attempts: everything but its sentinels.
attempts() {
    grep -v '^oracle-' "$1" || true
}

fails=0
fail() {
    log "FAIL: $*"
    fails=$((fails + 1))
}

build_planted_env || exit 1

log "fetching dependencies outside the namespace"
run_bounded "$FETCH_DEADLINE" env -u CARGO_NET_OFFLINE cargo fetch --locked
if ((STEP_RC != 0)); then
    log "FAIL: cargo fetch failed or timed out (exit $STEP_RC)"
    exit 1
fi

CONTROL_PROBE='
import os, socket, sys
def attempt(fn):
    try:
        fn()
    except OSError:
        pass
attempt(lambda: socket.create_connection(("recorder-control.example.com", 443), timeout=2).close())
attempt(lambda: socket.create_connection(("198.51.100.7", 8443), timeout=2).close())
attempt(lambda: socket.socket(socket.AF_INET, socket.SOCK_DGRAM).sendto(b"x", ("192.0.2.1", 9999)))
if os.path.exists(sys.argv[1]):
    attempt(lambda: socket.create_connection(("2001:db8::7", 2222), timeout=2).close())
'
CONTROL_EXPECTED=(
    '^dns qtype=[0-9]+ name=recorder-control\.example\.com$'
    '^tcp dest=203\.0\.113\.10:443$'
    '^tcp dest=198\.51\.100\.7:8443$'
    '^udp dest=192\.0\.2\.1:9999$'
)

log "leg 1: recorder control"
run_leg control "$CONTROL_DEADLINE" python3 -c "$CONTROL_PROBE" "$WORK/control.status.ipv6"
control="$(attempts "$LEG_LOG")"
expected=("${CONTROL_EXPECTED[@]}")
if [[ -e "$WORK/control.status.ipv6" ]]; then
    expected+=('^tcp dest=\[2001:db8::7\]:2222$')
else
    log "note: IPv6 is disabled in the namespace, so no IPv6 attempt can be made"
fi
if [[ -n "$LEG_ERROR" ]]; then
    fail "recorder control: $LEG_ERROR"
else
    missing=()
    for pattern in "${expected[@]}"; do
        grep -qE "$pattern" <<<"$control" || missing+=("$pattern")
    done
    if ((${#missing[@]})); then
        fail "recorder control: not recorded: ${missing[*]}; recorded: ${control:-<nothing>}"
    else
        echo "PASS: recorder sees DNS, name-resolved TCP, and direct-IP TCP / UDP on non-standard ports"
    fi
fi

log "leg 2: standard gate: ${STANDARD_GATE[*]}"
run_leg standard "$STANDARD_DEADLINE" "${STANDARD_GATE[@]}"
standard="$(attempts "$LEG_LOG")"
if [[ -n "$LEG_ERROR" ]]; then
    fail "standard gate: $LEG_ERROR"
elif [[ -n "$standard" ]]; then
    fail "standard gate made $(wc -l <<<"$standard") network attempt(s):"
    sort <<<"$standard" | uniq -c | sort -rn | head -40 >&2
elif [[ "$LEG_RC" -ne 0 ]]; then
    fail "standard gate exited $LEG_RC under the planted environment; zero attempts from a failed run proves nothing"
else
    echo "PASS: standard gate passed with zero network attempts"
fi

for target in "${LIVE_TARGETS[@]}"; do
    live_command "$target"
    log "leg: live control for $target: ${LIVE_COMMAND[*]}"
    run_leg "live-$target" "$LIVE_DEADLINE" "${LIVE_COMMAND[@]}"
    live="$(attempts "$LEG_LOG")"
    if [[ -n "$LEG_ERROR" ]]; then
        fail "live control $target: $LEG_ERROR"
    elif [[ -z "$live" ]]; then
        fail "live control $target: made no network attempt under the planted variables, so the standard-gate leg is not evidence for it (cargo exit $LEG_RC)"
    else
        echo "PASS: live target $target dials out under the same variables ($(wc -l <<<"$live") attempts recorded)"
    fi
done

if ((fails)); then
    log "$fails leg(s) failed"
    exit 1
fi
echo "check-live-gate-isolation: all legs passed"
