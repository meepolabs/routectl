#!/usr/bin/env bash
# Driver: Codex CLI in one-shot mode (`codex exec`).
#
# Its dialect is EMPIRICAL, not assumed: driven end to end, codex reaches
# `POST /v1/responses` with `wire_api="responses"`. It gets its own file
# rather than riding external-agent-cli.sh, which is Anthropic-dialect and
# would hand codex a base-url variable it never reads -- exactly the
# silent-wrong-capture class the ingress-mismatch gate exists to catch.
#
# ONE FILE PER HARNESS, no harness dispatch: an undrivable harness has NO
# file here.
#
# codex has no `ANTHROPIC_BASE_URL`-style carrier; it is pointed at the
# local daemon through its OWN config-override mechanism
# (`-c model_providers.<id>.*`), re-passed on every `codex exec` invocation
# because each turn is a fresh process.
#
# `ROUTECTL_DRIVER_CODEX_BIN` overrides the client binary (default `codex`)
# so the self-test can inject a stub; a real run needs a credential behind
# the upstream lane, never behind this driver.
#
# Usage (always through the runner, which exports the contract):
#   scripts/capture_driver.sh --lane openai-responses-api --case tools-multiturn-01 \
#     --expected-ingress openai-responses -- scripts/drivers/codex.sh

set -eu

. "$(cd "$(dirname "$0")" && pwd)/lib/common.sh"

CODEX_BIN="${ROUTECTL_DRIVER_CODEX_BIN:-codex}"

driver_require_runner_env

# base-url is the ONLY mode this driver supports: codex names no MITM
# trust path (no NODE_EXTRA_CA_CERTS-equivalent carrier is documented for
# it), so a front-proxy request would either fail outright or -- worse --
# reach the upstream directly and land a fixture labelled front-proxy
# whose shape is actually base-url. Fail closed here rather than let that
# happen downstream.
if [ "$ROUTECTL_FIXTURE_CONNECTION_MODE" != base-url ]; then
  driver_die "connection mode '$ROUTECTL_FIXTURE_CONNECTION_MODE' is refused: this driver names no verified trust path outside base-url" 2
fi

driver_require_daemon
CASE_FILE="$(driver_case_file)"
driver_seed_workspace "$CASE_FILE"

driver_record_client codex "$CODEX_BIN"

# The updater brake, applied AFTER the version read so the value recorded
# is the one the run will keep. It is a no-op for a pinned static binary
# with no update path, but exporting it closes the class rather than
# leaving codex as the one driver that silently omits it.
export DISABLE_AUTOUPDATER=1

# A CASE never names a model (see lib/common.sh); the default here is
# codex-appropriate rather than the shared Anthropic-shaped default,
# computed straight off the runner's own env var rather than off
# DRIVER_REQUEST_MODEL, which common.sh has already defaulted to
# claude-sonnet-4-5 by the time this line runs.
CODEX_MODEL="${ROUTECTL_DRIVER_REQUEST_MODEL:-gpt-5-codex}"

# The name codex's config reads its credential env var from. Not
# ROUTECTL_DRIVER_CLIENT_API_KEY itself: codex wants the NAME of an env
# var in its config, not the value, so the value is placed under a
# codex-specific name and that name is what the config override points
# at. The value is the same placeholder every other driver uses: codex
# authenticates to the LOCAL daemon, and routectl injects the real
# upstream credential from the lane config's api_key_ref.
CODEX_LOCAL_KEY_VAR=CODEX_ROUTECTL_LOCAL_API_KEY
export "$CODEX_LOCAL_KEY_VAR=$(driver_local_api_key)"

# codex's own config-override mechanism, not a base-url env var: values
# are parsed as TOML, so a string needs its own embedded quotes. Re-built
# fresh for every invocation below, because each `codex exec` is an
# independent process with no state carried except what `resume --last`
# reopens from disk.
#
# Each override is TWO argv words to codex (`-c` and the `key=value`
# pair), so this emits them as two separate lines: the read loop below
# appends one array element per line, and a single `echo -c key=value`
# line would glue both words into ONE element, which codex's arg parser
# would not split back apart.
codex_config_args() {
  printf '%s\n%s\n' -c model_provider='"routectl"'
  printf '%s\n%s\n' -c model_providers.routectl.name='"routectl"'
  printf '%s\n%s\n' -c "model_providers.routectl.base_url=\"$ROUTECTL_BASE_URL/v1\""
  printf '%s\n%s\n' -c "model_providers.routectl.env_key=\"$CODEX_LOCAL_KEY_VAR\""
  printf '%s\n%s\n' -c model_providers.routectl.wire_api='"responses"'
}

turn=0
while IFS= read -r prompt; do
  turn=$((turn + 1))
  if [ "$turn" = 1 ]; then
    argv=("$CODEX_BIN" exec)
  else
    argv=("$CODEX_BIN" exec resume --last)
  fi
  argv+=(--skip-git-repo-check --dangerously-bypass-approvals-and-sandbox
    --model "$CODEX_MODEL")
  while IFS= read -r arg; do
    argv+=("$arg")
  done < <(codex_config_args)
  argv+=("$prompt")
  rc=0
  # The client's stdin is /dev/null, never the loop's, for the same reason
  # every other driver here holds it: inherited, a client that reads
  # stdin drains the remaining prompts off the pipe this loop is still
  # reading from, and a multi-turn case collapses into one turn.
  "${argv[@]}" </dev/null >>"$ROUTECTL_DRIVER_RUN/codex-turns.log" 2>&1 || rc=$?
  if [ "$rc" != 0 ]; then
    echo "driver: $CODEX_BIN exec turn $turn exited $rc" >&2
    tail -n 20 "$ROUTECTL_DRIVER_RUN/codex-turns.log" >&2 || true
    exit "$rc"
  fi
done < <(driver_case_turns "$CASE_FILE")

# A case whose turns produced no invocation is a silent no-capture: the
# rig would run against a trace holding no dialogue and either refuse or,
# worse, land whatever else the daemon logged.
[ "$turn" -gt 0 ] || driver_die "case '$ROUTECTL_FIXTURE_CASE_ID' yielded no turns" 1

echo "driver: codex case=$ROUTECTL_FIXTURE_CASE_ID mode=$ROUTECTL_FIXTURE_CONNECTION_MODE turns=$turn"
