#!/usr/bin/env bash
# Driver: Claude Code in NON-INTERACTIVE PRINT MODE (`claude -p`, the
# Agent SDK's own entry shape).
#
# The same binary as claude-code.sh, and that is the point of the separate
# file rather than an argument to it: `--print` is a different WIRE shape,
# not a different rendering. A print run has a different turn structure
# and a different tool loop from an interactive session, so a fixture from
# one is not evidence about the other. Being a flag on an installed binary
# also means this harness has no install gate and therefore nothing to
# file to the wild-evidence pen.
#
# ONE FILE PER HARNESS, no harness dispatch: an undrivable harness has NO
# file here.
#
# Usage (always through the runner, which exports the contract):
#   scripts/capture_driver.sh --lane anthropic-api --case tools-multiturn-01 \
#     -- scripts/drivers/claude-code-print.sh
#
# Multi-turn print runs resume: turn 1 opens a session with a caller-chosen
# `--session-id` and each later turn passes `--resume <id>`. Re-invoking
# without resume would send every turn as a fresh conversation and a
# multi-turn case would capture N one-turn shapes -- exactly the evidence
# the tool-loop cases exist to avoid.
#
# `ROUTECTL_DRIVER_CLAUDE_BIN` overrides the client binary (default
# `claude`) so the self-test can inject a stub; a real run needs a
# credential. front-proxy mode additionally needs
# `ROUTECTL_DRIVER_PROXY_URL` / `ROUTECTL_DRIVER_PROXY_CA`.

set -eu

. "$(cd "$(dirname "$0")" && pwd)/lib/common.sh"

CLAUDE_BIN="${ROUTECTL_DRIVER_CLAUDE_BIN:-claude}"

driver_require_runner_env
driver_require_daemon
CASE_FILE="$(driver_case_file)"
driver_seed_workspace "$CASE_FILE"
driver_apply_anthropic_connection_mode
driver_record_client claude-code-print "$CLAUDE_BIN"

export DISABLE_AUTOUPDATER=1
export DISABLE_TELEMETRY=1
export DISABLE_ERROR_REPORTING=1
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1

# A session id the run controls, so turn 2 can resume turn 1. Derived from
# the case id via a uuid v4 shape rather than from anything environmental:
# `--session-id` wants a uuid, and an environment-derived one would put a
# machine identifier in the client's own session directory.
SESSION_ID="${ROUTECTL_DRIVER_SESSION_ID:-$(
  printf '%s' "$ROUTECTL_FIXTURE_CASE_ID" | sha256sum | cut -c1-30 |
    sed 's/^\(........\)\(....\)\(...\)\(...\)\(............\)$/\1-\2-4\3-8\4-\5/'
)}"

common_argv=("--print" "--output-format" "text" "--model" "$DRIVER_REQUEST_MODEL")

# EVERY false knob is FORCED off, never left to a default. The client's own
# floor has tools, thinking, and prompt caching ON, so an unforced `false`
# captures that floor rather than the knob -- a baseline case would land with
# a tool list, a thinking budget, and cache breakpoints under a fixture
# claiming none of them.
if [ "$(driver_case_field "$CASE_FILE" tools)" = true ]; then
  common_argv+=(--permission-mode bypassPermissions)
else
  # The wildcard rather than an enumerated name list: a list silently rots as
  # the client grows tools, and the enumeration it replaces leaked 16 of them
  # onto the wire against a case asking for none.
  common_argv+=(--disallowed-tools "*")
fi

if [ "$(driver_case_field "$CASE_FILE" thinking)" = true ]; then
  common_argv+=(--effort high)
  export MAX_THINKING_TOKENS="${ROUTECTL_DRIVER_THINKING_TOKENS:-8192}"
else
  # A zero budget is what turns the request's thinking block from
  # {"type":"enabled",...} into {"type":"disabled"}.
  export MAX_THINKING_TOKENS=0
fi

if [ "$(driver_case_field "$CASE_FILE" cache_breakpoints)" != true ]; then
  export DISABLE_PROMPT_CACHING=1
fi

# The mcp-tools case hands the client a stdio MCP server, generated fresh
# per run rather than baked into the image: `python3` is already an image
# dependency, so a committed stub script needs no image change, and the
# server list itself is a driver concern, not a case knob (each client
# configures MCP differently). `--strict-mcp-config` keeps any ambient MCP
# config on the run's HOME out of this request entirely.
MCP_STUB_SCRIPT=""
if [ "$(driver_case_field "$CASE_FILE" wire_pattern)" = mcp-tools ]; then
  MCP_STUB_SCRIPT="$DRIVERS_DIR/stub_mcp.py"
  MCP_CONFIG_PATH="$ROUTECTL_DRIVER_RUN/mcp-config.json"
  cat >"$MCP_CONFIG_PATH" <<MCPCONFIG
{"mcpServers":{"fixture":{"command":"python3","args":["$MCP_STUB_SCRIPT"]}}}
MCPCONFIG
  common_argv+=(--mcp-config "$MCP_CONFIG_PATH" --strict-mcp-config)
fi

turn=0
while IFS= read -r prompt; do
  turn=$((turn + 1))
  argv=("$CLAUDE_BIN" "${common_argv[@]}")
  if [ "$turn" = 1 ]; then
    argv+=(--session-id "$SESSION_ID")
  else
    argv+=(--resume "$SESSION_ID")
  fi
  rc=0
  # The client's stdin is /dev/null, never the loop's. Inherited, the
  # client reads the remaining prompts off the pipe this loop is still
  # reading from: a 2-turn case then ran ONE turn with both prompts
  # concatenated into a single user text block, and the fixture claimed a
  # multi-turn shape it never carried.
  "${argv[@]}" "$prompt" \
    </dev/null >>"$ROUTECTL_DRIVER_RUN/print-turns.log" 2>&1 || rc=$?
  if [ "$rc" != 0 ]; then
    echo "driver: $CLAUDE_BIN --print turn $turn exited $rc" >&2
    tail -n 20 "$ROUTECTL_DRIVER_RUN/print-turns.log" >&2 || true
    exit "$rc"
  fi
done < <(driver_case_turns "$CASE_FILE")

# A case whose turns produced no invocation is a silent no-capture: the
# rig would run against a trace holding no dialogue and either refuse or,
# worse, land whatever else the daemon logged.
[ "$turn" -gt 0 ] || driver_die "case '$ROUTECTL_FIXTURE_CASE_ID' yielded no turns" 1

# The client owns the MCP stub's stdio child, so it dies with the client
# rather than needing this driver to spawn or track it directly. Asserted
# here rather than assumed: a client that leaks the child leaves it running
# past this point, discoverable by the one thing that identifies it -- the
# stub script's own path -- without sending it any signal.
if [ -n "$MCP_STUB_SCRIPT" ]; then
  mcp_leftover="$(driver_processes_matching "$MCP_STUB_SCRIPT")"
  if [ -n "$mcp_leftover" ]; then
    driver_die "the MCP stub process outlived the client that spawned it (pid(s): $(printf '%s' "$mcp_leftover" | tr '\n' ' '))" 1
  fi
fi

echo "driver: claude-code-print case=$ROUTECTL_FIXTURE_CASE_ID mode=$ROUTECTL_FIXTURE_CONNECTION_MODE turns=$turn"
