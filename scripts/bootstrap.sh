#!/usr/bin/env bash
# One-time per-clone setup: install the local commit gate.
#
# The predecessor to this script was a hooks installer nobody ran -- a fresh
# clone had zero local enforcement until someone remembered a manual step.
# This is the single command a new clone runs, and it is idempotent, so
# re-running it after a config change is always safe.
#
# `--install-hooks` pre-builds each hook's environment now rather than
# lazily inside the first commit, so a contributor's first commit is not the
# thing that pays a multi-minute toolchain fetch.
#
# The hook types come from `default_install_hook_types` in
# .pre-commit-config.yaml -- do not pass --hook-type here, or the config
# stops being the single source of truth for which stages are wired.
#
# Usage: bash scripts/bootstrap.sh

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

if ! command -v pre-commit >/dev/null 2>&1; then
    cat >&2 <<'MSG'
bootstrap: pre-commit is not installed. Install it, then re-run:
  pipx install pre-commit    (or: pip install --user pre-commit,
                              brew install pre-commit,
                              uv tool install pre-commit)
MSG
    exit 1
fi

# Installing into a linked worktree writes the same shared hooks directory
# the main checkout uses, so one run covers every worktree of this clone.
pre-commit install --install-hooks

echo "bootstrap: local commit gate installed (see .pre-commit-config.yaml)"
