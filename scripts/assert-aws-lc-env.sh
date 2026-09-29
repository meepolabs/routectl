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
# Rejected, matching names case-insensitively (Windows environment names are
# case-insensitive):
#   - AWS_LC_SYS_{USE_SYSTEM,STATIC,SYSTEM_DIR,SYSTEM_BINDINGS,NO_PREFIX}
#     followed by any suffix at all
#   - AWS_LC_SYS_SYSTEM_DIR, which points the build at a system AWS-LC
#   - AWS_LC_SYS_USE_SYSTEM set to anything but exactly "0"
#   - AWS_LC_SYS_STATIC set to anything but exactly "1"
#   - any other spelling of those three names than the exact upper-case one
# Unset USE_SYSTEM / STATIC pass: the workflows pin them, and a missing pin is
# check-aws-lc-pin.sh's finding.
#
# Only variable NAMES are printed, never values.
#
# Usage: assert-aws-lc-env.sh
# Exit codes: 0 = environment clean, 1 = at least one finding.

set -euo pipefail

readonly SUFFIXED_RE='^AWS_LC_SYS_(USE_SYSTEM|STATIC|SYSTEM_DIR|SYSTEM_BINDINGS|NO_PREFIX)_.'
readonly UNSUFFIXED_RE='^AWS_LC_SYS_(USE_SYSTEM|STATIC|SYSTEM_DIR)$'

# Print the finding for one NAME=VALUE environment entry, or nothing.
# Case folding uses nocasematch rather than ${var^^}, which the bash 3.2 some
# runners put first on PATH lacks.
entry_finding() {
    local entry="$1" name value
    name="${entry%%=*}"
    value="${entry#*=}"
    shopt -s nocasematch
    if [[ "$name" =~ $SUFFIXED_RE ]]; then
        echo "$name: target-suffixed override outranks the workflow pin"
        return 0
    elif [[ ! "$name" =~ $UNSUFFIXED_RE ]]; then
        return 0
    fi
    shopt -u nocasematch
    case "$name" in
    AWS_LC_SYS_SYSTEM_DIR)
        echo "$name: selects a system AWS-LC" ;;
    AWS_LC_SYS_USE_SYSTEM)
        [[ "$value" == 0 ]] || echo "$name: value is not exactly \"0\"" ;;
    AWS_LC_SYS_STATIC)
        [[ "$value" == 1 ]] || echo "$name: value is not exactly \"1\"" ;;
    *)
        echo "$name: non-canonical spelling of an AWS-LC build variable" ;;
    esac
}

main() {
    local entry finding entries=0 failed=0
    # `env` rather than `compgen -e`: the shell drops names it cannot
    # represent as a variable (a suffix carrying a `.` or `-`), and those are
    # exactly the injected forms this must catch. Newline framing, not
    # `env -0`, because not every runner's `env` has -0; every real entry
    # still starts a line, so a multi-line value can only add a spurious
    # finding, never hide one. The dump goes through a file so a failing
    # `env` stops the script instead of reading as a clean environment.
    ENV_DUMP="$(mktemp)"
    trap 'rm -f "$ENV_DUMP"' EXIT
    env >"$ENV_DUMP"
    while IFS= read -r entry || [[ -n "$entry" ]]; do
        entries=$((entries + 1))
        finding="$(entry_finding "$entry")"
        if [[ -n "$finding" ]]; then
            echo "aws-lc-env: FAIL: $finding" >&2
            failed=1
        fi
    done <"$ENV_DUMP"
    if [[ "$entries" -eq 0 ]]; then
        echo "aws-lc-env: FAIL: read no environment entries; cannot vouch for the build environment" >&2
        return 1
    fi
    if [[ "$failed" -ne 0 ]]; then
        echo "aws-lc-env: unset the variables above; aws-lc-sys must build its vendored AWS-LC as a static library" >&2
        return 1
    fi
    echo "aws-lc-env: PASS"
}

main "$@"
