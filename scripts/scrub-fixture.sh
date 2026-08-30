#!/usr/bin/env bash
# Scrub personal data out of a captured fixture, and REFUSE any fixture
# that still carries some.
#
# Two modes, deliberately asymmetric:
#
#   --write <path>...   Apply the transforms that are proven safe to
#                       automate: rewrite the operator's own home path to
#                       a neutral placeholder, and redact the VALUE of
#                       every credential-shaped and every account-scoped
#                       header while keeping its NAME. Runs as the fixture
#                       is written, before it is promoted into the corpus.
#
#   --check <path>...   Scan for residual personal data and exit NON-ZERO
#                       on the first class found. Names the file and the
#                       pattern CLASS; never echoes the matched value.
#
#   --lane-known <lane> QUERY the provider-shape table (PROVIDER_SHAPE_KINDS
#                       / PROVIDER_SHAPE_EXCLUDED below): does this gate
#                       have a credential-shape CLASSIFICATION for that
#                       lane? Takes no path, reads no fixture, scans
#                       nothing. Exit 0 classified, 1 unclassified.
#                       scripts/capture_fixtures.sh calls it in driver mode
#                       so a lane the gate knows nothing about cannot
#                       promote a fixture. This is the ONE enforcement
#                       point for that rule.
#
# Why the asymmetry: a rewriter silently promotes whatever pattern it
# failed to anticipate, and the operator never learns the fixture was
# dirty. So only the transforms whose correct rewrite is unambiguous
# are automated; everything else refuses and leaves the call to a human.
# `--check` is what a capture pipeline runs before promoting a fixture.
#
# Auth redaction is the load-bearing half of `--write`. A capture against
# an OAuth lane records a live bearer token in the raw header trace, and a
# corpus that ever held one is uncommittable in practice no matter what a
# later scan says. The name/value split matters: deleting the header would
# destroy the wire shape the fixture exists to pin, so the name survives
# and only the value collapses to a placeholder.
#
# THE DENY SET IS DERIVED FROM THE ENVIRONMENT, never hardcoded -- that is
# what makes the gate work on a second contributor's machine instead of
# only on the one it was written on. Classes:
#
#   home-path          literal $HOME
#   home-path-encoded  $HOME with `/` replaced by `-` (the
#                      `.claude/projects/-home-...` directory-name form
#                      that appears in tool-output paths)
#   git-author-name    `git config user.name`
#   git-author-email   `git config user.email`
#   hostname           `hostname` (or $HOSTNAME, or /etc/hostname)
#   seat-session-id    a `session_id` stored in the local seat store
#                      (`credentials.json`). routectl resolves it per seat
#                      and puts it on the wire as `x-claude-code-session-id`
#                      on the OAuth-bearer surface, so a capture on that
#                      surface records a stable per-credential identifier
#                      that fingerprints the operator across every fixture.
#                      This class is what catches the value once it has
#                      been copied OUT of a header -- into a body, a
#                      metadata `user_id`, a tool-output transcript --
#                      where no name-keyed rule can see it.
#   home-prefix        any `/home/<name>` other than the placeholder
#   home-prefix-encoded  the same, dash-encoded
#   ls-owner-column    an `ls -l` / `ls -l@` / `ls -o` long listing whose
#                      owner or group column is not a neutral name
#   auth-header        a credential-shaped header name whose value is not
#                      a redaction placeholder
#   anthropic-account-id  an `anthropic-organization-id` or
#                      `anthropic-workspace-id` header whose value is not a
#                      redaction placeholder. These arrive on a real
#                      upstream response and name the operator's cloud
#                      account; in the same file they sit beside the
#                      request id, the trace parent and the quota
#                      utilisation numbers, so the account id is what turns
#                      that telemetry into a profile of one operator.
#   claude-session-header  an `x-claude-code-session-id` header whose value
#                      is not a redaction placeholder. Denied by NAME
#                      regardless of provenance: the value is
#                      container-minted on some lanes and seat-derived on
#                      others, a public auditor cannot tell the two apart
#                      from the committed file, so the committed form is
#                      always the placeholder.
#   chatgpt-account-id  a `chatgpt-account-id` header whose value is not a
#                      redaction placeholder. The openai-responses egress
#                      stamps it from the seat's account id, so it names the
#                      operator's ChatGPT account. Denied by NAME for the
#                      same reason as the session header: the value is a
#                      plain uuid, so a value-shaped rule would refuse every
#                      uuid in a body.
#   bearer-token       a `bearer <opaque>` value anywhere in the fixture,
#                      scheme word matched case-insensitively
#   provider-key       a raw vendor credential carrying no scheme word at
#                      all (`sk-ant-api03-...`, `ghp_...`, `AKIA...`)
#   google-oauth-token a `ya29.`-prefixed Google OAuth access token
#   google-api-key     an `AIza`-prefixed Google API key
#   jwt                a three-segment `eyJ`-prefixed JSON Web Token
#   aws-temp-key-id    an `ASIA`-prefixed temporary AWS access key id
#   nvidia-api-key     an `nvapi-`-prefixed NVIDIA API key
#   headers-unparseable  a `*.headers.json` that is not valid JSON, so its
#                      auth content cannot be inspected at all
#
# The header classes and the BODY classes are separate layers on purpose.
# `auth-header` reasons over a parsed header NAME, so it covers any value
# under a credential-shaped name. A body has no such structure: a pasted
# `curl -H 'AUTHORIZATION: BEARER ...'`, a `cat .env` transcript, or a
# `~/.claude/.credentials.json` dump is just text, and asking a coding
# agent about a config file is routine traffic. So `bearer-token` and
# `provider-key` scan raw bytes anywhere in the fixture, header file or
# not.
#
# UN-INTERROGABLE ENVIRONMENT. A value the gate cannot read (no git
# identity configured, no hostname available, an empty $HOME) narrows the
# deny set -- so every skip prints a WARN naming the dropped class, and the
# skip is never silent. It is a warning rather than a hard failure on
# purpose: the classes that matter most on an unconfigured box
# (`home-prefix`, `ls-owner-column`, `auth-header`, `bearer-token`) are
# structural and stay active regardless, and a gate that refuses to run at
# all on a bare CI container is a gate somebody switches off. Two values
# are ALSO skipped when present but degenerate, because deny-listing them
# would flag legitimate content instead of personal data: a $HOME that is
# already the placeholder, and a hostname in the neutral set (routectl's
# own fixtures are full of `localhost` base URLs).
#
# Exit codes:
#   0  clean (--check), scrubbed (--write), or the lane is CLASSIFIED
#      (--lane-known): either it carries a prefix-detectable shape or it
#      is explicitly excluded with a written reason
#   1  residual personal data found (--check), or the lane is
#      UNCLASSIFIED (--lane-known) -- the gate has never been told
#      anything about it
#   2  usage error, unreadable path, or a missing prerequisite
#
# `--write` is NOT a substitute for `--check`: it never touches a git
# author name in a captured `git log` body or an `ls -l` owner column,
# because there is no safe automatic rewrite for either.
#
# Requires python3: the header files are JSON, and a regex rewrite of a
# JSON document is the exact failure class this repo already fixed once in
# the capture rig. Absent python3 is reported and fails, never skipped.
# --- END USAGE ---

# pipefail is safe here: no pipeline below relies on an early-terminating
# consumer (no `grep ... | head` producer taking SIGPIPE), so a failing
# stage is always a real error.
set -euo pipefail

# The neutral stand-ins written by --write and accepted by --check.
PLACEHOLDER_HOME="/home/user"
PLACEHOLDER_HOME_ENC="-home-user"

# Mirrors REDACTED_BEARER / REDACTED_SECRET in
# crates/routectl-core/src/log_safe.rs, so a fixture scrubbed here is
# indistinguishable from one whose trace was already redacted in-process.
REDACTED_BEARER="Bearer [REDACTED]"
REDACTED_SECRET="[REDACTED]"

# Owner/group names in an `ls -l` column that identify nobody.
NEUTRAL_OWNERS=("user" "root")

# Hostnames that are not personal identifiers. Deny-listing one of these
# would match routectl's own loopback base URLs in every fixture body.
NEUTRAL_HOSTNAMES=("localhost" "localhost.localdomain")

# Session ids that identify nobody. The nil UUID is what a placeholder,
# a doc example and a zeroed test record all carry, so deny-listing it
# would refuse legitimate content instead of the operator's seat.
NEUTRAL_SESSION_IDS=("00000000-0000-0000-0000-000000000000")

# Shortest environment-derived literal worth matching. A one- or two-char
# git name or hostname matches inside ordinary words and would turn the
# gate into noise; below this length the class is skipped with a warning.
MIN_LITERAL_LEN=3

# Print the header block as usage. Delimited by a sentinel rather than a
# line count: a magic `2,NNp` range silently starts cutting content the
# moment the header grows, which is how the python3 prerequisite fell out
# of `--help` once already.
usage() {
  sed -n '2,/^# --- END USAGE ---$/p' "$0" | sed '$d'
}

warn_skip() {
  echo "scrub-fixture: WARN deny class '$1' inactive -- $2" >&2
}

fatal() {
  echo "scrub-fixture: $1" >&2
  exit 2
}

MODE=""
LANE_QUERY=""
declare -a TARGETS=()

MODES_EXCLUSIVE="--check, --write and --lane-known are mutually exclusive"

while [ $# -gt 0 ]; do
  case "$1" in
    --check) [ -z "$MODE" ] || fatal "$MODES_EXCLUSIVE"; MODE="check"; shift ;;
    --write) [ -z "$MODE" ] || fatal "$MODES_EXCLUSIVE"; MODE="write"; shift ;;
    --lane-known)
      [ -z "$MODE" ] || fatal "$MODES_EXCLUSIVE"
      MODE="lane-known"
      shift
      [ $# -gt 0 ] || fatal "--lane-known requires a <lane> value (see --help)"
      LANE_QUERY="$1"
      shift
      ;;
    -h|--help) usage; exit 0 ;;
    --) shift; while [ $# -gt 0 ]; do TARGETS+=("$1"); shift; done ;;
    -*) fatal "unknown option: $1" ;;
    *) TARGETS+=("$1"); shift ;;
  esac
done

[ -n "$MODE" ] || fatal "one of --check, --write or --lane-known is required (see --help)"

if [ "$MODE" = "lane-known" ]; then
  # A table query, not a scan: no path, no fixture, no python3. An empty
  # lane value is a usage error rather than an unclassified answer --
  # `--lane-known "$lane"` with an unset variable would otherwise report
  # the caller's own bug as a table verdict.
  [ -n "$LANE_QUERY" ] || fatal "--lane-known was given an empty <lane> value (see --help)"
  [ "${#TARGETS[@]}" -eq 0 ] || fatal "--lane-known takes no path argument (see --help)"
else
  [ "${#TARGETS[@]}" -gt 0 ] || fatal "at least one path is required (see --help)"

  command -v python3 >/dev/null 2>&1 ||
    fatal "python3 not found; header JSON cannot be parsed and the gate refuses to guess"
fi

# --- deny set --------------------------------------------------------
# Parallel arrays: DENY_CLASS[i] names the class, DENY_VALUE[i] is the
# literal to search for. Structural classes carry no literal and are
# implemented as their own scanners below.
declare -a DENY_CLASS=()
declare -a DENY_VALUE=()
# Per-class grep flag set: `w` adds word-boundary matching, `i` adds
# case-insensitivity. Word boundaries keep a short git name from matching
# inside an unrelated word; a path is matched verbatim because a path
# segment boundary is not a word boundary.
declare -a DENY_FLAGS=()

add_deny() {
  DENY_CLASS+=("$1")
  DENY_VALUE+=("$2")
  DENY_FLAGS+=("$3")
}

is_neutral_hostname() {
  local candidate h
  candidate="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  for h in "${NEUTRAL_HOSTNAMES[@]}"; do
    [ "$candidate" = "$h" ] && return 0
  done
  return 1
}

is_neutral_owner() {
  local o
  for o in "${NEUTRAL_OWNERS[@]}"; do
    [ "$1" = "$o" ] && return 0
  done
  return 1
}

derive_home() {
  local home="${HOME:-}"
  home="${home%/}"
  if [ -z "$home" ]; then
    warn_skip "home-path" "\$HOME is unset or empty"
    warn_skip "home-path-encoded" "\$HOME is unset or empty"
    return 0
  fi
  case $home in
    *$'\n'*)
      # A newline would split the sed script in --write and silently skip
      # or corrupt the rewrite, promoting a fixture that still carries the
      # private path. Refuse loudly instead.
      fatal "\$HOME contains a newline; refusing to scrub"
      ;;
  esac
  if [ "$home" = "$PLACEHOLDER_HOME" ]; then
    warn_skip "home-path" "\$HOME is already the neutral placeholder"
    warn_skip "home-path-encoded" "\$HOME is already the neutral placeholder"
    return 0
  fi
  add_deny "home-path" "$home" ""
  add_deny "home-path-encoded" "${home//\//-}" ""
}

derive_git_identity() {
  local name email
  name="$(git config user.name 2>/dev/null || true)"
  email="$(git config user.email 2>/dev/null || true)"
  if [ "${#name}" -lt "$MIN_LITERAL_LEN" ]; then
    warn_skip "git-author-name" "git config user.name is unset or shorter than $MIN_LITERAL_LEN chars"
  else
    add_deny "git-author-name" "$name" "wi"
  fi
  if [ "${#email}" -lt "$MIN_LITERAL_LEN" ]; then
    warn_skip "git-author-email" "git config user.email is unset or shorter than $MIN_LITERAL_LEN chars"
  else
    add_deny "git-author-email" "$email" "i"
  fi
}

derive_hostname() {
  local host=""
  if command -v hostname >/dev/null 2>&1; then
    host="$(hostname 2>/dev/null || true)"
  fi
  [ -n "$host" ] || host="${HOSTNAME:-}"
  if [ -z "$host" ] && [ -r /etc/hostname ]; then
    host="$(tr -d '[:space:]' </etc/hostname)"
  fi
  if [ "${#host}" -lt "$MIN_LITERAL_LEN" ]; then
    warn_skip "hostname" "no hostname available, or shorter than $MIN_LITERAL_LEN chars"
    return 0
  fi
  if is_neutral_hostname "$host"; then
    warn_skip "hostname" "hostname is in the neutral set and would match legitimate loopback URLs"
    return 0
  fi
  add_deny "hostname" "$host" "wi"
}

is_neutral_session_id() {
  local candidate s
  candidate="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  for s in "${NEUTRAL_SESSION_IDS[@]}"; do
    [ "$candidate" = "$s" ] && return 0
  done
  return 1
}

# The path the seat store itself reads, mirroring
# crates/routectl-auth/src/oauth/file_io.rs `default_path`:
# $XDG_CONFIG_HOME/routectl/credentials.json, falling back to
# $HOME/.config/routectl/credentials.json. Deriving it the same way is
# what makes the class track a hermetic XDG (where there is no seat and
# the class must warn) as well as a real one.
seat_store_path() {
  local xdg="${XDG_CONFIG_HOME:-}" home="${HOME:-}"
  if [ -n "$xdg" ]; then
    printf '%s/routectl/credentials.json' "${xdg%/}"
    return 0
  fi
  [ -n "$home" ] || return 1
  printf '%s/.config/routectl/credentials.json' "${home%/}"
}

# Every `session_id` the local seat store holds, one per line.
#
# Read with python3 rather than a grep, for the same reason the header
# layer is parsed rather than regexed: a malformed or unexpected document
# must be distinguishable from one holding no session id, and a regex
# cannot make that call. The VALUES never leave this function's caller --
# they go into the deny set and are only ever reported by class name.
seat_session_ids() {
  python3 - "$1" <<'PY' 2>/dev/null || return 1
import json, sys

with open(sys.argv[1], encoding="utf-8") as fh:
    doc = json.load(fh)
if not isinstance(doc, dict):
    raise SystemExit(1)
providers = doc.get("providers")
if not isinstance(providers, dict):
    raise SystemExit(0)
for record in providers.values():
    if not isinstance(record, dict):
        continue
    sid = record.get("session_id")
    if isinstance(sid, str) and sid:
        print(sid)
PY
}

# routectl resolves a seat's `session_id` on the OAuth-bearer surface
# (crates/routectl-router/src/factory/build.rs) and puts it on the wire as
# `x-claude-code-session-id`. That header name is VISIBLE to the header
# rules, so a capture on that surface lands the value verbatim in the
# corpus -- a stable identifier that ties every fixture to one operator.
#
# One deny entry per stored seat, all under the one class name: which seat
# a value came from does not change the remedy, and the class name is the
# entire diagnostic a refusal gets. Derived from the store on disk exactly
# as the daemon reads it, so every path out of here that adds nothing to
# the deny set warns -- the class's absence must be loud.
derive_seat_session_id() {
  local path ids sid n=0
  if ! path="$(seat_store_path)"; then
    warn_skip "seat-session-id" "neither \$XDG_CONFIG_HOME nor \$HOME is set, so the seat store path is unknown"
    return 0
  fi
  if [ ! -r "$path" ]; then
    warn_skip "seat-session-id" "no readable seat store at the configured credentials path"
    return 0
  fi
  if ! ids="$(seat_session_ids "$path")"; then
    warn_skip "seat-session-id" "the seat store is not readable as the expected JSON document"
    return 0
  fi
  while IFS= read -r sid; do
    [ -n "$sid" ] || continue
    if [ "${#sid}" -lt "$MIN_LITERAL_LEN" ]; then
      warn_skip "seat-session-id" "a stored session id is shorter than $MIN_LITERAL_LEN chars"
      continue
    fi
    if is_neutral_session_id "$sid"; then
      warn_skip "seat-session-id" "a stored session id is in the neutral set and would match placeholder content"
      continue
    fi
    n=$((n + 1))
    add_deny "seat-session-id" "$sid" "i"
  done <<<"$ids"
  [ "$n" -gt 0 ] ||
    warn_skip "seat-session-id" "the seat store holds no usable session id"
}

# The deny set and the file list belong to the two SCANNING modes only.
# `--lane-known` answers from the table below and touches no fixture, so
# deriving an environment deny set for it would emit WARN lines about
# classes no scan is going to run, and collecting files would demand a
# path the mode does not take.
if [ "$MODE" != "lane-known" ]; then
  derive_home
  derive_git_identity
  derive_hostname
  derive_seat_session_id
fi

# --- file collection -------------------------------------------------
# A target may be a fixture directory (scanned recursively) or a single
# file. An unreadable target is a hard error, never an empty scan -- so the
# validation runs HERE, in the current shell, and not inside the `find`
# pipeline below: a `fatal` from a process substitution exits only the
# subshell, and the gate would report a vacuous clean scan.
validate_targets() {
  local t
  for t in "${TARGETS[@]}"; do
    if [ ! -d "$t" ] && [ ! -f "$t" ]; then
      fatal "not a readable file or directory: $t"
    fi
  done
}

collect_files() {
  local t
  for t in "${TARGETS[@]}"; do
    if [ -d "$t" ]; then
      find "$t" -type f -print
    else
      printf '%s\n' "$t"
    fi
  done
}

declare -a FILES=()
if [ "$MODE" != "lane-known" ]; then
  validate_targets

  while IFS= read -r f; do
    [ -n "$f" ] && FILES+=("$f")
  done < <(collect_files)

  # A directory target holding no files at all would otherwise scan nothing
  # and print PASS.
  [ "${#FILES[@]}" -gt 0 ] || fatal "no files found under the given path(s); refusing a vacuous scan"
fi

# --- structural scanners ---------------------------------------------
# Each returns 0 when the class is PRESENT in the file. None of them
# prints the matched text: the class name is the diagnostic, and echoing
# the value would copy the leak into a CI log.

# Any `/home/<name>` that is not the neutral placeholder. Candidates are
# extracted and compared exactly, so `/home/user/x` is accepted while
# `/home/userx` is not -- a comparison a plain regex cannot make without
# lookahead.
#
# Non-ASCII usernames: the `case` comparison is byte-exact, so a
# `/home/<non-Latin>` path IS caught -- the extraction is what narrows it,
# since the character class is ASCII. A fully non-Latin username therefore
# yields the bare `/home/` prefix, which is skipped as a false positive.
# The supplementary check below covers it without contorting the
# extraction: a `/home/` followed by any non-ASCII, non-separator byte is
# a name this class cannot read but must still refuse.
has_home_prefix() {
  local candidate
  while IFS= read -r candidate; do
    [ -n "$candidate" ] || continue
    case "$candidate" in
      "/home/" | "$PLACEHOLDER_HOME") continue ;;
    esac
    return 0
  done < <(grep -oE '/home/[A-Za-z0-9._-]*' "$1" || true)
  # A home directory whose name starts outside ASCII.
  LC_ALL=C grep -qE '/home/[^A-Za-z0-9._/"'"'"'[:space:]-]' "$1" && return 0
  return 1
}

# The dash-encoded twin of has_home_prefix: the `-home-<name>` shape that
# `.claude/projects/` directory names carry into tool output. The plain
# form was refused for a third party while this one passed, so a body
# echoing someone else's project dir name landed clean.
#
# Requires a trailing `-` after the name, because that is what the encoding
# guarantees (`/home/x/Desktop` -> `-home-x-Desktop`) and it is what keeps
# the rule off an ordinary hyphenated word ending in `-home-`. The name
# segment is compared against the placeholder's own name exactly, mirroring
# the plain form's accept rule.
has_home_prefix_encoded() {
  local candidate name
  while IFS= read -r candidate; do
    [ -n "$candidate" ] || continue
    # Strip the leading `-home-` and the trailing `-`.
    name="${candidate#-home-}"
    name="${name%-}"
    [ -n "$name" ] || continue
    [ "$name" = "${PLACEHOLDER_HOME##*/}" ] && continue
    return 0
  done < <(grep -oE -- '-home-[A-Za-z0-9._]+-' "$1" || true)
  return 1
}

# An `ls -l` long listing. The trailing size field is required so an
# unanchored 10-character mode string in ordinary prose is not enough to
# trigger; the owner and group columns are then compared against the
# neutral set. Unanchored on purpose: in a captured body the listing sits
# inside a JSON string, past an escaped newline, never at line start.
#
# Two variants beyond GNU `ls -l`, because a listing pasted from any of
# them carries the same owner name: macOS `ls -l@` / `ls -le` append `@`
# or `+` to the mode string, and `ls -o` omits the GROUP column entirely.
# The `-o` form is a SEPARATE pattern rather than an optional group in one
# regex: with the group made optional, the size field would match the
# group column and the owner check would read the wrong field.
LS_MODE_RE='[-dlbcps][rwxsStT-]{9}[.+@e]?'
LS_LONG_RE="$LS_MODE_RE"'[[:space:]]+[0-9]+[[:space:]]+[A-Za-z0-9_.-]+[[:space:]]+[A-Za-z0-9_.-]+[[:space:]]+[0-9]+'
# `ls -o`: mode, links, owner, size -- no group. The size field is
# numeric and the owner is not, so requiring a non-numeric owner keeps
# this from matching the `-l` form's `owner group size` tail.
LS_SHORT_RE="$LS_MODE_RE"'[[:space:]]+[0-9]+[[:space:]]+[A-Za-z_][A-Za-z0-9_.-]*[[:space:]]+[0-9]+'

# True when any owner or group column in a listing names a non-neutral
# account. `$1` is the file, `$2` the regex, and the remaining args are the
# awk field indices to treat as account names.
has_non_neutral_account() {
  local file="$1" regex="$2"
  shift 2
  local match idx account
  while IFS= read -r match; do
    [ -n "$match" ] || continue
    for idx in "$@"; do
      account="$(printf '%s\n' "$match" | awk -v i="$idx" '{print $i}')"
      [ -n "$account" ] || continue
      is_neutral_owner "$account" || return 0
    done
  done < <(grep -oE "$regex" "$file" || true)
  return 1
}

has_ls_owner_column() {
  # `-l`: owner is field 3, group field 4. `-o`: owner is field 3 and
  # there is no group. Checked separately so each form's field positions
  # are read correctly.
  has_non_neutral_account "$1" "$LS_LONG_RE" 3 4 && return 0
  has_non_neutral_account "$1" "$LS_SHORT_RE" 3 && return 0
  return 1
}

# A `bearer <opaque>` value anywhere in the fixture, including inside a
# body a caller pasted a token into. Case-INSENSITIVE on the scheme word:
# a body carrying a pasted `curl -H "AUTHORIZATION: BEARER ..."` is the
# reachable shape, and the header layer already matches its own names
# case-insensitively. `:` is in the token class because JWT-adjacent and
# vendor-scoped tokens carry it, and the separator is a whitespace class
# rather than one literal space so a wrapped paste still matches.
#
# The `[REDACTED]` placeholder cannot match: `[` and `]` are outside the
# token class, leaving no 16-char run. The accept direction is asserted.
BEARER_RE='bearer[[:space:]]+[A-Za-z0-9._~+/=:-]{16,}'

has_bearer_token() {
  grep -qiE "$BEARER_RE" "$1"
}

# A raw provider credential carrying NO scheme word, which is how one
# arrives in a body: an `ANTHROPIC_API_KEY=...` line from a `cat .env`, or
# a `~/.claude/.credentials.json` transcript. Keyed on the vendor prefixes
# rather than on entropy, so the shape is unambiguous and the
# false-positive risk is near zero -- but each prefix still requires >=16
# opaque chars AFTER it, so documentation prose naming a bare prefix does
# not trip the rule. That accept direction is asserted.
PROVIDER_KEY_RE='(sk-ant-api03-|sk-ant-oat01-|sk-ant-ort01-|sk-proj-|sk-or-v1-|ghp_|AKIA)[A-Za-z0-9_-]{16,}'

has_provider_key() {
  grep -qE "$PROVIDER_KEY_RE" "$1"
}

# Vendor shapes the prefix set above does not carry, each its OWN named
# rule and its OWN finding class: a class name is the entire diagnostic a
# refused capture gets, so folding five vendors into one alternation would
# tell the operator "provider-key" and nothing about which shape to look
# for. They also each need their own anchor, and one alternation cannot
# carry five.
#
# LEFT ANCHOR, on every one of them. `(^|[^A-Za-z0-9_-])` requires the
# prefix to begin a token rather than land mid-run. Measured: unanchored,
# an `AIza`/`sk-`-shaped rule fires 2-4 times per 64MB of random base64
# (an SSE body is exactly that); anchored, zero, with every positive
# control still firing. A captured response body is mostly base64, so the
# unanchored form is a gate somebody switches off.
#
# The `eyJ` prefix on the JWT rule is mandatory, and free: measured over
# 462MB of real traffic, the eyJ-anchored rule and a bare three-segment
# matcher have the IDENTICAL hit set, because neither standard nor
# url-safe base64 emits `.`, so only a genuinely JWT-shaped value carries
# the separators. A prefix-free three-segment rule buys no recall and is
# the forbidden generic-entropy matcher arriving by a side door.
#
# NOT here, deliberately: the Google refresh-token `1//` shape. Unanchored
# it hits 160 of the 250 corpus fixture files, every one a coincidence mid-SSE-base64;
# anchored it hits zero. No demonstrated positive, a known 160-file false
# positive mode.
# The JSON-escape alternatives (`\\[bfnrt]` and the `\\uXXXX` form) are
# LOAD-BEARING, not defensive. A captured body is
# single-line JSON, so an embedded newline is the two BYTES `\` + `n`. `\` is
# outside the token class but `n` is inside it, so without this alternative a
# credential that BEGINS an escaped line reads as a continuation of the
# preceding `...n` and no boundary is ever presented -- measured: a bare
# `AIza`-shaped token on its own line of a chat message passed all five rules
# at exit 0 while the same token after `=` was refused, and 554 of the 250
# corpus files carry `\n` immediately followed by a token character. Adding the
# escapes closes all five classes at zero measured false-positive cost across
# that corpus and the whole accept set.
ANCHOR_LEFT='(^|[^A-Za-z0-9_-]|\\[bfnrt]|\\u[0-9a-fA-F]{4})'

# TWO alternatives, not one widened class. The modern form is a single
# opaque run; the LEGACY form is `ya29.<1-3 digits>.<opaque>` and its short
# middle segment leaves under the floor's worth of characters before the
# second dot, so a single-run rule declines it. Admitting `.` into the body
# class generally was measured to be WRONG: it refused
# `ya29.oauth-token-refresh.flow.docs` and `ya29.a.b.c.d.e.f.g.h.i.j.k`,
# which are prose, and a gate with false positives is a gate somebody
# switches off. The legacy segment is spelled explicitly instead.
GOOGLE_OAUTH_TOKEN_RE="$ANCHOR_LEFT"'ya29\.([0-9]{1,3}\.)?[A-Za-z0-9_-]{20,}'
GOOGLE_API_KEY_RE="$ANCHOR_LEFT"'AIza[0-9A-Za-z_-]{35}'
JWT_RE="$ANCHOR_LEFT"'eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}'
AWS_TEMP_KEY_ID_RE="$ANCHOR_LEFT"'ASIA[A-Z0-9]{16}'
NVIDIA_API_KEY_RE="$ANCHOR_LEFT"'nvapi-[A-Za-z0-9_-]{20,}'

# Bedrock's short-term API key. Prefix-keyed like its siblings.
BEDROCK_API_KEY_RE="$ANCHOR_LEFT"'bedrock-api-key-[A-Za-z0-9_=&-]{20,}'

# The two AWS credentials that have NO prefix of their own: the secret access
# key is 40 opaque base64 characters and the session token is a long opaque
# run, so neither can be keyed on its value the way every rule above is.
# They are keyed on the ASSIGNMENT instead -- the variable NAME followed by
# `=` or `:` and then an opaque run -- which is the shape they actually arrive
# in: a `printenv` dump or an `~/.aws/credentials` excerpt echoed into a body.
# That operator is what buys the specificity, so this stays a shape rule and
# not the generic entropy matcher this gate refuses to become: the paired
# accept controls cover every `env://AWS_SECRET_ACCESS_KEY` config spelling
# and the "variable is unset" prose, which carry the name but no value.
AWS_CRED_ASSIGN_RE='(AWS_SECRET_ACCESS_KEY|aws_secret_access_key|AWS_SESSION_TOKEN|aws_session_token)[[:space:]]*[=:][[:space:]]*[A-Za-z0-9/+=_-]{20,}'

has_google_oauth_token() {
  grep -qE "$GOOGLE_OAUTH_TOKEN_RE" "$1"
}

has_google_api_key() {
  grep -qE "$GOOGLE_API_KEY_RE" "$1"
}

has_jwt() {
  grep -qE "$JWT_RE" "$1"
}

has_aws_temp_key_id() {
  grep -qE "$AWS_TEMP_KEY_ID_RE" "$1"
}

has_nvidia_api_key() {
  grep -qE "$NVIDIA_API_KEY_RE" "$1"
}

has_bedrock_api_key() {
  grep -qE "$BEDROCK_API_KEY_RE" "$1"
}

has_aws_cred_assignment() {
  grep -qE "$AWS_CRED_ASSIGN_RE" "$1"
}

# --- provider shape coverage -----------------------------------------
# The map from a provider KIND to the credential shapes this gate can
# detect for it. Declared as a closed, sentinel-delimited array literal
# because `--lane-known` reads it as data while a Rust drift detector
# parses the same block as TEXT without executing this script. Same
# posture as CRED_SUBSTRINGS above.
#
# The kind vocabulary is the lane token set `normalize_lane` emits in
# scripts/capture_fixtures.sh, which is `ProviderEntry::kind_str()`.
# Classification is THREE-state, mirroring
# crates/routectl-cli/src/commands/provider_env.rs: a kind is in the
# table, or explicitly excluded with a written reason, or unclassified --
# and unclassified is a test failure. Two states would let a kind whose
# real secret is undetectable ship as covered.
#
# --- BEGIN PROVIDER_SHAPE_KINDS ---
# <kind_str token>=<rule-id>[,<rule-id>...]   rule ids are the vendor prefix
# tokens each rule keys on, spelled so they appear VERBATIM in that rule's regex.
PROVIDER_SHAPE_KINDS=(
  "anthropic-api=sk-ant-api03,sk-ant-oat01,sk-ant-ort01"
  "openai-compat=sk-proj,sk-or-v1,nvapi"
  "openai-responses=sk-proj"
  "gemini=ya29,AIza"
  "bedrock=bedrock-api-key,AWS_SECRET_ACCESS_KEY,AWS_SESSION_TOKEN"
)
# Kinds with NO prefix-detectable credential shape, reason recorded per entry.
PROVIDER_SHAPE_EXCLUDED=(
  # EMPTY, and that is a verdict rather than an oversight: every provider kind
  # this build can name now has at least one body-layer credential shape the
  # gate detects. bedrock was excluded here until its two prefix-less
  # credentials (the secret access key, the session token) were keyed on their
  # ASSIGNMENT rather than their value -- the exclusion's old reason cited the
  # HEADER layer, which only covers credentials routectl itself puts on the
  # wire and says nothing about a credential arriving in a captured BODY.
  # A kind belongs here only when no name-keyed and no prefix-keyed rule can
  # see its credential, and the reason must name the layer it was checked at.
)
# --- END PROVIDER_SHAPE_KINDS ---
#
# PROVIDER_SHAPE_EXCLUDED stays declared with its meaning documented even
# if it ever empties -- same posture as gated_lanes.txt. An absent list
# reads as "nothing to exclude"; an empty one reads as "we looked".

# --- lane-known mode -------------------------------------------------
# Answers ONE question off the table above: does the gate hold a
# credential-shape classification for this lane?
#
# An EXCLUDED lane answers 0 (classified). Exclusion is a deliberate
# verdict with a written reason recorded beside the entry -- somebody
# looked at bedrock's credentials, found 40 prefix-less base64 characters
# no prefix rule can see, and wrote down why, with the header layer
# covering its SigV4 values. That is knowledge, not ignorance. Exit 1 is
# reserved for the lane nobody has ever classified: an unclassified lane
# means the gate cannot say whether a fixture on it would leak, and that
# is the state a driver fixture may not promote through.
lane_is_known() {
  local lane="$1" row
  for row in "${PROVIDER_SHAPE_KINDS[@]}"; do
    [ "${row%%=*}" = "$lane" ] && return 0
  done
  for row in "${PROVIDER_SHAPE_EXCLUDED[@]}"; do
    [ "$row" = "$lane" ] && return 0
  done
  return 1
}

run_lane_known() {
  local lane="$1"
  if lane_is_known "$lane"; then
    # SILENT on success. The rig calls this per promotion, and a success line
    # on stdout would interleave with its `captured=N` line -- the one piece
    # of rig stdout a human reads. The exit code is the whole contract.
    return 0
  fi
  echo "scrub-fixture: lane '$lane' has NO credential-shape classification" >&2
  echo "scrub-fixture: add it to PROVIDER_SHAPE_KINDS, or to PROVIDER_SHAPE_EXCLUDED" >&2
  echo "scrub-fixture: with the reason it has no prefix-detectable shape." >&2
  return 1
}

# Credential-shape rules for a header NAME, mirroring is_redact_header /
# header_name_looks_credential in crates/routectl-core/src/log_safe.rs.
# Shared verbatim by the check and write paths so a fixture the writer
# redacted is exactly the set the checker accepts.
#
# This set is a REPLICA of a Rust one, so it drifts silently: three names
# (`set-cookie`, `cookie`, `x-routectl-mitm-proxied`) were already in the
# Rust REDACT_HEADER_NAMES and absent here, and a session cookie is a
# replayable credential. `redact_cred_substrings_cover_rust_names` in
# crates/routectl-core/src/log_safe.rs now asserts every Rust entry is
# covered by the set below, so the next addition on either side fails a
# test instead of opening a hole. Keep the two in step.
read -r -d '' HEADER_RULES_PY <<'PY' || true
CRED_SUBSTRINGS = (
    "authorization", "authentication", "api-key", "apikey", "api_key",
    "token", "secret", "bearer", "jwt", "assertion",
    "session-key", "session_key", "sessionkey",
    # `cookie` subsumes `set-cookie`; both carry session credentials.
    "cookie",
    # The MITM front-proxy seam nonce: not a credential, but an
    # unguessable value whose whole purpose is staying unobservable.
    "mitm-proxied",
)
VISIBLE = {
    "x-amz-date", "x-amz-content-sha256",
    "x-ratelimit-limit-tokens", "x-ratelimit-remaining-tokens",
    "x-ratelimit-reset-tokens",
}

def is_secret_name(name):
    lc = name.lower()
    if lc in VISIBLE:
        return False
    if lc.startswith("x-amz-"):
        return True
    if any(s in lc for s in CRED_SUBSTRINGS):
        return True
    return lc.endswith("-key")

# Header names that are not credentials but identify the operator's cloud
# ACCOUNT or SEAT. Keyed on the name, never on the value: every one of these
# carries a plain uuid, and a value-shaped rule would refuse every
# legitimate uuid in a captured body. One class name per group, because the
# class name is the entire diagnostic a refusal gets.
ACCOUNT_SCOPED_NAMES = {
    "anthropic-organization-id": "anthropic-account-id",
    "anthropic-workspace-id": "anthropic-account-id",
    "x-claude-code-session-id": "claude-session-header",
    "chatgpt-account-id": "chatgpt-account-id",
}

def account_scoped_class(name):
    return ACCOUNT_SCOPED_NAMES.get(name.lower())

def needs_redaction(name):
    return is_secret_name(name) or account_scoped_class(name) is not None

def load_pairs(path):
    import json
    with open(path, encoding="utf-8") as fh:
        doc = json.load(fh)
    return doc
PY

# Exit 0 clean, 3 an unredacted value survives, 4 unparseable. On 3 the
# CLASS NAMES found are printed one per line -- never the values -- so one
# parse of the file reports every header-layer class at once.
headers_auth_state() {
  local rc=0
  python3 - "$1" "$REDACTED_BEARER" "$REDACTED_SECRET" <<PY || rc=$?
import sys
$HEADER_RULES_PY

path, redacted_bearer, redacted_secret = sys.argv[1], sys.argv[2], sys.argv[3]
accepted = {redacted_bearer, redacted_secret}
try:
    doc = load_pairs(path)
except Exception:
    sys.exit(4)
if not isinstance(doc, list):
    sys.exit(4)
found = []
for entry in doc:
    if not isinstance(entry, list) or len(entry) < 2:
        continue
    name, value = entry[0], entry[1]
    if not isinstance(name, str):
        continue
    if not needs_redaction(name):
        continue
    if isinstance(value, str) and value in accepted:
        continue
    cls = account_scoped_class(name) if not is_secret_name(name) else "auth-header"
    if cls not in found:
        found.append(cls)
if found:
    for cls in found:
        print(cls)
    sys.exit(3)
sys.exit(0)
PY
  return $rc
}

# --- check mode ------------------------------------------------------
run_check() {
  local findings="" f i class value flags
  for f in "${FILES[@]}"; do
    [ -r "$f" ] || fatal "unreadable file: $f"
    i=0
    while [ "$i" -lt "${#DENY_CLASS[@]}" ]; do
      class="${DENY_CLASS[$i]}"
      value="${DENY_VALUE[$i]}"
      flags="${DENY_FLAGS[$i]}"
      local -a gflags=(-q -F)
      case "$flags" in *w*) gflags+=(-w) ;; esac
      case "$flags" in *i*) gflags+=(-i) ;; esac
      if grep "${gflags[@]}" -e "$value" "$f"; then
        findings+="  $f  $class"$'\n'
      fi
      i=$((i + 1))
    done
    has_home_prefix "$f" && findings+="  $f  home-prefix"$'\n'
    has_home_prefix_encoded "$f" && findings+="  $f  home-prefix-encoded"$'\n'
    has_ls_owner_column "$f" && findings+="  $f  ls-owner-column"$'\n'
    has_bearer_token "$f" && findings+="  $f  bearer-token"$'\n'
    has_provider_key "$f" && findings+="  $f  provider-key"$'\n'
    has_google_oauth_token "$f" && findings+="  $f  google-oauth-token"$'\n'
    has_google_api_key "$f" && findings+="  $f  google-api-key"$'\n'
    has_jwt "$f" && findings+="  $f  jwt"$'\n'
    has_aws_temp_key_id "$f" && findings+="  $f  aws-temp-key-id"$'\n'
    has_nvidia_api_key "$f" && findings+="  $f  nvidia-api-key"$'\n'
    has_bedrock_api_key "$f" && findings+="  $f  bedrock-api-key"$'\n'
    has_aws_cred_assignment "$f" && findings+="  $f  aws-credential-assignment"$'\n'
    case "$f" in
      *.headers.json)
        local hrc=0 hclasses hclass
        hclasses="$(headers_auth_state "$f")" || hrc=$?
        case "$hrc" in
          0) : ;;
          3)
            while IFS= read -r hclass; do
              [ -n "$hclass" ] || continue
              findings+="  $f  $hclass"$'\n'
            done <<<"$hclasses"
            ;;
          4) findings+="  $f  headers-unparseable"$'\n' ;;
          *) fatal "header inspection failed on $f" ;;
        esac
        ;;
    esac
  done

  if [ -n "$findings" ]; then
    echo "scrub-fixture: FAIL residual personal data (class shown, value withheld):" >&2
    printf '%s' "$findings" >&2
    echo "scrub-fixture: this fixture must not be promoted. Remove the content by" >&2
    echo "scrub-fixture: hand or recapture the request without it -- there is no" >&2
    echo "scrub-fixture: automatic rewrite for these classes on purpose." >&2
    return 1
  fi
  echo "scrub-fixture: PASS ${#FILES[@]} file(s) clean"
  return 0
}

# --- write mode ------------------------------------------------------
# Rewrite the operator's home path to the neutral placeholder, in both the
# literal `/home/x/...` form and the dash-encoded `-home-x-...` form that
# appears in `.claude/projects/` directory names echoed by tool output.
# Behavior preserved from the capture rig this moved out of: `#` as the sed
# delimiter, BRE metacharacters and the delimiter escaped on the match side
# (the replacement sides are fixed literals with no `&` or backslash), and
# the encoded form substituted BEFORE the plain one.
rewrite_home() {
  local file="$1" home enc home_re enc_re
  home="${HOME:-}"
  home="${home%/}"
  [ -n "$home" ] || return 0
  [ "$home" = "$PLACEHOLDER_HOME" ] && return 0
  enc="${home//\//-}"
  home_re=$(printf '%s' "$home" | sed 's/[]\\.*^$[#]/\\&/g')
  enc_re=$(printf '%s' "$enc" | sed 's/[]\\.*^$[#]/\\&/g')
  sed -i \
    -e "s#${enc_re}#${PLACEHOLDER_HOME_ENC}#g" \
    -e "s#${home_re}#${PLACEHOLDER_HOME}#g" \
    "$file"
}

# Replace the VALUE of every credential-shaped and every account-scoped
# header with a placeholder, keeping the NAME. Parsed and re-emitted with a
# real JSON parser: a regex rewrite of a JSON document cannot tell a
# malformed input from a clean one, and an unparseable headers file here is
# a hard failure rather than a silently-unredacted promotion.
redact_headers_file() {
  local file="$1" rc=0
  python3 - "$file" "$REDACTED_BEARER" "$REDACTED_SECRET" <<PY || rc=$?
import json, sys
$HEADER_RULES_PY

path, redacted_bearer, redacted_secret = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    doc = load_pairs(path)
except Exception:
    sys.exit(4)
if not isinstance(doc, list):
    sys.exit(4)
for entry in doc:
    if not isinstance(entry, list) or len(entry) < 2:
        continue
    name, value = entry[0], entry[1]
    if not isinstance(name, str) or not needs_redaction(name):
        continue
    if isinstance(value, str) and value.lstrip()[:7].lower() == "bearer ":
        entry[1] = redacted_bearer
    else:
        entry[1] = redacted_secret
with open(path, "w", encoding="utf-8") as fh:
    fh.write(json.dumps(doc, separators=(",", ":")))
    fh.write("\n")
PY
  if [ "$rc" -ne 0 ]; then
    fatal "not valid header JSON, refusing to promote unredacted: $file"
  fi
}

run_write() {
  local f
  for f in "${FILES[@]}"; do
    [ -w "$f" ] || fatal "not writable: $f"
    case "$f" in
      *.headers.json) redact_headers_file "$f" ;;
    esac
    rewrite_home "$f"
  done
  echo "scrub-fixture: scrubbed ${#FILES[@]} file(s)"
}

case "$MODE" in
  check) run_check ;;
  write) run_write ;;
  lane-known) run_lane_known "$LANE_QUERY" ;;
esac
