#!/usr/bin/env bash
# Assert the EFFECTIVE rustc and rustfmt are the toolchain pinned in
# rust-toolchain.toml.
#
# WHY THIS EXISTS: every gate leg invokes bare `cargo` / `rustfmt` and trusts
# the rustup shim to read rust-toolchain.toml. That trust is unverified, and
# three ordinary situations break it silently -- RUSTUP_TOOLCHAIN exported in
# the shell, a directory override left behind by `rustup override set`, or no
# rustup at all with a system toolchain first on PATH. In each case the gates
# still run and still pass; they just ran against a compiler and formatter the
# pin never selected, which is exactly the drift the exact-patch pin exists to
# prevent (see the comment block in rust-toolchain.toml).
#
# This is a VERSION check, not a rustup-presence check: a distro toolchain that
# happens to BE the pinned version passes. The pin constrains the effective
# compiler, not the installer that put it there.
#
# rustfmt is checked independently of rustc rather than assumed to follow it,
# because the failure mode is asymmetric: a stray /usr/bin/rustfmt earlier on
# PATH than the rustup shim shadows only the formatter. rustfmt reports its own
# version line (1.9.0-stable), never the rustc version, so the pin is verified
# through the toolchain build id both binaries carry -- the commit hash and
# build date in parentheses. Same build id means same toolchain; that is the
# strongest statement rustfmt's own output supports.
#
# Exit codes: 0 = both tools are the pinned toolchain, 1 = a tool is missing,
# the wrong toolchain resolved, or the pin itself could not be read.

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

PIN_FILE="rust-toolchain.toml"

if [[ ! -f "$PIN_FILE" ]]; then
    echo "toolchain: FAIL $PIN_FILE not found under $ROOT -- the pin cannot be read," >&2
    echo "toolchain: so the gates cannot prove which toolchain they ran against." >&2
    exit 1
fi

PINNED="$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$PIN_FILE" | head -1)"
if [[ -z "$PINNED" ]]; then
    echo "toolchain: FAIL no 'channel = \"...\"' line in $PIN_FILE -- refusing to pass" >&2
    echo "toolchain: while blind to the pin." >&2
    exit 1
fi

# Distinct from the wrong-channel message on purpose: "not installed" and
# "installed but not the pinned one" have different fixes.
require_tool() {
    local tool="$1"
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "toolchain: FAIL $tool not found on PATH; the pinned toolchain is $PINNED" >&2
        echo "toolchain: install it with: rustup toolchain install $PINNED" >&2
        exit 1
    fi
}

wrong_channel() {
    local detail="$1"
    echo "toolchain: FAIL $detail" >&2
    echo "toolchain: rust-toolchain.toml pins $PINNED, so the gates would run against a" >&2
    echo "toolchain: toolchain the pin never selected. Likely causes:" >&2
    echo "toolchain:   - RUSTUP_TOOLCHAIN is exported in this shell (unset it)" >&2
    echo "toolchain:   - a directory override from 'rustup override set' (clear it with" >&2
    echo "toolchain:     'rustup override unset' in $ROOT)" >&2
    echo "toolchain:   - rustup is absent and a system toolchain is first on PATH" >&2
    echo "toolchain:     (install rustup, or make the system toolchain $PINNED)" >&2
    exit 1
}

require_tool rustc
require_tool rustfmt

RUSTC_LINE="$(rustc --version)"
RUSTFMT_LINE="$(rustfmt --version)"

RUSTC_VERSION="$(printf '%s\n' "$RUSTC_LINE" | awk '{print $2}')"
if [[ "$RUSTC_VERSION" != "$PINNED" ]]; then
    wrong_channel "rustc reports '$RUSTC_LINE'"
fi

# "<hash> <date>" from the trailing parenthesised group, empty when a build
# omits it (some distro builds do).
build_id_of() {
    printf '%s\n' "$1" | sed -n 's/.*(\([^ )]*\)[[:space:]]\{1,\}\([^)]*\)).*/\1 \2/p'
}

RUSTC_ID="$(build_id_of "$RUSTC_LINE")"
RUSTFMT_ID="$(build_id_of "$RUSTFMT_LINE")"

# rustc abbreviates the commit hash to 9 chars and rustfmt to 10, so compare on
# the shorter of the two rather than requiring equal length.
same_build() {
    local a="$1" b="$2" a_hash a_date b_hash b_date n
    a_hash="${a%% *}"
    a_date="${a##* }"
    b_hash="${b%% *}"
    b_date="${b##* }"
    [[ "$a_date" == "$b_date" ]] || return 1
    if [[ -n "$a_hash" && -n "$b_hash" ]]; then
        n="${#a_hash}"
        ((${#b_hash} < n)) && n="${#b_hash}"
        [[ "${a_hash:0:n}" == "${b_hash:0:n}" ]] || return 1
    else
        [[ -z "$a_hash" && -z "$b_hash" ]] || return 1
    fi
    return 0
}

if ! same_build "$RUSTC_ID" "$RUSTFMT_ID"; then
    wrong_channel "rustfmt ('$RUSTFMT_LINE') is not from the same toolchain build as rustc ('$RUSTC_LINE')"
fi

echo "toolchain: PASS rustc and rustfmt are the pinned $PINNED"
