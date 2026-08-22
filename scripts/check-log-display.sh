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
# Scope: provider egress/response code, ingress request/stream code, the
# per-request handlers and MITM proxy, and the router + server-startup +
# config-command surfaces -- the code that touches caller and upstream bytes
# plus the code that renders operator-written config table keys
# (`[providers.X]`, `[models.X]`, `[aliases]` and friends). A config key is
# not "trusted input": a TOML file is an attacker-reachable artifact on a
# shared box, and a startup line is exactly where a forged record is least
# likely to be noticed. Nor is a config-key FIELD only ever a config key --
# on the forwarded-credential lane the `model` field carries the client
# request body's own model string, so the config-key tier must reach every
# surface that renders one.
#
# Field names are curated (WIRE_FIELDS below) rather than "every `%` field",
# because `error = %e` and `path = %p.display()` are internal values whose
# blanket inclusion would make the allowlist larger than the rule it guards.
#
# Adding a field name to WIRE_FIELDS is how the gate grows. A flagged site
# that provably carries no wire bytes goes in the allowlist file with a
# stated reason. A genuinely new sanitizer wrapper is a one-line edit to
# SANITIZERS below -- the list is closed on purpose.
#
# Five scan shapes, because a field can reach `%` five ways:
#   1. `field = %expr` on one line,
#   2. the same split across lines by rustfmt,
#   3. tracing's positional shorthand `%field,` (no sanitizer can sit in
#      that form at all, so every wire-field occurrence is a finding),
#   4. `field = %<name>_safe`, a value sanitized once at its `let` and
#      reused by sibling fields (verified against that `let`, not trusted
#      on the name alone),
#   5. `{field}` interpolated into the message body, which renders through
#      Display with no field ever appearing -- invisible to 1-4.
# Shapes 1 and 2 share one multiline pattern whose negative lookahead sits
# IMMEDIATELY after the `%`, so the sanitizer must wrap THIS field's value:
# a trailing `// ... sanitize_for_log` comment, or a sanitized sibling
# field on the same line, no longer launders an unsanitized one.
#
# A sixth path stays out of scope: the tier path sets are enumerated
# directories, so code moving out of one is a hole. The tier-drift check
# below closes it by requiring every `.rs` in the request-graph crates to
# be in a tier or declared unscanned with a reason.
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
# config bag. Scanned on every path.
WIRE_FIELDS='type_tag|block_type|part_type|media_type|source_type|event_type|event_kind|tool_choice|tool_choice_type|finish_reason|block_reason|tool_type|fn_name|tool|tool_id|flag|key|field|generated_id|correlation_id|terminal|representative_claim|overage_status|utilization|overage_utilization|reset'

# Field names that render an OPERATOR-WRITTEN config table key: a
# `[providers.X]` name, a `[models.X]` nickname, an `[aliases]` / overlay
# selector, or an advisory validator message quoting one of those. Scanned
# on CONFIG_KEY_PATHS -- every routectl-cli / routectl-router surface that
# reads the config tables or echoes a resolved name back, which now includes
# the per-request handlers (`model` there can be the client's own request
# body string, not a config key at all). The egress crate is the one
# exception: it renders `provider` on nearly every request-path line, and
# scanning it there would swamp the rule this gate guards without adding a
# reachable sink (a name that reaches an egress has already passed the
# surfaces below).
CONFIG_KEY_FIELDS='warning|pattern|provider|model|nickname|selector|changed_selectors'

# The four sanitizer entry points, enumerated rather than matched as a
# family: an open `sanitize_.*_for_log` pattern would accept any future
# name shaped like one, including a helper that only LOOKS like it
# sanitizes. Adding a real new wrapper here is a one-line gate edit --
# cheap, and it forces the wrapper past a reader.
SANITIZERS='(?:[A-Za-z0-9_]+::)*(?:sanitize_for_log_with_cap|sanitize_for_log|sanitize_detail_for_log|sanitize_warning_for_log)'

# A local bound once from a sanitizer and reused by sibling fields in the
# same function. Recognized by name, then verified against its `let`.
SAFE_LOCAL='[a-z0-9_]*_safe\b'

SEARCH_PATHS=(
    crates/routectl-providers/src
    crates/routectl-cli/src/ingress
    crates/routectl-cli/src/handlers
    crates/routectl-cli/src/proxy
    crates/routectl-cli/src/server
    crates/routectl-router/src
)

CONFIG_KEY_PATHS=(
    crates/routectl-cli/src/commands
    crates/routectl-cli/src/handlers
    crates/routectl-cli/src/proxy
    crates/routectl-cli/src/server
    crates/routectl-router/src
)

# Every `.rs` file under the two request-graph crates must sit in a tier
# above or be named here WITH a reason -- see the tier-drift check below.
# A new module directory is therefore a deliberate choice ("scan it" or
# "here is why not"), not a silent omission. Prefer scanning: the entries
# here are only the files where a tier scan has nothing to look at.
declare -A UNSCANNED_DIRS=(
    [crates/routectl-cli/src/lib.rs]="crate root: module declarations and re-exports, no tracing call of any kind"
    [crates/routectl-cli/src/main.rs]="binary shim: parses argv and hands off to commands/, no tracing call of any kind"
    [crates/routectl-cli/src/config_classify.rs]="pure Config-diff classification returning changed-key sets to its callers; no tracing call of any kind"
    [crates/routectl-cli/src/test_secret.rs]="test-only secret helper, not compiled into the shipped binary; no tracing call of any kind"
)

if ! command -v rg >/dev/null 2>&1; then
    echo "log-display: FAIL ripgrep (rg) not found; the gate cannot scan" >&2
    exit 1
fi

missing_paths=""
for p in "${SEARCH_PATHS[@]}" "${CONFIG_KEY_PATHS[@]}"; do
    if [[ ! -d "$p" ]]; then
        missing_paths+="  $p"$'\n'
    fi
done
if [[ -n "$missing_paths" ]]; then
    echo "log-display: FAIL search path(s) missing -- moved or renamed code would" >&2
    echo "log-display: go unscanned. Update SEARCH_PATHS / CONFIG_KEY_PATHS in $0:" >&2
    printf '%s' "$missing_paths" >&2
    exit 1
fi

# Tier-drift check. The gate's blind spot is not a bad regex, it is a new
# module nobody added to a tier: `handlers/` and `commands/` both carried
# live findings for exactly as long as they sat outside both path sets. So
# enumerate every `.rs` in the two request-graph crates and require each one
# to be covered by a tier or listed in UNSCANNED_DIRS with a reason.
TIER_CRATE_ROOTS=(
    crates/routectl-cli/src
    crates/routectl-router/src
)

is_tier_covered() {
    local file="$1" p
    for p in "${SEARCH_PATHS[@]}" "${CONFIG_KEY_PATHS[@]}" "${!UNSCANNED_DIRS[@]}"; do
        if [[ "$file" == "$p" || "$file" == "$p"/* ]]; then
            return 0
        fi
    done
    return 1
}

undeclared=""
while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    is_tier_covered "$f" || undeclared+="  $f"$'\n'
done < <(rg --files --glob '*.rs' "${TIER_CRATE_ROOTS[@]}")

if [[ -n "$undeclared" ]]; then
    echo "log-display: FAIL request-graph source outside every tier and not declared" >&2
    echo "log-display: unscanned. Add it to SEARCH_PATHS (wire tier) or to" >&2
    echo "log-display: CONFIG_KEY_PATHS (config-key tier), or add its path to" >&2
    echo "log-display: UNSCANNED_DIRS in $0 with a stated reason:" >&2
    printf '%s' "$undeclared" >&2
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

# Each scanner takes the field alternation as `$1` and the paths to scan as
# the remaining arguments, so the wire tier and the config-key tier share
# one implementation over their two different scopes.

# `field = %expr` (shapes 1 and 2): one multiline pattern, whitespace and
# newlines tolerated around the `=` and the `%`.
scan_assigned() {
    local fields="$1"
    shift
    rg -U -P --no-heading --line-number --only-matching --replace '$1' \
        -e "(?<![A-Za-z0-9_])($fields)[ \t\r\n]*=[ \t\r\n]*%[ \t\r\n]*(?!$SANITIZERS|$SAFE_LOCAL)" \
        "$@" || true
}

# `field = %<local>_safe` (shape 4): a value sanitized once at its binding
# and rendered by several sibling warns in the same function. Accepting the
# inline call only would force the sanitizer into every arm, and a message
# body interpolating `{provider_safe}` cannot hold a call at all. The
# `_safe` suffix is a claim, not proof -- so each use is paired back to a
# `let` binding that really does call a sanitizer, in the same file. An
# unbacked `_safe` name is a finding like any other.
scan_safe_locals() {
    local fields="$1"
    shift
    rg -U -P --no-heading --line-number --only-matching --replace '$1:$2' \
        -e "(?<![A-Za-z0-9_])($fields)[ \t\r\n]*=[ \t\r\n]*%[ \t\r\n]*($SAFE_LOCAL)" \
        "$@" || true
}

# True when `$2` is bound in file `$1` by a `let` whose initializer calls a
# sanitizer, AND the name is bound NOWHERE ELSE in the file.
#
# The second half is what makes the first half mean anything. The scan is
# file-scoped, not scope-aware, so a single sanitized `let` anywhere in the
# file would otherwise vouch for every same-named value in it -- an inner
# shadow, a second function's own raw `let`, a `&str` parameter, or a `mut`
# re-assignment after the sanitized binding. Requiring the name to have
# exactly one origin makes the pairing sound without a Rust parser: any
# other binding of it is a finding, even a harmless one (rename the local).
#
# `[ \t]*+` is possessive on purpose. A greedy `[ \t]*` would backtrack to
# consume fewer spaces and let `(?!$SANITIZERS\b)` succeed against the
# whitespace itself, so the sanitized `let` would match its own "other
# binding" pattern and every `_safe` local would be rejected.
has_sanitized_binding() {
    rg -q -P -e "let[ \t]+(?:mut[ \t]+)?$2[ \t]*(?::[^=]*)?=[ \t]*$SANITIZERS\b" "$1" || return 1
    ! rg -q -P \
        -e "let[ \t]+(?:mut[ \t]+)?$2[ \t]*(?::[^=]*)?=[ \t]*+(?!$SANITIZERS\b)" \
        -e "^[ \t]*$2[ \t]*=[^=]" \
        -e "(?<![A-Za-z0-9_])$2[ \t]*:[ \t]*&?(?:str|String|Cow)" \
        "$1"
}

# Positional shorthand `%field` (shape 3), at the start of a line or right
# after the macro's `(` or a `,`.
scan_positional() {
    local fields="$1"
    shift
    rg -P --no-heading --line-number --only-matching --replace '$1' \
        -e "(?:^|[(,])[ \t]*%($fields)\b" \
        "$@" || true
}

# `{name}` inside a WARN/ERROR/INFO message body (shape 5). An inline
# format capture renders through Display exactly like a `%` field does, so
# it carries the same raw bytes into the log line -- and neither of the
# field shapes above sees it, because the value never appears as a field at
# all. Restricted to the tier's own curated field names: a body
# interpolating `{host}` or `{bound}` is an internal value, and flagging
# every lowercase capture would bury the rule in noise. A `_safe` capture
# is not matched here (the name is not in the field list), and is proved
# against its `let` by the shape-4 scanner wherever it is also a field.
scan_message_body() {
    local fields="$1"
    shift
    rg -U -P --no-heading --line-number --only-matching --replace '$1' \
        -e "tracing::(?:warn|error|info)!\((?:[^;]*?)\"[^\"]*\{($fields)\}" \
        "$@" || true
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
    scan_assigned "$WIRE_FIELDS" "${SEARCH_PATHS[@]}"
    scan_positional "$WIRE_FIELDS" "${SEARCH_PATHS[@]}"
    scan_message_body "$WIRE_FIELDS" "${SEARCH_PATHS[@]}"
    scan_assigned "$CONFIG_KEY_FIELDS" "${CONFIG_KEY_PATHS[@]}"
    scan_positional "$CONFIG_KEY_FIELDS" "${CONFIG_KEY_PATHS[@]}"
    scan_message_body "$CONFIG_KEY_FIELDS" "${CONFIG_KEY_PATHS[@]}"
)

while IFS= read -r hit; do
    [[ -n "$hit" ]] || continue
    file="${hit%%:*}"
    rest="${hit#*:}"
    lineno="${rest%%:*}"
    rest="${rest#*:}"
    field="${rest%%:*}"
    local_name="${rest#*:}"
    if is_comment_line "$file" "$lineno"; then
        continue
    fi
    if [[ -n "${ALLOWED["$file:$field"]:-}" ]]; then
        continue
    fi
    if has_sanitized_binding "$file" "$local_name"; then
        continue
    fi
    FINDINGS+="  $file:$lineno  $field (\`$local_name\` has no sanitized let binding)"$'\n'
done < <(
    scan_safe_locals "$WIRE_FIELDS" "${SEARCH_PATHS[@]}"
    scan_safe_locals "$CONFIG_KEY_FIELDS" "${CONFIG_KEY_PATHS[@]}"
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
