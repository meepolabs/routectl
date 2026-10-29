#!/usr/bin/env bash
# End-to-end proof that the live wire check names a planted loss on each
# leg, through the same runner a wire-watch cell uses.
#
# A python mock upstream on a probed free loopback port answers three cells,
# each driven by a curl stub through scripts/capture_driver.sh against the
# REAL routectl binary:
#
#   request-leg-plant   a Responses request carrying `temperature`, which
#                       the responses->openai-responses egress drops
#   response-leg-plant  a Responses stream whose message item carries an
#                       `id`, which the responses->openai-responses egress
#                       drops; the mock plants the id only for this cell,
#                       recognised by a marker in its input text
#   clean-control       a plain chat completion: no finding, nonzero checks
#
# Both plants are entries of KNOWN_UNDECLARED in
# crates/routectl-cli/src/handlers/zero_detection_tests.rs. The weld below
# runs on every invocation, binary or not: when one of those losses is
# fixed its entry leaves that list, and this test fails until the plant is
# swapped for a current entry and the snapshots are re-recorded.
#
# The recorded health.json snapshots under scripts/wire_watch/testdata/
# are what the report tests read. They are recorded from a real run of this
# script, never written by hand:
#
#   bash scripts/wire_watch/planted_drop.test.sh --record
#
# Normal mode compares each fresh snapshot's (lane, leg, path, path_class)
# finding set with its recording.
#
# Binary: ROUTECTL_BIN, else release/routectl under CARGO_TARGET_DIR
# (default target/). Neither present is a named SKIP of the end-to-end half;
# the weld and the source hygiene checks still run and the exit is 0.
#
# Every daemon and listener here binds a port probed free on 127.0.0.1, the
# daemon runs under the runner's throwaway HOME and XDG root (with
# ROUTECTL_CONFIG unset, which would otherwise outrank that root), and every
# process this script starts is stopped by its own pid. A source grep below
# holds these files to that.
#
# Flags (self-test seams):
#   --record               write the fresh snapshots into testdata/
#   --plants off           the mock and the stub omit the planted members;
#                          the comparison must then fail
#   --known-source FILE    weld against FILE instead of the committed source

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
HELPER="$HERE/planted_drop.py"
TESTDATA="$HERE/testdata"
KNOWN_SOURCE="$REPO/crates/routectl-cli/src/handlers/zero_detection_tests.rs"

RECORD=0
PLANTS=on
while [ $# -gt 0 ]; do
    case "$1" in
        --record) RECORD=1; shift ;;
        --plants)
            [ $# -ge 2 ] || { echo "planted_drop: --plants needs on or off" >&2; exit 2; }
            PLANTS="$2"; shift 2 ;;
        --known-source)
            [ $# -ge 2 ] || { echo "planted_drop: --known-source needs a file" >&2; exit 2; }
            KNOWN_SOURCE="$2"; shift 2 ;;
        *) echo "planted_drop: unknown arg: $1" >&2; exit 2 ;;
    esac
done
case "$PLANTS" in on|off) ;; *) echo "planted_drop: --plants takes on or off" >&2; exit 2 ;; esac
if [ "$RECORD" = 1 ] && [ "$PLANTS" = off ]; then
    echo "planted_drop: refusing to record with the plants off" >&2
    exit 2
fi

fails=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; fails=$((fails + 1)); }

for tool in python3 ss curl; do
    command -v "$tool" >/dev/null 2>&1 || { echo "FAIL: $tool not found"; exit 1; }
done

# ---------------------------------------------------------------------
# Weld: every plant is still a known undeclared loss
# ---------------------------------------------------------------------

WORK="$(mktemp -d)"
MOCK_PID=""
# shellcheck disable=SC2329 # invoked by the EXIT trap
cleanup() {
    if [ -n "$MOCK_PID" ]; then
        kill "$MOCK_PID" 2>/dev/null || true
        wait "$MOCK_PID" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

if python3 "$HELPER" weld --source "$KNOWN_SOURCE"; then
    pass "every plant is still in KNOWN_UNDECLARED"
else
    fail "a plant left KNOWN_UNDECLARED"
fi

# The weld must be able to fail: the same source with the request-leg plant's
# entry removed has to be refused with the swap message.
weld_control="$WORK/weld-control.rs"
python3 - "$KNOWN_SOURCE" "$weld_control" <<'PY'
import re, sys
src = open(sys.argv[1], encoding="utf-8").read()
cut = re.sub(
    r'\s*KnownUndeclared \{\s*lane: RESPONSES,\s*leg: Leg::Request,\s*path: "/temperature",.*?\},',
    "", src, count=1, flags=re.S)
open(sys.argv[2], "w", encoding="utf-8").write(cut)
PY
if python3 "$HELPER" weld --source "$weld_control" >"$WORK/weld.out" 2>&1; then
    fail "the weld accepts a source missing a planted entry"
elif grep -q "swap the plant" "$WORK/weld.out"; then
    pass "the weld refuses a removed plant with the swap message"
else
    fail "the weld refused a removed plant without the swap message: $(cat "$WORK/weld.out")"
fi

# ---------------------------------------------------------------------
# Hygiene: what this proof may bind and open
# ---------------------------------------------------------------------

# Every literal the greps look for is assembled at run time, so these files
# never spell one and the greps cannot match their own patterns.
LIVE_PORT="$((9000 + 100))"
LEDGER_NAME="usage"".db"
OWN_FILES=("${BASH_SOURCE[0]}" "$HELPER")
# A bind or URL on anything but 127.0.0.1: a wildcard, the loopback host
# NAME (which may resolve off loopback), or a fixed port on loopback.
WIDE_BIND_RE='0\.0\.0\.0|\[::\]|local''host|127\.0\.0\.1:[0-9]'

hygiene_hits() {
    local hits=0
    grep -qF -- "$LIVE_PORT" "$@" && hits=$((hits + 1))
    grep -qF -- "$LEDGER_NAME" "$@" && hits=$((hits + 1))
    grep -qE -- "$WIDE_BIND_RE" "$@" && hits=$((hits + 1))
    # The daemon starts only through the runner, which gives it a throwaway
    # HOME and XDG root; a direct `serve` here would boot on the caller's.
    # shellcheck disable=SC2016 # matches source text, not an expansion
    grep -qE -- '"\$BIN"[[:space:]]+serve|routectl[[:space:]]+serve' "$@" && hits=$((hits + 1))
    printf '%s\n' "$hits"
}

hits="$(hygiene_hits "${OWN_FILES[@]}")"
if [ "$hits" = 0 ]; then
    pass "these files name neither the live port nor the ledger, bind only probed loopback ports, and start no daemon directly"
else
    fail "$hits hygiene pattern(s) matched in these files"
fi
# shellcheck disable=SC2016 # the patterns match source text, not expansions
if grep -qE 'ThreadingHTTPServer\(\("127\.0\.0\.1", port\)' "$HELPER" &&
    grep -qE -- '--port "\$MOCK_PORT"' "${BASH_SOURCE[0]}" &&
    grep -qE '^MOCK_PORT="\$\(pick_port\)"' "${BASH_SOURCE[0]}"; then
    pass "the mock upstream binds 127.0.0.1 on the probed port"
else
    fail "the mock upstream bind is not the probed loopback port"
fi

hygiene_control="$WORK/hygiene-control.sh"
{
    printf 'curl http://127.0.0.1:%s/health\n' "$LIVE_PORT"
    printf 'sqlite3 ~/.config/routectl/%s\n' "$LEDGER_NAME"
    printf 'server = ThreadingHTTPServer(("%s", 0), H)\n' "0.0"".0.0"
    # shellcheck disable=SC2016 # writes the literal text a regression would hold
    printf '"$BIN" %s --port 1\n' "ser""ve"
} >"$hygiene_control"
check_hits="$(hygiene_hits "$hygiene_control")"
if [ "$check_hits" = 4 ]; then
    pass "each hygiene pattern fires on a file that does contain it"
else
    fail "only $check_hits of 4 hygiene patterns fire on a planted control"
fi

# ---------------------------------------------------------------------
# Binary
# ---------------------------------------------------------------------

BIN="${ROUTECTL_BIN:-${CARGO_TARGET_DIR:-$REPO/target}/release/routectl}"
if [ ! -x "$BIN" ]; then
    echo "SKIP: planted-drop end to end: no routectl binary at $BIN (set ROUTECTL_BIN or cargo build --release)"
    if [ "$fails" -eq 0 ]; then
        echo "planted drop: weld passed, end to end skipped"
        exit 0
    fi
    echo "planted drop: $fails assertion(s) failed"
    exit 1
fi

# ---------------------------------------------------------------------
# Throwaway tree
# ---------------------------------------------------------------------

TREE="$WORK/tree"
mkdir -p "$TREE"
cp -R "$REPO/scripts" "$TREE/scripts"
OUT_ROOT="$WORK/landing"
mkdir -p "$OUT_ROOT"

port_in_use() {
    ss -ltn 2>/dev/null | awk -v p=":$1" 'NR > 1 && index($4, p) == length($4) - length(p) + 1 { found = 1 } END { exit !found }'
}
pick_port() {
    local candidate
    for _ in $(seq 1 64); do
        candidate=$((20000 + RANDOM % 8000))
        port_in_use "$candidate" || { printf '%s\n' "$candidate"; return 0; }
    done
    return 1
}

MOCK_PORT="$(pick_port)" || { echo "FAIL: no free port for the mock upstream"; exit 1; }
python3 "$HELPER" serve --port "$MOCK_PORT" --ready-file "$WORK/mock.ready" --plants "$PLANTS" \
    2>"$WORK/mock.log" &
MOCK_PID=$!
for _ in $(seq 1 80); do
    [ -e "$WORK/mock.ready" ] && break
    kill -0 "$MOCK_PID" 2>/dev/null || break
    sleep 0.1
done
if [ ! -e "$WORK/mock.ready" ]; then
    echo "FAIL: the mock upstream never listened on 127.0.0.1:$MOCK_PORT"
    cat "$WORK/mock.log"
    exit 1
fi

# The upstream credential is a placeholder the mock never checks; it is held
# in a 0600 file inside the throwaway tree so the lane configs carry no
# value.
KEY_FILE="$WORK/upstream.key"
( umask 077; printf 'placeholder\n' >"$KEY_FILE" )

write_lane() {
    local name="$1" provider_block="$2"
    cat >"$TREE/scripts/drivers/config/planted-$name.toml" <<TOML
version = 5

[server]
host = "127.0.0.1"
port = 8787

$provider_block

[models.planted]
provider = "mock"
upstream = "planted-upstream"

[aliases]
default = "planted"

[retry]
max_attempts = 1

[wire_check]
sample_one_in = 1
TOML
}
write_lane chat "[providers.mock]
kind        = \"openai-compat\"
base_url    = \"http://127.0.0.1:$MOCK_PORT/v1\"
api_key_ref = \"file://$KEY_FILE\""
write_lane responses "[providers.mock]
kind        = \"openai-responses\"
base_url    = \"http://127.0.0.1:$MOCK_PORT/v1\"
api_key_ref = \"file://$KEY_FILE\"
auth_kind   = \"api-key\""

STUB="$WORK/stub-driver.sh"
cat >"$STUB" <<'SH'
#!/usr/bin/env bash
set -eu
request="$(python3 "$PLANTED_DROP_HELPER" request "$PLANTED_DROP_CELL" --plants "$PLANTED_DROP_PLANTS")"
route="$(printf '%s\n' "$request" | sed -n 1p)"
printf '%s\n' "$request" | sed -n 2p >"$ROUTECTL_DRIVER_RUN/request.json"
curl -fsS -m 20 -H 'content-type: application/json' \
    --data-binary "@$ROUTECTL_DRIVER_RUN/request.json" \
    "$ROUTECTL_BASE_URL$route" >"$ROUTECTL_DRIVER_RUN/response.body"
SH
chmod +x "$STUB"

# ---------------------------------------------------------------------
# Cells
# ---------------------------------------------------------------------

# The runner's own exit code is not asserted: a hand-built request is not a
# client fixture, so the rig may refuse to land one. What this proof reads
# is the cell evidence, which the runner writes on every exit path once the
# daemon is healthy; the driver's own exit code inside it must be 0.
run_cell() {
    local cell="$1" lane="$2" ingress="$3" out="$OUT_ROOT/$1" rc=0
    PLANTED_DROP_HELPER="$HELPER" PLANTED_DROP_CELL="$cell" PLANTED_DROP_PLANTS="$PLANTS" \
    ROUTECTL_BIN="$BIN" ROUTECTL_DRIVER_OUT_ROOT="$OUT_ROOT" \
        env -u ROUTECTL_CONFIG bash "$TREE/scripts/capture_driver.sh" --lane "planted-$lane" --case plain-turn-01 \
        --expected-ingress "$ingress" --out "$out" --out-root "$OUT_ROOT" \
        --work "$WORK/runs" -- "$STUB" >"$WORK/$cell.log" 2>&1 || rc=$?
    local evidence="$out/.cell-evidence"
    if [ ! -s "$evidence/health.json" ]; then
        fail "$cell: no health.json (runner exit $rc)"
        tail -n 30 "$WORK/$cell.log"
        return 1
    fi
    local driver_rc
    driver_rc="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["driver_rc"])' \
        "$evidence/client_result.json" 2>/dev/null || echo unreadable)"
    if [ "$driver_rc" != 0 ]; then
        fail "$cell: the stub driver exited $driver_rc"
        tail -n 30 "$WORK/$cell.log"
        return 1
    fi
    return 0
}

recorded_name() { printf '%s/%s.health.json\n' "$TESTDATA" "$1"; }

while read -r cell lane ingress; do
    run_cell "$cell" "$lane" "$ingress" || continue
    fresh="$OUT_ROOT/$cell/.cell-evidence/health.json"
    if [ "$PLANTS" = on ]; then
        if python3 "$HELPER" check "$cell" "$fresh"; then
            pass "$cell: fresh snapshot carries what the cell plants"
        else
            fail "$cell: fresh snapshot does not carry what the cell plants"
        fi
    fi
    recorded="$(recorded_name "$cell")"
    if [ "$RECORD" = 1 ]; then
        mkdir -p "$TESTDATA"
        python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); open(sys.argv[2],"w").write(json.dumps(d, indent=2, sort_keys=True)+"\n")' \
            "$fresh" "$recorded"
        pass "$cell: recorded $(basename "$recorded")"
    elif [ ! -r "$recorded" ]; then
        fail "$cell: no recording at $recorded (run with --record)"
    elif python3 "$HELPER" compare "$fresh" "$recorded"; then
        pass "$cell: fresh findings match the recording"
    else
        fail "$cell: fresh findings differ from the recording"
    fi
done < <(python3 "$HELPER" cells)

if [ "$RECORD" = 1 ] && [ "$fails" -eq 0 ]; then
    if bash "$REPO/scripts/scrub-fixture.sh" --check "$TESTDATA" >/dev/null; then
        pass "the recordings pass the scrub gate"
    else
        fail "the recordings fail the scrub gate"
    fi
fi

if [ "$fails" -eq 0 ]; then
    echo "planted drop: all assertions passed"
    exit 0
fi
echo "planted drop: $fails assertion(s) failed"
exit 1
