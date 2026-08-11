#!/usr/bin/env bash
# Check rustfmt compliance of every `include!`d source fragment.
#
# WHY THIS EXISTS: `cargo fmt --all -- --check` walks the MODULE tree from
# each crate root. A fragment pulled in by `include!("foo_tests.rs")` is not
# a module -- rustfmt never opens the file, so the workspace fmt gate passes
# VACUOUSLY on every fragment in the repo. This repo splits large test
# modules into `include!`d fragments to stay under the file-size ceiling, so
# the vacuous pass covers a substantial amount of source: one fragment had
# been drifting unformatted for its entire history behind a green gate.
#
# `#[path = "..."] mod name;` files are NOT in scope here: those ARE modules,
# so cargo fmt already covers them. Only `include!` defeats it.
#
# Fragments are discovered from the `include!` call sites themselves rather
# than from a filename pattern, so a newly added fragment is covered the
# moment it is included -- nothing to remember to register.
#
# Exit codes: 0 = all fragments formatted, 1 = at least one is not, 2 = usage.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi

cd "$REPO_ROOT"

# The edition must be passed explicitly: invoked as a bare `rustfmt` on a
# single file there is no Cargo manifest in play, so the edition cannot be
# inferred and rustfmt would silently fall back to 2015 and reformat toward
# a different grammar than the crates actually use.
EDITION=2024

if ! command -v rustfmt >/dev/null 2>&1; then
    echo "fmt-fragments: ERROR rustfmt not installed" >&2
    exit 1
fi

# Resolve each include!("relative.rs") against the directory of the file
# that includes it, which is how rustc resolves it.
collect_fragments() {
    local src rel dir
    while IFS=: read -r src rel; do
        dir="$(dirname "$src")"
        printf '%s\n' "$dir/$rel"
    done < <(
        grep -rn --include='*.rs' -oE 'include!\("[^"]+\.rs"\)' crates \
            | sed -E 's/^([^:]+):[0-9]+:include!\("(.+)"\)$/\1:\2/'
    ) | sort -u
}

mapfile -t fragments < <(collect_fragments)

if (( ${#fragments[@]} == 0 )); then
    echo "fmt-fragments: no include!d fragments found"
    exit 0
fi

failed=()
missing=()
for frag in "${fragments[@]}"; do
    if [[ ! -f "$frag" ]]; then
        missing+=("$frag")
        continue
    fi
    if ! rustfmt --edition "$EDITION" --check "$frag" >/dev/null 2>&1; then
        failed+=("$frag")
    fi
done

printf 'fmt-fragments: checked %d include!d fragment(s)\n' "${#fragments[@]}"

if (( ${#missing[@]} )); then
    echo
    echo "include! targets that do not exist (broken include, or a path this"
    echo "script resolved wrongly -- either way it needs a look):"
    printf '  %s\n' "${missing[@]}"
fi

if (( ${#failed[@]} )); then
    echo
    echo "NOT rustfmt-clean:"
    printf '  %s\n' "${failed[@]}"
    echo
    echo "Fix with:"
    printf '  rustfmt --edition %s %s\n' "$EDITION" "${failed[*]}"
    exit 1
fi

if (( ${#missing[@]} )); then
    exit 1
fi

echo "all include!d fragments are rustfmt-clean."
exit 0
