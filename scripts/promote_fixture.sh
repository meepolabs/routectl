#!/usr/bin/env bash
# Promote ONE captured fixture from a writable scratch root into the
# committed driver corpus.
#
#   scripts/promote_fixture.sh --from <fixture-dir> \
#                             --scratch-root <dir> \
#                             [--to <corpus-root>]
#
#   --from          the fixture directory to promote. Must live exactly
#                   two components under --scratch-root, i.e.
#                   `<scratch-root>/<lane>/<case-id>`, because that pair
#                   IS the corpus key the capture rig lands on.
#   --scratch-root  the confinement root --from must live under. Required:
#                   the scratch tree is outside the repo (the capture
#                   container mounts the repo read-only), so there is no
#                   repo-relative constant to derive it from, and a root
#                   guessed from --from would confine nothing.
#   --to            the corpus root to promote INTO. Defaults to
#                   `crates/routectl-cli/tests/fixtures/driver` and is
#                   itself confined to that default, so this flag can
#                   narrow the destination but never leave the corpus.
#
# WHY THIS IS A SCRIPT AND NOT A DOCUMENTED `mv`. A `mv` of one fixture
# directory over an existing one MERGES: files the previous capture wrote
# survive next to the new ones. File presence IS part of the fixture
# schema -- an `upstream_response.json` present means the run was
# non-stream, absent means it streamed -- so a merged directory is a
# fixture no single capture ever produced, and every drift signal read
# off it is read off evidence that does not exist. Promotion therefore
# copies into a staging directory beside the destination and lands it by
# RENAME-ASIDE-THEN-DELETE, the same idiom the rig uses for a case rerun:
# the old fixture is renamed out of the way, the new one is renamed into
# place, and only then is the old one deleted. A reader sees the whole
# old fixture or the whole new one, never a union of both.
#
# WHY IT RE-RUNS THE LANDING GATES. The rig already ran
# `scrub-fixture.sh --check`, the wire-pattern predicate, and the
# mode/seam coherence check when it captured, but a scratch fixture is
# by design hand-inspectable and hand-editable between capture and
# promotion -- that is what the scratch root is FOR. These re-checks are
# the only thing standing between an edited scratch fixture and the
# committed corpus, so a non-zero verdict from any of them is a REFUSAL
# that promotes nothing and leaves the destination exactly as it was.
#
# The claims are read from the STAGED `meta.json`, which is where the rig
# recorded them and the only on-disk statement of what the fixture claims
# to be. An edit that flips `wire_pattern` or `client.connection_mode` is
# exactly what these gates exist to catch, so an unreadable meta.json is a
# refusal rather than a skip.
#
# The checks run on the STAGED COPY, before any rename: what gets scanned
# is byte-for-byte what would land, and a refusal has nothing to undo.
#
# Path confinement is NOT implemented here. Both paths go through
# `scripts/drivers/lib/confine.sh`, which owns the one copy of the
# resolution pair and the per-component symlink walk; its refusal
# messages name `--out`, the flag its first caller used, because the
# library is shared rather than duplicated per script.
#
# Exit codes:
#   0  promoted; the destination now holds exactly the source's file set
#   1  the staged content failed a landing gate -- residual personal data,
#      a body that does not exhibit the wire pattern its `meta.json`
#      claims, a connection-mode claim outside the two the corpus holds,
#      or captured ingress headers that contradict its recorded
#      connection mode. Nothing was promoted.
#   2  usage error, a confinement refusal, a fixture path that is not
#      `<scratch-root>/<lane>/<case-id>`, or a missing prerequisite
#      (this is also what a scrub gate that could not run at all reports)
#
# Run it from anywhere:
#   bash scripts/promote_fixture.sh --from ... --scratch-root ...
# --- END USAGE ---

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Print the header block as usage. Sentinel-delimited rather than a line
# range, so the block cannot start silently truncating as it grows.
usage() {
  sed -n '2,/^# --- END USAGE ---$/p' "$0" | sed '$d'
}

fatal() {
  echo "promote_fixture: $1" >&2
  exit 2
}

# The single owner of path confinement, shared with the capture rig.
# Absent library is a hard failure, never an unconfined promotion: this
# script deletes directories, so an unguarded path is the worst failure
# it has.
CONFINE_LIB="$ROOT/scripts/drivers/lib/confine.sh"
[ -r "$CONFINE_LIB" ] ||
  fatal "confinement library not found at $CONFINE_LIB; refusing to promote"
# shellcheck source=scripts/drivers/lib/confine.sh
. "$CONFINE_LIB"

# The single owner of credential and personal-data vocabulary.
SCRUB="$ROOT/scripts/scrub-fixture.sh"
[ -r "$SCRUB" ] ||
  fatal "scrub gate not found at $SCRUB; refusing to promote"

# The single owner of the wire-pattern predicates. Absent is a hard
# failure, never an unverified promotion -- the same fail-closed shape the
# capture rig uses.
VERIFY_PATTERN="$ROOT/scripts/drivers/lib/verify_pattern.py"
[ -r "$VERIFY_PATTERN" ] ||
  fatal "wire-pattern predicate not found at $VERIFY_PATTERN; refusing to promote"

# Name of the MITM front-proxy seam header, as spelled in
# REDACT_HEADER_NAMES in crates/routectl-core/src/log_safe.rs -- which is
# why a captured ingress header set retains the NAME while its value is
# redacted.
MITM_SEAM_HEADER="x-routectl-mitm-proxied"

DEFAULT_CORPUS="$ROOT/crates/routectl-cli/tests/fixtures/driver"

SRC=""
SCRATCH_ROOT=""
CORPUS="$DEFAULT_CORPUS"

while [ $# -gt 0 ]; do
  case "$1" in
    --from) [ $# -ge 2 ] || fatal "--from requires a value"; SRC="$2"; shift 2 ;;
    --scratch-root)
      [ $# -ge 2 ] || fatal "--scratch-root requires a value"
      SCRATCH_ROOT="$2"; shift 2 ;;
    --to) [ $# -ge 2 ] || fatal "--to requires a value"; CORPUS="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) fatal "unknown arg: $1" ;;
  esac
done

[ -n "$SRC" ] || fatal "--from is required (see --help)"
[ -n "$SCRATCH_ROOT" ] || fatal "--scratch-root is required (see --help)"
[ -n "$CORPUS" ] || fatal "--to cannot be empty"

SRC="$(abspath_lexical "$SRC")"
SCRATCH_ROOT="$(abspath_lexical "$SCRATCH_ROOT")"
CORPUS="$(abspath_lexical "$CORPUS")"

# Every path below is quoted at every use. `$(...)` strips trailing
# newlines, so an unquoted substitution used as a path can word-split
# into arguments this script would then hand to `rm -rf`.
confine_out_under "$SRC" "$SCRATCH_ROOT"
confine_out_under "$CORPUS" "$DEFAULT_CORPUS"

# The confinement above compares PHYSICALLY resolved paths, so it accepts
# a --from and a --scratch-root spelled through different symlinked
# ancestors. The relative fixture key below is computed LEXICALLY, so
# refuse that case rather than derive a key from a prefix that does not
# match: an empty or wrong key is how a rename-aside ends up aimed at the
# corpus root instead of one fixture.
case "$SRC" in
  "$SCRATCH_ROOT"/*) : ;;
  *)
    fatal "refusing --from '$SRC': it is not spelled as a path under --scratch-root '$SCRATCH_ROOT'"
    ;;
esac
REL="${SRC#"$SCRATCH_ROOT"/}"

# A fixture is keyed `(lane, case_id)`, so its path under either root is
# exactly two components. Refusing anything else is not tidiness: the
# loader mistakes a lane DIRECTORY handed to it as a fixture for a
# fixture, and a REL that collapsed to one component (or to nothing, when
# --from IS the scratch root) would aim the rename-aside below at a lane
# directory or at the corpus root itself.
LANE="${REL%%/*}"
CASE_ID="${REL#*/}"
case "$REL" in
  */*/*) fatal "refusing --from '$SRC': a fixture sits at '<scratch-root>/<lane>/<case-id>', this is deeper" ;;
  */*) : ;;
  *) fatal "refusing --from '$SRC': a fixture sits at '<scratch-root>/<lane>/<case-id>', this names only one component" ;;
esac
{ [ -n "$LANE" ] && [ -n "$CASE_ID" ]; } ||
  fatal "refusing --from '$SRC': it has an empty lane or case-id component"

[ -d "$SRC" ] || fatal "not a readable fixture directory: $SRC"

DST="$CORPUS/$LANE/$CASE_ID"
# The destination path is derived, so its components cannot be symlinks
# smuggled in through argv -- but an EXISTING one can be a symlink planted
# in the corpus tree, and that would redirect the rename out of the
# corpus. The library's per-component walk is what sees it.
confine_out_under "$DST" "$CORPUS"

if [ -e "$DST" ] && [ ! -d "$DST" ]; then
  fatal "refusing to promote: '$DST' exists and is not a directory"
fi

mkdir -p "$CORPUS/$LANE"

# Stage inside the corpus root so the landing rename is same-filesystem
# and therefore atomic; the scratch root is typically a separate mount,
# which is why the source is COPIED here rather than moved. `mktemp -d`
# names the staging directory, so no `rm -rf` argument below is ever
# built from caller input. The `.tmp.` prefix matches the rig's naming so
# the same sweep recognizes a crashed run's leftovers.
STAGED="$(mktemp -d "$CORPUS/.tmp.promote.XXXXXX")"
cleanup() {
  [ -n "${STAGED:-}" ] && [ -d "$STAGED" ] && rm -rf "$STAGED"
  return 0
}
trap cleanup EXIT

cp -R "$SRC/." "$STAGED/"

# The gate runs on the staged bytes, which ARE the bytes that would land.
# Its own exit code is propagated rather than flattened: 1 means the
# fixture is dirty, 2 means the gate could not run, and a caller that
# collapsed those would retry a broken gate as if it were a dirty
# fixture.
scrub_rc=0
bash "$SCRUB" --check "$STAGED" || scrub_rc=$?
if [ "$scrub_rc" -ne 0 ]; then
  echo "promote_fixture: refusing to promote '$SRC': it did not pass the scrub gate." >&2
  echo "the destination '$DST' is untouched. Fix the fixture in scratch and retry." >&2
  exit "$scrub_rc"
fi

# The two CLAIMS the staged meta.json makes about itself, read back with a
# real JSON parser: a grep would accept a value from any nesting level, and
# `connection_mode` is nested under `client`.
#
# An unreadable or malformed meta.json is a refusal at exit 2: the gates
# below cannot run, and "could not check" is never "checked and clean".
#
# The reader's exit status is captured OUTSIDE the command substitution: an
# assignment inside one runs in a subshell and its value never reaches this
# scope, which would read every failure as a clean empty claim.
claim_read_rc=0
CLAIMS="$(python3 - "$STAGED/meta.json" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        meta = json.load(handle)
    if not isinstance(meta, dict):
        raise ValueError("meta.json is not a JSON object")
    client = meta.get("client")
    print(meta.get("wire_pattern", ""))
    print(client.get("connection_mode", "") if isinstance(client, dict) else "")
except (OSError, UnicodeDecodeError, ValueError) as exc:
    print(f"promote_fixture: unreadable staged meta.json: {exc}", file=sys.stderr)
    sys.exit(2)
PY
)" || claim_read_rc=$?
if [ "$claim_read_rc" -ne 0 ]; then
  echo "promote_fixture: refusing to promote '$SRC': its claims cannot be read." >&2
  echo "the destination '$DST' is untouched." >&2
  exit 2
fi
CLAIMED_PATTERN="$(printf '%s\n' "$CLAIMS" | sed -n 1p)"
CLAIMED_MODE="$(printf '%s\n' "$CLAIMS" | sed -n 2p)"

# The wire-pattern claim, enforced against the staged bytes. An EMPTY claim
# is a live-box capture, which genuinely could not observe the pin; the
# driver corpus this script promotes into is never that, so an empty
# pattern here is a fixture nothing can gate and it does not land.
if [ -z "$CLAIMED_PATTERN" ]; then
  echo "promote_fixture: refusing to promote '$SRC': its meta.json records no wire_pattern," >&2
  echo "so there is no claim to verify. the destination '$DST' is untouched." >&2
  exit 1
fi
if ! python3 "$VERIFY_PATTERN" "$STAGED" "$CLAIMED_PATTERN"; then
  echo "promote_fixture: refusing to promote '$SRC': it does not exhibit the wire pattern" >&2
  echo "its meta.json claims ('$CLAIMED_PATTERN'). the destination '$DST' is untouched." >&2
  exit 1
fi

# The mode claim is a CLOSED SET, checked before the seam evidence is
# even read: the corpus this script promotes into is the driver one,
# where every fixture records the mode its runner pinned, so an empty or
# unrecognized mode is an edited or malformed claim. Waving it past the
# seam gate would promote exactly the hand-edited fixture these gates
# exist to catch -- the gate below has no arm for such a mode, and "no
# arm fired" is not "checked and coherent".
case "$CLAIMED_MODE" in
  front-proxy|base-url) : ;;
  "")
    echo "promote_fixture: refusing to promote '$SRC': its meta.json records no connection mode," >&2
    echo "so the seam gate has no claim to check. the destination '$DST' is untouched." >&2
    exit 1
    ;;
  *)
    echo "promote_fixture: refusing to promote '$SRC': it records unsupported connection mode" >&2
    echo "'$CLAIMED_MODE' (base-url or front-proxy). the destination '$DST' is untouched." >&2
    exit 1
    ;;
esac

# The connection-mode claim, enforced against the staged ingress headers.
# An environment carrier proves INTENT; the seam header is the only
# evidence of TRANSIT, so a fixture labelled front-proxy that never
# transited the seam is caught here rather than read as client drift later.
# Both directions, because a check that only looked at front-proxy runs
# would be satisfiable by one that never fires.
seam_rc=0
python3 - "$STAGED/ingress_request.headers.json" "$MITM_SEAM_HEADER" <<'PY' || seam_rc=$?
import json
import sys

path, needle = sys.argv[1], sys.argv[2].lower()
shape = "captured ingress headers are not a JSON array of [name, value] pairs"
try:
    with open(path, encoding="utf-8") as handle:
        pairs = json.load(handle)
except (OSError, UnicodeDecodeError, ValueError) as exc:
    print(f"promote_fixture: unreadable {path}: {exc}", file=sys.stderr)
    sys.exit(2)
if not isinstance(pairs, list):
    print(f"promote_fixture: {shape}: {path}", file=sys.stderr)
    sys.exit(2)
for pair in pairs:
    if not isinstance(pair, list) or not pair:
        print(f"promote_fixture: {shape}: {path}", file=sys.stderr)
        sys.exit(2)
    if str(pair[0]).lower() == needle:
        sys.exit(0)
sys.exit(1)
PY
case "$CLAIMED_MODE:$seam_rc" in
  front-proxy:1)
    echo "promote_fixture: refusing to promote '$SRC': it records connection mode 'front-proxy'" >&2
    echo "but its captured ingress headers carry no $MITM_SEAM_HEADER, so the run did not" >&2
    echo "transit the MITM listener. the destination '$DST' is untouched." >&2
    exit 1
    ;;
  base-url:0)
    echo "promote_fixture: refusing to promote '$SRC': it records connection mode 'base-url'" >&2
    echo "but its captured ingress headers carry $MITM_SEAM_HEADER, so the run DID transit" >&2
    echo "the MITM listener. the destination '$DST' is untouched." >&2
    exit 1
    ;;
  front-proxy:*|base-url:*)
    if [ "$seam_rc" -ne 0 ] && [ "$seam_rc" -ne 1 ]; then
      echo "promote_fixture: refusing to promote '$SRC': its captured ingress headers cannot be" >&2
      echo "read, so connection mode '$CLAIMED_MODE' is unprovable. the destination is untouched." >&2
      exit 2
    fi
    ;;
esac

# Rename-aside-then-delete. Both operands are absolute (a
# `cd <dir> && rm` compound short-circuits into a no-op when cwd already
# IS that dir, and reads as "it ran and changed nothing").
if [ -d "$DST" ]; then
  STALE="$CORPUS/.tmp.stale.$LANE.$CASE_ID.$$"
  mv "$DST" "$STALE"
  mv "$STAGED" "$DST"
  rm -rf "$STALE"
else
  mv "$STAGED" "$DST"
fi

echo "promoted $SRC -> $DST"
