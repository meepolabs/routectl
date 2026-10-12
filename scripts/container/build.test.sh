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
for pin in claude codex pi hermes; do
    if ! grep -qF -- "$(pin_of "$pin" "$DEFAULT_OUT")" <<<"$BASE_TAG"; then
        check "the derived tag carries the $pin pin" "present" "absent"
    else
        echo "PASS: the derived tag carries the $pin pin"
    fi
done

# --- any one pin changes the tag ------------------------------------
HERMES_COMMIT_DEFAULT="$(pin_of hermes-commit "$DEFAULT_OUT")"
for row in \
    "claude|--claude-version 9.9.9" \
    "codex|--codex-version 9.9.9" \
    "pi|--pi-version 9.9.9" \
    "hermes|--hermes-version v9.9.9 --hermes-commit $HERMES_COMMIT_DEFAULT"; do
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
