#!/usr/bin/env bash
# Self-test for scripts/container/build.sh's argument surface and tag
# derivation, through its --dry-run mode. Needs NO docker: a stub `docker`
# that fails loudly is put first on PATH, so a dry run that reached docker
# turns a leg red instead of starting a multi-minute build.
#
# The derived tag is what keeps a rebuild at a different client version from
# replacing the image a fixture was captured under, so the core assertion is
# that changing ANY ONE pin changes the tag. Each pin is its own row: a
# derivation that dropped one client would pass a test that only varied
# another.
#
# Run it from anywhere:
#   bash scripts/container/build.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD="$HERE/build.sh"

fails=0

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- expected '$expected', got '$actual'"
        fails=$((fails + 1))
    fi
}

WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/bin"
printf '#!/bin/sh\necho "stub docker invoked: $*" >&2\nexit 99\n' >"$WORK/bin/docker"
chmod +x "$WORK/bin/docker"

run_build() {
    PATH="$WORK/bin:$PATH" bash "$BUILD" "$@" 2>"$WORK/stderr"
}

# The first derived tag (the pin-derived one, never the default alias).
derived_tag() {
    run_build --dry-run "$@" | awk '$1 == "tag" { print $2; exit }'
}

pin_of() {
    awk -v k="$1" '$1 == "pin" && $2 == k { print $3 }' <<<"$2"
}

# --- defaults --------------------------------------------------------
DEFAULT_OUT="$(run_build --dry-run)"
check "a default dry run exits 0 without docker" "0" "$?"

# The pins come from the Dockerfile's own ARG defaults; read them the same
# way here so a re-pin of the Dockerfile does not need an edit in this file.
for arg in CLAUDE_VERSION:claude CODEX_VERSION:codex PI_VERSION:pi HERMES_VERSION:hermes HERMES_COMMIT:hermes-commit; do
    want="$(sed -n "s/^ARG ${arg%%:*}=//p" "$HERE/Dockerfile" | head -n 1)"
    if [ -z "$want" ]; then
        check "the Dockerfile declares a default for ${arg%%:*}" "non-empty" ""
        continue
    fi
    check "the dry run reports the committed ${arg#*:} pin" "$want" "$(pin_of "${arg#*:}" "$DEFAULT_OUT")"
done

DEFAULT_TAGS="$(awk '$1 == "tag" { print $2 }' <<<"$DEFAULT_OUT")"
check "a default build also tags routectl-capture:default" \
    "1" "$(grep -cx 'routectl-capture:default' <<<"$DEFAULT_TAGS")"
check "no tag is :latest" "0" "$(grep -c ':latest$' <<<"$DEFAULT_TAGS")"

BASE_TAG="$(derived_tag)"
for pin in claude codex pi hermes hermes-commit; do
    if ! grep -qF -- "$(pin_of "$pin" "$DEFAULT_OUT")" <<<"$BASE_TAG"; then
        check "the derived tag carries the $pin pin" "present" "absent"
    else
        echo "PASS: the derived tag carries the $pin pin"
    fi
done

# --- any one pin changes the tag ------------------------------------
HERMES_COMMIT_DEFAULT="$(pin_of hermes-commit "$DEFAULT_OUT")"
# A different 40-hex commit, derived so it can never equal the default.
OTHER_COMMIT="$(tr '0-9a-f' '1-9a-f0' <<<"$HERMES_COMMIT_DEFAULT")"
for row in \
    "claude|--claude-version 9.9.9" \
    "codex|--codex-version 9.9.9" \
    "pi|--pi-version 9.9.9" \
    "hermes|--hermes-version v9.9.9 --hermes-commit $HERMES_COMMIT_DEFAULT" \
    "hermes-commit|--hermes-version $(pin_of hermes "$DEFAULT_OUT") --hermes-commit $OTHER_COMMIT"; do
    name="${row%%|*}"
    # shellcheck disable=SC2086 # the row's flags are word-split on purpose
    tag="$(derived_tag ${row#*|})"
    if [ -n "$tag" ] && [ "$tag" != "$BASE_TAG" ]; then
        echo "PASS: changing only the $name pin changes the derived tag ($tag)"
    else
        check "changing only the $name pin changes the derived tag" "a tag other than $BASE_TAG" "$tag"
    fi
    # shellcheck disable=SC2086
    check "a non-default $name pin does not tag routectl-capture:default" "0" \
        "$(run_build --dry-run ${row#*|} | grep -cx 'tag routectl-capture:default')"
done

# The paired accept direction: the same pins give the same tag, so the
# rows above are not passing on a tag that varies run to run.
check "identical pins derive an identical tag" "$BASE_TAG" "$(derived_tag)"
check "an explicit pin equal to the default is still the default build" \
    "1" "$(run_build --dry-run --pi-version "$(pin_of pi "$DEFAULT_OUT")" | grep -cx 'tag routectl-capture:default')"

# --- every pin is a build ARG ----------------------------------------
CMD_LINE="$(awk '$1 == "command"' <<<"$DEFAULT_OUT")"
for arg in CLAUDE_VERSION CODEX_VERSION PI_VERSION HERMES_VERSION HERMES_COMMIT; do
    check "the build command passes $arg" "1" "$(grep -c -- "--build-arg $arg=" <<<"$CMD_LINE")"
done

# --- every pin is a provenance label ---------------------------------
# The label records the full pin, so an image's provenance is readable off
# the image itself and not only off its tag.
for row in claude-version:claude codex-version:codex pi-version:pi \
    hermes-version:hermes hermes-commit:hermes-commit; do
    check "the build command labels the image with the ${row#*:} pin" "1" \
        "$(grep -c -- "--label routectl.capture.${row%%:*}=$(pin_of "${row#*:}" "$DEFAULT_OUT")" <<<"$CMD_LINE")"
done

# --- the pi lock and the pi pin agree --------------------------------
# pi installs from the committed lock, and PI_VERSION only checks what that
# lock installed. A lock re-generated at another version, or a package.json
# spec that is a range a relock could move, would let the two drift until a
# build failed; both are held here, without docker.
PI_DIR="$HERE/pi"
PI_PIN="$(pin_of pi "$DEFAULT_OUT")"
pi_lock_facts() {
    python3 -I - "$PI_DIR/package.json" "$PI_DIR/package-lock.json" <<'PY'
import json, sys

name = "@earendil-works/pi-coding-agent"
try:
    manifest = json.load(open(sys.argv[1], encoding="utf-8"))
    lock = json.load(open(sys.argv[2], encoding="utf-8"))
except (OSError, ValueError):
    print("spec -")
    print("locked -")
    print("unpinned -")
    raise SystemExit(0)
packages = lock.get("packages", {})
print("spec", manifest.get("dependencies", {}).get(name, "-"))
print("locked", packages.get("node_modules/" + name, {}).get("version", "-"))
unpinned = [
    path for path, entry in packages.items()
    if path and not entry.get("link") and not (entry.get("integrity") and entry.get("resolved"))
]
print("unpinned", len(unpinned) if packages else "-")
PY
}
PI_FACTS="$(pi_lock_facts)"
check "the pi package.json pins exactly the Dockerfile's PI_VERSION" \
    "$PI_PIN" "$(awk '$1 == "spec" { print $2 }' <<<"$PI_FACTS")"
check "the pi lock resolves exactly the Dockerfile's PI_VERSION" \
    "$PI_PIN" "$(awk '$1 == "locked" { print $2 }' <<<"$PI_FACTS")"
check "every package in the pi lock carries a resolved URL and an integrity" \
    "0" "$(awk '$1 == "unpinned" { print $2 }' <<<"$PI_FACTS")"

# --- usage refusals, each exit 2 -------------------------------------
for row in \
    "an unknown flag|--bogus" \
    "the removed single-client --version flag|--version 1.0.0" \
    "a flag missing its value|--pi-version" \
    "--hermes-version without --hermes-commit|--hermes-version v9.9.9" \
    "--hermes-commit without --hermes-version|--hermes-commit $HERMES_COMMIT_DEFAULT" \
    "a short hermes commit|--hermes-version v9.9.9 --hermes-commit abc123" \
    "a pin carrying the tag separator|--pi-version 1_1" \
    "a pin carrying a tag-illegal character|--codex-version 1/2" \
    "a :latest tag|--tag routectl-capture:latest" \
    "a tag with no explicit tag part|--tag routectl-capture"; do
    # shellcheck disable=SC2086
    run_build --dry-run ${row#*|} >/dev/null
    check "${row%%|*} exits 2" "2" "$?"
done

# --- the stub really would have caught a docker call -----------------
# Positive control for the no-docker claim: a non-dry run reaches the stub
# and fails, so a dry run that silently built would have been visible.
run_build >/dev/null
rc=$?
if [ "$rc" -ne 0 ] && grep -q "stub docker invoked" "$WORK/stderr"; then
    echo "PASS: a non-dry run reaches docker (the stub), so the dry-run legs above did not"
else
    check "a non-dry run reaches the docker stub" "stub invoked" "rc=$rc"
fi

echo
if [ "$fails" -eq 0 ]; then
    echo "build.sh self-test: all assertions passed"
    exit 0
fi
echo "build.sh self-test: $fails assertion(s) failed"
exit 1
