#!/usr/bin/env bash
# Self-test for check-log-display.sh. Each case builds a throwaway cargo
# workspace seeded with the REAL approved sink module, applies one edit, and
# asserts the guard's verdict. Every reject family has a paired accept case,
# so a guard that fails (or passes) everything cannot go green here.
#
# Run it from anywhere:
#   bash scripts/check-log-display.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
SINK_REL="crates/routectl-cli/src/log_sink.rs"

fails=0
LAST_OUTPUT="$(mktemp)"
trap 'rm -f "$LAST_OUTPUT"' EXIT

# Literal edit of a fixture file; fails the case setup loudly when the
# target text is absent so a drifted target cannot pass as a caught fault.
patch_file() {
    local file="$1" old="$2" new="$3" body
    body="$(<"$file")"
    if [[ "$body" != *"$old"* ]]; then
        echo "patch_file: target not found in $file: $old" >&2
        return 1
    fi
    printf '%s\n' "${body/"$old"/"$new"}" >"$file"
}

manifest() {
    local name="$1" deps="$2"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n\n%s\n' "$name" "$deps"
}

build_fixture() {
    mkdir -p scripts crates/routectl-cli/src crates/routectl-cli/tests \
        crates/routectl-core/src crates/routectl-testkit/src
    cp "$HERE/check-log-display.sh" "$HERE/check-log-display.py" scripts/
    cp "$REPO/$SINK_REL" "$SINK_REL"
    printf '[workspace]\nresolver = "2"\nmembers = ["crates/*"]\n' >Cargo.toml
    manifest routectl-cli '[dependencies]
tracing = "0.1"
tracing-subscriber = "0.3"

[dev-dependencies]
routectl-testkit = { path = "../routectl-testkit" }' >crates/routectl-cli/Cargo.toml
    manifest routectl-core '[dependencies]
tracing = "0.1"

[dev-dependencies]
routectl-testkit = { path = "../routectl-testkit" }' >crates/routectl-core/Cargo.toml
    manifest routectl-testkit '[dependencies]
tracing = "0.1"' >crates/routectl-testkit/Cargo.toml
    printf 'pub mod log_sink;\n' >crates/routectl-cli/src/lib.rs
    printf 'fn main() {\n    routectl_cli::log_sink::init();\n}\n' >crates/routectl-cli/src/main.rs
    printf 'fn t() {\n    tracing::subscriber::with_default(S, || {});\n}\n' \
        >crates/routectl-cli/tests/capture.rs
    printf '// A tracing_subscriber::fmt().init() mention in a comment is not code.\n/* nor is Dispatch in a block */\npub fn f() -> &'"'"'static str {\n    "tracing_subscriber::fmt().init()"\n}\n' \
        >crates/routectl-core/src/lib.rs
    printf 'pub fn capture() {\n    tracing::subscriber::with_default(S, || {});\n}\n' \
        >crates/routectl-testkit/src/lib.rs
}

# Build the fixture, run `setup` in it, track everything, run `POST_ADD`
# (which may drop files back out of the index), then the guard under PATH
# `guard_path`. rc 90 is a broken fixture.
run_guard() {
    local setup="$1" guard_path="${2:-\$PATH}" tmp rc
    tmp="$(mktemp -d)"
    : >"$LAST_OUTPUT"
    (
        cd "$tmp" || exit 90
        git init -q . || exit 90
        build_fixture || exit 90
        if [[ -n "$setup" ]]; then
            eval "$setup" || exit 90
        fi
        git -c core.autocrlf=false -c core.safecrlf=false add -A . 2>/dev/null || exit 90
        if [[ -n "${POST_ADD:-}" ]]; then
            eval "$POST_ADD" || exit 90
        fi
        local resolved_path
        resolved_path="$(eval printf '%s' "\"$guard_path\"")"
        PATH="$resolved_path" bash scripts/check-log-display.sh </dev/null >"$LAST_OUTPUT" 2>&1
    )
    rc=$?
    rm -rf "$tmp"
    return "$rc"
}

assert_pass() {
    local desc="$1" setup="${2:-}" rc=0
    run_guard "$setup" || rc=$?
    if [[ "$rc" -eq 0 ]]; then
        echo "PASS: accepted -- $desc"
    else
        echo "FAIL: expected PASS but got rc=$rc -- $desc"
        sed 's/^/    /' "$LAST_OUTPUT"
        fails=$((fails + 1))
    fi
}

assert_reject() {
    local desc="$1" setup="$2" needle="$3" guard_path="${4:-\$PATH}" rc=0
    run_guard "$setup" "$guard_path" || rc=$?
    if [[ "$rc" -eq 1 ]] && grep -qF -- "$needle" "$LAST_OUTPUT"; then
        echo "PASS: rejected -- $desc"
    else
        echo "FAIL: expected rc=1 naming '$needle' but got rc=$rc -- $desc"
        sed 's/^/    /' "$LAST_OUTPUT"
        fails=$((fails + 1))
    fi
}

S="$SINK_REL"
CORE=crates/routectl-core/src/lib.rs
CLI_LIB=crates/routectl-cli/src/lib.rs
INSTALL='pub fn install() {\n    tracing_subscriber::fmt().init();\n}\n'
SIDE="printf '$INSTALL' >crates/routectl-core/src/side_tests.rs"
OUTSIDE="outside $S"

assert_pass "the real sink, a test target, testkit, and comment/string mentions"

# --- inventory outside the approved module -------------------------------
assert_reject "a second subscriber in another production module" \
    "printf 'pub mod other;\n' >>$CORE && printf '$INSTALL' >crates/routectl-core/src/other.rs" \
    "$OUTSIDE"
assert_reject "an unguarded *_tests.rs sidecar" \
    "$SIDE && printf '#[path = \"side_tests.rs\"]\nmod side_tests;\n' >>$CORE" "$OUTSIDE"
assert_pass "the same sidecar behind #[cfg(test)]" \
    "$SIDE && printf '#[cfg(test)]\n#[path = \"side_tests.rs\"]\nmod side_tests;\n' >>$CORE"
assert_pass "the same sidecar behind #[cfg(all(test, unix))]" \
    "$SIDE && printf '#[cfg(all(test, unix))]\nmod side_tests;\n' >>$CORE"
assert_reject "the same sidecar behind #[cfg(any(test, unix))]" \
    "$SIDE && printf '#[cfg(any(test, unix))]\nmod side_tests;\n' >>$CORE" "$OUTSIDE"
assert_pass "an inline #[cfg(test)] module using a capture subscriber" \
    "printf '#[cfg(test)]\nmod tests {\n    fn t() { tracing::subscriber::with_default(S, || {}); }\n}\n' >>$CORE"
assert_reject "a subscriber after an inline cfg(test) module closes" \
    "printf '#[cfg(test)]\nmod tests {\n    fn t() {}\n}\npub fn g() { tracing::subscriber::with_default(S, || {}); }\n' >>$CORE" \
    "$OUTSIDE"
assert_reject "a tracked file no target reaches" \
    "printf '$INSTALL' >crates/routectl-core/src/orphan.rs" "no target reaches"
assert_reject "an aliased subscriber crate import" \
    "printf 'use tracing_subscriber as ts;\n' >>$CORE" "$OUTSIDE"
assert_reject "fmt::init, try_init, and FmtSubscriber outside the sink" \
    "printf 'pub fn a() { tracing_subscriber::fmt::init(); }\npub fn b(s: FmtSubscriber) { s.try_init(); }\n' >>$CORE" \
    "$OUTSIDE"
assert_reject "a global default through tracing::dispatcher" \
    "printf 'pub fn g(d: D) { tracing::dispatcher::set_global_default(d).ok(); }\n' >>$CORE" \
    "$OUTSIDE"
assert_reject "a hand-written Subscriber impl" \
    "printf 'impl tracing::Subscriber for Sink {}\n' >>$CORE" "$OUTSIDE"
assert_reject "a test file declared as a production [[bin]]" \
    "printf '\n[[bin]]\nname = \"capture\"\npath = \"tests/capture.rs\"\n' >>crates/routectl-cli/Cargo.toml" \
    "$OUTSIDE"
assert_reject "CRLF line endings do not hide a production subscriber" \
    "printf 'pub mod other;\r\n' >>$CORE && printf 'pub fn i() {\r\n    tracing_subscriber::fmt().init();\r\n}\r\n' >crates/routectl-core/src/other.rs" \
    "$OUTSIDE"

# --- include! and #[path] ---------------------------------------------------
for form in '("frag.rs")' '["frag.rs"]' '{"frag.rs"}'; do
    assert_reject "a production include!$form of a subscriber" \
        "printf '$INSTALL' >crates/routectl-core/src/frag.rs && printf 'include!$form;\n' >>$CORE" \
        "$OUTSIDE"
done
assert_pass "a test-scoped include! of a subscriber" \
    "printf '$INSTALL' >crates/routectl-core/src/frag.rs && printf '#[cfg(test)]\nmod t {\n    include!(\"frag.rs\");\n}\n' >>$CORE"
assert_reject "a non-literal include!" \
    "printf 'include!(concat!(\"fr\", \"ag.rs\"));\n' >>$CORE" "not a string literal"
assert_reject "an include! of a git-ignored file" \
    "printf 'include!(\"gen.rs\");\n' >>$CORE && printf 'pub fn g() {}\n' >crates/routectl-core/src/gen.rs && printf 'gen.rs\n' >crates/routectl-core/src/.gitignore" \
    "is ignored or outside the repo"
assert_reject "an include! of a missing (generated) file" \
    "printf 'include!(\"generated.rs\");\n' >>$CORE" "is missing"
assert_reject "a #[cfg_attr(.., path)] module" \
    "printf 'pub fn g() {}\n' >crates/routectl-core/src/alt.rs && printf '#[cfg_attr(unix, path = \"alt.rs\")]\nmod alt;\n' >>$CORE" \
    "cfg_attr"

# --- manifests (cargo metadata) --------------------------------------------
assert_reject "a table-form renamed tracing-subscriber dependency" \
    "printf '\n[dependencies.sink]\npackage = \"tracing-subscriber\"\nversion = \"0.3\"\n' >>crates/routectl-core/Cargo.toml" \
    "renamed dependency"
assert_reject "an inline-form renamed tracing-subscriber dev-dependency" \
    "patch_file crates/routectl-core/Cargo.toml '[dev-dependencies]' '[dev-dependencies]
ts = { package = \"tracing-subscriber\", version = \"0.3\" }'" "renamed dependency"
assert_reject "tracing-subscriber as a production dependency of another crate" \
    "patch_file crates/routectl-core/Cargo.toml 'tracing = \"0.1\"' 'tracing = \"0.1\"
tracing-subscriber = \"0.3\"'" "outside the log-sink allowlist"
assert_pass "tracing-subscriber as a dev-dependency of another crate" \
    "printf 'tracing-subscriber = \"0.3\"\n' >>crates/routectl-core/Cargo.toml"
assert_reject "routectl-testkit as a normal dependency" \
    "patch_file crates/routectl-core/Cargo.toml '[dependencies]' '[dependencies]
routectl-testkit = { path = \"../routectl-testkit\" }'" "is a normal dependency"
assert_reject "routectl-testkit as a build dependency" \
    "printf '\n[build-dependencies]\nroutectl-testkit = { path = \"../routectl-testkit\" }\n' >>crates/routectl-core/Cargo.toml" \
    "is a build dependency"

# --- the approved module itself ---------------------------------------------
assert_pass "the sink with CRLF line endings" "sed -i 's/\$/\r/' $S"
assert_reject "the escaping formatter removed from the builder" \
    "patch_file $S '        .fmt_fields(EscapingFields)
' ''" "differs from the pinned chain"
assert_reject "the escaping formatter swapped for DefaultFields" \
    "patch_file $S '.fmt_fields(EscapingFields)' '.fmt_fields(DefaultFields::new())'" \
    "differs from the pinned chain"
assert_reject "an appended builder method that replaces event formatting" \
    "patch_file $S '        .with_writer(writer);' '        .with_writer(writer)
        .event_format(tracing_subscriber::fmt::format().json());'" "differs from the pinned chain"
assert_reject "a formatter swap on only one clock arm" \
    "patch_file $S 'Box::new(configured.without_time().finish())' 'Box::new(configured.without_time().fmt_fields(DefaultFields::new()).finish())'" \
    "differs from the pinned chain"
assert_reject "an aliased constructor in the sink module" \
    "patch_file $S 'use tracing_subscriber::EnvFilter;' 'use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt as build_fmt;' && printf 'fn second() { let _ = build_fmt().finish(); }\n' >>$S" \
    "imports differ from the pinned set"
assert_reject "a pub re-export from the sink module" \
    "printf 'pub use tracing_subscriber::fmt as sink_fmt;\n' >>$S" "imports differ"
assert_reject "a second fmt() constructor in the sink module" \
    "printf 'fn second() { let _ = tracing_subscriber::fmt().finish(); }\n' >>$S" \
    "references the fmt() constructor 2 times"
assert_reject "tracing_subscriber::fmt::init in the sink module" \
    "printf 'fn again() { tracing_subscriber::fmt::init(); }\n' >>$S" "names init 3 times"
assert_reject "a second global install in the sink module" \
    "printf 'fn again() { subscriber(EnvFilter::new(\"x\"), std::io::stderr, false, Clock::Wall).init(); }\n' >>$S" \
    "calls .init 2 times"
assert_reject "the install kept only as a comment" \
    "patch_file $S '    subscriber(filter, std::io::stderr, ansi, Clock::Wall).init();' '    // subscriber(filter, std::io::stderr, ansi, Clock::Wall).init();'" \
    "must install exactly once"
assert_pass "a comment-only .init() mention in the sink module" \
    "printf '// Nothing here calls .init() or tracing_subscriber::fmt() again.\n' >>$S"
assert_reject "a registry or layer in the sink module" \
    "printf 'fn layered() { let _ = tracing_subscriber::registry(); }\n' >>$S" "forbidden names"
assert_reject "the sink module no longer reached from any target" \
    "patch_file $CLI_LIB 'pub mod log_sink;' ''" "no target reaches"
assert_reject "the sink module deleted" "rm $S" "is not in the inventory"

# --- files the index does not hold yet -----------------------------------
# A squash-merge or partial-commit index can lack a file the work tree
# already compiles; the inventory must still see and scan it.
UNSTAGE_SINK="git rm -q --cached $S"
POST_ADD="$UNSTAGE_SINK" \
    assert_pass "the approved sink present in the work tree before its first commit"
POST_ADD="$UNSTAGE_SINK" \
    assert_reject "an uncommitted approved sink is still pinned" \
    "patch_file $S '.fmt_fields(EscapingFields)' '.fmt_fields(DefaultFields::new())'" \
    "differs from the pinned chain"
POST_ADD="git rm -q --cached crates/routectl-core/src/other.rs" \
    assert_reject "an untracked second subscriber in a production module" \
    "printf 'pub mod other;\n' >>$CORE && printf '$INSTALL' >crates/routectl-core/src/other.rs" \
    "$OUTSIDE"
POST_ADD="git rm -q --cached crates/routectl-core/src/orphan.rs" \
    assert_reject "an untracked subscriber file no target reaches" \
    "printf '$INSTALL' >crates/routectl-core/src/orphan.rs" "no target reaches"

# --- fail-closed ------------------------------------------------------------
assert_reject "a tracked file that is not UTF-8" \
    "printf 'pub mod bad;\n' >>$CORE && printf 'fn f() {}\n// \xff\n' >crates/routectl-core/src/bad.rs" \
    "not valid UTF-8"
POST_ADD="rm crates/routectl-cli/tests/capture.rs" \
    assert_reject "a tracked file missing from the work tree" "" "cannot read inventoried file"
POST_ADD='rm -rf .git' \
    assert_reject "a work tree that is not a git repo" "" "not inside a git work tree"
assert_reject "a manifest cargo cannot load" \
    "printf 'this is not toml\n' >>crates/routectl-core/Cargo.toml" "cargo metadata failed"
assert_reject "a module that resolves to no file" \
    "printf 'mod ghost;\n' >>$CORE" "resolves to 0 inventoried files"
assert_reject "a file module declared inside an include!d fragment" \
    "printf 'mod nested;\n' >crates/routectl-core/src/frag.rs && printf 'pub fn n() {}\n' >crates/routectl-core/src/nested.rs && printf 'include!(\"frag.rs\");\n' >>$CORE" \
    "inside an include!d file"
# shellcheck disable=SC2016 # the stub PATH expands inside the throwaway repo
assert_reject "a missing python3" \
    'mkdir -p stubbin
     for tool in bash git cargo mktemp rm dirname; do ln -sf "$(command -v "$tool")" "stubbin/$tool"; done' \
    "required tool 'python3' not found" '$PWD/stubbin'

if [[ "$fails" -ne 0 ]]; then
    echo "check-log-display.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "check-log-display.test.sh: all assertions passed"
