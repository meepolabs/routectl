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

# Seconds between typing a line and pressing Enter. The client reads a
# multi-byte burst arriving in one write as a PASTE, and inside a paste a
# carriage return is text, not submit: a long prompt written together with
# its `\r` sat unsent in the input box, the `/exit` after it joined the same
# box, and the session never ended. A separate, later `\r` is a keypress.
SUBMIT_SECONDS="${ROUTECTL_DRIVER_SUBMIT_SECONDS:-1}"

# Seconds the session may stay open after the keystroke script has typed
# `/exit` and finished, before the driver ends it. Whatever kept the client
# up (a swallowed `/exit`, a turn still streaming, an unforeseen dialog),
# the run fails with a named error instead of hanging the cell.
EXIT_GRACE_SECONDS="${ROUTECTL_DRIVER_EXIT_GRACE_SECONDS:-30}"
KILL_AFTER_SECONDS=5

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

# A fresh client home opens on blocking first-run screens -- theme and
# onboarding, "use this API key?", and "do you trust this folder?" -- and
# keystrokes typed into them never reach the prompt, so the run records no
# dialogue. Seed exactly the three answers those screens persist, in the
# shape the client itself writes to `$HOME/.claude.json` after answering
# them by hand: onboarding done, the LOCAL placeholder key approved, and the
# run workspace trusted.
#
# A tools case runs with `--permission-mode bypassPermissions`, which opens
# one more blocking screen ("running in Bypass Permissions mode ... No, exit
# / Yes, I accept") whose default answer exits the client before any
# request. Its persisted answer is the user-settings key
# `skipDangerousModePermissionPrompt`, so `$HOME/.claude/settings.json`
# carries that one key and nothing else. Print mode never shows this screen,
# which is why claude-code-print.sh seeds no home.
#
# Nothing beyond those four answers: no account, no OAuth state, no other
# setting, and never a real credential.
#
# The key is recorded by its trailing 20 characters, which is the form the
# client stores an approval under; a full-value entry is not matched and
# the prompt reappears.
CLIENT_KEY_APPROVAL_SUFFIX_LEN=20

seed_client_home() {
  case "$HOME" in
    "$ROUTECTL_DRIVER_RUN"/*) ;;
    *) driver_die "HOME '$HOME' is not inside the run workspace; refusing to write client state there" 2 ;;
  esac
  local workspace key
  workspace="$(cd "$ROUTECTL_DRIVER_WORK" && pwd -P)"
  key="$(driver_local_api_key)"
  if [ "${#key}" -gt "$CLIENT_KEY_APPROVAL_SUFFIX_LEN" ]; then
    key="${key: -$CLIENT_KEY_APPROVAL_SUFFIX_LEN}"
  fi
  mkdir -p "$HOME/.claude"
  python3 - "$HOME/.claude.json" "$HOME/.claude/settings.json" "$workspace" "$key" <<'PY'
import json
import sys

state_path, settings_path, workspace, key_suffix = sys.argv[1:]
state = {
    "hasCompletedOnboarding": True,
    "customApiKeyResponses": {"approved": [key_suffix], "rejected": []},
    "projects": {workspace: {"hasTrustDialogAccepted": True}},
}
with open(state_path, "x", encoding="utf-8") as handle:
    json.dump(state, handle)
with open(settings_path, "x", encoding="utf-8") as handle:
    json.dump({"skipDangerousModePermissionPrompt": True}, handle)
PY
}
seed_client_home

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
    printf 'printf "%%s" %q\n' "$prompt"
    printf 'sleep %s\n' "$SUBMIT_SECONDS"
    printf 'printf "\\r"\n'
    printf 'sleep %s\n' "$TURN_SECONDS"
  done
  printf 'printf "/exit"\n'
  printf 'sleep %s\n' "$SUBMIT_SECONDS"
  printf 'printf "\\r"\n'
  printf 'sleep %s\n' "$EXIT_SECONDS"
} >"$KEYS"
chmod +x "$KEYS"

TURN_COUNT="$(driver_case_turns "$CASE_FILE" | wc -l)"
SESSION_BUDGET="$(awk -v s="$SETTLE_SECONDS" -v n="$TURN_COUNT" -v t="$TURN_SECONDS" \
  -v k="$SUBMIT_SECONDS" -v e="$EXIT_SECONDS" -v g="$EXIT_GRACE_SECONDS" \
  'BEGIN { print s + n * (k + t) + k + e + g }')"

# `script -q -c <cmd> /dev/null` gives the client a real pty while stdin
# stays ours to write into. The typescript goes to /dev/null: the capture
# sink is the daemon's trace, and a terminal transcript would only add a
# file full of escape sequences to scrub. On `timeout`'s TERM, `script`
# kills the client it spawned, so the bound ends the whole session.
rc=0
"$KEYS" | timeout --kill-after="$KILL_AFTER_SECONDS" "$SESSION_BUDGET" \
  script -q -e -c "$(printf '%q ' "${argv[@]}")" /dev/null >"$ROUTECTL_DRIVER_RUN/session.log" 2>&1 || rc=$?

if [ "$rc" = 124 ] || [ "$rc" = 137 ]; then
  echo "driver: --- session.log tail (escape sequences stripped) ---" >&2
  sed 's/\x1b\[[0-9;?<>=]*[a-zA-Z~]//g; s/\x1b\][^\x07]*\x07//g' "$ROUTECTL_DRIVER_RUN/session.log" |
    tr '\r' '\n' | tail -c 2000 >&2 || true
  echo >&2
  driver_die "$CLAUDE_BIN session still open ${EXIT_GRACE_SECONDS}s after /exit; ended it after ${SESSION_BUDGET}s (rc $rc)" 1
fi

if [ "$rc" != 0 ]; then
  echo "driver: $CLAUDE_BIN exited $rc" >&2
  tail -n 20 "$ROUTECTL_DRIVER_RUN/session.log" >&2 || true
  exit "$rc"
fi

echo "driver: claude-code case=$ROUTECTL_FIXTURE_CASE_ID mode=$ROUTECTL_FIXTURE_CONNECTION_MODE turns=$TURN_COUNT"
