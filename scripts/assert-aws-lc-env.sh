#!/usr/bin/env bash
# AWS-LC build-environment preflight: fail before any cargo invocation when
# the process environment carries an aws-lc-sys build override the workflow
# pin does not control.
#
# check-aws-lc-pin.sh proves the workflow FILES pin AWS_LC_SYS_USE_SYSTEM=0
# and AWS_LC_SYS_STATIC=1, but aws-lc-sys resolves each build variable as
# AWS_LC_SYS_<NAME>_<target> first and AWS_LC_SYS_<NAME> second, where
# <target> is the cargo TARGET lowercased with `-` mapped to `_` (e.g.
# AWS_LC_SYS_USE_SYSTEM_x86_64_unknown_linux_gnu). A suffixed value the
# runner exports therefore outranks the unsuffixed pin, and no workflow-file
# check can see it. This runs inside the job, so it sees what cargo will see.
#
# Rejected: every variable whose name, compared case-insensitively (Windows
# environment names are case-insensitive), starts with
#   AWS_LC_SYS_{USE_SYSTEM,STATIC,SYSTEM_DIR,SYSTEM_BINDINGS,NO_PREFIX}
# suffixed or not -- EXCEPT exactly AWS_LC_SYS_USE_SYSTEM=0 and exactly
# AWS_LC_SYS_STATIC=1, the two workflow pins, compared byte for byte. Unset
# pins pass: a missing pin is check-aws-lc-pin.sh's finding.
#
# Only variable NAMES are printed, never values.
#
# Usage: assert-aws-lc-env.sh
# Exit codes: 0 = environment clean, 1 = at least one finding.

set -euo pipefail

readonly FAMILY_RE='^AWS_LC_SYS_(USE_SYSTEM|STATIC|SYSTEM_DIR|SYSTEM_BINDINGS|NO_PREFIX)'

# Print the finding for one NAME=VALUE environment entry, or nothing.
entry_finding() {
    local entry="$1" name value upper
    name="${entry%%=*}"
    value="${entry#*=}"
    # tr rather than ${name^^}: macOS ships bash 3.2, which lacks it.
    upper="$(printf '%s' "$name" | tr '[:lower:]' '[:upper:]')"
    [[ "$upper" =~ $FAMILY_RE ]] || return 0
    if [[ "$name" == AWS_LC_SYS_USE_SYSTEM ]]; then
        [[ "$value" == 0 ]] || echo "$name: value is not exactly \"0\""
    elif [[ "$name" == AWS_LC_SYS_STATIC ]]; then
        [[ "$value" == 1 ]] || echo "$name: value is not exactly \"1\""
    else
        echo "$name: AWS-LC build override outside the two workflow pins"
    fi
}

main() {
    local entry finding entries=0 failed=0
    # `env -0` rather than `compgen -e` or plain `env`: the shell drops names
    # it cannot represent as a variable (a suffix carrying a `.` or `-`),
    # exactly the injected forms this must catch, and newline framing would
    # let a value forge or hide an entry boundary. Every runner this guards
    # has it -- GNU coreutils on Linux and in Git Bash on Windows, and the
    # macOS env since shell_cmds-240 -- so there is no fallback: a failing
    # or empty `env -0` stops the script rather than reading as clean.
    ENV_DUMP="$(mktemp)"
    trap 'rm -f "$ENV_DUMP"' EXIT
    if ! env -0 >"$ENV_DUMP"; then
        echo "aws-lc-env: FAIL: env -0: command failed; cannot vouch for the build environment" >&2
        return 1
    fi
    while IFS= read -r -d '' entry; do
        entries=$((entries + 1))
        finding="$(entry_finding "$entry")"
        if [[ -n "$finding" ]]; then
            echo "aws-lc-env: FAIL: $finding" >&2
            failed=1
        fi
    done <"$ENV_DUMP"
    if [[ "$entries" -eq 0 ]]; then
        echo "aws-lc-env: FAIL: env -0: produced no entries; cannot vouch for the build environment" >&2
        return 1
    fi
    if [[ "$failed" -ne 0 ]]; then
        echo "aws-lc-env: unset the variables above; aws-lc-sys must build its vendored AWS-LC as a static library" >&2
        return 1
    fi
    echo "aws-lc-env: PASS"
}

main "$@"
