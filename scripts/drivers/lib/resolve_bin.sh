#!/usr/bin/env bash
# Resolution of the routectl binary a probe or self-test should drive, as
# one sourced declaration.
#
# Sourced, never executed. Same posture as
# scripts/drivers/lib/ingress_kinds.sh: one owner, sourced by every
# caller, an absent library is a hard failure rather than a skipped
# check.
#
# The order is deliberate. An explicit ROUTECTL_BIN wins, so a caller can
# point a probe at a binary that is neither on PATH nor under the target
# dir. PATH comes next, because an installed routectl is what a
# contributor's own run resolves. The target-dir sweep is last and tries
# debug before release: a developer who just built one is usually probing
# the build they are iterating on, and a stale release artifact silently
# winning over a fresh debug one is the confusing case.
#
# REPO_ROOT must be set by the caller before sourcing; the target-dir
# fallback is relative to it.

resolve_bin() {
    if [ -n "${ROUTECTL_BIN:-}" ]; then
        printf '%s\n' "$ROUTECTL_BIN"
        return 0
    fi
    if command -v routectl >/dev/null 2>&1; then
        command -v routectl
        return 0
    fi
    local target_dir="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
    for profile in debug release; do
        if [ -x "$target_dir/$profile/routectl" ]; then
            printf '%s\n' "$target_dir/$profile/routectl"
            return 0
        fi
    done
    return 1
}
