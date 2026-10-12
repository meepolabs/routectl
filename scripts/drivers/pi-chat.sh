#!/usr/bin/env bash
# Driver: the pi coding agent on the OpenAI Chat Completions dialect.
#
# pi reaches routectl's `POST /v1/chat/completions` through a custom provider declared
# in its own models.json with `api: openai-completions`. pi has no base-url ENV
# carrier, so this driver owns its carrier: it writes models.json under the
# run's throwaway HOME and selects that provider by name on every turn. See
# lib/pi.sh for the models.json keys, the settings that carry the case
# knobs, and the turn loop.
#
# ONE FILE PER PAIRING: the same binary on a different dialect is a
# different wire shape, and external-agent-cli.sh is Anthropic-only.
#
# Usage (always through the runner, which exports the contract):
#   scripts/capture_driver.sh --lane openai-compat --case tools-multiturn-01 \
#     --expected-ingress openai -- scripts/drivers/pi-chat.sh

set -eu

. "$(cd "$(dirname "$0")" && pwd)/lib/common.sh"
. "$DRIVERS_DIR/lib/pi.sh"

pi_prepare_custom_provider openai-completions
driver_record_client pi-chat "$PI_BIN"

# The updater brake, applied AFTER the version read so the value recorded
# is the one the run keeps; the class is closed for every driver.
export DISABLE_AUTOUPDATER=1

pi_run_turns "$PI_CASE_FILE" pi-chat "$DRIVER_REQUEST_MODEL"
