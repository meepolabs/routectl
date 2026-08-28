#!/usr/bin/env bash
# Self-test for promote_fixture.sh. Exits 0 when all assertions pass,
# non-zero on the first failure.
#
# Every case drives the REAL script inside a throwaway repo carrying the
# REAL confinement library and the REAL scrub gate, against synthetic
# fixture directories under a throwaway scratch root. Nothing here is a
# stub of the logic under test: the whole point of the script is that it
# does not re-implement confinement and does not skip the scrub gate, and
# a copied-in fake of either would assert nothing.
#
# The environment is part of the fixture. The scrub gate derives its deny
# set from `$HOME`, the git identity, the hostname and the seat store, so
# every invocation runs under a fake home, a repo-local git identity, a
# `hostname` stub and a throwaway XDG -- otherwise whoever runs the suite
# decides whether a case passes.
#
# Both directions are pinned. The refusal assertions alone would pass
# against a script that refuses everything, so each one is paired with a
# clean control that must promote at exit 0.
#
# Requires python3 (the scrub gate does, for header JSON).
#
# Run it from anywhere:
#   bash scripts/promote_fixture.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROMOTE="$HERE/promote_fixture.sh"
SCRUB="$HERE/scrub-fixture.sh"
CONFINE="$HERE/drivers/lib/confine.sh"

fails=0

# The identities the throwaway environment reports. Deliberately not the
# operator's real ones, and deliberately absent from every fixture body
# below so a clean control stays clean on any box.
FAKE_GIT_NAME="Ada Contributor"
FAKE_GIT_EMAIL="ada@example.invalid"
FAKE_HOSTNAME="devbox-17"

# Corpus root inside a throwaway repo -- the script's own default, which
# it also confines `--to` against.
CORPUS_REL="crates/routectl-cli/tests/fixtures/driver"

if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 not found; this self-test cannot exercise the scrub gate"
    exit 1
fi

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- expected '$expected', got '$actual'"
        fails=$((fails + 1))
    fi
}

check_log() {
    local label="$1" needle="$2" file="$3"
    if grep -qF -- "$needle" "$file"; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- '$needle' absent from $file"
        sed -n '1,20p' "$file"
        fails=$((fails + 1))
    fi
}

# Build a throwaway repo plus a sibling scratch root, and echo the work
# directory. The repo carries the real scripts because the promotion path
# sources one and shells out to the other; a repo missing either is its
# own separate case below.
make_work() {
    local work
    work="$(mktemp -d)"
    mkdir -p "$work/repo/scripts/drivers/lib" \
        "$work/repo/$CORPUS_REL" \
        "$work/scratch" \
        "$work/home/acontributor" \
        "$work/stubbin" \
        "$work/xdg/routectl"
    cp "$PROMOTE" "$work/repo/scripts/promote_fixture.sh"
    cp "$CONFINE" "$work/repo/scripts/drivers/lib/confine.sh"
    cp "$SCRUB" "$work/repo/scripts/scrub-fixture.sh"
    printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$FAKE_HOSTNAME" >"$work/stubbin/hostname"
    chmod +x "$work/stubbin/hostname"
    (
        cd "$work/repo" || exit 2
        git init -q .
        git config user.name "$FAKE_GIT_NAME"
        git config user.email "$FAKE_GIT_EMAIL"
    ) >/dev/null 2>&1
    printf '%s\n' "$work"
}

# Run the real promotion script from inside the throwaway repo. Returns
# its exit status; stdout+stderr land in `<work>/promote.log`, truncated
# per run, so a case can assert on the refusal message.
promote() {
    local work="$1"
    shift
    local rc=0
    (
        cd "$work/repo" || exit 2
        HOME="$work/home/acontributor" \
            XDG_CONFIG_HOME="$work/xdg" \
            PATH="$work/stubbin:$PATH" \
            bash scripts/promote_fixture.sh "$@"
    ) >"$work/promote.log" 2>&1 || rc=$?
    return "$rc"
}

# Write a fixture directory with the given files. `$1` is the directory,
# then NAME=CONTENT pairs. The file SET is what matters to most cases
# below -- file presence is part of the fixture schema, so a promotion
# that merges is visible as an extra file rather than as changed bytes.
mk_fixture() {
    local dir="$1"
    shift
    mkdir -p "$dir"
    local pair
    for pair in "$@"; do
        printf '%s' "${pair#*=}" >"$dir/${pair%%=*}"
    done
}

# A stable manifest of a directory tree: every file's relative path and a
# checksum of its bytes, sorted. Two trees with the same manifest hold the
# same file set with the same content, which is what "byte-identical" and
# "the destination matches the source" mean below.
tree_manifest() {
    local dir="$1"
    [ -d "$dir" ] || { printf 'ABSENT\n'; return 0; }
    (
        cd "$dir" || exit 2
        find . -type f | sort | while IFS= read -r f; do
            printf '%s %s\n' "$f" "$(cksum <"$f" | awk '{print $1 "-" $2}')"
        done
    )
}

# Relative paths of the files in a tree, sorted, on one line.
tree_files() {
    local dir="$1"
    [ -d "$dir" ] || { printf 'ABSENT\n'; return 0; }
    (cd "$dir" && find . -type f | sed 's|^\./||' | sort | tr '\n' ' ')
}

# Count of leftover staging / rename-aside directories in a corpus root.
# A refusal that leaves one behind has published a half-promotion into the
# committed tree under a name the rig's sweep would later delete silently.
tmp_dirs_in() {
    local corpus="$1"
    find "$corpus" -maxdepth 1 -name '.tmp.*' | wc -l | tr -d ' '
}

# --- Case 1: promotion over an EXISTING fixture replaces, never merges ---
# The load-bearing assertion of the whole script. `only_in_old.json` is
# present ONLY in the destination's previous content; a plain `mv` over an
# existing directory leaves it there, and file presence IS the fixture
# schema, so a reader would then take a non-stream verdict off a fixture
# no capture produced.
work="$(make_work)"
scratch="$work/scratch"
corpus="$work/repo/$CORPUS_REL"
mk_fixture "$scratch/anthropic-api/plain-turn-01" \
    'meta.json={"case_id":"plain-turn-01"}' \
    'ingress_request.json={"model":"claude-sonnet-4-5"}' \
    'egress_response.json={"id":"msg_new"}'
mk_fixture "$corpus/anthropic-api/plain-turn-01" \
    'meta.json={"case_id":"plain-turn-01","stale":true}' \
    'ingress_request.json={"model":"old"}' \
    'only_in_old.json={"from":"a previous capture"}'
src_manifest="$(tree_manifest "$scratch/anthropic-api/plain-turn-01")"
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "promotion over an existing fixture exits 0" "0" "$rc"
check "a file present only in the OLD fixture is gone after promotion" "0" \
    "$([ -e "$corpus/anthropic-api/plain-turn-01/only_in_old.json" ] && echo 1 || echo 0)"
check "the destination holds exactly the source's file set" \
    "egress_response.json ingress_request.json meta.json " \
    "$(tree_files "$corpus/anthropic-api/plain-turn-01")"
check "the destination content matches the source byte for byte" \
    "$src_manifest" "$(tree_manifest "$corpus/anthropic-api/plain-turn-01")"
check "no rename-aside directory survives the promotion" "0" "$(tmp_dirs_in "$corpus")"
check "the source fixture is left in scratch" "1" \
    "$([ -d "$scratch/anthropic-api/plain-turn-01" ] && echo 1 || echo 0)"
rm -rf "$work"

# --- Case 2: PAIRED CONTROL -- a clean fixture promotes onto a fresh path ---
# Without this the refusal cases below are satisfiable by a script that
# refuses everything.
work="$(make_work)"
scratch="$work/scratch"
corpus="$work/repo/$CORPUS_REL"
mk_fixture "$scratch/anthropic-api/plain-turn-01" \
    'meta.json={"case_id":"plain-turn-01"}' \
    'ingress_request.headers.json=[["content-type","application/json"]]'
src_manifest="$(tree_manifest "$scratch/anthropic-api/plain-turn-01")"
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "a clean fixture promotes onto a fresh destination at exit 0" "0" "$rc"
check "the fresh destination matches the source" \
    "$src_manifest" "$(tree_manifest "$corpus/anthropic-api/plain-turn-01")"
check_log "the success line names both paths" \
    "promoted $scratch/anthropic-api/plain-turn-01 -> $corpus/anthropic-api/plain-turn-01" \
    "$work/promote.log"
check "the lane directory was created under the corpus" "1" \
    "$([ -d "$corpus/anthropic-api" ] && echo 1 || echo 0)"
rm -rf "$work"

# --- Case 3: a DIRTY fixture is refused, absent destination stays absent ---
# A `/home/<other>` path is the class a hand-edited scratch fixture most
# plausibly carries: an agent transcript quoting a third party's file.
work="$(make_work)"
scratch="$work/scratch"
corpus="$work/repo/$CORPUS_REL"
mk_fixture "$scratch/anthropic-api/plain-turn-01" \
    'meta.json={"case_id":"plain-turn-01"}' \
    'ingress_request.json={"text":"see /home/someoneelse/notes.md"}'
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "a fixture carrying a foreign home path is refused with exit 1" "1" "$rc"
check_log "the refusal names the scrub gate as the reason" \
    "did not pass the scrub gate" "$work/promote.log"
check_log "the refusal names the offending class" "home-prefix" "$work/promote.log"
check "a destination that did not exist is still absent after a refusal" "ABSENT" \
    "$(tree_manifest "$corpus/anthropic-api/plain-turn-01")"
check "no staging directory is left in the corpus after a refusal" "0" "$(tmp_dirs_in "$corpus")"
rm -rf "$work"

# --- Case 4: a DIRTY fixture leaves an EXISTING destination byte-identical ---
work="$(make_work)"
scratch="$work/scratch"
corpus="$work/repo/$CORPUS_REL"
# Key-shaped content is ASSEMBLED at runtime from a prefix plus an opaque
# run, so no line of this file holds a full credential shape for the
# repo's own staged-secret hook to flag.
FAKE_RUN="$(printf 'A%.0s' $(seq 1 40))"
fake_key() { printf '%s%s' "$1" "${2:-$FAKE_RUN}"; }
mk_fixture "$scratch/anthropic-api/plain-turn-01" \
    'meta.json={"case_id":"plain-turn-01"}' \
    "ingress_request.json={\"text\":\"ANTHROPIC_API_KEY=$(fake_key 'sk-ant-api03-')\"}"
mk_fixture "$corpus/anthropic-api/plain-turn-01" \
    'meta.json={"case_id":"plain-turn-01","generation":"first"}' \
    'ingress_request.json={"model":"claude-sonnet-4-5"}'
before="$(tree_manifest "$corpus/anthropic-api/plain-turn-01")"
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "a fixture carrying a raw provider key is refused with exit 1" "1" "$rc"
check_log "the refusal names the provider-key class" "provider-key" "$work/promote.log"
check "an existing destination is byte-identical after a refusal" \
    "$before" "$(tree_manifest "$corpus/anthropic-api/plain-turn-01")"
check "the refusal says the destination is untouched" "1" \
    "$(grep -c 'is untouched' "$work/promote.log")"
rm -rf "$work"

# --- Case 5: the scrub gate's own failure is NOT flattened into "dirty" ---
# Exit 2 from the gate means it could not run at all. A caller that saw 1
# would go hunting for personal data in a fixture that was never scanned.
work="$(make_work)"
scratch="$work/scratch"
mkdir -p "$scratch/anthropic-api/plain-turn-01"
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "a fixture the gate cannot scan at all is refused with exit 2, not 1" "2" "$rc"
check_log "the unscannable fixture refusal comes from the gate" \
    "refusing a vacuous scan" "$work/promote.log"
rm -rf "$work"

# --- Case 6: confinement of --from, delegated to the shared library ----
work="$(make_work)"
scratch="$work/scratch"
corpus="$work/repo/$CORPUS_REL"
mk_fixture "$work/elsewhere/anthropic-api/plain-turn-01" \
    'meta.json={"case_id":"plain-turn-01"}'
rc=0
promote "$work" --from "$work/elsewhere/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "a --from outside the scratch root is refused with exit 2" "2" "$rc"
check "nothing landed in the corpus from an out-of-root source" "ABSENT" \
    "$(tree_manifest "$corpus/anthropic-api")"

# The same source reached by a `..` traversal spelled under the root.
rc=0
promote "$work" --from "$scratch/../elsewhere/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "a --from escaping the scratch root via .. is refused with exit 2" "2" "$rc"

# A symlinked lane component pointing out of the scratch root.
mkdir -p "$work/outside-lane/plain-turn-01"
printf '%s' '{}' >"$work/outside-lane/plain-turn-01/meta.json"
ln -s "$work/outside-lane" "$scratch/linked-lane"
rc=0
promote "$work" --from "$scratch/linked-lane/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "a --from with a symlinked component under the root is refused" "2" "$rc"
check_log "the symlink refusal names the component" "symlink component" "$work/promote.log"

# A DANGLING symlink component: physical resolution walks up to the
# nearest EXISTING ancestor, so a broken link slips past `cd -P` and only
# the library's per-component `[ -L ]` walk sees it.
ln -s "$work/no-such-target" "$scratch/dangling-lane"
rc=0
promote "$work" --from "$scratch/dangling-lane/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "a --from with a DANGLING symlink component is refused" "2" "$rc"
check_log "the dangling-symlink refusal names the component" \
    "symlink component" "$work/promote.log"
rm -rf "$work"

# --- Case 7: confinement of --to, against the corpus default ----------
work="$(make_work)"
scratch="$work/scratch"
mk_fixture "$scratch/anthropic-api/plain-turn-01" \
    'meta.json={"case_id":"plain-turn-01"}'
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" --to "$work/repo/src" || rc=$?
check "a --to outside the corpus root is refused with exit 2" "2" "$rc"
check "nothing was written to the out-of-corpus destination" "0" \
    "$([ -e "$work/repo/src" ] && echo 1 || echo 0)"

rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" \
    --to "$work/repo/$CORPUS_REL/../../../../.." || rc=$?
check "a --to escaping the corpus root via .. is refused with exit 2" "2" "$rc"

# A symlinked component INSIDE the corpus tree redirects the landing
# rename out of the corpus, so the destination path is confined too even
# though it is derived rather than passed in.
mkdir -p "$work/repo/$CORPUS_REL"
ln -s "$work/elsewhere-corpus" "$work/repo/$CORPUS_REL/anthropic-api"
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "a symlinked LANE directory in the corpus is refused" "2" "$rc"
check_log "the corpus symlink refusal names the component" \
    "symlink component" "$work/promote.log"
rm -rf "$work"

# --- Case 8: --from must name a fixture, not a lane or a root ----------
# A REL that collapses to fewer than two components would aim the
# rename-aside at a lane directory or at the corpus root itself.
work="$(make_work)"
scratch="$work/scratch"
corpus="$work/repo/$CORPUS_REL"
mk_fixture "$scratch/anthropic-api/plain-turn-01" \
    'meta.json={"case_id":"plain-turn-01"}'
rc=0
promote "$work" --from "$scratch/anthropic-api" --scratch-root "$scratch" || rc=$?
check "a --from naming a LANE directory is refused with exit 2" "2" "$rc"
check_log "the lane-directory refusal states the required shape" \
    "<scratch-root>/<lane>/<case-id>" "$work/promote.log"
check "nothing landed in the corpus from a lane-directory source" "ABSENT" \
    "$(tree_manifest "$corpus/anthropic-api")"

rc=0
promote "$work" --from "$scratch" --scratch-root "$scratch" || rc=$?
check "a --from naming the scratch root itself is refused with exit 2" "2" "$rc"

mk_fixture "$scratch/anthropic-api/plain-turn-01/nested" 'meta.json={}'
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01/nested" \
    --scratch-root "$scratch" || rc=$?
check "a --from three components deep is refused with exit 2" "2" "$rc"
rm -rf "$work"

# --- Case 9: usage errors and missing prerequisites -------------------
work="$(make_work)"
scratch="$work/scratch"
mk_fixture "$scratch/anthropic-api/plain-turn-01" 'meta.json={}'
rc=0
promote "$work" --scratch-root "$scratch" || rc=$?
check "a missing --from is a usage error" "2" "$rc"
check_log "the usage error names --from" "--from is required" "$work/promote.log"

rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" || rc=$?
check "a missing --scratch-root is a usage error" "2" "$rc"
check_log "the usage error names --scratch-root" "--scratch-root is required" \
    "$work/promote.log"

rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" --nope || rc=$?
check "an unknown flag is a usage error" "2" "$rc"

rc=0
promote "$work" --from "$scratch/anthropic-api/no-such-case" \
    --scratch-root "$scratch" || rc=$?
check "a --from that does not exist is refused with exit 2" "2" "$rc"

# The library and the gate are both hard prerequisites: an absent one is
# never a silently unconfined or unscanned promotion.
rm -f "$work/repo/scripts/drivers/lib/confine.sh"
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "an absent confinement library refuses the promotion" "2" "$rc"
check_log "the refusal names the missing library" "confinement library not found" \
    "$work/promote.log"
cp "$CONFINE" "$work/repo/scripts/drivers/lib/confine.sh"

rm -f "$work/repo/scripts/scrub-fixture.sh"
rc=0
promote "$work" --from "$scratch/anthropic-api/plain-turn-01" \
    --scratch-root "$scratch" || rc=$?
check "an absent scrub gate refuses the promotion" "2" "$rc"
check_log "the refusal names the missing gate" "scrub gate not found" \
    "$work/promote.log"
rm -rf "$work"

# --- Case 10: --help renders the header -------------------------------
help_out="$(bash "$PROMOTE" --help 2>&1 || true)"
if printf '%s' "$help_out" | grep -q -- '--scratch-root' &&
    ! printf '%s' "$help_out" | grep -q 'END USAGE'; then
    echo "PASS: --help renders the header including --scratch-root"
else
    echo "FAIL: --help output is truncated or leaks the sentinel"
    printf '%s\n' "$help_out" | tail -5
    fails=$((fails + 1))
fi

# --- Case 11: the confinement logic exists in exactly ONE place --------
# The resolution pair and the per-component symlink walk encode three
# separately-discovered subtleties. A second copy is a path-traversal
# surface that drifts from the first, so assert this script CALLS them and
# defines neither -- with a positive control proving the matcher fires on
# the file that does define them.
check "promote_fixture.sh defines no abspath_lexical" "0" \
    "$(grep -c '^abspath_lexical()' "$PROMOTE")"
check "promote_fixture.sh defines no abspath_physical" "0" \
    "$(grep -c '^abspath_physical()' "$PROMOTE")"
check "promote_fixture.sh defines no confine_out_under" "0" \
    "$(grep -c '^confine_out_under()' "$PROMOTE")"
check "promote_fixture.sh runs no per-component symlink test" "0" \
    "$(grep -c '\[ -L ' "$PROMOTE")"
check "positive control: the library DOES define abspath_lexical" "1" \
    "$(grep -c '^abspath_lexical()' "$CONFINE")"
check "positive control: the library DOES define abspath_physical" "1" \
    "$(grep -c '^abspath_physical()' "$CONFINE")"
check "positive control: the library DOES run the symlink test" "1" \
    "$(grep -q '\[ -L ' "$CONFINE" && echo 1 || echo 0)"
check "promote_fixture.sh sources the shared library" "1" \
    "$(grep -c 'drivers/lib/confine.sh"$' "$PROMOTE")"

if [ "$fails" -gt 0 ]; then
    echo "promote_fixture self-test: $fails failure(s)"
    exit 1
fi
echo "promote_fixture self-test: all assertions passed"
