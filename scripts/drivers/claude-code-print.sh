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

NO_TOOL_LIST="Bash Read Write Edit Glob Grep WebFetch WebSearch Task NotebookEdit TodoWrite"

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
  printf '%s' "$ROUTECTL_FIXTURE_CASE_ID" | sha256sum | cut -c1-32 |
    sed 's/^\(........\)\(....\)\(...\)\(...\)\(............\)$/\1-\2-4\3-8\4-\5/'
)}"

common_argv=("--print" "--output-format" "text" "--model" "$DRIVER_REQUEST_MODEL")

if [ "$(driver_case_field "$CASE_FILE" tools)" = true ]; then
  common_argv+=(--permission-mode bypassPermissions)
else
  # shellcheck disable=SC2206 # the word split IS the flag's list form
  common_argv+=(--disallowed-tools $NO_TOOL_LIST)
fi

if [ "$(driver_case_field "$CASE_FILE" thinking)" = true ]; then
  common_argv+=(--effort high)
  export MAX_THINKING_TOKENS="${ROUTECTL_DRIVER_THINKING_TOKENS:-8192}"
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
  "${argv[@]}" "$prompt" >>"$ROUTECTL_DRIVER_RUN/print-turns.log" 2>&1 || rc=$?
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

echo "driver: claude-code-print case=$ROUTECTL_FIXTURE_CASE_ID mode=$ROUTECTL_FIXTURE_CONNECTION_MODE turns=$turn"
