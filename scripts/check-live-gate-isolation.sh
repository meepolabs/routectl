#!/usr/bin/env bash
# Hostile-environment check for the standard test gate.
#
# Plants a synthetic value in every environment variable the live provider
# tests read, then runs the standard all-features workspace test gate inside
# a private network namespace and asserts it makes ZERO network attempts.
#
# Each leg runs in fresh user + network + pid + mount namespaces. The network
# namespace has only a loopback interface, and every IPv4 / IPv6
# destination is routed to it, so no packet can leave the machine and every
# packet crosses `lo`. The recorder (net-oracle.py) captures on `lo` and
# logs every outgoing packet to a non-loopback destination -- any protocol,
# any port -- plus every packet to a resolver address, and answers DNS so
# clients fail fast. Traffic between loopback addresses is not a network
# attempt and is not logged.
#
# A Unix socket is a way out the packet recorder cannot see (a Docker or
# D-Bus daemon, the host resolver's varlink socket, an SSH agent), so each
# leg's mount namespace masks the host runtime socket trees (/run, and
# /var/run where it is not a symlink into /run) with an empty tmpfs, then
# recreates the resolver config inside it when /etc/resolv.conf pointed
# there. SSH_AUTH_SOCK, DBUS_SESSION_BUS_ADDRESS, DBUS_SYSTEM_BUS_ADDRESS
# and DOCKER_HOST are unset, and a socket they named outside those trees is
# covered by a bind mount. Abstract Unix sockets belong to the network
# namespace and are already private. Every leg first checks that a
# test-owned canary socket in the masked tree, reachable outside, cannot be
# reached inside.
#
# Legs, in order:
#
#   1. recorder control -- a DNS lookup, a connect to the name it resolved,
#      and direct-IP TCP / UDP to non-standard ports; the recorder MUST log
#      each one, or the check could not see such an attempt at all. The same
#      leg connects to the canary and to every host daemon socket present
#      outside (Docker, D-Bus, resolver, nscd, SSH agent); each MUST fail.
#   2. standard gate    -- `scripts/test-gate.sh workspace-all-features`, the
#      all-features workspace suite on the `test-release` profile; MUST pass
#      and log nothing.
#   3. live controls    -- each live target named explicitly, one leg per
#      target, under the same planted variables; EACH MUST log at least one
#      attempt, proving the planted values make that target dial out. Their
#      test verdicts are ignored: every live test fails against the recorder.
#
# A leg is valid only if the recorder was still running when the leg's
# command finished, then stopped cleanly on request with no error logged.
# A recorder that dies, raises, or drops packets fails the leg: an empty log
# from a dead recorder is not evidence. Before the recorder is asked to stop,
# every other process left in the leg's pid namespace (a daemon a test
# spawned and never reaped) is killed and awaited, so no process of the leg
# outlives the capture. Every step has a deadline, and an interrupted run
# kills every process of the running leg.
#
# The deadlines sum, with every kill slack and the cleanup allowance, to
# less than STEP_BUDGET, the CI step timeout; the script refuses to start
# otherwise, and check-live-gate-isolation.test.sh ties STEP_BUDGET to the
# workflow.
#
# Dependencies are fetched first, outside the namespace and with no planted
# variable set; the legs then build and run `--offline` inside it, so build
# scripts are covered too. Cargo legs pass `--no-fail-fast` so a failing
# binary cannot stop the run before a later one had its chance to dial.
# Proxy variables are cleared for the namespaced legs.
#
# Every leg runs with HOME and XDG_CONFIG_HOME pointed at its own scratch
# directories under the run's work dir, so nothing a leg runs can read or
# write the invoking user's real config or data (a usage ledger resolved
# from HOME, say). The leg also sets CARGO_ENV_XDG_CONFIG_HOME, because the
# repo's .cargo/config.toml forces XDG_CONFIG_HOME (to target/test-xdg) for
# every binary cargo runs, and only cargo's environment form of that [env]
# entry replaces a forced value. RUSTUP_HOME and CARGO_HOME are taken from the outer
# environment first and passed through, so the toolchain proxies and the
# fetched registry cache stay reachable.
#
# Needs util-linux `unshare` with unprivileged user namespaces, iproute2
# `ip`, python3, bash >= 5.1, and a directory under /run the invoking user
# can write for the canary ($XDG_RUNTIME_DIR, or the sticky /run/lock).
# Without the namespace the check SKIPS BY NAME and exits 0, unless --require-netns is given (CI passes it), in which
# case the missing namespace is a failure.
#
# Run from anywhere:
#   bash scripts/check-live-gate-isolation.sh [--require-netns]
#   bash scripts/check-live-gate-isolation.sh --self-check
#
# --self-check runs no namespace and no cargo: it checks the deadline budget
# and prints it, then prints the planted variable names, one per line.
#
# Exit codes: 0 = pass (or named skip), 1 = a leg failed, 2 = usage.

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
ORACLE="$HERE/net-oracle.py"
SELF="$HERE/$(basename "${BASH_SOURCE[0]}")"

LIVE_SOURCES=(
    "$REPO_ROOT"/crates/routectl-cli/tests/live_*.rs
    "$REPO_ROOT"/crates/routectl-cli/tests/live_matrix/*.rs
    "$REPO_ROOT"/crates/routectl-router/tests/live_*.rs
)
# Credential variables planted whatever the source scan finds: the product's
# provider credentials (config `env://` conventions and the AWS chain).
FIXED_PLANTED_NAMES=(
    ANTHROPIC_API_KEY
    OPENAI_API_KEY
    GEMINI_API_KEY
    AWS_ACCESS_KEY_ID
    AWS_SECRET_ACCESS_KEY
    AWS_SESSION_TOKEN
    AWS_BEARER_TOKEN_BEDROCK
    AWS_REGION
)
# Every `test = false` target gated on `live-integration`, in any crate;
# check-live-gate-isolation.test.sh ties this list to the manifests.
LIVE_TARGETS=(live_matrix live_anthropic_oauth live_learned_capability)
# The standard gate runs through the gate-command registry, so this check and
# every other caller of that subcommand run the same command.
STANDARD_GATE=(bash "$HERE/test-gate.sh" workspace-all-features)
# The standard gate's selection (--workspace --all-features, which includes
# live-integration, on the same profile) narrowed by --test, so each live leg
# resolves the same feature set and reuses that build: it compiles only its
# own test target, which the standard gate never builds (`test = false`),
# instead of every crate again under another feature set.
live_command() {
    LIVE_COMMAND=(cargo test --workspace --all-features --profile test-release
        --offline --no-fail-fast --test "$1" -- --test-threads=1)
}
PROXY_VARS=(HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY http_proxy https_proxy all_proxy no_proxy)
HOST_SOCKET_VARS=(SSH_AUTH_SOCK DBUS_SESSION_BUS_ADDRESS DBUS_SYSTEM_BUS_ADDRESS DOCKER_HOST)
MASKED_TREES=(/run /var/run)
# Host daemon sockets the control leg must fail to reach, when present.
WELL_KNOWN_SOCKETS=(
    /run/docker.sock
    /var/run/docker.sock
    /run/containerd/containerd.sock
    /run/dbus/system_bus_socket
    /run/systemd/resolve/io.systemd.Resolve
    /run/nscd/socket
    /run/systemd/journal/stdout
)

# Deadlines, in seconds. STEP_BUDGET is the CI step's timeout-minutes * 60:
# worst_case_seconds plus BUDGET_HEADROOM, rounded up to a whole minute.
# check-live-gate-isolation.test.sh derives that sum and asserts it.
STEP_BUDGET=6240
FETCH_DEADLINE=300
CONTROL_DEADLINE=120
STANDARD_DEADLINE=3000
LIVE_DEADLINE=750
# `timeout --kill-after` grace for every bounded command.
KILL_AFTER=10
# Inside a leg: recorder start-up, residual-process kill, recorder stop.
ORACLE_START_DEADLINE=10
RESIDUAL_KILL_DEADLINE=10
ORACLE_STOP_DEADLINE=10
# Slack the outer hard kill allows beyond a leg's own deadline; it must
# cover everything inside() does besides the leg's command.
LEG_KILL_SLACK=60
# Canary start-up, trap cleanup, and the verdict, after the last leg.
CLEANUP_ALLOWANCE=60
# Required headroom between the worst case and STEP_BUDGET.
BUDGET_HEADROOM=120

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

# Connects to each Unix socket path given and writes `<result> <path>` per
# path to argv[1]: `connected`, or the errno name the connect failed with.
UNIX_PROBE_LIB='
import errno, os, socket, sys
def probe_unix(out_path, paths):
    with open(out_path, "w", encoding="utf-8", errors="replace") as out:
        for path in paths:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(2)
            try:
                s.connect(path)
                result = "connected"
            except OSError as exc:
                result = errno.errorcode.get(exc.errno, "error")
            finally:
                s.close()
            out.write(result + " " + path + "\n")
'

# Resolver config survives the mask: when /etc/resolv.conf resolves into a
# masked tree, its content is written back at the same path.
mask_host_sockets() {
    local resolv_target resolv_content="" tree real path
    resolv_target="$(readlink -f /etc/resolv.conf)" || return 1
    resolv_content="$(cat /etc/resolv.conf)" || return 1
    local -A masked=()
    for tree in "${MASKED_TREES[@]}"; do
        [[ -d "$tree" ]] || continue
        real="$(readlink -f "$tree")"
        [[ -n "${masked[$real]:-}" ]] && continue
        mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs "$real" || return 1
        masked[$real]=1
    done
    if ! [[ -r /etc/resolv.conf ]]; then
        mkdir -p "$(dirname "$resolv_target")" || return 1
        printf '%s\n' "$resolv_content" >"$resolv_target" || return 1
    fi
    while IFS= read -r path; do
        [[ -n "$path" && -S "$path" ]] || continue
        mount --bind /dev/null "$path" || return 1
    done <<<"${MASK_SOCKET_PATHS:-}"
}

# Kills and awaits every process of this pid namespace but this shell and
# the recorder. Orphans reparent to this shell (pid 1), which reaps them; a
# zombie cannot act, so it counts as gone.
kill_residual() {
    local keep="$1" status="$2" tenths=$((RESIDUAL_KILL_DEADLINE * 10)) entry pid stat survivors
    local first=1
    while :; do
        survivors=()
        for entry in /proc/[0-9]*; do
            pid="${entry#/proc/}"
            [[ "$pid" == "$BASHPID" || "$pid" == "$keep" ]] && continue
            read -r stat <"$entry/stat" 2>/dev/null || continue
            stat="${stat##*) }"
            [[ "${stat%% *}" == Z ]] && continue
            survivors+=("$pid")
        done
        if ((first)); then
            echo "residual_killed=${#survivors[@]}" >>"$status"
            first=0
        fi
        ((${#survivors[@]})) || return 0
        kill -KILL "${survivors[@]}" 2>/dev/null
        ((tenths-- > 0)) || return 1
        sleep 0.1
    done
}

inside() {
    local log_path="$1" ready="$2" status="$3" deadline="$4"
    shift 4
    if ! mask_host_sockets; then
        echo "unix-mask=setup-failed" >>"$status"
        return 90
    fi
    local canary_seen
    canary_seen="$(python3 -c "$UNIX_PROBE_LIB
probe_unix('/dev/stdout', ['$CANARY_SOCKET'])")"
    if [[ "$canary_seen" != "ENOENT $CANARY_SOCKET" ]]; then
        echo "unix-mask=canary-visible ($canary_seen)" >>"$status"
        return 92
    fi
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
    timeout --kill-after="$KILL_AFTER" "$deadline" \
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
    if ! kill_residual "$oracle_pid" "$status"; then
        echo "recorder=residual-processes-survived" >>"$status"
        return 91
    fi
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
# Worst-case wall time of a full run, from every deadline and kill slack.
worst_case_seconds() {
    local leg_cost=$((LEG_KILL_SLACK + KILL_AFTER))
    echo $((FETCH_DEADLINE + KILL_AFTER +
        CONTROL_DEADLINE + leg_cost +
        STANDARD_DEADLINE + leg_cost +
        ${#LIVE_TARGETS[@]} * (LIVE_DEADLINE + leg_cost) +
        CLEANUP_ALLOWANCE))
}

check_budget() {
    local worst inner
    worst="$(worst_case_seconds)"
    inner=$((ORACLE_START_DEADLINE + KILL_AFTER + RESIDUAL_KILL_DEADLINE + ORACLE_STOP_DEADLINE))
    if ((inner >= LEG_KILL_SLACK)); then
        log "FAIL: in-leg overhead ${inner}s does not fit LEG_KILL_SLACK=${LEG_KILL_SLACK}s"
        return 1
    fi
    if ((worst + BUDGET_HEADROOM > STEP_BUDGET)); then
        log "FAIL: worst case ${worst}s + ${BUDGET_HEADROOM}s headroom exceeds STEP_BUDGET=${STEP_BUDGET}s"
        return 1
    fi
    echo "budget worst_case=$worst step=$STEP_BUDGET headroom=$((STEP_BUDGET - worst))"
}

# Every SCREAMING_SNAKE_CASE string literal in the live sources: env var
# names are read either inline or through a named const, and both are string
# literals there. A literal that is not an env var name is planted harmlessly.
derive_planted_names() {
    grep -ohE '"[A-Z][A-Z0-9]*(_[A-Z0-9]+)+"' "${LIVE_SOURCES[@]}" | tr -d '"' | sort -u
}

planted_names() {
    local derived
    derived="$(derive_planted_names)"
    if [[ -z "$derived" ]]; then
        log "FAIL: no variable names derived from ${LIVE_SOURCES[*]}"
        return 1
    fi
    printf '%s\n' "$derived" "${FIXED_PLANTED_NAMES[@]}" | sort -u
}

require_netns=0
case "${1:-}" in
    "") ;;
    --require-netns) require_netns=1 ;;
    --self-check)
        check_budget || exit 1
        planted_names || exit 1
        exit 0
        ;;
    *)
        echo "usage: $0 [--require-netns | --self-check]" >&2
        exit 2
        ;;
esac

check_budget >/dev/null || exit 1

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
UNSHARE=(unshare --user --map-root-user --net --pid --mount --fork --kill-child --mount-proc)
"${UNSHARE[@]}" bash -c 'ip link set lo up && mount -t tmpfs tmpfs /run' 2>/dev/null ||
    skip_or_fail "cannot create unprivileged user + network + pid + mount namespaces"

cd "$REPO_ROOT" || exit 1

WORK="$(mktemp -d)"
RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
RUNNING_PID=""
CANARY_PID=""

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
    if [[ -n "$CANARY_PID" ]]; then
        kill -KILL "$CANARY_PID" 2>/dev/null
        wait "$CANARY_PID" 2>/dev/null
    fi
    [[ -n "${CANARY_SOCKET:-}" ]] && rm -f "$CANARY_SOCKET"
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
    timeout --kill-after="$KILL_AFTER" "$deadline" "$@" &
    RUNNING_PID=$!
    STEP_RC=0
    wait "$RUNNING_PID" || STEP_RC=$?
    RUNNING_PID=""
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
    names="$(planted_names)" || return 1
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

# Runs "$@" in fresh namespaces with the planted environment and a scratch
# HOME / XDG_CONFIG_HOME, under `deadline` seconds. The repo's
# .cargo/config.toml forces XDG_CONFIG_HOME for every process cargo runs;
# CARGO_ENV_XDG_CONFIG_HOME replaces that forced entry, so test binaries see
# the leg's scratch directory too. Sets LEG_LOG (the
# recorder log) and LEG_ERROR (empty when the leg is valid evidence, else
# why it is not) and LEG_RC (the command's exit status; meaningful only when
# LEG_ERROR is empty). The scratch assignments follow PLANTED, so they win
# over any same-named planted variable.
run_leg() {
    local name="$1" deadline="$2"
    shift 2
    LEG_LOG="$WORK/$name.log"
    LEG_STATUS="$WORK/$name.status"
    : >"$LEG_LOG"
    : >"$LEG_STATUS"
    mkdir -p "$WORK/$name.home" "$WORK/$name.xdg"
    local unset_args=() var
    for var in "${PROXY_VARS[@]}" "${HOST_SOCKET_VARS[@]}"; do unset_args+=(-u "$var"); done
    run_bounded $((deadline + LEG_KILL_SLACK)) \
        env "${unset_args[@]}" "${PLANTED[@]}" OUTER_UID="$(id -u)" OUTER_GID="$(id -g)" \
        HOME="$WORK/$name.home" XDG_CONFIG_HOME="$WORK/$name.xdg" \
        CARGO_ENV_XDG_CONFIG_HOME="$WORK/$name.xdg" \
        RUSTUP_HOME="$RUSTUP_HOME" CARGO_HOME="$CARGO_HOME" \
        CANARY_SOCKET="$CANARY_SOCKET" MASK_SOCKET_PATHS="$MASK_SOCKET_PATHS" \
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
        LEG_ERROR="namespace or recorder setup failed (${state:-no recorder state}$(sed -n 's/^unix-mask=/, unix mask: /p' "$LEG_STATUS"))"
    elif [[ "$inside_rc" -eq 92 ]]; then
        LEG_ERROR="host Unix socket tree visible inside the namespace ($(sed -n 's/^unix-mask=//p' "$LEG_STATUS"))"
    elif [[ "$inside_rc" -ne 0 || "$state" != "exited rc=0" ]]; then
        LEG_ERROR="recorder did not run for the whole leg and stop cleanly (${state:-no recorder state}, harness exit $inside_rc)"
    elif grep -q '^oracle-error' "$LEG_LOG"; then
        LEG_ERROR="recorder reported: $(grep -m1 '^oracle-error' "$LEG_LOG")"
    elif [[ "$(head -n1 "$LEG_LOG")" != "oracle-ready" || "$(tail -n1 "$LEG_LOG")" != "oracle-stopped" ]]; then
        LEG_ERROR="recorder log is missing its ready or stopped sentinel"
    elif [[ "$LEG_RC" -eq 124 || "$LEG_RC" -eq 137 ]]; then
        LEG_ERROR="the leg's command exceeded its deadline"
    fi
    local residual
    residual="$(sed -n 's/^residual_killed=//p' "$LEG_STATUS")"
    if [[ -z "$LEG_ERROR" && "${residual:-0}" -gt 0 ]]; then
        log "note: killed $residual leftover process(es) of the leg before stopping the recorder"
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

# The unix: socket path a host socket variable names, if any.
socket_path_of() {
    local var="$1" value="${!1:-}"
    case "$var" in
        SSH_AUTH_SOCK) printf '%s\n' "$value" ;;
        DOCKER_HOST) [[ "$value" == unix://* ]] && printf '%s\n' "${value#unix://}" ;;
        DBUS_*)
            local part
            IFS=';' read -ra parts <<<"$value"
            for part in "${parts[@]}"; do
                [[ "$part" == unix:*path=* ]] || continue
                part="${part#*path=}"
                printf '%s\n' "${part%%,*}"
            done
            ;;
    esac
}

# A directory under a masked tree this user can create a socket in.
canary_dir() {
    local dir
    for dir in "${XDG_RUNTIME_DIR:-}" /run/lock "/run/user/$(id -u)"; do
        [[ -n "$dir" && -d "$dir" && -w "$dir" ]] || continue
        case "$(readlink -f "$dir")/" in
            /run/*) echo "$dir"; return 0 ;;
        esac
    done
    return 1
}

start_canary() {
    local dir tenths=100
    dir="$(canary_dir)" || {
        log "FAIL: no writable directory under /run for the Unix-socket canary"
        return 1
    }
    CANARY_SOCKET="$dir/routectl-gate-canary-$$.sock"
    python3 -c '
import os, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.bind(sys.argv[1])
s.listen(8)
while True:
    conn, _ = s.accept()
    conn.close()
' "$CANARY_SOCKET" &
    CANARY_PID=$!
    while [[ ! -S "$CANARY_SOCKET" ]]; do
        if ((tenths-- <= 0)) || ! kill -0 "$CANARY_PID" 2>/dev/null; then
            log "FAIL: the Unix-socket canary did not start"
            return 1
        fi
        sleep 0.1
    done
}

# Socket paths the control leg must fail to reach: the canary, the well-known
# daemon sockets, the runtime-dir session bus, and whatever the host socket
# variables name. Sets MASK_SOCKET_PATHS (variable-named paths only) and
# UNIX_TARGETS, and writes their pre-isolation reachability to outside.unix.
collect_unix_targets() {
    local var path
    MASK_SOCKET_PATHS=""
    for var in "${HOST_SOCKET_VARS[@]}"; do
        while IFS= read -r path; do
            [[ -n "$path" ]] || continue
            MASK_SOCKET_PATHS+="$path"$'\n'
            UNIX_TARGETS+=("$path")
        done < <(socket_path_of "$var")
    done
    UNIX_TARGETS=("$CANARY_SOCKET" "${WELL_KNOWN_SOCKETS[@]}" "/run/user/$(id -u)/bus" "${UNIX_TARGETS[@]}")
    python3 -c "$UNIX_PROBE_LIB
probe_unix(sys.argv[1], sys.argv[2:])" "$WORK/outside.unix" "${UNIX_TARGETS[@]}"
}

build_planted_env || exit 1
UNIX_TARGETS=()
start_canary || exit 1
collect_unix_targets || exit 1
if [[ "$(head -n1 "$WORK/outside.unix")" != "connected $CANARY_SOCKET" ]]; then
    log "FAIL: the Unix-socket canary is not reachable before isolation: $(head -n1 "$WORK/outside.unix")"
    exit 1
fi

log "fetching dependencies outside the namespace"
run_bounded "$FETCH_DEADLINE" env -u CARGO_NET_OFFLINE cargo fetch --locked
if ((STEP_RC != 0)); then
    log "FAIL: cargo fetch failed or timed out (exit $STEP_RC)"
    exit 1
fi

CONTROL_PROBE="$UNIX_PROBE_LIB"'
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
with open(sys.argv[2], "w", encoding="ascii") as env_out:
    for var in sys.argv[3].split():
        env_out.write(var + ("=set" if var in os.environ else "=unset") + "\n")
probe_unix(sys.argv[4], sys.argv[5:])
'
CONTROL_EXPECTED=(
    '^dns qtype=[0-9]+ name=recorder-control\.example\.com$'
    '^tcp dest=203\.0\.113\.10:443$'
    '^tcp dest=198\.51\.100\.7:8443$'
    '^udp dest=192\.0\.2\.1:9999$'
)

# Inside the control leg: every host socket variable unset, and no probed
# socket reachable -- the canary above all, which was reachable outside.
judge_unix_isolation() {
    local leaked reached present
    leaked="$(grep -v '=unset$' "$WORK/control.env" 2>/dev/null)"
    if [[ ! -s "$WORK/control.env" || -n "$leaked" ]]; then
        fail "Unix-socket isolation: host socket variables visible inside: ${leaked:-<probe wrote nothing>}"
        return
    fi
    if [[ "$(wc -l <"$WORK/control.unix" 2>/dev/null)" -ne ${#UNIX_TARGETS[@]} ]]; then
        fail "Unix-socket isolation: probe covered $(wc -l <"$WORK/control.unix" 2>/dev/null) of ${#UNIX_TARGETS[@]} sockets"
        return
    fi
    reached="$(grep '^connected ' "$WORK/control.unix")"
    if [[ -n "$reached" ]]; then
        fail "Unix-socket isolation: reachable inside: $(tr '\n' ' ' <<<"$reached")"
        return
    fi
    present="$(awk '$1 != "ENOENT" {print $2}' "$WORK/outside.unix" | tr '\n' ' ')"
    echo "PASS: no host Unix socket reachable inside; present outside and blocked: $present"
}

log "leg 1: recorder control"
run_leg control "$CONTROL_DEADLINE" python3 -c "$CONTROL_PROBE" "$WORK/control.status.ipv6" \
    "$WORK/control.env" "${HOST_SOCKET_VARS[*]}" "$WORK/control.unix" "${UNIX_TARGETS[@]}"
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
    judge_unix_isolation
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
