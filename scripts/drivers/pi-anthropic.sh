#!/usr/bin/env bash
# Driver: the pi coding agent on the Anthropic Messages dialect.
#
# external-agent-cli.sh owns the turn loop, the version read, and the
# front-proxy refusal; this file names pi's flags and hands over. `-p` is
# pi's one-shot, `-c` continues the most recent session, `--model` takes
# the model id, `--thinking` a level, and `--provider anthropic` pins the
# provider so a model id the bundled catalog lacks still resolves there.
#
# What it does first is pi-specific. pi IGNORES ANTHROPIC_BASE_URL -- its
# anthropic provider carries a hardcoded upstream -- so the carrier that
# actually points it at the daemon is `providers.anthropic.baseUrl` in a
# models.json written under the run's throwaway HOME (lib/pi.sh). The
# client binary handed over is lib/pi-continue-guard.sh, which refuses a
# `-c` with no session to continue and drops the ANTHROPIC_BASE_URL pi
# would not have read anyway.
#
# Usage (always through the runner, which exports the contract):
#   scripts/capture_driver.sh --lane anthropic-api --case tools-multiturn-01 \
#     --expected-ingress anthropic -- scripts/drivers/pi-anthropic.sh

set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/lib/common.sh"
. "$HERE/lib/pi.sh"

driver_require_runner_env
# The refusal is external-agent-cli.sh's; nothing is written before it.
if [ "$ROUTECTL_FIXTURE_CONNECTION_MODE" = base-url ]; then
  CASE_FILE="$(driver_case_file)"
  pi_clear_inherited_carriers
  pi_use_run_home
  pi_write_models "$CASE_FILE" anthropic "$ROUTECTL_BASE_URL"
  pi_write_settings "$CASE_FILE"
fi

export ROUTECTL_DRIVER_AGENT_BIN="$HERE/lib/pi-continue-guard.sh"
export ROUTECTL_DRIVER_AGENT_ONESHOT_FLAG="-p"
export ROUTECTL_DRIVER_AGENT_CONTINUE_FLAG="-c"
export ROUTECTL_DRIVER_AGENT_MODEL_FLAG="--model"
export ROUTECTL_DRIVER_AGENT_VERSION_FLAG="--version"
export ROUTECTL_DRIVER_AGENT_REASONING_FLAG="--thinking"
export ROUTECTL_DRIVER_AGENT_EXTRA_ARGS="--provider anthropic"
# external-agent-cli.sh applies the same brake after its version read;
# pi does not read it, the export closes the class for every driver.
export DISABLE_AUTOUPDATER=1

exec "$HERE/external-agent-cli.sh"
