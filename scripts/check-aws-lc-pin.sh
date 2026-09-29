#!/usr/bin/env bash
# AWS-LC build-mode pin check: every workflow that builds the workspace must
# make aws-lc-sys compile its vendored AWS-LC source into a static library,
# whatever the runner image or an earlier step leaves in the environment.
#
# aws-lc-sys reads two build-script variables, each in a target-suffixed form
# (e.g. AWS_LC_SYS_STATIC_x86_64_unknown_linux_gnu) checked before the plain
# one:
#   AWS_LC_SYS_USE_SYSTEM  unset -> adopt an AWS-LC found via OPENSSL_DIR or
#                          pkg-config instead of the vendored source
#   AWS_LC_SYS_STATIC      "0"   -> emit a dynamic library
#
# So each workflow must pin both in its single top-level `env:` mapping,
# where every job inherits them, and must not set either anywhere else: a
# job- or step-level `env:`, a `$GITHUB_ENV` write, or a duplicate key can
# each override the pin for part of the build. Any mention of either name
# outside the two pin lines fails unless it sits on a whole-line comment --
# deliberately broader than "an override", since without a YAML parser the
# check cannot tell a harmless mention from one.
#
# Usage: check-aws-lc-pin.sh [WORKFLOW_FILE...]
#   With no arguments, checks .github/workflows/ci.yml and release.yml.
#
# Exit codes: 0 = every file pinned, 1 = at least one finding.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

readonly USE_SYSTEM_PIN='AWS_LC_SYS_USE_SYSTEM: "0"'
readonly STATIC_PIN='AWS_LC_SYS_STATIC: "1"'

# Print one finding per line for workflow $1; print nothing when it is pinned.
workflow_findings() {
    awk -v use_pin="$USE_SYSTEM_PIN" -v static_pin="$STATIC_PIN" '
        function rtrim(s) { sub(/[[:space:]]+$/, "", s); return s }
        function without_comment(s) { sub(/[[:space:]]+#.*$/, "", s); return rtrim(s) }
        /^[[:space:]]*#/ || /^[[:space:]]*$/ { next }
        /^env:/ {
            env_count++
            if (env_count == 1) {
                in_env = 1
                if (without_comment($0) != "env:") {
                    print "line " NR ": top-level env: is not a block mapping"
                }
            } else {
                print "line " NR ": duplicate top-level env: mapping"
            }
            next
        }
        in_env && /^[^[:space:]]/ { in_env = 0 }
        in_env && child == "" {
            match($0, /^[[:space:]]*/)
            child = substr($0, 1, RLENGTH)
        }
        in_env && without_comment($0) == child use_pin {
            if (++use_count > 1) print "line " NR ": duplicate AWS_LC_SYS_USE_SYSTEM key"
            next
        }
        in_env && without_comment($0) == child static_pin {
            if (++static_count > 1) print "line " NR ": duplicate AWS_LC_SYS_STATIC key"
            next
        }
        /AWS_LC_SYS_(USE_SYSTEM|STATIC)/ {
            print "line " NR ": AWS-LC build variable set outside the top-level pin: " $0
        }
        END {
            if (env_count == 0) print "no top-level env: mapping"
            if (use_count == 0) print "top-level env: lacks " use_pin
            if (static_count == 0) print "top-level env: lacks " static_pin
        }
    ' "$1"
}

main() {
    local files=("$@") file findings failed=0
    if [[ ${#files[@]} -eq 0 ]]; then
        files=("$REPO_ROOT/.github/workflows/ci.yml" "$REPO_ROOT/.github/workflows/release.yml")
    fi
    for file in "${files[@]}"; do
        if [[ ! -f "$file" ]]; then
            echo "aws-lc-pin: FAIL: $file: workflow file not found" >&2
            failed=1
            continue
        fi
        findings="$(workflow_findings "$file")"
        if [[ -n "$findings" ]]; then
            while IFS= read -r finding; do
                echo "aws-lc-pin: FAIL: $file: $finding" >&2
            done <<<"$findings"
            failed=1
        fi
    done
    if [[ "$failed" -ne 0 ]]; then
        echo "aws-lc-pin: expected '$USE_SYSTEM_PIN' and '$STATIC_PIN' once each in the top-level env:, and nowhere else" >&2
        return 1
    fi
    echo "aws-lc-pin: PASS"
}

main "$@"
