#!/usr/bin/env bash
# Flag tracing fields that render a wire-derived string via Display (`%`).
#
# Why this gate exists: the `fmt` subscriber writes a `%`-rendered field
# value into the log line verbatim -- no control-char escaping, no length
# cap. `?` (Debug) and bare-`&str` fields DO escape; `%` does not. So a
# wire string carrying `\n` forges a whole log line and one carrying an
# ANSI CSI sequence scrolls an operator's terminal. `sanitize_for_log`
# filters non-printable ASCII and caps at 256 chars; every wire-derived
# `%` field must route through it.
#
# Scope: provider egress/response code and ingress request/stream code --
# the two surfaces that touch caller and upstream bytes. Field names are
# curated (WIRE_FIELDS below) rather than "every `%` field", because
# `provider = %id`, `error = %e`, and `path = %p.display()` are internal
# values whose blanket inclusion would make the allowlist larger than the
# rule it guards.
#
# Adding a field name to WIRE_FIELDS is how the gate grows. A flagged site
# that provably carries no wire bytes goes in the allowlist file with a
# stated reason.
#
# Pure grep -- no cargo invocation, so it stays cheap enough for a
# pre-commit leg.

set -e

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

ALLOWLIST="tools/log-display-allowlist.txt"

# Tracing field names whose value is derived from a request or response
# body, an SSE frame, or a caller-supplied config bag.
WIRE_FIELDS='type_tag|block_type|part_type|media_type|source_type|event_type|event_kind|tool_choice|tool_choice_type|finish_reason|block_reason|tool_type|fn_name|tool|tool_id|flag|key|field|generated_id|correlation_id|terminal'

SEARCH_PATHS=(
    crates/routectl-providers/src
    crates/routectl-cli/src/ingress
)

# `path:field` pairs (not path:line -- line numbers rot on every edit).
declare -A ALLOWED=()
if [[ -f "$ALLOWLIST" ]]; then
    while IFS= read -r line; do
        line="${line%%#*}"
        line="$(printf '%s' "$line" | tr -d '[:space:]')"
        [[ -n "$line" ]] || continue
        ALLOWED["$line"]=1
    done <"$ALLOWLIST"
fi

FINDINGS=""
while IFS= read -r hit; do
    file="${hit%%:*}"
    rest="${hit#*:}"
    lineno="${rest%%:*}"
    text="${rest#*:}"
    # Strip the leading indentation, then read the field name off the front.
    trimmed="$(printf '%s' "$text" | sed -e 's/^[[:space:]]*//')"
    case "$trimmed" in
    //*) continue ;;
    esac
    field="${trimmed%% =*}"
    if [[ -n "${ALLOWED["$file:$field"]:-}" ]]; then
        continue
    fi
    FINDINGS+="  $file:$lineno  $field"$'\n'
done < <(rg --no-heading --line-number -e "(^|[[:space:]])($WIRE_FIELDS) = %" "${SEARCH_PATHS[@]}" |
    grep -v 'sanitize_for_log' || true)

if [[ -n "$FINDINGS" ]]; then
    echo "log-display: FAIL wire-derived tracing fields rendered via % without sanitize_for_log:" >&2
    printf '%s' "$FINDINGS" >&2
    echo "log-display: a % field passes raw bytes into the log line -- no control-char" >&2
    echo "log-display: escaping, no length cap. Wrap the value in sanitize_for_log, or add" >&2
    echo "log-display: a '<path>:<field>  # <reason>' entry to $ALLOWLIST." >&2
    exit 1
fi

echo "log-display: PASS no unsanitized wire-derived % tracing fields"
