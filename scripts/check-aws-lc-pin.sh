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
# where every job inherits them, and must not set any variable of the five
# override families (USE_SYSTEM, STATIC, SYSTEM_DIR, SYSTEM_BINDINGS,
# NO_PREFIX; suffixed or not, any letter case) anywhere else: a job- or
# step-level `env:`, a `$GITHUB_ENV` write, or a duplicate key can each
# override the pin for part of the build. Any mention outside the two pin
# lines fails unless it sits on a whole-line comment -- deliberately broader
# than "an override", since without a YAML parser the check cannot tell a
# harmless mention from one.
#
# A pin in the file cannot outrank what the runner itself exports, so
# assert-aws-lc-env.sh rejects such an environment at run time, and this
# check requires every job with an actions/checkout step to run that
# preflight as the very next step -- `run: <preflight>`, or a `run: |` block
# whose first line is the preflight. A job without a checkout cannot run the
# preflight, so one that runs cargo or sets up a Rust toolchain fails too.
# The jobs in EXEMPT_JOBS below skip the preflight; each must still exist
# and must not run cargo.
#
# The check is lexical, so it is sound only over YAML written the plain way.
# By design it therefore rejects, outside comments, quoted strings, and block
# scalar bodies (where shell text such as `rm -f ./*.sha256` lives), every
# construct that could hide a key or a step from it: anchors (`&name`),
# aliases (`*name`), and merge keys (`<<:`) anywhere in the file, and under
# `jobs:` a bare `-` sequence item, a flow-style (`- {` / `- [`) item, and a
# quoted key.
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
readonly ENV_PREFLIGHT='bash scripts/assert-aws-lc-env.sh'

# <workflow file name>:<job id> -- why the job needs no preflight.
readonly EXEMPT_JOBS=(
    "ci.yml:osv-scan -- runs only the pinned osv-scanner binary over lockfiles"
    "release.yml:release -- downloads, signs, and publishes built artifacts"
)

# Print one finding per line for workflow $1; print nothing when it is pinned.
workflow_findings() {
    local exempt
    exempt="$(printf '%s\n' "${EXEMPT_JOBS[@]}" | sed 's/ -- .*//')"
    awk -v use_pin="$USE_SYSTEM_PIN" -v static_pin="$STATIC_PIN" \
        -v preflight="$ENV_PREFLIGHT" -v exempt="$exempt" -v wf="${1##*/}" '
        function rtrim(s) { sub(/[[:space:]]+$/, "", s); return s }
        function trim(s) { sub(/^[[:space:]]+/, "", s); return rtrim(s) }
        function without_comment(s) { sub(/[[:space:]]+#.*$/, "", s); return rtrim(s) }
        function runs_cargo(s) {
            if (s ~ /^[[:space:]]*(-[[:space:]]+)?name:/) return 0
            return s ~ /(^|[^A-Za-z0-9_.\/-])cargo([[:space:]]|$)/ || s ~ /uses:[[:space:]]*dtolnay\/rust-toolchain@/
        }
        function close_job() {
            if (job == "") return
            if (job in exempt_job) {
                exempt_seen[job] = 1
                if (cargo_line) print "line " cargo_line ": exempt job " job " runs cargo"
            } else if (checkout_step && !preflight_ok) {
                print "line " checkout_line ": job " job " does not run the preflight as the step right after checkout: " preflight
            } else if (!checkout_step && cargo_line) {
                print "line " cargo_line ": job " job " runs cargo without a checkout, so it cannot run the preflight: " preflight
            }
            job = ""; step_indent = ""; step = 0; checkout_step = 0; checkout_line = 0
            preflight_ok = 0; cargo_line = 0; block_step = 0
        }
        BEGIN {
            block_indent = -1
            n = split(exempt, entries, "\n")
            for (i = 1; i <= n; i++) {
                split(entries[i], parts, ":")
                if (parts[1] == wf) exempt_job[parts[2]] = 1
            }
        }
        function indent_of(s) { match(s, /^ */); return RLENGTH }
        function unquoted(s) {
            gsub(/"([^"\\]|\\.)*"/, "\"\"", s)
            gsub(/\047[^\047]*\047/, "\047\047", s)
            return s
        }
        # Flag YAML constructs the lexical walk cannot follow (see header).
        function unsupported_findings(node, rest, value) {
            if (node ~ /^[[:space:]]*(-[[:space:]]+)*[&*]/) {
                print "line " NR ": YAML anchor or alias is not supported here: " $0
            }
            rest = node; sub(/^[[:space:]]*(-[[:space:]]+)*/, "", rest)
            if (match(rest, /:[[:space:]]+/)) {
                value = substr(rest, RSTART + RLENGTH)
                if (value ~ /^(![^[:space:]]*[[:space:]]+)?[&*]/ ||
                    (value ~ /^[[{]/ && value ~ /([[{,]|:)[[:space:]]*[&*]/)) {
                    print "line " NR ": YAML anchor or alias is not supported here: " $0
                }
            }
            if (node ~ /(^|[[:space:]{,])<<[[:space:]]*:/) {
                print "line " NR ": YAML merge key is not supported here: " $0
            }
            if (!in_jobs) return
            if (node ~ /^[[:space:]]*-$/) {
                print "line " NR ": bare - sequence item is not supported under jobs:"
            } else if (node ~ /^[[:space:]]*-[[:space:]]+[[{]/) {
                print "line " NR ": flow-style sequence item is not supported under jobs: " $0
            } else if (node ~ /^[[:space:]]*(-[[:space:]]+)*["\047]/) {
                print "line " NR ": quoted key is not supported under jobs: " $0
            }
        }
        /^[[:space:]]*#/ || /^[[:space:]]*$/ { next }
        block_indent >= 0 && indent_of($0) <= block_indent { block_indent = -1 }
        block_indent < 0 {
            node = without_comment(unquoted($0))
            if (/^[^[:space:]]/) in_jobs = ($0 ~ /^jobs:/)
            unsupported_findings(node)
            if (node ~ /(:|^[[:space:]]*-)[[:space:]]+[|>][-+0-9]*$/) block_indent = indent_of($0)
        }
        /^[^[:space:]]/ { close_job(); in_jobs = ($0 ~ /^jobs:/) }
        in_jobs && /^  [A-Za-z0-9_-]+:/ {
            close_job()
            job = $1; sub(/:$/, "", job)
        }
        job != "" {
            line = without_comment($0)
            if (block_step) {
                if (block_step == checkout_step + 1 && trim(line) == preflight) preflight_ok = 1
                block_step = 0
            }
            if (step_indent == "" && line ~ /^[[:space:]]+-[[:space:]]/ && seen_steps) {
                match(line, /^[[:space:]]+/)
                step_indent = substr(line, 1, RLENGTH)
            }
            if (line ~ /^[[:space:]]+steps:$/) seen_steps = 1
            if (step_indent != "" && index(line, step_indent "- ") == 1) {
                step++
                seen_steps = 0
            }
            if (step && line ~ /^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]*actions\/checkout@/ && !checkout_step) {
                checkout_step = step; checkout_line = NR
            }
            if (step && checkout_step && step == checkout_step + 1 && line ~ /^[[:space:]]*(-[[:space:]]+)?run:[[:space:]]/) {
                cmd = line; sub(/^[[:space:]]*(-[[:space:]]+)?run:[[:space:]]+/, "", cmd)
                if (cmd == preflight) preflight_ok = 1
                else if (cmd == "|" || cmd == "|-") block_step = step
            }
            if (!cargo_line && runs_cargo(line)) cargo_line = NR
        }
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
        toupper($0) ~ /AWS_LC_SYS_(USE_SYSTEM|STATIC|SYSTEM_DIR|SYSTEM_BINDINGS|NO_PREFIX)/ {
            print "line " NR ": AWS-LC build variable set outside the top-level pin: " $0
        }
        END {
            close_job()
            for (name in exempt_job) {
                if (!(name in exempt_seen)) print "exemption names job " name ", which this workflow does not define"
            }
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
        echo "aws-lc-pin: expected '$USE_SYSTEM_PIN' and '$STATIC_PIN' once each in the top-level env:, and nowhere else," >&2
        echo "aws-lc-pin: and '$ENV_PREFLIGHT' as the step right after every non-exempt job's checkout" >&2
        return 1
    fi
    echo "aws-lc-pin: PASS"
}

main "$@"
