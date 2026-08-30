#!/usr/bin/env bash
# Capture per-request body+summary fixtures from the routectl TRACE log.
#
# Reads /tmp/routectl-trace.log (or `--log <path>`), finds completed
# request_ids since the last capture, and writes per-request fixture
# directories under `crates/routectl-cli/tests/fixtures/captured/<id>/`.
#
# A request counts as "complete" when its trace carries either an
# `upstream success body` line (non-stream path) OR a `stream summary`
# line (stream path). The capture covers any ingress dialect and any
# egress provider routectl emits a provider_kind for. The captured
# meta.json records both `ingress_kind` and `provider_kind` so a
# downstream consumer can dispatch on either.
#
# meta.json carries NO file-presence flags: which optional files a
# fixture directory holds is read from the directory listing by the
# replay loader. The tmp-then-rename promotion below already makes a
# fixture directory all-or-nothing, so a second copy of that fact in
# meta.json could only ever disagree with the filesystem -- and did.
# The one schema is documented in docs/REPLAY-FIXTURES.md.
#
# State: the script writes the timestamp of the last seen completion
# to `crates/routectl-cli/tests/fixtures/captured/.last_capture_ts`
# and resumes from there on the next run, so periodic invocations
# don't re-capture the same requests.
#
# Usage:
#   scripts/capture_fixtures.sh [--log /tmp/routectl-trace.log] \
#                               [--out crates/routectl-cli/tests/fixtures/captured] \
#                               [--limit 4] [--force] [--allow-unsafe-out] \
#                               [--driver-mode]
#
# `--limit N` caps the number of NEW requests captured this run
# (periodic runs typically pass a small limit).
# `--force` ignores the resume marker and re-captures from the start
# of the log.
# `--out` is confined to the default captured dir (which is gitignored)
# because fixtures carry RAW headers -- auth included when the daemon
# runs with ROUTECTL_TRACE_HEADERS. `--allow-unsafe-out` lifts that
# guard for a deliberate out-of-tree capture.
#
# Environment pins recorded verbatim into meta.json when set, empty when
# not (a live-box capture cannot observe them from the trace):
#   ROUTECTL_FIXTURE_CASE_ID          scenario identity for rerun diffs
#   ROUTECTL_FIXTURE_CONFIG_SHA       hash of the config in force
#   ROUTECTL_FIXTURE_CONNECTION_MODE  how the client reached routectl
#   ROUTECTL_FIXTURE_WIRE_PATTERN     wire shape the case claims to cover
#
# One further driver-mode pin is NOT recorded into meta.json, because the
# value it pins is already there as a traced fact:
#   ROUTECTL_FIXTURE_EXPECTED_INGRESS  ingress dialect the run expects
#
# And one more, OPTIONAL in both modes:
#   ROUTECTL_FIXTURE_CLIENT_BINARY_VERSION
#                                     the version the driver read off the
#                                     RUNNING client binary, forwarded by
#                                     the runner. Recorded as
#                                     `client.binary_version`, empty when
#                                     unset. Optional in DRIVER mode too:
#                                     a driver that cannot interrogate its
#                                     client already fails its own run, so
#                                     making the pin mandatory here would
#                                     only refuse a live-box capture that
#                                     genuinely has no binary to read.
#
# TWO CAPTURE MODES, TWO POLICIES.
#
# Default (live-box): a trace drained from a real session. It genuinely
# cannot observe the pins above, so an empty pin is honest; the landing
# directory is keyed on `request_id`; a missing outgoing structural
# summary warns; scrubbing is write-only.
#
# `--driver-mode`: a hermetic capture produced by a driver that KNOWS
# every pin, so an empty pin is a bug rather than a fact:
#
#   * All five pins are MANDATORY. An unset one aborts the run naming
#     the variable, because an empty case id collapses every case in a
#     lane onto one landing directory and the corpus silently
#     overwrites itself.
#   * ROUTECTL_FIXTURE_EXPECTED_INGRESS is checked against the
#     vocabulary in scripts/drivers/lib/ingress_kinds.sh before any
#     daemon output is read, and then against the TRACED
#     `meta.ingress_kind` before the promoting mv. A client that accepts
#     the runner's connection carriers and ignores them reaches an
#     upstream on its own dialect and lands a fixture that is evidence
#     for the wrong one -- an environment check cannot see that, because
#     the environment said what the run intended. The traced token says
#     what the daemon actually parsed.
#   * Landing directories key on `(lane, case_id)` --
#     `<out>/<lane>/<case_id>/` -- so a rerun of the same case RE-LANDS
#     on the same path and produces a DIFF instead of a fresh sibling.
#     The rerun REPLACES the previous directory: the old one is renamed
#     aside into a `.tmp.stale.*` name, the new one moves into place,
#     then the old one is deleted, so a reader either sees the whole
#     previous fixture or the whole new one. `request_id` stays in
#     meta.json for traceability, it just no longer names the directory.
#     A case id therefore has to be a path-safe SCENARIO name
#     (`tools-multiturn-01`) and must never be derived from the
#     environment -- a hostname or a real path in it is personal data
#     that the scrub gate below refuses, since `--check` scans
#     meta.json too.
#   * A missing structural summary on either direction FAILS that
#     fixture: half the structural evidence is not a canonical fixture.
#   * `scrub-fixture.sh --check` runs after `--write` and a non-zero
#     exit refuses the promotion. A driver fixture is canonical by
#     construction or it is not landed.
#   * The recorded wire-pattern claim is a SELECTOR, not a run gate. One
#     agentic turn produces SEVERAL completed upstream requests, and the
#     case's claim is the statement of WHICH of them the case means. A
#     candidate whose captured bytes do not exhibit the claim is SKIPPED
#     and counted; the next candidate is considered. Candidates are
#     examined in first-`ingress request body` order (see in_scope_ids),
#     and THE FIRST candidate satisfying the claim is the one the case
#     pins. First-match encodes no assumption about whether a claim grows
#     or shrinks across a turn, and it is the MINIMAL witness -- the
#     smallest body that proves the shape, which matters because a tool
#     loop's later requests carry strictly more `tool_result` bytes.
#     "Richest" and "last" are timing properties rather than wire shapes,
#     and would pin a fixture to client chattiness, so two deployments'
#     fixtures for one case would stop being comparable.
#
#     STAGE ALL, VALIDATE ALL, PROMOTE ONCE. Every candidate is staged,
#     scrubbed, checked and validated; only the SELECTED staged directory
#     is retained, and a single promotion plus a single manifest append
#     happen after the whole scan succeeds. Every gate below is computed
#     from the CANDIDATE's own bytes and trace lines -- the seam check
#     from its header capture, the
#     expected-ingress pin from its traced dialect -- so a later
#     candidate can refuse the run for a reason the selected candidate
#     passed. Promoting as the scan ran meant such a refusal deleted the
#     evidence the run had already landed correctly; with nothing promoted
#     while candidates are still being examined, that is structurally
#     impossible rather than merely unwound.
#
#     A LATER CANDIDATE THAT ALSO SATISFIES THE CLAIM IS REDUNDANT, not
#     ambiguous: a tool loop resends its `tool_use` / `tool_result` pair
#     in every later request, so a monotone claim is satisfied by every
#     candidate after the first witness. It is counted separately from a
#     skip (`candidates_redundant`), because a reader seeing two skips
#     would conclude two requests failed the claim when one failed and
#     one was a redundant witness.
#
#     A redundant match is accepted only as a strict CONTINUATION of the
#     selected one: same traced ingress kind, same normalized lane and raw
#     provider kind, same model, and a turn list strictly LONGER. A client
#     retry, a fallback attempt carrying the same history, and a stray
#     side-request are none of those, and a trace holding two genuinely
#     different interactions under one case id is refused here -- that is
#     what keeps "one case id pins one interaction" enforced by something.
#
#     The recorded connection mode is still enforced against the captured
#     ingress headers: a `front-proxy` fixture whose headers do not carry
#     the MITM seam header name never transited the seam whatever its
#     environment said, and a `base-url` fixture that carries it did.
#     Either mismatch REFUSES THE RUN -- the seam is a property of the run,
#     identical for every request in the trace, so skipping past it would
#     only reach the same verdict one candidate later.
#
#     PER-REQUEST FACTS MAY SKIP, PER-RUN FACTS ABORT. Two per-request
#     facts among the gates. The wire pattern is one. The other is LANE
#     RESOLUTION: the lane comes from THIS request's traced
#     `provider_kind`, so a candidate whose lane will not resolve says
#     nothing about a candidate already selected off its own lines -- with
#     a selection held it is a SKIP, and with none held it is still the
#     refusal, because that candidate is the one the case would pin. A
#     missing structural summary, an unclassified lane, seam/mode
#     incoherence and an unexpected ingress dialect remain properties of
#     the RUN. ONE deliberate exception, named as one: scrub `--check`
#     residue is per-request and stays FATAL, because silently skipping
#     past a body that carried a credential shape would turn the loudest
#     safety signal in the pipeline into a per-request footnote. Staging
#     every candidate is what keeps that exception meaningful: a `break` on
#     the first match would leave the rest of the turn unscrubbed and
#     unchecked.
#   * The two independent statements of the CLIENT VERSION -- the wire
#     value parsed from the client-controlled `user-agent` and the
#     binary-side value the driver read off the running client -- are
#     compared by `scripts/drivers/lib/client_version.py`, and a
#     DISAGREEMENT refuses the promotion. Either value ABSENT is recorded
#     as absent and promotes: the pair is unprovable, not contradicted.
#   * The lane must be CLASSIFIED by the scrub gate --
#     `scrub-fixture.sh --lane-known <lane>` exits non-zero and the
#     fixture does not land. A `--check` pass on a lane whose credential
#     shape nobody has classified proves nothing, so an unclassified
#     lane fails closed. A lane the gate lists as having no
#     prefix-detectable shape (with the reason recorded) counts as
#     classified: that is a verdict, not ignorance.
#
# Exit codes:
#   0  the run completed; `captured=<n>` on stdout
#   1  a pin was unset (driver mode), the trace log was unreadable, or a
#      fixture was refused -- a refusal aborts the RUN, so a driver
#      runner reads any non-zero exit as "this case produced no fixture".
#      In driver mode this also covers "candidates existed but NONE
#      exhibited the claim": the case describes a shape its own run did
#      not produce, which is a case defect and never retryable. And it
#      covers a later candidate that exhibits the claim but is NOT a
#      continuation of the selected one -- a trace holding two different
#      interactions under one case id, equally a case defect.
#   2  usage error, or an --out outside the captured tree
#   3  DRIVER MODE ONLY: the run completed, refused nothing, and landed
#      zero fixtures because there was NO candidate at all -- the driven
#      case produced no completed request (a 429, an upstream that
#      returned no success body, a client that died before sending).
#      Deliberately NOT folded into 1: this is retryable, while "we
#      refused a fixture we produced" never is, and neither is "the case
#      claims a shape none of its requests carried". In LIVE-BOX mode
#      zero captures is the normal quiet-window answer and exits 0.
# --- END USAGE ---

# set -e so a partial-fixture mid-write failure aborts the script
# instead of silently writing a half-poisoned directory. set -u
# catches typo'd variable references early. pipefail is NOT used:
# several pipelines below intentionally early-terminate the
# producer (`grep ... | head -1`), which leaves the producer with
# a SIGPIPE-induced non-zero exit under pipefail and triggers
# set -e abort on a successful capture path.
set -eu

LOG="/tmp/routectl-trace.log"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/crates/routectl-cli/tests/fixtures/captured"
LIMIT=0
FORCE=0
ALLOW_UNSAFE_OUT=0
DRIVER_MODE=0

# Landing paths promoted by this run, space-delimited. Only driver mode
# reads it: two requests in one driver trace share the run's single case
# id, so without this the second would overwrite the first.
DRIVER_LANDED=""

# write_fixture's return code for "this candidate does not exhibit the
# claimed wire pattern". Distinguished from every other non-zero return so
# the capture loop can SKIP the candidate and consider the next one, while
# re-raising anything else. Every other refusal in write_fixture is a
# property of the RUN and returns 1, which the loop propagates.
#
# 90 rather than a low number: the loop re-raises an unrecognized code as
# the script's own exit status, and the low range is spoken for by the
# documented exit codes above.
PATTERN_MISMATCH_RC=90

# write_fixture's return code for "this candidate satisfies the claim, but
# an earlier candidate already did". Distinguished from a pattern mismatch
# because the two say opposite things about the same case: a mismatch is a
# request that does not carry the shape, a REDUNDANT match is a further
# witness of the shape the selected candidate already proved. A tool loop
# resends its `tool_use` / `tool_result` pair in every later request, so
# under a monotone claim redundancy is the NORMAL shape of a multi-turn
# capture rather than a defect. Adjacent to the mismatch code for the same
# reason it is not in the low range.
REDUNDANT_MATCH_RC=91

# write_fixture's return code for "this candidate's own traced
# provider_kind resolved to no lane, and an earlier candidate is already
# selected".
#
# THE LANE IS A PER-REQUEST FACT. It is computed from THIS request's own
# trace lines -- its `provider_kind` field, which names the provider the
# alias arm resolved to for this request -- so a candidate whose lane will
# not resolve says nothing about the selected candidate, whose lane resolved
# from its own lines. Treating it as a run-level refusal is what let a LATER
# candidate discard an already-gated selection: promote-once meant nothing
# was corrupted, but the run still ended with no fixture when a valid one
# had been chosen. So it joins the pattern mismatch as a SKIP.
#
# ONLY when a selection already exists. A candidate reaching the lane gate
# with nothing selected yet IS the request the case would pin, and a fixture
# nothing can gate is refused rather than skipped past -- the fail-closed
# reading is unchanged for the request that would land.
#
# Distinct from the mismatch code because the two are different facts about
# the candidate and the selection line counts them the same way only by
# choice: a lane that will not resolve is counted as a skip, since a reader
# needs "one candidate was not usable" and the two reasons are both in the
# rig's own warning above it.
LANE_UNRESOLVED_RC=92

# Selector accounting, driver mode only. Every completed request in the
# trace is a CANDIDATE for the run's single case id; the recorded wire
# pattern decides which one the case means, and the FIRST satisfying
# candidate is the selection. Reported as one structured line at the end
# of the run.
#
# `candidates_redundant` is its own counter and never folded into
# `candidates_skipped`: a reader seeing two skips would conclude two
# requests failed the claim, when one failed and one was a further witness
# of the shape that landed.
CANDIDATES_EXAMINED=0
CANDIDATES_SKIPPED=0
CANDIDATES_REDUNDANT=0
SELECTED_REQUEST_ID=""

# The staged (not yet promoted) directory of the SELECTED candidate, its
# landing path, and the manifest line it will append -- all held until the
# scan over every candidate has succeeded. Empty until a candidate
# satisfies the claim; only ONE candidate ever fills them, because the
# selection is the first match.
SELECTED_STAGED=""
SELECTED_DST=""
SELECTED_MANIFEST_LINE=""

# The selected candidate's COMPLETION timestamp, which becomes the resume
# marker's value once the promotion lands. Under promote-once the marker is
# written after the scan rather than as each candidate is examined, so it
# has to be carried out of the loop rather than read off the loop variable.
SELECTED_TS=""

# The selected candidate's identity fields, kept for the continuation
# comparison a later redundant match is held to. Every one is already
# computed per candidate by write_fixture, so the check is a comparison
# rather than new plumbing.
SELECTED_IKIND=""
SELECTED_LANE=""
SELECTED_PKIND=""
SELECTED_MODEL=""
SELECTED_TURNS=0

# Byte length of manifest.jsonl before this run appended anything, so a
# driver run ending in a refusal can put the append-only file back. Set
# once the landing root exists.
MANIFEST_BYTES_BEFORE=0

# Discard whatever this run staged but has not promoted. Under promote-once
# nothing is promoted while candidates are still being examined, so a
# refusal from any candidate -- including one AFTER the selected candidate
# -- has no promoted fixture to destroy and no manifest line to retract.
# What remains to clean up is the staged directory the selection was
# holding, which the startup sweep would prune on the next run anyway; it
# is removed here so a refusal leaves the landing root as it found it.
#
# Driver mode only. Live-box mode promotes each capture as it goes and
# stages nothing across candidates: it keys on request_id, so each of its
# captures is independent evidence and a later refusal says nothing about
# an earlier one.
#
# The manifest restore is retained rather than deleted: the append now
# happens once after the scan, so under promote-once this is belt to
# promote-once's braces, and a future edit that reintroduces an in-scan
# append would otherwise silently lose the guarantee.
discard_driver_staging() {
  [ "$DRIVER_MODE" = 1 ] || return 0
  local dst
  for dst in $DRIVER_LANDED; do
    rm -rf "$dst"
  done
  DRIVER_LANDED=""
  if [ -n "$SELECTED_STAGED" ]; then
    rm -rf "$SELECTED_STAGED"
    SELECTED_STAGED=""
  fi
  if [ -f "$OUT/manifest.jsonl" ]; then
    # truncate(1) is not assumed present: the manifest is append-only, so
    # the prefix is copied forward and renamed over, the same shape the
    # resume marker uses.
    local keep
    keep="$(mktemp "$OUT/.tmp.manifest.XXXXXX")" || return 0
    head -c "$MANIFEST_BYTES_BEFORE" "$OUT/manifest.jsonl" > "$keep" 2>/dev/null || :
    mv "$keep" "$OUT/manifest.jsonl" || rm -f "$keep"
  fi
}

# The driver-mode selection record: which candidate this run pinned, how
# many it examined, how many it skipped, how many were redundant witnesses,
# and the ordering basis it used. Emitted on BOTH the success path and the
# refusal path -- a run that ends in a refusal is the one where a reader
# most needs to know which candidate was reached, and reconstructing that
# by correlating request ids across a log is the work this line exists to
# remove.
#
# NO BODY CONTENT: every field is an id, a count, or the fixed basis token.
# A rig log is a CI artifact, and a body is unscrubbed at the point a
# refusal prints.
emit_selection_line() {
  echo "capture_fixtures: selection case=$ROUTECTL_FIXTURE_CASE_ID selected_request_id=${SELECTED_REQUEST_ID:-none} candidates_examined=$CANDIDATES_EXAMINED candidates_skipped=$CANDIDATES_SKIPPED candidates_redundant=$CANDIDATES_REDUNDANT ordering_basis=first-ingress-body"
}

# Print the header block as usage. Delimited by a sentinel rather than a
# line count: a magic `1,NNp` range silently starts cutting content the
# moment the header grows, and the driver-mode policy is exactly the part
# a caller needs to read.
usage() {
  sed -n '2,/^# --- END USAGE ---$/p' "$0" | sed '$d'
}

# Workspace package version, stamped into every meta.json + manifest
# entry for forward-compat. Pulled once at startup from the workspace
# Cargo.toml so a version bump mid-run cannot mix versions across one
# capture batch.
ROUTECTL_VERSION="$(grep -E '^version = ' "$ROOT/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')"

# Fixture-format major written into every meta.json. Must match
# FIXTURE_SCHEMA_VERSION in
# crates/routectl-cli/tests/common/replay/loader.rs, which refuses any
# other major outright.
SCHEMA_VERSION=1

# The single owner of fixture scrubbing. Every fixture is passed through
# `--write` before promotion; a driver landing a fixture runs `--check`
# afterwards to refuse anything the write pass could not safely rewrite.
# Absent script is a hard failure, never an unscrubbed capture.
SCRUB="$ROOT/scripts/scrub-fixture.sh"
if [ ! -r "$SCRUB" ]; then
  echo "capture_fixtures: scrub script not found at $SCRUB; refusing to capture" >&2
  exit 1
fi

# The predicate side of the recorded wire-pattern claim: one predicate per
# token in the closed vocabulary, read off the fixture's own captured
# bytes. Only driver mode runs it, but the prerequisite is checked here
# with the others -- an absent predicate is a hard failure, never an
# unverified promotion, for the same reason the scrub script is.
VERIFY_PATTERN="$ROOT/scripts/drivers/lib/verify_pattern.py"
if [ ! -r "$VERIFY_PATTERN" ]; then
  echo "capture_fixtures: wire-pattern predicate not found at $VERIFY_PATTERN; refusing to capture" >&2
  exit 1
fi

# The comparison side of the two client-version statements. Same fail-closed
# shape as the predicate above: an absent comparator would leave the
# client-controlled user-agent as an unchecked claim on every landed
# fixture, which is the defect it exists to close.
CLIENT_VERSION="$ROOT/scripts/drivers/lib/client_version.py"
if [ ! -r "$CLIENT_VERSION" ]; then
  echo "capture_fixtures: client-version comparator not found at $CLIENT_VERSION; refusing to capture" >&2
  exit 1
fi

# The single owner of --out path confinement, shared with every other
# script that writes capture output to a caller-supplied directory.
# Absent library is a hard failure, never an unconfined write.
CONFINE_LIB="$ROOT/scripts/drivers/lib/confine.sh"
if [ ! -r "$CONFINE_LIB" ]; then
  echo "capture_fixtures: confinement library not found at $CONFINE_LIB; refusing to capture" >&2
  exit 1
fi
# shellcheck source=scripts/drivers/lib/confine.sh
. "$CONFINE_LIB"

# The single owner of the ingress-dialect vocabulary, shared with the
# runner and the promote script. Absent library is a hard failure, never
# an unvalidated expected-ingress pin: a pin nothing checks against the
# vocabulary would accept a typo, and a typo can never equal a traced
# token, so every capture would refuse for a reason that names the wrong
# fault.
INGRESS_KINDS_LIB="$ROOT/scripts/drivers/lib/ingress_kinds.sh"
if [ ! -r "$INGRESS_KINDS_LIB" ]; then
  echo "capture_fixtures: ingress vocabulary not found at $INGRESS_KINDS_LIB; refusing to capture" >&2
  exit 1
fi
# shellcheck source=scripts/drivers/lib/ingress_kinds.sh
. "$INGRESS_KINDS_LIB"

# Escape a value for use inside a JSON string literal. The meta.json and
# manifest lines below are emitted by hand (no jq dependency), so every
# interpolated STRING value must come through here or a value carrying a
# quote silently produces invalid JSON -- the rig would exit 0, promote
# the fixture, and the loader would skip it forever. Both reachable
# sources are real: the environment pins are set programmatically by a
# driver, and the client name / version are parsed out of a
# CLIENT-CONTROLLED `user-agent` header.
#
# Backslash FIRST, then quote, so the backslash pass cannot double-escape
# the backslash the quote pass just introduced. Control characters are
# not handled: the values here come from single trace-log lines and from
# environment variables, neither of which can carry a raw newline into a
# field without breaking the extraction upstream of this point.
json_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

# The tracing target and the message set that OWN the provider vocabulary
# normalize_lane maps. REPLICAS of the log_safe.rs module path and of the
# four call sites that pass a `provider_kind` field: `trace_outgoing_body`,
# `trace_upstream_success_body`, and the two header traces (HDR_MSG_OUTGOING
# / HDR_MSG_UPSTREAM). Replicas because the rig runs in throwaway trees
# carrying scripts/ and no crates/; the self-test welds each spelling to the
# Rust source where that tree exists.
#
# `upstream error body` is deliberately ABSENT: it is a DEBUG line on the
# failure path, and a request whose upstream errored has no completion
# marker, so it is never a candidate. Adding it would widen the anchor to a
# line no in-scope request emits.
PROVIDER_KIND_TARGET="routectl_core::log_safe: "
PROVIDER_KIND_MESSAGES="outgoing request body
upstream success body
outgoing request headers
upstream response headers"

# Normalize a traced `provider_kind` into the LANE vocabulary --
# `ProviderEntry::kind_str()` in
# crates/routectl-router/src/config/schema.rs, the vocabulary a lane's
# class is derived from. Three spellings exist for one concept: the
# providers-crate PROVIDER_KIND consts (`anthropic`), the two Bedrock
# api_shape kinds (`bedrock-invoke` / `bedrock-converse`, which
# kind_str() does not split), and the config tokens. Normalizing HERE
# means no consumer needs a mapping table.
#
# An unmapped token prints a warning and yields the EMPTY string rather
# than passing an unknown spelling through: a lane-gated consumer
# refuses a fixture with no lane, so the fixture is still captured but
# cannot be mistaken for a lane it was not verified to be.
#
# The two Bedrock spellings collapse to one lane here, and that is the
# ONLY place the api-shape distinction is lost -- `meta.provider_kind`
# keeps the raw token, so a consumer that needs to tell an invoke capture
# from a converse one reads it there. There is deliberately no separate
# api_shape field: it would duplicate a token already on disk.
normalize_lane() {
  case "$1" in
    anthropic)                        printf 'anthropic-api\n' ;;
    openai-compat)                    printf 'openai-compat\n' ;;
    openai-responses)                 printf 'openai-responses\n' ;;
    gemini)                           printf 'gemini\n' ;;
    bedrock-invoke|bedrock-converse)  printf 'bedrock\n' ;;
    *)
      echo "capture_fixtures: unmapped provider_kind '$1'; leaving lane empty" >&2
      printf '\n'
      ;;
  esac
}

# Name of the MITM front-proxy seam header, as spelled in
# REDACT_HEADER_NAMES in crates/routectl-core/src/log_safe.rs -- which is
# why a captured ingress header set retains the NAME while its value is
# redacted. A REPLICA, because the rig runs in throwaway trees that carry
# scripts/ and no crates/ (the self-test asserts the two spellings agree).
MITM_SEAM_HEADER="x-routectl-mitm-proxied"

# Captured ingress headers of a fixture directory. The header files hold a
# JSON ARRAY of [name, value] pairs, so they are PARSED rather than
# grepped: a substring hit anywhere in a value would answer a question
# about names.
INGRESS_HEADERS_FILE="ingress_request.headers.json"

# The captured inbound body of a fixture directory, and the two keys a turn
# list can live under -- `messages` for the Anthropic and chat-completions
# shapes, `input` for the Responses shape. REPLICAS of INGRESS_BODY_FILE,
# ANTHROPIC_TURNS_KEY and RESPONSES_TURNS_KEY in
# scripts/drivers/lib/verify_pattern.py, because the rig runs in throwaway
# trees and reading a Python constant out of a sibling script at runtime
# would make the census depend on that file's layout rather than on its
# values. The self-test asserts the spellings agree.
INGRESS_BODY_FILE="ingress_request.json"
ANTHROPIC_TURNS_KEY="messages"
RESPONSES_TURNS_KEY="input"

# Does the fixture's captured ingress header set carry $MITM_SEAM_HEADER?
# 0 yes, 1 no, 2 the question could not be answered from the file.
#
# The match is on the NAME, case-insensitively: by the time this runs the
# value is a redaction placeholder, so a value comparison would test the
# scrub gate rather than the fixture's provenance.
ingress_carries_seam_header() {
  python3 - "$1" "$MITM_SEAM_HEADER" <<'PY'
import json
import sys

path, needle = sys.argv[1], sys.argv[2].lower()
shape = "captured ingress headers are not a JSON array of [name, value] pairs"
try:
    with open(path, encoding="utf-8") as handle:
        pairs = json.load(handle)
except (OSError, UnicodeDecodeError, ValueError) as exc:
    print(f"capture_fixtures: unreadable {path}: {exc}", file=sys.stderr)
    sys.exit(2)
if not isinstance(pairs, list):
    print(f"capture_fixtures: {shape}: {path}", file=sys.stderr)
    sys.exit(2)
for pair in pairs:
    if not isinstance(pair, list) or not pair:
        print(f"capture_fixtures: {shape}: {path}", file=sys.stderr)
        sys.exit(2)
    if str(pair[0]).lower() == needle:
        sys.exit(0)
sys.exit(1)
PY
}

# Refuse a staged fixture whose captured ingress headers contradict the
# connection mode it records. An environment carrier proves INTENT; the
# seam header is the only evidence of TRANSIT, so a client that silently
# fell back to a direct connection is caught here rather than landing as a
# front-proxy fixture whose shape is base-url.
#
# Both directions, and the reverse one is what stops the gate being
# satisfiable by a check that only ever looks at front-proxy runs. A mode
# outside these two is refused upstream by the driver, so there is
# deliberately no third arm here.
assert_mode_seam_coherent() {
  local tmp="$1" id="$2"
  case "$ROUTECTL_FIXTURE_CONNECTION_MODE" in
    front-proxy|base-url) : ;;
    *) return 0 ;;
  esac

  local seam_rc=0
  ingress_carries_seam_header "$tmp/$INGRESS_HEADERS_FILE" || seam_rc=$?
  case "$seam_rc" in
    0)
      if [ "$ROUTECTL_FIXTURE_CONNECTION_MODE" = base-url ]; then
        echo "capture_fixtures: $id records connection mode 'base-url' but its captured ingress" >&2
        echo "headers carry $MITM_SEAM_HEADER: the run DID transit the MITM listener; not promoting." >&2
        return 1
      fi
      ;;
    1)
      if [ "$ROUTECTL_FIXTURE_CONNECTION_MODE" = front-proxy ]; then
        echo "capture_fixtures: $id records connection mode 'front-proxy' but its captured ingress" >&2
        echo "headers carry no $MITM_SEAM_HEADER: the run did not transit the MITM listener," >&2
        echo "whatever its environment said; not promoting." >&2
        return 1
      fi
      ;;
    *)
      echo "capture_fixtures: cannot read $id's captured ingress headers, so its connection mode" >&2
      echo "'$ROUTECTL_FIXTURE_CONNECTION_MODE' is unprovable; not promoting." >&2
      return 1
      ;;
  esac
  return 0
}

# Refuse a staged fixture whose two statements of the client version
# CONTRADICT each other. `client.version` is parsed from the ingress
# `user-agent` -- the client's own self-report, which the client controls;
# `client.binary_version` was read off the running binary by the driver
# before any session opened. Two reads of one client, so a disagreement
# means the fixture is not evidence about either version, and the shape
# that produces one is a client that auto-updated mid-run.
#
# ABSENCE ON EITHER SIDE PROMOTES. A live-box capture has no binary to
# read, and a client whose user-agent carries no version has said nothing
# to contradict; the comparator reports that as NOT COMPARABLE (exit 3) and
# the absence is already recorded in meta.json as an empty field. Refusing
# it would refuse the client rather than the contradiction.
assert_client_version_coherent() {
  local id="$1" binary="$2" wire="$3"

  local cmp_rc=0
  python3 "$CLIENT_VERSION" --compare "$binary" "$wire" || cmp_rc=$?
  case "$cmp_rc" in
    0|3) return 0 ;;
    1)
      echo "capture_fixtures: $id records client version '$wire' on the wire but its driver" >&2
      echo "read '$binary' off the binary; not promoting the fixture." >&2
      return 1
      ;;
    *)
      echo "capture_fixtures: the client-version comparator could not run for $id, so the" >&2
      echo "two version statements are unchecked; not promoting the fixture." >&2
      return 1
      ;;
  esac
}

# Length of a staged fixture's captured ingress turn list, or 0 when the
# body is absent or unparseable. The key differs by dialect -- `messages`
# for the Anthropic and chat-completions shapes, `input` for the Responses
# shape -- and the census reads whichever one the body ACTUALLY carries,
# the same rule the wire-pattern predicate uses so the two cannot disagree
# about what a turn list is.
#
# An unreadable body yields 0 rather than failing: this feeds the
# continuation comparison, where "strictly longer" is the test, and 0 can
# never be strictly longer than a selected candidate's count. A body the
# rig could not parse therefore fails the comparison rather than passing it
# by default.
ingress_turn_count() {
  python3 - "$1" "$ANTHROPIC_TURNS_KEY" "$RESPONSES_TURNS_KEY" <<'PY'
import json
import sys

path = sys.argv[1]
keys = sys.argv[2:]
try:
    with open(path, encoding="utf-8") as handle:
        body = json.load(handle)
except (OSError, UnicodeDecodeError, ValueError):
    print(0)
    sys.exit(0)
if not isinstance(body, dict):
    print(0)
    sys.exit(0)
for key in keys:
    turns = body.get(key)
    if isinstance(turns, list):
        print(len(turns))
        sys.exit(0)
print(0)
PY
}

# Refuse a REDUNDANT match that is not a strict CONTINUATION of the
# selected candidate. This is the surviving job of the old already-landed
# refusal: two requests satisfying one monotone claim is the normal shape
# of a tool loop, but a trace holding two genuinely DIFFERENT interactions
# under one case id is not, and nothing else in the pipeline would catch
# it.
#
# Continuation means: the same traced ingress dialect, the same normalized
# lane and the same raw provider kind, the same model, and a turn list
# strictly LONGER than the selected candidate's. A client retry and a
# fallback attempt carry the SAME history, so neither is strictly longer;
# an alias arm that resolved to a different provider changes the lane or
# the raw kind; a stray side-request changes the model or the dialect.
#
# Every field compared here was already computed for the candidate on the
# way to this point, so this is a comparison rather than new plumbing.
assert_redundant_is_continuation() {
  local id="$1" ikind="$2" lane="$3" pkind="$4" model="$5" turns="$6"

  local mismatch=""
  [ "$ikind" = "$SELECTED_IKIND" ] || mismatch="ingress dialect"
  [ "$lane" = "$SELECTED_LANE" ] || mismatch="${mismatch:-lane}"
  [ "$pkind" = "$SELECTED_PKIND" ] || mismatch="${mismatch:-provider kind}"
  [ "$model" = "$SELECTED_MODEL" ] || mismatch="${mismatch:-model}"
  if [ -n "$mismatch" ]; then
    echo "capture_fixtures: $id also exhibits the claim of case '$ROUTECTL_FIXTURE_CASE_ID' but is not a" >&2
    echo "continuation of the selected request $SELECTED_REQUEST_ID: its $mismatch differs, so the trace" >&2
    echo "holds two different interactions under one case id; not promoting anything." >&2
    return 1
  fi
  if [ "$turns" -le "$SELECTED_TURNS" ]; then
    echo "capture_fixtures: $id also exhibits the claim of case '$ROUTECTL_FIXTURE_CASE_ID' but carries" >&2
    echo "$turns ingress turn(s) against the selected request $SELECTED_REQUEST_ID's $SELECTED_TURNS." >&2
    echo "A continuation extends the history, so this is a retry or a side-request rather than a" >&2
    echo "later turn of the same interaction; not promoting anything." >&2
    return 1
  fi
  return 0
}

while [ $# -gt 0 ]; do
  case "$1" in
    --log) [ $# -ge 2 ] || { echo "--log requires a value" >&2; exit 2; }; LOG="$2"; shift 2 ;;
    --out) [ $# -ge 2 ] || { echo "--out requires a value" >&2; exit 2; }; OUT="$2"; shift 2 ;;
    --limit) [ $# -ge 2 ] || { echo "--limit requires a value" >&2; exit 2; }; LIMIT="$2"; shift 2 ;;
    --force) FORCE=1; shift ;;
    --allow-unsafe-out) ALLOW_UNSAFE_OUT=1; shift ;;
    --driver-mode) DRIVER_MODE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# Driver mode fails closed on the five pins. `:?` aborts under `set -u`
# naming the variable, which is the whole point: an unset pin in a driver
# run is a bug in the driver, and an empty case_id would collapse every
# case in the lane onto one landing directory before anyone noticed. The
# live-box path below never reaches this block and keeps today's
# tolerance.
if [ "$DRIVER_MODE" = 1 ]; then
  : "${ROUTECTL_FIXTURE_CASE_ID:?driver mode requires a case id (ROUTECTL_FIXTURE_CASE_ID)}"
  : "${ROUTECTL_FIXTURE_CONFIG_SHA:?driver mode requires a config sha (ROUTECTL_FIXTURE_CONFIG_SHA)}"
  : "${ROUTECTL_FIXTURE_CONNECTION_MODE:?driver mode requires a connection mode (ROUTECTL_FIXTURE_CONNECTION_MODE)}"
  : "${ROUTECTL_FIXTURE_WIRE_PATTERN:?driver mode requires a wire pattern (ROUTECTL_FIXTURE_WIRE_PATTERN)}"
  : "${ROUTECTL_FIXTURE_EXPECTED_INGRESS:?driver mode requires an expected ingress dialect (ROUTECTL_FIXTURE_EXPECTED_INGRESS)}"

  # Checked against the vocabulary HERE, before the trace is read: a pin
  # outside the closed set can never equal a traced token, so leaving it
  # to the comparison below would refuse every capture with a message
  # about a dialect mismatch rather than about the typo that caused it.
  if ! ingress_kind_is_known "$ROUTECTL_FIXTURE_EXPECTED_INGRESS"; then
    echo "capture_fixtures: refusing expected ingress '$ROUTECTL_FIXTURE_EXPECTED_INGRESS':" >&2
    echo "it is not an ingress dialect this build parses ($(ingress_kinds_list))." >&2
    exit 2
  fi

  # The case id becomes two path components' worth of directory name, so
  # a separator or a traversal segment in it would land the fixture
  # outside the lane directory -- past the --out confinement check, which
  # ran on OUT alone.
  case "$ROUTECTL_FIXTURE_CASE_ID" in
    */*|.|..)
      echo "capture_fixtures: refusing case id '$ROUTECTL_FIXTURE_CASE_ID': it names the landing directory," >&2
      echo "so it must be a single path-safe scenario name (e.g. tools-multiturn-01)." >&2
      exit 2
      ;;
  esac
fi

# Confine --out to the default captured dir unless the operator
# explicitly opts out. Fixtures carry RAW headers (auth included when
# the daemon runs with ROUTECTL_TRACE_HEADERS) and the default tree is
# gitignored; writing them into an arbitrary -- possibly git-tracked --
# path risks committing secrets. OUT keeps its lexically-collapsed form
# for the write path; the containment test itself lives in
# scripts/drivers/lib/confine.sh, which is the only copy.
OUT="$(abspath_lexical "$OUT")"
DEFAULT_OUT_ABS="$(abspath_lexical "$ROOT/crates/routectl-cli/tests/fixtures/captured")"
if [ "$ALLOW_UNSAFE_OUT" = 0 ]; then
  confine_out_under "$OUT" "$DEFAULT_OUT_ABS"
fi

mkdir -p "$OUT"
MARKER="$OUT/.last_capture_ts"

# The manifest length this run inherited, read before anything appends, so
# a driver run ending in a refusal can restore it exactly.
if [ -f "$OUT/manifest.jsonl" ]; then
  MANIFEST_BYTES_BEFORE="$(wc -c <"$OUT/manifest.jsonl" | tr -d ' ')"
fi

# Sweep any stale per-request tmp dirs from a prior crashed run.
# We rename atomically (mv tmp -> $OUT/$id) at the end of each
# write_fixture, so any `.tmp.*` left behind here is poison from
# a previous abort. Naming uses `.tmp.<id>.XXXXXX` so the directory
# scan `[ -d "$OUT/$id" ]` (no dot prefix) cannot collide with
# legitimate captures.
find "$OUT" -maxdepth 1 -name '.tmp.*' -type d -exec rm -rf {} + 2>/dev/null || true

if [ ! -r "$LOG" ]; then
  echo "trace log not readable: $LOG" >&2
  exit 1
fi

# Sed pattern that strips ANSI color escapes.
strip_ansi() { sed -E 's/\x1b\[[0-9;]*m//g'; }

# Resume from the last captured timestamp (lexicographic compare works
# on ISO-8601 with fixed-width timestamps).
since="1970-01-01T00:00:00Z"
if [ "$FORCE" = 0 ] && [ -r "$MARKER" ]; then
  since="$(cat "$MARKER" 2>/dev/null)"
  [ -z "$since" ] && since="1970-01-01T00:00:00Z"
fi

stripped="$(mktemp)"
trap 'rm -f "$stripped"' EXIT
strip_ansi < "$LOG" > "$stripped"

# Collect request_ids that have a completion marker after `since` and
# whose trace also carries an `ingress request body` and a
# `provider_kind=` field. Any ingress dialect and any provider_kind
# value is in scope. The completion line carries `request_id=...` and
# a timestamp at the start of the line.
#
# BOTH HARVESTS ARE SCOPED TO THE EMITTER THAT OWNS THE VOCABULARY, and
# for `provider_kind` that is load-bearing rather than tidy. Two
# unrelated emitters spell a field of that name with two different
# vocabularies: the log_safe traces below carry the providers-crate
# PROVIDER_KIND const (`anthropic`), while the router's capability
# observation carries the CONFIG ENTRY's kind_str() (`anthropic-api`).
# Neither is wrong at its own site, and normalize_lane maps only the
# first. A scan on the bare field name is therefore a scan over two
# vocabularies at once: it took whichever line came last, handed the
# config spelling to normalize_lane, and left the lane empty on every
# request where a capability observation acted.
#
# First-wins is NOT the fix. It resolves today's line order by accident
# and inverts the moment an emitter moves. Anchoring on the emitter's own
# target and message -- the way the ingress arm already does -- makes a
# future third emitter of the same field name a no-op here instead of a
# silent lane change.
#
# EACH ROW CARRIES TWO TIMESTAMPS, and the distinction is load-bearing:
#
#   * the ORDERING key is the FIRST `ingress request body` occurrence for
#     the id -- the beginning of the request itself. Keying the order on
#     the COMPLETION marker instead made the order depend on stream flush
#     timing, so two requests initiated in a fixed order could sort either
#     way under a deterministic client. One case id selects one of several
#     candidates, and a selector over a nondeterministic order is a
#     nondeterministic selector.
#   * the COMPLETION timestamp stays the fixture's `captured_at_ts` and
#     the resume marker's value, and it is what `since` filters on: a
#     request is in scope when it COMPLETED after the last capture.
#
# A request whose completion marker is in scope but whose ingress body
# line is absent is not a candidate at all (`ingress_seen`), so the
# ordering key is never empty for a row this prints.
in_scope_ids() {
  awk -v since="$since" \
      -v pk_target="$PROVIDER_KIND_TARGET" \
      -v pk_messages="$PROVIDER_KIND_MESSAGES" '
    BEGIN {
      pk_msg_n = split(pk_messages, pk_msg, "\n")
    }
    /ingress request body ingress="[^"]+"/ {
      if (match($0, /request_id=[0-9a-f-]+/)) {
        id = substr($0, RSTART+11, RLENGTH-11)
        ingress_seen[id] = 1
        # FIRST occurrence only: a retried or re-logged body line must not
        # move a request later in the order.
        if (!(id in ingress_ts)) {
          ingress_ts[id] = substr($0, 1, 27)
        }
        if (match($0, /ingress="[^"]+"/)) {
          ingress_kind[id] = substr($0, RSTART+9, RLENGTH-10)
        }
      }
    }
    /provider_kind="[^"]+"/ {
      # THE EMITTER ANCHOR. Only lines whose message -- at the position
      # right after the FIRST log_safe target -- is one of the
      # provider-vocabulary messages may set this. A capability
      # observation carries the same field name in the config-entry
      # vocabulary and is skipped here, and a body that quotes one of
      # these lines sits past the first target occurrence so it can never
      # select either.
      p = index($0, pk_target)
      if (p > 0 && match($0, /request_id=[0-9a-f-]+/)) {
        id = substr($0, RSTART+11, RLENGTH-11)
        rest = substr($0, p + length(pk_target))
        for (i = 1; i <= pk_msg_n; i++) {
          if (substr(rest, 1, length(pk_msg[i])) != pk_msg[i]) continue
          # First occurrence WITHIN the matched message: the field region
          # precedes body=, so a body copy of the field cannot be read.
          if (match(rest, /provider_kind="[^"]+"/)) {
            provider_kind[id] = substr(rest, RSTART+15, RLENGTH-16)
          }
          break
        }
      }
    }
    /upstream success body|stream summary/ {
      if (match($0, /request_id=[0-9a-f-]+/)) {
        id = substr($0, RSTART+11, RLENGTH-11)
        ts = substr($0, 1, 27)  # ISO-8601 with millisecond precision
        # `>=` (not `>`) so two requests completing in the same
        # millisecond are not dropped on the next run. The
        # `[ -d "$OUT/$id" ] && continue` check downstream
        # prevents re-capturing the request whose ts matches the
        # marker.
        if (ts >= since) {
          completed[id] = ts
        }
      }
    }
    END {
      for (id in completed) {
        if (ingress_seen[id] && provider_kind[id]) {
          # An unextractable ingress token stays EMPTY rather than
          # becoming a sentinel word: empty means unpinned everywhere
          # else in the schema, and `unknown` would be a value in
          # neither the IngressAdapter::id() vocabulary nor the match
          # arms of any consumer. No apostrophes in this comment: the
          # awk program is single-quoted.
          #
          # Field 1 is the ORDERING key (first ingress body) and is
          # dropped by the reader after the sort; field 2 is the
          # completion timestamp the fixture records.
          print ingress_ts[id] "\t" completed[id] "\t" id "\t" provider_kind[id] "\t" ingress_kind[id]
        }
      }
    }
  ' "$stripped" | sort -k1,1 | cut -f2-
}

# Atomically move a staged directory onto its landing path. The single
# owner of the promoting rename, shared by both modes so neither can drift
# into a merge.
#
# Until the rename succeeds the landing directory holds its previous
# contents (or does not exist); an interrupted run leaves a `.tmp.*` dir
# that the next startup sweep removes.
#
# A RE-LANDING REPLACES the previous fixture rather than merging into it:
# the old directory is renamed aside under a `.tmp.` name, the new one moves
# into place, and only then is the old one deleted. A merge would leave
# files from the previous run that this capture never observed -- e.g. an
# `upstream_response.json` from a non-stream run surviving into a stream
# rerun -- and file presence IS the schema, so the drift signal would be
# read off a directory no single capture ever produced. A failed second
# rename restores the previous directory rather than leaving the path with
# none.
#
# The staged directory is left in place on failure: the caller decides
# whether to discard it or hold it, and the startup sweep prunes it either
# way.
promote_staged_dir() {
  local staged="$1" dst="$2" id="$3"
  if [ -d "$dst" ]; then
    local stale="$OUT/.tmp.stale.$id.$$"
    mv "$dst" "$stale" || return 1
    if ! mv "$staged" "$dst"; then
      mv "$stale" "$dst" || true
      return 1
    fi
    rm -rf "$stale"
    return 0
  fi
  mv "$staged" "$dst" || return 1
}

# For one request_id, write its fixture directory.
#
# Atomicity contract: writes go to a per-request tmp directory
# `$OUT/.tmp.<id>.XXXXXX`. A crash before the promotion leaves a `.tmp.*`
# directory behind, which the startup sweep prunes on the next run -- the
# landing directory is never half-populated, so the
# `[ -d "$OUT/$id" ] && continue` idempotency guard in the caller can be
# trusted.
#
# WHERE THE PROMOTION HAPPENS DIFFERS BY MODE, and that is the whole shape
# of driver-mode transactionality:
#
#   * LIVE-BOX mode promotes here, at the end of this function, and appends
#     its manifest line right after the rename. Each of its captures keys on
#     its own request_id and is independent evidence, so there is nothing to
#     hold back and nothing a later capture's failure says about an earlier
#     one.
#   * DRIVER mode does NOT promote here. It RETAINS the staged directory of
#     the SELECTED candidate (plus the manifest line it would append) and
#     returns; the caller promotes once, after every candidate has been
#     examined. Every gate in this function is computed from the candidate's
#     own bytes and trace lines, so a later candidate can refuse the run for
#     a reason the selected candidate passed -- and promoting mid-scan meant
#     such a refusal destroyed the correct fixture the run had already
#     landed.
#
# ERREXIT IS NOT ARMED IN THIS BODY. The caller captures the return status
# so it can skip a non-matching candidate, and bash disables errexit for
# the whole dynamic extent of a status-tested call -- `set -e` here or a
# subshell around the call does not restore it (measured, bash 5.3). So
# every step whose failure would leave a partial or wrongly-promoted
# fixture carries its own `|| return 1`; the caller re-raises that 1 as
# the run's exit status.
write_fixture() {
  local id="$1" ts="$2" pkind="$3" ikind="${4:-}"
  local tmp
  # An unguarded mktemp would leave $tmp empty and send every write below
  # to an absolute path at the filesystem root.
  tmp="$(mktemp -d "$OUT/.tmp.$id.XXXXXX")" || return 1

  # Pull every line for this request.
  local lines
  lines="$(grep -F "request_id=$id" "$stripped" || true)"
  if [ -z "$lines" ]; then
    rm -rf "$tmp"
    return 0
  fi

  # meta.json: high-level fields.
  local alias model
  alias="$(echo "$lines" | grep -oE '_with_options\{alias=[a-z0-9_-]+' | head -1 | sed 's/.*alias=//')"
  model="$(echo "$lines" | grep -oE '(complete|stream)\{provider=[^ ]+ model=[a-zA-Z0-9._:/-]+' | head -1 | sed 's/.*model=//')"
  local stream_flag finish input_tokens output_tokens total_tokens
  stream_flag="false"
  if echo "$lines" | grep -q 'stream summary'; then
    stream_flag="true"
    local ss
    ss="$(echo "$lines" | grep 'stream summary direction="upstream"' | head -1)"
    finish="$(echo "$ss" | grep -oE 'finish_reason="[^"]+"' | cut -d= -f2 | tr -d '"' || echo unknown)"
    input_tokens="$(echo "$ss" | grep -oE 'prompt_tokens=[0-9]+' | cut -d= -f2 || echo 0)"
    output_tokens="$(echo "$ss" | grep -oE 'completion_tokens=[0-9]+' | cut -d= -f2 || echo 0)"
    total_tokens="$(echo "$ss" | grep -oE 'total_tokens=[0-9]+' | cut -d= -f2 || echo 0)"
  fi

  # Body extraction. The tracing line layout is
  #
  #   TS LEVEL spans: routectl_core::log_safe: <message> <fields...> body=<JSON> redact_prompts_enabled=<bool>
  #
  # We ANCHOR on the structural message position: find the first
  # `routectl_core::log_safe: ` (the tracing target, emitted exactly once
  # per line) and require the message immediately after it to START with
  # the needle. This is what keeps capture correct even when the request
  # BODY itself contains the needle text or a quoted routectl log line
  # (e.g. a coding session about routectl's own logging): content copies
  # live after `body=`, past the first target occurrence, so they can
  # never select the wrong line. Within the matched message we take the
  # FIRST `body=` (the real field -- the message text and the fields
  # before it carry no `body=`) to end-of-line, then strip the trailing
  # `redact_prompts_enabled=<bool>` field.
  extract_body() {
    local needle="$1"
    echo "$lines" | awk -v needle="$needle" -v target="routectl_core::log_safe: " '
      {
        p = index($0, target)
        if (p == 0) next
        mstart = p + length(target)
        if (substr($0, mstart, length(needle)) != needle) next
        rest = substr($0, mstart)
        i = index(rest, "body=")
        if (i == 0) next
        val = substr(rest, i + 5)
        sub(/ redact_prompts_enabled=(true|false)$/, "", val)
        print val
        exit
      }
    '
  }

  # Header extraction. Reciprocal of the parsing contract documented in
  # crates/routectl-core/src/log_safe.rs (the header-trace section): the
  # four canonical needles are the HDR_MSG_* consts there, and `headers`
  # is the LAST field on the line. Same structural anchoring as
  # extract_body -- match the needle only at the message position right
  # after the first `routectl_core::log_safe: `, so a header needle never
  # selects a body line (or a body-content copy of the needle) -- then
  # take everything after the first `headers=` to end-of-line (only a
  # trailing space to strip; header lines carry no `redact_prompts_enabled`).
  extract_headers() {
    local needle="$1"
    echo "$lines" | awk -v needle="$needle" -v target="routectl_core::log_safe: " '
      {
        p = index($0, target)
        if (p == 0) next
        mstart = p + length(target)
        if (substr($0, mstart, length(needle)) != needle) next
        rest = substr($0, mstart)
        i = index(rest, "headers=")
        if (i == 0) next
        val = substr(rest, i + 8)
        sub(/[[:space:]]+$/, "", val)
        print val
        exit
      }
    '
  }

  local b_ing b_out b_uok b_egr
  b_ing="$(extract_body 'ingress request body')"
  b_out="$(extract_body 'outgoing request body')"
  b_uok="$(extract_body 'upstream success body')"
  b_egr="$(extract_body 'egress response body')"

  [ -n "$b_ing" ] && echo "$b_ing" > "$tmp/ingress_request.json"
  [ -n "$b_out" ] && echo "$b_out" > "$tmp/outgoing_request.json"
  [ -n "$b_uok" ] && echo "$b_uok" > "$tmp/upstream_response.json"
  [ -n "$b_egr" ] && echo "$b_egr" > "$tmp/egress_response.json"

  # Header fixtures (opt-in via ROUTECTL_TRACE_HEADERS on the daemon).
  # Each is written only when its trace line was present, mirroring
  # the body files above.
  local h_ing h_out h_uok h_egr
  h_ing="$(extract_headers 'ingress request headers')"
  h_out="$(extract_headers 'outgoing request headers')"
  h_uok="$(extract_headers 'upstream response headers')"
  h_egr="$(extract_headers 'egress response headers')"

  [ -n "$h_ing" ] && echo "$h_ing" > "$tmp/ingress_request.headers.json"
  [ -n "$h_out" ] && echo "$h_out" > "$tmp/outgoing_request.headers.json"
  [ -n "$h_uok" ] && echo "$h_uok" > "$tmp/upstream_response.headers.json"
  [ -n "$h_egr" ] && echo "$h_egr" > "$tmp/egress_response.headers.json"

  # Client identity. Name + version come from the captured ingress
  # `user-agent` (`claude-cli/2.1.167 (external, cli)` -> `claude-cli` +
  # `2.1.167`), the only client self-report on the wire. A live-box
  # capture cannot observe its own connection mode, case identity, or
  # config hash, so those come from the environment and stay EMPTY when
  # unset -- an empty pin is honest, a guessed one reads as drift later.
  local client_name client_version
  client_name="$(printf '%s' "$h_ing" | grep -oE '\["user-agent","[^"/]+' | head -1 | sed 's/.*,"//')"
  client_version="$(printf '%s' "$h_ing" | grep -oE '\["user-agent","[^"/]+/[0-9][^ "]*' | head -1 | sed 's#.*/##')"
  # The driver-side half of the client identity: the version read off the
  # RUNNING binary, forwarded by the runner before the run workspace was
  # removed. EMPTY is a real value here -- a live-box capture has no binary
  # to read -- and it is recorded as empty rather than backfilled from the
  # wire, because a field that silently mirrors its own counterpart cannot
  # contradict it.
  local client_binary_version="${ROUTECTL_FIXTURE_CLIENT_BINARY_VERSION:-}"
  local lane
  lane="$(normalize_lane "$pkind")"

  # Structural summary lines, ONE PER REQUEST-SIDE DIRECTION.
  #
  # Anchored the same way as extract_body: the needle must sit at the
  # message position right after the first `routectl_core::log_safe: `,
  # and the direction is matched in the FIELDS of that same line (the
  # `direction=` precedent is the stream-summary selection below). An
  # unanchored `grep 'structural summary' | head -2` selected an ingress
  # request BODY line whose JSON content happened to carry the phrase and
  # then dropped the real outgoing summary off the end of `head -2` --
  # measurably, on 15% of an existing corpus. Body text rides a different
  # event under `body=`, so anchoring on the emitter plus the event name
  # makes content copies unselectable, and picking each direction
  # explicitly makes the selection independent of line order and count.
  extract_structural() {
    local direction="$1"
    echo "$lines" | awk -v direction="$direction" \
                        -v needle="structural summary" \
                        -v target="routectl_core::log_safe: " '
      {
        p = index($0, target)
        if (p == 0) next
        mstart = p + length(target)
        if (substr($0, mstart, length(needle)) != needle) next
        rest = substr($0, mstart + length(needle))
        if (index(rest, "direction=\"" direction "\"") == 0) next
        print
        exit
      }
    '
  }

  local s_ing s_out missing
  s_ing="$(extract_structural ingress)"
  s_out="$(extract_structural outgoing)"
  missing=""
  [ -z "$s_ing" ] && missing="ingress"
  [ -z "$s_out" ] && missing="${missing:+$missing and }outgoing"

  # A fixture missing a direction's structural summary cannot have that
  # direction's wire shape checked against anything. In driver mode that
  # is a failed capture -- a canonical fixture carries both halves of its
  # structural evidence or it is not landed. Live-box mode warns and
  # keeps the fixture: a drained session's log is whatever the daemon
  # happened to emit, and a partial trace is still evidence.
  if [ -n "$missing" ]; then
    if [ "$DRIVER_MODE" = 1 ]; then
      echo "capture_fixtures: no $missing structural summary for $id; discarding the fixture" >&2
      rm -rf "$tmp"
      return 1
    fi
    echo "capture_fixtures: WARN no $missing structural summary for $id" >&2
  fi

  {
    [ -n "$s_ing" ] && printf '%s\n' "$s_ing"
    [ -n "$s_out" ] && printf '%s\n' "$s_out"
    :
  } > "$tmp/structural.txt"

  # Stream summary lines (two: upstream + egress direction).
  echo "$lines" | grep 'stream summary' > "$tmp/stream.txt" || true

  # meta.json -- emit by hand to avoid jq dependency. ONE schema: every
  # key here is a key the replay loader's FixtureMeta reads, apart from
  # the triage-only fields noted in docs/REPLAY-FIXTURES.md. No
  # file-presence flags (the directory listing is the only record).
  #
  # EVERY string value goes through json_escape. The three token counts,
  # schema_version, and stream are numeric / boolean and must NOT be
  # quoted or escaped.
  local j_id j_ts j_version j_alias j_model j_case j_sha j_wire
  local j_cname j_cversion j_cbinver j_cmode j_ikind j_pkind j_lane j_finish
  j_id="$(json_escape "$id")"
  j_ts="$(json_escape "$ts")"
  j_version="$(json_escape "$ROUTECTL_VERSION")"
  j_alias="$(json_escape "${alias:-}")"
  j_model="$(json_escape "${model:-}")"
  j_case="$(json_escape "${ROUTECTL_FIXTURE_CASE_ID:-}")"
  j_sha="$(json_escape "${ROUTECTL_FIXTURE_CONFIG_SHA:-}")"
  j_wire="$(json_escape "${ROUTECTL_FIXTURE_WIRE_PATTERN:-}")"
  j_cname="$(json_escape "${client_name:-}")"
  j_cversion="$(json_escape "${client_version:-}")"
  j_cbinver="$(json_escape "$client_binary_version")"
  j_cmode="$(json_escape "${ROUTECTL_FIXTURE_CONNECTION_MODE:-}")"
  j_ikind="$(json_escape "$ikind")"
  j_pkind="$(json_escape "$pkind")"
  j_lane="$(json_escape "$lane")"
  j_finish="$(json_escape "${finish:-}")"

  cat > "$tmp/meta.json" <<META
{
  "schema_version": $SCHEMA_VERSION,
  "request_id": "$j_id",
  "captured_at_ts": "$j_ts",
  "routectl_version": "$j_version",
  "alias": "$j_alias",
  "model": "$j_model",
  "case_id": "$j_case",
  "config_sha": "$j_sha",
  "wire_pattern": "$j_wire",
  "client": {
    "name": "$j_cname",
    "version": "$j_cversion",
    "binary_version": "$j_cbinver",
    "connection_mode": "$j_cmode"
  },
  "ingress_kind": "$j_ikind",
  "provider_kind": "$j_pkind",
  "lane": "$j_lane",
  "stream": $stream_flag,
  "finish_reason": "$j_finish",
  "input_tokens": ${input_tokens:-0},
  "output_tokens": ${output_tokens:-0},
  "total_tokens": ${total_tokens:-0}
}
META

  # Scrub before promoting. One owner for scrubbing:
  # scripts/scrub-fixture.sh --write rewrites the contributor's own home
  # path (captured bodies echo system-reminder text embedding it, in both
  # the literal form and the dash-encoded `.claude/projects/-home-...`
  # dir-name form) and redacts the value of every credential-shaped header
  # while keeping its name. Auth redaction happens HERE, at write time,
  # because a corpus that ever held a live bearer token is uncommittable in
  # practice no matter what a later scan says.
  #
  # A scrub failure aborts the capture: the tmp directory is removed rather
  # than promoted, so an unscrubbed fixture never reaches the corpus. It is
  # `return 1` and not a bare failure so `set -e` cannot promote it.
  if ! bash "$SCRUB" --write "$tmp"; then
    echo "capture_fixtures: scrub failed for $id; discarding the fixture" >&2
    rm -rf "$tmp"
    return 1
  fi

  # Driver mode additionally REFUSES to promote what the write pass could
  # not fully clean. `--check` exits 1 on residual personal data and 2 on
  # a usage / prerequisite problem; either way the fixture does not land,
  # because a driver corpus is canonical by construction. The live-box
  # path stays write-only: its corpus is local-only by policy, and a
  # refusal there would throw away evidence that cannot be recaptured.
  #
  # `--check` scans EVERY file under the fixture, meta.json included, so a
  # case id carrying a hostname or a real home path is refused by this
  # gate. Case ids are scenario names for exactly that reason.
  if [ "$DRIVER_MODE" = 1 ] && ! bash "$SCRUB" --check "$tmp"; then
    echo "capture_fixtures: scrub check refused $id; not promoting the fixture" >&2
    rm -rf "$tmp"
    return 1
  fi

  # ORDERING CONTRACT: `--write`, then `--check` on the FULL bytes, then
  # any size reduction, then promote. Nothing reduces a fixture today. If a
  # reduction is ever added -- a padding descriptor, an elision sentinel, a
  # truncation of a large block -- it belongs HERE, after the `--check`
  # above, and never before it.
  #
  # A reduction applied before the gate would hide a credential inside the
  # region it replaced: the gate proves only what it read, so bytes dropped
  # first are bytes nothing ever scanned. The regions a reduction would
  # target are the large `tool_result` blocks, and those carry the client's
  # own framing -- `file_path` values, line-numbered file content,
  # directory listings -- so a third party's home path sits deep inside
  # exactly the bytes a reduction would drop. That is what the
  # `home-prefix` and `home-prefix-encoded` classes exist to catch, and
  # they catch it only by scanning the whole file.
  #
  # The order is load-bearing in the other direction too: `--write`
  # neutralizes the contributor's own home path, so `--check` verifies the
  # rewritten bytes rather than the raw capture.

  # Driver mode enforces the CLAIMS the fixture records about itself, in
  # the slot the ordering contract reserves: after `--check` on the full
  # bytes, before the promotion. Every refusal discards the staged
  # directory and returns non-zero so `set -e` cannot promote it.
  #
  # The wire-pattern claim is the SELECTOR and returns
  # $PATTERN_MISMATCH_RC, which the loop reads as "not this request, try
  # the next". Every other refusal here is a property of the RUN and
  # returns 1, which the loop propagates as a rig refusal -- a defect,
  # never retryable.
  if [ "$DRIVER_MODE" = 1 ]; then
    # The claimed pattern comes from the recorded pin, never from argv: a
    # flag would let a caller declare a pattern the case does not claim,
    # which is the unverified claim arriving one layer earlier.
    if ! python3 "$VERIFY_PATTERN" "$tmp" "$ROUTECTL_FIXTURE_WIRE_PATTERN"; then
      echo "capture_fixtures: $id does not exhibit the wire pattern its case claims" >&2
      echo "('$ROUTECTL_FIXTURE_WIRE_PATTERN'); skipping this candidate." >&2
      rm -rf "$tmp"
      return "$PATTERN_MISMATCH_RC"
    fi

    if ! assert_mode_seam_coherent "$tmp" "$id"; then
      rm -rf "$tmp"
      return 1
    fi

    # The expected-ingress pin against the TRACED dialect. `$ikind` was
    # parsed out of the daemon's own `ingress request body` line, so it
    # says which adapter really parsed the request -- while the pin says
    # which one the run was set up to reach. A client that took the
    # runner's carriers and then talked its own dialect anyway lands here
    # with the two disagreeing, and no environment check upstream could
    # have seen it.
    #
    # There is deliberately no separate arm for an EMPTY traced token: the
    # pin is validated to be a vocabulary member, so it is never empty, and
    # an unequal comparison already refuses. (In-scope selection requires a
    # non-empty traced token anyway -- a request whose dialect the trace
    # does not name is never a candidate -- so a driver run with an
    # unobserved dialect lands nothing and exits 3 rather than reaching
    # here.)
    if [ "$ikind" != "$ROUTECTL_FIXTURE_EXPECTED_INGRESS" ]; then
      echo "capture_fixtures: $id was captured on ingress dialect '$ikind' but the run" >&2
      echo "expects '$ROUTECTL_FIXTURE_EXPECTED_INGRESS': the client reached routectl on a dialect this" >&2
      echo "case is not evidence for; not promoting the fixture." >&2
      rm -rf "$tmp"
      return 1
    fi

    # The client-version comparison runs LAST of the four, and after the
    # expected-ingress gate specifically: that gate decides whether this
    # fixture is evidence for the dialect it is being landed as at all,
    # which is a question about the fixture's IDENTITY, while this one is
    # about whether the client's two self-statements agree. A wrong-dialect
    # capture reported as a version disagreement would name the smaller
    # fault of the two.
    if ! assert_client_version_coherent "$id" "$client_binary_version" \
         "${client_version:-}"; then
      rm -rf "$tmp"
      return 1
    fi
  fi

  # Resolve the landing path. Driver mode keys on `(lane, case_id)` so a
  # rerun of the same case re-lands on the same path and diffs; live-box
  # mode keys on request_id. An empty lane means normalize_lane could not
  # map the traced provider_kind, which in driver mode would collapse the
  # path to `$OUT/<case_id>` and put a fixture nothing can gate into the
  # canonical corpus.
  local dst
  if [ "$DRIVER_MODE" = 1 ]; then
    if [ -z "$lane" ]; then
      # A LANE THAT WILL NOT RESOLVE IS A PER-REQUEST FACT: it comes from
      # THIS request's own traced `provider_kind`, so it says nothing about
      # a candidate already selected off its own lines. With a selection
      # held, skip this candidate; with none held, this is the request the
      # case would pin and a fixture nothing can gate fails closed.
      if [ -n "$SELECTED_STAGED" ]; then
        echo "capture_fixtures: no lane for $id (provider_kind '$pkind'); skipping this" >&2
        echo "candidate -- the lane is computed from its own trace lines and says nothing" >&2
        echo "about the already-selected candidate." >&2
        rm -rf "$tmp"
        return "$LANE_UNRESOLVED_RC"
      fi
      echo "capture_fixtures: no lane for $id (provider_kind '$pkind'); not promoting the fixture" >&2
      rm -rf "$tmp"
      return 1
    fi
    # A lane the scrub gate holds no credential-shape classification for
    # cannot promote. `--check` proved this fixture carries no residue of
    # the shapes the gate KNOWS; on an unclassified lane that proof is
    # vacuous, because nobody has yet said what a credential on it even
    # looks like. The gate owns the table and answers the question, so
    # there is exactly one place the vocabulary lives (a lane in
    # PROVIDER_SHAPE_EXCLUDED answers "classified" -- its shape is absent
    # by a written verdict, not by omission).
    if ! bash "$SCRUB" --lane-known "$lane"; then
      echo "capture_fixtures: lane '$lane' has no credential-shape classification in the" >&2
      echo "scrub gate; not promoting $id -- classify the lane first." >&2
      rm -rf "$tmp"
      return 1
    fi
    dst="$OUT/$lane/$ROUTECTL_FIXTURE_CASE_ID"

    # THE SELECTION, and the reason nothing is promoted here. This candidate
    # has now passed every gate; whether it is the case's evidence depends
    # on whether an EARLIER candidate already satisfied the claim, because
    # the selection is the FIRST match in first-ingress-body order.
    local turns
    turns="$(ingress_turn_count "$tmp/$INGRESS_BODY_FILE")"

    if [ -n "$SELECTED_STAGED" ]; then
      # A LATER MATCH IS REDUNDANT, not ambiguous. A tool loop resends its
      # `tool_use` / `tool_result` pair in every subsequent request, so a
      # monotone claim is satisfied by every candidate after the first
      # witness -- two matches is the normal shape of a multi-turn capture.
      # What the old already-landed refusal was really catching is a trace
      # holding two genuinely DIFFERENT interactions under one case id, and
      # that is what the continuation check below still refuses.
      if ! assert_redundant_is_continuation "$id" "$ikind" "$lane" "$pkind" \
           "${model:-}" "$turns"; then
        rm -rf "$tmp"
        return 1
      fi
      rm -rf "$tmp"
      return "$REDUNDANT_MATCH_RC"
    fi

    # First match: RETAIN the staged directory rather than promoting it, and
    # remember the identity fields a later redundant match is compared
    # against. The caller promotes once, after the scan.
    SELECTED_STAGED="$tmp"
    SELECTED_DST="$dst"
    SELECTED_REQUEST_ID="$id"
    SELECTED_TS="$ts"
    SELECTED_IKIND="$ikind"
    SELECTED_LANE="$lane"
    SELECTED_PKIND="$pkind"
    SELECTED_MODEL="${model:-}"
    SELECTED_TURNS="$turns"
    # The manifest line is BUILT here, where its escaped values are in
    # scope, and appended by the caller after the promotion -- so a
    # dangling manifest entry never points to a missing directory, and a
    # refusal from a later candidate leaves the append-only file untouched
    # rather than needing it retracted.
    SELECTED_MANIFEST_LINE="{\"request_id\":\"$j_id\",\"captured_at\":\"$j_ts\",\"routectl_version\":\"$j_version\",\"alias\":\"$j_alias\",\"model\":\"$j_model\",\"case_id\":\"$j_case\",\"ingress_kind\":\"$j_ikind\",\"provider_kind\":\"$j_pkind\",\"lane\":\"$j_lane\",\"stream\":$stream_flag,\"finish_reason\":\"$j_finish\",\"input_tokens\":${input_tokens:-0},\"output_tokens\":${output_tokens:-0},\"total_tokens\":${total_tokens:-0}}"
    echo "$id"
    return 0
  fi

  dst="$OUT/$id"

  # LIVE-BOX PROMOTION. Each live-box capture keys on its own request_id and
  # is independent evidence, so it promotes as it is written and appends its
  # manifest line immediately: there is no selection to hold it back for,
  # and a later capture's failure says nothing about this one.
  #
  # The rename is guarded: with errexit unarmed in this body a failed
  # promotion would fall through to the manifest append and record a fixture
  # that never landed.
  promote_staged_dir "$tmp" "$dst" "$id" || { rm -rf "$tmp"; return 1; }

  # Append to manifest.jsonl AFTER the rename so a dangling manifest
  # entry never points to a missing directory. One JSONL line per
  # request, append-only. Same escaping contract as meta.json above --
  # an unescaped quote here corrupts one line of an append-only file
  # that has no rewrite path.
  cat >> "$OUT/manifest.jsonl" <<MANIFEST_LINE
{"request_id":"$j_id","captured_at":"$j_ts","routectl_version":"$j_version","alias":"$j_alias","model":"$j_model","case_id":"$j_case","ingress_kind":"$j_ikind","provider_kind":"$j_pkind","lane":"$j_lane","stream":$stream_flag,"finish_reason":"$j_finish","input_tokens":${input_tokens:-0},"output_tokens":${output_tokens:-0},"total_tokens":${total_tokens:-0}}
MANIFEST_LINE

  echo "$id"
}

# Promote the SELECTED staged fixture and append its manifest line. Driver
# mode only, called ONCE after every candidate has been examined and none
# refused the run.
promote_selected_fixture() {
  [ -n "$SELECTED_STAGED" ] || return 0
  mkdir -p "$(dirname "$SELECTED_DST")" || return 1
  promote_staged_dir "$SELECTED_STAGED" "$SELECTED_DST" \
    "$SELECTED_REQUEST_ID" || return 1
  SELECTED_STAGED=""
  DRIVER_LANDED="$DRIVER_LANDED $SELECTED_DST"
  printf '%s\n' "$SELECTED_MANIFEST_LINE" >> "$OUT/manifest.jsonl"
}

# Iterate over candidates in first-ingress-body order, apply --limit if set.
#
# THE SET -E HAZARD, NAMED. Capturing write_fixture's status disables
# errexit for the ENTIRE dynamic extent of the call, function body
# included, and neither `set -e` inside the function nor a subshell around
# it restores that (measured, bash 5.3). Two consequences, both handled:
#
#   * "a refusal aborts the run" was free under errexit and is now an
#     EXPLICIT re-raise below. Every fail-closed class in write_fixture
#     other than the pattern selector returns 1, and that 1 leaves this
#     loop as the script's exit status. Without the re-raise each of them
#     would become a status nothing reads.
#   * write_fixture's own mutating steps can no longer rely on errexit
#     to abort a half-written fixture, so each one that would leave a
#     partial or wrongly-promoted directory carries its own
#     `|| return 1`.
captured=0
latest_ts=""
while IFS=$'\t' read -r ts id pkind ikind; do
  [ -z "$id" ] && continue
  [ -d "$OUT/$id" ] && continue          # already captured (defensive idempotency)
  CANDIDATES_EXAMINED=$((CANDIDATES_EXAMINED + 1))
  write_rc=0
  write_fixture "$id" "$ts" "$pkind" "$ikind" >/dev/null || write_rc=$?
  # The selector: this candidate is not the request the case claims. Count
  # it and consider the next one. Scoped to driver mode because that is
  # the only mode with a claim to select on.
  if [ "$DRIVER_MODE" = 1 ] && [ "$write_rc" = "$PATTERN_MISMATCH_RC" ]; then
    CANDIDATES_SKIPPED=$((CANDIDATES_SKIPPED + 1))
    continue
  fi
  # A LATER WITNESS of the shape the selected candidate already proved,
  # verified to be a strict continuation of it. Counted on its own line
  # field rather than as a skip: a reader seeing two skips would conclude
  # two requests failed the claim, when one failed and one carried the same
  # shape one turn further on.
  if [ "$DRIVER_MODE" = 1 ] && [ "$write_rc" = "$REDUNDANT_MATCH_RC" ]; then
    CANDIDATES_REDUNDANT=$((CANDIDATES_REDUNDANT + 1))
    continue
  fi
  # A LATER CANDIDATE WHOSE OWN LANE DID NOT RESOLVE, with a selection
  # already held. The lane is computed from that candidate's own traced
  # `provider_kind`, so it is a per-request fact and cannot indict the
  # selection -- counted as a skip and scanned past. write_fixture returns
  # this code only when a selection exists; the no-selection case is still
  # the run-level refusal re-raised below.
  if [ "$DRIVER_MODE" = 1 ] && [ "$write_rc" = "$LANE_UNRESOLVED_RC" ]; then
    CANDIDATES_SKIPPED=$((CANDIDATES_SKIPPED + 1))
    continue
  fi
  # THE RE-RAISE. Anything else non-zero is a run-level refusal and ends
  # the run with its own status, which is what errexit used to do for free.
  # A driver run that ends in a refusal leaves nothing it promoted, and
  # under promote-once that is STRUCTURAL rather than unwound: the single
  # promotion happens after this loop, so a refusal from a candidate AFTER
  # the selected one has no promoted fixture to destroy. What the discard
  # cleans up is the staged directory the selection was holding.
  if [ "$write_rc" != 0 ]; then
    discard_driver_staging
    # The refusal path needs this line MORE than the success path: it is the
    # only record of which candidate the run reached and how many it skipped
    # to get there, and a reader who has to reconstruct that by correlating
    # request ids across a log is doing the work the line exists to remove.
    [ "$DRIVER_MODE" = 1 ] && emit_selection_line
    exit "$write_rc"
  fi
  # DRIVER MODE KEEPS SCANNING past its selection and does NOT count a
  # landing here: nothing has landed yet. Every remaining candidate is still
  # staged, scrubbed and `--check`ed -- the one deliberate per-request
  # exception to the skip rule is fatal residue, and a `break` on the first
  # match would leave the rest of the turn unexamined for it. `--limit` is
  # deliberately not honored as a break in this mode either, for the same
  # reason: one driver run pins one case, so a cap on landings has nothing
  # to cap and would only stop the scan early.
  if [ "$DRIVER_MODE" = 1 ]; then
    continue
  fi
  SELECTED_REQUEST_ID="$id"
  captured=$((captured + 1))
  latest_ts="$ts"
  if [ "$LIMIT" -gt 0 ] && [ "$captured" -ge "$LIMIT" ]; then
    break
  fi
done < <(in_scope_ids)

# THE SINGLE PROMOTION. Driver mode reaches here only when every candidate
# was examined and none refused the run, so the selected staged fixture is
# now known to be the case's evidence and nothing later can retract it. The
# manifest append rides along inside, after the rename.
if [ "$DRIVER_MODE" = 1 ] && [ -n "$SELECTED_STAGED" ]; then
  if ! promote_selected_fixture; then
    echo "capture_fixtures: could not promote the selected fixture for case" >&2
    echo "'$ROUTECTL_FIXTURE_CASE_ID' into $SELECTED_DST; landing nothing." >&2
    discard_driver_staging
    emit_selection_line
    exit 1
  fi
  captured=1
  latest_ts="$SELECTED_TS"
fi

# Update resume marker only if we captured something. Write to a temp
# file inside $OUT and rename over the marker so an interrupt mid-write
# cannot leave an empty/partial marker that would force a full rescan
# (mirrors the fixture-dir tmp -> dst promotion in write_fixture).
if [ -n "$latest_ts" ]; then
  marker_tmp="$(mktemp "$OUT/.last_capture_ts.XXXXXX")"
  echo "$latest_ts" > "$marker_tmp"
  mv "$marker_tmp" "$MARKER"
fi

echo "captured=$captured since=$since latest=$latest_ts out=$OUT"

# ONE structured selection line per driver run. A reviewer reading a
# committed fixture cannot otherwise tell WHICH request of an agentic turn
# they are looking at, and the run is where that fact exists.
#
# This is OBSERVABILITY, not identity: it is a log line and deliberately
# not a meta.json field, because under the selector the ordinal is not the
# selection basis and a recorded one would preserve a number nothing reads.
#
# NO BODY CONTENT. Everything on the line is an id, a count, or the fixed
# ordering-basis token -- a rig log is a CI artifact, and a body is
# unscrubbed at the point this prints.
if [ "$DRIVER_MODE" = 1 ]; then
  emit_selection_line
fi

# Driver mode only: landing zero is a failed run, not a quiet window, and
# the two ways to land zero carry OPPOSITE verdicts.
#
# The loop re-raises every run-level refusal and the promotion above sets
# `captured=1` whenever a candidate was selected, so reaching this line with
# zero means the scan finished having selected nothing. What remains splits
# on the candidate count:
#
#   * candidates existed and every one was SKIPPED -- the case claims a
#     wire shape none of the requests its own run produced carried. That
#     is a defect in the case (or in the client's behavior under it) and
#     retrying spends tokens to reach the same verdict, so it is the
#     REFUSAL exit, exit 1, the same verdict a per-request refusal gets.
#   * zero candidates -- the trace held no completed request at all (a
#     429, an upstream that returned no success body, a client that died
#     before sending). Retryable, and unchanged: exit 3.
#
# A REDUNDANT candidate cannot reach here: redundancy is defined against a
# selection, so a run that counted one has a selected fixture and landed it.
#
# No new exit code, deliberately: the f4 matrix runner reads the rig's
# retryable / not-retryable distinction off exactly these two, and a third
# code would be a third case for every caller to learn.
#
# Only the case id is nameable here: `lane` is local to write_fixture and
# there is no traced provider_kind to normalize when nothing landed.
if [ "$DRIVER_MODE" = 1 ] && [ "$captured" -eq 0 ]; then
  if [ "$CANDIDATES_SKIPPED" -gt 0 ]; then
    echo "capture_fixtures: case '$ROUTECTL_FIXTURE_CASE_ID' examined $CANDIDATES_EXAMINED candidate request(s) and none exhibited the wire pattern it claims ('$ROUTECTL_FIXTURE_WIRE_PATTERN'); refusing the run" >&2
    exit 1
  fi
  echo "capture_fixtures: case '$ROUTECTL_FIXTURE_CASE_ID' landed no fixture; the trace at $LOG holds no completed request" >&2
  exit 3
fi
