#!/usr/bin/env bash
# Run the public-API baseline check IF the tooling for it is installed,
# and warn (exit 0) if it is not.
#
# WHY the conditional exists: the baseline went stale silently twice with
# nothing gating it locally, so the check blocks wherever the tooling is
# present. But it needs cargo-public-api plus a pinned nightly, and CI does
# not run this leg at all, so failing closed here would force a nightly
# toolchain onto every contributor machine to make any commit. Fail-open
# with a loud, actionable warning is the deliberate trade.
#
# WHY it is a script rather than an inline hook entry: the probe needs the
# nightly pin, and the pin's single source of truth is public-api.sh.
# Grepping it out here keeps the two from drifting apart, which a
# duplicated literal in .pre-commit-config.yaml would not.
#
# Always exits 0 unless public-api.sh itself reports drift.
#
# Usage: public-api-if-available.sh   (no arguments)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

PUBLIC_API_NIGHTLY="$(grep -oE '^PUBLIC_API_NIGHTLY=.*' scripts/public-api.sh | cut -d= -f2)"
if [[ -z "$PUBLIC_API_NIGHTLY" ]]; then
    echo "public-api-if-available: could not read PUBLIC_API_NIGHTLY from scripts/public-api.sh" >&2
    exit 1
fi

missing=()
command -v cargo-public-api >/dev/null 2>&1 ||
    missing+=("cargo-public-api -- install: cargo install cargo-public-api --version 0.52.0")
rustup toolchain list 2>/dev/null | grep -qF "$PUBLIC_API_NIGHTLY" ||
    missing+=("nightly toolchain $PUBLIC_API_NIGHTLY -- install: rustup toolchain install $PUBLIC_API_NIGHTLY --profile minimal")

if [[ ${#missing[@]} -gt 0 ]]; then
    echo "public-api: WARNING skipping baseline check (local-only leg; CI does not run this)" >&2
    for m in "${missing[@]}"; do
        echo "public-api:   missing $m" >&2
    done
    exit 0
fi

echo "public-api: baseline check (local-only leg; CI does not run this)"
exec bash scripts/public-api.sh --check all
