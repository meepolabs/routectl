#!/usr/bin/env bash
# Non-paid credential-resolution probe for a lane config.
#
# `routectl config check` exiting 0 is not evidence a lane can serve: an
# `api_key_ref` naming an unset env var is a WARNING there, not an error,
# so a misconfigured or unresolvable credential still checks clean and
# only fails later, as a 401 at the upstream. This probe closes that gap
# for the refs a lane actually declares, hermetically and with no real
# credential: it runs the real `routectl` binary's `config check` against
# the config once with synthetic (non-secret) values for the named env
# vars, and once with them unset, and requires the resolution warning to
# be ABSENT in the first run and PRESENT in the second for every var
# named. The second run is the paired control -- without it, a probe that
# always reports "resolves" would pass for the wrong reason (a check that
# can't fail proves nothing).
#
# No network call happens on either run: secret resolution for an
# `env://` ref is a local env-var read, not an upstream request, so this
# never touches a paid endpoint and never needs a real key.
#
# Usage:
#   credential_probe.sh CONFIG_FILE VAR [VAR...]
#
# CONFIG_FILE is the lane config to check. Each VAR is an env var name
# the config's `secret_uris()` should resolve through (its `api_key_ref`,
# `account_id_ref`, etc, minus the `env://` scheme prefix). Exits 0 only
# if the config resolves cleanly with all VARs set to synthetic values
# AND warns on every one of them when unset.
#
# `ROUTECTL_BIN` overrides the binary under test (default: `routectl` on
# PATH, falling back to a debug/release build under the repo's target
# dir if neither is found).

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"

die() {
    echo "credential_probe.sh: $*" >&2
    exit 2
}

[ "$#" -ge 2 ] || die "usage: credential_probe.sh CONFIG_FILE VAR [VAR...]"

CONFIG_FILE="$1"
shift
VARS=("$@")

[ -f "$CONFIG_FILE" ] || die "config file not found: $CONFIG_FILE"

# The single owner of binary resolution, shared with this script's
# self-test. Absent library is a hard failure rather than a silently
# skipped probe: a probe that cannot find a binary proves nothing about
# whether a lane resolves its credential.
RESOLVE_BIN_LIB="$REPO_ROOT/scripts/drivers/lib/resolve_bin.sh"
[ -r "$RESOLVE_BIN_LIB" ] ||
    die "binary-resolution library not found at $RESOLVE_BIN_LIB"
# shellcheck source=scripts/drivers/lib/resolve_bin.sh
. "$RESOLVE_BIN_LIB"

BIN="$(resolve_bin)" || die "no routectl binary found; set ROUTECTL_BIN or build one (cargo build --bin routectl)"

# Runs `config check` against CONFIG_FILE in a throwaway XDG_CONFIG_HOME
# so no real daemon state (usage db, catalog cache) is ever touched.
# Prints combined stdout+stderr (the resolution warning goes to stderr
# via tracing, the check report to stdout) and returns its exit code.
# Exported so it runs inside the `env ... bash -c` subshells below, which
# need their own process to control exactly which vars are set/unset.
# shellcheck disable=SC2329
run_check() {
    local bin="$1" config="$2" xdg
    xdg="$(mktemp -d)"
    XDG_CONFIG_HOME="$xdg" "$bin" config check --config "$config" 2>&1
    local status=$?
    rm -rf "$xdg"
    return "$status"
}
export -f run_check

fails=0

# Leg 1: every named var set to a synthetic (non-secret) value. The
# config must resolve cleanly -- exit 0 and no resolution warning for
# any of the vars under test.
env_args=()
for v in "${VARS[@]}"; do
    env_args+=("$v=synthetic-probe-value")
done
# shellcheck disable=SC2016
resolved_output="$(env "${env_args[@]}" bash -c 'run_check "$1" "$2"' _ "$BIN" "$CONFIG_FILE")"
resolved_status=$?

if [ "$resolved_status" -ne 0 ]; then
    echo "FAIL: $CONFIG_FILE -- config check exited $resolved_status with credentials set"
    echo "$resolved_output"
    fails=$((fails + 1))
else
    echo "PASS: $CONFIG_FILE -- config check exits 0 with credentials set"
fi

for v in "${VARS[@]}"; do
    if printf '%s' "$resolved_output" | grep -qF "cannot resolve secret-ref" \
        && printf '%s' "$resolved_output" | grep -qF "$v"; then
        echo "FAIL: $CONFIG_FILE -- $v still unresolved with a synthetic value set"
        fails=$((fails + 1))
    else
        echo "PASS: $CONFIG_FILE -- $v resolves with a synthetic value set"
    fi
done

# Leg 2 (the paired control): every named var explicitly unset. The
# config must warn, by name, for every one of them -- proving leg 1
# passed because the credential resolved, not because the probe can't
# detect an unresolved one.
unset_args=()
for v in "${VARS[@]}"; do
    unset_args+=("-u" "$v")
done
# shellcheck disable=SC2016
unresolved_output="$(env "${unset_args[@]}" bash -c 'run_check "$1" "$2"' _ "$BIN" "$CONFIG_FILE")"

for v in "${VARS[@]}"; do
    if printf '%s' "$unresolved_output" | grep -qF "cannot resolve secret-ref" \
        && printf '%s' "$unresolved_output" | grep -qF "$v"; then
        echo "PASS: $CONFIG_FILE -- $v warns when unset (control)"
    else
        echo "FAIL: $CONFIG_FILE -- $v did not warn when unset (control did not fire)"
        fails=$((fails + 1))
    fi
done

if [ "$fails" -ne 0 ]; then
    echo "credential_probe.sh: $fails check(s) failed for $CONFIG_FILE" >&2
    exit 1
fi

echo "credential_probe.sh: $CONFIG_FILE resolves cleanly and the unset control fires for: ${VARS[*]}"
exit 0
