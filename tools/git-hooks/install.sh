#!/usr/bin/env bash
# Idempotent installer for the checked-in git hooks. Symlinks the hooks
# from tools/git-hooks/ into .git/hooks/ so the working copy stays the
# source of truth (edit the tracked file, the hook updates).
#
# Safe to re-run: a symlink already pointing at the intended target is
# left as-is. A pre-existing NON-symlink hook (or a symlink to a
# different target) is NOT clobbered silently -- the installer aborts
# with a message unless `--force` is passed.
#
# Run from anywhere in the working tree:
#   bash tools/git-hooks/install.sh [--force]

set -e

FORCE=0
if [[ "${1:-}" == "--force" ]]; then
    FORCE=1
fi

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT/.git/hooks"

# Link one hook: $1 = relative target (from .git/hooks/), $2 = hook name.
# Idempotent when the link already points at the target. Refuses to
# overwrite a non-symlink (or a symlink to a different target) unless
# --force was given.
link_hook() {
    local target="$1"
    local name="$2"

    if [[ -L "$name" ]]; then
        # Already a symlink: no-op if it points where we want.
        if [[ "$(readlink "$name")" == "$target" ]]; then
            return 0
        fi
        if [[ "$FORCE" -ne 1 ]]; then
            echo "git-hooks: $name is a symlink to '$(readlink "$name")', not '$target'." >&2
            echo "git-hooks: refusing to overwrite; re-run with --force to replace it." >&2
            exit 1
        fi
    elif [[ -e "$name" ]]; then
        # A real (non-symlink) file/dir is in the way.
        if [[ "$FORCE" -ne 1 ]]; then
            echo "git-hooks: $name already exists and is not a symlink." >&2
            echo "git-hooks: refusing to overwrite; re-run with --force to replace it." >&2
            exit 1
        fi
    fi

    ln -sf "$target" "$name"
}

# Relative target so the symlink survives a moved checkout. From
# .git/hooks/ the repo-root tools dir is two levels up.
link_hook ../../tools/git-hooks/pre-commit pre-commit
link_hook ../../tools/git-hooks/commit-msg commit-msg

echo "git-hooks: installed pre-commit and commit-msg into .git/hooks/"
