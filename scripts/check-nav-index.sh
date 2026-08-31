#!/usr/bin/env bash
# Navigation-doc coverage check: every `crates/**/*.rs` source file (minus
# the `*_tests.rs` sidecars docs/CODEMAP.md's own header already excludes)
# and every `scripts/**/*.sh` script must appear by path in docs/CODEMAP.md
# or docs/DEVELOPMENT.md -- or, for a script, in a README.md in its own
# directory, which this repo already uses as a navigation home for the
# driver, case and profile surfaces.
#
# EXISTENCE ONLY. This never checks row wording, accuracy, or freshness --
# those have no mechanical oracle and stay with human review. It exists to
# catch the case a reviewer easily misses: a whole new file that landed
# with zero mention in either navigation doc.
#
# A Rust file is matched by its path relative to its crate root (the same
# shape CODEMAP.md rows use), searched within that crate's own "## <crate>"
# section only, so a same-named file in a different crate (`src/lib.rs` is
# every crate's) cannot mask a genuine gap. A script is matched by basename
# against both docs combined, mirroring the manual rule in CODEMAP.md's
# scripts/ section preamble.
#
# Advisory, not a commit gate: a doc lag should not block every commit.
# Run it by hand or from CI as a non-blocking signal.
#
# Exit codes: 0 = every file indexed, 1 = at least one gap found,
# 2 = usage error.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CODEMAP="$REPO_ROOT/docs/CODEMAP.md"
DEVELOPMENT="$REPO_ROOT/docs/DEVELOPMENT.md"

usage() {
    echo "usage: $0" >&2
    exit 2
}

# Print the lines of $1 (a CODEMAP-shaped file) belonging to the
# "## <crate>" section named by $2, up to (excluding) the next top-level
# "## " heading.
crate_section() {
    local codemap="$1" crate="$2"
    awk -v heading="## $crate" '
        $0 == heading { found=1; next }
        found && /^## / { exit }
        found { print }
    ' "$codemap"
}

check_rust_files() {
    local crates_root="$1" codemap="$2" development="$3"
    local -n out_ref="$4"
    local path relpath crate rest section
    while IFS= read -r -d '' path; do
        relpath="${path#"$crates_root"/}"
        case "$(basename "$relpath")" in
            *_tests.rs) continue ;;
        esac
        crate="${relpath%%/*}"
        rest="${relpath#"$crate"/}"
        section="$(crate_section "$codemap" "$crate")"
        if ! grep -qF "$rest" <<<"$section" && ! grep -qF "$rest" "$development"; then
            out_ref+=("crates/$relpath")
        fi
    done < <(find "$crates_root" -type f -name '*.rs' -print0 | sort -z)
}

check_scripts() {
    local scripts_root="$1" codemap="$2" development="$3" repo_root="$4"
    local -n scripts_out_ref="$5"
    local path base readme
    while IFS= read -r -d '' path; do
        base="$(basename "$path")"
        if grep -qF "$base" "$codemap" || grep -qF "$base" "$development"; then
            continue
        fi
        # A README beside the script counts as a navigation home: this repo
        # already keeps per-directory READMEs for the driver, case and profile
        # surfaces, and one of them carries a CODEMAP row of its own. Only the
        # script's OWN directory qualifies -- a README one level up describes a
        # different surface.
        readme="$(dirname "$path")/README.md"
        if [ -r "$readme" ] && grep -qF "$base" "$readme"; then
            continue
        fi
        scripts_out_ref+=("${path#"$repo_root"/}")
    done < <(find "$scripts_root" -type f -name '*.sh' -print0 | sort -z)
}

main() {
    [[ $# -eq 0 ]] || usage
    local missing=()
    check_rust_files "$REPO_ROOT/crates" "$CODEMAP" "$DEVELOPMENT" missing
    check_scripts "$REPO_ROOT/scripts" "$CODEMAP" "$DEVELOPMENT" "$REPO_ROOT" missing
    if [[ "${#missing[@]}" -gt 0 ]]; then
        echo "check-nav-index: ${#missing[@]} file(s) with no navigation-doc row:" >&2
        printf '  %s\n' "${missing[@]}" >&2
        exit 1
    fi
    echo "check-nav-index: every crates/**/*.rs and scripts/**/*.sh is indexed"
    exit 0
}

main "$@"
