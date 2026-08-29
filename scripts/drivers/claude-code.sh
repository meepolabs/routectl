#!/usr/bin/env bash
# Driver: INTERACTIVE Claude Code.
#
# Runs one canonical interaction case through `claude` in its default
# interactive mode, against the routectl the runner booted. Print mode
# lives in claude-code-print.sh: it is the same binary but a different
# wire shape (different turn structure and tool loop), which is exactly
# why the two are separate files.
#
# ONE FILE PER HARNESS. There is deliberately no harness dispatch here --
# a harness this box cannot drive has NO driver file, not a dead branch in
# a shared one, and an unshippable harness's driver can live outside the
# repo without any committed file changing.
#
# Usage (always through the runner, which exports the contract):
#   scripts/capture_driver.sh --lane anthropic-api --case thinking-01 \
#     [--connection-mode base-url|front-proxy] \
#     -- scripts/drivers/claude-code.sh
#
# BOTH CONNECTION MODES matter and this driver supports both. A MITM front
# proxy carries `role:"system"` turns inside `messages[]`; base-url mode
# inlines the same content as system-reminder TEXT and sends zero system
# turns. The mode is therefore a capture axis, and the runner pins it into
# every landed fixture as `meta.client.connection_mode`.
#
# `ROUTECTL_DRIVER_CLAUDE_BIN` overrides the client binary (default
# `claude`), the same override the runner has for the daemon and for the
# same reason: a real client run needs a credential, so the self-test
# injects a stub. front-proxy mode additionally needs
# `ROUTECTL_DRIVER_PROXY_URL` and `ROUTECTL_DRIVER_PROXY_CA`; see
# scripts/drivers/lib/common.sh.
#
# INTERACTIVE MEANS A PTY. `claude` with no `-p` renders a full-screen
# session and reads keystrokes, so the turns are typed into a pty
# allocated by `script(1)` rather than piped: a pipe on stdin puts the
# client in a non-interactive path and captures the wrong shape.

set -eu

. "$(cd "$(dirname "$0")" && pwd)/lib/common.sh"

CLAUDE_BIN="${ROUTECTL_DRIVER_CLAUDE_BIN:-claude}"

# Seconds to let the session settle before the first prompt is typed, and
# between turns. A turn typed before the previous one's tool loop finished
# would land as an interrupt rather than as a new turn, which captures a
# shape no user produces.
SETTLE_SECONDS="${ROUTECTL_DRIVER_SETTLE_SECONDS:-8}"
TURN_SECONDS="${ROUTECTL_DRIVER_TURN_SECONDS:-45}"
EXIT_SECONDS="${ROUTECTL_DRIVER_EXIT_SECONDS:-5}"

driver_require_runner_env
driver_require_daemon
CASE_FILE="$(driver_case_file)"
driver_seed_workspace "$CASE_FILE"
driver_apply_anthropic_connection_mode
driver_record_client claude-code "$CLAUDE_BIN"

# Keep the client off every network path that is not the capture. Its
# API traffic goes to routectl by construction; telemetry and error
# reporting would go straight upstream, and the auto-updater would move
# the client version mid-run -- which is the one value this corpus reads
# as its decay clock.
export DISABLE_AUTOUPDATER=1
export DISABLE_TELEMETRY=1
export DISABLE_ERROR_REPORTING=1
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1

argv=("$CLAUDE_BIN" --model "$DRIVER_REQUEST_MODEL")

# EVERY false knob is FORCED off, never left to a default. The client's own
# floor has tools, thinking, and prompt caching ON, so an unforced `false`
# captures that floor rather than the knob -- a baseline case would land with
# a tool list, a thinking budget, and cache breakpoints under a fixture
# claiming none of them.
if [ "$(driver_case_field "$CASE_FILE" tools)" = true ]; then
  argv+=(--permission-mode bypassPermissions)
else
  # The wildcard rather than an enumerated name list: a list silently rots as
  # the client grows tools, and the enumeration it replaces leaked 16 of them
  # onto the wire against a case asking for none.
  argv+=(--disallowed-tools "*")
fi

if [ "$(driver_case_field "$CASE_FILE" thinking)" = true ]; then
  argv+=(--effort high)
  export MAX_THINKING_TOKENS="${ROUTECTL_DRIVER_THINKING_TOKENS:-8192}"
else
  # A zero budget is what turns the request's thinking block from
  # {"type":"enabled",...} into {"type":"disabled"}.
  export MAX_THINKING_TOKENS=0
fi

if [ "$(driver_case_field "$CASE_FILE" cache_breakpoints)" != true ]; then
  export DISABLE_PROMPT_CACHING=1
fi

# Type the case's turns into the session, then leave. Written to a file
# and fed to the pty rather than assembled inline so the delays are
# explicit and the whole keystroke script is inspectable in a kept run
# workspace.
KEYS="$ROUTECTL_DRIVER_RUN/keystrokes.sh"
{
  printf '#!/usr/bin/env bash\nset -u\n'
  printf 'sleep %s\n' "$SETTLE_SECONDS"
  driver_case_turns "$CASE_FILE" | while IFS= read -r prompt; do
    printf 'printf "%%s\\r" %q\n' "$prompt"
    printf 'sleep %s\n' "$TURN_SECONDS"
  done
  printf 'printf "/exit\\r"\n'
  printf 'sleep %s\n' "$EXIT_SECONDS"
} >"$KEYS"
chmod +x "$KEYS"

# `script -q -c <cmd> /dev/null` gives the client a real pty while stdin
# stays ours to write into. The typescript goes to /dev/null: the capture
# sink is the daemon's trace, and a terminal transcript would only add a
# file full of escape sequences to scrub.
rc=0
"$KEYS" | script -q -e -c "$(printf '%q ' "${argv[@]}")" /dev/null >"$ROUTECTL_DRIVER_RUN/session.log" 2>&1 || rc=$?

if [ "$rc" != 0 ]; then
  echo "driver: $CLAUDE_BIN exited $rc" >&2
  tail -n 20 "$ROUTECTL_DRIVER_RUN/session.log" >&2 || true
  exit "$rc"
fi

echo "driver: claude-code case=$ROUTECTL_FIXTURE_CASE_ID mode=$ROUTECTL_FIXTURE_CONNECTION_MODE turns=$(driver_case_turns "$CASE_FILE" | wc -l)"
