#!/usr/bin/env bash
# Self-test for the gate registry's public-api subcommand and the pre-push
# leg that reaches it. Runs no cargo and no cargo-public-api.
#
# Pins:
#   - `test-gate.sh --print public-api` is exactly the public-api.sh
#     --check over every crate, and the subcommand refuses extra arguments;
#   - the pre-push leg runs that subcommand only when cargo-public-api at
#     the pinned version AND the pinned nightly are installed, and
#     propagates its failure, so a stale baseline fails the push;
#   - each missing piece of tooling makes the leg print the one skip line
#     and exit 0 without reaching the registry;
#   - a pin public-api.sh no longer carries fails the leg instead of
#     skipping it.
#
# The leg is driven from a scratch copy of scripts/ whose test-gate.sh is a
# stub recording its argv, with stub cargo-public-api, rustup, and rustup's
# cargo / rustdoc / rustc proxies on a PATH that holds only them and the
# system directories, so the caller's own toolchain never decides a verdict. Every skip case is paired with the
# run case it differs from by one stub.
#
# Run it from anywhere:
#   bash scripts/test-gate.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REGISTRY="$HERE/test-gate.sh"
LEG="$HERE/public-api-pre-push.sh"
PUBLIC_API="$HERE/public-api.sh"
SYSTEM_PATH=/usr/bin:/bin

fails=0
pass() { echo "PASS: $*"; }
fail() {
    echo "FAIL: $*"
    fails=$((fails + 1))
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

NIGHTLY="$(grep -oE '^PUBLIC_API_NIGHTLY=.*' "$PUBLIC_API" | cut -d= -f2)"
VERSION="$(grep -oE 'cargo-public-api --version [0-9]+\.[0-9]+\.[0-9]+' "$PUBLIC_API" \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
if [[ -z "$NIGHTLY" || -z "$VERSION" ]]; then
    echo "test-gate.test.sh: cannot read the pins from $PUBLIC_API" >&2
    exit 1
fi

# --- registry ---------------------------------------------------------------

got="$(bash "$REGISTRY" --print public-api 2>&1)"
if [[ "$got" == "bash scripts/public-api.sh --check all" ]]; then
    pass "--print public-api is the baseline check over every crate"
else
    fail "--print public-api printed '$got'"
fi

bash "$REGISTRY" --print public-api extra >/dev/null 2>&1
rc=$?
if [[ "$rc" -eq 2 ]]; then
    pass "public-api refuses extra arguments (exit 2)"
else
    fail "public-api with an extra argument exited $rc, want 2"
fi

# --- pre-push leg -----------------------------------------------------------

# A scratch scripts/ dir: the real leg and public-api.sh, and a test-gate.sh
# stub that records its argv and exits with $STUB_GATE_RC.
SCRIPTS="$TMP/scripts"
GATE_LOG="$TMP/gate-invoked"
mkdir -p "$SCRIPTS"
cp "$LEG" "$PUBLIC_API" "$SCRIPTS/"
cat >"$SCRIPTS/test-gate.sh" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$*" >"$GATE_LOG"
exit "\${STUB_GATE_RC:-0}"
STUB

# Writes the tool stubs into a fresh bin dir named $1 and prints its path.
# Options: --tool-version V, --no-tool, --toolchains "A B" (installed names
# without the host triple), --no-rustup, --plain-cargo (a cargo that is not
# the rustup proxy), --no-rustdoc, --no-rust-std (the installed toolchains
# lack that component).
make_bin() {
    local dir="$TMP/bin-$1" tool_version="$VERSION" toolchains="$NIGHTLY" tool=1 rustup=1
    local plain_cargo=0 rustdoc=1 rust_std=1
    shift
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --tool-version) tool_version="$2"; shift 2 ;;
            --toolchains) toolchains="$2"; shift 2 ;;
            --no-tool) tool=0; shift ;;
            --no-rustup) rustup=0; shift ;;
            --plain-cargo) plain_cargo=1; shift ;;
            --no-rustdoc) rustdoc=0; shift ;;
            --no-rust-std) rust_std=0; shift ;;
        esac
    done
    mkdir -p "$dir"
    local libdir="$dir/rustlib/lib"
    mkdir -p "$libdir"
    if ((rust_std)); then
        touch "$libdir/libstd-0000000000000000.rlib"
    fi
    if ((tool)); then
        printf '#!/bin/sh\necho "cargo-public-api %s"\n' "$tool_version" >"$dir/cargo-public-api"
        chmod +x "$dir/cargo-public-api"
    fi
    if ((rustup)); then
        # Models `rustup which --toolchain T BIN`: T resolves only when it is
        # an installed name, bare or with the host triple appended.
        cat >"$dir/rustup" <<STUB
#!/usr/bin/env bash
triple=x86_64-unknown-linux-gnu
installed=(stable $toolchains)
if [[ "\$1 \$2" == "which --toolchain" ]]; then
    for t in "\${installed[@]}"; do
        if [[ "\$3" == "\$t" || "\$3" == "\$t-\$triple" ]]; then
            echo "/stub/toolchains/\$t-\$triple/bin/\$4"
            exit 0
        fi
    done
    echo "error: toolchain '\$3' is not installed" >&2
    exit 1
fi
echo "rustup stub: unsupported: \$*" >&2
exit 1
STUB
        chmod +x "$dir/rustup"
        # Models a rustup proxy invoked as `<proxy> +T ...`: T must be an
        # installed name and the proxied component present.
        local proxy present
        for proxy in cargo rustdoc rustc; do
            present=1
            [[ "$proxy" == rustdoc ]] && present=$rustdoc
            cat >"$dir/$proxy" <<STUB
#!/usr/bin/env bash
installed=(stable $toolchains)
found=0
for t in "\${installed[@]}"; do
    [[ "\$1" == "+\$t" ]] && found=1
done
if ((!found)); then
    echo "error: toolchain '\${1#+}' is not installed" >&2
    exit 1
fi
if ((!$present)); then
    echo "error: '$proxy' is not installed for the toolchain" >&2
    exit 1
fi
if [[ "$proxy \$2 \$3" == "rustc --print target-libdir" ]]; then
    echo "$libdir"
    exit 0
fi
echo "$proxy 1.0.0-nightly (stub)"
STUB
            chmod +x "$dir/$proxy"
        done
    fi
    if ((plain_cargo)); then
        cat >"$dir/cargo" <<'STUB'
#!/bin/sh
case "$1" in +*) echo "error: no such command: $1" >&2; exit 101 ;; esac
echo "cargo 1.0.0"
STUB
        chmod +x "$dir/cargo"
    fi
    echo "$dir"
}

# Runs the scratch leg with PATH = $1 plus the system dirs. Sets OUT and RC.
run_leg() {
    rm -f "$GATE_LOG"
    OUT="$(PATH="$1:$SYSTEM_PATH" STUB_GATE_RC="${2:-0}" bash "$SCRIPTS/public-api-pre-push.sh" 2>&1)"
    RC=$?
}

gate_ran() { [[ -f "$GATE_LOG" && "$(cat "$GATE_LOG")" == "public-api" ]]; }

skip_line() { printf '%s\n' "$OUT" | grep -q '^public-api: SKIPPED locally (.*); CI runs this check\.'; }

for bin in cargo-public-api rustup cargo rustdoc rustc; do
    if [[ -n "$(PATH="$SYSTEM_PATH" command -v "$bin")" ]]; then
        fail "$bin is in $SYSTEM_PATH, so the absent-tool cases cannot be hermetic"
    fi
done

full="$(make_bin full)"
run_leg "$full"
if [[ "$RC" -eq 0 ]] && gate_ran && ! skip_line; then
    pass "tooling present: the leg runs test-gate.sh public-api"
else
    fail "tooling present: rc=$RC gate_ran=$(gate_ran && echo yes || echo no) out='$OUT'"
fi

run_leg "$full" 1
if [[ "$RC" -ne 0 ]] && gate_ran; then
    pass "tooling present: a failing baseline check fails the leg (rc=$RC)"
else
    fail "tooling present: a failing check exited $RC"
fi

assert_skip() {
    local desc="$1" bin="$2" reason="$3"
    run_leg "$bin"
    if [[ "$RC" -ne 0 ]]; then
        fail "$desc: exited $RC, a skip must exit 0 (out='$OUT')"
    elif gate_ran; then
        fail "$desc: the registry ran"
    elif ! skip_line; then
        fail "$desc: no skip line (out='$OUT')"
    elif [[ "$(printf '%s\n' "$OUT" | wc -l)" -ne 1 ]]; then
        fail "$desc: the skip printed more than one line (out='$OUT')"
    elif ! printf '%s\n' "$OUT" | grep -qF "$reason"; then
        fail "$desc: skip line lacks '$reason' (out='$OUT')"
    else
        pass "$desc: skipped with one line, exit 0"
    fi
}

assert_skip "cargo-public-api absent" "$(make_bin no-tool --no-tool)" "cargo-public-api not on PATH"
assert_skip "cargo-public-api at another version" \
    "$(make_bin old-tool --tool-version 0.0.1)" "is not the pinned $VERSION"
assert_skip "pinned nightly absent" \
    "$(make_bin no-nightly --toolchains "nightly-1999-01-01")" "toolchain $NIGHTLY not installed"
assert_skip "rustup absent" "$(make_bin no-rustup --no-rustup)" "rustup not on PATH"
assert_skip "cargo on PATH is not the rustup proxy" \
    "$(make_bin plain-cargo --plain-cargo)" "not the rustup proxy"
assert_skip "pinned nightly lacks rustdoc" \
    "$(make_bin no-rustdoc --no-rustdoc)" "rustdoc for $NIGHTLY unavailable"
assert_skip "pinned nightly lacks the host rust-std" \
    "$(make_bin no-rust-std --no-rust-std)" "lacks rust-std for the host"

# The nightly match must not accept a toolchain whose name merely starts
# with the pin, with or without a separating dash.
assert_skip "only a longer-named toolchain sharing the pin's prefix" \
    "$(make_bin prefix-nightly --toolchains "${NIGHTLY}0")" "toolchain $NIGHTLY not installed"
assert_skip "only a custom toolchain named after the pin" \
    "$(make_bin custom-nightly --toolchains "${NIGHTLY}-custom")" "toolchain $NIGHTLY not installed"

# A public-api.sh without its version pin is a wiring defect, not a skip.
sed -i 's/cargo-public-api --version [0-9.]*/cargo-public-api/' "$SCRIPTS/public-api.sh"
run_leg "$full"
if [[ "$RC" -eq 1 ]] && ! gate_ran && printf '%s\n' "$OUT" | grep -q 'could not read the pins'; then
    pass "an unreadable pin fails the leg"
else
    fail "an unreadable pin: rc=$RC out='$OUT'"
fi

if ((fails)); then
    echo "test-gate.test.sh: $fails failure(s)" >&2
    exit 1
fi
echo "test-gate.test.sh: all assertions passed"
