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
# Exit codes: 0 = all fragments formatted, 1 = at least one is not, a path
# escapes the repo, or an include! site cannot be parsed, 2 = usage.
#
# Properties a future edit must NOT regress (each is deliberate):
#   - `set -euo pipefail`, and quoting throughout (`mapfile -t` plus
#     `"${fragments[@]}"`) so a path with a space survives.
#   - `cd "$REPO_ROOT"` runs BEFORE any rustfmt call, so the rustup shim
#     resolves this repo's `rust-toolchain.toml` pin rather than a system
#     rustfmt with different defaults.
#   - `grep -r` (never `-R`) so symlinks are not followed into other trees.
#   - Both sides of the repo-root comparison are PHYSICAL paths (`pwd -P` plus
#     `physical_path`), so a symlinked checkout does not read as an escape.
#     Portable by construction -- no `realpath -m`, which BSD/macOS lacks.
#   - Every case this script can DETECT fails CLOSED: zero discovered fragments
#     is a hard error rather than a vacuous pass, a path leaving the repo is
#     refused before rustfmt sees it, and an `include!` the one-line parser
#     cannot consume is refused rather than silently skipped. The parser is the
#     honest limit of that guarantee: discovery is a grep, not a Rust parser, so
#     a spelling nobody has thought of is caught by the not-consumed check only
#     if it still contains the literal `include!` on some line.
#   - rustfmt runs with `--check` only and its output is discarded, so this
#     gate has no write primitive and no content-echo channel. Adding either
#     (an in-place fix, or echoing stderr) makes the escape guard load-bearing
#     rather than precautionary.

set -euo pipefail

# Both paths compared by the escape guard below must be PHYSICAL (symlinks
# resolved) or the comparison is meaningless. `pwd` is logical by default, so a
# symlinked checkout would keep the symlink here while the fragment side
# resolved through it, and the prefix test could never match -- rejecting every
# legitimate fragment. `pwd -P` on both sides is also portable, which matters:
# this leg runs in a contributor commit gate and the project ships macOS
# builds, where `realpath -m` is not available (BSD realpath lacks `-m`).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"

# Physical path of $1 without requiring it to exist: resolve the deepest
# existing ancestor with `cd -P`, then re-append the unresolved tail. Pure
# bash + POSIX `cd`/`pwd`, so it behaves identically on GNU and BSD.
physical_path() {
    local target="$1" dir base
    dir="$(dirname "$target")"
    base="$(basename "$target")"
    local tail="$base"
    # Walk up until an existing directory is found, accumulating the tail.
    while [[ ! -d "$dir" ]]; do
        tail="$(basename "$dir")/$tail"
        local parent
        parent="$(dirname "$dir")"
        # Defensive: dirname of "/" and of "." are themselves, so a path that
        # never resolves would spin here.
        [[ "$parent" == "$dir" ]] && break
        dir="$parent"
    done
    if [[ -d "$dir" ]]; then
        printf '%s/%s\n' "$(cd "$dir" && pwd -P)" "$tail"
    else
        printf '%s\n' "$target"
    fi
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    # Print the header block: every comment line after the shebang, stopping at
    # the first non-comment line. Deliberately NOT a hardcoded line range -- one
    # was here before and silently truncated this help mid-sentence the first
    # time the header grew.
    awk 'NR==1 && /^#!/ {next} /^#/ {sub(/^# ?/, ""); print; next} {exit}' "$0"
    exit 2
fi

cd "$REPO_ROOT"

# Before any rustfmt call, and repeated here rather than left to the caller:
# this leg is invoked standalone as often as it is from the commit gate,
# and a system rustfmt shadowing the rustup shim would format-check the
# fragments against different defaults than `cargo fmt` uses on the modules.
# The preflight also covers rustfmt's absence, which is why no separate
# command -v check remains below.
bash "$SCRIPT_DIR/assert-toolchain.sh"

# The edition must be passed explicitly: invoked as a bare `rustfmt` on a
# single file there is no Cargo manifest in play, so the edition cannot be
# inferred and rustfmt would silently fall back to 2015 and reformat toward
# a different grammar than the crates actually use.
EDITION=2024

# Resolve each include!("relative.rs") against the directory of the file
# that includes it, which is how rustc resolves it.
collect_fragments() {
    local src rel dir
    while IFS=: read -r src rel; do
        dir="$(dirname "$src")"
        printf '%s\n' "$dir/$rel"
    done < <(
        # Drop lines whose `include!` sits inside a `//` comment: that target
        # does not exist, and a missing target is a hard failure below, so a
        # dead comment would block every commit. `grep -v` on the
        # comment-prefix form is enough -- a real `include!` with a trailing
        # `//` comment after it still matches, which is correct.
        grep -rn --include='*.rs' -E 'include!\("[^"]+\.rs"\)' crates \
            | grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|/\*)' \
            | grep -oE '^[^:]+:[0-9]+:.*include!\("[^"]+\.rs"\)' \
            | sed -E 's/^([^:]+):[0-9]+:.*include!\("(.+)"\)$/\1:\2/'
    ) | sort -u
}

mapfile -t fragments < <(collect_fragments)

# FAIL CLOSED on an `include!` the one-line parser above cannot consume. rustc
# accepts a path split with a line continuation --
#   include!("frag\
#   ment.rs");
# -- which is legal Rust (verified by compiling one) and which the line-oriented
# grep above never sees. Without this check such a fragment is silently NOT
# checked while the gate reports success on its siblings: the exact vacuous pass
# this leg exists to close, reintroduced through a different door.
#
# Every non-consumed occurrence in this repo today is a comment line, so this
# lists zero and costs nothing until someone writes an exotic spelling.
unparsed="$(
    grep -rn --include='*.rs' -E 'include!' crates \
        | grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|/\*)' \
        | grep -vE 'include!\("[^"]+\.rs"\)' || true
)"
if [[ -n "$unparsed" ]]; then
    echo "fmt-fragments: found include! site(s) this script cannot parse, so it" >&2
    echo "cannot prove they are formatted. Refusing to pass while blind to them." >&2
    echo "Either spell the path on one line, or teach collect_fragments the form:" >&2
    printf '%s\n' "$unparsed" >&2
    exit 1
fi

if (( ${#fragments[@]} == 0 )); then
    # FAIL CLOSED. This repo has 25 `include!`d fragments; zero can only mean
    # discovery broke (a moved `crates/`, a grep behaviour change, a new
    # `include!` spelling). Exiting 0 here would report success while checking
    # nothing -- reproducing the vacuous-pass class this gate exists to close.
    echo "fmt-fragments: no include!d fragments found, which cannot be right" >&2
    echo "in this repo -- fragment discovery is broken. Refusing to pass" >&2
    echo "vacuously; fix discovery in scripts/fmt-fragments.sh." >&2
    exit 1
fi

failed=()
missing=()
escaped=()
for frag in "${fragments[@]}"; do
    # Reject a resolved path that leaves the repo BEFORE rustfmt is handed it.
    # `include!("../../../..//etc/passwd.rs")` is a legal relative path, so
    # discovery will faithfully resolve one if a source file spells it. Today
    # the blast radius is near nil (`--check` never writes and output goes to
    # /dev/null), so this guards a FUTURE edit -- anyone who later echoes
    # rustfmt's stderr or adds an in-place fix would otherwise turn a committed
    # hostile path into a real primitive. `physical_path` resolves without
    # requiring existence, so an escape is reported as an escape rather than
    # being reclassified as merely missing.
    abs="$(physical_path "$frag")"
    if [[ "$abs" != "$REPO_ROOT" && "$abs" != "$REPO_ROOT"/* ]]; then
        escaped+=("$frag")
        continue
    fi
    if [[ ! -f "$frag" ]]; then
        missing+=("$frag")
        continue
    fi
    if ! rustfmt --edition "$EDITION" --check "$frag" >/dev/null 2>&1; then
        failed+=("$frag")
    fi
done

printf 'fmt-fragments: checked %d include!d fragment(s)\n' "${#fragments[@]}"

if (( ${#escaped[@]} )); then
    echo
    echo "include! targets that resolve OUTSIDE the repo root -- refusing to run"
    echo "rustfmt on them. This is not a formatting problem: a source file in"
    echo "crates/ spells an include! path that escapes $REPO_ROOT."
    printf '  %s\n' "${escaped[@]}"
fi

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
    # One quoted command per path: `${failed[*]}` would flatten every path
    # into a single unquoted string, which breaks on a path with a space.
    for frag in "${failed[@]}"; do
        printf '  rustfmt --edition %s %q\n' "$EDITION" "$frag"
    done
    exit 1
fi

if (( ${#missing[@]} || ${#escaped[@]} )); then
    exit 1
fi

echo "all include!d fragments are rustfmt-clean."
exit 0
