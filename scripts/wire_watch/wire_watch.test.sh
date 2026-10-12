#!/usr/bin/env bash
# Self-test for the wire-watch scripts (matrix, run ledger, evidence
# reader). Exits 0 when every unit test passes.
#
# Needs only python3 (stdlib) and bash; no routectl binary, no network,
# no credential. The tests run against throwaway copies of the committed
# inputs and drive the real per-cell evidence writer.
#
# Run it from anywhere:
#   bash scripts/wire_watch/wire_watch.test.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Bytecode is not written: a mutation check restored within the same
# second could otherwise run the cached mutated module.
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s "$HERE/tests" -t "$HERE/tests" -v
