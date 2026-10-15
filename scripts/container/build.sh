#!/usr/bin/env bash
# Build the capture-cell image LOCALLY. No registry push, no tag contract,
# no published artifact -- a published image is a milestone non-goal, and the
# image is reproducible from the committed Dockerfile alone.
#
# This script BUILDS the image and nothing else. It does not run a capture:
# the run wrapper is a separate script with its own argument-refusal surface,
# and keeping the two apart is what stops build-time convenience from growing
# a run-time flag.
#
# The image is a thin client-installer plus runtime deps. It carries no
# routectl binary, no Rust toolchain, no repo copy, and no credential -- all
# of those arrive at run time as bind-mounts or environment.
#
# Usage:
#   build.sh [--claude-version <v>] [--codex-version <v>]
#            [--hermes-version <tag> --hermes-commit <sha>] [--tag <image-tag>]
#            [--dry-run]
#
#   --claude-version, --codex-version, --hermes-version
#               Per-client pin, passed as that client's build ARG. Each
#               defaults to the Dockerfile's own committed ARG default, which
#               is CONVENIENCE ONLY: the sole record of what a fixture was
#               captured with is `meta.client.version` read off the RUNNING
#               binary.
#               pi has no flag: `npm ci` installs whatever the committed
#               pi/package-lock.json resolves, so its pin is read from that
#               lock and passed to the build only for the image to check.
#   --hermes-commit
#               The commit the hermes release tag must resolve to. Required
#               with --hermes-version: the image checks the tag against it,
#               so a tag override alone could only fail the build.
#   --tag       Full image tag. Defaults to one derived from all four client
#               pins and the hermes commit, so a change to any pin builds a
#               different tag -- never a bare `:latest`, which would let a
#               rebuild at another client version silently replace the
#               image a fixture was captured under. A build at the committed defaults with no
#               --tag also tags `routectl-capture:default`, the image the
#               run wrapper and the self-tests use when given none.
#   --dry-run   Print the resolved pins, the tag(s), and the build command,
#               then exit 0 without building. Needs no docker.
#
# After a build it reports the version every client prints in the finished
# image, run as an unprivileged uid.
#
# Exit codes: 0 = image built (or dry run printed), 1 = build failed,
# 2 = usage, 3 = docker missing.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOCKERFILE="$SCRIPT_DIR/Dockerfile"
PI_LOCK="$SCRIPT_DIR/pi/package-lock.json"

IMAGE_REPO=routectl-capture
DEFAULT_TAG="$IMAGE_REPO:default"

# Run-time uid for the version report: the capture runs unprivileged, and
# root ignores the mode bits that decide whether a client is executable.
REPORT_USER="65534:65534"

CLIENTS="claude codex pi hermes"

usage() {
    sed -n '2,47p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
}

die_usage() {
    echo "build: $1" >&2
    exit 2
}

# The committed default for one ARG, read out of the Dockerfile so the pins
# have exactly one owner.
dockerfile_default() {
    local value
    value="$(sed -n "s/^ARG $1=//p" "$DOCKERFILE" | head -n 1)"
    if [ -z "$value" ]; then
        echo "build: the Dockerfile declares no default for ARG $1" >&2
        exit 1
    fi
    printf '%s\n' "$value"
}

# The pi version the committed lock resolves. Fails the build when the lock
# cannot be read, rather than tagging an image with a version it may not hold.
lock_pi_version() {
    local value
    value="$(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["packages"]["node_modules/@earendil-works/pi-coding-agent"]["version"])' "$PI_LOCK" 2>/dev/null)" || value=""
    if [ -z "$value" ]; then
        echo "build: cannot read the pi version from $PI_LOCK" >&2
        exit 1
    fi
    printf '%s\n' "$value"
}

# A pin becomes part of a docker tag, so it is held to a charset that is
# valid inside one and that excludes `_`, the separator the derived tag
# joins pins with -- which keeps the pin-to-tag mapping one-to-one.
valid_pin() {
    [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9.-]*$ ]]
}

CLAUDE_VERSION=""
CODEX_VERSION=""
HERMES_VERSION=""
HERMES_COMMIT=""
TAG=""
DRY_RUN=0

take_value() {
    [ "$2" -ge 2 ] && [ -n "$3" ] || die_usage "$1 needs a non-empty value"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --claude-version)
            take_value "$1" $# "${2:-}"; CLAUDE_VERSION="$2"; shift 2 ;;
        --codex-version)
            take_value "$1" $# "${2:-}"; CODEX_VERSION="$2"; shift 2 ;;
        --hermes-version)
            take_value "$1" $# "${2:-}"; HERMES_VERSION="$2"; shift 2 ;;
        --hermes-commit)
            take_value "$1" $# "${2:-}"; HERMES_COMMIT="$2"; shift 2 ;;
        --tag)
            take_value "$1" $# "${2:-}"; TAG="$2"; shift 2 ;;
        --dry-run)
            DRY_RUN=1; shift ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "build: unknown argument: $1" >&2
            usage
            exit 2
            ;;
    esac
done

if [ -n "$HERMES_VERSION" ] && [ -z "$HERMES_COMMIT" ]; then
    die_usage "--hermes-version needs --hermes-commit (the commit the tag must resolve to)"
fi
if [ -n "$HERMES_COMMIT" ] && [ -z "$HERMES_VERSION" ]; then
    die_usage "--hermes-commit needs --hermes-version"
fi
if [ -n "$HERMES_COMMIT" ] && ! [[ "$HERMES_COMMIT" =~ ^[0-9a-f]{40}$ ]]; then
    die_usage "--hermes-commit must be a full 40-character lowercase hex commit"
fi

is_default=1
[ -n "$CLAUDE_VERSION$CODEX_VERSION$HERMES_VERSION" ] && is_default=0

CLAUDE_VERSION="${CLAUDE_VERSION:-$(dockerfile_default CLAUDE_VERSION)}"
CODEX_VERSION="${CODEX_VERSION:-$(dockerfile_default CODEX_VERSION)}"
PI_VERSION="$(lock_pi_version)"
HERMES_VERSION="${HERMES_VERSION:-$(dockerfile_default HERMES_VERSION)}"
HERMES_COMMIT="${HERMES_COMMIT:-$(dockerfile_default HERMES_COMMIT)}"

# An explicit pin equal to the committed default is still the default build.
if [ "$is_default" -eq 0 ] \
    && [ "$CLAUDE_VERSION" = "$(dockerfile_default CLAUDE_VERSION)" ] \
    && [ "$CODEX_VERSION" = "$(dockerfile_default CODEX_VERSION)" ] \
    && [ "$HERMES_VERSION" = "$(dockerfile_default HERMES_VERSION)" ] \
    && [ "$HERMES_COMMIT" = "$(dockerfile_default HERMES_COMMIT)" ]; then
    is_default=1
fi

for pin in "claude:$CLAUDE_VERSION" "codex:$CODEX_VERSION" "pi:$PI_VERSION" "hermes:$HERMES_VERSION"; do
    valid_pin "${pin#*:}" || die_usage "${pin%%:*} version '${pin#*:}' is not a valid pin (letters, digits, '.', '-'; must start alphanumeric)"
done

TAGS=()
if [ -n "$TAG" ]; then
    case "$TAG" in
        *:latest|latest) die_usage "--tag must not be ':latest'" ;;
        *:*) ;;
        *) die_usage "--tag must name a tag explicitly (<repo>:<tag>)" ;;
    esac
    TAGS+=("$TAG")
else
    # The hermes commit is its own `_` component: a version may carry `-`,
    # so joining it to the version would make the split ambiguous.
    derived="claude${CLAUDE_VERSION}_codex${CODEX_VERSION}_pi${PI_VERSION}_hermes${HERMES_VERSION}_commit${HERMES_COMMIT}"
    # Docker's own limit on the tag component.
    [ "${#derived}" -le 128 ] || die_usage "the derived tag is longer than 128 characters; pass --tag"
    TAGS+=("$IMAGE_REPO:$derived")
    if [ "$is_default" -eq 1 ]; then
        TAGS+=("$DEFAULT_TAG")
    fi
fi

# The build context is scripts/container/ itself, not the repo root. The
# Dockerfile copies only pi's committed manifest and lock from it, so a wider
# context would only offer a future author the chance to bake a host artifact
# into a layer -- an image built from a host copy is unauditable,
# unreproducible, and unavailable to a contributor who has no such host
# artifact.
build_cmd=(docker build -f "$DOCKERFILE")
for t in "${TAGS[@]}"; do
    build_cmd+=(-t "$t")
done
build_cmd+=(
    --build-arg "CLAUDE_VERSION=$CLAUDE_VERSION"
    --build-arg "CODEX_VERSION=$CODEX_VERSION"
    --build-arg "PI_VERSION=$PI_VERSION"
    --build-arg "HERMES_VERSION=$HERMES_VERSION"
    --build-arg "HERMES_COMMIT=$HERMES_COMMIT"
    --label "routectl.capture.claude-version=$CLAUDE_VERSION"
    --label "routectl.capture.codex-version=$CODEX_VERSION"
    --label "routectl.capture.pi-version=$PI_VERSION"
    --label "routectl.capture.hermes-version=$HERMES_VERSION"
    --label "routectl.capture.hermes-commit=$HERMES_COMMIT"
    "$SCRIPT_DIR"
)

if [ "$DRY_RUN" -eq 1 ]; then
    printf 'pin claude %s\n' "$CLAUDE_VERSION"
    printf 'pin codex %s\n' "$CODEX_VERSION"
    printf 'pin pi %s\n' "$PI_VERSION"
    printf 'pin hermes %s\n' "$HERMES_VERSION"
    printf 'pin hermes-commit %s\n' "$HERMES_COMMIT"
    for t in "${TAGS[@]}"; do
        printf 'tag %s\n' "$t"
    done
    printf 'command'
    printf ' %q' "${build_cmd[@]}"
    printf '\n'
    exit 0
fi

if ! command -v docker >/dev/null 2>&1; then
    echo "build: docker is not installed or not on PATH" >&2
    exit 3
fi

echo "build: building ${TAGS[*]}" >&2
"${build_cmd[@]}" || exit 1

# Report what the image ACTUALLY carries, read off each running binary rather
# than echoed back from the ARG. A mismatch between a requested pin and the
# observed version is information, not an error -- the observed value is the
# one a fixture records. Only the first line is kept: hermes prints install
# details after its version line.
echo "build: built ${TAGS[*]}" >&2
for client in $CLIENTS; do
    observed="$(docker run --rm --user "$REPORT_USER" "${TAGS[0]}" "$client" --version 2>/dev/null | head -n 1)" || true
    echo "build: $client reports: ${observed:-<no version output>}" >&2
done
