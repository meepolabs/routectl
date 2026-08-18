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
# Three scan shapes, because a field can reach `%` three ways:
#   1. `field = %expr` on one line,
#   2. the same split across lines by rustfmt,
#   3. tracing's positional shorthand `%field,` (no sanitizer can sit in
#      that form at all, so every wire-field occurrence is a finding).
# Shapes 1 and 2 share one multiline pattern whose negative lookahead sits
# IMMEDIATELY after the `%`, so the sanitizer must wrap THIS field's value:
# a trailing `// ... sanitize_for_log` comment, or a sanitized sibling
# field on the same line, no longer launders an unsanitized one.
#
# Pure grep -- no cargo invocation, so it stays cheap enough for a
# pre-commit leg. Fail-closed: a missing `rg` or a renamed search path is a
# gate FAILURE, never a vacuous PASS.

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

ALLOWLIST="tools/log-display-allowlist.txt"

# Tracing field names whose value is derived from a request or response
# body, an SSE frame, an upstream response header, or a caller-supplied
# config bag.
WIRE_FIELDS='type_tag|block_type|part_type|media_type|source_type|event_type|event_kind|tool_choice|tool_choice_type|finish_reason|block_reason|tool_type|fn_name|tool|tool_id|flag|key|field|generated_id|correlation_id|terminal|representative_claim|overage_status|utilization|overage_utilization|reset'

# `sanitize_detail_for_log` is accepted alongside `sanitize_for_log`: it
# routes through the same control-char filter and length cap.
SANITIZERS='(?:[A-Za-z0-9_]+::)*sanitize(?:_detail)?_for_log'

SEARCH_PATHS=(
    crates/routectl-providers/src
    crates/routectl-cli/src/ingress
)

if ! command -v rg >/dev/null 2>&1; then
    echo "log-display: FAIL ripgrep (rg) not found; the gate cannot scan" >&2
    exit 1
fi

missing_paths=""
for p in "${SEARCH_PATHS[@]}"; do
    if [[ ! -d "$p" ]]; then
        missing_paths+="  $p"$'\n'
    fi
done
if [[ -n "$missing_paths" ]]; then
    echo "log-display: FAIL search path(s) missing -- moved or renamed code would" >&2
    echo "log-display: go unscanned. Update SEARCH_PATHS in $0:" >&2
    printf '%s' "$missing_paths" >&2
    exit 1
fi

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

# `field = %expr` (shapes 1 and 2): one multiline pattern, whitespace and
# newlines tolerated around the `=` and the `%`.
scan_assigned() {
    rg -U -P --no-heading --line-number --only-matching --replace '$1' \
        -e "(?<![A-Za-z0-9_])($WIRE_FIELDS)[ \t\r\n]*=[ \t\r\n]*%[ \t\r\n]*(?!$SANITIZERS)" \
        "${SEARCH_PATHS[@]}" || true
}

# Positional shorthand `%field` (shape 3), at the start of a line or right
# after the macro's `(` or a `,`.
scan_positional() {
    rg -P --no-heading --line-number --only-matching --replace '$1' \
        -e "(?:^|[(,])[ \t]*%($WIRE_FIELDS)\b" \
        "${SEARCH_PATHS[@]}" || true
}

# A doc / line comment quoting one of these shapes is prose, not a call.
is_comment_line() {
    local file="$1" lineno="$2" text
    text="$(sed -n "${lineno}p" "$file")"
    case "$(printf '%s' "$text" | sed -e 's/^[[:space:]]*//')" in
    //*) return 0 ;;
    esac
    return 1
}

FINDINGS=""
while IFS= read -r hit; do
    [[ -n "$hit" ]] || continue
    file="${hit%%:*}"
    rest="${hit#*:}"
    lineno="${rest%%:*}"
    field="${rest#*:}"
    if is_comment_line "$file" "$lineno"; then
        continue
    fi
    if [[ -n "${ALLOWED["$file:$field"]:-}" ]]; then
        continue
    fi
    FINDINGS+="  $file:$lineno  $field"$'\n'
done < <(
    scan_assigned
    scan_positional
)

if [[ -n "$FINDINGS" ]]; then
    echo "log-display: FAIL wire-derived tracing fields rendered via % without sanitize_for_log:" >&2
    printf '%s' "$FINDINGS" >&2
    echo "log-display: a % field passes raw bytes into the log line -- no control-char" >&2
    echo "log-display: escaping, no length cap. Wrap the value in sanitize_for_log, or add" >&2
    echo "log-display: a '<path>:<field>  # <reason>' entry to $ALLOWLIST." >&2
    exit 1
fi

echo "log-display: PASS no unsanitized wire-derived % tracing fields"
