#!/usr/bin/env bash
# Self-test for credential_probe.sh. Exits 0 when all assertions pass,
# non-zero on the first failure.
#
# Unlike the other self-tests under this directory, this one runs the
# REAL `routectl` binary rather than a stub: the behavior under test IS
# the real secret-resolution code path (`config check` against an
# `env://` ref), so a stub that only fakes a health check would prove
# nothing. It still never makes a network call and never touches a real
# credential -- `config check` resolves an `env://` ref by reading the
# named env var, nothing more, so every value used here is synthetic.
#
# This is also where the credential-resolution probe required for the
# `openai-responses` and `openai-compat` lane configs is RECORDED: this
# file running clean is the evidence, re-verified on every run rather
# than captured as a point-in-time note that would drift the moment
# either config changed.
#
# Requires a built `routectl` binary. Set ROUTECTL_BIN to point at one,
# or have it on PATH, or have already built it under the repo's target
# dir (respecting CARGO_TARGET_DIR if exported) -- this self-test does
# not build one itself.
#
# Run it from anywhere:
#   bash scripts/drivers/config/credential_probe.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROBE="$HERE/credential_probe.sh"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"

fails=0

# mktemp rather than a PID-derived name: a predictable path under a shared
# /tmp is pre-placeable by another local user, and the cleanup trap fires
# on the error paths too.
OUT="$(mktemp)"
trap 'rm -f "$OUT"' EXIT

check_exit0() {
    local label="$1"
    shift
    if "$@" >"$OUT" 2>&1; then
        echo "PASS: $label"
    else
        echo "FAIL: $label"
        sed -n '1,40p' "$OUT"
        fails=$((fails + 1))
    fi
}

check_nonzero() {
    local label="$1"
    shift
    if "$@" >"$OUT" 2>&1; then
        echo "FAIL: $label -- expected a non-zero exit, got 0"
        sed -n '1,40p' "$OUT"
        fails=$((fails + 1))
    else
        echo "PASS: $label"
    fi
}

# The same binary resolution the probe itself uses, sourced from its one
# owner: a self-test resolving differently from the script under test
# would verify a binary the probe never runs.
RESOLVE_BIN_LIB="$REPO_ROOT/scripts/drivers/lib/resolve_bin.sh"
[ -r "$RESOLVE_BIN_LIB" ] || {
    echo "FAIL: binary-resolution library not found at $RESOLVE_BIN_LIB"
    exit 1
}
# shellcheck source=scripts/drivers/lib/resolve_bin.sh
. "$RESOLVE_BIN_LIB"

BIN="$(resolve_bin)" || {
    echo "FAIL: no routectl binary found -- set ROUTECTL_BIN or build one (cargo build --bin routectl) before running this self-test"
    exit 1
}
export ROUTECTL_BIN="$BIN"

# Run against the committed config files themselves, not against copies:
# an edit that breaks credential resolution on a real lane is caught here
# rather than at the first paid capture on it.
check_exit0 "openai-responses.toml resolves ROUTECTL_DRIVER_OPENAI_API_KEY and ROUTECTL_DRIVER_OPENAI_ACCOUNT_ID (with the unset control)" \
    bash "$PROBE" "$HERE/openai-responses.toml" ROUTECTL_DRIVER_OPENAI_API_KEY ROUTECTL_DRIVER_OPENAI_ACCOUNT_ID

check_exit0 "openai-compat.toml resolves ROUTECTL_DRIVER_OPENAI_API_KEY (with the unset control)" \
    bash "$PROBE" "$HERE/openai-compat.toml" ROUTECTL_DRIVER_OPENAI_API_KEY

# The probe's own failure mode: a var name that never appears in the
# config's secret_uris() cannot possibly warn when unset, so the control
# must catch it. This is what proves the two checks above pass because
# the credential resolves, not because the probe is unable to fail.
check_nonzero "probe fails when asked to verify a var the config never references" \
    bash "$PROBE" "$HERE/openai-compat.toml" ROUTECTL_DRIVER_NONEXISTENT_VAR

# A config path that does not exist is a usage error, not a resolution
# result -- distinct exit path, worth pinning separately.
check_nonzero "probe fails on a missing config file" \
    bash "$PROBE" "$HERE/does-not-exist.toml" ROUTECTL_DRIVER_OPENAI_API_KEY

echo
if [ "$fails" -eq 0 ]; then
    echo "credential_probe.test.sh: all checks passed"
    exit 0
fi
echo "credential_probe.test.sh: $fails check(s) failed"
exit 1
