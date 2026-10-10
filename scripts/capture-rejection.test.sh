#!/usr/bin/env bash
# Self-test for capture-rejection.sh. Exits 0 when all assertions pass,
# non-zero otherwise.
#
# Every case drives the REAL script inside a throwaway repo carrying the
# REAL scrub gate, against a synthetic daemon log, writing into a scratch
# output root. The scrub gate derives its deny set from `$HOME`, the git
# identity, the hostname and the seat store, so every run uses a fake home,
# a repo-local git identity, a `hostname` stub and a throwaway
# XDG_CONFIG_HOME -- otherwise whoever runs the suite decides the verdict.
#
# Every refusal is paired with the clean capture of the same log shape, so
# a script that refuses everything fails the suite.
#
# Requires python3.
#
# Run it from anywhere:
#   bash scripts/capture-rejection.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CAPTURE="$HERE/capture-rejection.sh"
SCRUB="$HERE/scrub-fixture.sh"
LOG_SAFE_SRC="$HERE/../crates/routectl-core/src/log_safe.rs"

read -r -d '' -a git_local_env < <(git rev-parse --local-env-vars)
unset "${git_local_env[@]}"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CEILING_DIRECTORIES="${TMPDIR:-/tmp}"

fails=0

FAKE_GIT_NAME="Ada Contributor"
FAKE_GIT_EMAIL="ada@example.invalid"
FAKE_HOSTNAME="devbox-17"

ID_A="019eab77-0000-4000-8000-0000000000a1"
ID_B="019eab77-0000-4000-8000-0000000000b2"
ID_C="019eab77-0000-4000-8000-0000000000c3"
NOTE="Synthetic rejection for the capture self-test."
BODY_A='{"type":"error","error":{"type":"invalid_request_error","message":"thinking.enabled.display: Extra inputs are not permitted"}}'
BODY_B='{"type":"error","error":{"type":"invalid_request_error","message":"a different rejection"}}'

if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 not found; this self-test cannot run the capture script"
    exit 1
fi

pass() { echo "PASS: $1"; }
fail() {
    echo "FAIL: $1"
    fails=$((fails + 1))
}

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        pass "$label"
    else
        fail "$label -- expected '$expected', got '$actual'"
    fi
}

make_work() {
    local work
    work="$(mktemp -d)"
    mkdir -p "$work/repo/scripts" "$work/out" "$work/home/acontributor" \
        "$work/stubbin" "$work/xdg/routectl"
    cp "$CAPTURE" "$work/repo/scripts/capture-rejection.sh"
    cp "$SCRUB" "$work/repo/scripts/scrub-fixture.sh"
    printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$FAKE_HOSTNAME" >"$work/stubbin/hostname"
    chmod +x "$work/stubbin/hostname"
    (
        cd "$work/repo" || exit 2
        git init -q .
        git config user.name "$FAKE_GIT_NAME"
        git config user.email "$FAKE_GIT_EMAIL"
    ) >/dev/null 2>&1
    printf '%s\n' "$work"
}

# Run the real script from inside the throwaway repo with `--log
# <work>/daemon.log --out-root <work>/out` prepended. Output lands in
# `<work>/capture.log`.
capture() {
    local work="$1"
    shift
    local rc=0
    (
        cd "$work/repo" || exit 2
        HOME="$work/home/acontributor" \
            XDG_CONFIG_HOME="$work/xdg" \
            PATH="$work/stubbin:$PATH" \
            bash scripts/capture-rejection.sh \
            --log "$work/daemon.log" --out-root "$work/out" "$@"
    ) >"$work/capture.log" 2>&1 || rc=$?
    return "$rc"
}

# One DEBUG `upstream error body` event as the production subscriber renders
# it: span chain, target, message, then the emitter's fields in order.
# $1 id, $2 provider_kind, $3 status, $4 body.
error_body_line() {
    local span="request{method=POST path=/v1/messages request_id=$1}:complete_with_options{alias=my-alias}:complete{provider=$2:p model=m}"
    printf '2026-10-09T10:00:00.000000Z DEBUG %s: routectl_core::log_safe: upstream error body provider_kind="%s" provider="p" status=%s body=%s\n' \
        "$span" "$2" "$3" "$4"
}

# The router's class-decision event for the same attempt, carrying the
# lifted discriminator. $1 id, $2 upstream_type.
class_decision_line() {
    printf '2026-10-09T10:00:00.100000Z DEBUG request{method=POST path=/v1/messages request_id=%s}:complete_with_options{alias=my-alias}: routectl_router::router::dispatch: router failure class decision provider="p" model="m" surface="complete" status=Some(400) upstream_type="%s" original_class="bad_request"\n' \
        "$1" "$2"
}

# The fixture bytes the script must write.
expected_fixture() {
    local status="$1" lifted="$2" body="$3"
    python3 -c '
import json, sys
fx = {"note": sys.argv[1], "status": int(sys.argv[2])}
if sys.argv[3]:
    fx["lifted_type"] = sys.argv[3]
fx["body"] = sys.argv[4]
sys.stdout.write(json.dumps(fx, indent=2) + "\n")
' "$NOTE" "$status" "$lifted" "$body"
}

fixture_matches() {
    local label="$1" file="$2" expected="$3"
    # `$expected` arrived through a command substitution, which strips the
    # one trailing newline the script writes; printf puts it back.
    if [ -f "$file" ] && cmp -s "$file" <(printf '%s\n' "$expected"); then
        pass "$label"
    else
        fail "$label"
        [ -f "$file" ] && cat "$file"
    fi
}

assert_captured() {
    local label="$1" work="$2" file="$3" expected="$4"
    shift 4
    local rc=0
    capture "$work" "$@" || rc=$?
    check "$label: exit 0" "0" "$rc"
    [ "$rc" -eq 0 ] || cat "$work/capture.log"
    fixture_matches "$label: byte-exact fixture" "$file" "$expected"
}

# A refusal writes nothing anywhere under the output root.
assert_refused() {
    local label="$1" work="$2" want_rc="$3" needle="$4"
    shift 4
    local rc=0
    capture "$work" "$@" || rc=$?
    check "$label: exit $want_rc" "$want_rc" "$rc"
    check "$label: no file written" "" "$(find "$work/out" -type f)"
    if grep -qF -- "$needle" "$work/capture.log"; then
        pass "$label: names the reason"
    else
        fail "$label: '$needle' absent from the output"
        cat "$work/capture.log"
    fi
}

# --- the synthetic line is a faithful replica of the emitter ----------
if [ -f "$LOG_SAFE_SRC" ]; then
    emitter_fields="$(python3 - "$LOG_SAFE_SRC" <<'PY'
import re, sys
src = open(sys.argv[1], encoding="utf-8").read()
m = re.search(r'tracing::debug!\(\s*(.*?)"upstream error body"\s*\);', src, re.S)
names = [] if not m else [f.strip().split("=")[0].strip() for f in m.group(1).split(",") if f.strip()]
print(" ".join(names))
PY
)"
    check "the emitter writes provider_kind, provider, status, body in that order" \
        "provider_kind provider status body" "$emitter_fields"
    for marker in '... [truncated at {cap} bytes]' '(json body, {} bytes, not excerpted)'; do
        if grep -qF -- "$marker" "$LOG_SAFE_SRC"; then
            pass "the refused marker '$marker' is one log_safe writes"
        else
            fail "log_safe no longer writes '$marker'; the refusal is stale"
        fi
    done
else
    pass "no crates tree in this checkout; the emitter weld is not asserted"
fi

# --- a matching id yields a byte-exact fixture; other ids are ignored -
work="$(make_work)"
{
    error_body_line "$ID_B" anthropic 400 "$BODY_B"
    class_decision_line "$ID_B" not_the_target_type
    # Another request whose BODY quotes the target id: a content copy, not
    # a span, so it must never select the line.
    error_body_line "$ID_C" anthropic 400 "{\"message\":\"request_id=$ID_A\"}"
    # An id that merely starts with the target id.
    error_body_line "${ID_A}9" anthropic 400 "$BODY_B"
    error_body_line "$ID_A" anthropic 400 "$BODY_A"
    class_decision_line "$ID_A" invalid_request_error
} >"$work/daemon.log"
assert_captured "matching id with a lifted type" "$work" \
    "$work/out/anthropic-api/display-field.json" \
    "$(expected_fixture 400 invalid_request_error "$BODY_A")" \
    --request-id "$ID_A" --dialect anthropic-api --case display-field --note "$NOTE"
check "only the requested fixture was written" "$work/out/anthropic-api/display-field.json" \
    "$(find "$work/out" -type f)"

# The same log drives the other id to ITS envelope, so the selection above
# is by id rather than by position.
assert_captured "the other id in the same log" "$work" \
    "$work/out/anthropic-api/other.json" \
    "$(expected_fixture 400 not_the_target_type "$BODY_B")" \
    --request-id "$ID_B" --dialect anthropic-api --case other --note "$NOTE"
rm -rf "$work"

# --- no class-decision line: no lifted_type key --------------------------
work="$(make_work)"
{
    error_body_line "$ID_A" bedrock-converse 400 '{"message":"The model returned the following errors: bad field"}'
    class_decision_line "$ID_B" ValidationException
} >"$work/daemon.log"
assert_captured "bedrock-converse without a lifted type" "$work" \
    "$work/out/bedrock/converse.json" \
    "$(expected_fixture 400 "" '{"message":"The model returned the following errors: bad field"}')" \
    --request-id "$ID_A" --dialect bedrock --case converse --note "$NOTE"
rm -rf "$work"

# --- refusals ------------------------------------------------------------
work="$(make_work)"
{
    error_body_line "$ID_A" anthropic 400 "$BODY_A"
    error_body_line "$ID_A" anthropic 400 "$BODY_B"
} >"$work/daemon.log"
assert_refused "two differing bodies for one request" "$work" 1 "differing" \
    --request-id "$ID_A" --dialect anthropic-api --case c --note "$NOTE"
rm -rf "$work"

work="$(make_work)"
error_body_line "$ID_A" anthropic 400 "$BODY_A" >"$work/daemon.log"
assert_refused "unknown dialect" "$work" 2 "unknown --dialect" \
    --request-id "$ID_A" --dialect gemini --case c --note "$NOTE"
assert_refused "no line for the request id" "$work" 1 "no DEBUG \`upstream error body\` line" \
    --request-id "$ID_B" --dialect anthropic-api --case c --note "$NOTE"
assert_refused "a line from another dialect" "$work" 1 "seen provider_kind: anthropic" \
    --request-id "$ID_A" --dialect openai-responses --case c --note "$NOTE"
rm -rf "$work"

work="$(make_work)"
error_body_line "$ID_A" openai-compat 400 '{"error":{"message":"abc"}}... [truncated at 4096 bytes]' >"$work/daemon.log"
assert_refused "a body capped with a truncation marker" "$work" 1 "[truncated at N bytes]" \
    --request-id "$ID_A" --dialect openai-compat --case c --note "$NOTE"
rm -rf "$work"

work="$(make_work)"
error_body_line "$ID_A" openai-responses 400 '(json body, 70000 bytes, not excerpted)' >"$work/daemon.log"
assert_refused "a body elided as not excerpted" "$work" 1 "not excerpted" \
    --request-id "$ID_A" --dialect openai-responses --case c --note "$NOTE"
rm -rf "$work"

work="$(make_work)"
error_body_line "$ID_A" anthropic 400 '{"error":{"message":"see /home/somebody/project"}}' >"$work/daemon.log"
assert_refused "a scrub --check finding" "$work" 1 "residual personal data" \
    --request-id "$ID_A" --dialect anthropic-api --case c --note "$NOTE"
rm -rf "$work"

# --- an existing fixture is kept unless --force --------------------------
work="$(make_work)"
error_body_line "$ID_A" anthropic 400 "$BODY_A" >"$work/daemon.log"
mkdir -p "$work/out/anthropic-api"
printf 'previous\n' >"$work/out/anthropic-api/c.json"
rc=0
capture "$work" --request-id "$ID_A" --dialect anthropic-api --case c --note "$NOTE" || rc=$?
check "existing fixture without --force: exit 1" "1" "$rc"
check "existing fixture without --force: left untouched" "previous" "$(cat "$work/out/anthropic-api/c.json")"
assert_captured "existing fixture with --force" "$work" \
    "$work/out/anthropic-api/c.json" "$(expected_fixture 400 "" "$BODY_A")" \
    --request-id "$ID_A" --dialect anthropic-api --case c --note "$NOTE" --force
rm -rf "$work"

if [ "$fails" -ne 0 ]; then
    echo "capture-rejection.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "capture-rejection.test.sh: all assertions passed"
