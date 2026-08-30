#!/usr/bin/env bash
# Driver: a THIRD-PARTY agent CLI speaking the Anthropic dialect.
#
# The client is named by the environment, never by this file:
# `ROUTECTL_DRIVER_AGENT_BIN` is required and has no default. The harness
# it points at is a different codebase from the first-party clients, so its
# wire shape is independent evidence -- but its identity is the caller's to
# supply, which is also what lets an unshippable harness be driven without
# its name entering tracked content.
#
# ONE FILE PER HARNESS, and no harness dispatch: an undrivable harness has
# NO file here rather than a dead branch in a shared one.
#
# WHAT A CLIENT MUST DO to be drivable through this file:
#
#   * read its base URL from `ANTHROPIC_BASE_URL` and its credential from
#     `ANTHROPIC_API_KEY` (the driver exports both);
#   * accept one prompt per invocation behind a one-shot flag
#     (`ROUTECTL_DRIVER_AGENT_ONESHOT_FLAG`, default `-z`);
#   * print its version behind `--version`
#     (`ROUTECTL_DRIVER_AGENT_VERSION_FLAG`).
#
# Optional knobs, each skipped when unset:
#   ROUTECTL_DRIVER_AGENT_MODEL_FLAG      e.g. `-m`, takes the model id
#   ROUTECTL_DRIVER_AGENT_REASONING_FLAG  e.g. `--reasoning`, takes a level
#   ROUTECTL_DRIVER_AGENT_REASONING_LEVEL default `high`
#   ROUTECTL_DRIVER_AGENT_CONTINUE_FLAG   resumes the previous session;
#                                         REQUIRED for a multi-turn case
#   ROUTECTL_DRIVER_AGENT_EXTRA_ARGS      word-split, appended to every run
#
# A multi-turn case with no continue flag FAILS instead of running: N
# independent one-shot invocations would land a fixture labelled
# multi-turn whose trace holds N first turns, which is worse evidence than
# none.
#
# Usage (always through the runner, which exports the contract):
#   ROUTECTL_DRIVER_AGENT_BIN=<binary> \
#   scripts/capture_driver.sh --lane anthropic-api --case thinking-01 \
#     -- scripts/drivers/external-agent-cli.sh

set -eu

. "$(cd "$(dirname "$0")" && pwd)/lib/common.sh"

AGENT_BIN="${ROUTECTL_DRIVER_AGENT_BIN:-}"
[ -n "$AGENT_BIN" ] ||
  driver_die "ROUTECTL_DRIVER_AGENT_BIN is unset; this driver never names its client" 2
command -v "$AGENT_BIN" >/dev/null 2>&1 || [ -x "$AGENT_BIN" ] ||
  driver_die "ROUTECTL_DRIVER_AGENT_BIN '$AGENT_BIN' is not executable" 2

ONESHOT_FLAG="${ROUTECTL_DRIVER_AGENT_ONESHOT_FLAG:--z}"
VERSION_FLAG="${ROUTECTL_DRIVER_AGENT_VERSION_FLAG:---version}"
REASONING_LEVEL="${ROUTECTL_DRIVER_AGENT_REASONING_LEVEL:-high}"

driver_require_runner_env

# front-proxy is REFUSED here, before any daemon probe or client run: the
# trust path for the MITM CA is `NODE_EXTRA_CA_CERTS`, which only a Node
# client honors, and this driver's client is arbitrary by design. A client
# that ignored the carrier would not fail -- it would silently fall back
# to a direct connection and land a fixture labelled front-proxy whose
# shape is base-url. Fail closed in the one layer that knows the client
# is unverified; a driver written FOR a specific non-Node client carries
# its own trust path instead.
if [ "$ROUTECTL_FIXTURE_CONNECTION_MODE" = front-proxy ]; then
  driver_die "connection mode front-proxy is refused: this driver names no verified trust path for an arbitrary client, and one that ignores NODE_EXTRA_CA_CERTS would silently fall back to a direct connection" 2
fi

driver_require_daemon
CASE_FILE="$(driver_case_file)"
driver_seed_workspace "$CASE_FILE"
driver_apply_anthropic_connection_mode

# The version read is what makes this run's fixture say which client shape
# it pins. The flag is configurable because the binary is; a client that
# prints nothing fails the run inside driver_record_client.
driver_record_client external-agent-cli "$AGENT_BIN" "$VERSION_FLAG"

turn_count="$(driver_case_turns "$CASE_FILE" | wc -l)"
CONTINUE_FLAG="${ROUTECTL_DRIVER_AGENT_CONTINUE_FLAG:-}"
if [ "$turn_count" -gt 1 ] && [ -z "$CONTINUE_FLAG" ]; then
  driver_die "case '$ROUTECTL_FIXTURE_CASE_ID' has $turn_count turns but ROUTECTL_DRIVER_AGENT_CONTINUE_FLAG is unset; independent one-shots are not a multi-turn capture" 2
fi

common_argv=()
if [ -n "${ROUTECTL_DRIVER_AGENT_MODEL_FLAG:-}" ]; then
  common_argv+=("$ROUTECTL_DRIVER_AGENT_MODEL_FLAG" "$DRIVER_REQUEST_MODEL")
fi
if [ "$(driver_case_field "$CASE_FILE" thinking)" = true ] &&
   [ -n "${ROUTECTL_DRIVER_AGENT_REASONING_FLAG:-}" ]; then
  common_argv+=("$ROUTECTL_DRIVER_AGENT_REASONING_FLAG" "$REASONING_LEVEL")
fi
if [ -n "${ROUTECTL_DRIVER_AGENT_EXTRA_ARGS:-}" ]; then
  # shellcheck disable=SC2206 # word splitting is the documented contract
  common_argv+=($ROUTECTL_DRIVER_AGENT_EXTRA_ARGS)
fi

turn=0
while IFS= read -r prompt; do
  turn=$((turn + 1))
  argv=("$AGENT_BIN")
  [ "$turn" -gt 1 ] && argv+=("$CONTINUE_FLAG")
  [ "${#common_argv[@]}" -gt 0 ] && argv+=("${common_argv[@]}")
  argv+=("$ONESHOT_FLAG" "$prompt")
  rc=0
  "${argv[@]}" >>"$ROUTECTL_DRIVER_RUN/agent-turns.log" 2>&1 || rc=$?
  if [ "$rc" != 0 ]; then
    echo "driver: the agent CLI exited $rc on turn $turn" >&2
    tail -n 20 "$ROUTECTL_DRIVER_RUN/agent-turns.log" >&2 || true
    exit "$rc"
  fi
done < <(driver_case_turns "$CASE_FILE")

[ "$turn" -gt 0 ] || driver_die "case '$ROUTECTL_FIXTURE_CASE_ID' yielded no turns" 1

echo "driver: external-agent-cli case=$ROUTECTL_FIXTURE_CASE_ID mode=$ROUTECTL_FIXTURE_CONNECTION_MODE turns=$turn"
