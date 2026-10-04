#!/usr/bin/env bash
# Drift check for a hand-written replica of the structural summary line
# `trace_structural_summary` in crates/routectl-core/src/log_safe.rs emits.
#
# Sourced, never executed, and only by the shell self-tests. Each of them
# builds its canned trace text by hand, because the rig under test runs in
# throwaway trees with no daemon. A replica that drifts from the emitter
# keeps its own suite green while the predicates downstream read a line no
# real capture produces, so every suite runs its replicas through here.
#
# A line's SHAPE is the sequence of its `key=value` field names, each
# marked by whether the value is quoted (`name="`) or bare (`name=`). The
# reference shape is derived from the emitter's own `tracing::trace!` field
# list: a shorthand `name,` field is a `&str` recorded through Debug, which
# quotes it; `name = %expr` is recorded through Display and `name = expr`
# is a number or bool, both bare. drivers.test.sh welds that derivation to
# a committed capture's recorded line, so the rule is checked against real
# emitter output rather than trusted.

# The emitter source under the repo root `$1`, or nothing when this is a
# scripts-only checkout with no crates tree.
structural_emitter_source() {
    local src="$1/crates/routectl-core/src/log_safe.rs"
    [ -f "$src" ] && printf '%s\n' "$src"
    return 0
}

# The shape of one structural line: everything after the
# `structural summary ` message, reduced token by token.
structural_line_shape() {
    local line="$1" token name value shape=""
    local -a tokens
    case "$line" in
        *"structural summary "*) line="${line#*structural summary }" ;;
        *)
            printf '\n'
            return 0
            ;;
    esac
    read -r -a tokens <<<"$line"
    for token in "${tokens[@]}"; do
        name="${token%%=*}"
        value="${token#*=}"
        if [ "$name" = "$token" ]; then
            shape="$shape $token"
        elif [ "${value#\"}" != "$value" ]; then
            shape="$shape $name=\""
        else
            shape="$shape $name="
        fi
    done
    printf '%s\n' "${shape# }"
}

# The reference shape, read out of the emitter source at `$1`. A field
# line the rule above does not recognise is emitted as `UNPARSED:<line>`,
# so an emitter change the derivation cannot read fails every comparison
# loudly instead of silently shortening the reference.
emitter_structural_shape() {
    awk '
        /^pub fn trace_structural_summary\(/ { in_fn = 1; next }
        in_fn && /tracing::trace!\(/ { in_macro = 1; next }
        in_macro && /"structural summary"/ { exit }
        in_macro {
            field = $0
            sub(/^[ \t]+/, "", field)
            sub(/,[ \t]*$/, "", field)
            if (field == "") next
            if (field ~ /^[a-z_]+$/) {
                out = out " " field "=\""
            } else if (field ~ /^[a-z_]+ = /) {
                split(field, parts, " = ")
                out = out " " parts[1] "="
            } else {
                out = out " UNPARSED:" field
            }
        }
        END { sub(/^ /, "", out); print out }
    ' "$1"
}

# Is the replica `$1` faithful? Prints the reason and returns 1 when not.
#
#   $2  the wire pattern the replica claims to exhibit, checked through
#       verify_pattern.py's `--structural-line` mode -- the same predicate
#       the promotion gates run -- or `-` for a line claiming none (an
#       outgoing line, or a line whose suite claims a body-census pattern)
#   $3  path to verify_pattern.py
#   $4  path to log_safe.rs, or empty to skip the shape comparison (a
#       scripts-only checkout has no emitter source to compare against)
structural_line_drift() {
    local line="$1" pattern="$2" verifier="$3" emitter_src="$4"
    local want got why
    if [ -n "$emitter_src" ]; then
        want="$(emitter_structural_shape "$emitter_src")"
        got="$(structural_line_shape "$line")"
        if [ -z "$want" ]; then
            printf 'no structural field list could be read from %s\n' "$emitter_src"
            return 1
        fi
        if [ "$want" != "$got" ]; then
            printf 'shape differs from the emitter\n  emitter: %s\n  replica: %s\n' \
                "$want" "$got"
            return 1
        fi
    fi
    if [ "$pattern" != "-" ]; then
        if ! why="$(printf '%s\n' "$line" |
            python3 "$verifier" --structural-line "$pattern" 2>&1)"; then
            printf 'does not exhibit %s: %s\n' "$pattern" "$why"
            return 1
        fi
    fi
    return 0
}

# `structural_line_drift` as a named self-test assertion: prints PASS or
# FAIL with `$1` as the label and returns 1 on FAIL, so the caller counts
# the failure in its own tally. `$2`..`$5` are the drift check's arguments.
assert_structural_replica() {
    local label="$1" why
    shift
    if why="$(structural_line_drift "$@")"; then
        echo "PASS: $label"
        return 0
    fi
    echo "FAIL: $label -- $why"
    return 1
}

# Every structural line of the canned trace on stdin, each asserted with
# `assert_structural_replica`: the ingress line against the pattern `$2`,
# any other direction against shape alone. A line counts only when it is
# the log_safe event itself, so a request body that merely QUOTES one is
# not mistaken for it. A trace with no ingress line FAILS, since there
# would be nothing for the pattern to have checked.
# Returns the number of failures, so a caller adds `$?` to its tally.
#
#   $1  label prefix   $2  ingress pattern, or `-`
#   $3  verify_pattern.py   $4  log_safe.rs, or empty
assert_trace_replicas() {
    local label="$1" pattern="$2" verifier="$3" emitter_src="$4"
    local line direction failed=0 ingress_seen=0
    while IFS= read -r line; do
        case "$line" in
            *"log_safe: structural summary direction=\""*) ;;
            *) continue ;;
        esac
        direction="${line#*structural summary direction=\"}"
        direction="${direction%%\"*}"
        if [ "$direction" = "ingress" ]; then
            ingress_seen=1
            assert_structural_replica "$label: ingress line" \
                "$line" "$pattern" "$verifier" "$emitter_src" || failed=$((failed + 1))
        else
            assert_structural_replica "$label: $direction line" \
                "$line" - "$verifier" "$emitter_src" || failed=$((failed + 1))
        fi
    done
    if [ "$ingress_seen" -eq 0 ]; then
        echo "FAIL: $label: the trace carries no ingress structural line"
        failed=$((failed + 1))
    fi
    return "$failed"
}
