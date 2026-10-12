#!/usr/bin/env bash
# Driver: the hermes agent CLI on the OpenAI Chat Completions dialect.
#
# hermes reaches routectl's `POST /v1/chat/completions` through a custom
# provider declared in its own config with `api_mode: chat_completions`.
# It has no base-url ENV carrier for a custom provider -- an inherited
# OPENAI_BASE_URL is cleared rather than trusted -- so this driver owns
# its carrier: it writes the config under the run's throwaway HOME and
# selects that provider by name on every turn. See lib/hermes.sh for the config
# keys and the turn loop.
#
# ONE FILE PER PAIRING: the same binary on a different dialect is a
# different wire shape, and external-agent-cli.sh is Anthropic-only.
#
# Usage (always through the runner, which exports the contract):
#   scripts/capture_driver.sh --lane openai-compat --case tools-multiturn-01 \
#     --expected-ingress openai -- scripts/drivers/hermes-chat.sh

set -eu

. "$(cd "$(dirname "$0")" && pwd)/lib/common.sh"
. "$DRIVERS_DIR/lib/hermes.sh"

hermes_prepare_custom_provider chat_completions
driver_record_client hermes-chat "$HERMES_BIN"

# The updater brake, applied AFTER the version read so the value recorded
# is the one the run keeps; the class is closed for every driver.
export DISABLE_AUTOUPDATER=1

hermes_run_turns "$HERMES_CASE_FILE" hermes-chat "$DRIVER_REQUEST_MODEL"
