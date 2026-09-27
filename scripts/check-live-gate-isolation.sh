#!/usr/bin/env bash
# Hostile-environment check for the standard test gate.
#
# Plants a synthetic value in every environment variable the live provider
# tests read, then runs the standard all-features workspace test gate inside
# a private network namespace and asserts it makes ZERO network attempts.
#
# The namespace has only a loopback interface, and every IPv4 / IPv6
# destination is routed to it, so no packet can leave the machine. The
# recorder (net-oracle.py) listens on the resolver address(es) and on TCP
# 80 / 443 for every destination, and logs each DNS query and each TCP
# connection. Connections to loopback destinations are not network
# attempts and are not counted.
#
# Three legs run, in this order:
#
#   1. recorder control   -- a DNS lookup plus a connect to a public name;
#                            the recorder MUST log both, or the check could
#                            not see an attempt at all.
#   2. standard gate      -- `cargo test --workspace --all-features`; MUST
#                            pass and log nothing.
#   3. live control       -- the explicit live-integration command under the
#                            same planted variables; MUST log at least one
#                            attempt, proving the planted values are enough
#                            to make the live tests dial out. Its test
#                            verdicts are ignored: every live test fails
#                            against the recorder by design.
#
# Build and run happen inside the namespace (`--offline`), so build scripts
# are covered too. Both cargo legs pass `--no-fail-fast`: a failing binary
# must not stop the run before a later binary has had its chance to dial. Dependencies are fetched first, outside the namespace,
# with no planted variable in the environment. Proxy variables are cleared
# for the namespaced legs: a proxy on a port the recorder does not bind
# would turn an attempt into an unlogged refusal.
#
# Needs util-linux `unshare` with unprivileged user namespaces, iproute2
# `ip`, and python3. Where the namespace cannot be created the check SKIPS
# BY NAME and exits 0, unless --require-netns is given (CI passes it), in
# which case the missing namespace is a failure.
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
STANDARD_GATE=(cargo test --workspace --all-features --offline --no-fail-fast)
LIVE_COMMAND=(cargo test -p routectl-cli --features live-integration --offline --no-fail-fast
    --test live_matrix --test live_anthropic_oauth -- --test-threads=1)
PROXY_VARS=(HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY http_proxy https_proxy all_proxy no_proxy)

log() { echo "check-live-gate-isolation: $*" >&2; }

# ---------------------------------------------------------------------------
# Inside the namespace: bring up the catch-all loopback, start the recorder,
# run one leg, stop the recorder. Invoked by the outer half only.
# ---------------------------------------------------------------------------
inside() {
    local log_path="$1" ready="$2"
    shift 2
    ip link set lo up || return 90
    ip route add local 0.0.0.0/0 dev lo || return 90
    # IPv6 may be disabled in the namespace; then it has no route at all.
    ip -6 route add local ::/0 dev lo 2>/dev/null || true

    python3 "$ORACLE" "$log_path" "$ready" &
    local oracle_pid=$! waited=0
    while [[ ! -e "$ready" ]]; do
        if ! kill -0 "$oracle_pid" 2>/dev/null || ((waited >= 100)); then
            log "recorder did not start"
            return 90
        fi
        sleep 0.1
        waited=$((waited + 1))
    done

    # The leg runs as the invoking user again, in a nested user namespace:
    # as namespace root, permission-denied tests would pass their writes.
    local rc=0
    python3 "$ORACLE" --as-user "$OUTER_UID" "$OUTER_GID" -- "$@" || rc=$?
    kill "$oracle_pid" 2>/dev/null
    wait "$oracle_pid" 2>/dev/null
    return "$rc"
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

for tool in unshare ip python3 cargo; do
    command -v "$tool" >/dev/null 2>&1 || skip_or_fail "required tool '$tool' not found"
done
unshare --user --map-root-user --net ip link set lo up 2>/dev/null ||
    skip_or_fail "cannot create an unprivileged network namespace"

cd "$REPO_ROOT" || exit 1

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

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

# Runs "$@" in a fresh namespace with the planted environment. Sets
# LEG_RC and LEG_LOG (the recorder log path).
run_leg() {
    local name="$1"
    shift
    LEG_LOG="$WORK/$name.log"
    : >"$LEG_LOG"
    local unset_args=() var
    for var in "${PROXY_VARS[@]}"; do unset_args+=(-u "$var"); done
    LEG_RC=0
    env "${unset_args[@]}" "${PLANTED[@]}" OUTER_UID="$(id -u)" OUTER_GID="$(id -g)" \
        unshare --user --map-root-user --net \
        bash "$SELF" --inside "$LEG_LOG" "$WORK/$name.ready" "$@" || LEG_RC=$?
    rm -f "$WORK/$name.ready"
}

# Recorder lines that are network attempts: every DNS query, and every TCP
# connection whose destination is not loopback.
attempts() {
    grep -vE '^tcp dest=(127\.[0-9.]+|::1|::ffff:127\.[0-9.]+):' "$1" || true
}

fails=0
fail() {
    log "FAIL: $*"
    fails=$((fails + 1))
}

build_planted_env || exit 1

log "fetching dependencies outside the namespace"
env -u CARGO_NET_OFFLINE cargo fetch --locked >&2 || {
    log "FAIL: cargo fetch failed"
    exit 1
}

log "leg 1/3: recorder control"
run_leg control python3 -c '
import socket
try:
    socket.create_connection(("recorder-control.example.com", 443), timeout=5).close()
except OSError:
    pass
'
control="$(attempts "$LEG_LOG")"
if [[ "$LEG_RC" -eq 90 ]]; then
    fail "recorder control: namespace setup failed"
elif ! grep -q '^dns ' <<<"$control" || ! grep -q '^tcp ' <<<"$control"; then
    fail "recorder control: expected a DNS query and a TCP connection to be recorded, got: ${control:-<nothing>}"
else
    echo "PASS: recorder sees a DNS query and a TCP connection"
fi

log "leg 2/3: standard gate: ${STANDARD_GATE[*]}"
run_leg standard "${STANDARD_GATE[@]}"
standard="$(attempts "$LEG_LOG")"
if [[ -n "$standard" ]]; then
    fail "standard gate made $(wc -l <<<"$standard") network attempt(s):"
    sort <<<"$standard" | uniq -c | sort -rn | head -40 >&2
elif [[ "$LEG_RC" -ne 0 ]]; then
    fail "standard gate exited $LEG_RC under the planted environment; zero attempts from a failed run proves nothing"
else
    echo "PASS: standard gate passed with zero network attempts"
fi

log "leg 3/3: live control: ${LIVE_COMMAND[*]}"
run_leg live "${LIVE_COMMAND[@]}"
live="$(attempts "$LEG_LOG")"
if [[ "$LEG_RC" -eq 90 ]]; then
    fail "live control: namespace setup failed"
elif [[ -z "$live" ]]; then
    fail "live control: the explicit live command made no network attempt under the planted variables, so the standard-gate leg is not evidence (cargo exit $LEG_RC)"
else
    echo "PASS: explicit live command dials out under the same variables ($(wc -l <<<"$live") attempts recorded)"
fi

if ((fails)); then
    log "$fails leg(s) failed"
    exit 1
fi
echo "check-live-gate-isolation: all legs passed"
