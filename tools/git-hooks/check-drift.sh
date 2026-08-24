#!/usr/bin/env bash
# Warns (never fails) when the running hook is a plain copy of a tracked
# hook -- rather than the symlink install.sh creates -- and its content
# differs from the tracked source. A copy pinned at install time gets no
# other signal that the two have diverged: install.sh only runs once, so
# a contributor who `cp`'d instead of symlinking never sees a later edit
# to tools/git-hooks/<hook> land in their .git/hooks/<hook> -- nor does a
# contributor who then deliberately edited their own copy get told the
# tracked source moved out from under them. `cmp` only detects that the
# two differ, not which one changed, so the warning states divergence
# and leaves the direction (and the fix) to the reader.
#
# Sourced by each hook (pre-commit, commit-msg) rather than invoked as a
# subprocess: it reads $0 as the path git actually executed, which a
# subprocess would only see as "check-drift.sh" itself. Sourced from the
# tracked tools/git-hooks/ path, so the check logic is always current even
# when the hook that sourced it is the stale copy under test.
#
# check_hook_drift <hook-name> <running-hook-path> <tracked-hook-path>
#
# Silent when:
#   - the running hook is a symlink (install.sh's own output; content is
#     the tracked file by construction), or
#   - the running hook's content matches the tracked file, or
#   - a warning for this hook already fired within the last 24h (one
#     calendar day is a defensible "once": long enough that a contributor
#     mid-session doesn't get renagged every commit, short enough that
#     re-attaching tomorrow surfaces it again if they still haven't fixed
#     it).
#
# Always returns 0: a hygiene nag must never block or slow the commit it
# rides on.
check_hook_drift() {
    local hook_name="$1" hook_path="$2" tracked_path="$3"

    [[ -L "$hook_path" ]] && return 0
    [[ -f "$hook_path" ]] || return 0
    [[ -f "$tracked_path" ]] || return 0

    if cmp -s "$hook_path" "$tracked_path" 2>/dev/null; then
        return 0
    fi

    local marker_dir marker
    marker_dir="$(dirname "$hook_path")"
    marker="$marker_dir/.drift-warned-$hook_name"
    if [[ -f "$marker" ]]; then
        local recent
        recent="$(find "$marker" -mmin -1440 2>/dev/null)"
        [[ -n "$recent" ]] && return 0
    fi

    echo "git-hooks: WARNING .git/hooks/$hook_name is a copy, not a symlink, and its content differs from tools/git-hooks/$hook_name." >&2
    echo "git-hooks: this only means the two differ, not which one moved -- a deliberate local edit differs too. If yours wasn't deliberate, consider: bash tools/git-hooks/install.sh --force" >&2
    touch "$marker" 2>/dev/null || true
    return 0
}
