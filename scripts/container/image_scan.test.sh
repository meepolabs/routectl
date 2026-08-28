#!/usr/bin/env bash
# Verify that NO credential is present in ANY layer of the capture-cell
# image, by walking every layer of `docker save` output and running the
# repo's OWN scrub gate over the extracted content.
#
# "No credential in any layer" is an acceptance claim, and a claim about
# an image nobody opened is an assumption. This opens it.
#
# ONE OWNER OF CREDENTIAL VOCABULARY. Every pattern comes from
# scripts/scrub-fixture.sh; this file defines no credential regex of its
# own and contains no vendor key prefix as a literal -- the positive
# control below derives its prefix FROM that script's own rule, and an
# assertion at the end proves no such literal crept into this file. A
# second regex list here would drift from the gate the corpus is held to,
# and then the image scan and the fixture gate would disagree about what
# a credential looks like.
#
# WHY A SHELL SELF-TEST. Nothing in crates/ shells out to docker, and
# establishing that precedent was ruled against: a Rust test that needs a
# daemon socket turns `cargo test` into an environment-dependent gate.
#
# THE POSITIVE CONTROL IS THE LOAD-BEARING HALF. A scan whose walk finds
# zero layers, or whose extraction produces zero files, reports a clean
# image just as loudly as a genuinely clean one. So a throwaway image is
# built with a credential-shaped token planted in its LOWER layer, and it
# must FAIL this same scan, naming the class, at that layer index. That
# is what distinguishes a walk over every layer from a walk over the last
# one. The control image is removed on every exit path, including failure.
#
# Requires docker and python3:
#   docker absent  -> SKIPS BY NAME (a printed line saying what was not
#                     run) and exits 0. Never a silent pass.
#   python3 absent -> FAILS. The scrub gate hard-fails without it by
#                     design, so a skip here would hide a broken gate.
#
# The committed image is not built by this script. Building it downloads a
# ~250MB client from the network, which is a flake source and several
# minutes on a runner that has no image cached. When
# `routectl-capture:default` is absent the committed-image leg SKIPS BY
# NAME and the control leg still runs -- so the step is never vacuous: it
# still proves the walk-and-scan pipeline catches a planted token in a
# lower layer. Build the image with scripts/container/build.sh to
# exercise the committed-image leg.
#
# Run it from anywhere:
#   bash scripts/container/image_scan.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
SCRUB="$REPO_ROOT/scripts/scrub-fixture.sh"

IMAGE="routectl-capture:default"
CONTROL_IMAGE="routectl-capture-scan-control:dirty-layer"

# Per-run scratch on REAL disk inside the repo, mirroring the bench rig's
# .routectl-bench-work/ precedent. NOT $TMPDIR: `docker save` of this
# image is ~190MB and /tmp here is a tmpfs small enough that a cold cargo
# build already fills it, which surfaces as StorageFull in unrelated
# gates. Gitignored, and removed by the trap below.
WORK_PARENT="$REPO_ROOT/.routectl-image-scan-work"

fails=0

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- expected '$expected', got '$actual'"
        fails=$((fails + 1))
    fi
}

fail() {
    echo "FAIL: $1"
    fails=$((fails + 1))
}

# A named skip: the caller must be able to read WHAT was not verified off
# the log, which is the whole difference between a skip and a pass.
skip() {
    echo "SKIP: $1"
}

if ! command -v docker >/dev/null 2>&1; then
    skip "image layer credential scan -- docker is not installed or not on PATH;"
    skip "  neither the committed image scan nor the deliberately-dirty positive"
    skip "  control was run. Install docker to verify no credential is baked into"
    skip "  any layer of $IMAGE."
    exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 not found; the scrub gate cannot run and this scan would"
    echo "      report a clean image without having scanned one"
    exit 1
fi

if [ ! -r "$SCRUB" ]; then
    echo "FAIL: the scrub gate is not readable at $SCRUB"
    exit 1
fi

mkdir -p "$WORK_PARENT" || exit 1
WORK="$(mktemp -d "$WORK_PARENT/run.XXXXXX")" || exit 1

# shellcheck disable=SC2329 # invoked indirectly, by the EXIT trap below
cleanup() {
    # Both arguments are validated non-empty before use: a command
    # substitution strips trailing newlines and an empty one turns
    # `rm -rf "$x"/...` into a deletion of the wrong tree.
    docker image rm -f "$CONTROL_IMAGE" >/dev/null 2>&1 || true
    if [ -n "${WORK:-}" ] && [ -d "$WORK" ]; then
        # Layer tars carry read-only directories; without this the
        # removal leaves the tree behind.
        chmod -R u+rwX "$WORK" >/dev/null 2>&1 || true
        rm -rf "$WORK"
    fi
    rmdir "$WORK_PARENT" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# --- the hermetic environment the gate runs under --------------------
# The scrub gate derives four of its deny classes from the RUNNING
# environment: $HOME, the git identity, the hostname, and the local seat
# store. None of those is part of the image, so leaving them active makes
# the verdict depend on whose box ran the scan. Measured on this box: the
# operator's hostname occurs as a byte run inside an upstream vendor
# binary in the image, so the real-environment scan reported a `hostname`
# finding against content no local environment ever touched.
#
# Every one of the four is therefore neutralised the way the gate's own
# documented skip path expects -- $HOME set to the placeholder, a
# hostname in the gate's neutral set, no git identity, an empty XDG with
# no seat store -- and the gate prints its WARN line for each, so the
# narrowing is never silent.
HERMETIC_BIN="$WORK/hermetic-bin"
HERMETIC_XDG="$WORK/hermetic-xdg"
mkdir -p "$HERMETIC_BIN" "$HERMETIC_XDG"
printf '#!/bin/sh\nprintf "localhost\\n"\n' >"$HERMETIC_BIN/hostname"
chmod +x "$HERMETIC_BIN/hostname"

# Run the real gate over a directory tree. `cd /` because the gate reads
# `git config user.name`, and this scratch root sits inside the repo --
# from here git would resolve the repo's own identity and reintroduce the
# environment dependence the hermetic environment exists to remove.
scrub_check() {
    (
        cd / || exit 2
        env -i \
            PATH="$HERMETIC_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            HOME="/home/user" \
            XDG_CONFIG_HOME="$HERMETIC_XDG" \
            LC_ALL=C \
            bash "$SCRUB" --check "$1" 2>&1
    )
}

# --- what the walk narrows, and why ---------------------------------
# TEXT FILES ONLY. A credential enters an image as a file the build
# WROTE: an env file, a config, a token store, an installer log, a shell
# history. Every one is text. A vendor prefix appearing inside a stripped
# upstream ELF is a coincidental byte run, not a leak -- measured: the
# base image's libunistring carries a 40-character run that satisfies a
# vendor-key rule, and it recurs in every build of this Dockerfile. The
# narrowing is applied to the gate's INPUT rather than to its findings so
# that what was scanned is a fact about the file list, not a judgement
# about a verdict.
#
# The narrowing cannot hide a real leak silently: if it ever excluded
# everything, the positive control's planted token would be excluded too
# and the control assertion goes red.
#
# Hardlinked, not copied: the layers here total ~500MB uncompressed.
farm_text_files() {
    local src="$1" farm="$2" f rel dir n=0
    while IFS= read -r -d '' f; do
        rel="${f#"$src"/}"
        dir="${rel%/*}"
        if [ "$dir" != "$rel" ]; then
            mkdir -p "$farm/$dir" || continue
        fi
        ln "$f" "$farm/$rel" 2>/dev/null || cp "$f" "$farm/$rel" 2>/dev/null || continue
        n=$((n + 1))
    done < <(find "$src" -type f -print0 2>/dev/null |
        xargs -0 -r -n 200 grep -IlZ . 2>/dev/null)
    printf '%s\n' "$n"
}

# The layer paths `docker save` recorded, in base-to-top order, one per
# line. Read off the archive's OWN manifest rather than assumed: a modern
# docker emits an OCI layout whose layers are `blobs/sha256/<digest>`,
# while the legacy layout uses `<id>/layer.tar`. Both record the list in
# manifest.json, so reading it handles either. A save carrying no
# manifest.json is an unsupported layout and fails LOUDLY -- a walk that
# silently found zero layers is exactly how this scan would pass
# vacuously.
manifest_layer_paths() {
    python3 - "$1" <<'PY'
import json, sys

with open(sys.argv[1], encoding="utf-8") as fh:
    doc = json.load(fh)
if not isinstance(doc, list) or not doc:
    raise SystemExit(1)
layers = doc[0].get("Layers")
if not isinstance(layers, list) or not layers:
    raise SystemExit(1)
for path in layers:
    if not isinstance(path, str) or not path:
        raise SystemExit(1)
    print(path)
PY
}

# The class names in a gate verdict, one per line. Reads the gate's OWN
# output; carries no pattern of its own. A finding line is
# `  <path>  <class>`.
finding_classes() {
    awk '/^  \// { print $NF }' <<<"$1" | sort -u
}

# Classes that are NOT a credential and that fire on the distro content
# of any Linux root filesystem:
#
#   home-prefix       /etc/passwd names distro accounts (`/home/ubuntu`),
#                     the base build log names its builder account, and
#                     perl's shipped documentation quotes example home
#                     paths. The class exists to catch an OPERATOR's home
#                     path in a captured fixture; a distro's own is not
#                     that.
#   ls-owner-column   python's ftplib docstring contains a sample `ls -l`
#                     listing whose owner column is a BSD group name.
#
# Anything the gate reports outside this set fails the scan. Listing the
# accounted classes rather than the credential ones is deliberate: a new
# credential class added to the gate is then covered here the moment it
# lands, with no edit to this file, and a new PERSONAL class fails until
# somebody looks at it.
ACCOUNTED_CLASSES="home-prefix ls-owner-column"

class_is_accounted() {
    local c
    for c in $ACCOUNTED_CLASSES; do
        [ "$1" = "$c" ] && return 0
    done
    return 1
}

# Walk every layer of an image and echo one `<index> <class>` line per
# unaccounted finding, plus a `layers <n>` line and a `text <n>` line.
# The layer count is echoed rather than asserted here so the caller can
# compare it against the image's own count.
walk_image() {
    local image="$1" dir="$2"
    # A separate statement: bash makes every name in one `local` list
    # local before assigning any of them, so `tarball="$dir/..."` in the
    # line above reads an unset `dir` under `set -u`.
    local tarball="$dir/image.tar"
    local -a paths=()
    local path idx=0 lroot farm textcount verdict rc cls

    if ! docker save "$image" -o "$tarball" 2>"$dir/save.err"; then
        printf 'error docker-save-failed\n'
        sed -n '1,5p' "$dir/save.err" >&2
        return 0
    fi
    if ! tar -xf "$tarball" -C "$dir" manifest.json 2>/dev/null; then
        printf 'error no-manifest-in-save\n'
        return 0
    fi
    while IFS= read -r path; do
        [ -n "$path" ] && paths+=("$path")
    done < <(manifest_layer_paths "$dir/manifest.json")
    if [ "${#paths[@]}" -eq 0 ]; then
        printf 'error unreadable-manifest-layer-list\n'
        return 0
    fi

    for path in "${paths[@]}"; do
        idx=$((idx + 1))
        lroot="$dir/layer-$idx"
        farm="$dir/text-$idx"
        mkdir -p "$lroot" "$farm"
        if ! tar -xf "$tarball" -C "$dir" "$path" 2>/dev/null; then
            printf 'error layer-blob-not-extractable\n'
            continue
        fi
        # Auto-detects gzip: a save emits compressed layers for an OCI
        # layout and uncompressed ones for some legacy images.
        tar -xf "$dir/$path" -C "$lroot" \
            --no-same-owner --no-same-permissions >/dev/null 2>&1
        # Layer tars carry mode-0500 directories and mode-0400 files; the
        # gate treats an unreadable file as a hard error, and without this
        # the tree also cannot be removed.
        chmod -R u+rwX "$lroot" >/dev/null 2>&1 || true

        textcount="$(farm_text_files "$lroot" "$farm")"
        printf 'text %s\n' "$textcount"
        # A layer holding no text content at all (a single stripped
        # binary, a metadata-only layer) is scanned as nothing rather
        # than handed to the gate, which refuses an empty path set.
        [ "$textcount" -gt 0 ] || continue

        verdict="$(scrub_check "$farm")"
        rc=$?
        if [ "$rc" -gt 1 ]; then
            printf 'error gate-exit-%s\n' "$rc"
            continue
        fi
        while IFS= read -r cls; do
            [ -n "$cls" ] || continue
            class_is_accounted "$cls" && continue
            printf '%s %s\n' "$idx" "$cls"
        done < <(finding_classes "$verdict")
    done
    printf 'layers %s\n' "$idx"
}

# --- the committed image --------------------------------------------
if docker image inspect "$IMAGE" >/dev/null 2>&1; then
    IMAGE_LAYERS="$(docker image inspect "$IMAGE" --format '{{len .RootFS.Layers}}' 2>/dev/null)"
    mkdir -p "$WORK/committed"
    COMMITTED_OUT="$(walk_image "$IMAGE" "$WORK/committed")"

    COMMITTED_WALKED="$(awk '$1 == "layers" { print $2 }' <<<"$COMMITTED_OUT")"
    COMMITTED_ERRORS="$(awk '$1 == "error" { print $2 }' <<<"$COMMITTED_OUT")"
    COMMITTED_FINDINGS="$(awk '$1 ~ /^[0-9]+$/ { print }' <<<"$COMMITTED_OUT")"
    COMMITTED_TEXT="$(awk '$1 == "text" { n += $2 } END { print n + 0 }' <<<"$COMMITTED_OUT")"

    check "the layer walk of the committed image hit no structural error" \
        "" "$COMMITTED_ERRORS"

    # Greater than one, so a walk that found only the top layer is a
    # failure rather than a scan of a one-element list.
    if [ "${COMMITTED_WALKED:-0}" -gt 1 ]; then
        echo "PASS: the walk extracted more than one layer ($COMMITTED_WALKED)"
    else
        fail "the walk extracted more than one layer -- got '${COMMITTED_WALKED:-}'"
    fi

    # Against the image's OWN count, not a number written down here: a
    # rebuild that adds a Dockerfile stage must not quietly leave a layer
    # unscanned.
    check "the walk covered every layer the image reports" \
        "$IMAGE_LAYERS" "$COMMITTED_WALKED"

    if [ "${COMMITTED_TEXT:-0}" -gt 0 ]; then
        echo "PASS: the walk extracted text content to scan ($COMMITTED_TEXT files)"
    else
        fail "the walk extracted text content to scan -- got '${COMMITTED_TEXT:-}' files"
    fi

    check "no credential class in any layer of the committed image" \
        "" "$COMMITTED_FINDINGS"
else
    skip "committed image scan -- $IMAGE is not built locally, so no layer of it"
    skip "  was scanned for a credential. Build it with scripts/container/build.sh."
    skip "  The positive control below still runs."
fi

# --- the deliberately-dirty positive control ------------------------
# The token's prefix is READ OUT OF the scrub gate's own vendor-key rule,
# never written here. Two reasons, both load-bearing: a literal copy is a
# second owner of credential vocabulary that drifts from the gate, and
# the repo's own secret scanner rejects a source line carrying a full key
# shape -- suppressing a secret scanner to keep a fixture would be the
# wrong trade in any file, and worst of all in one whose subject is
# credential handling.
provider_key_prefix() {
    sed -n "s/^PROVIDER_KEY_RE='(\([^|)]*\)|.*/\1/p" "$SCRUB" | head -n 1
}

# The opaque run, split from its prefix so no full key shape exists on
# any line of this file. Long enough to clear the rule's own floor.
FAKE_RUN="AbCdEf0123456789ABCDEFGH"
fake_key() { printf '%s%s' "$1" "${2:-$FAKE_RUN}"; }

KEY_PREFIX="$(provider_key_prefix)"

# Without this the control plants a token with an empty prefix, the gate
# correctly finds nothing, and the control passes for the wrong reason --
# which is the exact vacuity the control exists to prevent.
if [ "${#KEY_PREFIX}" -ge 4 ]; then
    echo "PASS: a vendor key prefix was read out of the scrub gate's own rule"
else
    fail "a vendor key prefix was read out of the scrub gate's own rule -- got a ${#KEY_PREFIX}-char value; the control would plant no token"
fi

if [ "${#KEY_PREFIX}" -ge 4 ]; then
    CONTROL_CTX="$WORK/control-context"
    mkdir -p "$CONTROL_CTX/lower" "$CONTROL_CTX/upper"

    # Braced. An unbraced `$KEY_PREFIX...` would be a DIFFERENT,
    # undefined variable and the planted file would hold no token, so the
    # control would go green against a walk that reads nothing.
    printf 'PROVIDER_TOKEN=%s\n' "$(fake_key "${KEY_PREFIX}")" \
        >"$CONTROL_CTX/lower/service-env.txt"
    printf 'this layer carries no credential\n' >"$CONTROL_CTX/upper/notes.txt"

    # FROM scratch: two layers, no base image, no network. The token is
    # in the LOWER one, which is the only reason this control can tell a
    # walk over every layer apart from a walk over the top one.
    cat >"$CONTROL_CTX/Dockerfile" <<'DOCKERFILE'
FROM scratch
COPY lower/ /seeded/
COPY upper/ /extra/
DOCKERFILE

    if docker build -q -t "$CONTROL_IMAGE" "$CONTROL_CTX" >/dev/null 2>"$WORK/control-build.err"; then
        CONTROL_LAYERS="$(docker image inspect "$CONTROL_IMAGE" --format '{{len .RootFS.Layers}}' 2>/dev/null)"
        mkdir -p "$WORK/control"
        CONTROL_OUT="$(walk_image "$CONTROL_IMAGE" "$WORK/control")"

        CONTROL_WALKED="$(awk '$1 == "layers" { print $2 }' <<<"$CONTROL_OUT")"
        CONTROL_ERRORS="$(awk '$1 == "error" { print $2 }' <<<"$CONTROL_OUT")"
        CONTROL_FINDINGS="$(awk '$1 ~ /^[0-9]+$/ { print }' <<<"$CONTROL_OUT")"

        check "the layer walk of the control image hit no structural error" \
            "" "$CONTROL_ERRORS"
        check "the control image has more than one layer to distinguish" \
            "$CONTROL_LAYERS" "$CONTROL_WALKED"

        if [ -n "$CONTROL_FINDINGS" ]; then
            echo "PASS: the deliberately-dirty control FAILS the same scan"
        else
            fail "the deliberately-dirty control FAILS the same scan -- the scan found nothing, so a clean verdict on the committed image proves nothing"
        fi

        # Naming the class is the entire diagnostic a refusal gets: a
        # bare non-zero exit would not tell an operator what to look for.
        CONTROL_CLASS="$(awk '$1 ~ /^[0-9]+$/ { print $2 }' <<<"$CONTROL_FINDINGS" | sort -u)"
        if [ -n "$CONTROL_CLASS" ]; then
            echo "PASS: the control failure names the credential class ($CONTROL_CLASS)"
        else
            fail "the control failure names the credential class -- no class was reported"
        fi

        # THE ASSERTION THAT PROVES THE WALK. The token is in layer 1 of
        # 2, so a walk that extracts only the top layer reddens HERE
        # while every count above still passes.
        CONTROL_LOWER="$(awk '$1 == "1" { print $2 }' <<<"$CONTROL_FINDINGS" | sort -u)"
        if [ -n "$CONTROL_LOWER" ]; then
            echo "PASS: the planted token was found in the LOWER layer, not the top"
        else
            fail "the planted token was found in the LOWER layer, not the top -- the walk is not reaching every layer"
        fi

        # The paired accept direction: the top layer is clean, so the
        # scan is not simply reporting every layer of every image dirty.
        CONTROL_UPPER="$(awk '$1 == "2" { print $2 }' <<<"$CONTROL_FINDINGS" | sort -u)"
        check "the control's clean top layer reports no credential class" \
            "" "$CONTROL_UPPER"
    else
        fail "the positive control image could not be built; the scan is unverified"
        sed -n '1,5p' "$WORK/control-build.err"
    fi
fi

# --- this file owns no credential vocabulary ------------------------
# Asserted rather than trusted, and asserted against the gate's OWN
# prefix list so it needs no literal of its own: a future edit pasting a
# vendor prefix in here would create the second owner this whole file is
# arranged to avoid.
gate_key_prefixes() {
    sed -n "s/^PROVIDER_KEY_RE='(\([^)]*\))\[.*/\1/p" "$SCRUB" | head -n 1 | tr '|' '\n'
}

PREFIX_HITS=0
PREFIX_SEEN=0
while IFS= read -r prefix; do
    [ -n "$prefix" ] || continue
    PREFIX_SEEN=$((PREFIX_SEEN + 1))
    grep -qF -- "$prefix" "${BASH_SOURCE[0]}" && PREFIX_HITS=$((PREFIX_HITS + 1))
done < <(gate_key_prefixes)

if [ "$PREFIX_SEEN" -gt 1 ]; then
    echo "PASS: the gate's vendor prefix list was readable to check against ($PREFIX_SEEN entries)"
else
    fail "the gate's vendor prefix list was readable to check against -- got $PREFIX_SEEN entries, so the check below is vacuous"
fi
check "this file hardcodes no vendor key prefix of its own" "0" "$PREFIX_HITS"

echo
if [ "$fails" -eq 0 ]; then
    echo "image layer credential scan: all assertions passed"
    exit 0
fi
echo "image layer credential scan: $fails assertion(s) failed"
exit 1
