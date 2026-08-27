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
    printf 'health=%s\n' "$(curl -fsS -m 2 "$ROUTECTL_BASE_URL/health" || echo unreachable)"
} >"$PROBE_OUT"
SH
    chmod +x "$1"
}

# Build a throwaway repo carrying the real runner, the real rig, the real
# scrub script, and the real committed lane config, plus the stub daemon
# and the probe driver. Prints the work root.
make_work() {
    local work
    work="$(mktemp -d)"
    mkdir -p "$work/repo/scripts/drivers/config" \
        "$work/repo/crates/routectl-cli/tests/fixtures" \
        "$work/bin"
    cp "$RUNNER" "$work/repo/scripts/capture_driver.sh"
    cp "$RIG" "$work/repo/scripts/capture_fixtures.sh"
    cp "$SCRUB" "$work/repo/scripts/scrub-fixture.sh"
    cp "$LANE_CONFIG" "$work/repo/scripts/drivers/config/anthropic-api.toml"
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

driver_corpus() {
    printf '%s\n' "$1/repo/crates/routectl-cli/tests/fixtures/driver"
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

    # The three pins plus the base URL are the driver's whole interface.
    check "the case id pin reaches the driver" "driver-selftest-01" \
        "$(probe_get "$work" case_id)"
    check "the config sha pin reaches the driver" "$EXPECTED_SHA" \
        "$(probe_get "$work" config_sha)"
    check "the connection mode pin defaults to base-url" "base-url" \
        "$(probe_get "$work" connection_mode)"
    check "the base url pin names the selected port" "http://127.0.0.1:$port_a" \
        "$(probe_get "$work" base_url)"
    check "the daemon answered the driver's own health probe" \
        '{"status":"ok","version":"stub"}' "$(probe_get "$work" health)"
else
    echo "FAIL: the driver command never ran (runner log: $work/runner.log)"
    sed -n '1,20p' "$work/runner.log"
    fails=$((fails + 8))
fi

# The pins must reach the RIG too, not just the driver: meta.json is where
# a rerun comparison reads them back.
meta="$(driver_corpus "$work")/anthropic-api/driver-selftest-01/meta.json"
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
else
    echo "FAIL: no fixture landed at $meta (runner log: $work/runner.log)"
    sed -n '1,30p' "$work/runner.log"
    fails=$((fails + 5))
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
meta="$(driver_corpus "$work")/anthropic-api/driver-selftest-02/meta.json"
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
if [ -d "$(driver_corpus "$work")/anthropic-api/driver-selftest-04" ]; then
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
if [ -d "$(driver_corpus "$work")/anthropic-api/driver-selftest-08" ]; then
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

# --- Case 9: the runner names nothing the operator's live daemon owns --
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

# --- Case 10: --help renders the header, sentinel included ------------
help_out="$(bash "$RUNNER" --help 2>&1 || true)"
if printf '%s' "$help_out" | grep -q 'ROUTECTL_FIXTURE_CONFIG_SHA' &&
    ! printf '%s' "$help_out" | grep -q 'END USAGE'; then
    echo "PASS: --help renders the driver contract without leaking the sentinel"
else
    echo "FAIL: --help output is truncated or leaks the sentinel"
    printf '%s\n' "$help_out" | tail -5
    fails=$((fails + 1))
fi

if [ "$fails" -gt 0 ]; then
    echo "capture_driver self-test: $fails failure(s)"
    exit 1
fi
echo "capture_driver self-test: all assertions passed"
