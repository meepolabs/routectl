#!/usr/bin/env bash
# Linux release-binary linkage check: the build pins aws-lc-sys to a vendored,
# statically linked AWS-LC, so a release ELF must not load libssl or libcrypto
# from the host at run time.
#
# Reads the dynamic section with `readelf -d` (the file is only read, never
# executed, so one host checks every architecture) and fails on any NEEDED
# entry matching ^lib(ssl|crypto)\.so. It also requires libc.so.6 in NEEDED:
# a glibc release binary always carries it, so its absence means readelf
# reported nothing usable (no dynamic section, a static binary, a truncated
# file) and an empty NEEDED list must not read as "no libssl".
#
# Usage: check-release-linkage.sh <elf-file>
#
# Exit codes: 0 = pass, 1 = finding, 2 = usage error, unreadable or non-ELF
# input, or readelf unavailable.

set -euo pipefail

readonly DENY_RE='^lib(ssl|crypto)\.so'
readonly REQUIRED_LIB='libc.so.6'

usage_error() {
    echo "release-linkage: $1" >&2
    exit 2
}

[[ $# -eq 1 ]] || usage_error "usage: check-release-linkage.sh <elf-file>"
elf="$1"
[[ -f "$elf" && -r "$elf" ]] || usage_error "not a readable file: $elf"
command -v readelf >/dev/null 2>&1 || usage_error "readelf not found on PATH"

if ! dynamic="$(LC_ALL=C readelf -d -W -- "$elf" 2>&1)"; then
    printf '%s\n' "$dynamic" >&2
    usage_error "readelf could not read $elf"
fi

needed=()
while IFS= read -r line; do
    if [[ "$line" == *"(NEEDED)"*"Shared library: ["*"]"* ]]; then
        lib="${line#*Shared library: [}"
        needed+=("${lib%]*}")
    fi
done <<<"$dynamic"

findings=0
has_required=0
for lib in ${needed[@]+"${needed[@]}"}; do
    if [[ "$lib" =~ $DENY_RE ]]; then
        echo "release-linkage: FAIL: $elf dynamically links $lib" >&2
        findings=$((findings + 1))
    fi
    [[ "$lib" == "$REQUIRED_LIB" ]] && has_required=1
done

if [[ "$has_required" -eq 0 ]]; then
    echo "release-linkage: FAIL: $elf has no NEEDED $REQUIRED_LIB (static, no dynamic section, or unreadable)" >&2
    findings=$((findings + 1))
fi

if [[ "$findings" -ne 0 ]]; then
    exit 1
fi
echo "release-linkage: PASS: $elf (NEEDED: ${needed[*]})"
