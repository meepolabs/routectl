#!/usr/bin/env bash
# Self-test for check-release-linkage.sh. Exits 0 when all assertions pass,
# non-zero otherwise.
#
# Every control binary is compiled here at run time with cc: dummy shared
# libraries whose sonames are libssl.so.3 and libcrypto.so.3 stand in for a
# system OpenSSL, so no case depends on what the host has installed. A
# missing cc or readelf fails the self-test; it never skips.
#
# Run it from anywhere:
#   bash scripts/check-release-linkage.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$HERE/check-release-linkage.sh"

for tool in cc readelf; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "FAIL: $tool not found on PATH; the self-test needs it to build and read its controls" >&2
        exit 1
    fi
done

fails=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

die_build() {
    echo "FAIL: could not build control: $1" >&2
    exit 1
}

# Build a shared library whose DT_SONAME is $1.
build_dummy_lib() {
    local soname="$1"
    printf 'int %s_marker(void) { return 0; }\n' "${soname%%.*}" >"$tmp/$soname.c"
    cc -shared -fPIC -Wl,-soname,"$soname" -o "$tmp/$soname" "$tmp/$soname.c" ||
        die_build "$soname"
}

printf 'int main(void) { return 0; }\n' >"$tmp/main.c"

build_dummy_lib libssl.so.3
build_dummy_lib libcrypto.so.3

cc -o "$tmp/libc-only" "$tmp/main.c" || die_build libc-only
cc -o "$tmp/with-libcrypto" "$tmp/main.c" \
    -Wl,--no-as-needed "$tmp/libcrypto.so.3" || die_build with-libcrypto
cc -o "$tmp/with-libssl" "$tmp/main.c" \
    -Wl,--no-as-needed "$tmp/libssl.so.3" || die_build with-libssl
cc -static -o "$tmp/static" "$tmp/main.c" || die_build static
printf 'not an elf file\n' >"$tmp/not-elf"

# Positive control for the fixtures themselves: each dynamic control must
# carry the NEEDED entry its case is about, or a PASS/FAIL below proves
# nothing about the checker.
assert_needed() {
    local path="$1" lib="$2"
    if readelf -d -W "$path" | grep -qF "Shared library: [$lib]"; then
        echo "PASS: fixture $(basename "$path") has NEEDED $lib"
    else
        echo "FAIL: fixture $(basename "$path") lacks NEEDED $lib"
        fails=$((fails + 1))
    fi
}

assert_pass() {
    local desc="$1" path="$2" out
    if out="$(bash "$CHECKER" "$path" 2>&1)" && printf '%s' "$out" | grep -q 'release-linkage: PASS'; then
        echo "PASS: accepted -- $desc"
    else
        echo "FAIL: expected PASS -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    fi
}

# Expect exit code $3 and output containing $4.
assert_reject() {
    local desc="$1" path="$2" want_rc="$3" expect="$4" out rc
    out="$(bash "$CHECKER" "$path" 2>&1)"
    rc=$?
    if [[ "$rc" -ne "$want_rc" ]]; then
        echo "FAIL: expected exit $want_rc, got $rc -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    elif ! printf '%s' "$out" | grep -qF "$expect"; then
        echo "FAIL: rejected for the WRONG reason (no '$expect') -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    else
        echo "PASS: rejected -- $desc"
    fi
}

assert_needed "$tmp/libc-only" libc.so.6
assert_needed "$tmp/with-libcrypto" libcrypto.so.3
assert_needed "$tmp/with-libssl" libssl.so.3

assert_pass "libc-only dynamic binary" "$tmp/libc-only"

assert_reject "NEEDED libcrypto.so.3" "$tmp/with-libcrypto" 1 \
    'dynamically links libcrypto.so.3'

assert_reject "NEEDED libssl.so.3" "$tmp/with-libssl" 1 \
    'dynamically links libssl.so.3'

assert_reject "static binary with no NEEDED entries" "$tmp/static" 1 \
    'has no NEEDED libc.so.6'

assert_reject "non-ELF file" "$tmp/not-elf" 2 \
    'readelf could not read'

assert_reject "missing file" "$tmp/absent" 2 \
    'not a readable file'

out="$(bash "$CHECKER" 2>&1)"
rc=$?
if [[ "$rc" -eq 2 ]] && printf '%s' "$out" | grep -qF 'usage:'; then
    echo "PASS: rejected -- no argument"
else
    echo "FAIL: expected usage error (exit 2) with no argument, got $rc"
    printf '%s\n' "$out"
    fails=$((fails + 1))
fi

mkdir "$tmp/empty-path"
out="$(PATH="$tmp/empty-path" "$BASH" "$CHECKER" "$tmp/libc-only" 2>&1)"
rc=$?
if [[ "$rc" -eq 2 ]] && printf '%s' "$out" | grep -qF 'readelf not found'; then
    echo "PASS: rejected -- readelf missing from PATH"
else
    echo "FAIL: expected exit 2 when readelf is missing, got $rc"
    printf '%s\n' "$out"
    fails=$((fails + 1))
fi

if [[ "$fails" -ne 0 ]]; then
    echo "check-release-linkage.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "check-release-linkage.test.sh: all assertions passed"
