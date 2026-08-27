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
#   build.sh [--version <client-version>] [--tag <image-tag>]
#
#   --version   Client version baked in as the CLAUDE_VERSION build ARG.
#               Defaults to the Dockerfile's own committed default, which is
#               CONVENIENCE ONLY: the sole record of what a fixture was
#               captured with is `meta.client.version` read off the RUNNING
#               binary. Two versions build two tags from this one Dockerfile.
#   --tag       Full image tag. Defaults to `routectl-capture:<version>` when
#               --version is given, `routectl-capture:default` otherwise --
#               never a bare `:latest`, which would let a rebuild at a
#               different client version silently replace the image a fixture
#               was captured under.
#
# Exit codes: 0 = image built, 1 = build failed, 2 = usage, 3 = docker missing.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

IMAGE_REPO=routectl-capture

VERSION=""
TAG=""

usage() {
    sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || { echo "build: --version needs a value" >&2; exit 2; }
            VERSION="$2"
            shift 2
            ;;
        --tag)
            [ $# -ge 2 ] || { echo "build: --tag needs a value" >&2; exit 2; }
            TAG="$2"
            shift 2
            ;;
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

if ! command -v docker >/dev/null 2>&1; then
    echo "build: docker is not installed or not on PATH" >&2
    exit 3
fi

if [ -z "$TAG" ]; then
    if [ -n "$VERSION" ]; then
        TAG="$IMAGE_REPO:$VERSION"
    else
        TAG="$IMAGE_REPO:default"
    fi
fi

# The build context is scripts/container/ itself, not the repo root. The
# Dockerfile copies nothing, so a wider context would only offer a future
# author the chance to bake a host artifact into a layer -- an image built
# from a host copy is unauditable, unreproducible, and unavailable to a
# contributor who has no such host artifact.
build_args=()
if [ -n "$VERSION" ]; then
    build_args+=(--build-arg "CLAUDE_VERSION=$VERSION")
fi

echo "build: building $TAG" >&2
docker build \
    -f "$SCRIPT_DIR/Dockerfile" \
    -t "$TAG" \
    "${build_args[@]+"${build_args[@]}"}" \
    "$SCRIPT_DIR"

# Report what the image ACTUALLY carries, read off the running binary rather
# than echoed back from the ARG. A mismatch between the requested pin and the
# observed version is information, not an error -- the observed value is the
# one a fixture records.
observed="$(docker run --rm "$TAG" claude --version)"
echo "build: built $TAG" >&2
echo "build: client reports: $observed" >&2
