#!/usr/bin/env bash
# Shared scaffolding for the pi drivers (scripts/drivers/pi-*.sh).
#
# Sourced, never executed. pi-continue-guard.sh sources it WITHOUT
# lib/common.sh, so nothing here may call a common.sh function at source
# time. pi speaks several dialects off one binary, so each dialect
# is its own driver file; what they share -- where pi keeps its state,
# which inherited variables could steer it away from the run's daemon, the
# models.json that names the daemon, the settings that carry the case
# knobs, and the per-turn loop with its resume guard -- lives here.
#
# The client binary is always `pi` resolved from PATH. The self-test
# injects a stub by prepending its directory to PATH.

PI_BIN="pi"

# The custom provider the responses/chat models.json declares and every
# turn of those drivers selects.
PI_PROVIDER_NAME="routectl"

# The env var models.json names as the provider's key source. pi resolves
# a `$NAME` apiKey from the environment at request time, so the file a run
# leaves behind holds the NAME, never a value, whatever the caller exported.
PI_LOCAL_KEY_VAR="PI_ROUTECTL_LOCAL_API_KEY"

# Variables pi reads that would put a credential other than the
# placeholder in the client's reach, move its state out of the run, or
# change the request shape behind the case's back. pi has no base-url env
# carrier at all (its anthropic provider ignores ANTHROPIC_BASE_URL), so
# the routing hazards are the state roots and the HTTP proxy variables,
# which DRIVER_CONNECTION_CARRIERS already names.
#   ANTHROPIC_OAUTH_TOKEN / ANTHROPIC_API_KEY / OPENAI_API_KEY /
#   AWS_BEARER_TOKEN_BEDROCK -- provider credentials pi reads natively.
#   PI_CODING_AGENT_DIR -- re-pointed below, never trusted.
#   PI_CODING_AGENT_SESSION_DIR -- would move the session store, and with
#       it what `-c` resumes, outside the run.
#   PI_CACHE_RETENTION -- `long` changes the cache_control the wire carries.
PI_INHERITED_CARRIERS="ANTHROPIC_OAUTH_TOKEN ANTHROPIC_API_KEY OPENAI_API_KEY
AWS_BEARER_TOKEN_BEDROCK PI_CODING_AGENT_DIR PI_CODING_AGENT_SESSION_DIR
PI_CACHE_RETENTION"

# pi keeps models.json, settings.json, auth.json, and its session store
# under PI_CODING_AGENT_DIR, defaulting to ~/.pi/agent. An inherited value
# would point the run at the operator's real profile -- its stored
# credentials and its configured endpoints -- so it is always re-pointed,
# and only ever inside the run workspace.
pi_use_run_home() {
  case "$HOME" in
    "$ROUTECTL_DRIVER_RUN"/*) ;;
    *) driver_die "HOME '$HOME' is not inside the run workspace; refusing to point pi state there" 2 ;;
  esac
  export PI_CODING_AGENT_DIR="$HOME/.pi/agent"
  mkdir -p "$PI_CODING_AGENT_DIR"
}

# Cleared BEFORE pi_use_run_home re-exports PI_CODING_AGENT_DIR. The two
# offline switches keep pi's startup catalog refresh, version check, and
# install ping off the network: none of them is a turn, and none of them
# may leave the box during a capture.
pi_clear_inherited_carriers() {
  # shellcheck disable=SC2086 # the carrier list is a deliberate word split
  unset $PI_INHERITED_CARRIERS $DRIVER_CONNECTION_CARRIERS
  export PI_OFFLINE=1 PI_TELEMETRY=0
}

# Write $PI_CODING_AGENT_DIR/models.json. Keys, as pi 1.1.0 validates them
# (ProviderConfigSchema in coding-agent's model-config.ts):
#   providers.<id>.baseUrl -- for the built-in `anthropic` provider this
#       overrides the endpoint of every catalog model; the Anthropic SDK
#       appends /v1/messages, so the value is the daemon root.
#   providers.<id>.api -- openai-responses / openai-completions for a
#       custom provider, which then needs `models` to be selectable.
#   providers.<id>.apiKey -- "$NAME" is resolved from the environment.
#   models[].reasoning -- off, pi clamps every thinking level to off.
#
# args: <case-file> <provider-id> <base-url> [api]
pi_write_models() {
  local case_file="$1" provider="$2" base_url="$3" api="${4:-}"
  python3 - "$PI_CODING_AGENT_DIR/models.json" "$provider" "$base_url" "$api" \
    "$PI_LOCAL_KEY_VAR" "$DRIVER_REQUEST_MODEL" \
    "$(driver_case_field "$case_file" thinking)" <<'PY'
import json
import sys

path, provider, base_url, api, key_var, model, thinking = sys.argv[1:8]
entry = {"baseUrl": base_url, "apiKey": "$" + key_var}
if api:
    entry["api"] = api
    entry["models"] = [{"id": model, "reasoning": thinking == "true"}]
with open(path, "w", encoding="utf-8") as fh:
    json.dump({"providers": {provider: entry}}, fh, indent=2, sort_keys=True)
    fh.write("\n")
PY
  export "$PI_LOCAL_KEY_VAR=$(driver_local_api_key)"
}

# Write $PI_CODING_AGENT_DIR/settings.json: the case knobs pi takes only
# from its settings.
#   defaultTools: [] -- an explicit empty list enables no tool, where an
#       absent one leaves read/bash/edit/write on.
#   defaultThinkingLevel: "off" -- the default level is medium, which a
#       reasoning model would put on the wire.
#   cacheWarming: "off" -- warming replays a request on a timer, an extra
#       request the case never asked for.
# pi has no setting that removes cache_control; a no-cache case on the
# anthropic dialect still carries pi's default cache markers.
#
# args: <case-file>
pi_write_settings() {
  python3 - "$PI_CODING_AGENT_DIR/settings.json" \
    "$(driver_case_field "$1" tools)" \
    "$(driver_case_field "$1" thinking)" <<'PY'
import json
import sys

path, tools, thinking = sys.argv[1:4]
settings = {"cacheWarming": "off"}
if tools != "true":
    settings["defaultTools"] = []
if thinking != "true":
    settings["defaultThinkingLevel"] = "off"
with open(path, "w", encoding="utf-8") as fh:
    json.dump(settings, fh, indent=2, sort_keys=True)
    fh.write("\n")
PY
}

# Whether pi has recorded a session `-c` could resume. pi writes a session
# file only once the conversation holds a message, under
# $PI_CODING_AGENT_DIR/sessions/<encoded cwd>/; the run has one cwd.
pi_has_session() {
  [ -d "$PI_CODING_AGENT_DIR/sessions" ] &&
    [ -n "$(find "$PI_CODING_AGENT_DIR/sessions" -type f -name '*.jsonl' -print -quit 2>/dev/null)" ]
}

# Every step before the client runs, for a dialect whose carrier is a
# custom provider in models.json. Leaves the validated case path in
# PI_CASE_FILE.
#
# base-url is the ONLY mode: pi names no MITM trust path a driver can set,
# so a front-proxy request would either fail or reach the upstream
# directly and land a fixture labelled front-proxy whose shape is
# base-url. Refused before the daemon probe.
#
# args: <api>
pi_prepare_custom_provider() {
  local api="$1" case_file

  driver_require_runner_env
  if [ "$ROUTECTL_FIXTURE_CONNECTION_MODE" != base-url ]; then
    driver_die "connection mode '$ROUTECTL_FIXTURE_CONNECTION_MODE' is refused: pi names no verified trust path outside base-url" 2
  fi

  driver_require_daemon
  case_file="$(driver_case_file)"
  driver_seed_workspace "$case_file"
  pi_clear_inherited_carriers
  pi_use_run_home
  pi_write_models "$case_file" "$PI_PROVIDER_NAME" "$ROUTECTL_BASE_URL/v1" "$api"
  pi_write_settings "$case_file"
  # shellcheck disable=SC2034 # read by the sourcing driver
  PI_CASE_FILE="$case_file"
}

# Drive the case one `-p` invocation per turn, `-c` from turn 2.
#
# `-c` continues the most recent session and, when there is none, SILENTLY
# starts a new one -- a case labelled multi-turn would land N first turns.
# So a later turn runs only once the previous one left a session behind.
#
# args: <case-file> <client-record-name> <model>
pi_run_turns() {
  local case_file="$1" name="$2" model="$3" turn prompt rc
  local log="$ROUTECTL_DRIVER_RUN/pi-turns.log"
  local -a argv common_argv

  common_argv=(--provider "$PI_PROVIDER_NAME" --model "$model")
  if [ "$(driver_case_field "$case_file" thinking)" = true ]; then
    common_argv+=(--thinking "${ROUTECTL_DRIVER_AGENT_REASONING_LEVEL:-high}")
  fi

  turn=0
  while IFS= read -r prompt; do
    turn=$((turn + 1))
    argv=("$PI_BIN" "${common_argv[@]}")
    if [ "$turn" -gt 1 ]; then
      pi_has_session ||
        driver_die "turn $((turn - 1)) of case '$ROUTECTL_FIXTURE_CASE_ID' recorded no session, so turn $turn has nothing to resume; independent one-shots are not a multi-turn capture" 1
      argv+=(-c)
    fi
    argv+=(-p "$prompt")
    rc=0
    # The client's stdin is /dev/null, never the loop's. Inherited, a
    # client that reads stdin drains the remaining prompts off the pipe
    # this loop is still reading from, and a multi-turn case collapses
    # into one turn carrying every prompt concatenated.
    "${argv[@]}" </dev/null >>"$log" 2>&1 || rc=$?
    if [ "$rc" != 0 ]; then
      echo "driver: pi -p turn $turn exited $rc" >&2
      tail -n 20 "$log" >&2 || true
      exit "$rc"
    fi
  done < <(driver_case_turns "$case_file")

  [ "$turn" -gt 0 ] || driver_die "case '$ROUTECTL_FIXTURE_CASE_ID' yielded no turns" 1

  echo "driver: $name case=$ROUTECTL_FIXTURE_CASE_ID mode=$ROUTECTL_FIXTURE_CONNECTION_MODE turns=$turn"
}
