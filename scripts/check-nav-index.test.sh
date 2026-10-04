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
# The --enforce cases plant their own allowlist beside the copied checker
# and also assert the failure MESSAGE, so each control is attributed to
# the rule it covers: a new-file control that went red only because the
# allowlist was unreadable, or a stale-entry control that went red only
# because of an unrelated gap, would otherwise read as a pass.
#
# Run it from anywhere:
#   bash scripts/check-nav-index.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$HERE/check-nav-index.sh"

fails=0

# Under the commit hook git exports GIT_DIR, GIT_INDEX_FILE and friends for
# the repo being committed. Left set, the throwaway repos' `git init` and
# `git add` below would write into THAT repo's config and index instead of
# their own, so every repo-local git variable is cleared for the whole run.
read -r -d '' -a git_local_env < <(git rev-parse --local-env-vars)
unset "${git_local_env[@]}"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CEILING_DIRECTORIES="${TMPDIR:-/tmp}"
# Only the rows that set it on purpose may pick a base revision.
unset NAV_INDEX_BASE

# Build a throwaway repo with docs/CODEMAP.md and docs/DEVELOPMENT.md
# holding the given bodies, run the checker inside it, and return its
# exit code plus captured stderr via the named refs. Any further
# arguments are passed to the checker.
run_checker() {
    local codemap_body="$1" development_body="$2" extra_setup="$3"
    local -n rc_ref="$4" err_ref="$5"
    shift 5
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
    (cd "$tmp" && bash scripts/check-nav-index.sh "$@") 2>"$errfile"
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    rc_ref=$?
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    err_ref="$(cat "$errfile")"
    chmod -R u+rwx "$tmp"
    rm -rf "$tmp" "$errfile"
}

assert_exit() {
    local desc="$1" expected_rc="$2" codemap_body="$3" development_body="$4" extra_setup="$5"
    shift 5
    local rc err
    run_checker "$codemap_body" "$development_body" "$extra_setup" rc err "$@"
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

# --- a directory find cannot read fails the run instead of shrinking the
# --- list it checks (paired control: the same tree, readable) ---

UNREADABLE_SETUP="mkdir -p crates/demo-crate/src/locked
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/locked/hidden.rs"
UNREADABLE_CODEMAP="## demo-crate

- \`src/lib.rs\` -- crate root
- \`src/locked/hidden.rs\` -- hidden"

assert_exit "a readable subdirectory is listed and checked" 0 \
    "$UNREADABLE_CODEMAP" "" "$UNREADABLE_SETUP"

if [[ "$(id -u)" -eq 0 ]]; then
    echo "SKIP: unreadable-subdirectory rows (root reads any directory)"
else
    assert_exit "an unreadable crates subdirectory is an error, not a clean run" 2 \
        "$UNREADABLE_CODEMAP" "" "$UNREADABLE_SETUP
     chmod 000 crates/demo-crate/src/locked"
    assert_exit "an unreadable scripts subdirectory is an error, not a clean run" 2 \
        "$UNREADABLE_CODEMAP" "" "$UNREADABLE_SETUP
     mkdir -p scripts/locked && chmod 000 scripts/locked"
fi

# Run the checker with --enforce and assert both its exit code and that
# its stderr carries the given fixed string (empty: no message expected).
assert_enforce() {
    local desc="$1" expected_rc="$2" expected_msg="$3" codemap_body="$4" development_body="$5" extra_setup="$6"
    local rc err
    run_checker "$codemap_body" "$development_body" "$extra_setup" rc err --enforce
    if [[ "$rc" -ne "$expected_rc" ]]; then
        echo "FAIL: $desc -- expected exit $expected_rc, got $rc" >&2
        echo "$err" >&2
        fails=$((fails + 1))
    elif [[ -n "$expected_msg" ]] && ! grep -qF -- "$expected_msg" <<<"$err"; then
        echo "FAIL: $desc -- exit $rc as expected, but stderr lacks '$expected_msg'" >&2
        echo "$err" >&2
        fails=$((fails + 1))
    else
        echo "PASS: $desc"
    fi
}

# --- --enforce: an allowlisted gap passes; a new unindexed file fails ---

ENFORCE_CODEMAP="## demo-crate

- \`src/lib.rs\` -- crate root"
ENFORCE_ALLOWLIST="printf '# header comment\\n\\ncrates/demo-crate/src/old_gap.rs\\n' >scripts/check-nav-index.allowlist"

assert_enforce "--enforce passes when every gap is allowlisted" 0 "" \
    "$ENFORCE_CODEMAP" "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/old_gap.rs
     $ENFORCE_ALLOWLIST"

assert_exit "the report mode still fails on that allowlisted gap" 1 \
    "$ENFORCE_CODEMAP" "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/old_gap.rs
     $ENFORCE_ALLOWLIST"

assert_enforce "--enforce fails on a new unindexed file outside the allowlist" 1 \
    "  crates/demo-crate/src/new_file.rs" \
    "$ENFORCE_CODEMAP" "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/old_gap.rs
     : >crates/demo-crate/src/new_file.rs
     $ENFORCE_ALLOWLIST"

assert_enforce "--enforce fails on a new unindexed script outside the allowlist" 1 \
    "  scripts/orphan.sh" \
    "$ENFORCE_CODEMAP" "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/old_gap.rs
     : >scripts/orphan.sh
     $ENFORCE_ALLOWLIST"

# --- --enforce inside a git work tree: an untracked file is not judged
# --- until it is staged (paired control on the same file) ---

ENFORCE_GIT_BASE="mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/old_gap.rs
     $ENFORCE_ALLOWLIST
     git init -q . && git add -A
     : >crates/demo-crate/src/scratch.rs"

assert_enforce "--enforce ignores an untracked unindexed file in a git tree" 0 "" \
    "$ENFORCE_CODEMAP" "" "$ENFORCE_GIT_BASE"

assert_enforce "--enforce fails once that same file is staged" 1 \
    "  crates/demo-crate/src/scratch.rs" \
    "$ENFORCE_CODEMAP" "" \
    "$ENFORCE_GIT_BASE
     git add crates/demo-crate/src/scratch.rs"

# --- --enforce: the allowlist only shrinks ---

assert_enforce "--enforce fails on an allowlisted file that gained a row" 1 \
    "now indexed; remove from the allowlist" \
    "$ENFORCE_CODEMAP
- \`src/old_gap.rs\` -- now documented" "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/old_gap.rs
     $ENFORCE_ALLOWLIST"

assert_enforce "--enforce fails on an allowlisted file that no longer exists" 1 \
    "file no longer exists; remove from the allowlist" \
    "$ENFORCE_CODEMAP" "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     $ENFORCE_ALLOWLIST"

assert_enforce "--enforce fails on an unsorted allowlist" 1 \
    "is not sorted and duplicate-free" \
    "$ENFORCE_CODEMAP" "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/old_gap.rs
     : >crates/demo-crate/src/a_gap.rs
     printf 'crates/demo-crate/src/old_gap.rs\\ncrates/demo-crate/src/a_gap.rs\\n' >scripts/check-nav-index.allowlist"

# --- --enforce against a git base: the allowlist may shrink, never grow ---

# A repo whose HEAD commit carries an allowlist of old_gap.rs and
# second_gap.rs, both genuine gaps.
GROWTH_BASE="mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs
     : >crates/demo-crate/src/old_gap.rs
     : >crates/demo-crate/src/second_gap.rs
     printf 'crates/demo-crate/src/old_gap.rs\\ncrates/demo-crate/src/second_gap.rs\\n' >scripts/check-nav-index.allowlist
     git init -q . && git add -A
     git -c user.name=t -c user.email=t@example.invalid commit -q -m base"
GROWN_ALLOWLIST="printf 'crates/demo-crate/src/new_gap.rs\\ncrates/demo-crate/src/old_gap.rs\\ncrates/demo-crate/src/second_gap.rs\\n' >scripts/check-nav-index.allowlist"

assert_enforce "--enforce passes on the base revision's own allowlist" 0 "" \
    "$ENFORCE_CODEMAP" "" "$GROWTH_BASE"

assert_enforce "--enforce fails on an allowlist line the base revision lacks" 1 \
    "not in the base revision's allowlist (HEAD)" \
    "$ENFORCE_CODEMAP" "" \
    "$GROWTH_BASE
     : >crates/demo-crate/src/new_gap.rs
     $GROWN_ALLOWLIST
     git add -A"

assert_enforce "--enforce passes when a closed gap leaves the allowlist" 0 "" \
    "$ENFORCE_CODEMAP
- \`src/second_gap.rs\` -- now documented" "" \
    "$GROWTH_BASE
     printf 'crates/demo-crate/src/old_gap.rs\\n' >scripts/check-nav-index.allowlist
     git add -A"

NAV_INDEX_BASE=HEAD^ assert_enforce "NAV_INDEX_BASE names the base the growth check compares against" 1 \
    "not in the base revision's allowlist (HEAD^)" \
    "$ENFORCE_CODEMAP" "" \
    "$GROWTH_BASE
     : >crates/demo-crate/src/new_gap.rs
     $GROWN_ALLOWLIST
     git add -A
     git -c user.name=t -c user.email=t@example.invalid commit -q -m grow"

NAV_INDEX_BASE=no-such-rev assert_enforce "an unresolvable NAV_INDEX_BASE is an error, not a skipped check" 2 \
    "NAV_INDEX_BASE=no-such-rev is not a commit" \
    "$ENFORCE_CODEMAP" "" "$GROWTH_BASE"

assert_enforce "--enforce refuses a missing allowlist rather than passing" 2 \
    "allowlist not readable" \
    "$ENFORCE_CODEMAP" "" \
    "mkdir -p crates/demo-crate/src
     : >crates/demo-crate/src/lib.rs"

assert_exit "an unknown argument is a usage error" 2 "" "" "" --bogus

# --- the checker's work dir is removed under a hostile TMPDIR ----------
# A TMPDIR holding an apostrophe and a space must not break the cleanup.
# The control runs the same checker with `rm` stubbed to a no-op, so the
# work dir is left behind: that proves the checker does create its work
# dir under the given TMPDIR, and the empty-TMPDIR verdict is not vacuous.
# Prints the number of entries the checker left in its TMPDIR.
leftover_work_dirs() {
    local stub_rm="$1" tmp hostile rc
    tmp="$(mktemp -d)"
    hostile="$tmp/it's a tmp"
    mkdir -p "$hostile" "$tmp/repo/scripts" "$tmp/repo/docs" \
        "$tmp/repo/crates/demo-crate/src" "$tmp/stubbin"
    cp "$CHECKER" "$tmp/repo/scripts/check-nav-index.sh"
    # shellcheck disable=SC2016 # the backticks are literal markdown
    printf '## demo-crate\n\n- `src/lib.rs` -- crate root\n\n- `check-nav-index.sh` -- self\n' \
        >"$tmp/repo/docs/CODEMAP.md"
    : >"$tmp/repo/docs/DEVELOPMENT.md"
    : >"$tmp/repo/crates/demo-crate/src/lib.rs"
    [[ "$stub_rm" -eq 0 ]] || printf '#!/bin/sh\nexit 0\n' >"$tmp/stubbin/rm"
    chmod +x "$tmp/stubbin/rm" 2>/dev/null
    rc=0
    (cd "$tmp/repo" && TMPDIR="$hostile" PATH="$tmp/stubbin:$PATH" \
        bash scripts/check-nav-index.sh) >/dev/null 2>&1 || rc=$?
    if [[ "$rc" -ne 0 ]]; then
        printf 'checker-exit-%s\n' "$rc"
    else
        find "$hostile" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' '
    fi
    rm -rf "$tmp"
}

left="$(leftover_work_dirs 0)"
if [[ "$left" == "0" ]]; then
    echo "PASS: the work dir is removed under a TMPDIR with an apostrophe and a space"
else
    echo "FAIL: work dir not removed under a hostile TMPDIR -- left: $left" >&2
    fails=$((fails + 1))
fi
left="$(leftover_work_dirs 1)"
if [[ "$left" == "1" ]]; then
    echo "PASS: control: with rm stubbed out the work dir is left under that TMPDIR"
else
    echo "FAIL: control: expected one work dir left with rm stubbed, got: $left" >&2
    fails=$((fails + 1))
fi

if [[ "$fails" -ne 0 ]]; then
    echo "check-nav-index self-test: $fails failure(s)" >&2
    exit 1
fi
echo "check-nav-index self-test: all assertions passed"
exit 0
