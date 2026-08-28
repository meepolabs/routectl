#!/usr/bin/env bash
# Run ONE capture inside the capture-cell image.
#
# This is a CALLER of scripts/capture_driver.sh, never a mode of it. The
# directory boundary is the design: nothing here is reachable from the
# runner or the rig, so neither of those scripts can grow a
# container-conditional branch, and the host capture path stays the
# supported one.
#
# Everything after `--` is handed to capture_driver.sh VERBATIM inside the
# container, and the container's exit code comes back out of this script
# unchanged -- so the runner's 0/2/3/4/5/6/7 contract is what a caller
# reads, exactly as it would from a host run.
#
# Usage:
#   run_capture.sh --scratch <host-dir> [--image <tag>] \
#                  -- <capture_driver.sh args...>
#
#   e.g.
#   run_capture.sh --scratch /var/tmp/routectl-cell -- \
#     --lane anthropic-api --case plain-turn-01 \
#     -- scripts/drivers/claude-code-print.sh
#
#   --scratch   Host directory the fixture lands in. REQUIRED, and
#               deliberately so: a fixture carries RAW headers, and a
#               default would create a credential-bearing destination
#               under the operator's home without anyone choosing it.
#               Must be OUTSIDE this repo, because the repo is mounted
#               read-only and a landing path inside it could not be
#               written. It is the ONLY writable mount.
#   --image     Image tag to run. Default `routectl-capture:default`,
#               which is what scripts/container/build.sh writes with no
#               --version. Never a bare `:latest`.
#
# WHAT CROSSES THE BOUNDARY, and nothing else:
#
#   this repo          -> /workspace                    READ-ONLY
#   the host binary    -> /usr/local/lib/routectl/bin   READ-ONLY
#   --scratch          -> /scratch                      writable
#   the upstream token -> ROUTECTL_DRIVER_ANTHROPIC_API_KEY (by NAME)
#
# THE READ-ONLY REPO IS LOAD-BEARING, not tidiness. A driven agent runs
# with file tools and permission prompts disabled; a writable repo mount
# would let the thing under capture edit tracked source, and a capture is
# evidence only if the tree that produced it did not move under it.
#
# NO HOST ENVIRONMENT IS FORWARDED except the token variable named above.
# That is a property, not an omission: an inherited ANTHROPIC_BASE_URL is
# how a capture once ended up recording a daemon nobody meant to capture,
# and a container that starts from an empty environment cannot inherit
# one.
#
# THE SEAT FILE IS NOT MOUNTED and none exists inside the container. The
# access token is extracted HERE, on the host, and passed as the
# pre-existing `env://ROUTECTL_DRIVER_ANTHROPIC_API_KEY` the lane config
# already names. Read-only mounting the seat was measured to make a token
# refresh UN-RECORDABLE rather than impossible -- the refresh POST rotates
# the token upstream first and only then fails to persist -- so the exact
# harm the read-only mount existed to prevent is reachable through it. A
# plain env read has no refresh path at all, which makes "mid-run expiry
# is a hard stop, never a refresh" true by construction: expiry is a clean
# upstream 401, rig exit 3, runner exit 7, already modelled.
#
# The token is passed to `docker run` BY NAME (`-e VAR`, no `=value`), so
# it never appears in an argument vector -- /proc/<pid>/cmdline is world
# readable and a value on a command line is readable by every account on
# the box. It is never written to a file and never logged.
#
# WHAT THIS REFUSES, each with its own exit code, before docker is
# consulted at all -- so a refusal reads identically on a box with no
# docker:
#
#   10  `--network host` / `--network=host` in the caller's argv
#   11  `--net host` / `--net=host`
#   12  `--privileged`
#   13  `--pid host` / `--pid=host`
#   14  any caller-added mount flag (-v / --volume / --mount / --tmpfs /
#       --volumes-from)
#   15  the host routectl binary is missing or not executable
#   16  the scratch root is inside this repo
#   17  docker is not installed or not on PATH
#   18  the image is not present locally
#   19  the seat file is unreadable
#   20  the seat carries no usable anthropic access token
#
# The first four exist because each one individually dissolves the
# isolation the cell is for, and a shared exit code would let a caller who
# reads only the number confuse them. They are refused by NAME rather than
# by a passthrough-to-docker allowlist because there is no docker
# passthrough here at all: the container's shape is decided in this file
# and nowhere else.
#
# The mount refusal is deliberately an OVER-APPROXIMATION: the tokens are
# refused anywhere in the caller's argv, including inside the driver
# command. A driver that genuinely needs a `-v` of its own spells it
# differently; that cost is smaller than a mount flag reaching docker
# because it sat after the driver separator.
#
# Exit codes 0 and 2-7 are the runner's, propagated verbatim. 2 doubles as
# this script's own usage error, which is the same class of fault.
#
# `ROUTECTL_BIN` selects the host binary to mount (default
# `target/release/routectl`). `ROUTECTL_CAPTURE_CELL_SEAT` selects the
# seat file to read (default `$XDG_CONFIG_HOME/routectl/credentials.json`,
# falling back to `$HOME/.config/...` -- the same resolution order the
# daemon itself uses). Both exist so the self-test can exercise this
# script end to end without a real credential and without a real daemon.
# --- END USAGE ---

set -eu

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

# The one owner of path resolution in this repo, shared with the runner,
# the rig and the promote script. Sourced rather than reimplemented: the
# resolution pair there encodes three separately-discovered subtleties
# (collapse after resolution, dangling symlinks past `cd -P`,
# per-component symlink checks before resolution), and the scratch-root
# check below is a containment question of exactly that kind.
CONFINE_LIB="$REPO_ROOT/scripts/drivers/lib/confine.sh"
if [ ! -r "$CONFINE_LIB" ]; then
    echo "run_capture: confinement library not found at $CONFINE_LIB; refusing to run" >&2
    exit 2
fi
# shellcheck source=scripts/drivers/lib/confine.sh
. "$CONFINE_LIB"

# In-container paths. Fixed rather than derived: the runner resolves its
# lane configs, its cases and its confinement root from its own location,
# so the mount point is part of this script's contract with the image and
# a caller has no business choosing it.
CELL_REPO=/workspace
CELL_SCRATCH=/scratch
CELL_BIN_DIR=/usr/local/lib/routectl/bin
CELL_BIN="$CELL_BIN_DIR/routectl"

DEFAULT_IMAGE="routectl-capture:default"
TOKEN_VAR="ROUTECTL_DRIVER_ANTHROPIC_API_KEY"

usage() {
    sed -n '2,/^# --- END USAGE ---$/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//' >&2
}

die() {
    echo "run_capture: $1" >&2
    exit "$2"
}

SCRATCH=""
IMAGE="$DEFAULT_IMAGE"

# This script's own flags stop at the FIRST `--`; everything after it is
# the runner's argv, which carries its own `--` for the driver command.
# Two separators rather than positional guessing: a caller's `--lane` must
# never be mistaken for a flag of this script, and a flag of this script
# must never be forwarded into the container.
while [ $# -gt 0 ]; do
    case "$1" in
        --scratch)
            [ $# -ge 2 ] && [ -n "$2" ] || die "--scratch needs a non-empty value" 2
            SCRATCH="$2"
            shift 2
            ;;
        --image)
            [ $# -ge 2 ] && [ -n "$2" ] || die "--image needs a non-empty value" 2
            IMAGE="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        *)
            echo "run_capture: unknown argument: $1" >&2
            echo "run_capture: this script's own flags end at \`--\`; the runner's argv follows it" >&2
            exit 2
            ;;
    esac
done

[ -n "$SCRATCH" ] || die "--scratch is required (it is the only writable mount, so it is never guessed)" 2
[ $# -ge 1 ] || die "no capture_driver.sh arguments given (pass them after \`--\`)" 2

# ---------------------------------------------------------------------
# Refusals decided from the caller's argv alone
# ---------------------------------------------------------------------

# One rule per exit code, and each rule matches BOTH spellings docker
# accepts (`--flag value` and `--flag=value`). A rule that saw only one
# spelling would be a guard a caller defeats with an equals sign.
refuse_isolation_flags() {
    local prev="" arg
    for arg in "$@"; do
        case "$arg" in
            --network=host)
                die "refusing '--network=host': host networking puts the container on the host's loopback, where the live daemon and its seat store are reachable. that is the isolation this cell exists to provide." 10 ;;
            --net=host)
                die "refusing '--net=host': the short spelling of --network=host, and refused for the same reason." 11 ;;
            --privileged|--privileged=true)
                die "refusing '--privileged': it hands the container the host's devices and drops confinement wholesale, so nothing else this script mounts read-only would still be read-only in effect." 12 ;;
            --pid=host)
                die "refusing '--pid=host': the host pid namespace makes every host process visible and signalable, including the live daemon this run must not touch." 13 ;;
            -v|--volume|--mount|--tmpfs|--volumes-from)
                die "refusing '$arg': the mount set is decided in this script -- repo read-only, binary read-only, one writable scratch root. a caller-added mount is how a writable repo or a seat file gets in." 14 ;;
            -v=*|--volume=*|--mount=*|--tmpfs=*|--volumes-from=*)
                die "refusing '${arg%%=*}': the mount set is decided in this script -- repo read-only, binary read-only, one writable scratch root. a caller-added mount is how a writable repo or a seat file gets in." 14 ;;
        esac
        if [ "$arg" = host ]; then
            case "$prev" in
                --network)
                    die "refusing '--network host': host networking puts the container on the host's loopback, where the live daemon and its seat store are reachable. that is the isolation this cell exists to provide." 10 ;;
                --net)
                    die "refusing '--net host': the short spelling of --network host, and refused for the same reason." 11 ;;
                --pid)
                    die "refusing '--pid host': the host pid namespace makes every host process visible and signalable, including the live daemon this run must not touch." 13 ;;
            esac
        fi
        prev="$arg"
    done
}

refuse_isolation_flags "$@"

# ---------------------------------------------------------------------
# The scratch root
# ---------------------------------------------------------------------

# A newline defeats every containment test below and every quoting
# discipline downstream: both resolvers read their result back through
# `$(...)`, which strips trailing newlines and would leave a multi-line
# path validated only up to its first line. Refused outright, the same way
# the confinement library refuses it.
case "$SCRATCH" in
    *"
"*)
        die "--scratch contains a newline; a path that cannot survive command substitution cannot be confined" 2 ;;
esac

# Resolved PHYSICALLY on both sides, and BEFORE anything is created: a
# refusal must not leave a directory behind inside the repo it just
# refused to write to. `abspath_physical` walks up to the nearest existing
# ancestor, so it resolves a path that does not exist yet. A lexical
# compare would not see a symlink pointing back into the repo, which is
# exactly the shape that presents an in-repo destination as an outside one.
SCRATCH_ABS="$(abspath_physical "$SCRATCH")"
REPO_ABS="$(abspath_physical "$REPO_ROOT")"
[ -n "$SCRATCH_ABS" ] && [ -n "$REPO_ABS" ] ||
    die "could not physically resolve --scratch '$SCRATCH' or the repo root; an unresolved path cannot be confined" 2

case "$SCRATCH_ABS" in
    "$REPO_ABS"|"$REPO_ABS"/*)
        echo "run_capture: refusing --scratch '$SCRATCH_ABS': it is inside this repo." >&2
        echo "the repo is mounted read-only, so a fixture could not be written there --" >&2
        echo "and the read-only mount is what stops a driven agent editing tracked" >&2
        echo "source. choose a scratch root outside the repo." >&2
        exit 16
        ;;
esac

mkdir -p "$SCRATCH_ABS" 2>/dev/null ||
    die "--scratch '$SCRATCH_ABS' could not be created" 2
[ -d "$SCRATCH_ABS" ] || die "--scratch '$SCRATCH_ABS' is not a directory" 2

# ---------------------------------------------------------------------
# The host binary
# ---------------------------------------------------------------------

# No Rust in the image, by design: a toolchain layer would make the image
# a build environment and the fixture's provenance a question about which
# compiler ran where. The host-built binary is bind-mounted read-only
# instead, so the routectl under capture is the one the operator built and
# can point at.
HOST_BIN="${ROUTECTL_BIN:-$REPO_ROOT/target/release/routectl}"
[ -f "$HOST_BIN" ] && [ -x "$HOST_BIN" ] ||
    die "host routectl binary '$HOST_BIN' is missing or not executable; build it (cargo build --release) or point ROUTECTL_BIN at one. the image carries no routectl." 15

# Resolved through its DIRECTORY, then recombined. `abspath_physical`
# resolves a path by `cd -P`-ing into its nearest existing ancestor, which
# cannot be a regular file -- handed the binary itself it fails outright.
# The dirname is the ancestor that matters anyway: a symlinked directory in
# the path is what would make the mounted binary a different file from the
# one checked above.
HOST_BIN_DIR_ABS="$(abspath_physical "$(dirname "$HOST_BIN")")" ||
    die "could not physically resolve the directory of the host binary '$HOST_BIN'" 15
[ -n "$HOST_BIN_DIR_ABS" ] ||
    die "could not physically resolve the directory of the host binary '$HOST_BIN'" 15
HOST_BIN_ABS="$HOST_BIN_DIR_ABS/$(basename "$HOST_BIN")"
[ -f "$HOST_BIN_ABS" ] && [ -x "$HOST_BIN_ABS" ] ||
    die "host routectl binary '$HOST_BIN' does not resolve to an executable file at '$HOST_BIN_ABS'" 15

# ---------------------------------------------------------------------
# The seat: extracted HERE, never mounted
# ---------------------------------------------------------------------

# Same resolution order the daemon uses, so the token this run carries is
# the one the operator's own routectl would resolve.
if [ -n "${ROUTECTL_CAPTURE_CELL_SEAT:-}" ]; then
    SEAT="$ROUTECTL_CAPTURE_CELL_SEAT"
elif [ -n "${XDG_CONFIG_HOME:-}" ]; then
    SEAT="$XDG_CONFIG_HOME/routectl/credentials.json"
else
    SEAT="${HOME:-}/.config/routectl/credentials.json"
fi

[ -r "$SEAT" ] ||
    die "seat file '$SEAT' is unreadable; the container has no seat of its own and cannot obtain a token any other way" 19

# Read-only, one field, nothing else. The seat is not rewritten, not
# locked, and no lock file is created beside it: the live daemon holds the
# same file open and this run has no business perturbing it. Errors go to
# /dev/null on purpose -- a parse failure must not spill neighbouring seat
# content into this script's stderr.
extract_token() {
    python3 - "$1" <<'PY' 2>/dev/null
import json, sys

try:
    with open(sys.argv[1], encoding="utf-8") as fh:
        doc = json.load(fh)
    token = doc["providers"]["anthropic"]["access_token"]
except Exception:
    raise SystemExit(1)
if not isinstance(token, str) or not token:
    raise SystemExit(1)
sys.stdout.write(token)
PY
}

TOKEN="$(extract_token "$SEAT")" || TOKEN=""
[ -n "$TOKEN" ] ||
    die "seat file '$SEAT' carries no usable providers.anthropic.access_token; refusing to start a run that would 401 at the upstream after booting a daemon" 20

# Exported so `docker run -e NAME` can forward it BY NAME. The value never
# reaches an argument vector, a file, or a log line.
export "$TOKEN_VAR=$TOKEN"

# ---------------------------------------------------------------------
# Docker
# ---------------------------------------------------------------------

command -v docker >/dev/null 2>&1 ||
    die "docker is not installed or not on PATH" 17

docker image inspect "$IMAGE" >/dev/null 2>&1 ||
    die "image '$IMAGE' is not present locally; build it with scripts/container/build.sh. a run that pulled an image from a registry would capture under a layer set nobody in this repo committed." 18

# `--user` with the host uid/gid so the fixture lands owned by the
# operator rather than by root -- a root-owned fixture cannot be promoted
# or deleted without sudo, and the scrub gate would then run over a tree
# the operator cannot rewrite.
#
# `--network bridge` is EXPLICIT even though it is the daemon's default: a
# daemon whose default network was reconfigured would silently change the
# one property the isolation assertion rests on, and the assertion would
# still pass while meaning something else.
#
# The TOKEN is passed BY NAME (`-e VAR`, no `=value`), which forwards the
# exported value from this process's environment. `-e VAR=value` would put
# the token in this process's argv, which /proc exposes to every account
# on the box. The other two carry in-container paths, which are not
# secrets and are set here rather than inherited so a host value cannot
# leak in.
#
# `ROUTECTL_DRIVER_OUT_ROOT` is what makes the `--out` below acceptable to
# the runner: the runner confines `--out` to a CLOSED SET of roots, and
# with the repo read-only the landing path has to be outside it, so the
# only way to name /scratch is to put it in that set.
docker run \
    --rm \
    --user "$(id -u):$(id -g)" \
    --network bridge \
    --workdir "$CELL_REPO" \
    -v "$REPO_ABS:$CELL_REPO:ro" \
    -v "$HOST_BIN_ABS:$CELL_BIN:ro" \
    -v "$SCRATCH_ABS:$CELL_SCRATCH" \
    -e "$TOKEN_VAR" \
    -e "ROUTECTL_BIN=$CELL_BIN" \
    -e "ROUTECTL_DRIVER_OUT_ROOT=$CELL_SCRATCH" \
    "$IMAGE" \
    bash "$CELL_REPO/scripts/capture_driver.sh" \
    --out "$CELL_SCRATCH" \
    --out-root "$CELL_SCRATCH" \
    "$@"
