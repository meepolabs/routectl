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
SETTLE_SECONDS="${ROUTECTL_DRIVER_SETTLE_SECONDS-8}"
TURN_SECONDS="${ROUTECTL_DRIVER_TURN_SECONDS-45}"
EXIT_SECONDS="${ROUTECTL_DRIVER_EXIT_SECONDS-5}"

# Seconds between typing a line and pressing Enter. The client reads a
# multi-byte burst arriving in one write as a PASTE, and inside a paste a
# carriage return is text, not submit: a long prompt written together with
# its `\r` sat unsent in the input box, the `/exit` after it joined the same
# box, and the session never ended. A separate, later `\r` is a keypress.
SUBMIT_SECONDS="${ROUTECTL_DRIVER_SUBMIT_SECONDS-1}"

# Seconds the session may stay open after the keystroke script has typed
# `/exit` and finished, before the driver ends it. Whatever kept the client
# up (a swallowed `/exit`, a turn still streaming, an unforeseen dialog),
# the run fails with a named error instead of hanging the cell.
EXIT_GRACE_SECONDS="${ROUTECTL_DRIVER_EXIT_GRACE_SECONDS-30}"
KILL_AFTER_SECONDS=5

# Every pacing value reaches a `sleep` in the keystroke script and the
# session bound below, so each must be a whole number of seconds in a sane
# range; anything else fails the run before the client starts.
MAX_PACING_SECONDS=3600
require_pacing_seconds() {
  local name="$1" value="$2"
  case "$value" in
    '' | *[!0-9]*) driver_die "$name='$value' is not a whole number of seconds" 2 ;;
  esac
  if [ "${#value}" -gt 4 ] || [ "$((10#$value))" -gt "$MAX_PACING_SECONDS" ]; then
    driver_die "$name='$value' is above the ${MAX_PACING_SECONDS}s ceiling" 2
  fi
}
require_pacing_seconds ROUTECTL_DRIVER_SETTLE_SECONDS "$SETTLE_SECONDS"
require_pacing_seconds ROUTECTL_DRIVER_TURN_SECONDS "$TURN_SECONDS"
require_pacing_seconds ROUTECTL_DRIVER_EXIT_SECONDS "$EXIT_SECONDS"
require_pacing_seconds ROUTECTL_DRIVER_SUBMIT_SECONDS "$SUBMIT_SECONDS"
require_pacing_seconds ROUTECTL_DRIVER_EXIT_GRACE_SECONDS "$EXIT_GRACE_SECONDS"
SETTLE_SECONDS=$((10#$SETTLE_SECONDS))
TURN_SECONDS=$((10#$TURN_SECONDS))
EXIT_SECONDS=$((10#$EXIT_SECONDS))
SUBMIT_SECONDS=$((10#$SUBMIT_SECONDS))
EXIT_GRACE_SECONDS=$((10#$EXIT_GRACE_SECONDS))

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
SESSION_BUDGET=$((SETTLE_SECONDS + TURN_COUNT * (SUBMIT_SECONDS + TURN_SECONDS) + SUBMIT_SECONDS + EXIT_SECONDS + EXIT_GRACE_SECONDS))

# Seconds the driver waits for the pty's session leader to report in.
HANDSHAKE_SECONDS=30

# Open `path` (a fifo) as a read-only and a write-only descriptor without
# blocking: a read-write open first gives each end a peer, and is dropped.
# The read-only end is what lets a reader see EOF once every writer closes.
open_fifo_pair() {
  local path="$1" hold
  exec {hold}<>"$path"
  exec {FIFO_READ}<"$path" {FIFO_WRITE}>"$path"
  exec {hold}<&-
}

# Why `sid` cannot be the client's session, or nothing when it can. The id
# came off a pipe only the session leader held, and is still checked
# against what this driver started: a pid in its own session, other than
# the driver's, whose parent is the `script` that `timeout` (the driver's
# own child, `pty_pid`) is running.
session_leader_problem() {
  local sid="$1" stat own_sid leader_ppid leader_session script_ppid _x
  case "$sid" in
    '' | *[!0-9]* | ??????????*) echo "the leader reported '$sid', not a pid"; return ;;
  esac
  [ "$sid" -gt 1 ] || { echo "the leader reported pid $sid"; return; }
  stat="$(driver_proc_stat "$$")" || { echo "the driver cannot read its own session"; return; }
  read -r _x own_sid _x <<<"$stat"
  [ "$sid" != "$own_sid" ] || { echo "pid $sid is the driver's own session"; return; }
  stat="$(driver_proc_stat "$sid")" || { echo "pid $sid is not running"; return; }
  read -r leader_ppid leader_session _x <<<"$stat"
  [ "$leader_session" = "$sid" ] || { echo "pid $sid does not lead its own session"; return; }
  stat="$(driver_proc_stat "$leader_ppid")" || { echo "the parent of pid $sid is not running"; return; }
  read -r script_ppid _x <<<"$stat"
  [ "$script_ppid" = "$pty_pid" ] || echo "pid $sid was not started by this driver's pty"
}

# Stop a pty whose leader never proved itself: closing the ack makes the
# leader exit without starting the client, and the driver's own children
# are ended directly. Then fail the run.
abort_pty() {
  exec {ack_w}>&-
  kill -TERM "$pty_pid" 2>/dev/null || true
  kill -TERM -- "-$keys_pid" 2>/dev/null || true
  wait "$pty_pid" "$keys_pid" 2>/dev/null || true
  driver_die "client session handshake failed: $1" 1
}

# End every process left in the client's pty session: TERM, a short wait,
# then KILL. Ending `script` alone is not enough -- it signals only its
# direct child, so a client that ignores TERM, or a child it moved into a
# process group of its own, would outlive the run. The session id survives
# both, and only processes the client's session spawned carry it.
end_client_session() {
  local waited=0
  driver_signal_session TERM "$leader_sid"
  while [ -n "$(driver_session_pids "$leader_sid")" ] && [ "$waited" -lt "$KILL_AFTER_SECONDS" ]; do
    sleep 1
    waited=$((waited + 1))
  done
  driver_signal_session KILL "$leader_sid"
}

# `script -q -c <cmd> /dev/null` gives the client a real pty while stdin
# stays ours to write into. The typescript goes to /dev/null: the capture
# sink is the daemon's trace, and a terminal transcript would only add a
# file full of escape sequences to scrub.
#
# The shell `script` starts on the pty leads a new session, and that
# session id is what `end_client_session` sweeps. The leader hands it over
# on fd 9, an unlinked pipe, closes fd 9, and starts the client only after
# the driver acknowledges on fd 8. Nothing the client can write is ever
# read back: the driver reads one line, and the leader's line is written
# before the client exists.
pty_dir="$(mktemp -d "$ROUTECTL_DRIVER_RUN/pty.XXXXXX")"
mkfifo "$pty_dir/handshake" "$pty_dir/ack" "$pty_dir/keys"
open_fifo_pair "$pty_dir/handshake"
hs_r="$FIFO_READ" hs_w="$FIFO_WRITE"
open_fifo_pair "$pty_dir/ack"
ack_r="$FIFO_READ" ack_w="$FIFO_WRITE"
rm -f "$pty_dir/handshake" "$pty_dir/ack"

pty_command="printf '%s\\n' \"\$\$\" >&9; exec 9>&-; IFS= read -r go <&8; exec 8<&-; [ \"\$go\" = go ] || exit 97; exec $(printf '%q ' "${argv[@]}")"

setsid "$KEYS" >"$pty_dir/keys" {hs_r}<&- {hs_w}>&- {ack_r}<&- {ack_w}>&- &
keys_pid=$!
timeout --kill-after="$KILL_AFTER_SECONDS" "$SESSION_BUDGET" \
  script -q -e -c "$pty_command" /dev/null <"$pty_dir/keys" >"$ROUTECTL_DRIVER_RUN/session.log" 2>&1 \
  9>&"$hs_w" 8<&"$ack_r" {hs_r}<&- {hs_w}>&- {ack_r}<&- {ack_w}>&- &
pty_pid=$!
exec {hs_w}>&- {ack_r}<&-

leader_sid=""
IFS= read -r -t "$HANDSHAKE_SECONDS" -u "$hs_r" leader_sid ||
  { exec {hs_r}<&-; abort_pty "the pty's session leader reported no pid within ${HANDSHAKE_SECONDS}s"; }
exec {hs_r}<&-
leader_problem="$(session_leader_problem "$leader_sid")"
[ -z "$leader_problem" ] || abort_pty "$leader_problem"
printf 'go\n' >&"$ack_w"
exec {ack_w}>&-

rc=0
wait "$pty_pid" || rc=$?
end_client_session
kill -TERM -- "-$keys_pid" 2>/dev/null || true
wait "$keys_pid" 2>/dev/null || true
rm -rf "$pty_dir"

if [ "$rc" = 124 ] || [ "$rc" = 137 ]; then
  # The tail is client output headed for an operator's terminal, so it is
  # reduced to an allowlist rather than scrubbed of known sequences: tab,
  # newline and printable ASCII survive, a carriage return becomes a
  # newline, and every other byte (ESC, BEL, BS, the C1 range, non-ASCII)
  # becomes `?`. Sequence parameters remain as inert text.
  echo "driver: --- session.log tail (non-printable bytes shown as ?) ---" >&2
  tail -c 2000 "$ROUTECTL_DRIVER_RUN/session.log" |
    LC_ALL=C tr '\r' '\n' | LC_ALL=C tr -c '\t\n\040-\176' '?' >&2 || true
  echo >&2
  driver_die "$CLAUDE_BIN session still open ${EXIT_GRACE_SECONDS}s after /exit; ended it after ${SESSION_BUDGET}s (rc $rc)" 1
fi

if [ "$rc" != 0 ]; then
  echo "driver: $CLAUDE_BIN exited $rc" >&2
  tail -n 20 "$ROUTECTL_DRIVER_RUN/session.log" >&2 || true
  exit "$rc"
fi

echo "driver: claude-code case=$ROUTECTL_FIXTURE_CASE_ID mode=$ROUTECTL_FIXTURE_CONNECTION_MODE turns=$TURN_COUNT"
