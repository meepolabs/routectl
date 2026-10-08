#!/usr/bin/env bash
# Pre-push leg of the public-API baseline check. Runs the gate registry's
# `public-api` subcommand (scripts/test-gate.sh) when the pinned tooling is
# installed, and SKIPS with one visible line when it is not, because CI's
# public-api job runs the same check unconditionally. Missing tooling never
# fails a push; a stale baseline does, once the tooling is present.
#
# "Installed" means both pins at the top of scripts/public-api.sh are met:
# cargo-public-api on PATH reporting exactly the version on its Bootstrap
# `cargo install` line, and the rustup toolchain PUBLIC_API_NIGHTLY installed
# with everything the check needs from it, probed through the same `+toolchain`
# proxies on PATH the check goes through: cargo, rustdoc, and rustc with the
# host's standard library. Probes never let rustup auto-install a toolchain.
# The pins are read from that file the same way CI's public-api job reads
# them, so there is no second copy to drift. A pin that cannot be read is a
# wiring defect and fails rather than skips.
#
# Usage: public-api-pre-push.sh
#
# Exit codes: 0 = clean or skipped, 1 = drift / check failure / unreadable
# pin, 2 = usage.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PUBLIC_API_SCRIPT="$HERE/public-api.sh"

if [[ $# -gt 0 ]]; then
    echo "public-api-pre-push: accepts no arguments" >&2
    exit 2
fi

read_pins() {
    nightly="$(grep -oE '^PUBLIC_API_NIGHTLY=.*' "$PUBLIC_API_SCRIPT" | cut -d= -f2 || true)"
    version="$(grep -oE 'cargo-public-api --version [0-9]+\.[0-9]+\.[0-9]+' "$PUBLIC_API_SCRIPT" \
        | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)"
    if [[ -z "$nightly" || -z "$version" ]]; then
        echo "public-api-pre-push: FAIL: could not read the pins from $PUBLIC_API_SCRIPT" >&2
        exit 1
    fi
}

# Prints why the tooling is unusable, or nothing when both pins are met.
missing_tooling() {
    local have libdir
    export RUSTUP_AUTO_INSTALL=0
    if ! command -v cargo-public-api >/dev/null 2>&1; then
        echo "cargo-public-api not on PATH"
        return
    fi
    have="$(cargo-public-api --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)"
    if [[ "$have" != "$version" ]]; then
        echo "cargo-public-api ${have:-<unknown version>} is not the pinned $version"
        return
    fi
    if ! command -v rustup >/dev/null 2>&1; then
        echo "rustup not on PATH, so the pinned $nightly cannot be selected"
        return
    fi
    if ! rustup which --toolchain "$nightly" cargo >/dev/null 2>&1; then
        echo "toolchain $nightly not installed"
        return
    fi
    if ! cargo "+$nightly" --version >/dev/null 2>&1; then
        echo "cargo on PATH rejects +$nightly, so it is not the rustup proxy"
        return
    fi
    if ! rustdoc "+$nightly" --version >/dev/null 2>&1; then
        echo "rustdoc for $nightly unavailable on PATH"
        return
    fi
    if ! libdir="$(rustc "+$nightly" --print target-libdir 2>/dev/null)"; then
        echo "rustc for $nightly unavailable on PATH"
        return
    fi
    if ! compgen -G "$libdir/libstd-*.rlib" >/dev/null; then
        echo "toolchain $nightly lacks rust-std for the host"
        return
    fi
}

read_pins
reason="$(missing_tooling)"
if [[ -n "$reason" ]]; then
    echo "public-api: SKIPPED locally ($reason); CI runs this check." \
        "To run it here, install the tooling on the Bootstrap lines of scripts/public-api.sh."
    exit 0
fi

exec bash "$HERE/test-gate.sh" public-api
