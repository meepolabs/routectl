#!/usr/bin/env bash
# Self-test for check-nav-index.sh. Exits 0 when all assertions pass,
# non-zero on the first failure.
#
# The checker reads the tree it runs in, so every case builds a throwaway
# repo (its own crates/, scripts/, docs/CODEMAP.md, docs/DEVELOPMENT.md)
# and runs the checker inside it -- never against this repo's own docs.
# Every "passes" assertion is paired with a control proving the same
# checker call FAILS once the file it covers is planted unindexed, so a
# checker that only ever reports clean cannot slip through unnoticed.
#
# Run it from anywhere:
#   bash scripts/check-nav-index.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$HERE/check-nav-index.sh"

fails=0

# Build a throwaway repo with docs/CODEMAP.md and docs/DEVELOPMENT.md
# holding the given bodies, run the checker inside it, and return its
# exit code plus captured stderr via the named refs.
run_checker() {
    local codemap_body="$1" development_body="$2" extra_setup="$3"
    local -n rc_ref="$4" err_ref="$5"
    local tmp
    tmp="$(mktemp -d)"
    (
        cd "$tmp" || exit 2
        mkdir -p scripts docs crates
        cp "$CHECKER" scripts/check-nav-index.sh
        printf '%s\n\n- `check-nav-index.sh` -- self\n' "$codemap_body" >docs/CODEMAP.md
        printf '%s\n' "$development_body" >docs/DEVELOPMENT.md
        eval "$extra_setup"
    )
    local errfile
    errfile="$(mktemp)"
    (cd "$tmp" && bash scripts/check-nav-index.sh) 2>"$errfile"
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    rc_ref=$?
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    err_ref="$(cat "$errfile")"
    rm -rf "$tmp" "$errfile"
}

assert_exit() {
    local desc="$1" expected_rc="$2" codemap_body="$3" development_body="$4" extra_setup="$5"
    local rc err
    run_checker "$codemap_body" "$development_body" "$extra_setup" rc err
    if [[ "$rc" -eq "$expected_rc" ]]; then
        echo "PASS: $desc"
    else
        echo "FAIL: $desc -- expected exit $expected_rc, got $rc" >&2
        echo "$err" >&2
        fails=$((fails + 1))
    fi
}

# --- indexed rust file passes, unindexed sibling fails (paired control) ---

assert_exit "indexed crate file passes" 0 \
    "## demo-crate

- \`src/lib.rs\` -- crate root" \
    "" \
    "mkdir -p crates/demo-crate/src && : >crates/demo-crate/src/lib.rs"

assert_exit "unindexed crate file fails" 1 \
    "## demo-crate

- \`src/lib.rs\` -- crate root" \
    "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/unmapped.rs"

# --- crate-section scoping: a match in the WRONG crate's section still fails ---

assert_exit "same relpath named only in a different crate's section still fails" 1 \
    "## other-crate

- \`src/lib.rs\` -- crate root

## demo-crate

- \`src/main.rs\` -- entry point" \
    "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/main.rs
     : >crates/demo-crate/src/lib.rs"

# --- *_tests.rs sidecars are excluded regardless of indexing ---

assert_exit "unindexed _tests.rs sidecar does not fail the run" 0 \
    "## demo-crate

- \`src/lib.rs\` -- crate root" \
    "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/lib_tests.rs"

# --- DEVELOPMENT.md is an equally valid home for a crate file ---

assert_exit "file named only in DEVELOPMENT.md passes" 0 \
    "## demo-crate

- \`src/lib.rs\` -- crate root" \
    "see crates/demo-crate/src/helper.rs for the helper" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/helper.rs"

# --- scripts are matched by basename against either doc ---

assert_exit "script named in CODEMAP.md by basename passes" 0 \
    "## scripts/

- \`build.sh\` -- local image build" \
    "" \
    ": >scripts/build.sh"

assert_exit "script named in DEVELOPMENT.md by basename passes" 0 \
    "" \
    "bash scripts/bootstrap.sh" \
    ": >scripts/bootstrap.sh"

assert_exit "unindexed script fails" 1 \
    "" \
    "" \
    ": >scripts/orphan.sh"

# --- a README in the script's OWN directory is a navigation home; one a
# --- level up is not (paired control on the directory boundary) ---

assert_exit "script named in a README beside it passes" 0 \
    "" \
    "" \
    "mkdir -p scripts/tool
     : >scripts/tool/helper.sh
     echo 'helper.sh -- the helper' >scripts/tool/README.md"

assert_exit "script named only in a README one level up still fails" 1 \
    "" \
    "" \
    "mkdir -p scripts/tool
     : >scripts/tool/helper.sh
     echo 'helper.sh -- the helper' >scripts/README.md"

if [[ "$fails" -ne 0 ]]; then
    echo "check-nav-index self-test: $fails failure(s)" >&2
    exit 1
fi
echo "check-nav-index self-test: all assertions passed"
exit 0
