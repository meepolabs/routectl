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

# The set covers the four PATTERNS the corpus exists to pin. A set missing
# one is a corpus that cannot detect drift in that shape at all.
patterns="$(
    for path in "${case_files[@]}"; do
        python3 "$VALIDATOR" --field wire_pattern "$path"
    done | sort -u
)"
for pattern in tool-use-multiturn cache-breakpoints thinking large-context; do
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

# The current large-context-01 case puts its padding in FILES the client is
# asked to read, so the request it produces carries only the prompt plus
# the client's own preamble. Its refusal here is the EXPECTED outcome and
# the observation the case definition is settled against -- the floor is
# not tuned to make it pass. Built from the committed baseline fixture's
# real captured body (the observed size of a first request) plus the case's
# own first-turn prompt.
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
denies "the current large-context case shape is refused as expected" \
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

denies "a pattern with no predicate is refused rather than waved through" \
    "$pat/baseline-disabled" mcp-tools "no predicate"

# The refusal NAMES the token and its deferred status. A generic "no
# predicate" reason would read the same for a typo'd pattern as for the one
# deliberately held back, and the deferred list is the only place the
# distinction is recorded -- a token missing from BOTH the table and that
# list is an oversight the message must not describe as a plan.
denies "the refusal names the token it has no predicate for" \
    "$pat/baseline-disabled" mcp-tools "'mcp-tools'"
denies "the refusal names the deferred set the token belongs to" \
    "$pat/baseline-disabled" mcp-tools "deferred: mcp-tools"

# --- the shared classification set ------------------------------------
# The structural lines the three structural predicates are asserted
# against, so an implementation of them cannot drift unnoticed from what a
# shape means. Only the Python side reads the set today; the Rust half of
# the cross-check is not wired up yet.

# The header is the comment block ahead of the first record, read as such
# rather than as a fixed line count: a needle pinned to `head -N` passes or
# fails on where the sentence sits rather than on whether it is there.
class_header="$(sed -n '/^[^#]/q;p' "$CLASSIFICATION")"
if printf '%s' "$class_header" | grep -qF "baseline, thinking,"; then
    echo "PASS: the classification set states its three-predicate scope"
else
    fail "the classification set header does not state which predicates it covers"
fi
# The header must not claim a cross-check that does not run yet: a reader
# who believes the Rust side consumes this file reads an undetected
# divergence as a checked one.
if printf '%s' "$class_header" | grep -qF "does not exist yet"; then
    echo "PASS: the classification set header states which side reads it today"
else
    fail "the classification set header does not say the Rust half is not wired up"
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
for pattern in tool-use-multiturn large-context; do
    if printf '%s' "$(s_line ingress 0 disabled 0)" | \
        python3 "$VERIFIER" --structural-line "$pattern" 2>/dev/null; then
        fail "a structural line satisfied the body-census pattern $pattern"
    else
        echo "PASS: a structural line cannot classify the $pattern census"
    fi
done

rm -rf "$pat"

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

# ---------------------------------------------------------------------
# Fixtures for the end-to-end cases
# ---------------------------------------------------------------------

# The canned trace the stub daemon emits on stderr: a complete non-stream
# request carrying BOTH request-side structural summaries, which driver
# mode requires before it will land a fixture. The user-agent is what the
# rig parses `meta.client.version` out of, so the version this trace
# carries is the one a landed fixture must report. Every value is
# synthetic.
canned_trace() {
    local id="019eab77-0000-4000-8000-0000000000e2"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    local structural='kind="anthropic" id=p model=claude-sonnet-4-5 max_tokens=64 thinking_shape="" output_config_effort="" tool_choice_shape="" cache_control_count=0 messages_len=2 tools_len=0 anthropic_beta="" provider_extras_keys="" stream=false'
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
2026-08-26T10:00:00.500000Z TRACE $span: $target structural summary direction="outgoing" $structural
TRACE
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
# When the runner passes `--mitm-port` (front-proxy mode), the stub mints
# the CA under the run's XDG root exactly where the real daemon does at
# listener start, so the CA path the runner exports names a readable file.
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
# `STUB_CLIENT_RC` makes it fail on demand, which is how the driver's own
# failure propagation is asserted. `STUB_CLIENT_VERSION` empty is how a
# client that cannot state its version is asserted.
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
{
    printf 'invocation argv=%s\n' "$*"
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
# against both stubs. `--keep` is deliberate: the driver records the client
# version into the run workspace, and a removed workspace could not be
# asserted on. The kept path is read back out of the runner's own log.
#
# The driver path is ABSOLUTE. The runner runs its driver command with cwd
# set to the run's throwaway git repo, so a repo-relative path would
# resolve against that workspace instead of against the checkout.
#
# Usage: driver_run <work> <driver-relative-path> <case-id> [extra runner flags...]
driver_run() {
    local work="$1" driver="$2" case_id="$3"
    shift 3
    local port rc=0
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
            bash scripts/capture_driver.sh --work "$work/runs" --keep \
            --case "$case_id" --lane "${DRIVER_LANE:-anthropic-api}" "$@" \
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
        check "$driver: meta.client.connection_mode is populated" "base-url" \
            "$(meta_get "$meta" client.connection_mode)"
    else
        fail "$driver: no fixture landed at $meta"
        sed -n '1,30p' "$work/runner.log"
        fails=$((fails + 3))
    fi

    [ -n "$kept" ] && rm -rf "$kept"
    rm -rf "$work"
done

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
if client_get "$work" argv | sed -n '2p' | grep -q -- '--resume'; then
    echo "PASS: a multi-turn print run resumes rather than reopening"
else
    fail "a multi-turn print run did not resume the first turn's session"
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

if [ "$fails" -gt 0 ]; then
    echo "drivers self-test: $fails failure(s)"
    exit 1
fi
echo "drivers self-test: all assertions passed"
