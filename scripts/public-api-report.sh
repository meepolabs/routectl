#!/usr/bin/env bash
# Informational public-API report. Runs `public-api.sh --check all` (the copy
# beside this script), lets its output through unchanged, then classifies the
# outcome as one GitHub Actions annotation line on stdout and, when
# GITHUB_STEP_SUMMARY is set, one line appended to the job summary:
#
#   clean          ::notice   public-api.sh exited 0
#   drift          ::warning  exit 1 with at least one `surface drift for
#                             <crate>` or `missing baseline for <crate>`
#                             line; names every such crate once, then every
#                             crate whose surface could not be listed or
#                             carried a machine-specific path, as
#                             "; could not check <crates>"
#   could not run  ::warning  cargo-public-api not on PATH, or not the
#                             version PUBLIC_API_TOOL_VERSION pins when that
#                             is set (public-api.sh is then never run), or
#                             any other outcome; carries public-api.sh's
#                             last stderr line as the reason
#
# It always exits 0: public-API drift is reported, never enforced. Any
# argument is a wiring defect, not a report, and exits 2.
#
# Usage: [PUBLIC_API_TOOL_VERSION=<x.y.z>] public-api-report.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PUBLIC_API="$SCRIPT_DIR/public-api.sh"
TITLE=public-api

usage() {
    echo "usage: $0 (takes no arguments)" >&2
    exit 2
}

# Encodes an annotation message the way workflow commands require.
escape_message() {
    local s="$1"
    s="${s//%/%25}"
    s="${s//$'\r'/%0D}"
    s="${s//$'\n'/%0A}"
    printf '%s' "$s"
}

# Prints the annotation of level $1 carrying message $2 and appends the
# message to the job summary.
report() {
    local level="$1" message="$2"
    printf '::%s title=%s::%s\n' "$level" "$TITLE" "$(escape_message "$message")"
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        if ! printf '%s: %s\n' "$TITLE" "$message" >>"$GITHUB_STEP_SUMMARY"; then
            echo "public-api-report: could not append to the job summary" >&2
        fi
    fi
}

# Runs the check with its stdout and stderr passing through, copying its
# stderr to $1. Returns the check's exit code.
run_check() {
    local errlog="$1" rc
    {
        bash "$PUBLIC_API" --check all 2>&1 1>&3 | tee "$errlog" >&2
        rc="${PIPESTATUS[0]}"
    } 3>&1
    return "$rc"
}

# Prints the distinct lines of stdin in first-seen order, comma-separated.
join_unique() {
    awk '!seen[$0]++ { printf "%s%s", sep, $0; sep = ", " }'
}

# Prints the drifted crates named in $1, first-seen order, comma-separated.
drifted_crates() {
    sed -nE 's/^public-api: (surface drift|missing baseline) for ([A-Za-z0-9_.-]+).*/\2/p' "$1" | join_unique
}

# Prints the crates in $1 whose surface could not be listed or carried a
# machine-specific path, first-seen order, comma-separated.
unchecked_crates() {
    sed -nE \
        -e 's/^public-api: failed to list surface for ([A-Za-z0-9_.-]+)$/\1/p' \
        -e 's/^public-api: machine-specific path in ([A-Za-z0-9_.-]+) surface.*/\1/p' \
        "$1" | join_unique
}

# Prints why the installed cargo-public-api cannot be used, or nothing when
# it is on PATH and matches PUBLIC_API_TOOL_VERSION (when that is set).
tool_problem() {
    local want="${PUBLIC_API_TOOL_VERSION:-}" got
    if ! command -v cargo-public-api >/dev/null 2>&1; then
        printf 'cargo-public-api not installed'
        return
    fi
    [[ -n "$want" ]] || return 0
    got="$(cargo-public-api --version 2>/dev/null | sed -nE '1s/^cargo-public-api ([^ ]+).*/\1/p')"
    if [[ "$got" != "$want" ]]; then
        printf 'cargo-public-api %s, want %s' "${got:-of unknown version}" "$want"
    fi
}

# Prints why the check could not run: its last non-blank stderr line.
failure_reason() {
    local rc="$1" errlog="$2" line
    line="$(grep -v '^[[:space:]]*$' "$errlog" | tail -n 1)"
    printf '%s' "${line:-public-api.sh exited $rc with no error output}"
}

classify() {
    local rc="$1" errlog="$2" crates unchecked
    if [[ "$rc" -eq 0 ]]; then
        report notice "public API baselines match"
        return
    fi
    crates="$(drifted_crates "$errlog")"
    if [[ "$rc" -eq 1 && -n "$crates" ]]; then
        unchecked="$(unchecked_crates "$errlog")"
        report warning "drift in $crates${unchecked:+; could not check $unchecked}"
        return
    fi
    report warning "could not run ($(failure_reason "$rc" "$errlog"))"
}

[[ $# -eq 0 ]] || usage

PROBLEM="$(tool_problem)"
if [[ -n "$PROBLEM" ]]; then
    report warning "could not run ($PROBLEM)"
    exit 0
fi

ERRLOG="$(mktemp)"
trap 'rm -f "$ERRLOG"' EXIT
run_check "$ERRLOG"
rc=$?
classify "$rc" "$ERRLOG"
exit 0
