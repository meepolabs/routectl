#!/usr/bin/env bash
# Shared scaffolding for the harness drivers under scripts/drivers/.
#
# Sourced, never executed. It owns the parts of the driver contract that
# are identical for every harness -- reading the case through the single
# validator, seeding the throwaway cwd with the files the prompts name,
# proving the daemon is reachable before a client is launched, and
# recording the client version -- so a per-harness driver holds only the
# argv and env mapping that is genuinely specific to its client.
#
# ONE FILE PER HARNESS is the layout rule this library exists to keep
# affordable: without shared scaffolding each driver would carry a copy of
# all of the above, and "not yet drivable degrades to NO FILE" would cost
# five near-duplicate blocks instead of one.
#
# The runner (scripts/capture_driver.sh) is what exports the environment
# read here; `scripts/capture_driver.sh --help` is the contract.

# Every function below is called from a driver running under `set -eu`.

DRIVERS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CASES_DIR="$DRIVERS_DIR/cases"
VALIDATE_CASE="$DRIVERS_DIR/lib/validate_case.py"

driver_die() {
  echo "driver: $1" >&2
  exit "${2:-1}"
}

# Fail closed on the runner's contract rather than on a downstream
# symptom. A driver launched outside the runner would otherwise reach a
# client with an empty base URL and produce a capture of nothing.
driver_require_runner_env() {
  local var
  for var in ROUTECTL_BASE_URL ROUTECTL_DRIVER_RUN ROUTECTL_DRIVER_WORK \
             ROUTECTL_FIXTURE_CASE_ID ROUTECTL_FIXTURE_CONNECTION_MODE; do
    if [ -z "${!var:-}" ]; then
      driver_die "$var is unset; a driver runs under scripts/capture_driver.sh" 2
    fi
  done
}

# Resolve the case file for the runner's case id. The id comes from the
# runner rather than from a driver argument so the fixture pin and the
# case data can never name different scenarios.
driver_case_file() {
  local path="$CASES_DIR/$ROUTECTL_FIXTURE_CASE_ID.json"
  [ -r "$path" ] || driver_die "no case file for '$ROUTECTL_FIXTURE_CASE_ID' at $path" 2
  # Validated on every run, not just in the self-test: by the time a
  # driver is running there is a booted daemon and a client about to open
  # a session, so a malformed case has to fail before either is used.
  python3 "$VALIDATE_CASE" --check "$path" || driver_die "case '$ROUTECTL_FIXTURE_CASE_ID' is invalid" 2
  printf '%s\n' "$path"
}

driver_case_field() {
  python3 "$VALIDATE_CASE" --field "$2" "$1"
}

driver_case_turns() {
  python3 "$VALIDATE_CASE" --turns "$1"
}

# The daemon is a PRECONDITION for the client too, not only for the
# runner. The runner's own health poll ran before the driver started, and
# a daemon that died in between would otherwise leave a client failing in
# its own vocabulary -- a credential error, an empty session, a silent
# zero-request run -- and the rig would land a fixture off a trace that
# holds no dialogue.
driver_require_daemon() {
  curl -fsS -m 5 "$ROUTECTL_BASE_URL/health" >/dev/null 2>&1 ||
    driver_die "routectl at $ROUTECTL_BASE_URL is unreachable" 1
}

# Record the client identity read from the BINARY at run time. A corpus is
# a snapshot of a client version, and the installed client auto-updates:
# without a version read at run time a fixture cannot say which client
# shape it pins, and a mid-week update reads as unexplained drift.
#
# Empty output is a HARD failure, not a blank field. A run whose client
# cannot state its own version produces a fixture with no decay clock, and
# the whole point of case keying is that a client version change shows up
# as a diff rather than as silent rot.
#
# `meta.client.version` on the landed fixture is parsed by the capture rig
# out of the ingress `user-agent` -- the client's own self-report on the
# WIRE, which is the value a replay consumer can act on. The read here is
# the driver-side half: it fails the run BEFORE a session opens when the
# binary cannot be interrogated, and it puts the version in the run's own
# record (kept by the runner's `--keep`) next to the case and mode pins.
#
# args: <client-name> <binary> [version-flag, default --version]
driver_record_client() {
  local name="$1" bin="$2" flag="${3:---version}" version
  version="$("$bin" "$flag" 2>/dev/null | head -1 | tr -d '\r')" || version=""
  [ -n "$version" ] ||
    driver_die "'$bin $flag' produced nothing; the run would carry no decay clock" 1
  {
    printf 'name=%s\n' "$name"
    printf 'version=%s\n' "$version"
    printf 'connection_mode=%s\n' "$ROUTECTL_FIXTURE_CONNECTION_MODE"
    printf 'case_id=%s\n' "$ROUTECTL_FIXTURE_CASE_ID"
  } >"$ROUTECTL_DRIVER_RUN/client.txt"
  echo "driver: client=$name version=$version case=$ROUTECTL_FIXTURE_CASE_ID"
}

# Deterministic filler. `yes`-style repetition would compress to nothing
# and a random source would make two runs of one case incomparable, so the
# line body is a fixed synthetic string carrying only its own index.
_driver_write_filler() {
  local path="$1" bytes="$2"
  awk -v want="$bytes" 'BEGIN {
    written = 0
    i = 0
    while (written < want) {
      i++
      line = sprintf("row %06d synthetic filler for large-context capture", i)
      print line
      written += length(line) + 1
    }
  }' >"$path"
}

# Seed the throwaway cwd with the files the case prompts name. The set is
# fixed and documented in cases/README.md: a case describes an
# interaction, so it names files by convention instead of carrying a
# per-case file manifest that every driver would have to interpret.
#
# Every byte written here is synthetic. The client reads these files back
# into its own request bodies, so anything real in them would land in a
# committed fixture.
driver_seed_workspace() {
  local case_file="$1" padding table
  cd "$ROUTECTL_DRIVER_WORK"

  printf '17\n' >notes-alpha.txt
  printf '25\n' >notes-beta.txt

  if [ "$(driver_case_field "$case_file" cache_breakpoints)" = true ]; then
    table="$ROUTECTL_DRIVER_WORK/reference-table.txt"
    _driver_write_filler "$table" 32768
  fi

  padding="$(driver_case_field "$case_file" context_padding_bytes)"
  if [ "$padding" -gt 0 ]; then
    local remaining="$padding" chunk=262144 index=0
    while [ "$remaining" -gt 0 ]; do
      [ "$remaining" -lt "$chunk" ] && chunk="$remaining"
      index=$((index + 1))
      _driver_write_filler "$(printf 'filler-%02d.txt' "$index")" "$chunk"
      remaining=$((remaining - chunk))
    done
  fi
}

# The model id a driver asks its client for. A CASE never names a model:
# the set covers wire patterns, and which upstream model serves them is
# the lane config's business (`[aliases]` there catches this id by glob and
# maps it to the lane's own entry). Overridable for a lane whose alias
# vocabulary differs.
DRIVER_REQUEST_MODEL="${ROUTECTL_DRIVER_REQUEST_MODEL:-claude-sonnet-4-5}"

# Anything non-empty satisfies a client's own credential preflight: the
# client authenticates to the LOCAL daemon, and routectl injects the real
# upstream credential from the lane config's `api_key_ref`. A caller with
# a client that validates the value can pass its own through
# ROUTECTL_DRIVER_CLIENT_API_KEY.
DRIVER_LOCAL_KEY_PLACEHOLDER="routectl-driver-local"

driver_local_api_key() {
  printf '%s\n' "${ROUTECTL_DRIVER_CLIENT_API_KEY:-$DRIVER_LOCAL_KEY_PLACEHOLDER}"
}

# Export the environment an Anthropic-dialect client needs to reach the
# runner's daemon in the run's connection mode. THE TWO MODES EMIT
# DIFFERENT WIRE SHAPES -- a MITM front proxy carries `role:"system"`
# turns inside `messages[]`, while base-url mode inlines the same content
# as system-reminder text and sends zero system turns -- so the mode is a
# capture axis, not a transport preference.
#
# front-proxy needs the listener URL and the CA the client must trust.
# Neither is derivable from the daemon's base URL, and neither is in the
# lane config, so both arrive from the caller and an unset one fails the
# run: a front-proxy request that silently fell back to base-url would
# land labelled front-proxy and read as client drift forever.
driver_apply_anthropic_connection_mode() {
  # EVERY mode starts by clearing the OTHER mode's carriers. The runner
  # gives a driver a fresh HOME and cwd but forwards the caller's
  # environment, and an operator who routes their own client through
  # routectl has these set already -- so a front-proxy run that only ADDED
  # its proxy variables would leave an inherited ANTHROPIC_BASE_URL
  # pointing the client at whatever daemon the operator runs, and the
  # capture would be of that daemon rather than of the hermetic one.
  unset ANTHROPIC_BASE_URL ANTHROPIC_AUTH_TOKEN
  unset HTTPS_PROXY https_proxy HTTP_PROXY http_proxy NODE_EXTRA_CA_CERTS

  case "$ROUTECTL_FIXTURE_CONNECTION_MODE" in
    base-url)
      export ANTHROPIC_BASE_URL="$ROUTECTL_BASE_URL"
      export ANTHROPIC_API_KEY="$(driver_local_api_key)"
      ;;
    front-proxy)
      [ -n "${ROUTECTL_DRIVER_PROXY_URL:-}" ] ||
        driver_die "connection mode front-proxy requires ROUTECTL_DRIVER_PROXY_URL (the [mitm] listener)" 2
      [ -n "${ROUTECTL_DRIVER_PROXY_CA:-}" ] ||
        driver_die "connection mode front-proxy requires ROUTECTL_DRIVER_PROXY_CA (the [mitm] CA pem)" 2
      [ -r "$ROUTECTL_DRIVER_PROXY_CA" ] ||
        driver_die "front-proxy CA is unreadable: $ROUTECTL_DRIVER_PROXY_CA" 2
      export HTTPS_PROXY="$ROUTECTL_DRIVER_PROXY_URL"
      export https_proxy="$ROUTECTL_DRIVER_PROXY_URL"
      export NODE_EXTRA_CA_CERTS="$ROUTECTL_DRIVER_PROXY_CA"
      export ANTHROPIC_API_KEY="$(driver_local_api_key)"
      ;;
    *)
      driver_die "unsupported connection mode '$ROUTECTL_FIXTURE_CONNECTION_MODE' (base-url or front-proxy)" 2
      ;;
  esac
}

