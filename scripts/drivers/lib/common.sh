#!/usr/bin/env bash
# Shared scaffolding for the harness drivers under scripts/drivers/.
#
# Sourced, never executed. It owns the parts of the driver contract that
# are identical for every harness -- reading the case through the single
# validator, seeding the throwaway cwd with the files the prompts name,
# proving the daemon is reachable before a client is launched, recording
# the client version, and loading a committed client profile in the one
# order that is safe -- so a per-harness driver holds only the argv and env
# mapping that is genuinely specific to its client.
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
PROFILES_DIR="$DRIVERS_DIR/profiles"
VALIDATE_CASE="$DRIVERS_DIR/lib/validate_case.py"

# Basename of the run record `driver_record_client` writes. The runner
# reads `version=` back out of this file after the driver exits, so the
# name is a REPLICA of `CLIENT_RECORD` in scripts/capture_driver.sh (a
# driver library is not sourced by the runner; the drivers self-test
# asserts the two spellings agree).
DRIVER_CLIENT_RECORD="client.txt"

driver_die() {
  echo "driver: $1" >&2
  exit "${2:-1}"
}

# Fail closed on the runner's contract rather than on a downstream
# symptom. A driver launched outside the runner would otherwise reach a
# client with an empty base URL and produce a capture of nothing.
#
# ROUTECTL_FIXTURE_EXPECTED_INGRESS is deliberately absent from this list:
# no driver reads it, and the rig fails closed on it independently. A
# driver-side requirement would only make a driver refuse for a pin it has
# no use for, in a place further from the reader than the rig already is.
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
# binary cannot be interrogated, and it writes the version into the run's
# own record next to the case and mode pins.
#
# That record is not only a debugging artifact: the runner reads
# `version=` back out of it after the driver exits and forwards it to the
# rig as the binary-side pin, so the read reaches the fixture BEFORE the
# run workspace is removed. Two independent statements of one client's
# version then sit on the fixture, and a promotion boundary can refuse
# their disagreement instead of trusting the wire alone.
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
  } >"$ROUTECTL_DRIVER_RUN/$DRIVER_CLIENT_RECORD"
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
#
# THIS IS A PLACEHOLDER, NOT A CREDENTIAL, and the distinction is the one a
# lane-config author gets wrong -- the two names are one letter apart in
# intent:
#
#   ROUTECTL_DRIVER_CLIENT_API_KEY / ROUTECTL_DRIVER_CLIENT_BEARER
#       what the CLIENT presents to the LOCAL daemon. Never reaches an
#       upstream. Defaulted to an obviously-fake value here, correctly.
#   ROUTECTL_DRIVER_<PROVIDER>_API_KEY
#       the REAL upstream credential routectl injects on egress, keyed on
#       routectl's own provider name (`anthropic`, `openai`, `gemini`) and
#       never on a lane. A lane config names it as
#       `api_key_ref = "env://..."`. Never defaulted and never placeheld:
#       an absent one refuses the run rather than 401ing at the upstream.
#       See scripts/container/run_capture.sh for the full convention,
#       including the account-id variable the chatgpt-oauth surface needs.
DRIVER_LOCAL_KEY_PLACEHOLDER="routectl-driver-local"

driver_local_api_key() {
  printf '%s\n' "${ROUTECTL_DRIVER_CLIENT_API_KEY:-$DRIVER_LOCAL_KEY_PLACEHOLDER}"
}

# The bearer a front-proxy client puts on the wire as `Authorization`. It
# is a PLACEHOLDER, not a credential: the MITM seam admits a request only
# if one is present, and the request is then dispatched with the lane
# config's own `api_key_ref` credential -- the inbound bearer never leaves
# the daemon. So an obviously-fake value is both sufficient and the only
# correct thing to put in a driver environment.
DRIVER_LOCAL_BEARER_PLACEHOLDER="routectl-driver-front-proxy-placeholder-not-a-token"

driver_local_bearer() {
  printf '%s\n' "${ROUTECTL_DRIVER_CLIENT_BEARER:-$DRIVER_LOCAL_BEARER_PLACEHOLDER}"
}

# The carriers that DECIDE which daemon a client reaches. Named once,
# because two consumers must agree on the set: the connection-mode apply
# below unsets every one of them before setting its own, and the client
# profile loader REFUSES a profile that names any of them. A carrier added
# here is therefore forbidden in a profile the moment it becomes a
# carrier, with no second list to remember.
DRIVER_CONNECTION_CARRIERS="ANTHROPIC_BASE_URL ANTHROPIC_AUTH_TOKEN
HTTPS_PROXY https_proxy HTTP_PROXY http_proxy NODE_EXTRA_CA_CERTS"

# The ORDERING LATCH. Set by the connection-mode apply, read by the
# profile loader, and the reason the loader can refuse a late load at all.
DRIVER_CONNECTION_MODE_APPLIED=0

# Load one committed client profile from scripts/drivers/profiles/ (see
# that directory's README for the closed-set rule and the forbidden keys).
#
# BEFORE the connection-mode apply, never after. A profile sourced late
# could re-set ANTHROPIC_BASE_URL -- which the apply had just cleared --
# and the run would silently capture the OPERATOR'S LIVE DAEMON instead of
# the hermetic one. That is not hypothetical: this project caught exactly
# that fault once, which is why the ordering is a refusal in code rather
# than a sentence in a README.
#
# The file is PARSED, not sourced: `key=value` lines only, so a profile
# cannot run a command, and the forbidden classes below are checkable
# rather than a convention. Values are taken literally -- no expansion,
# no quote stripping.
#
# args: <profile-name>, resolving scripts/drivers/profiles/<name>.env
driver_load_client_profile() {
  local name="$1" path line key value carrier

  [ "$DRIVER_CONNECTION_MODE_APPLIED" = 0 ] ||
    driver_die "client profile '$name' was loaded AFTER the connection-mode apply; a profile that re-sets a connection carrier would point the client at the operator's live daemon" 2

  case "$name" in
    ''|*/*|*..*)
      driver_die "client profile name '$name' is not a committed profile in the closed set" 2 ;;
  esac
  path="$PROFILES_DIR/$name.env"
  [ -r "$path" ] ||
    driver_die "no committed client profile '$name' at $path" 2

  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      ''|'#'*) continue ;;
      *=*) ;;
      *) driver_die "client profile '$name' carries a non-key=value line: $line" 2 ;;
    esac
    key="${line%%=*}"
    value="${line#*=}"

    case "$key" in
      *[!A-Za-z0-9_]*|[0-9]*|'')
        driver_die "client profile '$name' names an invalid key '$key'" 2 ;;
    esac
    case "$value" in
      *'$'*|*'`'*)
        driver_die "client profile '$name' key '$key' carries a shell substitution; a profile is literal key=value" 2 ;;
    esac
    for carrier in $DRIVER_CONNECTION_CARRIERS; do
      [ "$key" != "$carrier" ] ||
        driver_die "client profile '$name' names the connection carrier '$key'; the connection mode owns those, not a profile" 2
    done
    # Credential-shaped keys, matched on the SUFFIX rather than anywhere in
    # the name: a substring test on `TOKEN` refuses `MAX_THINKING_TOKENS`,
    # which is one of the few keys a profile legitimately carries (the
    # thinking tier is a body-shape axis). Refusing it would make the whole
    # seam unusable for its first real profile.
    case "$key" in
      *_API_KEY|API_KEY|*_TOKEN|*_SECRET|*_BEARER|*_PASSWORD|*_CREDENTIALS)
        driver_die "client profile '$name' names the credential '$key'; a provider credential reaches routectl through the lane config's api_key_ref, never a committed profile" 2 ;;
    esac

    export "$key=$value"
  done <"$path"
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
#
# front-proxy ALSO needs a bearer, because the seam's admission gate
# rejects an `x-api-key`-only request before body parse. A probe of the
# admitted path measured where the credential on the wire comes from: the
# lane config's own provider credential, never the inbound bearer (the
# outgoing leg carried the daemon's minted identity, and mutating only the
# config credential -- with the client's bearer untouched -- changed the
# dispatch outcome). The exported bearer is therefore a placeholder that
# satisfies admission and nothing else, so no real client token belongs in
# a driver environment and none is accepted as a requirement here.
# `ANTHROPIC_API_KEY` stays exported alongside it for the client's own
# credential preflight. That the bearer actually reaches the wire as
# `authorization` is confirmed from a recorded real-client trace before
# any paid run.
#
# The gate's second requirement, `x-claude-code-session-id`, needs no
# export: the real client mints and sends it natively. A synthetic
# preflight request written by hand must carry both headers or it is
# rejected at admission and proves nothing.
driver_apply_anthropic_connection_mode() {
  # EVERY mode starts by clearing the OTHER mode's carriers. The runner
  # gives a driver a fresh HOME and cwd but forwards the caller's
  # environment, and an operator who routes their own client through
  # routectl has these set already -- so a front-proxy run that only ADDED
  # its proxy variables would leave an inherited ANTHROPIC_BASE_URL
  # pointing the client at whatever daemon the operator runs, and the
  # capture would be of that daemon rather than of the hermetic one.
  #
  # The same reason is why the latch closes here: from this point on, a
  # client profile load is refused, because a profile could re-set exactly
  # what this line clears.
  # shellcheck disable=SC2086 # the carrier list is a deliberate word split
  unset $DRIVER_CONNECTION_CARRIERS
  DRIVER_CONNECTION_MODE_APPLIED=1

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
      export ANTHROPIC_AUTH_TOKEN="$(driver_local_bearer)"
      export ANTHROPIC_API_KEY="$(driver_local_api_key)"
      ;;
    *)
      driver_die "unsupported connection mode '$ROUTECTL_FIXTURE_CONNECTION_MODE' (base-url or front-proxy)" 2
      ;;
  esac
}

