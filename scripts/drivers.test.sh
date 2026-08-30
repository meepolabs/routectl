#!/usr/bin/env bash
# Self-test for the canonical interaction set and the harness drivers.
# Exits 0 when all assertions pass, non-zero on the first failure.
#
# NO REAL CLIENT AND NO REAL DAEMON. A real run needs a provider
# credential; CI has none and a driven session spends tokens. So every
# end-to-end case runs the REAL runner and the REAL driver against a STUB
# `routectl` (injected through the runner's `ROUTECTL_BIN`) and a STUB
# client (injected through each driver's documented binary override). The
# stub daemon emits a canned trace on the same stderr the runner captures
# from a real daemon, so the driver -> runner -> rig -> landing path is
# exercised whole.
#
# Every case runs inside a throwaway repo carrying copies of the runner,
# the rig, the scrub script, and the whole scripts/drivers tree, exactly as
# the runner's own self-test does: the runner and the case validator both
# resolve their roots from their own location, so a throwaway repo is what
# keeps the real corpus out of the blast radius.
#
# The stub client dumps its argv and the environment it was handed to a
# file OUTSIDE the run workspace, since the runner removes that workspace
# on exit. Asserting the connection mode from inside the client's own
# environment is the only honest place to assert it: that environment is
# what decides which wire shape a real client would emit.
#
# Requires python3 (the case validator and the stub listener), `ss` (the
# runner's port probe), and `script` (the interactive driver's pty).
#
# Run it from anywhere:
#   bash scripts/drivers.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
RUNNER="$HERE/capture_driver.sh"
RIG="$HERE/capture_fixtures.sh"
SCRUB="$HERE/scrub-fixture.sh"
DRIVERS="$HERE/drivers"
CASES="$DRIVERS/cases"
VALIDATOR="$DRIVERS/lib/validate_case.py"
VERIFIER="$DRIVERS/lib/verify_pattern.py"
INGRESS_KINDS="$DRIVERS/lib/ingress_kinds.sh"
CLIENT_VERSION="$DRIVERS/lib/client_version.py"
CLASSIFICATION="$DRIVERS/lib/wire_pattern_classification.tsv"

DRIVER_FILES=(
    "$DRIVERS/claude-code.sh"
    "$DRIVERS/claude-code-print.sh"
    "$DRIVERS/external-agent-cli.sh"
)

fails=0

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- expected '$expected', got '$actual'"
        fails=$((fails + 1))
    fi
}

check_ne() {
    local label="$1" forbidden="$2" actual="$3"
    if [ "$forbidden" != "$actual" ]; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- value must differ from '$forbidden'"
        fails=$((fails + 1))
    fi
}

fail() {
    echo "FAIL: $1"
    fails=$((fails + 1))
}

# Assert the validator ACCEPTS a case.
accepts() {
    local label="$1" path="$2" out
    if out="$(python3 "$VALIDATOR" --check "$path" 2>&1)"; then
        echo "PASS: $label"
    else
        fail "$label -- validator refused it: $out"
    fi
}

# Assert the validator REFUSES a case, and that its refusal names the
# reason rather than crashing: an unhandled traceback also exits non-zero.
refuses() {
    local label="$1" path="$2" needle="$3" out rc=0
    out="$(python3 "$VALIDATOR" --check "$path" 2>&1)" || rc=$?
    if [ "$rc" = 0 ]; then
        fail "$label -- the validator ACCEPTED it"
    elif ! printf '%s' "$out" | grep -qF -- "$needle"; then
        fail "$label -- refused, but the reason did not mention '$needle': $out"
    else
        echo "PASS: $label"
    fi
}

for tool in python3 ss git curl sha256sum script; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "FAIL: $tool not found; this self-test cannot exercise the drivers"
        exit 1
    fi
done

# ---------------------------------------------------------------------
# Part 1: the canonical interaction set, as data
# ---------------------------------------------------------------------

case_files=()
while IFS= read -r path; do
    case_files+=("$path")
done < <(find "$CASES" -maxdepth 1 -name '*.json' | sort)

if [ "${#case_files[@]}" -lt 4 ]; then
    fail "the case set holds only ${#case_files[@]} files; it must cover four wire patterns"
else
    echo "PASS: the case set holds ${#case_files[@]} case files"
fi

# Every committed case conforms to the documented schema. This is the
# positive control the rejection assertions below need: without it a
# validator that refused EVERYTHING would read as a clean sweep.
for path in "${case_files[@]}"; do
    accepts "$(basename "$path") conforms to the schema" "$path"
done

# Ids are unique. Two cases sharing an id would key on one landing
# directory and the second capture would silently replace the first.
id_count="${#case_files[@]}"
uniq_count="$(
    for path in "${case_files[@]}"; do
        python3 "$VALIDATOR" --field case_id "$path"
    done | sort -u | wc -l
)"
check "every case id is unique" "$id_count" "$uniq_count"

# Ids are path-safe and neutral BY CHARSET, asserted on the committed set
# rather than only inside the validator: the id names a directory under the
# corpus root and is scanned by the scrub gate as part of meta.json.
for path in "${case_files[@]}"; do
    id="$(python3 "$VALIDATOR" --field case_id "$path")"
    if printf '%s' "$id" | grep -qE '^[a-z0-9]+(-[a-z0-9]+)*$'; then
        echo "PASS: case id '$id' is path-safe and neutral"
    else
        fail "case id '$id' is outside the documented charset"
    fi
    check "case id '$id' matches its filename stem" "$(basename "$path" .json)" "$id"
done

# The set covers the five PATTERNS the corpus exists to pin. A set missing
# one is a corpus that cannot detect drift in that shape at all.
patterns="$(
    for path in "${case_files[@]}"; do
        python3 "$VALIDATOR" --field wire_pattern "$path"
    done | sort -u
)"
for pattern in tool-use-multiturn cache-breakpoints thinking large-context mcp-tools; do
    if printf '%s\n' "$patterns" | grep -qx "$pattern"; then
        echo "PASS: the set covers the $pattern wire pattern"
    else
        fail "no case covers the $pattern wire pattern"
    fi
done

# A case names no model: which model serves a pattern is the lane config's
# business, and a model id in a case file would make the set a snapshot of
# a model list instead of of a wire shape.
if grep -rlE 'claude-[a-z]+-[0-9]|gpt-[0-9]|gemini-[0-9]' "$CASES" >/dev/null 2>&1; then
    fail "a case file names a specific model"
else
    echo "PASS: no case file names a specific model"
fi

# A case embeds no harness invocation: that is a driver's job, and a case
# that named a binary could only ever be replayed through one client. The
# pattern is the SHAPE of an invocation -- a field that would carry a
# command line -- rather than a list of client names: an enumeration here
# would have to hold every harness's name in tracked content, which is the
# thing the one-file-per-harness layout exists to avoid.
invocation_re='"(harness|client|binary|bin|command|cmd|argv|exec|flags?|args)"[[:space:]]*:'
if grep -qE "$invocation_re" "$CASES"/*.json; then
    fail "a case file carries a harness invocation field"
else
    echo "PASS: no case file embeds a harness invocation"
fi

# --- The front-proxy twin is welded to its base case --------------------
# The cross-mode comparison needs the SAME interaction captured in both
# connection modes, so `plain-turn-01-fp` is a full copy of
# `plain-turn-01`: a symlink would break the filename-stem weld on
# `case_id`, and suffixing at the runner would split the case identity
# between the run record and `meta.json`. The cost of a copy is drift, and
# a drifted twin turns a mode comparison into an unattributable diff --
# the difference could be the mode or could be the case. Welded here, as a
# test rather than a mechanism, so a divergence in `turns`, `knobs` or
# `wire_pattern` is a failure instead of a silently wrong comparison.
BASE_PAIR="$CASES/plain-turn-01.json"
TWIN_PAIR="$CASES/plain-turn-01-fp.json"

# Only the three identity/prose fields may differ; everything else is
# compared as PARSED JSON, so key order and whitespace cannot mask a
# difference and cannot manufacture one either.
twin_matches_base() {
    if python3 - "$1" "$2" <<'PY'
import json, sys

IDENTITY = ("case_id", "title", "notes")


def interaction(path):
    with open(path, encoding="utf-8") as handle:
        case = json.load(handle)
    return {key: value for key, value in case.items() if key not in IDENTITY}


sys.exit(0 if interaction(sys.argv[1]) == interaction(sys.argv[2]) else 1)
PY
    then
        printf 'yes\n'
    else
        printf 'no\n'
    fi
}

check "the front-proxy twin carries the base case's interaction" "yes" \
    "$(twin_matches_base "$BASE_PAIR" "$TWIN_PAIR")"
check_ne "the twin's case_id differs from the base's" \
    "$(python3 "$VALIDATOR" --field case_id "$BASE_PAIR")" \
    "$(python3 "$VALIDATOR" --field case_id "$TWIN_PAIR")"

# PAIRED CONTROL: the same comparison against a twin whose knob was
# flipped MUST report a difference. Without it the weld above is
# satisfiable by a comparison that always agrees -- a field filter that
# drops everything, or an exit status nobody reads.
twin_mut="$(mktemp -d)"
drifted="$twin_mut/plain-turn-01-fp.json"
python3 - "$TWIN_PAIR" "$drifted" <<'PY'
import json, sys

with open(sys.argv[1], encoding="utf-8") as handle:
    case = json.load(handle)
case["knobs"]["thinking"] = not case["knobs"]["thinking"]
with open(sys.argv[2], "w", encoding="utf-8") as handle:
    json.dump(case, handle)
PY
check_ne "the mutation actually altered the twin" \
    "$(sha256sum <"$TWIN_PAIR" | cut -d' ' -f1)" \
    "$(sha256sum <"$drifted" | cut -d' ' -f1)"
check "the weld reports a difference when a twin knob drifts" "no" \
    "$(twin_matches_base "$BASE_PAIR" "$drifted")"
rm -rf "$twin_mut"

# --- The rejection half, each paired against the accepted original ------
# A malformed case that reached a driver would be caught only after a
# daemon was booted and a client was mid-session.
mut="$(mktemp -d)"
BASE_CASE="$CASES/tools-multiturn-01.json"

# The rejections are asserted against MUTATIONS of a case the validator
# just accepted, so a refusal can only be attributed to the mutation.
#
# The copy's `case_id` is rewritten to its own filename stem BEFORE the
# mutation is applied. Otherwise every mutation would trip the stem rule
# first and each assertion would pass for the wrong reason -- a mutation
# that changes the id still wins, since it runs after.
mutate() {
    local name="$1" expr="$2"
    local dest="$mut/$name.json"
    python3 - "$BASE_CASE" "$dest" "$expr" "$name" <<'PY'
import json, sys
with open(sys.argv[1]) as fh:
    case = json.load(fh)
case["case_id"] = sys.argv[4]
exec(sys.argv[3], {"case": case})
with open(sys.argv[2], "w") as fh:
    json.dump(case, fh)
PY
    printf '%s\n' "$dest"
}

# The id charset. A `../` or a `/` in an id escapes the lane directory the
# rig confined the landing to, and the rig's own guard runs on OUT alone.
#
# The needle is the CHARSET message's own wording, not the field name: the
# stem rule below also names `case_id`, so a laxer needle let all five of
# these pass on the stem check alone -- confirmed by deleting the charset
# check, which left them green.
charset_needle="names the landing directory"
refuses "an id with a traversal segment is refused" \
    "$(mutate 'traversal' 'case["case_id"] = "../escaped"')" "$charset_needle"
refuses "an id with a path separator is refused" \
    "$(mutate 'separator' 'case["case_id"] = "lane/case"')" "$charset_needle"

# These two are named so the file's own STEM is the offending id, which
# leaves the charset rule as the only rule that can refuse them.
refuses "an id with an uppercase character is refused" \
    "$(mutate 'Tools-Multiturn-01' 'pass')" "$charset_needle"
refuses "an id with an underscore is refused" \
    "$(mutate 'tools_multiturn_01' 'pass')" "$charset_needle"
refuses "an id that does not match its filename stem is refused" \
    "$(mutate 'mismatched' 'case["case_id"] = "some-other-id"')" "filename stem"

# The schema.
refuses "a case missing a required key is refused" \
    "$(mutate 'no-knobs' 'del case["knobs"]')" "missing keys"
refuses "a case carrying an unknown key is refused" \
    "$(mutate 'extra-key' 'case["harness"] = "some-cli"')" "unknown keys"
# A case still carrying the retired `lane` key must fail LOUDLY rather than
# be tolerated: the lane now comes from the runner's `--lane` alone, so a
# case that still names one would silently disagree with the run it drives.
refuses "a case still carrying a lane key is refused" \
    "$(mutate 'stale-lane' 'case["lane"] = "anthropic-api"')" "unknown keys"
refuses "a case on an unknown schema version is refused" \
    "$(mutate 'wrong-version' 'case["schema_version"] = 2')" "schema_version"
refuses "a case with an unknown wire pattern is refused" \
    "$(mutate 'unknown-pattern' 'case["wire_pattern"] = "freeform"')" "wire_pattern"
refuses "a case with an empty turn list is refused" \
    "$(mutate 'no-turns' 'case["turns"] = []')" "turns"
refuses "a turn carrying a key other than prompt is refused" \
    "$(mutate 'extra-turn-key' 'case["turns"][0]["role"] = "user"')" "prompt"
refuses "an empty prompt is refused" \
    "$(mutate 'empty-prompt' 'case["turns"][0]["prompt"] = "  "')" "empty"
refuses "a prompt carrying a newline is refused" \
    "$(mutate 'multiline-prompt' 'case["turns"][0]["prompt"] = "one\ntwo"')" "single-line"
refuses "an unknown knob is refused" \
    "$(mutate 'unknown-knob' 'case["knobs"]["stream"] = True')" "unknown knobs"
refuses "a missing knob is refused" \
    "$(mutate 'missing-knob' 'del case["knobs"]["thinking"]')" "missing knobs"
refuses "a non-boolean capability knob is refused" \
    "$(mutate 'stringy-knob' 'case["knobs"]["tools"] = "yes"')" "boolean"
refuses "a boolean where padding bytes belong is refused" \
    "$(mutate 'boolean-padding' 'case["knobs"]["context_padding_bytes"] = True')" "integer"
refuses "negative padding is refused" \
    "$(mutate 'negative-padding' 'case["knobs"]["context_padding_bytes"] = -1')" "negative"
refuses "padding past the cap is refused" \
    "$(mutate 'huge-padding' 'case["knobs"]["context_padding_bytes"] = 1 << 30')" "cap"

# Malformed JSON, and a non-object document.
printf '{"schema_version": 1,\n' >"$mut/broken.json"
refuses "a case that is not valid JSON is refused" "$mut/broken.json" "not valid JSON"
printf '[]\n' >"$mut/array.json"
refuses "a case that is not a JSON object is refused" "$mut/array.json" "JSON object"
refuses "a missing case file is refused" "$mut/absent.json" "unreadable"

# The paired positive control for every mutation above: the SAME machinery
# that produced them, applied as a no-op, still yields an accepted case.
accepts "an unmutated copy of the base case is still accepted" \
    "$(mutate 'tools-multiturn-01' 'pass')"

rm -rf "$mut"

# ---------------------------------------------------------------------
# Part 1b: the wire-pattern predicate
# ---------------------------------------------------------------------
# `meta.wire_pattern` is a recorded CLAIM; this is the code that decides
# whether the captured bytes back it. Every leg below runs the predicate
# against a staged fixture directory built here, so a refusal can only be
# attributed to the one clause the leg flips -- a fixture-reading assertion
# alone cannot distinguish a real check from one that passes on any input.

pat="$(mktemp -d)"

# Assert the verifier ACCEPTS a staged fixture as exhibiting a pattern.
exhibits() {
    local label="$1" dir="$2" pattern="$3" out
    if out="$(python3 "$VERIFIER" "$dir" "$pattern" 2>&1)"; then
        echo "PASS: $label"
    else
        fail "$label -- the verifier refused it: $out"
    fi
}

# Assert the verifier REFUSES, and that its reason names the clause rather
# than crashing: an unhandled traceback also exits non-zero.
denies() {
    local label="$1" dir="$2" pattern="$3" needle="$4" out rc=0
    out="$(python3 "$VERIFIER" "$dir" "$pattern" 2>&1)" || rc=$?
    if [ "$rc" = 0 ]; then
        fail "$label -- the verifier ACCEPTED it"
    elif ! printf '%s' "$out" | grep -qF -- "$needle"; then
        fail "$label -- refused, but the reason did not mention '$needle': $out"
    else
        echo "PASS: $label"
    fi
}

# One ingress structural summary line in the real field order, with the
# three predicate fields supplied by the caller. A `-` thinking argument
# OMITS the token, which is a distinct input from an empty value.
s_line() {
    local direction="$1" tools_len="$2" thinking="$3" cache="$4" thinking_token=""
    [ "$thinking" = "-" ] || thinking_token="thinking_shape=$thinking "
    printf 'structural summary direction="%s" kind="ingress" id="anthropic" ' "$direction"
    printf 'model=claude-sonnet-4-5 max_tokens=32000 %s' "$thinking_token"
    printf 'output_config_effort= tool_choice_shape= cache_control_count=%s ' "$cache"
    printf 'messages_len=1 tools_len=%s anthropic_beta= provider_extras_keys= stream=true\n' "$tools_len"
}

# Stage a fixture directory holding a structural file and/or an ingress
# body. An empty argument leaves that file ABSENT, which is the input the
# fail-closed legs need.
stage() {
    local name="$1" structural="$2" body="$3"
    local dir="$pat/$name"
    mkdir -p "$dir"
    [ -n "$structural" ] && printf '%s' "$structural" >"$dir/structural.txt"
    [ -n "$body" ] && printf '%s' "$body" >"$dir/ingress_request.json"
    printf '%s\n' "$dir"
}

# --- baseline: accept legs, then one leg per clause ---------------------

exhibits "baseline accepts an explicitly disabled thinking shape" \
    "$(stage baseline-disabled "$(s_line ingress 0 disabled 0)" "")" baseline
exhibits "baseline accepts an absent thinking shape" \
    "$(stage baseline-absent "$(s_line ingress 0 - 0)" "")" baseline
denies "baseline refuses a line carrying tools" \
    "$(stage baseline-tools "$(s_line ingress 16 disabled 0)" "")" baseline "tools_len"
denies "baseline refuses an enabled thinking shape" \
    "$(stage baseline-thinking "$(s_line ingress 0 enabled:31999 0)" "")" baseline "thinking_shape"
denies "baseline refuses cache breakpoints" \
    "$(stage baseline-cache "$(s_line ingress 0 disabled 3)" "")" baseline "cache_control_count"

# An absent count token is not a zero: a summary the emitter did not write
# describes no observed shape, and reading it as zero would let a truncated
# capture satisfy the pattern with the easiest possible line.
absent_count="$(s_line ingress 0 disabled 0 | sed 's/cache_control_count=0 //')"
denies "baseline refuses a line with no cache_control_count token" \
    "$(stage baseline-no-count "$absent_count" "")" baseline "absent"

# --- baseline is ANTHROPIC-ONLY, scoped off the line's own id -----------
# A non-Anthropic client's floor request carries tools its runtime requires
# rather than tools a case permitted, so `tools_len == 0` describes a
# request that client cannot send. The scope is a NAMED refusal rather than
# a per-dialect tool-count floor: a floor keyed on a measured client tool
# count would pin that client's version into the predicate and lie at its
# next release. The accept legs above are the paired positive control --
# without them a predicate refusing every dialect would read as this scope.

for dialect in openai openai-responses; do
    other_dialect="$(s_line ingress 0 disabled 0 | \
        sed "s/id=\"anthropic\"/id=\"$dialect\"/")"
    denies "baseline refuses the $dialect ingress dialect by name" \
        "$(stage "baseline-$dialect" "$other_dialect" "")" baseline "$dialect"
    denies "the $dialect refusal names the Anthropic-only scope" \
        "$pat/baseline-$dialect" baseline "Anthropic-only"
    # Every other baseline clause is satisfied on that line, so a reason
    # blaming the tool count would mean the scope check never fired.
    out="$(python3 "$VERIFIER" "$pat/baseline-$dialect" baseline 2>&1)" || true
    if printf '%s' "$out" | grep -qF "tools_len"; then
        fail "the $dialect refusal blames a tool count instead of the scope: $out"
    else
        echo "PASS: the $dialect refusal does not blame a tool count"
    fi
done
unset other_dialect dialect

# An ABSENT id token is not the Anthropic one: a line naming no dialect
# cannot be scoped, and defaulting it would let any dialect's capture
# satisfy the pattern by omitting one field.
no_dialect="$(s_line ingress 0 disabled 0 | sed 's/id="anthropic" //')"
denies "baseline refuses a line carrying no ingress dialect token" \
    "$(stage baseline-no-dialect "$no_dialect" "")" baseline "id token absent"

# --- thinking -----------------------------------------------------------

exhibits "thinking accepts an enabled block with a budget" \
    "$(stage thinking-enabled "$(s_line ingress 0 enabled:31999 0)" "")" thinking
exhibits "thinking accepts an adaptive block" \
    "$(stage thinking-adaptive "$(s_line ingress 0 adaptive:high 0)" "")" thinking
denies "thinking refuses an explicitly disabled block" \
    "$(stage thinking-disabled "$(s_line ingress 0 disabled 0)" "")" thinking "disabled"
denies "thinking refuses an absent thinking token" \
    "$(stage thinking-none "$(s_line ingress 0 - 0)" "")" thinking "absent"

# --- cache-breakpoints --------------------------------------------------

exhibits "cache-breakpoints accepts a single breakpoint" \
    "$(stage cache-one "$(s_line ingress 0 disabled 1)" "")" cache-breakpoints
denies "cache-breakpoints refuses a line with no breakpoint" \
    "$(stage cache-zero "$(s_line ingress 0 disabled 0)" "")" cache-breakpoints "cache_control_count"

# --- the ingress line is selected by its direction token ----------------
# The outgoing summary carries the same fields with DIFFERENT values (the
# committed baseline fixture's outgoing line already reports two cache
# breakpoints its ingress line does not), so a predicate reading the wrong
# line answers a question about traffic the case does not control.

both_ingress_baseline="$(printf '%s\n%s\n' \
    "$(s_line outgoing 16 enabled:31999 3)" "$(s_line ingress 0 disabled 0)")"
exhibits "the predicate reads the ingress line past a non-baseline outgoing one" \
    "$(stage direction-ingress-good "$both_ingress_baseline" "")" baseline

both_ingress_tools="$(printf '%s\n%s\n' \
    "$(s_line outgoing 0 disabled 0)" "$(s_line ingress 16 disabled 0)")"
denies "the predicate does not satisfy itself from the outgoing line" \
    "$(stage direction-ingress-bad "$both_ingress_tools" "")" baseline "tools_len"

denies "a fixture whose structural file carries no ingress line is refused" \
    "$(stage direction-none "$(s_line outgoing 0 disabled 0)" "")" baseline "no direction"
# The needle is the UNREADABLE reason, not the filename: a predicate that
# read a missing file as an empty one would still refuse -- for having no
# ingress line -- and a filename-shaped needle would call that a pass.
denies "a fixture with no structural file is refused" \
    "$(stage no-structural "" '{}')" baseline "unreadable"

# --- token parsing is exact on the name, not a substring ----------------
# A substring search for `thinking_shape=` also matches
# `output_thinking_shape=`, which would let an unrelated field satisfy a
# clause about a missing one. Both directions of that mistake are asserted:
# the substring reading would reject this line as baseline AND accept it as
# thinking, so one leg alone would not catch it.

substring_line="$(s_line ingress 0 - 0 | \
    sed 's/output_config_effort=/output_thinking_shape=enabled:31999 output_config_effort=/')"
exhibits "an output_thinking_shape token does not disqualify a baseline line" \
    "$(stage exact-baseline "$substring_line" "")" baseline
denies "an output_thinking_shape token does not satisfy the thinking pattern" \
    "$(stage exact-thinking "$substring_line" "")" thinking "absent"

# --- tool-use-multiturn: an ingress body census ------------------------
# The census, not the offered tool list: the client offers its tools on
# every request once they are permitted, so a tools-array check is
# satisfied by the committed baseline fixture. Only a LATER turn replaying
# an earlier exchange puts a tool_use block and its tool_result on the wire.

tool_pair='{"tools":[{"name":"Read"}],"messages":[
 {"role":"user","content":[{"type":"text","text":"read it"}]},
 {"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Read","input":{}}]},
 {"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"7"}]},
 {"role":"user","content":[{"type":"text","text":"and the next one"}]}]}'
exhibits "tool-use-multiturn accepts a resent tool_use / tool_result pair" \
    "$(stage tools-pair "" "$tool_pair")" tool-use-multiturn

tools_only="$(printf '%s' "$tool_pair" | python3 -c '
import json, sys
body = json.load(sys.stdin)
body["messages"] = [t for t in body["messages"] if t["content"][0]["type"] == "text"]
json.dump(body, sys.stdout)
')"
denies "an offered tools array alone does not satisfy tool-use-multiturn" \
    "$(stage tools-array-only "" "$tools_only")" tool-use-multiturn "tool-call"

call_only="$(printf '%s' "$tool_pair" | python3 -c '
import json, sys
body = json.load(sys.stdin)
body["messages"] = [t for t in body["messages"] if t["content"][0]["type"] != "tool_result"]
json.dump(body, sys.stdout)
')"
denies "a tool call with no result turn does not satisfy tool-use-multiturn" \
    "$(stage tools-call-only "" "$call_only")" tool-use-multiturn "tool result"

# Ordering is a clause of its own: a result turn BEFORE the call it answers
# is not the resent pair, and a census that ignored order would accept it.
reversed_pair="$(printf '%s' "$tool_pair" | python3 -c '
import json, sys
body = json.load(sys.stdin)
turns = body["messages"]
turns[1], turns[2] = turns[2], turns[1]
json.dump(body, sys.stdout)
')"
denies "a tool result preceding its call does not satisfy tool-use-multiturn" \
    "$(stage tools-reversed "" "$reversed_pair")" tool-use-multiturn "no later turn"

# The needle is the ABSENCE reason, not the filename: a predicate that let
# a missing body through to the JSON read would still refuse -- for an
# unreadable file -- and a filename-shaped needle would call that a pass.
denies "a fixture with no ingress body is refused for tool-use-multiturn" \
    "$(stage tools-no-body "$(s_line ingress 0 disabled 0)" "")" \
    tool-use-multiturn "recorded no ingress body"
denies "a fixture with no ingress body is refused for large-context" \
    "$pat/tools-no-body" large-context "recorded no ingress body"

# --- the census answers for every ingress dialect ----------------------
# A Responses-shape body carries its turns under `input` and spells the
# pair as `function_call` / `function_call_output` ITEMS, which carry no
# role at all. Same census, same ordering clause, one predicate: the turn
# list is picked by which key the body carries, never by a recorded dialect
# claim -- `meta.ingress_kind` sits beside the `wire_pattern` claim this
# module checks, so reading one to decide the other verifies nothing.

responses_pair='{"tools":[{"type":"function","name":"read"}],"input":[
 {"type":"message","role":"user","content":[{"type":"input_text","text":"read it"}]},
 {"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"},
 {"type":"function_call_output","call_id":"call_1","output":"7"},
 {"type":"message","role":"user","content":[{"type":"input_text","text":"and the next one"}]}]}'
exhibits "tool-use-multiturn accepts a Responses function_call / output pair" \
    "$(stage responses-pair "" "$responses_pair")" tool-use-multiturn

# The chat-completions spelling of the same pair -- an assistant
# `tool_calls` array plus a `role: "tool"` turn -- under `messages`. The
# predicate has always read it; asserted here because the census now
# claims to answer for every ingress dialect.
chat_pair='{"tools":[{"type":"function","function":{"name":"read"}}],"messages":[
 {"role":"user","content":"read it"},
 {"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{}"}}]},
 {"role":"tool","tool_call_id":"call_1","content":"7"}]}'
exhibits "tool-use-multiturn accepts a chat-completions tool_calls / tool pair" \
    "$(stage chat-pair "" "$chat_pair")" tool-use-multiturn

responses_call_only="$(printf '%s' "$responses_pair" | python3 -c '
import json, sys
body = json.load(sys.stdin)
body["input"] = [i for i in body["input"] if i["type"] != "function_call_output"]
json.dump(body, sys.stdout)
')"
denies "a Responses call with no later output does not satisfy the census" \
    "$(stage responses-call-only "" "$responses_call_only")" \
    tool-use-multiturn "tool result"

# The ordering clause holds on the Responses shape too, and the refusal
# names the LIST it read: a census that reported `messages` here would be
# describing a body it never parsed.
responses_reversed="$(printf '%s' "$responses_pair" | python3 -c '
import json, sys
body = json.load(sys.stdin)
items = body["input"]
items[1], items[2] = items[2], items[1]
json.dump(body, sys.stdout)
')"
denies "a Responses output preceding its call does not satisfy the census" \
    "$(stage responses-reversed "" "$responses_reversed")" \
    tool-use-multiturn "no later turn"
denies "the Responses refusal names the input list it read" \
    "$pat/responses-reversed" tool-use-multiturn "input[2]"

# An offered Responses tool list alone is not the pattern, the same way an
# Anthropic `tools` array alone is not: the client offers its tools on
# every request once they are permitted.
responses_tools_only="$(printf '%s' "$responses_pair" | python3 -c '
import json, sys
body = json.load(sys.stdin)
body["input"] = [i for i in body["input"] if i["type"] == "message"]
json.dump(body, sys.stdout)
')"
denies "an offered Responses tool list alone does not satisfy the census" \
    "$(stage responses-tools-only "" "$responses_tools_only")" \
    tool-use-multiturn "tool-call"

# A body carrying NEITHER turn list is refused naming both keys it looked
# for. The turn list is what the census reads, so its absence is the wrong
# input rather than a body that fails the pattern.
denies "a body carrying neither turn list is refused naming both keys" \
    "$(stage no-turn-list "" '{"model":"gpt-5","tools":[{"name":"read"}]}')" \
    tool-use-multiturn "no turn list under any of messages, input"

# A body carrying BOTH turn-list keys is refused as AMBIGUOUS rather than
# resolved by precedence. No dialect emits two, so such a body is
# hand-edited or hybrid, and satisfying the claim from one list while the
# other contradicts it is the lie this gate exists to refuse. Both legs
# matter: the second proves an EMPTY sibling key still counts as present,
# which a presence check written as "non-empty" would miss.
both_keys_populated='{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1"}]}],"input":[{"type":"function_call","call_id":"c1","name":"read"}]}'
denies "a body carrying both turn lists is refused as ambiguous" \
    "$(stage both-turn-lists "" "$both_keys_populated")" \
    tool-use-multiturn "more than one turn list"
both_keys_one_empty='{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1"}]}],"input":[]}'
denies "an empty sibling turn-list key still counts as present" \
    "$(stage both-turn-lists-one-empty "" "$both_keys_one_empty")" \
    tool-use-multiturn "more than one turn list"

# An EMPTY turn list under either key is the same refusal: an empty list
# carries no census, and treating it as present would make the reason
# point at an ordering clause no turn could have failed.
for empty in '{"messages":[]}' '{"input":[]}'; do
    denies "an empty turn list is refused as no turn list ($empty)" \
        "$(stage "empty-turns-$(printf '%s' "$empty" | tr -dc 'a-z')" "" "$empty")" \
        tool-use-multiturn "no turn list"
done
unset empty

# --- large-context: an ingress body byte floor -------------------------

floor="$(python3 -c '
import sys
sys.path.insert(0, sys.argv[1])
import verify_pattern
print(verify_pattern.MIN_LARGE_CONTEXT_BYTES)
' "$DRIVERS/lib")"
check "the large-context floor is stated as a named constant" "1" \
    "$(grep -c '^MIN_LARGE_CONTEXT_BYTES' "$VERIFIER")"

python3 -c '
import json, os, sys
floor = int(sys.argv[2])
for name, size in (("large-over", floor + 4096), ("large-under", floor - 1)):
    directory = os.path.join(sys.argv[1], name)
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, "ingress_request.json")
    body = {"messages": [{"role": "user", "content": "x"}]}
    text = json.dumps(body)
    body["messages"][0]["content"] = "x" * (size - len(text) + 1)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(json.dumps(body))
    assert os.path.getsize(path) == size, (name, os.path.getsize(path), size)
' "$pat" "$floor"
exhibits "large-context accepts a body above the floor" "$pat/large-over" large-context
denies "large-context refuses a body one byte under the floor" \
    "$pat/large-under" large-context "floor"

# A byte count is not a shape. These two are OVER the floor, so a predicate
# that only measured the file would promote both -- a truncated capture and
# a binary blob, each claiming a wire shape neither carries.
python3 -c '
import json, os, sys
floor = int(sys.argv[2])
valid = json.dumps({"messages": [{"role": "user", "content": "x" * (floor + 4096)}]})
for name, payload in (
    # A capture cut off mid-write: valid UTF-8, unparseable JSON.
    ("large-truncated", valid[: floor + 2048].encode("utf-8")),
    # Not text at all, which fails one step earlier than the JSON parse.
    ("large-binary", b"\xff\xfe" + b"\x00\x01" * (floor // 2)),
):
    directory = os.path.join(sys.argv[1], name)
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, "ingress_request.json")
    with open(path, "wb") as handle:
        handle.write(payload)
    size = os.path.getsize(path)
    assert size >= floor, (name, size, floor)
' "$pat" "$floor"
denies "large-context refuses an oversized body that is not valid JSON" \
    "$pat/large-truncated" large-context "not valid JSON"
denies "large-context refuses an oversized body that is not valid UTF-8" \
    "$pat/large-binary" large-context "not valid UTF-8"

# A body carrying ONLY the case's prompt plus the client's own preamble is
# under the floor. That is what a large-context case produces when nothing
# draws the padding INTO the body -- the shape a counting prompt yielded,
# measured at 121-125 KB against the 262144 floor, and the reason the case
# now asks for the filler to be quoted rather than counted. Kept as the
# paired control for the floor: it fixes what "not large" looks like from
# this client, so the floor cannot be lowered to meet it. Built from the
# committed baseline fixture's real captured body plus the case's own
# first-turn prompt.
lc_prompt="$(python3 "$VALIDATOR" --turns "$CASES/large-context-01.json" | head -1)"
lc_dir="$pat/large-context-01-shape"
mkdir -p "$lc_dir"
python3 - "$ROOT/crates/routectl-cli/tests/fixtures/driver/anthropic-api/plain-turn-01" \
         "$lc_dir/ingress_request.json" "$lc_prompt" <<'PY'
import json, os, sys
source = os.path.join(sys.argv[1], "ingress_request.json")
if os.path.isfile(source):
    with open(source, encoding="utf-8") as handle:
        body = json.load(handle)
else:
    # No driver corpus in this checkout. The preamble size a real first
    # request carries is the point of the leg, so stand in an explicit
    # figure rather than skipping: the committed baseline fixture measured
    # ~28 KB, and any value in that range is under the floor.
    body = {"system": [{"type": "text", "text": "p" * 28000}], "messages": []}
body["messages"] = [{"role": "user", "content": [{"type": "text", "text": sys.argv[3]}]}]
with open(sys.argv[2], "w", encoding="utf-8") as handle:
    json.dump(body, handle)
PY
denies "a body carrying only the prompt and the client preamble is refused" \
    "$lc_dir" large-context "floor"

# --- the table covers the closed set ----------------------------------
# Every token in WIRE_PATTERNS resolves to a predicate. The "no predicate"
# refusal is what a missing entry produces, so its absence here is the
# assertion -- and its PRESENCE for the deferred token is the paired
# control proving the check can fire at all.

uncovered=0
while IFS= read -r token; do
    out="$(python3 "$VERIFIER" "$pat/baseline-disabled" "$token" 2>&1)" || true
    if printf '%s' "$out" | grep -qF "no predicate"; then
        uncovered=$((uncovered + 1))
        echo "  uncovered wire pattern: $token"
    fi
done < <(python3 -c '
import sys
sys.path.insert(0, sys.argv[1])
import validate_case
print("\n".join(sorted(validate_case.WIRE_PATTERNS)))
' "$DRIVERS/lib")
check "every wire pattern in the closed set has a predicate" "0" "$uncovered"

# --- mcp-tools: a namespaced tool name, never a count -------------------
# A tools-enabled request carries its built-ins on every request once tools
# are permitted at all, and the declaration block alone runs well over
# 100KB before any content -- so neither a bare tools-array presence nor a
# body-size check is evidence of an MCP server. The one shape that can only
# come from `--mcp-config`, the offered name being server-namespaced
# (`mcp__<server>__<tool>`), is what the predicate keys on.

mcp_pair='{"tools":[{"name":"Bash"},{"name":"mcp__fixture__add"}],"messages":[
 {"role":"user","content":[{"type":"text","text":"add them"}]}]}'
exhibits "mcp-tools accepts a body offering a server-namespaced tool name" \
    "$(stage mcp-namespaced "" "$mcp_pair")" mcp-tools

# MANDATORY PAIRED CONTROL: a body whose tools are ALL built-ins, offered
# under the exact same tools-enabled shape, must fail the same claim -- the
# positive leg above proves nothing about the predicate's selectivity
# without this one.
builtins_only='{"tools":[{"name":"Bash"},{"name":"Read"},{"name":"str_replace_editor"}],"messages":[
 {"role":"user","content":[{"type":"text","text":"add them"}]}]}'
denies "mcp-tools refuses a body whose tools are all built-ins" \
    "$(stage mcp-builtins-only "" "$builtins_only")" mcp-tools "no offered tool name is server-namespaced"

no_tools='{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}'
denies "mcp-tools refuses a body that offers no tools at all" \
    "$(stage mcp-no-tools "" "$no_tools")" mcp-tools "offers no tools"

denies "a fixture with no ingress body is refused for mcp-tools" \
    "$pat/tools-no-body" mcp-tools "recorded no ingress body"

# --- the shared classification set ------------------------------------
# The structural lines the three structural predicates are asserted
# against, so an implementation of them cannot drift unnoticed from what a
# shape means. Both sides read the set: the Python predicates here, and the
# Rust reference logic in the routectl-cli test suite.

# The header is the comment block ahead of the first record, read as such
# rather than as a fixed line count: a needle pinned to `head -N` passes or
# fails on where the sentence sits rather than on whether it is there.
class_header="$(sed -n '/^[^#]/q;p' "$CLASSIFICATION")"
if printf '%s' "$class_header" | grep -qF "baseline, thinking,"; then
    echo "PASS: the classification set states its three-predicate scope"
else
    fail "the classification set header does not state which predicates it covers"
fi
# The header must NAME both consumers. A reader who cannot tell which side
# reads the file cannot tell a checked divergence from an undetected one,
# and the Rust half is the only thing that makes this set a cross-check
# rather than a Python self-test.
if printf '%s' "$class_header" | grep -qF "wire_pattern_weld.rs"; then
    echo "PASS: the classification set header names its Rust consumer"
else
    fail "the classification set header does not name the Rust consumer"
fi
if printf '%s' "$class_header" | grep -qF "drivers.test.sh"; then
    echo "PASS: the classification set header names its Python consumer"
else
    fail "the classification set header does not name the Python consumer"
fi

class_records=0
class_wrong=0
while IFS=$'\t' read -r yes no line; do
    case "$yes" in ''|'#'*) continue ;; esac
    class_records=$((class_records + 1))
    if [ "$yes" = "-" ] && [ "$no" = "-" ]; then
        class_wrong=$((class_wrong + 1))
        echo "  record asserts nothing: $line"
        continue
    fi
    for pattern in ${yes//,/ }; do
        [ "$pattern" = "-" ] && continue
        if ! printf '%s' "$line" | python3 "$VERIFIER" --structural-line "$pattern" 2>/dev/null; then
            class_wrong=$((class_wrong + 1))
            echo "  record claims $pattern but the predicate refuses it: $line"
        fi
    done
    for pattern in ${no//,/ }; do
        [ "$pattern" = "-" ] && continue
        if printf '%s' "$line" | python3 "$VERIFIER" --structural-line "$pattern" 2>/dev/null; then
            class_wrong=$((class_wrong + 1))
            echo "  record denies $pattern but the predicate accepts it: $line"
        fi
    done
done <"$CLASSIFICATION"
check "every classification record matches the predicate that reads it" "0" "$class_wrong"
if [ "$class_records" -ge 8 ]; then
    echo "PASS: the classification set holds $class_records records"
else
    fail "the classification set holds only $class_records records"
fi

# The set is scoped to the structural predicates, and the mode that reads
# it refuses a body-census pattern rather than answering "no" -- a census
# question a line cannot decide is the wrong question, not a rejection.
for pattern in tool-use-multiturn large-context mcp-tools; do
    if printf '%s' "$(s_line ingress 0 disabled 0)" | \
        python3 "$VERIFIER" --structural-line "$pattern" 2>/dev/null; then
        fail "a structural line satisfied the body-census pattern $pattern"
    else
        echo "PASS: a structural line cannot classify the $pattern census"
    fi
done

rm -rf "$pat"

# ---------------------------------------------------------------------
# Part 1c: the ingress-dialect vocabulary
# ---------------------------------------------------------------------
# The closed set the expected-ingress pin is validated against, at all
# three enforcement points. It is a REPLICA of what `IngressAdapter::id()`
# returns, because the rig runs in throwaway trees that carry scripts/ and
# no crates/ -- so a weld to the real source is the only thing keeping the
# copy honest. A drifted replica refuses a dialect this build parses, or
# accepts one it does not.

mapfile -t DECLARED_INGRESS_KINDS < <(
    sed -n '/^# --- BEGIN INGRESS_KINDS ---$/,/^# --- END INGRESS_KINDS ---$/p' \
        "$INGRESS_KINDS" | sed -n 's/^ *"\([^"]*\)" *$/\1/p'
)
check "the vocabulary parses to a non-empty set" "1" \
    "$([ "${#DECLARED_INGRESS_KINDS[@]}" -gt 0 ] && echo 1 || echo 0)"

# The weld. Guarded on the crates tree's presence -- this suite also runs
# from a scripts-only checkout, where there is nothing to weld to.
ingress_src_dir="$ROOT/crates/routectl-cli/src/ingress"
if [ -d "$ingress_src_dir" ]; then
    real_ingress_kinds="$(grep -rhA1 -- "fn id(&self) -> &'static str {" "$ingress_src_dir" |
        sed -n 's/^ *"\([a-z0-9-]\+\)" *$/\1/p' | sort -u | tr '\n' ' ')"
    check "the source-derived ingress set is non-empty" "1" \
        "$([ -n "$real_ingress_kinds" ] && echo 1 || echo 0)"
    check "the declared vocabulary equals the adapters' own id() set" \
        "$real_ingress_kinds" \
        "$(printf '%s\n' "${DECLARED_INGRESS_KINDS[@]}" | sort -u | tr '\n' ' ')"
    unset real_ingress_kinds
else
    echo "PASS: no crates tree in this checkout; the ingress vocabulary weld is not asserted"
fi
unset ingress_src_dir

# The membership predicate, exercised through the library itself so the
# assertions are about the code the three scripts call and not about a
# re-implementation. The EMPTY string is deliberately not a member: empty
# means "the capture did not observe the dialect" everywhere else in the
# schema, and a pin is a statement about what a run expects -- so treating
# it as a wildcard would make the whole gate satisfiable by an unobserved
# capture.
(
    # shellcheck source=scripts/drivers/lib/ingress_kinds.sh
    . "$INGRESS_KINDS"
    fails=0
    for kind in "${DECLARED_INGRESS_KINDS[@]}"; do
        if ingress_kind_is_known "$kind"; then
            echo "PASS: '$kind' is recognized as a vocabulary member"
        else
            echo "FAIL: declared member '$kind' is not recognized by the predicate"
            fails=$((fails + 1))
        fi
        if printf '%s' "$(ingress_kinds_list)" | grep -qF -- "$kind"; then
            echo "PASS: the refusal list names '$kind'"
        else
            echo "FAIL: the refusal list omits declared member '$kind'"
            fails=$((fails + 1))
        fi
    done
    # `anthropic-api` is the trap spelling: a real token in the LANE
    # vocabulary and in no ingress one, so a predicate keyed on a substring
    # or on the lane set would accept it.
    for reject in "" "anthropic-api" "openai-compat" "bedrock" "ANTHROPIC"; do
        if ingress_kind_is_known "$reject"; then
            echo "FAIL: '$reject' was accepted as an ingress dialect"
            fails=$((fails + 1))
        else
            echo "PASS: '${reject:-<empty>}' is refused as an ingress dialect"
        fi
    done
    exit "$fails"
) || fails=$((fails + $?))
# ---------------------------------------------------------------------
# Part 1d: the client-version comparator
# ---------------------------------------------------------------------
# A fixture carries TWO statements of one client's version: the wire value
# parsed from the client-controlled `user-agent`, and the binary-side value
# the driver read off the running client. This is the code that decides
# whether they agree, and the promotion gates refuse a DISAGREEMENT.
#
# Every leg drives the comparator directly on a pair of strings, so a
# verdict can only be attributed to the pair -- a fixture-level assertion
# alone could not distinguish a real comparison from one that always agrees.

# Exit codes, as the comparator's own docstring defines them. Named here
# because three distinct non-zero verdicts is exactly the distinction a
# blanket non-zero assertion would erase.
CV_AGREE=0
CV_DISAGREE=1
CV_NOT_COMPARABLE=3

cv_verdict() {
    local rc=0
    python3 "$CLIENT_VERSION" --compare "$1" "$2" >/dev/null 2>&1 || rc=$?
    printf '%s\n' "$rc"
}

cv_check() {
    local label="$1" expected="$2" binary="$3" wire="$4"
    check "$label" "$expected" "$(cv_verdict "$binary" "$wire")"
}

# The REAL spellings, which differ by construction: a binary prints a human
# line, the rig extracts a bare token out of `claude-cli/<v> (external,
# cli)`. A string comparison would call this pair a disagreement and refuse
# every genuine capture, so this leg is what pins the comparison to tokens.
cv_check "the real binary line and the real wire token agree" \
    "$CV_AGREE" "2.1.246 (Claude Code)" "2.1.246"
cv_check "a differently-decorated binary line still agrees" \
    "$CV_AGREE" "codex-cli 0.151.0" "0.151.0"
cv_check "identical bare tokens agree" "$CV_AGREE" "2.1.246" "2.1.246"

# The defect the gate exists for: a client that auto-updated mid-run reports
# one version off the binary and another on the wire.
cv_check "a mid-run update is a DISAGREEMENT" \
    "$CV_DISAGREE" "2.1.247 (Claude Code)" "2.1.246"
cv_check "a major-version difference is a DISAGREEMENT" \
    "$CV_DISAGREE" "3.0.0" "2.1.246"

# A version-shaped token must not be satisfied by a coincidental substring
# on either side, in both directions.
cv_check "a longer token is not matched by a shorter prefix of it" \
    "$CV_DISAGREE" "2.1.24" "2.1.246"
cv_check "a shorter token is not matched by a longer one containing it" \
    "$CV_DISAGREE" "2.1.246" "2.1.24"

# Absence is NOT COMPARABLE, and the two sides are checked separately: a
# comparator that only handled one empty side would read the other as a
# disagreement and refuse a live-box capture.
cv_check "an absent binary version is not comparable" \
    "$CV_NOT_COMPARABLE" "" "2.1.246"
cv_check "an absent wire version is not comparable" \
    "$CV_NOT_COMPARABLE" "2.1.246 (Claude Code)" ""
cv_check "both absent is not comparable" "$CV_NOT_COMPARABLE" "" ""

# A version line that carries no dotted-numeric token has said nothing to
# contradict, so it is not comparable rather than a disagreement. Refusing
# it would refuse the client instead of the contradiction.
cv_check "a word-only version line is not comparable" \
    "$CV_NOT_COMPARABLE" "development build" "2.1.246"
cv_check "a lone major is not read as a version token" \
    "$CV_NOT_COMPARABLE" "7" "2.1.246"

# A prerelease suffix reduces the same way on both sides, because the
# reduction is one function applied twice.
cv_check "a prerelease suffix does not manufacture a disagreement" \
    "$CV_AGREE" "2.1.246-beta.1 (Claude Code)" "2.1.246-beta.1"

# A DISAGREEMENT must name both readings: a refusal that printed neither
# sends whoever hit it back to the fixture to find out what disagreed.
cv_reason="$(python3 "$CLIENT_VERSION" --compare "2.1.247" "2.1.246" 2>&1 || true)"
for needle in 2.1.247 2.1.246; do
    if printf '%s' "$cv_reason" | grep -qF -- "$needle"; then
        echo "PASS: the disagreement names the reading '$needle'"
    else
        fail "the disagreement did not name the reading '$needle': $cv_reason"
    fi
done

# A usage error is its OWN code, distinct from every verdict above: a caller
# that invoked the comparator wrong must not read the answer as agreement.
cv_usage_rc=0
python3 "$CLIENT_VERSION" 2.1.246 2.1.246 >/dev/null 2>&1 || cv_usage_rc=$?
check "a missing --compare is a usage error, not a verdict" "2" "$cv_usage_rc"

# ---------------------------------------------------------------------
# Part 1e: the stub MCP server
# ---------------------------------------------------------------------
# stub_mcp.py is what makes the mcp-tools wire pattern an image-change-free
# addition: it must stay stdlib-only, offline, and byte-for-byte
# deterministic, or a capture built on it would carry the same evidence
# problems this suite exists to keep out of the corpus.

STUB_MCP="$DRIVERS/stub_mcp.py"

stray_imports="$(grep -E '^(import|from) ' "$STUB_MCP" | grep -vE '^import (json|sys)$' || true)"
if [ -z "$stray_imports" ]; then
    echo "PASS: stub_mcp.py imports only the standard library"
else
    fail "stub_mcp.py imports something beyond json/sys: $stray_imports"
fi

if grep -qE 'socket|urllib|http\.client|requests' "$STUB_MCP"; then
    fail "stub_mcp.py references a networking module by name"
else
    echo "PASS: stub_mcp.py names no networking module"
fi

# One JSON-RPC line per request, fed and drained over a real pipe -- the
# actual transport a client uses, not a function call into the module.
mcp_probe() {
    python3 - "$STUB_MCP" <<'PY'
import json
import subprocess
import sys

proc = subprocess.Popen(
    [sys.executable, sys.argv[1]],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    text=True,
)


def send(msg):
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()


def recv():
    return json.loads(proc.stdout.readline())


send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
init = recv()
send({"jsonrpc": "2.0", "method": "notifications/initialized"})
send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
listed = recv()
send(
    {
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "add", "arguments": {"a": 17, "b": 25}},
    }
)
called = recv()
proc.stdin.close()
rc = proc.wait(timeout=5)

print(json.dumps({"init": init, "listed": listed, "called": called, "rc": rc}))
PY
}

mcp_run_a="$(mcp_probe)"
mcp_run_b="$(mcp_probe)"
check "two independent stub MCP runs answer byte-for-byte identically" \
    "$mcp_run_a" "$mcp_run_b"

if [ -z "$mcp_run_a" ]; then
    fail "the stub MCP server produced no output to check"
else
    if printf '%s' "$mcp_run_a" | grep -qF '"protocolVersion"'; then
        echo "PASS: initialize answers with a protocol version"
    else
        fail "initialize did not answer with a protocol version"
    fi
    offered="$(printf '%s' "$mcp_run_a" | python3 -c '
import json, sys
doc = json.load(sys.stdin)
names = sorted(t["name"] for t in doc["listed"]["result"]["tools"])
print(",".join(names))
')"
    check "tools/list offers exactly the two static tools" "add,echo" "$offered"
    sum_text="$(printf '%s' "$mcp_run_a" | python3 -c '
import json, sys
doc = json.load(sys.stdin)
print(doc["called"]["result"]["content"][0]["text"])
')"
    check "tools/call add(17, 25) answers deterministically" "42" "$sum_text"
    exit_code="$(printf '%s' "$mcp_run_a" | python3 -c '
import json, sys
print(json.load(sys.stdin)["rc"])
')"
    check "the stub exits on its own once the client closes stdin" "0" "$exit_code"
fi

# ---------------------------------------------------------------------
# Part 2: driver hygiene, asserted on the committed files
# ---------------------------------------------------------------------

for driver in "${DRIVER_FILES[@]}" "$DRIVERS/lib/common.sh"; do
    if bash -n "$driver" 2>/dev/null; then
        echo "PASS: $(basename "$driver") parses"
    else
        fail "$(basename "$driver") is not syntactically valid bash"
        bash -n "$driver"
    fi
done

# The script's CODE, comments stripped: the drivers' own headers explain at
# length why they must NOT dispatch on a harness, so a whole-file grep
# would fire on the explanation.
code_lines() {
    sed 's/#.*$//' "$1"
}

# ONE FILE PER HARNESS. A dispatch statement is the drift surface the
# layout exists to avoid, and it cannot degrade an undrivable harness to
# "no file" -- a dead branch still reads as coverage.
dispatch_re='case[[:space:]]+"?\$\{?(harness|HARNESS|client|CLIENT)'
for driver in "${DRIVER_FILES[@]}" "$DRIVERS/lib/common.sh"; do
    if grep -qE "$dispatch_re" <(code_lines "$driver"); then
        fail "$(basename "$driver") dispatches on a harness variable"
    else
        echo "PASS: $(basename "$driver") carries no harness dispatch"
    fi
done

# No real home path in tracked driver or case content. A driven client
# reads its cwd back into request bodies, and the scrub gate refuses a
# fixture carrying one -- but a path baked into a committed script would
# be refused at the landing gate on every future run instead of here.
home_re='/home/[a-z]'
home_hits=0
while IFS= read -r hit; do
    case "$hit" in
        *"/home/user"*) ;;
        *) home_hits=$((home_hits + 1)); echo "  offending line: $hit" ;;
    esac
done < <(grep -rnE "$home_re" "$DRIVERS" 2>/dev/null)
check "no driver or case references a real home path" "0" "$home_hits"

# No curl-based synthetic stand-in for a client. A hand-rolled request
# proves nothing about a CLIENT's wire shape, which is the whole point of
# a driver corpus -- and the drivers legitimately curl /health, so the
# assertion is about a POSTED body, not about curl.
if grep -qE 'curl[^|]*(-X[[:space:]]*POST|--data|-d[[:space:]])' \
    <(cat <(code_lines "${DRIVER_FILES[0]}") \
          <(code_lines "${DRIVER_FILES[1]}") \
          <(code_lines "${DRIVER_FILES[2]}") \
          <(code_lines "$DRIVERS/lib/common.sh")); then
    fail "a driver posts a hand-built request instead of driving a client"
else
    echo "PASS: no driver posts a synthetic stand-in request"
fi

# Positive control for the three greps above. Without it a broken pattern
# or a mistyped path would read as three clean passes.
control="$(mktemp)"
printf 'case "$harness" in claude) ;; esac\ncd /home/someone/work\ncurl -X POST http://x -d @body.json\n{"command": "run-me"}\n' >"$control"
control_hits=0
for pattern in "$dispatch_re" "$home_re" 'curl[^|]*(-X[[:space:]]*POST|--data|-d[[:space:]])' "$invocation_re"; do
    if grep -qE "$pattern" "$control"; then
        control_hits=$((control_hits + 1))
    fi
done
check "the hygiene greps fire on a file that does contain them" "4" "$control_hits"
rm -f "$control"

# The runner's own live-daemon guards apply to a driver too: a driver runs
# on a box where a real routectl serves the operator's own traffic.
for driver in "${DRIVER_FILES[@]}" "$DRIVERS/lib/common.sh"; do
    if grep -qE 'pkill|killall|9100|usage\.db' <(code_lines "$driver"); then
        fail "$(basename "$driver") names the live daemon or kills by name"
    else
        echo "PASS: $(basename "$driver") names nothing the live daemon owns"
    fi
done

# NO DRIVER READS THE EXPECTED-INGRESS PIN, and that is the layering, not
# an omission. The pin belongs to the (driver, lane) pairing the CALLER
# chose; the rig is the enforcer, and it reads the value from the same
# environment. A driver that consulted it could only decide whether to
# refuse a run it has no other reason to doubt -- further from the traced
# evidence than the rig already is -- and a driver that BRANCHED on it
# would be the harness dispatch this layout exists to forbid, wearing a
# different variable name.
for driver in "${DRIVER_FILES[@]}" "$DRIVERS/lib/common.sh"; do
    if grep -qF 'ROUTECTL_FIXTURE_EXPECTED_INGRESS' <(code_lines "$driver"); then
        fail "$(basename "$driver") reads the expected-ingress pin; the rig owns that check"
    else
        echo "PASS: $(basename "$driver") leaves the expected-ingress check to the rig"
    fi
done

# Positive control for that grep: it must fire on a file that DOES read the
# pin, or four clean passes prove nothing. The rig is the real such file.
if grep -qF 'ROUTECTL_FIXTURE_EXPECTED_INGRESS' <(code_lines "$RIG"); then
    echo "PASS: the expected-ingress grep fires on the rig, which does read the pin"
else
    fail "the expected-ingress grep matches nothing in the rig, so its absence proves nothing"
fi
# EVERY driver brakes the client's auto-updater. A client that updates
# itself mid-run moves the one value the corpus reads as its decay clock,
# and it lands a capture whose binary-side and wire versions disagree --
# which the promotion gate refuses, so an unbraked driver produces
# unpromotable fixtures rather than merely noisy ones. Asserted over the
# whole driver set rather than per file: a per-file check is a special case
# of this, and it would keep passing while a fourth driver shipped with no
# brake, which is exactly how the gap this closes was introduced.
for driver in "${DRIVER_FILES[@]}"; do
    if grep -qE '^[[:space:]]*export[[:space:]]+DISABLE_AUTOUPDATER=' \
        <(code_lines "$driver"); then
        echo "PASS: $(basename "$driver") exports the updater brake"
    else
        fail "$(basename "$driver") exports no DISABLE_AUTOUPDATER; its client can update mid-run"
    fi
done

# The run record's basename is spelled in TWO files: the driver library
# writes it, the runner reads `version=` back out of it after the driver
# exits. Neither sources the other, so a drift in the spelling would leave
# the runner reading a file nobody writes -- silently forwarding an empty
# binary-side version and restoring the unchecked-user-agent state.
lib_record="$(sed -n 's/^DRIVER_CLIENT_RECORD="\(.*\)"$/\1/p' "$DRIVERS/lib/common.sh")"
runner_record="$(sed -n 's|^CLIENT_RECORD="\$RUN/\(.*\)"$|\1|p' "$ROOT/scripts/capture_driver.sh")"
check "the run record's basename is non-empty in the driver library" \
    "1" "$([ -n "$lib_record" ] && echo 1 || echo 0)"
check "the runner reads the same run-record basename the library writes" \
    "$lib_record" "$runner_record"

# ---------------------------------------------------------------------
# Fixtures for the end-to-end cases
# ---------------------------------------------------------------------

# The canned trace the stub daemon emits on stderr: a complete non-stream
# request carrying BOTH request-side structural summaries, which driver
# mode requires before it will land a fixture. The user-agent is what the
# rig parses `meta.client.version` out of, so the version this trace
# carries is the one a landed fixture must report. Every value is
# synthetic.
#
# `kind` and `id` differ per direction, as the emitter writes them: the
# ingress call site passes the ingress dialect token, the outgoing one the
# provider kind and configured provider id. The `baseline` predicate scopes
# itself to the Anthropic dialect off the ingress line's `id`, so reusing
# the outgoing spelling on both lines would be refused.
canned_trace() {
    local id="019eab77-0000-4000-8000-0000000000e2"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    local fields='model=claude-sonnet-4-5 max_tokens=64 thinking_shape=disabled output_config_effort= tool_choice_shape= cache_control_count=0 messages_len=2 tools_len=0 anthropic_beta= provider_extras_keys= stream=false'
    local structural="kind=\"ingress\" id=\"anthropic\" $fields"
    local structural_out="kind=\"anthropic\" id=\"anthropic-api:anthropic\" $fields"
    cat <<TRACE
2026-08-26T10:00:00.000000Z TRACE $span:messages{ingress="anthropic"}: $target ingress request body ingress="anthropic" body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-26T10:00:00.100000Z TRACE $span:complete_with_options{alias=my-alias}:complete{provider=anthropic:p model=claude-sonnet-4-5}: $target outgoing request body provider_kind="anthropic" provider=p body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-26T10:00:00.200000Z TRACE $span: $target upstream success body provider_kind="anthropic" provider=p body={"id":"msg_1"} redact_prompts_enabled=false
2026-08-26T10:00:00.300000Z TRACE $span: $target egress response body ingress="anthropic" body={"id":"msg_1"} redact_prompts_enabled=false
2026-08-26T10:00:00.010000Z TRACE $span: $target ingress request headers direction="ingress" headers=[["user-agent","claude-cli/9.9.9 (external, cli)"],["content-type","application/json"]]
2026-08-26T10:00:00.110000Z TRACE $span: $target outgoing request headers direction="outgoing" headers=[["content-type","application/json"]]
2026-08-26T10:00:00.210000Z TRACE $span: $target upstream response headers direction="upstream" headers=[["content-type","application/json"]]
2026-08-26T10:00:00.310000Z TRACE $span: $target egress response headers direction="egress" headers=[["content-type","application/json"]]
2026-08-26T10:00:00.400000Z TRACE $span: $target structural summary direction="ingress" $structural
2026-08-26T10:00:00.500000Z TRACE $span: $target structural summary direction="outgoing" $structural_out
TRACE
}

# A canned trace whose ingress body carries a tool-call turn AND a later
# turn carrying its result, with a tools array on the structural line: the
# shape a `tool-use-multiturn` case claims, which the rig's promotion gate
# now verifies against the recorded claim.
canned_trace_tools() {
    local body='{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"list the files"},{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"ls"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"notes.txt"}]}]}'
    canned_trace |
        sed "s|ingress request body ingress=\"anthropic\" body={\"model\":\"claude-sonnet-4-5\"}|ingress request body ingress=\"anthropic\" body=$body|" |
        sed 's/tools_len=0/tools_len=16/'
}

# A canned trace whose ingress structural line carries an ACTIVE thinking
# block: the shape a `thinking` case claims.
canned_trace_thinking() {
    canned_trace | sed 's/thinking_shape=disabled/thinking_shape=enabled:8192/'
}

# A canned trace whose ingress body offers a server-namespaced tool name
# alongside a built-in: the shape an `mcp-tools` case claims. The stub
# client used by driver_run never actually talks to an MCP server, so
# without this the rig's promotion gate would refuse every mcp-tools run
# regardless of what the driver wired up on argv.
canned_trace_mcp_tools() {
    local body='{"model":"claude-sonnet-4-5","tools":[{"name":"Bash"},{"name":"mcp__fixture__add"}],"messages":[{"role":"user","content":"add them"}]}'
    canned_trace |
        sed "s|ingress request body ingress=\"anthropic\" body={\"model\":\"claude-sonnet-4-5\"}|ingress request body ingress=\"anthropic\" body=$body|" |
        sed 's/tools_len=0/tools_len=2/'
}

# A canned trace whose ingress structural line carries cache breakpoints:
# the shape a `cache-breakpoints` case claims.
canned_trace_cache_breakpoints() {
    canned_trace | sed 's/cache_control_count=0/cache_control_count=2/'
}

# The MITM seam header name, read out of the rig rather than restated so
# the two spellings cannot drift. The rig refuses to promote a front-proxy
# fixture whose captured ingress headers do not carry it -- an environment
# carrier states intent, this header is the evidence of transit.
SEAM_HEADER="$(sed -n 's/^MITM_SEAM_HEADER="\(.*\)"$/\1/p' "$RIG")"

# Add the seam header to a canned trace's captured ingress headers, so a
# front-proxy run in this suite lands the fixture it drove.
with_seam_header() {
    sed 's/\(ingress request headers direction="ingress" headers=\[\)/\1["'"$SEAM_HEADER"'","d41d8cd98f00b204e9800998ecf8427e"],/'
}

write_listener() {
    cat >"$1" <<'PY'
import http.server
import socketserver
import sys


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            body = b'{"status":"ok","version":"stub"}'
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404)
            self.send_header("content-length", "0")
            self.end_headers()

    def log_message(self, *args):
        pass


socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("127.0.0.1", int(sys.argv[1])), Handler) as srv:
    srv.serve_forever()
PY
}

# The stub `routectl`, same shape as the runner's own self-test uses:
# records its pid, emits the canned trace on stderr, then EXECs the
# listener so the pid the runner captured stays the pid holding the port.
# When the runner passes `--mitm-port` (front-proxy mode), the stub emits
# the structured MITM listening line the runner's readiness gate anchors
# on (with the ANSI escapes the real trace carries) and mints the CA
# under the run's XDG root exactly where the real daemon does at listener
# start, so the CA path the runner exports names a readable file.
write_stub_routectl() {
    cat >"$1" <<'SH'
#!/usr/bin/env bash
set -u
port=""
mitm_port=""
while [ $# -gt 0 ]; do
    case "$1" in
        --port) port="$2"; shift 2 ;;
        --mitm-port) mitm_port="$2"; shift 2 ;;
        *) shift ;;
    esac
done
printf '%s\n' "$$" >"$STUB_PID_FILE"
if [ -r "${STUB_TRACE_FILE:-}" ]; then
    cat "$STUB_TRACE_FILE" >&2
fi
if [ -n "$mitm_port" ]; then
    printf '\033[2m2026-08-25T10:00:00.050000Z\033[0m \033[32m INFO\033[0m \033[2mroutectl_cli::server::serve\033[0m\033[2m:\033[0m MITM front-proxy listening \033[3maddr\033[0m\033[2m=\033[0m127.0.0.1:%s \033[3mmitm_host\033[0m\033[2m=\033[0mapi.anthropic.com\n' \
        "$mitm_port" >&2
    mkdir -p "$XDG_CONFIG_HOME/routectl/mitm-certs/current"
    printf -- '-----BEGIN CERTIFICATE-----\nc3R1Yg==\n-----END CERTIFICATE-----\n' \
        >"$XDG_CONFIG_HOME/routectl/mitm-certs/current/mitm-ca-cert.pem"
fi
exec python3 "$STUB_LISTENER" "$port"
SH
    chmod +x "$1"
}

# The stub CLIENT. Dumps its argv and the environment that decides which
# wire shape a real client would emit, APPENDING to a file outside the run
# workspace (the runner removes that workspace on exit, and a multi-turn
# driver invokes the client more than once).
#
# IT DRAINS STDIN, and that is load-bearing rather than incidental. The
# real clients read stdin; a driver whose turn loop lets the client inherit
# the loop's own stdin has the client swallow the remaining prompts, and a
# 2-turn case then runs ONE turn with every prompt concatenated. A stub
# that never read stdin could not exhibit that, so the "once per turn"
# assertion below passed against a stub incapable of failing it -- a
# vacuous negative. The drained byte count is recorded per invocation so
# the assertion can also see WHETHER anything was there to drain.
#
# `STUB_CLIENT_RC` makes it fail on demand, which is how the driver's own
# failure propagation is asserted. `STUB_CLIENT_VERSION` empty is how a
# client that cannot state its version is asserted. `STUB_CLIENT_LEAK_MCP`,
# when set to a needle, spawns a background process whose cmdline carries
# that needle and does NOT wait for it -- modeling a client that leaks its
# MCP stdio child instead of letting it die on exit, which is the one
# misbehavior the driver's post-run leak check exists to catch.
write_stub_client() {
    cat >"$1" <<'SH'
#!/usr/bin/env bash
set -u
for arg in "$@"; do
    if [ "$arg" = "--version" ] || [ "$arg" = "-V" ]; then
        printf '%s\n' "${STUB_CLIENT_VERSION-9.9.9 (Stub Client)}"
        exit 0
    fi
done
if [ -n "${STUB_CLIENT_LEAK_MCP:-}" ]; then
    ( exec -a "$STUB_CLIENT_LEAK_MCP" sleep 30 ) &
    disown
fi
stdin_bytes="$(cat | wc -c | tr -d ' ')"
{
    printf 'invocation argv=%s\n' "$*"
    printf 'invocation stdin_bytes=%s\n' "$stdin_bytes"
    printf 'invocation base_url=%s\n' "${ANTHROPIC_BASE_URL:-}"
    printf 'invocation api_key_set=%s\n' "$([ -n "${ANTHROPIC_API_KEY:-}" ] && echo yes || echo no)"
    printf 'invocation bearer=%s\n' "${ANTHROPIC_AUTH_TOKEN:-}"
    printf 'invocation https_proxy=%s\n' "${HTTPS_PROXY:-}"
    printf 'invocation node_ca=%s\n' "${NODE_EXTRA_CA_CERTS:-}"
    printf 'invocation thinking_tokens=%s\n' "${MAX_THINKING_TOKENS-unset}"
    printf 'invocation caching_disabled=%s\n' "${DISABLE_PROMPT_CACHING-unset}"
    printf 'invocation cwd=%s\n' "$PWD"
    printf 'invocation home=%s\n' "$HOME"
    printf 'invocation notes_alpha=%s\n' "$([ -f notes-alpha.txt ] && echo yes || echo no)"
    printf 'invocation filler=%s\n' "$(ls filler-*.txt 2>/dev/null | wc -l)"
    printf 'invocation filler_max_bytes=%s\n' \
        "$(wc -c filler-*.txt 2>/dev/null | awk '$2!="total"{if($1>m)m=$1}END{print m+0}')"
    printf 'invocation filler_max_lines=%s\n' \
        "$(wc -l filler-*.txt 2>/dev/null | awk '$2!="total"{if($1>m)m=$1}END{print m+0}')"
    printf 'invocation table=%s\n' "$([ -f reference-table.txt ] && echo yes || echo no)"
} >>"$CLIENT_OUT"
exit "${STUB_CLIENT_RC:-0}"
SH
    chmod +x "$1"
}

# A throwaway repo carrying the real runner, rig, scrub script, lane
# configs, and the whole real drivers tree, plus the two stubs.
make_work() {
    local work
    work="$(mktemp -d)"
    mkdir -p "$work/repo/scripts" "$work/repo/crates/routectl-cli/tests/fixtures" "$work/bin"
    cp "$RUNNER" "$work/repo/scripts/capture_driver.sh"
    cp "$RIG" "$work/repo/scripts/capture_fixtures.sh"
    cp "$SCRUB" "$work/repo/scripts/scrub-fixture.sh"
    cp -r "$DRIVERS" "$work/repo/scripts/drivers"
    printf '[workspace.package]\nversion = "9.9.9"\n' >"$work/repo/Cargo.toml"
    write_listener "$work/bin/listener.py"
    write_stub_routectl "$work/bin/routectl-stub"
    write_stub_client "$work/bin/client-stub"
    canned_trace >"$work/canned-trace.log"
    printf '%s\n' "$work"
}

free_port() {
    local candidate i=0
    while [ "$i" -lt 200 ]; do
        candidate=$((25000 + RANDOM % 4000))
        if ! ss -ltn 2>/dev/null | awk -v p=":$candidate" \
            'NR > 1 && index($4, p) == length($4) - length(p) + 1 { found = 1 } END { exit !found }'; then
            printf '%s\n' "$candidate"
            return 0
        fi
        i=$((i + 1))
    done
    return 1
}

# Two CONSECUTIVE free ports, printed as the lower one. The runner's
# front-proxy mode draws TWO distinct ports from its window, so a
# single-port window would exhaust; every driver_run gets the pair so the
# same invocation shape covers both modes.
free_port_pair() {
    local candidate i=0
    while [ "$i" -lt 200 ]; do
        candidate="$(free_port)" || return 1
        if ! ss -ltn 2>/dev/null | awk -v p=":$((candidate + 1))" \
            'NR > 1 && index($4, p) == length($4) - length(p) + 1 { found = 1 } END { exit !found }'; then
            printf '%s\n' "$candidate"
            return 0
        fi
        i=$((i + 1))
    done
    return 1
}

# Run the real runner in the throwaway repo, driving one real driver
# against both stubs. `--keep` is the DEFAULT here: the driver records the
# client version into the run workspace, and a removed workspace could not
# be asserted on. The kept path is read back out of the runner's own log.
#
# `DRIVER_KEEP=0` drops the flag, which is the production shape -- the
# workspace is removed on exit. A case that asserts the binary-side version
# reached the FIXTURE must run that way: with `--keep` the record survives,
# so the assertion could pass against a runner that never crossed the value
# back at all.
#
# The driver path is ABSOLUTE. The runner runs its driver command with cwd
# set to the run's throwaway git repo, so a repo-relative path would
# resolve against that workspace instead of against the checkout.
#
# Usage: driver_run <work> <driver-relative-path> <case-id> [extra runner flags...]
driver_run() {
    local work="$1" driver="$2" case_id="$3"
    shift 3
    local port rc=0 keep_argv=()
    [ "${DRIVER_KEEP:-1}" = 0 ] || keep_argv=(--keep)
    port="$(free_port_pair)"
    (
        cd "$work/repo" || exit 2
        ROUTECTL_BIN="$work/bin/routectl-stub" \
        STUB_PID_FILE="$work/stub.pid" \
        STUB_TRACE_FILE="$work/canned-trace.log" \
        STUB_LISTENER="$work/bin/listener.py" \
        CLIENT_OUT="$work/client.txt" \
        STUB_CLIENT_RC="${STUB_CLIENT_RC:-0}" \
        ROUTECTL_DRIVER_CLAUDE_BIN="$work/bin/client-stub" \
        ROUTECTL_DRIVER_AGENT_BIN="$work/bin/client-stub" \
        ROUTECTL_DRIVER_AGENT_CONTINUE_FLAG="--continue" \
        ROUTECTL_DRIVER_AGENT_MODEL_FLAG="-m" \
        ROUTECTL_DRIVER_AGENT_REASONING_FLAG="--reasoning" \
        ROUTECTL_DRIVER_SETTLE_SECONDS=0 \
        ROUTECTL_DRIVER_TURN_SECONDS=0 \
        ROUTECTL_DRIVER_EXIT_SECONDS=0 \
        ROUTECTL_DRIVER_PORT_MIN="$port" \
        ROUTECTL_DRIVER_PORT_MAX="$((port + 1))" \
            bash scripts/capture_driver.sh --work "$work/runs" \
            "${keep_argv[@]+"${keep_argv[@]}"}" \
            --case "$case_id" --lane "${DRIVER_LANE:-anthropic-api}" \
            --expected-ingress "${DRIVER_EXPECTED_INGRESS:-anthropic}" "$@" \
            -- "$work/repo/scripts/drivers/$driver"
    ) >"$work/runner.log" 2>&1 || rc=$?
    return "$rc"
}

# The run workspace the runner kept, read from its own message.
kept_run() {
    sed -n 's/.*run workspace kept at //p' "$1/runner.log" | head -1
}

client_get() {
    sed -n "s/^invocation $2=//p" "$1/client.txt"
}

meta_get() {
    python3 - "$1" "$2" <<'PY'
import json, sys
with open(sys.argv[1]) as fh:
    node = json.load(fh)
for part in sys.argv[2].split("."):
    node = node[part]
print(node)
PY
}

# Where the runner lands a fixture with no `--out`: the gitignored scratch
# root at the repo root, not the committed corpus. This suite drives the
# real runner in a throwaway repo, so the default is what it exercises.
landed_meta() {
    printf '%s\n' "$1/repo/.routectl-driver-scratch/anthropic-api/$2/meta.json"
}

# ---------------------------------------------------------------------
# Part 3: every driver runs end to end against the stubs
# ---------------------------------------------------------------------

for driver in claude-code.sh claude-code-print.sh external-agent-cli.sh; do
    work="$(make_work)"
    # The case claims `tool-use-multiturn`, and the rig refuses to promote a
    # fixture that does not exhibit the pattern its case claims.
    canned_trace_tools >"$work/canned-trace.log"
    rc=0
    driver_run "$work" "$driver" tools-multiturn-01 || rc=$?
    check "$driver: a run against the stub daemon exits 0" "0" "$rc"

    if [ -f "$work/client.txt" ]; then
        echo "PASS: $driver: the client was actually invoked"
        check "$driver: the client saw the runner's base url" \
            "yes" "$([ -n "$(client_get "$work" api_key_set)" ] && echo yes || echo no)"
        check "$driver: the throwaway cwd was seeded for the case" \
            "yes" "$(client_get "$work" notes_alpha | head -1)"
        check_ne "$driver: the client ran outside the invoking HOME" \
            "$HOME" "$(client_get "$work" home | head -1)"
    else
        fail "$driver: the client never ran (runner log: $work/runner.log)"
        sed -n '1,25p' "$work/runner.log"
        fails=$((fails + 3))
    fi

    # The version read from the BINARY at run time is what gives a fixture
    # its decay clock. Asserted from the run's own record, not from the
    # trace: the trace's user-agent is synthetic here, while this value
    # came out of the driver invoking the binary.
    kept="$(kept_run "$work")"
    if [ -n "$kept" ] && [ -f "$kept/client.txt" ]; then
        check "$driver: the driver recorded the client version from the binary" \
            "9.9.9 (Stub Client)" "$(sed -n 's/^version=//p' "$kept/client.txt")"
        check "$driver: the run record names the case" \
            "tools-multiturn-01" "$(sed -n 's/^case_id=//p' "$kept/client.txt")"
    else
        fail "$driver: no client record in the kept run workspace"
        fails=$((fails + 1))
    fi

    meta="$(landed_meta "$work" tools-multiturn-01)"
    if [ -f "$meta" ]; then
        echo "PASS: $driver: the rig landed a fixture at <lane>/<case_id>"
        check "$driver: meta.case_id carries the case" "tools-multiturn-01" \
            "$(meta_get "$meta" case_id)"
        check "$driver: meta.client.version is populated" "9.9.9" \
            "$(meta_get "$meta" client.version)"
        # The driver-side read reaching the fixture. The stub binary prints
        # a DECORATED line and the canned trace's user-agent carries the
        # bare token, so this pair also exercises the real spelling
        # asymmetry: the two agree on the token and the promotion gate
        # passed, which is why the fixture above landed at all.
        check "$driver: meta.client.binary_version carries the driver's read" \
            "9.9.9 (Stub Client)" "$(meta_get "$meta" client.binary_version)"
        check "$driver: meta.client.connection_mode is populated" "base-url" \
            "$(meta_get "$meta" client.connection_mode)"
        # The traced dialect the expected-ingress gate compared the pin
        # against. Recorded rather than the pin itself, which is why the
        # gate is a comparison at all -- and asserted here so a run that
        # landed the fixture is known to have landed it having AGREED, not
        # having skipped the check.
        check "$driver: meta.ingress_kind carries the traced dialect the pin was checked against" \
            "anthropic" "$(meta_get "$meta" ingress_kind)"
        # The pin is NOT recorded: the value it pins is already on disk as
        # the traced token above, so a second copy could only ever disagree
        # with the fact it was checked against. This repo has already
        # deleted two speculative meta fields for the same reason.
        if grep -q 'expected_ingress' "$meta"; then
            fail "$driver: meta.json records the expected-ingress pin beside the traced dialect"
        else
            echo "PASS: $driver: meta.json records no second copy of the traced dialect"
        fi
    else
        fail "$driver: no fixture landed at $meta"
        sed -n '1,30p' "$work/runner.log"
        fails=$((fails + 5))
    fi

    [ -n "$kept" ] && rm -rf "$kept"
    rm -rf "$work"
done

# ---------------------------------------------------------------------
# Part 3b: the client version crosses the workspace's removal
# ---------------------------------------------------------------------
# The defect this closes: the driver-side version read used to die with the
# run workspace, leaving `meta.client.version` -- parsed from the
# CLIENT-CONTROLLED user-agent -- as the only statement about the client,
# with nothing to check it against.
#
# So this part runs WITHOUT `--keep`, the production shape. Part 3's legs
# all keep the workspace, so they cannot tell a value that crossed back
# from one that merely survived in a directory nobody deleted.

work="$(make_work)"
rc=0
DRIVER_KEEP=0 driver_run "$work" claude-code-print.sh plain-turn-01 || rc=$?
check "a run with no --keep exits 0" "0" "$rc"
# The precondition: the workspace really is gone. Without it, the fixture
# assertion below could be satisfied by a run that kept it after all.
kept="$(kept_run "$work")"
check "the run workspace was removed" "" "$kept"
if [ -z "$kept" ] && [ ! -d "$work/runs" ] || [ -z "$(ls -A "$work/runs" 2>/dev/null)" ]; then
    echo "PASS: the run workspace left nothing behind"
else
    fail "the run workspace survived at $work/runs, so the crossing is unproven"
fi
meta="$(landed_meta "$work" plain-turn-01)"
if [ -f "$meta" ]; then
    check "the binary-side version reached the fixture with no workspace to read" \
        "9.9.9 (Stub Client)" "$(meta_get "$meta" client.binary_version)"
else
    fail "no fixture landed at $meta"
    sed -n '1,30p' "$work/runner.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# DISAGREEMENT REFUSES PROMOTION. The stub client's version is forced to a
# value the canned trace's user-agent contradicts, which is the shape a
# client that auto-updated mid-run produces. Nothing else about the run
# changes, so the refusal is attributable to the disagreement alone -- and
# Part 3's agreeing legs are the paired positive control that this gate does
# not refuse every run.
work="$(make_work)"
rc=0
STUB_CLIENT_VERSION="1.1.1 (Stub Client)" \
    DRIVER_KEEP=0 driver_run "$work" claude-code-print.sh plain-turn-01 || rc=$?
check_ne "a binary-vs-wire disagreement fails the run" "0" "$rc"
if [ -f "$(landed_meta "$work" plain-turn-01)" ]; then
    fail "a fixture whose two client versions disagree still landed"
else
    echo "PASS: a fixture whose two client versions disagree does not land"
fi
if grep -qF 'off the binary' "$work/runner.log"; then
    echo "PASS: the refusal names the binary-side reading"
else
    fail "the refusal did not name the binary-side reading"
    sed -n '1,30p' "$work/runner.log"
fi
rm -rf "$work"

# AN ABSENT WIRE VERSION IS RECORDED AS ABSENT, NEVER INVENTED. The trace's
# user-agent is stripped of its version, so the rig parses none -- and the
# binary-side read still succeeded. The pair is unprovable rather than
# contradicted, so the fixture LANDS with the wire field empty and the
# binary field populated: nothing backfills one from the other, which is
# what keeps the two able to disagree at all.
work="$(make_work)"
canned_trace | sed 's|claude-cli/9\.9\.9 (external, cli)|claude-cli|' \
    >"$work/canned-trace.log"
rc=0
DRIVER_KEEP=0 driver_run "$work" claude-code-print.sh plain-turn-01 || rc=$?
check "an absent wire version still lands a fixture" "0" "$rc"
meta="$(landed_meta "$work" plain-turn-01)"
if [ -f "$meta" ]; then
    check "an absent wire version is recorded as empty" "" \
        "$(meta_get "$meta" client.version)"
    check "the binary-side version is NOT copied into the wire field" \
        "9.9.9 (Stub Client)" "$(meta_get "$meta" client.binary_version)"
    # The name still parses out of the same header, which is the control
    # proving the trace edit removed the VERSION and not the user-agent.
    check "the client name still parses from the stripped user-agent" \
        "claude-cli" "$(meta_get "$meta" client.name)"
else
    fail "no fixture landed for the absent-wire-version case"
    sed -n '1,30p' "$work/runner.log"
    fails=$((fails + 3))
fi
rm -rf "$work"

# ---------------------------------------------------------------------
# Part 4: a driver fails closed
# ---------------------------------------------------------------------

# An unreachable daemon. The driver is invoked DIRECTLY here, with the
# runner's contract hand-set: the runner's own health poll passes before a
# driver starts, so a daemon that died in between is only reachable as a
# driver-level case. Without the driver's own check the client would fail
# in its own vocabulary -- a credential error, an empty session -- and the
# rig would land a fixture off a trace holding no dialogue.
work="$(make_work)"
dead_port="$(free_port)"
live_port="$(free_port)"
while [ "$live_port" = "$dead_port" ]; do live_port="$(free_port)"; done

# Invoke a driver outside the runner, with a caller-chosen base URL.
direct_run() {
    local work="$1" driver="$2" base="$3" rc=0
    local run="$work/direct-run" cwd="$work/direct-work"
    rm -rf "$run" "$cwd"
    mkdir -p "$run" "$cwd"
    (
        cd "$work/repo" || exit 2
        HOME="$work/direct-home" \
        CLIENT_OUT="$work/client.txt" \
        ROUTECTL_DRIVER_CLAUDE_BIN="$work/bin/client-stub" \
        ROUTECTL_DRIVER_AGENT_BIN="$work/bin/client-stub" \
        ROUTECTL_DRIVER_AGENT_CONTINUE_FLAG="--continue" \
        ROUTECTL_DRIVER_SETTLE_SECONDS=0 \
        ROUTECTL_DRIVER_TURN_SECONDS=0 \
        ROUTECTL_DRIVER_EXIT_SECONDS=0 \
        ROUTECTL_BASE_URL="$base" \
        ROUTECTL_DRIVER_RUN="$run" \
        ROUTECTL_DRIVER_WORK="$cwd" \
        ROUTECTL_FIXTURE_CASE_ID="${DIRECT_CASE:-tools-multiturn-01}" \
        ROUTECTL_FIXTURE_CONFIG_SHA="deadbeef" \
        ROUTECTL_FIXTURE_CONNECTION_MODE="${DIRECT_MODE:-base-url}" \
        ROUTECTL_FIXTURE_EXPECTED_INGRESS="anthropic" \
            bash "scripts/drivers/$driver"
    ) >"$work/direct.log" 2>&1 || rc=$?
    return "$rc"
}

listener_pid=""
python3 "$work/bin/listener.py" "$live_port" >/dev/null 2>&1 &
listener_pid=$!
i=0
while [ "$i" -lt 40 ] && ! curl -fsS -m 1 "http://127.0.0.1:$live_port/health" >/dev/null 2>&1; do
    sleep 0.1
    i=$((i + 1))
done

if curl -fsS -m 1 "http://127.0.0.1:$live_port/health" >/dev/null 2>&1; then
    echo "PASS: the paired reachable-daemon control has a live listener"
    for driver in claude-code.sh claude-code-print.sh external-agent-cli.sh; do
        rc=0
        direct_run "$work" "$driver" "http://127.0.0.1:$live_port" || rc=$?
        check "$driver: exits 0 against a reachable daemon" "0" "$rc"
        rc=0
        direct_run "$work" "$driver" "http://127.0.0.1:$dead_port" || rc=$?
        check_ne "$driver: exits non-zero when the daemon is unreachable" "0" "$rc"
        if grep -qF 'unreachable' "$work/direct.log"; then
            echo "PASS: $driver: the refusal names the unreachable daemon"
        else
            fail "$driver: the refusal did not name the unreachable daemon"
            sed -n '1,10p' "$work/direct.log"
        fi
    done
else
    fail "could not bind a listener; the reachable/unreachable pair proves nothing"
    fails=$((fails + 8))
fi
[ -n "$listener_pid" ] && kill "$listener_pid" 2>/dev/null
wait "$listener_pid" 2>/dev/null
rm -rf "$work"

# A client that cannot state its version, and a client that fails.
work="$(make_work)"
rc=0
STUB_CLIENT_RC=9 driver_run "$work" claude-code-print.sh plain-turn-01 || rc=$?
check_ne "a failing client aborts the run" "0" "$rc"
if [ -f "$(landed_meta "$work" plain-turn-01)" ]; then
    fail "a failing client still landed a fixture"
else
    echo "PASS: a failing client lands no fixture"
fi
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# An unknown case id: a driver reads its case from the runner's pin, so an
# id with no case file is a run that would capture an interaction nobody
# described.
work="$(make_work)"
rc=0
driver_run "$work" claude-code-print.sh no-such-case || rc=$?
check_ne "an unknown case id aborts the run" "0" "$rc"
check_ne "the unknown-case run landed no fixture" "yes" \
    "$([ -f "$(landed_meta "$work" no-such-case)" ] && echo yes || echo no)"
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# The third driver names no client of its own: an unset binary is a
# refusal, not a default.
work="$(make_work)"
rc=0
(
    cd "$work/repo" || exit 2
    ROUTECTL_BASE_URL="http://127.0.0.1:1" \
    ROUTECTL_DRIVER_RUN="$work" \
    ROUTECTL_DRIVER_WORK="$work" \
    ROUTECTL_FIXTURE_CASE_ID="plain-turn-01" \
    ROUTECTL_FIXTURE_CONNECTION_MODE="base-url" \
        bash scripts/drivers/external-agent-cli.sh
) >"$work/direct.log" 2>&1 || rc=$?
check_ne "the third driver refuses to run with no client named" "0" "$rc"
if grep -qF 'ROUTECTL_DRIVER_AGENT_BIN' "$work/direct.log"; then
    echo "PASS: the third driver's refusal names the missing client variable"
else
    fail "the third driver's refusal did not name the missing client variable"
fi
rm -rf "$work"

# The third driver refuses front-proxy mode OUTRIGHT: its client is
# arbitrary, so there is no verified trust path for the MITM CA, and a
# client that ignored NODE_EXTRA_CA_CERTS would silently fall back to a
# direct connection rather than fail. The refusal fires before the daemon
# precondition (a dead port here is what proves that), and the message
# grep is what attributes the failure to the mode rather than to the
# missing daemon. Part 3's base-url pass through this driver is the
# paired control: a driver that refused every run would fail there.
work="$(make_work)"
rc=0
DIRECT_MODE=front-proxy DIRECT_CASE=plain-turn-01 \
    direct_run "$work" external-agent-cli.sh "http://127.0.0.1:1" || rc=$?
check "the third driver refuses front-proxy mode with exit 2" "2" "$rc"
if grep -qF 'no verified trust path' "$work/direct.log"; then
    echo "PASS: the front-proxy refusal names the missing trust path"
else
    fail "the front-proxy refusal did not name the missing trust path"
    sed -n '1,10p' "$work/direct.log"
fi
if grep -qF 'NODE_EXTRA_CA_CERTS' "$work/direct.log"; then
    echo "PASS: the front-proxy refusal names the silent-fallback carrier"
else
    fail "the front-proxy refusal did not name the silent-fallback carrier"
fi
rm -rf "$work"

# ---------------------------------------------------------------------
# Part 5: the claude-code driver honors BOTH connection modes
# ---------------------------------------------------------------------
# The two modes emit different wire shapes: a MITM front proxy carries
# `role:"system"` turns inside `messages[]` while base-url mode inlines the
# same content as system-reminder text with zero system turns. A mode that
# did not reach the client's environment would land a fixture labelled
# front-proxy whose shape is base-url, and every later cross-mode diff
# would read as client drift.

work="$(make_work)"
# The case claims `thinking`, and the rig refuses to promote a fixture whose
# captured structural line shows no active thinking block.
canned_trace_thinking >"$work/canned-trace.log"
rc=0
# The inherited bearer is what makes the two "no bearer" assertions below
# non-vacuous: base-url mode must CLEAR a carrier the caller's environment
# already holds, not merely decline to add one.
ANTHROPIC_AUTH_TOKEN="inherited-carrier-must-not-survive" \
    driver_run "$work" claude-code.sh thinking-01 --connection-mode base-url || rc=$?
check "claude-code: a base-url run exits 0" "0" "$rc"
check "claude-code: base-url reaches the client as ANTHROPIC_BASE_URL" \
    "yes" "$([ -n "$(client_get "$work" base_url | head -1)" ] && echo yes || echo no)"
check "claude-code: base-url sets no proxy in the client's environment" \
    "" "$(client_get "$work" https_proxy | head -1)"
check "claude-code: base-url clears an inherited bearer and sets none" \
    "" "$(client_get "$work" bearer | head -1)"
meta="$(landed_meta "$work" thinking-01)"
if [ -f "$meta" ]; then
    check "claude-code: base-url reaches the fixture pin" "base-url" \
        "$(meta_get "$meta" client.connection_mode)"
else
    fail "claude-code: the base-url run landed no fixture"
fi
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

work="$(make_work)"
# A front-proxy fixture must prove TRANSIT, not just intent: the rig refuses
# one whose captured ingress headers carry no seam header. The case also
# claims `thinking`, so the trace carries both.
canned_trace_thinking | with_seam_header >"$work/canned-trace.log"
rc=0
# The runner owns both proxy carriers now: it probes the MITM port and
# points the CA at the path the daemon mints under the run's XDG root.
# The inherited bearer is what makes the placeholder assertion below
# non-vacuous, and the front-proxy LANE is what the mode/config coherence
# check requires.
DRIVER_LANE=anthropic-api.front-proxy \
ANTHROPIC_AUTH_TOKEN="inherited-carrier-must-not-survive" \
    driver_run "$work" claude-code.sh thinking-01 --connection-mode front-proxy || rc=$?
check "claude-code: a front-proxy run exits 0" "0" "$rc"
proxy_url="$(client_get "$work" https_proxy | head -1)"
case "$proxy_url" in
    http://127.0.0.1:[0-9]*)
        echo "PASS: claude-code: front-proxy reaches the client as HTTPS_PROXY" ;;
    *)
        fail "claude-code: HTTPS_PROXY is '$proxy_url', not the runner's MITM listener"
        ;;
esac
case "$(client_get "$work" node_ca | head -1)" in
    */xdg/routectl/mitm-certs/current/mitm-ca-cert.pem)
        echo "PASS: claude-code: front-proxy points the client at the run-minted CA" ;;
    *)
        fail "claude-code: NODE_EXTRA_CA_CERTS is '$(client_get "$work" node_ca | head -1)'"
        ;;
esac
check "claude-code: front-proxy does not also set a direct base url" \
    "" "$(client_get "$work" base_url | head -1)"
# The seam's admission gate rejects an x-api-key-only request before body
# parse, so the bearer carrier must reach the client -- and it must be the
# driver's own placeholder rather than whatever the caller's environment
# had inherited, which the unconditional unset clears first.
check "claude-code: front-proxy exports the bearer carrier the seam admits on" \
    "routectl-driver-front-proxy-placeholder-not-a-token" \
    "$(client_get "$work" bearer | head -1)"
check "claude-code: front-proxy still exports the client's api key too" \
    "yes" "$(client_get "$work" api_key_set | head -1)"
# The landing lane is NORMALIZED by the rig from the trace's
# provider_kind, not copied from the runner's --lane, so the front-proxy
# lane's fixture still lands under the provider lane.
meta="$(landed_meta "$work" thinking-01)"
if [ -f "$meta" ]; then
    check "claude-code: front-proxy reaches the fixture pin" "front-proxy" \
        "$(meta_get "$meta" client.connection_mode)"
else
    fail "claude-code: the front-proxy run landed no fixture"
fi
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# A caller whose client validates the carrier's shape can pass its own
# value through. Without this leg the placeholder assertion above would
# hold equally against an arm that hardcoded the constant.
work="$(make_work)"
canned_trace_thinking | with_seam_header >"$work/canned-trace.log"
rc=0
DRIVER_LANE=anthropic-api.front-proxy \
ROUTECTL_DRIVER_CLIENT_BEARER="caller-supplied-placeholder" \
    driver_run "$work" claude-code.sh thinking-01 --connection-mode front-proxy || rc=$?
check "claude-code: a front-proxy run with a caller-supplied bearer exits 0" "0" "$rc"
check "claude-code: a caller-supplied bearer replaces the placeholder" \
    "caller-supplied-placeholder" "$(client_get "$work" bearer | head -1)"
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# Paired refusal: a DRIVER given front-proxy mode without the carriers
# must FAIL rather than fall back to base-url. The runner always supplies
# both now, so the only honest way to reach this arm is a direct driver
# invocation with the runner's contract hand-set minus the carriers --
# which is also the shape of the real failure it guards (a driver run
# outside the runner). A live listener is required, because the driver's
# own daemon precondition fires before its mode check. The success case
# above is what keeps this from passing against a driver that refuses
# front-proxy outright.
work="$(make_work)"
fp_port="$(free_port)"
python3 "$work/bin/listener.py" "$fp_port" >/dev/null 2>&1 &
fp_listener=$!
i=0
while [ "$i" -lt 40 ] && ! curl -fsS -m 1 "http://127.0.0.1:$fp_port/health" >/dev/null 2>&1; do
    sleep 0.1
    i=$((i + 1))
done
rc=0
DIRECT_MODE=front-proxy DIRECT_CASE=thinking-01 \
    direct_run "$work" claude-code.sh "http://127.0.0.1:$fp_port" || rc=$?
check_ne "claude-code: front-proxy with no proxy url aborts" "0" "$rc"
if grep -qF 'ROUTECTL_DRIVER_PROXY_URL' "$work/direct.log"; then
    echo "PASS: claude-code: the refusal names the missing proxy url"
else
    fail "claude-code: the refusal did not name the missing proxy url"
    sed -n '1,15p' "$work/direct.log"
fi
kill "$fp_listener" 2>/dev/null
wait "$fp_listener" 2>/dev/null
rm -rf "$work"

# An unsupported mode is a refusal, not a silent base-url run.
work="$(make_work)"
rc=0
driver_run "$work" claude-code.sh thinking-01 --connection-mode sidecar || rc=$?
check_ne "claude-code: an unsupported connection mode aborts" "0" "$rc"
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# ---------------------------------------------------------------------
# Part 6: the case's knobs actually reach the client
# ---------------------------------------------------------------------
# A knob the driver read but never applied would make the whole set
# decorative: every case would capture the same shape under a different id.

work="$(make_work)"
driver_run "$work" claude-code-print.sh tools-multiturn-01 || true
check "a multi-turn case invokes the client once per turn" "2" \
    "$(client_get "$work" argv | wc -l)"
# The client's stdin must be EMPTY on every invocation. The stub drains it,
# so a driver that let the client inherit the turn loop's own stdin shows
# up here as a non-zero byte count on turn 1 -- and as one invocation
# instead of two above, since the drained bytes are the remaining prompts.
# This is what makes the once-per-turn assertion non-vacuous: without a
# draining stub it passed against a stub that could not exhibit the bug.
turn_stdin="$(client_get "$work" stdin_bytes | tr '\n' ',')"
check "the client sees no stdin on any turn" "0,0," "$turn_stdin"
unset turn_stdin
if client_get "$work" argv | sed -n '2p' | grep -q -- '--resume'; then
    echo "PASS: a multi-turn print run resumes rather than reopening"
else
    fail "a multi-turn print run did not resume the first turn's session"
fi
# Each turn carries ITS OWN prompt, not the concatenation. The collapsed
# shape put both prompts in one argv, which the count assertion above
# catches only because the loop then ran once -- a driver that passed both
# prompts to each of two invocations would satisfy the count.
if client_get "$work" argv | sed -n '1p' | grep -qF 'notes-beta.txt'; then
    fail "the first turn's argv carries the SECOND turn's prompt"
else
    echo "PASS: the first turn's argv carries only its own prompt"
fi
if client_get "$work" argv | sed -n '2p' | grep -qF 'notes-beta.txt'; then
    echo "PASS: the second turn's argv carries the second prompt"
else
    fail "the second turn's argv does not carry the second turn's prompt"
fi
# Paired controls for the forced-off assertions below: a case whose knob is
# TRUE must leave the knob alone, or those assertions would pass against a
# driver that forced everything off unconditionally.
if client_get "$work" argv | grep -q -- '--disallowed-tools'; then
    fail "a tools case denied the client its tools anyway"
else
    echo "PASS: a tools case leaves the client's tools enabled"
fi
check "a no-thinking case forces the thinking budget to zero" "0" \
    "$(client_get "$work" thinking_tokens | head -1)"
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# The SECOND driver carrying the same loop shape. Its turn loop feeds the
# client from the same pipe, so it has the same inheritance to get wrong
# and needs its own evidence -- a fix applied to one file leaves the other
# broken, and only this leg says so.
work="$(make_work)"
canned_trace_tools >"$work/canned-trace.log"
driver_run "$work" external-agent-cli.sh tools-multiturn-01 || true
check "the agent CLI driver invokes the client once per turn" "2" \
    "$(client_get "$work" argv | wc -l)"
turn_stdin="$(client_get "$work" stdin_bytes | tr '\n' ',')"
check "the agent CLI's client sees no stdin on any turn" "0,0," "$turn_stdin"
unset turn_stdin
if client_get "$work" argv | sed -n '1p' | grep -qF 'notes-beta.txt'; then
    fail "the agent CLI's first turn carries the SECOND turn's prompt"
else
    echo "PASS: the agent CLI's first turn carries only its own prompt"
fi
if client_get "$work" argv | sed -n '2p' | grep -q -- '--continue'; then
    echo "PASS: the agent CLI's later turn continues the session"
else
    fail "the agent CLI's later turn did not continue the session"
fi
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

work="$(make_work)"
driver_run "$work" claude-code-print.sh thinking-01 || true
if client_get "$work" argv | grep -q -- '--effort'; then
    echo "PASS: a thinking case asks the client for extended thinking"
else
    fail "a thinking case did not ask the client for extended thinking"
fi
check "a thinking case hands the client a non-zero thinking budget" "8192" \
    "$(client_get "$work" thinking_tokens | head -1)"
# The wildcard, not a name list: an enumeration cannot deny a tool the
# client grew after this file was written, and the list it replaced leaked
# 16 tools onto the wire under a case asking for none.
if client_get "$work" argv | grep -qF -- '--disallowed-tools *'; then
    echo "PASS: a no-tools case denies the client every tool by wildcard"
else
    fail "a no-tools case did not deny the client its tools by wildcard"
fi
check "a no-cache case forces prompt caching off in the client's environment" "1" \
    "$(client_get "$work" caching_disabled | head -1)"
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

work="$(make_work)"
driver_run "$work" claude-code-print.sh large-context-01 || true
padding_files="$(client_get "$work" filler | head -1)"
if [ -n "$padding_files" ] && [ "$padding_files" -gt 0 ]; then
    echo "PASS: a large-context case materializes its filler in the throwaway cwd"
else
    echo "FAIL: a large-context case left the client nothing large to read"
    fails=$((fails + 1))
fi
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# --- mcp-tools: the stdio server reaches the client, and its death is
# checked rather than assumed ---------------------------------------------

work="$(make_work)"
canned_trace_mcp_tools >"$work/canned-trace.log"
driver_run "$work" claude-code-print.sh mcp-tools-01 || true
if client_get "$work" argv | head -1 | grep -q -- '--mcp-config'; then
    echo "PASS: an mcp-tools case hands the client an mcp-config path"
else
    fail "an mcp-tools case did not pass --mcp-config to the client"
fi
if client_get "$work" argv | head -1 | grep -q -- '--strict-mcp-config'; then
    echo "PASS: an mcp-tools case asks the client to ignore ambient MCP config"
else
    fail "an mcp-tools case did not pass --strict-mcp-config to the client"
fi
mcp_config_path="$(client_get "$work" argv | head -1 |
    sed 's/.*--mcp-config \([^ ]*\).*/\1/')"
if [ -n "$mcp_config_path" ] && [ -f "$mcp_config_path" ] &&
   grep -qF '"command":"python3"' "$mcp_config_path" &&
   grep -qF 'stub_mcp.py' "$mcp_config_path"; then
    echo "PASS: the mcp-config file hands the client the committed stub script"
else
    fail "the mcp-config file does not point the client at the committed stub"
fi
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# The well-behaved leg: the stub client here spawns no MCP child at all, so
# the driver's leak check must find nothing and let the run finish clean --
# the paired control for the leak leg below, without which a check that
# always fires (or never runs) would read the same as one that works.
work="$(make_work)"
canned_trace_mcp_tools >"$work/canned-trace.log"
rc=0
driver_run "$work" claude-code-print.sh mcp-tools-01 || rc=$?
if [ "$rc" = 0 ]; then
    echo "PASS: a well-behaved client leaves no mcp stub process behind"
else
    fail "a well-behaved run failed the leak check ($rc): $(tail -n 5 "$work/runner.log")"
fi
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# The leak leg: the stub client models a misbehaving one by spawning a
# detached process whose cmdline carries the stub script's own path, and
# not waiting for it -- exactly what a client that failed to reap its MCP
# child would leave behind. The driver must catch this itself rather than
# trust the client's exit code, since the client here exits 0.
work="$(make_work)"
canned_trace_mcp_tools >"$work/canned-trace.log"
leak_needle="$work/repo/scripts/drivers/stub_mcp.py"
rc=0
STUB_CLIENT_LEAK_MCP="$leak_needle" \
    driver_run "$work" claude-code-print.sh mcp-tools-01 || rc=$?
if [ "$rc" != 0 ] &&
   grep -qF "outlived the client that spawned it" "$work/runner.log"; then
    echo "PASS: a leaked mcp stub process is caught rather than assumed gone"
else
    fail "the driver did not catch a leaked mcp stub process: $(tail -n 5 "$work/runner.log")"
fi
# Clean up the leaked process by the exact PID this probe finds -- never a
# pattern kill, per the same rule the driver's own check follows.
leaked_pid=""
for dir in /proc/[0-9]*; do
    pid="${dir#/proc/}"
    [ -r "$dir/cmdline" ] || continue
    if tr '\0' ' ' <"$dir/cmdline" 2>/dev/null | grep -qF -- "$leak_needle"; then
        leaked_pid="$pid"
        break
    fi
done
[ -n "$leaked_pid" ] && kill "$leaked_pid" 2>/dev/null || true
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# --- driver_processes_matching itself: found while alive, gone once dead --

(
    . "$DRIVERS/lib/common.sh"
    unique_needle="drivers-test-probe-$$"
    ( exec -a "$unique_needle" sleep 5 ) &
    probe_pid=$!
    sleep 0.2
    found="$(driver_processes_matching "$unique_needle")"
    if [ "$found" = "$probe_pid" ]; then
        echo "PASS: driver_processes_matching finds a running process by cmdline"
    else
        echo "FAIL: driver_processes_matching found '$found', expected '$probe_pid'"
        exit 1
    fi
    kill "$probe_pid" 2>/dev/null || true
    wait "$probe_pid" 2>/dev/null || true
    found="$(driver_processes_matching "$unique_needle")"
    if [ -z "$found" ]; then
        echo "PASS: driver_processes_matching finds nothing once the process is gone"
    else
        echo "FAIL: driver_processes_matching still reports '$found' after the process exited"
        exit 1
    fi
) || fails=$((fails + 1))

# The filler only reaches the wire as a TOOL RESULT, so every generated file
# must be one the client will return IN FULL. A driven Claude Code refuses a
# file over its own read caps and returns nothing, which is how a padded case
# produced an UNDER-floor body: the observed caps are 262144 bytes, 25000
# content tokens, and a 2000-line default read window. These bounds are
# asserted rather than trusted because a chunk size raised past them
# materializes bytes that cannot leave the disk, and the only symptom is a
# refused capture after real spend.
work="$(make_work)"
driver_run "$work" claude-code-print.sh large-context-01 || true
filler_max_bytes="$(client_get "$work" filler_max_bytes | head -1)"
filler_max_lines="$(client_get "$work" filler_max_lines | head -1)"
if [ -n "$filler_max_bytes" ] && [ "$filler_max_bytes" -gt 0 ] &&
   [ "$filler_max_bytes" -lt 262144 ]; then
    echo "PASS: every generated filler file is under the client's read size cap"
else
    echo "FAIL: a filler file at $filler_max_bytes bytes is not under the 262144 read cap"
    fails=$((fails + 1))
fi
if [ -n "$filler_max_lines" ] && [ "$filler_max_lines" -gt 0 ] &&
   [ "$filler_max_lines" -lt 2000 ]; then
    echo "PASS: every generated filler file is under the client's default read window"
else
    echo "FAIL: a filler file at $filler_max_lines lines is not under the 2000-line window"
    fails=$((fails + 1))
fi
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# Paired control for the filler assertion: a case with zero padding must
# leave NO filler, or the assertion above would pass on a driver that
# always generated it.
work="$(make_work)"
driver_run "$work" claude-code-print.sh plain-turn-01 || true
check "a case with no padding materializes no filler" "0" \
    "$(client_get "$work" filler | head -1)"
check "a case with no cache breakpoints materializes no reference table" "no" \
    "$(client_get "$work" table | head -1)"
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

work="$(make_work)"
driver_run "$work" claude-code-print.sh cache-breakpoints-01 || true
check "a cache-breakpoint case materializes the prefix the turns reuse" "yes" \
    "$(client_get "$work" table | head -1)"
check "a cache-breakpoint case leaves prompt caching enabled" "unset" \
    "$(client_get "$work" caching_disabled | head -1)"
kept="$(kept_run "$work")"
[ -n "$kept" ] && rm -rf "$kept"
rm -rf "$work"

# ---------------------------------------------------------------------
# Part 7: the client-profile seam, and its ordering constraint
# ---------------------------------------------------------------------
# ZERO profiles are committed (the closed set is empty until a cell needs
# one), so every leg here writes a throwaway profile into a throwaway repo.
# What is asserted is the RULE, not any profile: the ordering latch, and
# the forbidden classes the README states.
#
# The ordering is the load-bearing one. `driver_apply_anthropic_connection_mode`
# clears both modes' carriers precisely because the runner forwards the
# caller's environment and an operator who routes their own client through
# routectl already has ANTHROPIC_BASE_URL set. A profile applied AFTER that
# clear could re-set it, and the run would capture the operator's LIVE
# daemon while landing a fixture labelled hermetic.

work="$(make_work)"
PROFILES="$work/repo/scripts/drivers/profiles"
mkdir -p "$PROFILES"

# Exercise the library directly. A profile is a LIBRARY concern -- no
# committed driver loads one yet -- and the two orderings cannot both be
# reached through a driver that hardcodes one of them.
#
# `<order>` is `before` or `after`, naming when the profile is loaded
# relative to the connection-mode apply. Prints the loader's own message on
# failure; returns its exit status.
profile_run() {
    local order="$1" name="$2" rc=0
    (
        set -eu
        cd "$work/repo" || exit 9
        export ROUTECTL_BASE_URL="http://127.0.0.1:1"
        export ROUTECTL_DRIVER_RUN="$work" ROUTECTL_DRIVER_WORK="$work"
        export ROUTECTL_FIXTURE_CASE_ID="plain-turn-01"
        export ROUTECTL_FIXTURE_CONNECTION_MODE="base-url"
        . scripts/drivers/lib/common.sh
        if [ "$order" = before ]; then
            driver_load_client_profile "$name"
            driver_apply_anthropic_connection_mode
        else
            driver_apply_anthropic_connection_mode
            driver_load_client_profile "$name"
        fi
        printf 'base_url=%s\n' "${ANTHROPIC_BASE_URL:-}" >"$work/profile.txt"
        printf 'applied=%s\n' "${PROFILE_PROBE:-unset}" >>"$work/profile.txt"
    ) >"$work/profile.log" 2>&1 || rc=$?
    return "$rc"
}

# The ACCEPTED control comes first: without it, "a late load is refused"
# is satisfiable by a loader that refuses every load.
printf 'PROFILE_PROBE=applied\n' >"$PROFILES/selftest.env"
rm -f "$work/profile.txt"
rc=0
profile_run before selftest || rc=$?
check "a profile loaded BEFORE the connection-mode apply is accepted" "0" "$rc"
check "the accepted profile's key reached the client environment" "applied" \
    "$(sed -n 's/^applied=//p' "$work/profile.txt")"
check "the accepted profile left the mode's own carrier in force" \
    "http://127.0.0.1:1" "$(sed -n 's/^base_url=//p' "$work/profile.txt")"

# The REFUSAL: same profile, same file, only the order changed.
rm -f "$work/profile.txt"
rc=0
profile_run after selftest || rc=$?
check "a profile loaded AFTER the connection-mode apply is refused" "2" "$rc"
if grep -qF 'AFTER the connection-mode apply' "$work/profile.log"; then
    echo "PASS: the late-load refusal names the ordering"
else
    fail "the late-load refusal did not name the ordering"
    sed -n '1,5p' "$work/profile.log"
fi
check "the refused late load exported nothing" "no" \
    "$([ -f "$work/profile.txt" ] && echo yes || echo no)"

# The forbidden classes the README states, each refused by name, each
# against the same accepted-order call the control above proved works --
# so a refusal is attributable to the profile's content and to nothing else.
profile_refuses() {
    local label="$1" body="$2" needle="$3" rc=0
    printf '%s\n' "$body" >"$PROFILES/selftest-bad.env"
    profile_run before selftest-bad || rc=$?
    if [ "$rc" = 0 ]; then
        fail "$label -- the loader ACCEPTED it"
    elif ! grep -qF -- "$needle" "$work/profile.log"; then
        fail "$label -- refused, but the reason did not name '$needle'"
        sed -n '1,5p' "$work/profile.log"
    else
        echo "PASS: $label"
    fi
    rm -f "$PROFILES/selftest-bad.env"
}

# A connection carrier is THE fault this seam exists to prevent: the one
# that would silently repoint the client at the operator's live daemon.
profile_refuses "a profile naming ANTHROPIC_BASE_URL is refused" \
    'ANTHROPIC_BASE_URL=http://127.0.0.1:65535' "connection carrier"
profile_refuses "a profile naming a proxy carrier is refused" \
    'HTTPS_PROXY=http://127.0.0.1:65535' "connection carrier"
profile_refuses "a profile naming the MITM trust path is refused" \
    'NODE_EXTRA_CA_CERTS=/tmp/not-a-ca.pem' "connection carrier"
profile_refuses "a profile naming a provider credential is refused" \
    'ANTHROPIC_API_KEY=sk-not-a-real-key' "credential"
profile_refuses "a profile naming a bearer is refused" \
    'SOME_BEARER=nope' "credential"
# The two substitution legs are single-quoted on purpose: the fixture is
# the LITERAL `$(...)` / backtick a profile would carry, so an expansion
# here would test nothing.
# shellcheck disable=SC2016
profile_refuses "a profile value carrying a shell substitution is refused" \
    'MAX_THINKING_TOKENS=$(id -u)' "shell substitution"
# shellcheck disable=SC2016
profile_refuses "a profile value carrying a backtick is refused" \
    'MAX_THINKING_TOKENS=`id -u`' "shell substitution"
profile_refuses "a profile line that is not key=value is refused" \
    'rm -rf /' "non-key=value line"
profile_refuses "a profile key that is not a variable name is refused" \
    'not a key=1' "invalid key"

# THE ACCEPT SET, drawn from the keys a profile actually exists to carry.
# The credential rule classifies untrusted key NAMES, so moving its
# boundary moves it in both directions: a substring test on `TOKEN` refuses
# `MAX_THINKING_TOKENS`, which is the thinking-tier knob -- a body-shape
# axis and the most likely content of the first real profile. Without these
# legs a tightened rule reads as a stricter gate instead of a broken seam.
profile_accepts() {
    local label="$1" body="$2" rc=0
    printf '%s\n' "$body" >"$PROFILES/selftest-ok.env"
    profile_run before selftest-ok || rc=$?
    if [ "$rc" = 0 ]; then
        echo "PASS: $label"
    else
        fail "$label -- the loader REFUSED it"
        sed -n '1,3p' "$work/profile.log"
    fi
    rm -f "$PROFILES/selftest-ok.env"
}
profile_accepts "the thinking-tier knob is accepted" 'MAX_THINKING_TOKENS=8192'
profile_accepts "a client feature flag is accepted" 'DISABLE_PROMPT_CACHING=1'
profile_accepts "an MCP config path is accepted" 'CLAUDE_MCP_CONFIG=mcp.json'
profile_accepts "a comment and a blank line are accepted" '# a profile
'

# The closed set is closed: a name is a committed profile in this
# directory or it is nothing. No path argument, no traversal.
rc=0
profile_run before no-such-profile || rc=$?
check "an uncommitted profile name is refused" "2" "$rc"
if grep -qF 'no committed client profile' "$work/profile.log"; then
    echo "PASS: the refusal names the missing committed profile"
else
    fail "the refusal did not name the missing committed profile"
fi
rc=0
profile_run before ../config/anthropic-api || rc=$?
check "a profile name with a path component is refused" "2" "$rc"
if grep -qF 'closed set' "$work/profile.log"; then
    echo "PASS: the traversal refusal names the closed set"
else
    fail "the traversal refusal did not name the closed set"
fi
rm -rf "$work"

# ZERO profiles are committed, and the README is what carries the rules
# until one is. Each grep names a LOAD-BEARING statement rather than a
# heading: a heading can be reworded, and a rule the README stops stating
# is a rule the first profile author will get wrong.
committed_profiles="$(find "$DRIVERS/profiles" -maxdepth 1 -name '*.env' 2>/dev/null | wc -l)"
check "the committed profile set is still empty" "0" "$committed_profiles"
PROFILES_README="$DRIVERS/profiles/README.md"
check "the profiles directory carries its README" "yes" \
    "$([ -r "$PROFILES_README" ] && echo yes || echo no)"
readme_states() {
    check "the profiles README states $1" "yes" \
        "$(grep -qF -- "$2" "$PROFILES_README" && echo yes || echo no)"
}
readme_states "the closed-set rule" "The set of profiles is CLOSED"
readme_states "that a profile is parsed, not sourced" "PARSED, not sourced"
readme_states "the forbidden connection carriers" "ANTHROPIC_BASE_URL"
readme_states "the forbidden credential suffixes" "_BEARER"
readme_states "the forbidden shell substitution" "backtick"
readme_states "the before-apply half of the ordering rule" \
    "BEFORE \`driver_apply_anthropic_connection_mode\`"
readme_states "the after-apply refusal" "load after it is REFUSED"
readme_states "the deferred client-side pin" "client_config_sha"

# Positive control: every grep above MUST fire on a file that does carry
# the statement, and MUST NOT on one that does not. Without it a mistyped
# needle reads as a clean pass on a README that says nothing.
readme_control="$(mktemp)"
printf 'nothing load-bearing here\n' >"$readme_control"
control_misses=0
while IFS= read -r needle; do
    grep -qF -- "$needle" "$PROFILES_README" || control_misses=$((control_misses + 1))
    ! grep -qF -- "$needle" "$readme_control" || control_misses=$((control_misses + 1))
done <<'NEEDLES'
The set of profiles is CLOSED
PARSED, not sourced
load after it is REFUSED
NEEDLES
check "the README greps fire on the README and not on an empty control" "0" \
    "$control_misses"
rm -f "$readme_control"

# No `client_config_sha` anywhere yet: it lands with the first profile, and
# an always-empty pin answers neither question the two-pin design separates.
check "no script writes a client_config_sha pin" "0" \
    "$(grep -rl 'client_config_sha' "$RIG" "$RUNNER" "$DRIVERS" \
        --exclude=README.md 2>/dev/null | wc -l)"

if [ "$fails" -gt 0 ]; then
    echo "drivers self-test: $fails failure(s)"
    exit 1
fi
echo "drivers self-test: all assertions passed"
