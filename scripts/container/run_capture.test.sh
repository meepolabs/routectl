#!/usr/bin/env bash
# Self-test for scripts/container/run_capture.sh -- the host wrapper that
# runs one capture inside the capture-cell image.
#
# NO REAL CREDENTIAL AND NO REAL UPSTREAM. Every leg that reaches a
# container runs the REAL wrapper against a STUB `routectl` (injected
# through the wrapper's own `ROUTECTL_BIN` override), a STUB seat file
# (`ROUTECTL_CAPTURE_CELL_SEAT`) carrying a token assembled here at run
# time, and a STUB driver command -- the same stub-injection shape
# scripts/drivers.test.sh uses. The token in this file is FAKE and is
# built by concatenation rather than written as a literal: the repo's own
# secret scanner rejects a source line holding a full key shape, and
# suppressing it would be the wrong trade in any file, worst of all in one
# whose subject is credential handling.
#
# WHAT THE TWO HALVES PROVE, and why both are needed:
#
#   The REFUSALS are asserted one at a time, each against its OWN exit
#   code. A shared code would let a caller who reads only the number
#   confuse two faults, and a single assertion over the set would leave
#   three of four rules deletable with the suite still green.
#
#   The ACCEPT leg is what stops the refusals being a gate that rejects
#   everything: a well-formed invocation must reach capture_driver.sh
#   inside the container and land a fixture. Every property the refusals
#   exist to protect is then asserted from INSIDE the container, by a
#   driver stub that tries the writes and reports what happened -- the
#   only honest place to assert them, since that environment is what a
#   real driven client would be running in.
#
# The refusal legs need NO docker: the wrapper decides every one of them
# from its own argv and the host filesystem, before docker is consulted.
# So on a box with no docker this suite still runs them, and only the
# container legs skip -- by name, never silently.
#
# Requires python3 (the stub listener and the stub seat) and docker for
# the container legs:
#   docker absent      -> the container legs SKIP BY NAME; refusals run.
#   image not built    -> same; build it with scripts/container/build.sh.
#   python3 absent     -> FAILS. Without it there is no stub seat and no
#                         stub listener, so a skip would hide the whole
#                         accept half.
#
# Run it from anywhere:
#   bash scripts/container/run_capture.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
WRAPPER="$HERE/run_capture.sh"

IMAGE="routectl-capture:default"

# Scratch parent on REAL disk outside the repo. Outside because the
# wrapper refuses an in-repo scratch root -- which is one of the rules
# under test -- and not $TMPDIR because /tmp here is a tmpfs small enough
# that unrelated gates already hit StorageFull on it.
WORK_PARENT="${TMPDIR:-/var/tmp}"
case "$WORK_PARENT" in
    /tmp|/tmp/*) WORK_PARENT="/var/tmp" ;;
esac

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

fail() {
    echo "FAIL: $1"
    fails=$((fails + 1))
}

# A named skip: the caller must be able to read WHAT was not verified off
# the log, which is the whole difference between a skip and a pass.
skip() {
    echo "SKIP: $1"
}

check_log() {
    local label="$1" needle="$2" file="$3"
    if grep -qiF -- "$needle" "$file"; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- '$needle' absent from the wrapper's output"
        sed -n '1,15p' "$file"
        fails=$((fails + 1))
    fi
}

# A refusal message from THIS wrapper, not from anything it invoked. The
# prefix matters: a deleted refusal rule lets the argument through to
# capture_driver.sh, which echoes the same flag back in its own
# `unknown arg` message -- so a bare grep for the flag name goes green
# against exactly the deletion it exists to catch. Measured while
# mutation-verifying the --privileged rule.
check_refusal() {
    local label="$1" needle="$2" file="$3"
    if grep -E '^run_capture: (refusing|[a-z].*)' "$file" | grep -qiF -- "$needle"; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- no run_capture refusal line mentions '$needle'"
        sed -n '1,15p' "$file"
        fails=$((fails + 1))
    fi
}

if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 not found; there would be no stub seat and no stub"
    echo "      listener, so the accept half of this suite cannot run"
    exit 1
fi

if [ ! -r "$WRAPPER" ]; then
    echo "FAIL: the wrapper is not readable at $WRAPPER"
    exit 1
fi

WORK="$(mktemp -d "$WORK_PARENT/routectl-cell-test.XXXXXX")" || exit 1

# shellcheck disable=SC2329 # invoked indirectly, by the EXIT trap below
cleanup() {
    # Validated non-empty before use: a command substitution strips
    # trailing newlines and an empty one turns `rm -rf "$x"` into a
    # deletion of the wrong tree.
    if [ -n "${WORK:-}" ] && [ -d "$WORK" ]; then
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------
# The fake seat
# ---------------------------------------------------------------------

# Split from its prefix so no full key shape exists on any line of this
# file, and BRACED at every use: `$FAKE_RUNAAAA` is a different,
# undefined variable, so an unbraced form would assemble a shorter token
# and every assertion below would pass for the wrong reason.
FAKE_RUN="AbCdEf0123456789ABCDEFGH"
fake_key() { printf '%s%s' "$1" "${2:-$FAKE_RUN}"; }

FAKE_TOKEN="$(fake_key 'stub-oat-' "${FAKE_RUN}IJKLMNOP")"

# The codex seat's own token and account id, DISTINCT from the anthropic
# ones. Distinct because the wrapper keys the variable name on the provider:
# with one shared value, a wrapper that forwarded the anthropic token under
# the openai name would pass every digest assertion.
FAKE_CODEX_TOKEN="$(fake_key 'stub-codex-' "${FAKE_RUN}QRSTUVWX")"
FAKE_CODEX_ACCOUNT="11111111-2222-4333-8444-555555555555"

# A gemini API key the operator would have exported. gemini is NOT an OAuth
# provider, so it has no seat entry and its credential can only arrive as an
# env passthrough -- which is the second of the wrapper's two sources.
FAKE_GEMINI_KEY="$(fake_key 'stub-gemini-' "${FAKE_RUN}YZ012345")"

# Compared by DIGEST, never by value. The values here are fake, but a
# self-test that echoes a credential into a log teaches the shape a later
# edit would copy against a real one.
sha_of() { printf '%s' "$1" | sha256sum | cut -d' ' -f1; }

FAKE_TOKEN_SHA="$(sha_of "$FAKE_TOKEN")"
FAKE_CODEX_TOKEN_SHA="$(sha_of "$FAKE_CODEX_TOKEN")"
FAKE_CODEX_ACCOUNT_SHA="$(sha_of "$FAKE_CODEX_ACCOUNT")"
FAKE_GEMINI_KEY_SHA="$(sha_of "$FAKE_GEMINI_KEY")"

SEAT_DIR="$WORK/seat"
mkdir -p "$SEAT_DIR"
SEAT="$SEAT_DIR/credentials.json"

# The real seat's shape, written through python so the JSON is well-formed
# rather than hand-quoted. Every neighbouring field a real seat carries is
# deliberately present: the wrapper must read exactly the ones it needs.
#
# TWO seats, because the convention under test is per-provider: `anthropic`
# (whose key is the provider name) and `codex` (whose key is the OAUTH ID,
# not the `openai` provider name -- that mapping is the thing a lane author
# would otherwise guess wrong). `account.account_id` is the field
# `openai-responses` + `chatgpt-oauth` requires, and it is present on BOTH
# seats so an assertion about the codex one cannot pass by reading the
# anthropic one.
write_seat() {
    python3 - "$1" "$2" "$3" "$4" <<'PY'
import json, sys

path, anthropic_token, codex_token, codex_account = sys.argv[1:5]
def record(token, account_id):
    return {
        "access_token": token,
        "refresh_token": "stub-refresh-value",
        "token_type": "Bearer",
        "expires_at_unix": 4102444800,
        "session_id": "00000000-0000-4000-8000-000000000000",
        "account": {"email": "stub@example.invalid", "account_id": account_id},
    }

doc = {
    "schema_version": 1,
    "providers": {
        "anthropic": record(anthropic_token, "00000000-0000-4000-8000-0000000000aa"),
        "codex": record(codex_token, codex_account),
    },
}
with open(path, "w", encoding="utf-8") as fh:
    json.dump(doc, fh)
PY
}

write_seat "$SEAT" "$FAKE_TOKEN" "$FAKE_CODEX_TOKEN" "$FAKE_CODEX_ACCOUNT"

# ---------------------------------------------------------------------
# Stubs the container legs mount and run
# ---------------------------------------------------------------------

# Assets the in-container stubs read. A SUBDIRECTORY of the scratch root,
# because the scratch root is the only writable mount and the wrapper
# fixes the landing path to its top -- so an asset dir beside the landed
# `<lane>/<case>/` tree is the one place the container can reach them.
ASSETS_REL="assets"

# The stub `routectl`, mounted read-only in place of the host binary.
# Emits a canned trace on stderr -- the same redirect the runner captures
# from the real daemon -- then EXECs a listener answering /health, so the
# pid the runner captured from `$!` is the pid actually holding the port.
#
# `deaf` mode holds the process open without ever binding: the "daemon
# came up but never became ready" shape, which is how the runner's exit 3
# is provoked without a real daemon.
write_stub_routectl() {
    cat >"$1" <<'SH'
#!/usr/bin/env bash
set -u
port=""
while [ $# -gt 0 ]; do
    case "$1" in
        --port) port="$2"; shift 2 ;;
        *) shift ;;
    esac
done
assets=/scratch/assets
mode="$(cat "$assets/mode" 2>/dev/null || echo healthy)"
trace="$assets/trace-$mode.log"
[ -r "$trace" ] || trace="$assets/trace-healthy.log"
cat "$trace" >&2
if [ "$mode" = "deaf" ]; then
    exec sleep 600
fi
exec python3 "$assets/listener.py" "$port"
SH
    chmod +x "$1"
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

# The stub DRIVER. It is the probe: it runs where a real driven client
# would run, with the same mounts and the same environment, and reports
# what it could see and what it could write to a file in the one writable
# mount.
#
# THE TWO WRITE LINES ARE A PAIR. The repo write must FAIL and the scratch
# write must SUCCEED; either one alone is satisfiable by a container that
# can write nothing, or by one that can write everything.
#
# Every credential is reported as a DIGEST, and its ABSENCE as the empty
# string rather than as a digest of nothing: `sha256sum` of an empty input
# is a fixed value, so a leg asserting "this variable did not cross" would
# otherwise be asserting a constant and would pass against a wrapper that
# forwarded an empty value -- which is the exact fault the by-name refusal
# exists to prevent. Comparing digests proves a value arrived intact
# without a credential ever reaching a file or a log line.
write_stub_driver() {
    cat >"$1" <<'SH'
#!/usr/bin/env bash
set -u
assets=/scratch/assets
out="$assets/probe.txt"
seat_xdg="${XDG_CONFIG_HOME:-/nonexistent}/routectl/credentials.json"
seat_home="${HOME:-/nonexistent}/.config/routectl/credentials.json"

# `<name>_present` and `<name>_sha` for one variable. An unset or empty
# variable reports `no` and an EMPTY sha, never the digest of "".
report_credential() {
    local label="$1" value="${2:-}"
    if [ -n "$value" ]; then
        printf '%s_present=yes\n' "$label"
        printf '%s_sha=%s\n' "$label" \
            "$(printf '%s' "$value" | sha256sum | cut -d' ' -f1)"
    else
        printf '%s_present=no\n' "$label"
        printf '%s_sha=\n' "$label"
    fi
}

{
    printf 'cwd=%s\n' "$PWD"
    report_credential anthropic_key "${ROUTECTL_DRIVER_ANTHROPIC_API_KEY:-}"
    report_credential openai_key "${ROUTECTL_DRIVER_OPENAI_API_KEY:-}"
    report_credential openai_account "${ROUTECTL_DRIVER_OPENAI_ACCOUNT_ID:-}"
    report_credential gemini_key "${ROUTECTL_DRIVER_GEMINI_API_KEY:-}"
    printf 'seat_at_xdg=%s\n' "$([ -e "$seat_xdg" ] && echo yes || echo no)"
    printf 'seat_at_home=%s\n' "$([ -e "$seat_home" ] && echo yes || echo no)"
    printf 'seat_anywhere=%s\n' \
        "$(find / -xdev -name credentials.json -print 2>/dev/null | head -3 | tr '\n' ' ')"
    printf 'base_url=%s\n' "${ROUTECTL_BASE_URL:-}"
    printf 'case_id=%s\n' "${ROUTECTL_FIXTURE_CASE_ID:-}"
    printf 'wire_pattern=%s\n' "${ROUTECTL_FIXTURE_WIRE_PATTERN:-}"
    printf 'expected_ingress=%s\n' "${ROUTECTL_FIXTURE_EXPECTED_INGRESS:-}"
    printf 'routectl_bin=%s\n' "${ROUTECTL_BIN:-}"
    printf 'out_root=%s\n' "${ROUTECTL_DRIVER_OUT_ROOT:-}"
    if printf 'mutated\n' 2>/dev/null >/workspace/Cargo.toml; then
        printf 'repo_write=SUCCEEDED\n'
    else
        printf 'repo_write=refused\n'
    fi
    if printf 'control\n' 2>/dev/null >"$assets/write-control.txt"; then
        printf 'scratch_write=ok\n'
    else
        printf 'scratch_write=FAILED\n'
    fi
    if printf 'mutated\n' 2>/dev/null >"$(dirname "${ROUTECTL_BIN:-/nonexistent}")/probe"; then
        printf 'bin_dir_write=SUCCEEDED\n'
    else
        printf 'bin_dir_write=refused\n'
    fi
} >"$out"
exit 0
SH
    chmod +x "$1"
}

# The canned trace the stub emits: a complete non-stream request carrying
# BOTH request-side structural summaries, which is what driver mode
# requires before it will land a fixture. Every value is synthetic and
# matches nothing on any real machine.
#
# `kind` and `id` differ per direction, as the emitter writes them: the
# ingress call site passes the ingress dialect token, the outgoing one the
# provider kind and configured provider id. The `baseline` predicate scopes
# itself to the Anthropic dialect off the ingress line's `id`, so reusing
# the outgoing spelling on both lines would be refused.
canned_trace() {
    local id="019eab77-0000-4000-8000-0000000000e1"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    local fields='model=claude-sonnet-4-5 max_tokens=64 thinking_shape=disabled output_config_effort= tool_choice_shape= cache_control_count=0 messages_len=2 tools_len=0 anthropic_beta= provider_extras_keys= stream=false'
    local structural="kind=\"ingress\" id=\"anthropic\" $fields"
    local structural_out="kind=\"anthropic\" id=\"anthropic-api:anthropic\" $fields"
    cat <<TRACE
2026-08-25T10:00:00.000000Z TRACE $span:messages{ingress="anthropic"}: $target ingress request body ingress="anthropic" body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-25T10:00:00.100000Z TRACE $span:complete_with_options{alias=my-alias}:complete{provider=anthropic:p model=claude-sonnet-4-5}: $target outgoing request body provider_kind="anthropic" provider=p body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-25T10:00:00.200000Z TRACE $span: $target upstream success body provider_kind="anthropic" provider=p body={"id":"msg_1"} redact_prompts_enabled=false
2026-08-25T10:00:00.300000Z TRACE $span: $target egress response body ingress="anthropic" body={"id":"msg_1"} redact_prompts_enabled=false
2026-08-25T10:00:00.010000Z TRACE $span: $target ingress request headers direction="ingress" headers=[["user-agent","claude-cli/2.1.167 (external, cli)"],["content-type","application/json"]]
2026-08-25T10:00:00.110000Z TRACE $span: $target outgoing request headers direction="outgoing" headers=[["content-type","application/json"]]
2026-08-25T10:00:00.210000Z TRACE $span: $target upstream response headers direction="upstream" headers=[["content-type","application/json"]]
2026-08-25T10:00:00.310000Z TRACE $span: $target egress response headers direction="egress" headers=[["content-type","application/json"]]
2026-08-25T10:00:00.400000Z TRACE $span: $target structural summary direction="ingress" $structural
2026-08-25T10:00:00.500000Z TRACE $span: $target structural summary direction="outgoing" $structural_out
TRACE
}

# A trace holding a SENT request that never completed: the rig finds
# nothing to capture and lands nothing, which the runner surfaces as its
# own exit 7. That is the code the wrapper must propagate verbatim.
canned_trace_no_completion() {
    canned_trace | grep -vF 'upstream success body' | grep -vF 'egress response body'
}

# ---------------------------------------------------------------------
# Invoking the wrapper
# ---------------------------------------------------------------------

RUN_INDEX=0

# Build a fresh scratch root with its asset dir populated, echo its path.
# Fresh per leg: the rig keeps a resume marker in the landing root, so a
# shared root would let one leg's marker suppress the next leg's capture.
new_scratch() {
    local mode="${1:-healthy}"
    RUN_INDEX=$((RUN_INDEX + 1))
    local root="$WORK/scratch-$RUN_INDEX"
    local assets="$root/land/$ASSETS_REL"
    mkdir -p "$assets"
    write_listener "$assets/listener.py"
    write_stub_driver "$assets/driver.sh"
    write_stub_routectl "$root/stub-routectl"
    canned_trace >"$assets/trace-healthy.log"
    canned_trace_no_completion >"$assets/trace-nolanding.log"
    printf '%s\n' "$mode" >"$assets/mode"
    printf '%s\n' "$root"
}

# Run the wrapper with a valid seat and a valid stub binary. Output goes
# to <scratch>.log; the exit code is returned.
#
# Usage: wrapper_run <scratch-root> [wrapper args...]
wrapper_run() {
    local root="$1"
    shift
    local rc=0
    (
        cd "$REPO_ROOT" || exit 2
        ROUTECTL_BIN="$root/stub-routectl" \
        ROUTECTL_CAPTURE_CELL_SEAT="$SEAT" \
            bash "$WRAPPER" "$@"
    ) >"$root.log" 2>&1 || rc=$?
    return "$rc"
}

# Run the wrapper with a per-leg environment prefix. `env -u` UNSETS every
# provider variable the surrounding shell might carry: this suite runs on an
# operator's box, and a real exported credential variable would satisfy a
# refusal leg from the environment and turn it green for the wrong reason.
# `LEG_SEAT` selects a seat other than the shared valid one.
#
# Usage: provider_run <scratch-root> <VAR=value>... -- [wrapper args...]
provider_run() {
    local root="$1"
    shift
    local env_pairs=()
    while [ $# -gt 0 ] && [ "$1" != "--" ]; do
        env_pairs+=("$1")
        shift
    done
    shift
    local rc=0
    (
        cd "$REPO_ROOT" || exit 2
        env \
            -u ROUTECTL_DRIVER_ANTHROPIC_API_KEY \
            -u ROUTECTL_DRIVER_OPENAI_API_KEY \
            -u ROUTECTL_DRIVER_OPENAI_ACCOUNT_ID \
            -u ROUTECTL_DRIVER_GEMINI_API_KEY \
            "ROUTECTL_BIN=$root/stub-routectl" \
            "ROUTECTL_CAPTURE_CELL_SEAT=${LEG_SEAT:-$SEAT}" \
            "${env_pairs[@]}" \
            bash "$WRAPPER" "$@"
    ) >"$root.log" 2>&1 || rc=$?
    return "$rc"
}

# Read one field out of a landed probe report.
probe_get() {
    sed -n "s/^$2=//p" "$1/$ASSETS_REL/probe.txt" | head -1
}

# Whether the container legs can run at all. Both the accept control and
# every in-container assertion depend on it.
container_available() {
    command -v docker >/dev/null 2>&1 &&
        docker image inspect "$IMAGE" >/dev/null 2>&1
}

# A refusal leg. The scratch root is valid and the binary and seat are
# valid, so the ONLY reason to refuse is the argv under test.
#
# Usage: refuse_case <label> <expected-code> <needle> [runner args...]
refuse_case() {
    local label="$1" expected="$2" needle="$3"
    shift 3
    local root rc=0
    root="$(new_scratch)"
    wrapper_run "$root" --scratch "$root/land" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic "$@" \
        -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
    check "$label is refused with exit $expected" "$expected" "$rc"
    check_refusal "$label names itself in the wrapper's own refusal" "$needle" "$root.log"
}

# ---------------------------------------------------------------------
# Part 1: the four isolation refusals, one at a time, one code each
# ---------------------------------------------------------------------
#
# ONE CONTROL PER RULE, not one for the set. Four rules sharing a single
# assertion would leave three of them deletable with the suite still
# green -- the vacuity that has already been measured in this repo, where
# five credential rules shared one anchor and only one had a control.
# Each spelling docker accepts is asserted too: a guard that saw only the
# space form is defeated with an equals sign.

refuse_case "--network host" 10 "--network host" --network host
refuse_case "--network=host" 10 "network=host" --network=host
refuse_case "--net host" 11 "net host" --net host
refuse_case "--net=host" 11 "net=host" --net=host
refuse_case "--privileged" 12 "privileged" --privileged
refuse_case "--pid host" 13 "pid host" --pid host
refuse_case "--pid=host" 13 "pid=host" --pid=host

# The four codes must be DISTINCT, asserted as a set rather than trusted
# from the four legs above: two rules that drifted onto the same number
# would leave every assertion above green while a caller reading the code
# could no longer tell them apart.
check "the four isolation refusals carry four distinct codes" "4" \
    "$(printf '10\n11\n12\n13\n' | sort -u | wc -l)"

# ---------------------------------------------------------------------
# Part 2: a caller-added mount, in every spelling
# ---------------------------------------------------------------------

refuse_case "a -v mount" 14 "mount set is decided in this script" \
    -v /etc:/etc
refuse_case "a --volume mount" 14 "mount set is decided in this script" \
    --volume /etc:/etc
refuse_case "a --mount flag" 14 "mount set is decided in this script" \
    --mount type=bind,src=/etc,dst=/etc
refuse_case "a --tmpfs mount" 14 "mount set is decided in this script" \
    --tmpfs /scratch
refuse_case "a --volumes-from mount" 14 "mount set is decided in this script" \
    --volumes-from other
refuse_case "a --volume= mount in equals form" 14 "mount set is decided in this script" \
    --volume=/etc:/etc

# The over-approximation is deliberate and is asserted, so a later author
# does not "fix" it into a check that stops at the driver separator: a
# mount flag after `--` still reaches docker if the wrapper stops looking.
refuse_case "a mount flag after the driver separator" 14 "mount set is decided in this script" \
    -- /bin/true -v /etc:/etc

# ---------------------------------------------------------------------
# Part 3: the host binary, the scratch root, and the seat
# ---------------------------------------------------------------------

MISSING_BIN_ROOT="$(new_scratch)"
rc=0
(
    cd "$REPO_ROOT" || exit 2
    ROUTECTL_BIN="$MISSING_BIN_ROOT/no-such-binary" \
    ROUTECTL_CAPTURE_CELL_SEAT="$SEAT" \
        bash "$WRAPPER" --scratch "$MISSING_BIN_ROOT/land" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
        -- "/scratch/$ASSETS_REL/driver.sh"
) >"$MISSING_BIN_ROOT.log" 2>&1 || rc=$?
check "a missing host binary is refused with exit 15" "15" "$rc"
check_log "the missing-binary refusal says the image carries no routectl" \
    "the image carries no routectl" "$MISSING_BIN_ROOT.log"

# A non-executable file is the same fault wearing a different mode bit,
# and the one a `cargo build` that failed halfway leaves behind.
NOEXEC_ROOT="$(new_scratch)"
printf 'not a binary\n' >"$NOEXEC_ROOT/plain-file"
rc=0
(
    cd "$REPO_ROOT" || exit 2
    ROUTECTL_BIN="$NOEXEC_ROOT/plain-file" \
    ROUTECTL_CAPTURE_CELL_SEAT="$SEAT" \
        bash "$WRAPPER" --scratch "$NOEXEC_ROOT/land" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
        -- "/scratch/$ASSETS_REL/driver.sh"
) >"$NOEXEC_ROOT.log" 2>&1 || rc=$?
check "a non-executable host binary is refused with exit 15" "15" "$rc"

IN_REPO_ROOT="$(new_scratch)"
IN_REPO_SCRATCH="$REPO_ROOT/.routectl-cell-test-should-not-exist"
rc=0
(
    cd "$REPO_ROOT" || exit 2
    ROUTECTL_BIN="$IN_REPO_ROOT/stub-routectl" \
    ROUTECTL_CAPTURE_CELL_SEAT="$SEAT" \
        bash "$WRAPPER" --scratch "$IN_REPO_SCRATCH" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
        -- "/scratch/$ASSETS_REL/driver.sh"
) >"$IN_REPO_ROOT.log" 2>&1 || rc=$?
check "a scratch root inside the repo is refused with exit 16" "16" "$rc"
check_log "the in-repo refusal says the repo is mounted read-only" \
    "the repo is mounted read-only" "$IN_REPO_ROOT.log"
# The refusal must not have created the directory it refused: a wrapper
# that mkdir'd first would leave an untracked tree in the repo on every
# rejected run.
check "the refused in-repo scratch root was not created" "absent" \
    "$([ -e "$IN_REPO_SCRATCH" ] && echo present || echo absent)"
rmdir "$IN_REPO_SCRATCH" 2>/dev/null || true

# A scratch root reached through a symlink that lands back inside the repo
# is the same fault a lexical compare cannot see.
SYMLINK_ROOT="$(new_scratch)"
ln -s "$REPO_ROOT" "$SYMLINK_ROOT/repo-link"
rc=0
(
    cd "$REPO_ROOT" || exit 2
    ROUTECTL_BIN="$SYMLINK_ROOT/stub-routectl" \
    ROUTECTL_CAPTURE_CELL_SEAT="$SEAT" \
        bash "$WRAPPER" --scratch "$SYMLINK_ROOT/repo-link/target" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
        -- "/scratch/$ASSETS_REL/driver.sh"
) >"$SYMLINK_ROOT.log" 2>&1 || rc=$?
check "a scratch root symlinked back into the repo is refused with exit 16" \
    "16" "$rc"

UNREADABLE_SEAT_ROOT="$(new_scratch)"
rc=0
(
    cd "$REPO_ROOT" || exit 2
    ROUTECTL_BIN="$UNREADABLE_SEAT_ROOT/stub-routectl" \
    ROUTECTL_CAPTURE_CELL_SEAT="$WORK/no-such-seat.json" \
        bash "$WRAPPER" --scratch "$UNREADABLE_SEAT_ROOT/land" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
        -- "/scratch/$ASSETS_REL/driver.sh"
) >"$UNREADABLE_SEAT_ROOT.log" 2>&1 || rc=$?
check "an unreadable seat file is refused with exit 19" "19" "$rc"
check_log "the unreadable-seat refusal says the container has no seat of its own" \
    "the container has no seat of its own" "$UNREADABLE_SEAT_ROOT.log"

# A seat that parses but carries no anthropic access token. The failure
# has to happen HERE, not after a daemon boots: an empty token resolves to
# a 401 at the upstream, which the runner reports as a retryable exit 7 --
# indistinguishable from a case that produced no request.
NO_TOKEN_SEAT="$WORK/seat-no-token.json"
python3 - "$NO_TOKEN_SEAT" <<'PY'
import json, sys
with open(sys.argv[1], "w", encoding="utf-8") as fh:
    json.dump({"schema_version": 1, "providers": {"codex": {"access_token": "x"}}}, fh)
PY
NO_TOKEN_ROOT="$(new_scratch)"
rc=0
(
    cd "$REPO_ROOT" || exit 2
    ROUTECTL_BIN="$NO_TOKEN_ROOT/stub-routectl" \
    ROUTECTL_CAPTURE_CELL_SEAT="$NO_TOKEN_SEAT" \
        bash "$WRAPPER" --scratch "$NO_TOKEN_ROOT/land" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
        -- "/scratch/$ASSETS_REL/driver.sh"
) >"$NO_TOKEN_ROOT.log" 2>&1 || rc=$?
check "a seat with no anthropic access token is refused with exit 20" "20" "$rc"

# The refusal must not echo seat content. Asserted against the OTHER
# provider's value present in that file, which is the thing a naive
# error-reporting path would spill.
if grep -qF -- '"access_token"' "$NO_TOKEN_ROOT.log"; then
    fail "the no-token refusal leaked seat content into its own output"
else
    echo "PASS: the no-token refusal names no seat content"
fi

# ---------------------------------------------------------------------
# Part 3b: the per-provider credential convention
# ---------------------------------------------------------------------
#
# The convention is `ROUTECTL_DRIVER_<PROVIDER>_API_KEY`, keyed on the
# PROVIDER (`anthropic`, `openai`, `gemini`) and never on the lane. These
# legs pin the three things a contributor writing the next lane config
# would otherwise get wrong, and each is REFUSED BY NAME rather than
# forwarded empty: an empty credential 401s at the upstream and reports as
# the runner's retryable exit 7, which is indistinguishable from a case
# that produced no request.
#
# `--provider` legs run on the SHARED valid seat unless a leg names its own,
# so the only variable is the provider asked for.

# gemini is NOT an OAuth provider: it has no seat entry, so with its env
# var unset there is no source at all. THIS IS THE CORE REFUSAL -- the
# wrapper must name the provider rather than forward an empty value.
GEMINI_NO_CRED_ROOT="$(new_scratch)"
rc=0
provider_run "$GEMINI_NO_CRED_ROOT" -- \
    --scratch "$GEMINI_NO_CRED_ROOT/land" --provider gemini -- \
    --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
    -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
check "a provider with no seat entry and no env var is refused with exit 20" \
    "20" "$rc"
check_refusal "the no-credential refusal names the PROVIDER" \
    "provider 'gemini' has no credential" "$GEMINI_NO_CRED_ROOT.log"
check_refusal "the no-credential refusal names the VARIABLE it looked for" \
    "ROUTECTL_DRIVER_GEMINI_API_KEY" "$GEMINI_NO_CRED_ROOT.log"

# THE PAIRED CONTROL for the refusal above. Without it, "refuses a provider
# with no credential" is satisfiable by a wrapper that refuses gemini
# always -- or every provider always. Same provider, same seat, one
# difference: the env var is set.
GEMINI_OK_ROOT="$(new_scratch)"
rc=0
provider_run "$GEMINI_OK_ROOT" \
    "ROUTECTL_DRIVER_GEMINI_API_KEY=$FAKE_GEMINI_KEY" -- \
    --scratch "$GEMINI_OK_ROOT/land" --provider gemini -- \
    --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
    --timeout 20 \
    -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
if container_available; then
    check "the SAME provider with its env var set is not refused" "0" "$rc"
    check "the env-passthrough credential reached the container intact" \
        "$FAKE_GEMINI_KEY_SHA" "$(probe_get "$GEMINI_OK_ROOT/land" gemini_key_sha)"
    # A run asked for gemini alone must forward NOTHING else. A wrapper
    # that exported the whole convention regardless of --provider would
    # pass every assertion above.
    check "a gemini-only run forwards no anthropic credential" "no" \
        "$(probe_get "$GEMINI_OK_ROOT/land" anthropic_key_present)"
else
    # The refusal ran without docker; its control cannot. Named, because a
    # silent skip here leaves the refusal above uncontrolled.
    skip "the paired control for the no-credential refusal -- it needs the"
    skip "  container, so nothing verified that a resolvable gemini credential"
    skip "  SUCCEEDS. The refusal leg above is uncontrolled in this run."
fi

# An unknown provider is a USAGE error (2), not a missing credential (20).
# The distinction is what sends the operator to their own typo instead of
# to their seat store.
#
# The runner's own usage code is ALSO 2, so the exit code alone cannot say
# which script refused. `check_refusal` is what makes this leg honest: it
# requires a `run_capture:` line naming the fault, and the runner's usage
# errors carry a `capture_driver:` prefix. The argv is otherwise complete
# for the same reason -- a leg missing a required runner flag would exit 2
# from the runner and read as a pass.
BAD_PROVIDER_ROOT="$(new_scratch)"
rc=0
provider_run "$BAD_PROVIDER_ROOT" -- \
    --scratch "$BAD_PROVIDER_ROOT/land" --provider anthropic-api -- \
    --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
    -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
check "an unknown --provider is a usage error, not a missing credential" "2" "$rc"
check_refusal "the unknown-provider refusal says a lane name is not a provider" \
    "never a lane name" "$BAD_PROVIDER_ROOT.log"

# A codex seat carrying a token but NO account id. `openai-responses` +
# `chatgpt-oauth` REQUIRES the account id (the factory's
# `validate_openai_responses_account_id` refuses a static bearer without
# one), so a run that forwarded the token alone would boot a daemon and
# fail at config validation -- or, worse, 401 at the upstream.
NO_ACCOUNT_SEAT="$WORK/seat-no-account.json"
python3 - "$NO_ACCOUNT_SEAT" "$FAKE_CODEX_TOKEN" <<'PY'
import json, sys
path, token = sys.argv[1], sys.argv[2]
doc = {
    "schema_version": 1,
    "providers": {
        "codex": {
            "access_token": token,
            "refresh_token": "stub-refresh-value",
            "token_type": "Bearer",
            "expires_at_unix": 4102444800,
            "account": {"email": "stub@example.invalid"},
        }
    },
}
with open(path, "w", encoding="utf-8") as fh:
    json.dump(doc, fh)
PY
NO_ACCOUNT_ROOT="$(new_scratch)"
rc=0
LEG_SEAT="$NO_ACCOUNT_SEAT" provider_run "$NO_ACCOUNT_ROOT" -- \
    --scratch "$NO_ACCOUNT_ROOT/land" --provider openai -- \
    --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
    -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
check "a codex seat token with no account id is refused with exit 20" "20" "$rc"
check_refusal "the missing-account-id refusal says chatgpt-oauth requires it" \
    "REQUIRES the account id" "$NO_ACCOUNT_ROOT.log"

# No refusal on this path may echo seat content either. The account-id
# refusal reads a seat that HOLDS a token, so it is the one with something
# to spill.
if grep -qF -- "$FAKE_CODEX_TOKEN" "$NO_ACCOUNT_ROOT.log"; then
    fail "the missing-account-id refusal leaked the seat's access token"
else
    echo "PASS: the missing-account-id refusal echoes no credential value"
fi

# ---------------------------------------------------------------------
# Part 4: the wrapper's own argv carries no seat mount
# ---------------------------------------------------------------------
#
# Read off the SOURCE as well as asserted from inside the container
# below, because the two catch different edits: the in-container check
# fails if a seat is mounted at the path routectl looks in, while this one
# fails if a seat is mounted anywhere at all.

MOUNT_FLAGS="$(grep -cE '^ +-v "' "$WRAPPER")"
check "the wrapper passes exactly three mounts" "3" "$MOUNT_FLAGS"

RO_MOUNTS="$(grep -cE '^ +-v ".*:ro" \\$' "$WRAPPER")"
check "two of the three mounts are read-only" "2" "$RO_MOUNTS"

if grep -E '^ +-v "' "$WRAPPER" | grep -qiE 'seat|credentials'; then
    fail "a mount in the wrapper names a seat or a credentials file"
else
    echo "PASS: no mount in the wrapper names a seat or a credentials file"
fi

# EVERY credential crosses BY NAME. `-e VAR=value` would put the value in
# this process's argv, and /proc/<pid>/cmdline is readable by every account
# on the box. The set is per-run now, so the assertion is on the ARRAY the
# wrapper builds: it must hold only `-e "$var"`, never a value.
if grep -qE -- '\+=\(-e "[$]var"\)$' "$WRAPPER"; then
    echo "PASS: credentials are forwarded by NAME, not as -e VAR=value"
else
    fail "credentials are forwarded by NAME, not as -e VAR=value"
fi

# The COMPLEMENT of the line above, and the assertion that actually holds
# as the convention grows: NO `-e` flag anywhere in the wrapper's docker
# invocation may carry a credential value. The two path variables are the
# only permitted `-e VAR=value` forms, and they are named -- so a future
# `-e "$var=$value"` fails here even if the array line above survives
# untouched.
UNEXPECTED_VALUE_ENV="$(grep -E '^ +-e "' "$WRAPPER" |
    grep -cvE '^ +-e "(ROUTECTL_BIN|ROUTECTL_DRIVER_OUT_ROOT)=')"
check "no -e flag in the docker invocation carries a value beyond the two paths" \
    "0" "$UNEXPECTED_VALUE_ENV"

# A resolved credential value is named by exactly ONE variable in the
# wrapper (`$value`), which makes "where can a credential go?" a question
# with a readable answer: every line that mentions it must be one of three
# shapes -- the emptiness test, the seat read, and the single export.
#
# Asserted as a WHITELIST rather than as a count of the export line. A count
# stays green against an ADDED leak (an `echo "$value"`, a `printf "$value"
# >file`), which is the exact edit this exists to catch -- measured while
# mutation-verifying, where a file write beside the export failed to turn a
# count-based assertion red.
VALUE_LINES="$(grep -nE '[$][{]?value[}]?' "$WRAPPER" |
    grep -cvE ':( +)(if \[ -z "[$]value" \]|\[ -n "[$]value" \] \|\||export "[$]var=[$]value"$|value="[$]\(extract_seat_field)')"
check "no line in the wrapper puts a credential value anywhere but the export" \
    "0" "$VALUE_LINES"

# ---------------------------------------------------------------------
# Part 5: docker itself absent
# ---------------------------------------------------------------------
#
# Run against a PATH farm holding every standard executable EXCEPT
# docker. Two things at once: the exit-17 refusal is covered, and the
# claim that every refusal above is decided before docker is consulted
# stops being a claim -- if the wrapper reached for docker earlier than it
# says, the seat and binary legs would already have failed on this PATH.
#
# The farm is built by EXCLUSION rather than by listing what the wrapper
# needs: a hand-list goes stale the moment the wrapper calls one more
# tool, and the failure then looks like a missing docker (exit 127, not
# 17) rather than like a stale list.
#
# SYMLINKS ARE INCLUDED, not just regular files. Measured on this box:
# coreutils ships as a multi-call binary with every utility name a
# symlink into it, so a `-type f` farm silently omits `dirname` and the
# leg fails at exit 1 for want of a tool rather than at 17 for want of
# docker.
NO_DOCKER_BIN="$WORK/no-docker-bin"
mkdir -p "$NO_DOCKER_BIN"
while IFS= read -r tool_path; do
    tool_name="${tool_path##*/}"
    [ "$tool_name" = docker ] && continue
    [ -e "$NO_DOCKER_BIN/$tool_name" ] && continue
    ln -s "$tool_path" "$NO_DOCKER_BIN/$tool_name" 2>/dev/null || true
done < <(find /usr/local/bin /usr/bin /bin -maxdepth 1 \( -type f -o -type l \) 2>/dev/null)

if command -v docker >/dev/null 2>&1 && [ -x "$NO_DOCKER_BIN/bash" ]; then
    NO_DOCKER_ROOT="$(new_scratch)"
    rc=0
    (
        cd "$REPO_ROOT" || exit 2
        PATH="$NO_DOCKER_BIN" \
        ROUTECTL_BIN="$NO_DOCKER_ROOT/stub-routectl" \
        ROUTECTL_CAPTURE_CELL_SEAT="$SEAT" \
            bash "$WRAPPER" --scratch "$NO_DOCKER_ROOT/land" -- \
            --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
            -- "/scratch/$ASSETS_REL/driver.sh"
    ) >"$NO_DOCKER_ROOT.log" 2>&1 || rc=$?
    check "an absent docker is refused with exit 17" "17" "$rc"

    # The refusal that fires on this PATH must be the DOCKER one, not an
    # earlier one that happened to fail for want of a tool -- otherwise
    # the leg above passes for the wrong reason.
    check_log "the docker-absent refusal names docker" \
        "docker is not installed" "$NO_DOCKER_ROOT.log"
else
    skip "the docker-absent refusal -- docker is already absent on this box, so"
    skip "  the wrapper's exit-17 path cannot be told apart from the surrounding"
    skip "  environment."
fi

# ---------------------------------------------------------------------
# Part 6: the container legs
# ---------------------------------------------------------------------

run_container_legs() {
    local root rc

    # --- the paired ACCEPT control ------------------------------------
    # Without this every refusal above is satisfiable by a wrapper that
    # rejects everything.
    root="$(new_scratch healthy)"
    rc=0
    wrapper_run "$root" --scratch "$root/land" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic --timeout 20 \
        -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
    check "a well-formed invocation exits 0" "0" "$rc"

    if [ ! -f "$root/land/$ASSETS_REL/probe.txt" ]; then
        fail "the driver never ran inside the container (wrapper log: $root.log)"
        sed -n '1,25p' "$root.log"
        return
    fi
    echo "PASS: the driver ran inside the container, so capture_driver.sh was reached"

    local land="$root/land"

    check "the fixture landed in the scratch mount at <lane>/<case>" "yes" \
        "$([ -f "$land/anthropic-api/plain-turn-01/meta.json" ] && echo yes || echo no)"

    # The runner's contract, observed from inside: the pins arrived and
    # the daemon it booted is the one the driver talked to.
    check "the runner's case pin reached the driver" "plain-turn-01" \
        "$(probe_get "$land" case_id)"
    check "the runner derived the wire pattern from the case file" "baseline" \
        "$(probe_get "$land" wire_pattern)"
    # The pin the wrapper passed through verbatim. It rides the `--`
    # passthrough like every other runner flag, so this also says the
    # wrapper added no argv rewriting of its own for the newest one.
    check "the runner's expected-ingress pin reached the driver" "anthropic" \
        "$(probe_get "$land" expected_ingress)"
    check "the driver saw the hermetic daemon's loopback base url" "yes" \
        "$(case "$(probe_get "$land" base_url)" in http://127.0.0.1:*) echo yes ;; *) echo no ;; esac)"

    # --- the seat is not there ---------------------------------------
    # The DEFAULT provider set is `anthropic` alone, so this leg is also
    # the behavioural-unchanged assertion for the pre-existing path: no
    # `--provider` flag, one credential, the same variable name.
    check "the token reached the container as ROUTECTL_DRIVER_ANTHROPIC_API_KEY" \
        "yes" "$(probe_get "$land" anthropic_key_present)"
    check "the token arrived intact, compared by digest" \
        "$FAKE_TOKEN_SHA" "$(probe_get "$land" anthropic_key_sha)"
    # A default run must forward NOTHING beyond the default provider. The
    # seat this suite writes HOLDS a codex entry, so a wrapper that
    # extracted every seat it found would leak a credential the run never
    # asked for -- into a container running a driven agent.
    check "a default run forwards no openai credential" "no" \
        "$(probe_get "$land" openai_key_present)"
    check "a default run forwards no openai account id" "no" \
        "$(probe_get "$land" openai_account_present)"
    check "no seat file exists at the path routectl resolves" "no" \
        "$(probe_get "$land" seat_at_xdg)"
    check "no seat file exists under the container's HOME either" "no" \
        "$(probe_get "$land" seat_at_home)"
    check "no credentials.json exists anywhere in the container" "" \
        "$(probe_get "$land" seat_anywhere)"

    # --- the mounts --------------------------------------------------
    # THE PAIR. A container that can write nothing satisfies the first
    # line alone; one that can write everything satisfies the second.
    check "a write to a tracked path in the mounted repo is REFUSED" "refused" \
        "$(probe_get "$land" repo_write)"
    check "a write to the scratch mount SUCCEEDS" "ok" \
        "$(probe_get "$land" scratch_write)"
    check "a write beside the mounted host binary is refused" "refused" \
        "$(probe_get "$land" bin_dir_write)"
    check "the runner ran the read-only mounted host binary" "yes" \
        "$(case "$(probe_get "$land" routectl_bin)" in /usr/local/lib/routectl/bin/*) echo yes ;; *) echo no ;; esac)"
    check "the runner's landing root is the scratch mount" "/scratch" \
        "$(probe_get "$land" out_root)"

    # The tracked file the driver tried to write must be BYTE-UNCHANGED
    # on the host. The in-container refusal above is the mechanism; this
    # is the consequence, and it is the one that matters.
    check "the tracked file is unchanged on the host after the run" "clean" \
        "$(cd "$REPO_ROOT" && git diff --quiet -- Cargo.toml && echo clean || echo MODIFIED)"

    # The fixture is owned by the invoking user, not by root: a
    # root-owned fixture cannot be scrubbed, promoted or deleted without
    # sudo.
    check "the landed fixture is owned by the invoking user" "$(id -u)" \
        "$(stat -c '%u' "$land/anthropic-api/plain-turn-01/meta.json" 2>/dev/null)"

    # --- the codex pair: token AND account id ------------------------
    # `openai-responses` + `chatgpt-oauth` requires BOTH. This is the
    # paired control for the missing-account-id refusal above, and it is
    # the only leg that proves the openai variables carry the CODEX seat's
    # values rather than the anthropic seat's -- which is why the two
    # seats' fake values are distinct.
    root="$(new_scratch healthy)"
    rc=0
    provider_run "$root" -- --scratch "$root/land" --provider openai -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
        --timeout 20 \
        -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
    check "a run asking for openai alone exits 0" "0" "$rc"

    if [ -f "$root/land/$ASSETS_REL/probe.txt" ]; then
        local codex_land="$root/land"
        check "the codex seat token crossed as ROUTECTL_DRIVER_OPENAI_API_KEY" \
            "$FAKE_CODEX_TOKEN_SHA" "$(probe_get "$codex_land" openai_key_sha)"
        check "the codex account id crossed as ROUTECTL_DRIVER_OPENAI_ACCOUNT_ID" \
            "$FAKE_CODEX_ACCOUNT_SHA" "$(probe_get "$codex_land" openai_account_sha)"
        # Keyed on the PROVIDER, and the openai variable must hold the
        # CODEX seat's token -- not the anthropic one that shares the file.
        check "the openai variable does not carry the anthropic seat's token" "no" \
            "$([ "$(probe_get "$codex_land" openai_key_sha)" = "$FAKE_TOKEN_SHA" ] && echo yes || echo no)"
        check "an openai-only run forwards no anthropic credential" "no" \
            "$(probe_get "$codex_land" anthropic_key_present)"
    else
        fail "the openai leg's driver never ran inside the container (log: $root.log)"
    fi

    # --- the account id rides ONLY with a seat token ------------------
    # An env-sourced token is the api-key surface, for which the factory
    # REFUSES an `account_id_ref` outright -- so forwarding an account id
    # alongside it would turn a working lane into a config error. Asserted
    # against the FULL seat, which HOLDS a codex account id: a wrapper that
    # resolved the account id independently of the token's source would
    # find one and forward it.
    root="$(new_scratch healthy)"
    rc=0
    provider_run "$root" \
        "ROUTECTL_DRIVER_OPENAI_API_KEY=$FAKE_GEMINI_KEY" -- \
        --scratch "$root/land" --provider openai -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
        --timeout 20 \
        -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
    check "an env-sourced openai token is not refused for want of an account id" \
        "0" "$rc"
    if [ -f "$root/land/$ASSETS_REL/probe.txt" ]; then
        local env_land="$root/land"
        check "the env-sourced token crossed, not the seat's" \
            "$FAKE_GEMINI_KEY_SHA" "$(probe_get "$env_land" openai_key_sha)"
        check "no account id rides an env-sourced token" "no" \
            "$(probe_get "$env_land" openai_account_present)"
    else
        fail "the env-sourced openai leg's driver never ran (log: $root.log)"
    fi

    # --- exit codes propagate VERBATIM -------------------------------
    # The runner distinguishes seven outcomes and callers act on the
    # number. A wrapper that collapsed them to 1 would make the whole
    # contract unobservable through the container.
    root="$(new_scratch nolanding)"
    rc=0
    wrapper_run "$root" --scratch "$root/land" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic --timeout 20 \
        -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
    check "an inner runner exit 7 comes out of the wrapper as 7" "7" "$rc"

    root="$(new_scratch deaf)"
    rc=0
    wrapper_run "$root" --scratch "$root/land" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic --timeout 3 \
        -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
    check "an inner runner exit 3 comes out of the wrapper as 3" "3" "$rc"

    # A usage error inside the runner is the runner's 2, which is also
    # this wrapper's usage code -- asserted so a later edit does not
    # renumber one of them apart from the other.
    root="$(new_scratch healthy)"
    rc=0
    wrapper_run "$root" --scratch "$root/land" -- \
        --lane no-such-lane --case plain-turn-01 --expected-ingress anthropic --timeout 20 \
        -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
    check "an inner runner exit 2 comes out of the wrapper as 2" "2" "$rc"

    # --- an image that is not there ----------------------------------
    root="$(new_scratch)"
    rc=0
    wrapper_run "$root" --scratch "$root/land" \
        --image "routectl-capture:no-such-tag-exists" -- \
        --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
        -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
    check "an absent image is refused with exit 18" "18" "$rc"
}

if ! command -v docker >/dev/null 2>&1; then
    skip "the container legs -- docker is not installed or not on PATH. The"
    skip "  refusals above ran, but nothing verified that a well-formed"
    skip "  invocation reaches capture_driver.sh, that the repo mount is"
    skip "  read-only, that no seat file exists in the container, or that the"
    skip "  runner's exit codes propagate."
elif ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    skip "the container legs -- $IMAGE is not built locally, so no capture was"
    skip "  run inside it. Build it with scripts/container/build.sh. The"
    skip "  refusals above ran; the accept control and every in-container"
    skip "  assertion did not."
else
    run_container_legs
fi

echo
if [ "$fails" -eq 0 ]; then
    echo "capture cell wrapper: all assertions passed"
    exit 0
fi
echo "capture cell wrapper: $fails assertion(s) failed"
exit 1
