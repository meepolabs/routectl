#!/usr/bin/env bash
# Shared scaffolding for the hermes drivers (scripts/drivers/hermes-*.sh).
#
# Sourced after lib/common.sh, never executed. hermes speaks several
# dialects off one binary, so each dialect is its own driver file; what
# they share -- where hermes keeps its state, which inherited variables
# could steer it away from the run's daemon, the config that names the
# daemon, and the per-turn loop -- lives here.
#
# The client binary is always `hermes` resolved from PATH. The self-test
# injects a stub by prepending its directory to PATH.

HERMES_BIN="hermes"

# The custom provider the written config declares and every turn selects.
HERMES_PROVIDER_NAME="routectl"

# The env var the written config names as the provider's key source. The
# config carries the NAME, never the value, so the file a run leaves behind
# holds no credential whatever the caller exported.
HERMES_LOCAL_KEY_VAR="HERMES_ROUTECTL_LOCAL_API_KEY"

# Variables hermes reads that would route a turn somewhere other than the
# endpoint the driver chose, or put a credential other than the
# placeholder in the client's reach. The anthropic credential pool is
# seeded from the first three, the OpenAI-shaped and Bedrock endpoints
# read the next six, and the HERMES_* trio picks a default provider, a
# default model, or a Codex endpoint override. Inherited from an operator
# shell, any of them could carry a real credential onto the wire of the
# run's daemon -- and so into a fixture -- or route a turn past it.
HERMES_INHERITED_CARRIERS="ANTHROPIC_TOKEN CLAUDE_CODE_OAUTH_TOKEN
ANTHROPIC_API_KEY ANTHROPIC_BASE_URL OPENAI_BASE_URL OPENAI_API_KEY
OPENROUTER_BASE_URL OPENROUTER_API_KEY AWS_BEARER_TOKEN_BEDROCK
HERMES_INFERENCE_PROVIDER HERMES_INFERENCE_MODEL HERMES_CODEX_BASE_URL"

# hermes keeps its config, session store, and credential pool under
# HERMES_HOME, defaulting to ~/.hermes. An inherited HERMES_HOME would
# point the run at the operator's real profile -- its stored credentials
# and its configured endpoints -- so it is always re-pointed, and only
# ever inside the run workspace.
hermes_use_run_home() {
  case "$HOME" in
    "$ROUTECTL_DRIVER_RUN"/*) ;;
    *) driver_die "HOME '$HOME' is not inside the run workspace; refusing to point hermes state there" 2 ;;
  esac
  export HERMES_HOME="$HOME/.hermes"
  mkdir -p "$HERMES_HOME"
}

hermes_clear_inherited_carriers() {
  # shellcheck disable=SC2086 # the carrier list is a deliberate word split
  unset $HERMES_INHERITED_CARRIERS $DRIVER_CONNECTION_CARRIERS
}

# Write $HERMES_HOME/config.yaml: the case knobs hermes takes only from
# config and, when an api_mode is given, one custom provider pointing at
# the run's daemon. JSON is YAML, and hermes reads it as such.
#
# The keys, as hermes reads them:
#   providers.<name>.base_url / api_mode / key_env -- the named custom
#       provider `--provider <name>` resolves to. api_mode is one of
#       chat_completions / codex_responses / anthropic_messages, and an
#       explicit one is honored as given; key_env names the env var the
#       key is read from.
#   platform_toolsets.cli: [] -- an explicitly saved EMPTY list is
#       authoritative and enables no toolset, which is how a false `tools`
#       knob is forced off rather than left to the default toolset.
#   prompt_caching.cache_ttl: "off" -- disables cache markers outright.
#   agent.reasoning_effort: "none" -- the level hermes maps to a disabled
#       reasoning block, where an unset one leaves its default level on.
#
# args: <case-file> [api_mode]
hermes_write_config() {
  local case_file="$1" api_mode="${2:-}"
  python3 - "$HERMES_HOME/config.yaml" "$HERMES_PROVIDER_NAME" \
    "$ROUTECTL_BASE_URL/v1" "$api_mode" "$HERMES_LOCAL_KEY_VAR" \
    "$(driver_case_field "$case_file" tools)" \
    "$(driver_case_field "$case_file" cache_breakpoints)" \
    "$(driver_case_field "$case_file" thinking)" <<'PY'
import json
import sys

path, name, base_url, api_mode, key_env, tools, cache, thinking = sys.argv[1:9]
config = {}
if api_mode:
    config["providers"] = {
        name: {
            "name": name,
            "base_url": base_url,
            "api_mode": api_mode,
            "key_env": key_env,
        }
    }
if tools != "true":
    config["platform_toolsets"] = {"cli": []}
if cache != "true":
    config["prompt_caching"] = {"cache_ttl": "off"}
if thinking != "true":
    config["agent"] = {"reasoning_effort": "none"}
with open(path, "w", encoding="utf-8") as fh:
    json.dump(config, fh, indent=2, sort_keys=True)
    fh.write("\n")
PY
  if [ -n "$api_mode" ]; then
    export "$HERMES_LOCAL_KEY_VAR=$(driver_local_api_key)"
  fi
}

# Every step before the client runs, for a dialect whose carrier is the
# written config. Leaves the validated case path in HERMES_CASE_FILE.
#
# base-url is the ONLY mode: hermes names no MITM trust path a driver can
# set, so a front-proxy request would either fail or reach the upstream
# directly and land a fixture labelled front-proxy whose shape is
# base-url. Refused before the daemon probe.
#
# args: <api_mode>
hermes_prepare_custom_provider() {
  local api_mode="$1" case_file

  driver_require_runner_env
  if [ "$ROUTECTL_FIXTURE_CONNECTION_MODE" != base-url ]; then
    driver_die "connection mode '$ROUTECTL_FIXTURE_CONNECTION_MODE' is refused: hermes names no verified trust path outside base-url" 2
  fi

  driver_require_daemon
  case_file="$(driver_case_file)"
  driver_seed_workspace "$case_file"
  hermes_use_run_home
  hermes_clear_inherited_carriers
  hermes_write_config "$case_file" "$api_mode"
  # shellcheck disable=SC2034 # read by the sourcing driver
  HERMES_CASE_FILE="$case_file"
}

# The session id a finished turn recorded in its usage report, or empty.
hermes_turn_session() {
  python3 - "$1" <<'PY' 2>/dev/null || true
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as fh:
        value = json.load(fh).get("session_id")
except (OSError, ValueError, AttributeError):
    value = None
print(value if isinstance(value, str) else "")
PY
}

# Drive the case one `-z` invocation per turn, `-c` from turn 2.
#
# `-c` resumes the most recent session, and a turn with nothing to resume
# would run as a fresh one-shot -- a case labelled multi-turn landing N
# first turns. So each turn writes hermes's own usage report, and a later
# turn runs only once the previous one recorded a session id.
#
# args: <case-file> <client-record-name> <model>
hermes_run_turns() {
  local case_file="$1" name="$2" model="$3" turn prompt rc session
  local log="$ROUTECTL_DRIVER_RUN/hermes-turns.log" usage=""
  local -a argv common_argv

  common_argv=(--provider "$HERMES_PROVIDER_NAME" -m "$model")
  if [ "$(driver_case_field "$case_file" thinking)" = true ]; then
    common_argv+=(--reasoning "${ROUTECTL_DRIVER_AGENT_REASONING_LEVEL:-high}")
  fi

  turn=0
  while IFS= read -r prompt; do
    turn=$((turn + 1))
    argv=("$HERMES_BIN")
    if [ "$turn" -gt 1 ]; then
      session="$(hermes_turn_session "$usage")"
      [ -n "$session" ] ||
        driver_die "turn $((turn - 1)) of case '$ROUTECTL_FIXTURE_CASE_ID' recorded no session id, so turn $turn has nothing to resume; independent one-shots are not a multi-turn capture" 1
      argv+=(-c)
    fi
    usage="$ROUTECTL_DRIVER_RUN/hermes-usage-$turn.json"
    argv+=("${common_argv[@]}" --usage-file "$usage" -z "$prompt")
    rc=0
    # The client's stdin is /dev/null, never the loop's. Inherited, a
    # client that reads stdin drains the remaining prompts off the pipe
    # this loop is still reading from, and a multi-turn case collapses
    # into one turn carrying every prompt concatenated.
    "${argv[@]}" </dev/null >>"$log" 2>&1 || rc=$?
    if [ "$rc" != 0 ]; then
      echo "driver: hermes -z turn $turn exited $rc" >&2
      tail -n 20 "$log" >&2 || true
      exit "$rc"
    fi
  done < <(driver_case_turns "$case_file")

  [ "$turn" -gt 0 ] || driver_die "case '$ROUTECTL_FIXTURE_CASE_ID' yielded no turns" 1

  echo "driver: $name case=$ROUTECTL_FIXTURE_CASE_ID mode=$ROUTECTL_FIXTURE_CONNECTION_MODE turns=$turn"
}
