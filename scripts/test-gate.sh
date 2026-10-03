#!/usr/bin/env bash
# Gate-command registry: the single source for the exact cargo commands the
# repo's test gates run. The pre-push hook calls it, CI is meant to call it,
# and docs/DEVELOPMENT.md points at it, so the commands cannot drift apart
# across those places. Each subcommand prints the command it runs, then runs
# it from the repo root. Logic beyond the command itself stays in the
# dedicated scripts.
#
# Test subcommands use the `test-release` profile (workspace Cargo.toml):
# release correctness semantics with a cheaper build. The release-build
# subcommand uses the shipped release profile unchanged.
#
# Usage:
#   test-gate.sh pre-push [HARNESS_ARGS...]
#       cargo test --workspace --profile test-release
#         -- --skip egress_replay_all --skip ingress_replay_all
#       The two skipped names are kept verbatim: those tests are
#       report-only drivers over the local captured corpus and never gate.
#   test-gate.sh workspace-all-features [HARNESS_ARGS...]
#       cargo test --workspace --all-features --profile test-release
#         --offline --no-fail-fast
#   test-gate.sh release-build
#       cargo build --locked --release -p routectl-cli
#       Accepts no extra arguments.
#
# HARNESS_ARGS are appended after `--`, i.e. passed to the test harness
# (e.g. a test-name filter or --nocapture), never to cargo.
#
# Exit codes: the gate command's own exit code; 2 = usage.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
    sed -n '/^# Usage:/,/^# Exit codes:/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

run() {
    printf '+ %s\n' "$*" >&2
    cd "$REPO_ROOT"
    exec "$@"
}

if [[ $# -eq 0 ]]; then
    usage >&2
    exit 2
fi

subcommand="$1"
shift

case "$subcommand" in
    pre-push)
        run cargo test --workspace --profile test-release \
            -- --skip egress_replay_all --skip ingress_replay_all "$@"
        ;;
    workspace-all-features)
        run cargo test --workspace --all-features --profile test-release \
            --offline --no-fail-fast -- "$@"
        ;;
    release-build)
        if [[ $# -gt 0 ]]; then
            echo "test-gate.sh: release-build accepts no extra arguments" >&2
            exit 2
        fi
        run cargo build --locked --release -p routectl-cli
        ;;
    -h|--help)
        usage
        ;;
    *)
        echo "test-gate.sh: unknown subcommand: $subcommand" >&2
        usage >&2
        exit 2
        ;;
esac
