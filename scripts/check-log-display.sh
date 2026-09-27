#!/usr/bin/env bash
# Production log-sink inventory guard.
#
# Record integrity (one physical line per record, no raw control, format,
# separator, or default-ignorable character) is enforced at the one
# production tracing sink, crates/routectl-cli/src/log_sink.rs. This gate
# proves nothing else in production can build or install a subscriber and
# that the sink still wires its escaping formatter; see the docstring of
# check-log-display.py for the exact rules. It does NOT check sanitizer
# dataflow -- call-site sanitizers stay reviewed code, not gated code.
#
# Fails closed: a missing tool, a git / cargo / read / UTF-8 failure, or a
# module source that cannot be resolved to a cached or untracked,
# non-ignored file is a failure.
#
# Run from anywhere inside the repo:
#   bash scripts/check-log-display.sh

set -euo pipefail

for tool in git cargo python3; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "check-log-display: required tool '$tool' not found" >&2
        exit 1
    fi
done

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$HERE/check-log-display.py"
