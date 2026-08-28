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
# WHY IT RE-RUNS THE SCRUB GATE. The rig already ran
# `scrub-fixture.sh --check` when it captured, but a scratch fixture is
# by design hand-inspectable and hand-editable between capture and
# promotion -- that is what the scratch root is FOR. This re-check is the
# only thing standing between an edited scratch fixture and the committed
# corpus, so a non-zero `--check` is a REFUSAL that promotes nothing and
# leaves the destination exactly as it was.
#
# The check runs on the STAGED COPY, before any rename: what gets scanned
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
#   1  the staged content failed `scrub-fixture.sh --check` -- residual
#      personal data. Nothing was promoted.
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
