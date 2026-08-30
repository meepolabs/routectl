#!/usr/bin/env bash
# Self-test for capture_driver.sh. Exits 0 when all assertions pass,
# non-zero on the first failure.
#
# NO REAL DAEMON IS BOOTED. A real boot needs a credential and CI has
# none, so every case runs the REAL runner against a STUB `routectl` on
# `PATH` (injected through the runner's `ROUTECTL_BIN` override). The stub
# serves `/health`, writes its own pid to a file so a case can assert the
# runner killed exactly that process, and emits a canned trace to stderr
# -- which is the same stderr redirect the runner captures from the real
# daemon, so the capture handoff is exercised end to end.
#
# Every case also runs inside a throwaway repo carrying copies of the
# runner, the capture rig, the scrub script, and the committed lane
# config, exactly as the rig's own self-test does: the runner derives both
# its lane-config dir and the driver corpus root from its own location, so
# a throwaway repo is what keeps the real corpus and the real config out
# of the blast radius.
#
# The driver command is a PROBE script that dumps what it can see -- cwd,
# HOME, git identity, the XDG layout, the exported pins -- to a file
# OUTSIDE the run workspace, since the runner removes that workspace on
# exit. Asserting hermeticity from inside the driven environment is the
# only honest place to assert it: that environment is what a driven client
# would read back into a request body.
#
# Requires python3 (the stub listener) and `ss` (the runner's port probe).
#
# Run it from anywhere:
#   bash scripts/capture_driver.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNNER="$HERE/capture_driver.sh"
RIG="$HERE/capture_fixtures.sh"
SCRUB="$HERE/scrub-fixture.sh"
LANE_CONFIG="$HERE/drivers/config/anthropic-api.toml"

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

check_log() {
    local label="$1" needle="$2" file="$3"
    if grep -qF -- "$needle" "$file"; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- '$needle' absent from $file"
        sed -n '1,20p' "$file"
        fails=$((fails + 1))
    fi
}

for tool in python3 ss git curl sha256sum; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "FAIL: $tool not found; this self-test cannot exercise the runner"
        exit 1
    fi
done

# ---------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------

# The canned trace the stub emits on stderr: a complete non-stream request
# carrying BOTH request-side structural summaries, which is what driver
# mode requires before it will land a fixture. Every value is synthetic
# and matches nothing on any real machine, so the scrub check the rig runs
# before promotion has nothing to refuse.
canned_trace() {
    local id="019eab77-0000-4000-8000-0000000000d1"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    local structural='kind="anthropic" id=p model=claude-sonnet-4-5 max_tokens=64 thinking_shape="" output_config_effort="" tool_choice_shape="" cache_control_count=0 messages_len=2 tools_len=0 anthropic_beta="" provider_extras_keys="" stream=false'
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
2026-08-25T10:00:00.500000Z TRACE $span: $target structural summary direction="outgoing" $structural
TRACE
}

# A canned trace holding a SENT request that never completed: no
# `upstream success body` and no `stream summary` line, so the rig finds
# nothing to capture and refuses nothing. The runner must surface that as
# its own exit 7, not fold it into the refusal exit 5.
canned_trace_no_completion() {
    local id="019eab77-0000-4000-8000-0000000000d2"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    local structural='kind="anthropic" id=p model=claude-sonnet-4-5 max_tokens=64 thinking_shape="" output_config_effort="" tool_choice_shape="" cache_control_count=0 messages_len=2 tools_len=0 anthropic_beta="" provider_extras_keys="" stream=false'
    cat <<TRACE
2026-08-25T10:00:00.000000Z TRACE $span:messages{ingress="anthropic"}: $target ingress request body ingress="anthropic" body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-25T10:00:00.100000Z TRACE $span:complete_with_options{alias=my-alias}:complete{provider=anthropic:p model=claude-sonnet-4-5}: $target outgoing request body provider_kind="anthropic" provider=p body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-25T10:00:00.010000Z TRACE $span: $target ingress request headers direction="ingress" headers=[["user-agent","claude-cli/2.1.167 (external, cli)"]]
2026-08-25T10:00:00.110000Z TRACE $span: $target outgoing request headers direction="outgoing" headers=[["content-type","application/json"]]
2026-08-25T10:00:00.400000Z TRACE $span: $target structural summary direction="ingress" $structural
2026-08-25T10:00:00.500000Z TRACE $span: $target structural summary direction="outgoing" $structural
TRACE
}

# A canned trace whose request DID complete but carries only the ingress
# structural summary. Driver mode refuses a fixture with half its
# structural evidence, so the rig exits 1 -- the path the runner maps to
# exit 5, and the control proving 7 is not simply every non-zero rig exit.
canned_trace_half_structural() {
    canned_trace | grep -vF 'structural summary direction="outgoing"'
}

# The python listener, shared by the stub daemon and by the case that
# OCCUPIES a port. Answers only `/health`; anything else 404s, because a
# stub that answered everything would hide a runner that polled the wrong
# path.
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

# The stub `routectl`. Parses just enough of the real `serve` argv to find
# `--port`, records its own pid, emits the canned trace on stderr, then
# EXECs the listener -- so the pid the runner captured from `$!` stays the
# pid of the process actually holding the port, exactly as it does with
# the real daemon.
#
# `STUB_MODE=healthy` serves /health; `STUB_MODE=deaf` holds the process
# open without ever binding, which is the "daemon came up but never became
# ready" shape the health precondition exists to catch.
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
printf '%s\n' "$$" >"$STUB_PID_FILE"
printf '%s\n' "$port" >"$STUB_PORT_FILE"
if [ -r "${STUB_TRACE_FILE:-}" ]; then
    cat "$STUB_TRACE_FILE" >&2
fi
if [ "${STUB_MODE:-healthy}" = "deaf" ]; then
    exec sleep 600
fi
exec python3 "$STUB_LISTENER" "$port"
SH
    chmod +x "$1"
}

# The driver command. Dumps everything a driven client could observe about
# its environment into `$PROBE_OUT`, which lives outside the run workspace
# because the runner removes that workspace on exit.
write_probe_driver() {
    cat >"$1" <<'SH'
#!/usr/bin/env bash
set -u
{
    printf 'cwd=%s\n' "$PWD"
    printf 'home=%s\n' "$HOME"
    printf 'git_name=%s\n' "$(git config user.name)"
    printf 'git_email=%s\n' "$(git config user.email)"
    printf 'git_repo=%s\n' "$(git rev-parse --is-inside-work-tree 2>/dev/null || echo no)"
    printf 'home_entries=%s\n' "$(ls -A "$HOME" | tr '\n' ',')"
    printf 'xdg=%s\n' "$XDG_CONFIG_HOME"
    printf 'xdg_config_present=%s\n' \
        "$([ -f "$XDG_CONFIG_HOME/routectl/config.toml" ] && echo yes || echo no)"
    printf 'base_url=%s\n' "$ROUTECTL_BASE_URL"
    printf 'port=%s\n' "$ROUTECTL_DRIVER_PORT"
    printf 'case_id=%s\n' "$ROUTECTL_FIXTURE_CASE_ID"
    printf 'config_sha=%s\n' "$ROUTECTL_FIXTURE_CONFIG_SHA"
    printf 'connection_mode=%s\n' "$ROUTECTL_FIXTURE_CONNECTION_MODE"
    printf 'wire_pattern=%s\n' "$ROUTECTL_FIXTURE_WIRE_PATTERN"
    printf 'health=%s\n' "$(curl -fsS -m 2 "$ROUTECTL_BASE_URL/health" || echo unreachable)"
} >"$PROBE_OUT"
SH
    chmod +x "$1"
}

# Build a throwaway repo carrying the real runner, the real rig, the real
# Write one throwaway case file the runner can derive a wire pattern
# from. The runner reads the case file itself now (through
# validate_case.py), so a self-test repo without one has no runnable case
# at all. The pattern is a parameter, and case 1 uses a value that is
# nobody's default, so an assertion on it cannot pass against a runner
# that hardcodes one.
write_case() {
    local dir="$1" case_id="$2" pattern="$3"
    cat >"$dir/$case_id.json" <<CASE
{
  "schema_version": 1,
  "case_id": "$case_id",
  "title": "Throwaway case for the runner self-test",
  "wire_pattern": "$pattern",
  "turns": [
    {
      "prompt": "Say hello."
    }
  ],
  "knobs": {
    "tools": false,
    "thinking": false,
    "cache_breakpoints": false,
    "context_padding_bytes": 0
  }
}
CASE
}

# The wire pattern each self-test case declares. Case 1 claims a pattern
# no default anywhere in the tree uses, so its end-to-end assertion is
# about the derivation and not about a coincidence.
SELFTEST_CASE_PATTERN_01="cache-breakpoints"

# scrub script, and the real committed lane config, plus the stub daemon
# and the probe driver. Prints the work root.
make_work() {
    local work
    work="$(mktemp -d)"
    mkdir -p "$work/repo/scripts/drivers/config" \
        "$work/repo/scripts/drivers/lib" \
        "$work/repo/scripts/drivers/cases" \
        "$work/repo/crates/routectl-cli/tests/fixtures" \
        "$work/bin"
    cp "$RUNNER" "$work/repo/scripts/capture_driver.sh"
    cp "$RIG" "$work/repo/scripts/capture_fixtures.sh"
    cp "$SCRUB" "$work/repo/scripts/scrub-fixture.sh"
    cp "$HERE/drivers/lib/confine.sh" "$work/repo/scripts/drivers/lib/confine.sh"
    cp "$HERE/drivers/lib/validate_case.py" "$work/repo/scripts/drivers/lib/validate_case.py"
    cp "$LANE_CONFIG" "$work/repo/scripts/drivers/config/anthropic-api.toml"
    write_case "$work/repo/scripts/drivers/cases" driver-selftest-01 \
        "$SELFTEST_CASE_PATTERN_01"
    local case_id
    for case_id in driver-selftest-02 driver-selftest-03 driver-selftest-04 \
        driver-selftest-05 driver-selftest-06 driver-selftest-07 \
        driver-selftest-08 driver-selftest-09 driver-selftest-09b \
        driver-selftest-15 driver-selftest-15b driver-selftest-15c \
        driver-selftest-15d driver-selftest-16 driver-selftest-16b; do
        write_case "$work/repo/scripts/drivers/cases" "$case_id" baseline
    done
    # The rig reads the workspace version from the repo-root Cargo.toml.
    printf '[workspace.package]\nversion = "9.9.9"\n' >"$work/repo/Cargo.toml"
    write_listener "$work/bin/listener.py"
    write_stub_routectl "$work/bin/routectl-stub"
    write_probe_driver "$work/bin/probe-driver"
    canned_trace >"$work/canned-trace.log"
    printf '%s\n' "$work"
}

# Run the real runner inside a throwaway repo. Positional args are extra
# runner flags; the driver command is always the probe. Runner
# stdout+stderr land in `<work>/runner.log`. Returns the runner's exit
# status.
#
# `--work` points the run workspace under the case's throwaway dir so a
# case can assert both WHERE the hermetic workspace lives and that the run
# removed it; it also means a killed self-test leaves nothing in the
# system temp root.
#
# The port window is caller-controlled through the runner's documented
# overrides so a case can force a collision; `STUB_MODE` selects the
# stub's behavior.
runner_run() {
    local work="$1"
    shift
    local rc=0
    (
        cd "$work/repo" || exit 2
        ROUTECTL_BIN="$work/bin/routectl-stub" \
        STUB_PID_FILE="$work/stub.pid" \
        STUB_PORT_FILE="$work/stub.port" \
        STUB_TRACE_FILE="$work/canned-trace.log" \
        STUB_LISTENER="$work/bin/listener.py" \
        STUB_MODE="${STUB_MODE:-healthy}" \
        PROBE_OUT="$work/probe.txt" \
        ROUTECTL_DRIVER_OUT_ROOT="${ROUTECTL_DRIVER_OUT_ROOT-$work
$work/scratch}" \
            bash scripts/capture_driver.sh --work "$work/runs" "$@" \
            -- "$work/bin/probe-driver"
    ) >"$work/runner.log" 2>&1 || rc=$?
    return "$rc"
}

probe_get() {
    sed -n "s/^$2=//p" "$1/probe.txt"
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

# Where a run with no `--out` lands: the gitignored scratch root at the
# throwaway repo's root, NOT the committed corpus under tests/fixtures/.
default_landing_root() {
    printf '%s\n' "$1/repo/.routectl-driver-scratch"
}

# The script's CODE, comments stripped. The live-daemon greps below are
# about what the runner DOES, and the runner's own header explains at
# length why it must not do several of those things -- a whole-file grep
# would fire on the explanation.
code_lines() {
    sed 's/#.*$//' "$1"
}

# Bind a port with the listener and print `<pid> <port>`. Used to OCCUPY a
# port so the runner's probe has something real to skip.
#
# The listener's own output goes to /dev/null: this function is called in
# a command substitution, and a background child holding the substitution
# pipe open would block the caller until the child exits.
occupy_port() {
    local work="$1" port="$2"
    python3 "$work/bin/listener.py" "$port" >/dev/null 2>&1 &
    local pid=$!
    local i=0
    while [ "$i" -lt 40 ]; do
        if curl -fsS -m 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
            printf '%s\n' "$pid"
            return 0
        fi
        sleep 0.1
        i=$((i + 1))
    done
    kill "$pid" 2>/dev/null
    printf '\n'
    return 1
}

# A port nothing is listening on right now, drawn high so it does not
# collide with a conventionally-configured local service.
free_port() {
    local candidate i=0
    while [ "$i" -lt 200 ]; do
        candidate=$((21000 + RANDOM % 4000))
        if ! ss -ltn 2>/dev/null | awk -v p=":$candidate" \
            'NR > 1 && index($4, p) == length($4) - length(p) + 1 { found = 1 } END { exit !found }'; then
            printf '%s\n' "$candidate"
            return 0
        fi
        i=$((i + 1))
    done
    return 1
}

EXPECTED_SHA="$(sha256sum "$LANE_CONFIG" | cut -d' ' -f1)"

# --- Case 1: a full run is hermetic, pinned, and lands a fixture ------
# One healthy run carries most of the contract: the workspace the driver
# sees, the pins it sees, and the fixture the rig lands from the trace the
# runner captured off the daemon's stderr.
work="$(make_work)"
port_a="$(free_port)"
rc=0
ROUTECTL_DRIVER_PORT_MIN="$port_a" ROUTECTL_DRIVER_PORT_MAX="$port_a" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-01 || rc=$?
check "a healthy run exits 0" "0" "$rc"

if [ -f "$work/probe.txt" ]; then
    echo "PASS: the driver command ran in the hermetic workspace"

    # Hermeticity. The driven client sees a HOME that is not the real one
    # and an EMPTY one at that: anything reachable from HOME is content a
    # client's own tool output can pull into a request body.
    probe_home="$(probe_get "$work" home)"
    check_ne "the driven HOME is not the invoking HOME" "$HOME" "$probe_home"
    case "$probe_home" in
        "$work"/runs/*) echo "PASS: the driven HOME lives under the run workspace" ;;
        *)
            echo "FAIL: the driven HOME is outside the run workspace: $probe_home"
            fails=$((fails + 1))
            ;;
    esac
    check "the driven HOME is empty at boot" "" "$(probe_get "$work" home_entries)"

    probe_cwd="$(probe_get "$work" cwd)"
    check_ne "the driven cwd is not the invoking cwd" "$work/repo" "$probe_cwd"
    check "the driven cwd is a git work tree" "true" "$(probe_get "$work" git_repo)"
    check "the driven git author name is synthetic" "Fixture Driver" \
        "$(probe_get "$work" git_name)"
    check "the driven git author email is synthetic" "driver@fixtures.invalid" \
        "$(probe_get "$work" git_email)"

    # The config path the daemon resolves is
    # `$XDG_CONFIG_HOME/routectl/config.toml`; an XDG root without the
    # `routectl/` subdir boots the daemon against the caller's real config.
    check "XDG carries the routectl/ subdir with the lane config" "yes" \
        "$(probe_get "$work" xdg_config_present)"

    # The four pins plus the base URL are the driver's whole interface.
    check "the case id pin reaches the driver" "driver-selftest-01" \
        "$(probe_get "$work" case_id)"
    check "the config sha pin reaches the driver" "$EXPECTED_SHA" \
        "$(probe_get "$work" config_sha)"
    check "the connection mode pin defaults to base-url" "base-url" \
        "$(probe_get "$work" connection_mode)"
    check "the wire pattern pin reaches the driver" "$SELFTEST_CASE_PATTERN_01" \
        "$(probe_get "$work" wire_pattern)"
    check "the base url pin names the selected port" "http://127.0.0.1:$port_a" \
        "$(probe_get "$work" base_url)"
    check "the daemon answered the driver's own health probe" \
        '{"status":"ok","version":"stub"}' "$(probe_get "$work" health)"
else
    echo "FAIL: the driver command never ran (runner log: $work/runner.log)"
    sed -n '1,20p' "$work/runner.log"
    fails=$((fails + 9))
fi

# The pins must reach the RIG too, not just the driver: meta.json is where
# a rerun comparison reads them back.
meta="$(default_landing_root "$work")/anthropic-api/driver-selftest-01/meta.json"
if [ -f "$meta" ]; then
    echo "PASS: the rig lands the fixture at <lane>/<case_id>"
    check "meta.case_id carries the pin" "driver-selftest-01" \
        "$(meta_get "$meta" case_id)"
    check "meta.config_sha carries the committed config's sha" "$EXPECTED_SHA" \
        "$(meta_get "$meta" config_sha)"
    check "meta.client.connection_mode carries the pin" "base-url" \
        "$(meta_get "$meta" client.connection_mode)"
    check "meta.lane is the normalized lane" "anthropic-api" \
        "$(meta_get "$meta" lane)"
    # The wire pattern is DERIVED, so the expectation is read out of the
    # case file the runner read -- not restated here. A restated constant
    # would still pass if the runner stopped reading the case entirely,
    # which is the whole failure the derivation exists to prevent. Read
    # with a plain JSON parse rather than through validate_case.py, so the
    # producer and the expectation do not share a reader.
    case_pattern="$(python3 -c 'import json,sys
print(json.load(open(sys.argv[1]))["wire_pattern"])' \
        "$work/repo/scripts/drivers/cases/driver-selftest-01.json")"
    check "the case file under test declares the pattern the run should record" \
        "$SELFTEST_CASE_PATTERN_01" "$case_pattern"
    check "meta.wire_pattern equals the case file's own wire_pattern" \
        "$case_pattern" "$(meta_get "$meta" wire_pattern)"
else
    echo "FAIL: no fixture landed at $meta (runner log: $work/runner.log)"
    sed -n '1,30p' "$work/runner.log"
    fails=$((fails + 7))
fi

# Teardown: the run's own daemon pid is gone, and the workspace with it.
stub_pid="$(cat "$work/stub.pid" 2>/dev/null || true)"
if [ -n "$stub_pid" ] && kill -0 "$stub_pid" 2>/dev/null; then
    echo "FAIL: the run leaked its daemon (pid $stub_pid still alive)"
    kill -9 "$stub_pid" 2>/dev/null
    fails=$((fails + 1))
else
    echo "PASS: a completed run leaves no daemon behind"
fi
run_dir_count="$(find "$work/runs" -maxdepth 1 -name 'routectl-driver.*' | wc -l)"
check "a completed run removes its workspace" "0" "$run_dir_count"
rm -rf "$work"

# --- Case 2: the connection mode is caller-selectable -----------------
# With the mode hardcoded, a front-proxy capture would land labelled
# base-url and a cross-mode comparison would read as client drift.
work="$(make_work)"
port_b="$(free_port)"
rc=0
ROUTECTL_DRIVER_PORT_MIN="$port_b" ROUTECTL_DRIVER_PORT_MAX="$port_b" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-02 \
    --connection-mode front-proxy || rc=$?
check "a run with an explicit connection mode exits 0" "0" "$rc"
check "the explicit connection mode reaches the driver" "front-proxy" \
    "$(probe_get "$work" connection_mode)"
meta="$(default_landing_root "$work")/anthropic-api/driver-selftest-02/meta.json"
if [ -f "$meta" ]; then
    check "the explicit connection mode reaches meta.json" "front-proxy" \
        "$(meta_get "$meta" client.connection_mode)"
else
    echo "FAIL: no fixture landed for the front-proxy case"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 3: the port probe skips an occupied port ---------------------
# A fixed "probably free" port is the failure this guards: a bind failure
# on an occupied port leaves the OTHER listener answering /health, so the
# run proceeds against someone else's daemon.
work="$(make_work)"
taken="$(free_port)"
spare="$(free_port)"
while [ "$spare" = "$taken" ]; do spare="$(free_port)"; done
# `free_port` draws at random, so order the two into a valid window.
if [ "$spare" -lt "$taken" ]; then
    swap="$taken"
    taken="$spare"
    spare="$swap"
fi
occupant="$(occupy_port "$work" "$taken")"
if [ -n "$occupant" ]; then
    rc=0
    ROUTECTL_DRIVER_PORT_MIN="$taken" ROUTECTL_DRIVER_PORT_MAX="$spare" \
        runner_run "$work" --lane anthropic-api --case driver-selftest-03 || rc=$?
    check "a run whose window contains an occupied port exits 0" "0" "$rc"
    chosen="$(cat "$work/stub.port" 2>/dev/null || true)"
    check_ne "the runner did not pick the occupied port" "$taken" "$chosen"
    if [ -n "$chosen" ] && [ "$chosen" -ge "$taken" ] && [ "$chosen" -le "$spare" ]; then
        echo "PASS: the runner picked another port inside its window"
    else
        echo "FAIL: the runner picked '$chosen', outside [$taken,$spare]"
        fails=$((fails + 1))
    fi
    kill "$occupant" 2>/dev/null
    wait "$occupant" 2>/dev/null
else
    echo "FAIL: could not occupy port $taken to test the port probe"
    fails=$((fails + 3))
fi
rm -rf "$work"

# --- Case 4: a daemon that never answers aborts the run ---------------
# The health poll is a PRECONDITION. Without it the run drives a daemon
# that is not there and the rig lands a fixture off an empty trace, or
# worse, off whatever the port's real occupant did.
work="$(make_work)"
port_d="$(free_port)"
rc=0
STUB_MODE=deaf ROUTECTL_DRIVER_PORT_MIN="$port_d" ROUTECTL_DRIVER_PORT_MAX="$port_d" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-04 --timeout 2 || rc=$?
check "a daemon that never answers aborts with the health exit code" "3" "$rc"
check_log "the abort names the unhealthy daemon" "never became healthy" \
    "$work/runner.log"
if [ -f "$work/probe.txt" ]; then
    echo "FAIL: the driver ran against a daemon that never became healthy"
    fails=$((fails + 1))
else
    echo "PASS: the driver never runs when the daemon is unhealthy"
fi
stub_pid="$(cat "$work/stub.pid" 2>/dev/null || true)"
if [ -n "$stub_pid" ] && kill -0 "$stub_pid" 2>/dev/null; then
    echo "FAIL: the aborted run leaked its daemon (pid $stub_pid still alive)"
    kill -9 "$stub_pid" 2>/dev/null
    fails=$((fails + 1))
else
    echo "PASS: an aborted run kills the daemon it started"
fi
if [ -d "$(default_landing_root "$work")/anthropic-api/driver-selftest-04" ]; then
    echo "FAIL: the aborted run landed a fixture"
    fails=$((fails + 1))
else
    echo "PASS: the aborted run lands no fixture"
fi
rm -rf "$work"

# --- Case 5: a signal mid-run still tears the daemon down -------------
# The cleanup trap, not the happy path, is what keeps an interrupted
# capture from leaving a daemon holding a port for the rest of the
# session. The driver here sleeps, so the signal lands mid-run.
#
# The signal is SIGTERM, deliberately, and it goes to the RUNNER alone.
# A terminal SIGINT reaches the whole foreground process group, so the
# stub daemon would die of its own signal whether or not the runner
# cleaned up -- the assertion would pass against a runner with no trap at
# all. SIGTERM to one pid leaves the daemon's fate entirely to the trap,
# which is the only version of this case that can fail.
work="$(make_work)"
port_e="$(free_port)"
(
    cd "$work/repo" || exit 2
    ROUTECTL_BIN="$work/bin/routectl-stub" \
    STUB_PID_FILE="$work/stub.pid" \
    STUB_PORT_FILE="$work/stub.port" \
    STUB_TRACE_FILE="$work/canned-trace.log" \
    STUB_LISTENER="$work/bin/listener.py" \
    ROUTECTL_DRIVER_PORT_MIN="$port_e" \
    ROUTECTL_DRIVER_PORT_MAX="$port_e" \
        exec bash scripts/capture_driver.sh --lane anthropic-api \
        --work "$work/runs" --case driver-selftest-05 -- sleep 60
) >"$work/runner.log" 2>&1 &
runner_pid=$!
# Wait for the daemon to be up before signalling, so the signal lands
# while there IS something to leak.
i=0
while [ "$i" -lt 60 ] && ! curl -fsS -m 1 "http://127.0.0.1:$port_e/health" >/dev/null 2>&1; do
    sleep 0.25
    i=$((i + 1))
done
if curl -fsS -m 1 "http://127.0.0.1:$port_e/health" >/dev/null 2>&1; then
    echo "PASS: the interrupted run had a live daemon to leak"
    stub_pid="$(cat "$work/stub.pid" 2>/dev/null || true)"
    kill -TERM "$runner_pid" 2>/dev/null
    wait "$runner_pid" 2>/dev/null
    i=0
    while [ "$i" -lt 40 ] && kill -0 "$stub_pid" 2>/dev/null; do
        sleep 0.1
        i=$((i + 1))
    done
    if [ -n "$stub_pid" ] && kill -0 "$stub_pid" 2>/dev/null; then
        echo "FAIL: an interrupted run leaked its daemon (pid $stub_pid still alive)"
        kill -9 "$stub_pid" 2>/dev/null
        fails=$((fails + 1))
    else
        echo "PASS: an interrupted run kills the daemon it started"
    fi
    if curl -fsS -m 1 "http://127.0.0.1:$port_e/health" >/dev/null 2>&1; then
        echo "FAIL: something still answers on the interrupted run's port"
        fails=$((fails + 1))
    else
        echo "PASS: the interrupted run's port is free again"
    fi
else
    echo "FAIL: the daemon never came up, so the trap case proves nothing"
    kill -TERM "$runner_pid" 2>/dev/null
    wait "$runner_pid" 2>/dev/null
    fails=$((fails + 3))
fi
rm -rf "$work"

# --- Case 6: the sha is the committed config's, invariant across runs --
# If the runner hashed a port-patched copy instead, every run would carry
# a unique sha and `meta.config_sha` could no longer tell a config change
# from a fresh run -- the one question it exists to answer.
work="$(make_work)"
port_f="$(free_port)"
port_g="$(free_port)"
while [ "$port_g" = "$port_f" ]; do port_g="$(free_port)"; done
ROUTECTL_DRIVER_PORT_MIN="$port_f" ROUTECTL_DRIVER_PORT_MAX="$port_f" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-06 || true
sha_first="$(probe_get "$work" config_sha)"
port_first="$(probe_get "$work" port)"
ROUTECTL_DRIVER_PORT_MIN="$port_g" ROUTECTL_DRIVER_PORT_MAX="$port_g" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-06 || true
sha_second="$(probe_get "$work" config_sha)"
port_second="$(probe_get "$work" port)"
check_ne "the two runs used different ports" "$port_first" "$port_second"
check "the sha is the committed lane config's" "$EXPECTED_SHA" "$sha_first"
check "the sha is invariant across runs on different ports" "$sha_first" "$sha_second"
rm -rf "$work"

# --- Case 7: usage errors fail closed, before anything boots ----------
work="$(make_work)"
rc=0
runner_run "$work" --lane no-such-lane --case driver-selftest-07 || rc=$?
check "an unknown lane is a usage error" "2" "$rc"
check_log "the refusal names the missing lane config" "no committed config for lane" \
    "$work/runner.log"
rc=0
(
    cd "$work/repo" || exit 2
    bash scripts/capture_driver.sh --lane anthropic-api --case driver-selftest-07
) >"$work/runner.log" 2>&1 || rc=$?
check "a missing driver command is a usage error" "2" "$rc"
check_log "the refusal names the missing driver command" "no driver command given" \
    "$work/runner.log"
rc=0
(
    cd "$work/repo" || exit 2
    bash scripts/capture_driver.sh --case driver-selftest-07 -- true
) >"$work/runner.log" 2>&1 || rc=$?
check "a missing lane is a usage error" "2" "$rc"
# An inverted window makes the modulo in the port draw a modulo by a
# non-positive number, which yields a candidate OUTSIDE the window: the
# caller silently gets a port it did not ask for.
rc=0
ROUTECTL_DRIVER_PORT_MIN=23000 ROUTECTL_DRIVER_PORT_MAX=22000 \
    runner_run "$work" --lane anthropic-api --case driver-selftest-07 || rc=$?
check "an inverted port window is a usage error" "2" "$rc"
check_log "the refusal names the inverted window" "is inverted" "$work/runner.log"
rm -rf "$work"

# --- Case 8: a failing driver aborts before the rig runs --------------
# A driver that failed produced no dialogue, so a fixture from its trace
# would be evidence of nothing.
work="$(make_work)"
port_h="$(free_port)"
rc=0
(
    cd "$work/repo" || exit 2
    ROUTECTL_BIN="$work/bin/routectl-stub" \
    STUB_PID_FILE="$work/stub.pid" \
    STUB_PORT_FILE="$work/stub.port" \
    STUB_TRACE_FILE="$work/canned-trace.log" \
    STUB_LISTENER="$work/bin/listener.py" \
    ROUTECTL_DRIVER_PORT_MIN="$port_h" \
    ROUTECTL_DRIVER_PORT_MAX="$port_h" \
        bash scripts/capture_driver.sh --lane anthropic-api \
        --work "$work/runs" --case driver-selftest-08 -- sh -c 'exit 7'
) >"$work/runner.log" 2>&1 || rc=$?
check "a failing driver aborts with the driver exit code" "4" "$rc"
check_log "the abort reports the driver's status" "driver command exited 7" \
    "$work/runner.log"
if [ -d "$(default_landing_root "$work")/anthropic-api/driver-selftest-08" ]; then
    echo "FAIL: a failing driver still landed a fixture"
    fails=$((fails + 1))
else
    echo "PASS: a failing driver lands no fixture"
fi
stub_pid="$(cat "$work/stub.pid" 2>/dev/null || true)"
if [ -n "$stub_pid" ] && kill -0 "$stub_pid" 2>/dev/null; then
    echo "FAIL: the failed run leaked its daemon (pid $stub_pid still alive)"
    kill -9 "$stub_pid" 2>/dev/null
    fails=$((fails + 1))
else
    echo "PASS: a failed driver run kills the daemon it started"
fi
rm -rf "$work"

# --- Case 9: the rig's verdict is MAPPED, not collapsed ---------------
# A trace holding no completed request and a trace holding a fixture the
# rig refuses are two different verdicts (retryable vs a defect), and a
# blanket non-zero -> 5 mapping in the runner makes the rig's distinction
# unobservable to anything calling this script. Both directions are
# asserted here because the pair IS the contract.
work="$(make_work)"
canned_trace_no_completion >"$work/canned-trace.log"
port_i="$(free_port)"
rc=0
ROUTECTL_DRIVER_PORT_MIN="$port_i" ROUTECTL_DRIVER_PORT_MAX="$port_i" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-09 || rc=$?
check "a run whose trace holds no completed request exits 7" "7" "$rc"
check_log "the exit-7 message names the case" "driver-selftest-09" \
    "$work/runner.log"
# Keyed on a substring UNIQUE to the RIG's message, not the shared
# "landed no fixture" phrase both producers emit: with the shared phrase this
# assertion passed when either producer's line was deleted, so the rig's
# machine-facing half (the one carrying the trace path) was unpinned here.
check_log "the rig's own zero-landing message reaches the runner log" \
    "holds no completed request" "$work/runner.log"
if [ -d "$(default_landing_root "$work")/anthropic-api/driver-selftest-09" ]; then
    echo "FAIL: a run that landed no fixture created a corpus directory"
    fails=$((fails + 1))
else
    echo "PASS: a run that landed no fixture creates no corpus directory"
fi
rm -rf "$work"

# The other half of the mapping: a rig REFUSAL still exits 5. Driver mode
# refuses a fixture carrying only half its structural evidence, so this
# trace completes a request and then loses it at the landing gate.
work="$(make_work)"
canned_trace_half_structural >"$work/canned-trace.log"
port_j="$(free_port)"
rc=0
ROUTECTL_DRIVER_PORT_MIN="$port_j" ROUTECTL_DRIVER_PORT_MAX="$port_j" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-09b || rc=$?
check "a rig refusal still exits 5, not 7" "5" "$rc"
check_log "the exit-5 message reports a refusal" "refused the fixture" \
    "$work/runner.log"
rm -rf "$work"

# --- Case 10: the runner names nothing the operator's live daemon owns --
# The runner runs on a box where a real routectl serves the operator's own
# traffic. A name-based kill, the live port, or the live usage database
# appearing anywhere in this script is a defect no functional test would
# catch, because the hermetic path works fine right up until the run takes
# the operator's proxy down with it.
if grep -nE 'pkill|killall' "$RUNNER"; then
    echo "FAIL: the runner kills by NAME; it must kill only its own captured pid"
    fails=$((fails + 1))
else
    echo "PASS: the runner never kills by name"
fi
if grep -n '9100' "$RUNNER"; then
    echo "FAIL: the runner references the live daemon's port"
    fails=$((fails + 1))
else
    echo "PASS: the runner references no live daemon port"
fi
if grep -n 'usage\.db' "$RUNNER"; then
    echo "FAIL: the runner names a usage database"
    fails=$((fails + 1))
else
    echo "PASS: the runner names no usage database"
fi
if grep -n 'setsid' <(code_lines "$RUNNER"); then
    echo "FAIL: the runner launches under setsid; the captured pid would be the wrapper"
    fails=$((fails + 1))
else
    echo "PASS: the runner never launches the daemon under setsid"
fi
if grep -nE '^\s*(sqlite3|/usr/bin/sqlite3)\b' "$RUNNER"; then
    echo "FAIL: the runner opens a database with sqlite3"
    fails=$((fails + 1))
else
    echo "PASS: the runner opens no database"
fi

# Positive control for the four greps above: the same patterns MUST fire
# on a script that does contain them. Without it, a broken grep
# invocation would read as four passes.
control="$(mktemp)"
printf 'pkill routectl\ncurl 127.0.0.1:9100\nsqlite3 usage.db\nsetsid routectl serve\n' >"$control"
control_hits=0
for pattern in 'pkill|killall' '9100' 'usage\.db' 'setsid'; do
    if grep -qE "$pattern" "$control"; then
        control_hits=$((control_hits + 1))
    fi
done
check "the live-daemon greps fire on a script that does contain them" "4" "$control_hits"
rm -f "$control"

# --- Case 10b: neither capture script grows a container branch ---------
# A container is a CALLER of these scripts: a wrapper invokes the runner
# with the arguments it was given. The moment either script learns to
# branch on whether it is containerized, the host path stops being the
# supported one and dropping containers stops being a two-way door. An
# absence is what review is worst at enforcing, so it is enforced
# lexically here.
forbidden_literals=('container' 'docker' 'CONTAINER')
for script in "$RUNNER" "$RIG"; do
    for literal in "${forbidden_literals[@]}"; do
        check "$(basename "$script") carries no '$literal' literal" "0" \
            "$(grep -c -- "$literal" "$script" || true)"
    done
done

# Positive control for the six greps above: every pattern MUST fire on a
# file that does contain the word, in the placements a real regression
# would use -- a comment, a variable name, a string literal, a command,
# and mid-word. Built at runtime rather than committed, so the control
# itself is never a file the guard has to exempt.
lit_control="$(mktemp)"
printf '# container mode is unsupported\nCONTAINER_MODE=1\necho "inside a container"\ndocker run --rm true\ncontainerized_run() { :; }\n' >"$lit_control"
lit_control_hits=0
for literal in "${forbidden_literals[@]}"; do
    if grep -q -- "$literal" "$lit_control"; then
        lit_control_hits=$((lit_control_hits + 1))
    fi
done
check "the container-literal greps fire on a file that does contain them" "3" \
    "$lit_control_hits"

# Case-awareness, demonstrated one placement at a time: the lowercase and
# uppercase spellings are separate greps, so a failure names which
# spelling a regression used instead of folding both into one pattern.
case_control="$(mktemp)"
printf '# container\n' >"$case_control"
check "the lowercase grep catches a comment placement" "1" \
    "$(grep -c -- 'container' "$case_control" || true)"
check "the uppercase grep leaves a lowercase comment alone" "0" \
    "$(grep -c -- 'CONTAINER' "$case_control" || true)"
printf 'CONTAINER_MODE=1\n' >"$case_control"
check "the uppercase grep catches a variable-name placement" "1" \
    "$(grep -c -- 'CONTAINER' "$case_control" || true)"
check "the lowercase grep leaves an uppercase variable name alone" "0" \
    "$(grep -c -- 'container' "$case_control" || true)"
rm -f "$lit_control" "$case_control"

# --- Case 11: --help renders the header, sentinel included ------------
help_out="$(bash "$RUNNER" --help 2>&1 || true)"
if printf '%s' "$help_out" | grep -q 'ROUTECTL_FIXTURE_CONFIG_SHA' &&
    ! printf '%s' "$help_out" | grep -q 'END USAGE'; then
    echo "PASS: --help renders the driver contract without leaking the sentinel"
else
    echo "FAIL: --help output is truncated or leaks the sentinel"
    printf '%s\n' "$help_out" | tail -5
    fails=$((fails + 1))
fi

# --- Case 12: the lane config declares the oauth-bearer wire shape ----
# `auth_kind` decides what the captured egress LOOKS like: `oauth-bearer`
# against api.anthropic.com is what turns on signature re-signing, the
# pinned beta floor, the session-id header and user-agent resolution. The
# field is not defaultable -- the default is `api-key`, under which a
# subscription token ships as `x-api-key` for a 401 -- so a silent revert
# would produce a lane nobody uses, or no fixture at all. Pinned here so
# that revert is a test failure instead of a capture-time surprise.
declares_oauth_bearer() {
    if grep -qE '^[[:space:]]*auth_kind[[:space:]]*=[[:space:]]*"oauth-bearer"' "$1"; then
        printf 'yes\n'
    else
        printf 'no\n'
    fi
}
check "the committed lane config declares auth_kind = oauth-bearer" "yes" \
    "$(declares_oauth_bearer "$LANE_CONFIG")"

# Paired control: the same matcher on a copy with the line stripped must
# report absence. A matcher broken into always-true would otherwise read
# as a pass above.
stripped="$(mktemp)"
grep -v 'auth_kind' "$LANE_CONFIG" >"$stripped"
check "the matcher reports absence when the declaration is stripped" "no" \
    "$(declares_oauth_bearer "$stripped")"
rm -f "$stripped"

# --- Case 12b: the front-proxy config is a TWIN of the base config -----
# The front-proxy lane needs a `[mitm]` block and the base lane must not
# grow one (an unused block still binds a listener and mints a CA), so the
# two lanes are two committed files. That only works while they differ by
# NOTHING but the `[mitm]` table and their header comments: a provider,
# model, or alias that drifts between them silently confounds every
# comparison between a base-url fixture and a front-proxy one, since each
# file is hashed into its own fixtures' `config_sha` and nothing else
# records the difference. Welded here so a divergence is a test failure.
FRONT_PROXY_CONFIG="$HERE/drivers/config/anthropic-api.front-proxy.toml"
check "the front-proxy lane config is committed and readable" "yes" \
    "$([ -r "$FRONT_PROXY_CONFIG" ] && echo yes || echo no)"
check "the front-proxy config declares auth_kind = oauth-bearer" "yes" \
    "$(declares_oauth_bearer "$FRONT_PROXY_CONFIG")"

# Comments and blank lines carry no wire meaning, so both sides are
# reduced to their key-bearing lines; the `[mitm]` table is dropped
# wherever it appears, which is what makes the twin comparable to a base
# config that has none.
config_wire_lines() {
    awk '
        /^[[:space:]]*#/ { next }
        /^[[:space:]]*$/ { next }
        /^\[/            { in_mitm = ($0 == "[mitm]") }
        in_mitm          { next }
        { print }
    ' "$1"
}

twins_agree() {
    if diff -q <(config_wire_lines "$1") <(config_wire_lines "$2") >/dev/null 2>&1; then
        printf 'yes\n'
    else
        printf 'no\n'
    fi
}

check "the two lane configs differ only by the [mitm] table and comments" "yes" \
    "$(twins_agree "$LANE_CONFIG" "$FRONT_PROXY_CONFIG")"

# PAIRED CONTROL: the same comparison on a twin whose provider block was
# mutated MUST report a difference. Without it the check above is
# satisfiable by a comparison that always passes -- e.g. an awk program
# that swallows every line, or a `diff` whose exit status is discarded.
mutated="$(mktemp)"
sed 's/^kind        = "anthropic-api"$/kind        = "openai-compat"/' \
    "$FRONT_PROXY_CONFIG" >"$mutated"
check_ne "the mutation actually altered the twin" \
    "$(sha256sum <"$FRONT_PROXY_CONFIG" | cut -d' ' -f1)" \
    "$(sha256sum <"$mutated" | cut -d' ' -f1)"
check "the comparison reports a difference when the provider block drifts" "no" \
    "$(twins_agree "$LANE_CONFIG" "$mutated")"
rm -f "$mutated"

# The `[mitm]` table itself: present on the twin, absent from the base,
# and pinned to the Anthropic origin and host -- config validation refuses
# any other value, because the proxy forwards the client's full-scope
# token. The base-config direction is this matcher's positive control in
# reverse: it must report absence there or its presence report proves
# nothing.
declares_pinned_mitm() {
    if grep -q '^\[mitm\]$' "$1" &&
        grep -qE '^[[:space:]]*upstream_origin[[:space:]]*=[[:space:]]*"https://api\.anthropic\.com"[[:space:]]*$' "$1" &&
        grep -qE '^[[:space:]]*mitm_host[[:space:]]*=[[:space:]]*"api\.anthropic\.com"[[:space:]]*$' "$1"; then
        printf 'yes\n'
    else
        printf 'no\n'
    fi
}
check "the front-proxy config carries a [mitm] table pinned to the Anthropic origin" \
    "yes" "$(declares_pinned_mitm "$FRONT_PROXY_CONFIG")"
check "the base lane config carries no [mitm] table" "no" \
    "$(declares_pinned_mitm "$LANE_CONFIG")"

# Both halves of the hashed-bytes contract have to be readable in the
# file: the header says an edit invalidates prior fixtures' comparability,
# and the port is marked as a placeholder the runner overrides on argv.
# Without the latter a maintainer "fixes" the port and re-keys every
# front-proxy fixture's identity.
check "the front-proxy header carries the editing-invalidates-comparability contract" \
    "1" "$(grep -c "invalidates every prior fixture's comparability" \
        "$FRONT_PROXY_CONFIG" || true)"
check "the front-proxy config names the argv override for the MITM port" "yes" \
    "$(grep -q -- 'serve --mitm-port' "$FRONT_PROXY_CONFIG" && echo yes || echo no)"

# --- Case 13: the wire pattern is DERIVED, never declared on argv -----
# A flag would let a caller record a pattern the case does not claim,
# which is the same lie as an unpinned claim one layer earlier. Both
# halves are asserted: the flag is refused as an unknown arg, and the
# runner names no wire-pattern flag in its own argv parser.
work="$(make_work)"
rc=0
runner_run "$work" --lane anthropic-api --case driver-selftest-01 \
    --wire-pattern baseline || rc=$?
check "a wire-pattern flag is an unknown arg" "2" "$rc"
check_log "the refusal names the rejected flag" "unknown arg: --wire-pattern" \
    "$work/runner.log"
rm -rf "$work"

if grep -nE -- '--wire.pattern\)' <(code_lines "$RUNNER"); then
    echo "FAIL: the runner parses a wire-pattern flag; the value must come from the case"
    fails=$((fails + 1))
else
    echo "PASS: the runner's argv parser names no wire-pattern flag"
fi

# Paired control for that grep: the same pattern MUST fire on a parser
# that does accept the flag, or a broken invocation reads as a pass.
control="$(mktemp)"
printf '    --wire-pattern) WIRE_PATTERN="$2"; shift 2 ;;\n' >"$control"
if grep -qE -- '--wire.pattern\)' "$control"; then
    echo "PASS: the wire-pattern-flag grep fires on a parser that does accept it"
else
    echo "FAIL: the wire-pattern-flag grep matches nothing, so its absence proves nothing"
    fails=$((fails + 1))
fi
rm -f "$control"

# --- Case 14: an unreadable case file fails closed before any boot ----
# The runner reads the case file for the pattern. A run whose case is
# missing has no pattern to record, and a fixture with an empty claim is
# worse than none -- so it must abort as a usage error, before a daemon
# holds a port.
work="$(make_work)"
rm -f "$work/repo/scripts/drivers/cases/driver-selftest-01.json"
rc=0
runner_run "$work" --lane anthropic-api --case driver-selftest-01 || rc=$?
check "a missing case file is a usage error" "2" "$rc"
check_log "the refusal names the missing case file" "no case file for" \
    "$work/runner.log"
if [ -f "$work/stub.pid" ]; then
    echo "FAIL: the runner booted a daemon before reading the case file"
    fails=$((fails + 1))
else
    echo "PASS: the missing-case refusal happens before any daemon boots"
fi
rm -rf "$work"

# The same shape for a case file that exists and declares a pattern
# OUTSIDE the closed set: the validator refuses it, and the runner must
# surface that as its own usage error rather than recording the value.
work="$(make_work)"
write_case "$work/repo/scripts/drivers/cases" driver-selftest-01 freeform
rc=0
runner_run "$work" --lane anthropic-api --case driver-selftest-01 || rc=$?
check "a case declaring an unknown wire pattern is a usage error" "2" "$rc"
check_log "the refusal names the invalid wire pattern" "wire_pattern" \
    "$work/runner.log"
rm -rf "$work"

# --- Case 15: --out is caller-supplied, so the runner confines it ------
# The runner hands the rig `--allow-unsafe-out` (every driver landing root
# is outside the rig's own `captured/` tree, so the rig's check would
# refuse every driver run), which means the rig performs NO containment on
# this path. The check therefore has to be the runner's own, and a fixture
# carries RAW headers -- auth included, since the daemon boots with
# ROUTECTL_TRACE_HEADERS -- so an unconfined `--out` is a write primitive
# aimed wherever the caller pointed it.

# The default with no `--out` is the gitignored scratch root, NOT the
# committed corpus: a rerun of a case overwrites that case's fixture, so a
# corpus default replaces a reviewed fixture in place.
work="$(make_work)"
port_o="$(free_port)"
rc=0
ROUTECTL_DRIVER_PORT_MIN="$port_o" ROUTECTL_DRIVER_PORT_MAX="$port_o" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-15 || rc=$?
check "a run with no --out exits 0" "0" "$rc"
check "with no --out the fixture lands in the scratch root" "yes" \
    "$([ -f "$(default_landing_root "$work")/anthropic-api/driver-selftest-15/meta.json" ] \
        && echo yes || echo no)"
check "with no --out nothing lands under tests/fixtures/" "no" \
    "$([ -e "$work/repo/crates/routectl-cli/tests/fixtures/driver" ] && echo yes || echo no)"
case "$(default_landing_root "$work")" in
    */tests/fixtures/*)
        echo "FAIL: the default landing root is inside tests/fixtures/"
        fails=$((fails + 1))
        ;;
    *) echo "PASS: the default landing root is outside tests/fixtures/" ;;
esac
rm -rf "$work"

# PAIRED CONTROL for every refusal below: a legitimate --out under the
# allowed root is ACCEPTED and lands a fixture. Without it the refusals
# are satisfiable by a guard that rejects everything.
work="$(make_work)"
scratch="$work/scratch"
port_p="$(free_port)"
rc=0
ROUTECTL_DRIVER_PORT_MIN="$port_p" ROUTECTL_DRIVER_PORT_MAX="$port_p" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-15 \
    --out "$scratch/land" --out-root "$scratch" || rc=$?
check "an --out under the allowed root exits 0" "0" "$rc"
check "an --out under the allowed root lands the fixture there" "yes" \
    "$([ -f "$scratch/land/anthropic-api/driver-selftest-15/meta.json" ] && echo yes || echo no)"
check_log "the accepted run reports the landing root it used" \
    "out=$scratch/land" "$work/runner.log"
rm -rf "$work"

# An --out OUTSIDE the allowed root is refused. Keyed on the refusal arm's
# own message, not merely on a non-zero exit: the runner calls
# `confine_out_under` bare under `set -e`, so several unrelated arms (a
# newline in the path, an unresolvable ancestor) abort the run identically
# and an exit-code-only assertion would pass against a guard that never
# performs the containment compare at all.
work="$(make_work)"
scratch="$work/scratch"
mkdir -p "$scratch"
rc=0
runner_run "$work" --lane anthropic-api --case driver-selftest-15b \
    --out "$work/outside-the-root" --out-root "$scratch" || rc=$?
check "an --out outside the allowed root is a usage error" "2" "$rc"
check_log "the refusal names the containment arm" "outside the default captured dir" \
    "$work/runner.log"
check "the refused run wrote nothing to the out-of-root path" "no" \
    "$([ -e "$work/outside-the-root" ] && echo yes || echo no)"
if [ -f "$work/stub.pid" ]; then
    echo "FAIL: the runner booted a daemon before confining --out"
    fails=$((fails + 1))
else
    echo "PASS: the --out refusal happens before any daemon boots"
fi

# A `..` traversal spelled under the root reaches the same arm.
rc=0
runner_run "$work" --lane anthropic-api --case driver-selftest-15b \
    --out "$scratch/../outside-via-dots" --out-root "$scratch" || rc=$?
check "an --out escaping the allowed root via .. is a usage error" "2" "$rc"
check_log "the .. refusal names the containment arm" "outside the default captured dir" \
    "$work/runner.log"
rm -rf "$work"

# A symlinked component UNDER the root would redirect the write out of the
# tree, and a purely lexical compare cannot see it.
work="$(make_work)"
scratch="$work/scratch"
mkdir -p "$scratch" "$work/elsewhere"
ln -s "$work/elsewhere" "$scratch/linked"
rc=0
runner_run "$work" --lane anthropic-api --case driver-selftest-15c \
    --out "$scratch/linked/land" --out-root "$scratch" || rc=$?
check "an --out with a symlinked component under the root is refused" "2" "$rc"
check_log "the symlink refusal names the component" "symlink component" \
    "$work/runner.log"
check "the symlink target received nothing" "no" \
    "$([ -e "$work/elsewhere/land" ] && echo yes || echo no)"

# A DANGLING symlink component: physical resolution walks up to the
# nearest EXISTING ancestor, so a broken link slips past `cd -P` and only
# the library's per-component `[ -L ]` walk sees it.
ln -s "$work/no-such-target" "$scratch/dangling"
rc=0
runner_run "$work" --lane anthropic-api --case driver-selftest-15d \
    --out "$scratch/dangling/land" --out-root "$scratch" || rc=$?
check "an --out with a DANGLING symlink component is refused" "2" "$rc"
check_log "the dangling-symlink refusal names the component" "symlink component" \
    "$work/runner.log"
rm -rf "$work"

# `--out` with no value is a usage error naming the flag. Invoked directly
# rather than through `runner_run`, which always appends the driver
# command: `--out --` would consume the separator as a value.
work="$(make_work)"
rc=0
(
    cd "$work/repo" || exit 2
    bash scripts/capture_driver.sh --lane anthropic-api \
        --case driver-selftest-15 --out
) >"$work/runner.log" 2>&1 || rc=$?
check "--out with no value is a usage error" "2" "$rc"
check_log "the refusal names the valueless flag" "--out requires a value" \
    "$work/runner.log"
rc=0
(
    cd "$work/repo" || exit 2
    bash scripts/capture_driver.sh --lane anthropic-api \
        --case driver-selftest-15 --out-root
) >"$work/runner.log" 2>&1 || rc=$?
check "--out-root with no value is a usage error" "2" "$rc"
check_log "the refusal names the valueless root flag" "--out-root requires a value" \
    "$work/runner.log"
rm -rf "$work"

# The confinement logic lives in exactly ONE place. A second copy is a
# path-traversal surface that drifts from the first, so assert the runner
# CALLS the shared library and defines none of its parts -- with positive
# controls proving each matcher fires on the file that does define them.
CONFINE_LIB="$HERE/drivers/lib/confine.sh"
check "the runner defines no abspath_lexical" "0" \
    "$(grep -c '^abspath_lexical()' "$RUNNER")"
check "the runner defines no abspath_physical" "0" \
    "$(grep -c '^abspath_physical()' "$RUNNER")"
check "the runner defines no confine_out_under" "0" \
    "$(grep -c '^confine_out_under()' "$RUNNER")"
check "the runner runs no per-component symlink test" "0" \
    "$(grep -c '\[ -L ' "$RUNNER")"
check "positive control: the library DOES define abspath_lexical" "1" \
    "$(grep -c '^abspath_lexical()' "$CONFINE_LIB")"
check "positive control: the library DOES define abspath_physical" "1" \
    "$(grep -c '^abspath_physical()' "$CONFINE_LIB")"
check "positive control: the library DOES define confine_out_under" "1" \
    "$(grep -c '^confine_out_under()' "$CONFINE_LIB")"
check "positive control: the library DOES run the symlink test" "1" \
    "$(grep -q '\[ -L ' "$CONFINE_LIB" && echo 1 || echo 0)"
check "the runner sources the shared library" "1" \
    "$(grep -c 'drivers/lib/confine.sh"$' "$RUNNER")"

# --- Case 15c: --out-root is itself confined to a closed set ----------
# Without this, `--out` is confined against a root the SAME CALLER chose,
# so `--out /anywhere/x --out-root /anywhere` satisfies the containment
# check while landing raw fixture headers -- auth included -- wherever the
# caller pointed. The check below is what makes the `--out` confinement
# above mean something, so it needs its own refusal AND its own accept.
rm -rf "$work"
work="$(make_work)"
rc=0
# An empty seam means the closed set is the scratch root alone, which is
# what a run with no environment override gets.
ROUTECTL_DRIVER_OUT_ROOT="" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-15 \
    --out "$work/pirate/land" --out-root "$work/pirate" || rc=$?
check "an --out-root outside the closed set is a usage error" "2" "$rc"
check_log "the refusal names the root flag" "refusing --out-root" \
    "$work/runner.log"
check "the refused run wrote nothing to the caller-chosen root" "no" \
    "$([ -e "$work/pirate" ] && echo yes || echo no)"
check "the root refusal precedes any daemon boot" "no" \
    "$([ -f "$work/stub.pid" ] && echo yes || echo no)"

# A PARENT of an allowed root must not pass either: a suffix-stripped or
# prefix compare would accept it and re-open the same hole one level up.
rc=0
ROUTECTL_DRIVER_OUT_ROOT="$work/scratch" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-15 \
    --out "$work/land" --out-root "$work" || rc=$?
check "a PARENT of an allowed root is refused" "2" "$rc"

# ACCEPT CONTROL: the same shape with the root named by the seam is
# accepted, so the refusals above are not a gate that refuses everything.
rm -rf "$work"
work="$(make_work)"
scratch="$work/scratch"
port_r="$(free_port)"
rc=0
ROUTECTL_DRIVER_PORT_MIN="$port_r" ROUTECTL_DRIVER_PORT_MAX="$port_r" \
ROUTECTL_DRIVER_OUT_ROOT="$scratch" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-15 \
    --out "$scratch/land" --out-root "$scratch" || rc=$?
check "an --out-root named by the environment seam is accepted" "0" "$rc"
check "the accepted root lands the fixture under itself" "yes" \
    "$([ -f "$scratch/land/anthropic-api/driver-selftest-15/meta.json" ] &&
        echo yes || echo no)"

# --- Case 16: the resume marker is PER landing root -------------------
# The marker records how far a landing root has been captured. A shared
# marker would let an exploratory scratch run advance it past a corpus
# root's own timestamps, silently suppressing a corpus recapture -- and a
# recapture that lands nothing is precisely the drift signal the driver
# path exists to produce. Pinned here so a later refactor cannot "fix" the
# path into a constant.

# Run the rig the way the runner does but WITHOUT `--force`, which is what
# makes the marker observable at all: the runner always forces, so a
# runner-level assertion alone would pass against any marker path
# whatsoever.
rig_run() {
    local work="$1" out="$2" case_id="$3"
    shift 3
    local rc=0
    (
        cd "$work/repo" || exit 2
        ROUTECTL_FIXTURE_CASE_ID="$case_id" \
        ROUTECTL_FIXTURE_CONFIG_SHA="$EXPECTED_SHA" \
        ROUTECTL_FIXTURE_CONNECTION_MODE="base-url" \
        ROUTECTL_FIXTURE_WIRE_PATTERN="baseline" \
            bash scripts/capture_fixtures.sh --driver-mode \
            --log "$work/canned-trace.log" --out "$out" --allow-unsafe-out "$@"
    ) >>"$work/rig.log" 2>&1 || rc=$?
    return "$rc"
}

work="$(make_work)"
root_a="$work/root-a"
root_b="$work/root-b"
port_q="$(free_port)"
port_r="$(free_port)"
while [ "$port_r" = "$port_q" ]; do port_r="$(free_port)"; done

# Two real runs, two different landing roots, same case.
rc=0
ROUTECTL_DRIVER_PORT_MIN="$port_q" ROUTECTL_DRIVER_PORT_MAX="$port_q" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-16 \
    --out "$root_a" --out-root "$work" || rc=$?
check "a run into the first landing root exits 0" "0" "$rc"
check "the first landing root holds its OWN marker" "yes" \
    "$([ -f "$root_a/.last_capture_ts" ] && echo yes || echo no)"
check "the second landing root has no marker yet" "no" \
    "$([ -e "$root_b/.last_capture_ts" ] && echo yes || echo no)"

rc=0
ROUTECTL_DRIVER_PORT_MIN="$port_r" ROUTECTL_DRIVER_PORT_MAX="$port_r" \
    runner_run "$work" --lane anthropic-api --case driver-selftest-16 \
    --out "$root_b" --out-root "$work" || rc=$?
check "a run into the second landing root exits 0" "0" "$rc"
check "the second landing root holds its own marker too" "yes" \
    "$([ -f "$root_b/.last_capture_ts" ] && echo yes || echo no)"
check "each root's fixture landed under that root" "yes" \
    "$([ -f "$root_a/anthropic-api/driver-selftest-16/meta.json" ] &&
        [ -f "$root_b/anthropic-api/driver-selftest-16/meta.json" ] && echo yes || echo no)"

# The suppression pair, asserted without `--force` so the marker is
# load-bearing. First direction is the POSITIVE CONTROL: a marker whose
# timestamp postdates the trace DOES suppress a capture into its own root,
# so the assertions below are about which root the marker governs and not
# about a marker nothing reads.
rm -rf "$root_a" "$root_b"
mkdir -p "$root_a" "$root_b"
printf '2099-01-01T00:00:00.000000Z\n' >"$root_a/.last_capture_ts"
rc=0
rig_run "$work" "$root_a" driver-selftest-16b || rc=$?
check "a future marker suppresses a capture into ITS OWN root" "3" "$rc"
check "the suppressed capture landed nothing in that root" "no" \
    "$([ -e "$root_a/anthropic-api" ] && echo yes || echo no)"

# Second direction: the same marker must not reach across roots.
rc=0
rig_run "$work" "$root_b" driver-selftest-16b || rc=$?
check "the first root's marker does not suppress a capture into the second" "0" "$rc"
check "the second root's capture landed despite the first root's marker" "yes" \
    "$([ -f "$root_b/anthropic-api/driver-selftest-16b/meta.json" ] && echo yes || echo no)"
rm -rf "$work"

if [ "$fails" -gt 0 ]; then
    echo "capture_driver self-test: $fails failure(s)"
    exit 1
fi
echo "capture_driver self-test: all assertions passed"
