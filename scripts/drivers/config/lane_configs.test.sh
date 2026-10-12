#!/usr/bin/env bash
# Self-test for the committed driver lane configs and the Bedrock model
# candidate list. Exits 0 when every assertion passes.
#
# Every check runs against the COMMITTED files, and each one is paired
# with a planted copy that must fail it -- a check that cannot fail on a
# broken input proves nothing about the real one.
#
# Needs only python3 (stdlib) and bash; no routectl binary, no network,
# no credential.
#
# Run it from anywhere:
#   bash scripts/drivers/config/lane_configs.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
CHECKER="$HERE/lane_configs.py"
MODELS_TSV="$REPO_ROOT/scripts/wire_watch/models.tsv"

fails=0
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

pass() { echo "PASS: $1"; }
fail() {
    echo "FAIL: $1"
    fails=$((fails + 1))
}

# expect_rc LABEL WANT_RC CMD... -- run CMD, compare its exit code, and
# show its output only on a mismatch.
expect_rc() {
    local label="$1" want="$2" got
    shift 2
    "$@" >"$WORK/out" 2>&1
    got=$?
    if [ "$got" -eq "$want" ]; then
        pass "$label"
    else
        fail "$label -- want exit $want, got $got"
        sed -n '1,30p' "$WORK/out"
    fi
}

# expect_output LABEL NEEDLE -- the last expect_rc output names NEEDLE, so
# a planted failure is caught for the reason it was planted.
expect_output() {
    if grep -qF -- "$2" "$WORK/out"; then
        pass "$1"
    else
        fail "$1 -- output lacks '$2'"
        sed -n '1,30p' "$WORK/out"
    fi
}

# A scratch copy of the config dir and models.tsv a planted case may edit.
fresh_copy() {
    rm -rf "$WORK/cfg" "$WORK/models.tsv"
    mkdir -p "$WORK/cfg"
    cp "$HERE"/*.toml "$WORK/cfg/"
    cp "$MODELS_TSV" "$WORK/models.tsv"
}

lane_configs=("$HERE"/*.toml)

# --- every lane config checks every attempt -----------------------------
expect_rc "every committed lane config carries [wire_check] sample_one_in = 1" 0 \
    python3 -I "$CHECKER" wire-check "${lane_configs[@]}"

fresh_copy
grep -v '^sample_one_in = 1$' "$HERE/openai-responses.toml" >"$WORK/cfg/openai-responses.toml"
expect_rc "a config whose [wire_check] lost sample_one_in fails" 1 \
    python3 -I "$CHECKER" wire-check "$WORK/cfg/openai-responses.toml"
expect_output "the failure names the missing rate" "sample_one_in is None"

fresh_copy
awk '/^\[wire_check\]$/ { skip = 1; next } /^\[/ { skip = 0 } !skip' \
    "$HERE/bedrock-mantle.toml" >"$WORK/cfg/bedrock-mantle.toml"
expect_rc "a config with no [wire_check] table fails" 1 \
    python3 -I "$CHECKER" wire-check "$WORK/cfg/bedrock-mantle.toml"
expect_output "the failure names the missing table" "no [wire_check] table"

fresh_copy
sed 's/^sample_one_in = 1$/sample_one_in = 100/' "$HERE/anthropic-api.toml" >"$WORK/cfg/anthropic-api.toml"
expect_rc "a config sampling 1 in 100 fails" 1 \
    python3 -I "$CHECKER" wire-check "$WORK/cfg/anthropic-api.toml"

# --- the anthropic-api twins differ only by [mitm] -----------------------
# Comments and blank lines carry no wire meaning; the [mitm] table is
# dropped wherever it appears.
config_wire_lines() {
    awk '
        /^[[:space:]]*#/ { next }
        /^[[:space:]]*$/ { next }
        /^\[/            { in_mitm = ($0 == "[mitm]") }
        in_mitm          { next }
        { print }
    ' "$1"
}
twins_agree() {
    diff -q <(config_wire_lines "$1") <(config_wire_lines "$2") >/dev/null 2>&1
}
if twins_agree "$HERE/anthropic-api.toml" "$HERE/anthropic-api.front-proxy.toml"; then
    pass "the anthropic-api twins differ only by [mitm] and comments"
else
    fail "the anthropic-api twins differ outside [mitm]"
fi
fresh_copy
sed 's/^sample_one_in = 1$/sample_one_in = 2/' "$HERE/anthropic-api.front-proxy.toml" \
    >"$WORK/cfg/anthropic-api.front-proxy.toml"
if twins_agree "$HERE/anthropic-api.toml" "$WORK/cfg/anthropic-api.front-proxy.toml"; then
    fail "the twin comparison missed a [wire_check] drift on one twin"
else
    pass "the twin comparison catches a [wire_check] drift on one twin"
fi

# --- models.tsv and the Bedrock lane configs are welded ------------------
expect_rc "models.tsv and the Bedrock lane configs agree" 0 \
    python3 -I "$CHECKER" weld "$MODELS_TSV" "$HERE"

fresh_copy
python3 -I - "$WORK/cfg/bedrock-converse.toml" <<'PY'
import sys
path = sys.argv[1]
with open(path, encoding="utf-8") as handle:
    text = handle.read()
text = text.replace("[aliases]", '[models.converse-unlisted]\nprovider = "bedrock"\nupstream = "anthropic.claude-unlisted"\n\n[aliases]', 1)
with open(path, "w", encoding="utf-8") as handle:
    handle.write(text)
PY
expect_rc "a config model with no models.tsv row fails" 1 \
    python3 -I "$CHECKER" weld "$WORK/models.tsv" "$WORK/cfg"
expect_output "the failure names the unlisted model" "[models.converse-unlisted] has no models.tsv row"

fresh_copy
printf 'bedrock-mantle\tmantle-unconfigured\topenai.gpt-6-sol\tsweep\tyes\n' >>"$WORK/models.tsv"
expect_rc "a models.tsv row with no config model fails" 1 \
    python3 -I "$CHECKER" weld "$WORK/models.tsv" "$WORK/cfg"
expect_output "the failure names the unconfigured nickname" "mantle-unconfigured has no [models.mantle-unconfigured]"

fresh_copy
sed 's/\tinvoke-claude-haiku-5-5\t\(.*\)\tsweep\t/\tinvoke-claude-haiku-5-5\t\1\tprimary\t/' \
    "$MODELS_TSV" >"$WORK/models.tsv"
expect_rc "a lane with two primaries fails" 1 \
    python3 -I "$CHECKER" weld "$WORK/models.tsv" "$WORK/cfg"
expect_output "the failure names the lane's primaries" "lane bedrock-invoke: want exactly one primary"

fresh_copy
sed 's/\tmantle-gpt-6-sol\t\(.*\)\tprimary\t/\tmantle-gpt-6-sol\t\1\tsweep\t/' \
    "$MODELS_TSV" >"$WORK/models.tsv"
expect_rc "a lane with no primary fails" 1 \
    python3 -I "$CHECKER" weld "$WORK/models.tsv" "$WORK/cfg"

fresh_copy
sed 's/^default = "converse-claude-sonnet-5-5"$/default = "converse-claude-haiku-5-5"/' \
    "$HERE/bedrock-converse.toml" >"$WORK/cfg/bedrock-converse.toml"
expect_rc "a default alias not pointing at the primary fails" 1 \
    python3 -I "$CHECKER" weld "$WORK/models.tsv" "$WORK/cfg"
expect_output "the failure names the default" "[aliases] default is 'converse-claude-haiku-5-5'"

fresh_copy
sed 's/^"claude-\*" = /"invoke-claude-*" = /' "$HERE/bedrock-invoke.toml" >"$WORK/cfg/bedrock-invoke.toml"
expect_rc "an alias glob that prefixes a nickname fails" 1 \
    python3 -I "$CHECKER" weld "$WORK/models.tsv" "$WORK/cfg"
expect_output "the failure names the shadowing glob" "alias glob 'invoke-claude-*' shadows nickname"

fresh_copy
sed 's/^upstream = "openai.gpt-6-luna"$/upstream = "openai.gpt-6-lunar"/' \
    "$HERE/bedrock-mantle.toml" >"$WORK/cfg/bedrock-mantle.toml"
expect_rc "a config upstream that differs from models.tsv fails" 1 \
    python3 -I "$CHECKER" weld "$WORK/models.tsv" "$WORK/cfg"

# --- every upstream id is cited ------------------------------------------
fresh_copy
grep -v '^# doc .*model-card-openai-gpt-6-astra\.html' "$MODELS_TSV" >"$WORK/models.tsv"
expect_rc "an upstream id whose doc citation is removed fails" 1 \
    python3 -I "$CHECKER" weld "$WORK/models.tsv" "$WORK/cfg"
expect_output "the failure names the uncited id" "upstream openai.gpt-6-astra is not cited"

# --- each Bedrock lane names only its key's region -----------------------
expect_rc "Invoke/Converse name only us-west-2 and Mantle only us-east-1" 0 \
    python3 -I "$CHECKER" regions "$HERE"

fresh_copy
sed 's/^region    = "us-west-2"$/region    = "us-east-1"/' "$HERE/bedrock-converse.toml" \
    >"$WORK/cfg/bedrock-converse.toml"
expect_rc "a Converse config in us-east-1 fails" 1 \
    python3 -I "$CHECKER" regions "$WORK/cfg"
expect_output "the failure names the wrong region" "region 'us-east-1', lane allows only 'us-west-2'"

fresh_copy
python3 -I - "$WORK/cfg/bedrock-mantle.toml" <<'PY'
import sys
path = sys.argv[1]
with open(path, encoding="utf-8") as handle:
    lines = handle.read().split("\n")
for index, line in enumerate(lines):
    if line == 'region = "us-east-1"':
        lines[index] = 'region = "us-west-2"'
        break
with open(path, "w", encoding="utf-8") as handle:
    handle.write("\n".join(lines))
PY
expect_rc "one Mantle provider in us-west-2 fails" 1 \
    python3 -I "$CHECKER" regions "$WORK/cfg"
expect_output "the failure names the Mantle region" "region 'us-west-2', lane allows only 'us-east-1'"

# --- the reader refuses what it does not understand ----------------------
fresh_copy
printf 'models = [\n  "x",\n]\n' >>"$WORK/cfg/bedrock-invoke.toml"
expect_rc "a multi-line value is a parse error, not a silent skip" 2 \
    python3 -I "$CHECKER" regions "$WORK/cfg"

echo
if [ "$fails" -eq 0 ]; then
    echo "lane_configs.test.sh: all checks passed"
    exit 0
fi
echo "lane_configs.test.sh: $fails check(s) failed"
exit 1
