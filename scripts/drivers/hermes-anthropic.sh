#!/usr/bin/env bash
# Driver: the hermes agent CLI on the Anthropic Messages dialect.
#
# hermes's built-in `anthropic` provider reads its endpoint from
# ANTHROPIC_BASE_URL and its key from ANTHROPIC_API_KEY, which is exactly
# the contract external-agent-cli.sh drives -- so this file only names the
# client and its flags and hands over. `-z` is hermes's one-shot, `-c`
# resumes the most recent session, `--provider anthropic` pins the
# provider so nothing in the environment can pick another one.
#
# What it does first is hermes-specific: every hermes state root is pointed
# inside the run workspace and every inherited routing variable hermes
# reads is cleared (lib/hermes.sh), so neither an operator's real hermes
# profile nor a stray OPENAI_BASE_URL can reach the turn. The case knobs
# hermes only takes from config are written there too.
#
# Usage (always through the runner, which exports the contract):
#   scripts/capture_driver.sh --lane anthropic-api --case tools-multiturn-01 \
#     --expected-ingress anthropic -- scripts/drivers/hermes-anthropic.sh

set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/lib/common.sh"
. "$HERE/lib/hermes.sh"

driver_require_runner_env
# The refusal is external-agent-cli.sh's; nothing is written before it.
if [ "$ROUTECTL_FIXTURE_CONNECTION_MODE" = base-url ]; then
  CASE_FILE="$(driver_case_file)"
  hermes_use_run_home
  hermes_clear_inherited_carriers
  hermes_write_config "$CASE_FILE"
fi

export ROUTECTL_DRIVER_AGENT_BIN="$HERMES_BIN"
export ROUTECTL_DRIVER_AGENT_ONESHOT_FLAG="-z"
export ROUTECTL_DRIVER_AGENT_CONTINUE_FLAG="-c"
export ROUTECTL_DRIVER_AGENT_MODEL_FLAG="-m"
export ROUTECTL_DRIVER_AGENT_REASONING_FLAG="--reasoning"
export ROUTECTL_DRIVER_AGENT_EXTRA_ARGS="--provider anthropic"
# external-agent-cli.sh applies the same brake after its version read;
# hermes does not read it, the export closes the class for every driver.
export DISABLE_AUTOUPDATER=1

exec "$HERE/external-agent-cli.sh"
