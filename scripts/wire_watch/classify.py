"""One final outcome per cell, from run.json and the cell's evidence.

Stdlib only. The outcome comes from evidence, never from the runner's exit
code: a cell the rig refused to promote (runner exit 5) still carries a
full health snapshot, and that snapshot is what says whether the wire
check found anything.

Order, first that applies:
  1. A non-terminal cell (PENDING, RUNNING)        INCOMPLETE(not-terminal)
  2. A terminal state other than EXITED            its ledger outcome
  3. EXITED but stopped by a bound (ledger.bound_stopped)
                                                   TIMEOUT / CAP_REACHED /
                                                   INTERRUPTED, from detail
  4. Evidence the reader refuses                   INCOMPLETE(<reason>)
  5. A health row this module cannot read          INCOMPLETE(snapshot-shape)
     A last fidelity line without both counters    INCOMPLETE(artifact-missing)
  6. An upstream refusal (refusal_rules.tsv)       its NOT_RUN / FAILED, and
                                                   no finding rows
  7. Traced ingress lacks the expected one         FAILED(ingress)
  8. Retained loss findings a (lane, path_class) counter does not cover:
     no positive counter, or a counter below them (the rows disagree)
                                                   INCOMPLETE(findings-truncated)
     Every counter at or above its retained findings and one above it
     (the daemon's 128-entry finding ring overflowed)
                                                   FINDING, the retained rows
                                                   plus one per short (lane,
                                                   path_class): leg `unknown`,
                                                   no path, count = counter -
                                                   retained; a short counter
                                                   with a label outside the
                                                   daemon's vocabulary is
                                                   INCOMPLETE(snapshot-shape)
  9. Any loss row or finding, unrepaired rejection, local refusal,
     nonzero rc_parser_unlocalized_total, nonzero
     rc_field_verdicts_learned_total, or any learned_negatives row (the
     daemon is fresh per cell, so every row was minted by this cell)
                                                   FINDING
 10. The driver exited nonzero or never ran        FAILED(driver)
 11. No clean check with a positive count, or a check that did not
     end clean                                     INCOMPLETE(wire-check)
 12. Otherwise                                     PASS

Step 11 is why an empty finding set is never read as clean: a cell whose
daemon checked nothing proves nothing.

Public API
  Finding, CellResult, Context    frozen dataclasses
  classify(entry, cell_dir, ctx)  CellResult
  is_wire_path(path)              whether a path has the daemon's pointer shape
  LEGS, LEG_UNKNOWN, PATH_CLASSES, REJECTION_CLASS, PARSER_UNLOCALIZED, VERDICTS_LEARNED
"""

from __future__ import annotations

import os
import re
import sys
from collections import Counter
from dataclasses import dataclass, field, replace
from datetime import datetime, timezone
from typing import Callable, Dict, FrozenSet, Mapping, Optional, Sequence, Tuple

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import evidence  # noqa: E402
import ledger  # noqa: E402
import nofollow  # noqa: E402
import refusal  # noqa: E402
from evidence import CellEvidence, Incomplete, IncompleteReason, UpstreamError  # noqa: E402
from ledger import CellState, Entry, Outcome  # noqa: E402
from legs import NONE_CAPTURED, legs_captured  # noqa: E402

LEGS = ("request", "response")
# The leg of an overflow finding: the counters say how many losses the
# ring dropped, never on which leg.
LEG_UNKNOWN = "unknown"
# Replicas of the daemon's closed labels (routectl-core wire_check);
# test_report welds them to the Rust source. A label outside them makes the
# cell INCOMPLETE rather than a finding under a name nobody reads.
PATH_CLASSES = ("missing", "moved", "changed", "added", "translation_unconsumed")
REJECTION_CLASS = "unrepaired"
CHECK_CLEAN = "clean"
FINDINGS_TRUNCATED = "findings-truncated"
CHECK_LOSS = "loss"
LANE_LABEL_RE = re.compile(r"^(anthropic|chat|responses)->[a-z0-9-]{1,40}$")
# The daemon's sanitized pointer (crates/routectl-core/src/wire_check/path.rs,
# welded by test_report): "" or up to PATH_MAX_DEPTH segments, each `/` and
# a verbatim identifier key, an array index, the wildcard standing for a
# masked key, or a `$`-prefixed schema keyword; at most PATH_MAX_CHARS long.
PATH_MAX_CHARS = 256
PATH_MAX_DEPTH = 8
PATH_KEY_MAX_CHARS = 32
PATH_WILDCARD = "*"
PATH_SCHEMA_SIGIL_KEYWORDS = ("$defs", "$ref")
_PATH_SEGMENT_RE = re.compile(
    r"[A-Za-z_][A-Za-z0-9_-]{0,%d}|0|[1-9][0-9]{0,19}|%s|%s"
    % (PATH_KEY_MAX_CHARS - 1, re.escape(PATH_WILDCARD), "|".join(re.escape(k) for k in PATH_SCHEMA_SIGIL_KEYWORDS)),
    re.ASCII,
)
PARSER_UNLOCALIZED = "rc_parser_unlocalized_total"
VERDICTS_LEARNED = "rc_field_verdicts_learned_total"
# An EXITED cell carries in its detail the stop the runner made (EXITED(9))
# or the run cap it was the one to record (run-size). Keys are
# ledger.STOP_DETAILS.
STOP_OUTCOMES: Mapping[str, Tuple[Outcome, Optional[str]]] = {
    "timeout": (Outcome.TIMEOUT, None),
    "size": (Outcome.CAP_REACHED, "size"),
    "interrupted": (Outcome.INTERRUPTED, None),
    "run-size": (Outcome.CAP_REACHED, "run-size"),
}


@dataclass(frozen=True)
class Finding:
    lane: str
    leg: str
    path_class: str
    # None only on an overflow finding, which stands for losses whose rows
    # the daemon's ring dropped.
    path: Optional[str]
    count: int = 1


@dataclass(frozen=True)
class CellResult:
    entry: Entry
    outcome: Outcome
    reason: Optional[str] = None
    # Container-sourced text (an artifact name, a reader detail, a check
    # summary): escaped wherever it is shown.
    detail: Optional[str] = None
    findings: Tuple[Finding, ...] = ()
    counters: Tuple[Tuple[str, int], ...] = ()
    refused_by: Optional[UpstreamError] = None
    ingress_kinds: Tuple[str, ...] = ()
    legs: Tuple[bool, ...] = NONE_CAPTURED


def _no_expiry(_key_dir: str, _name: str) -> Optional[datetime]:
    return None


@dataclass(frozen=True)
class Context:
    rules: Tuple[refusal.Rule, ...]
    bedrock_lanes: FrozenSet[str]
    key_dir: Optional[str] = None
    key_expiry: Callable[[str, str], Optional[datetime]] = field(default=_no_expiry)


def is_wire_path(path: str) -> bool:
    if path == "":
        return True
    if len(path) > PATH_MAX_CHARS or not path.startswith("/"):
        return False
    segments = path[1:].split("/")
    return len(segments) <= PATH_MAX_DEPTH and all(_PATH_SEGMENT_RE.fullmatch(seg) for seg in segments)


class _Shape(Exception):
    """A health row this module cannot read."""


def _str_field(row: object, key: str) -> str:
    value = row.get(key) if isinstance(row, dict) else None
    if not isinstance(value, str):
        raise _Shape(key)
    return value


def _count_field(row: object, key: str) -> int:
    value = row.get(key) if isinstance(row, dict) else None
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise _Shape(key)
    return value


def _labelled(row: object) -> Tuple[str, str, str]:
    lane, leg, path = _str_field(row, "lane"), _str_field(row, "leg"), _str_field(row, "path")
    if not LANE_LABEL_RE.fullmatch(lane):
        raise _Shape("lane")
    if leg not in LEGS:
        raise _Shape("leg")
    if not is_wire_path(path):
        raise _Shape("path")
    return lane, leg, path


def _loss_finding(row: object) -> Finding:
    lane, leg, path = _labelled(row)
    path_class = _str_field(row, "path_class")
    if path_class not in PATH_CLASSES:
        raise _Shape("path_class")
    return Finding(lane, leg, path_class, path)


def _rejection(row: object) -> Finding:
    lane, leg, path = _labelled(row)
    return Finding(lane, leg, REJECTION_CLASS, path)


@dataclass(frozen=True)
class _Rows:
    findings: Tuple[Finding, ...]
    losses: int
    # rc_wire_loss_total per (lane, path_class).
    loss_counts: Tuple[Tuple[Tuple[str, str], int], ...]
    local_refusals: int
    checks: Tuple[Tuple[str, int], ...]
    learned: int


def _list(container: Mapping[str, object], key: str) -> list:
    value = container.get(key)
    if not isinstance(value, list):
        raise _Shape(key)
    return value


def _rows(ev: CellEvidence) -> _Rows:
    wc = ev.wire_check
    findings = [_loss_finding(r) for r in _list(wc, "loss_findings")]
    findings += [_rejection(r) for r in _list(wc, "unrepaired_rejections")]
    learned = _list(ev.health, "learned_negatives")
    for row in learned:
        _str_field(row, "verdict")
    loss_counts: Dict[Tuple[str, str], int] = {}
    for row in _list(wc, "loss"):
        key = (_str_field(row, "lane"), _str_field(row, "path_class"))
        loss_counts[key] = loss_counts.get(key, 0) + _count_field(row, "count")
    return _Rows(
        findings=tuple(findings),
        losses=sum(loss_counts.values()),
        loss_counts=tuple(sorted(loss_counts.items())),
        local_refusals=sum(_count_field(r, "count") for r in _list(wc, "local_refusals")),
        checks=tuple((_str_field(r, "outcome"), _count_field(r, "count")) for r in _list(wc, "checks")),
        learned=len(learned),
    )


def _overflow(rows: _Rows) -> Optional[Tuple[Finding, ...]]:
    """The findings the daemon's ring dropped, one per (lane, path_class)
    whose loss counter is above its retained loss findings; None when the
    rows disagree. The daemon counts each loss and keeps its finding in a
    ring of the newest 128 per kind, so a counter may run past its
    retained findings; a retained (lane, path_class) with no positive
    counter, or a counter below its retained findings, cannot happen on
    an overflow and means the rows cannot be attributed."""
    retained = Counter((f.lane, f.path_class) for f in rows.findings if f.path_class != REJECTION_CLASS)
    counted = {key: count for key, count in rows.loss_counts if count > 0}
    if any(counted.get(key, 0) < n for key, n in retained.items()):
        return None
    short = [(key, count - retained[key]) for key, count in counted.items() if count > retained[key]]
    for (lane, path_class), _ in short:
        if not LANE_LABEL_RE.fullmatch(lane):
            raise _Shape("lane")
        if path_class not in PATH_CLASSES:
            raise _Shape("path_class")
    return tuple(Finding(lane, LEG_UNKNOWN, path_class, None, n) for (lane, path_class), n in sorted(short))


def _cell_end(cell_dir: str) -> Optional[datetime]:
    """When the evidence writer finished: client_result.json's mtime."""
    try:
        dir_fd = nofollow.open_dir(os.path.join(cell_dir, evidence.EVIDENCE_DIRNAME))
    except nofollow.DirRefused:
        return None
    try:
        info = os.stat(evidence.RESULT_NAME, dir_fd=dir_fd, follow_symlinks=False)
    except OSError:
        return None
    finally:
        os.close(dir_fd)
    # The container sets this mtime; one past year 9999 has no datetime.
    try:
        return _utc_from_timestamp(info.st_mtime)
    except (OSError, OverflowError, ValueError):
        return None


def _utc_from_timestamp(seconds: float) -> datetime:
    return datetime.fromtimestamp(seconds, tz=timezone.utc)


def _key_expired(entry: Entry, cell_dir: str, ctx: Context) -> Callable[[], bool]:
    def check() -> bool:
        if ctx.key_dir is None or entry.spec.key_file is None:
            return False
        expiry = ctx.key_expiry(ctx.key_dir, entry.spec.key_file)
        end = _cell_end(cell_dir)
        return expiry is not None and end is not None and end >= expiry

    return check


def _ledger_outcome(entry: Entry) -> Optional[CellResult]:
    if entry.state in (CellState.PENDING, CellState.RUNNING):
        return CellResult(entry, Outcome.INCOMPLETE, "not-terminal")
    fixed = ledger.terminal_outcome(entry)
    if fixed is not None:
        return CellResult(entry, fixed, entry.reason, detail=entry.detail)
    if ledger.bound_stopped(entry):
        outcome, reason = STOP_OUTCOMES.get(entry.detail or "", (Outcome.INCOMPLETE, "bound-stopped"))
        return CellResult(entry, outcome, reason, detail=entry.detail)
    return None


def _has_finding(rows: _Rows, counters: Mapping[str, int]) -> bool:
    return bool(
        rows.findings
        or rows.losses
        or rows.local_refusals
        or rows.learned
        or counters[PARSER_UNLOCALIZED]
        or counters[VERDICTS_LEARNED]
        or any(outcome == CHECK_LOSS for outcome, _ in rows.checks)
    )


def _retained_losses(rows: _Rows) -> int:
    return sum(1 for f in rows.findings if f.path_class != REJECTION_CLASS)


def _loss_summary(rows: _Rows) -> str:
    return f"{rows.losses} losses counted, {_retained_losses(rows)} findings retained (ring or window)"


def _overflow_summary(rows: _Rows) -> str:
    return f"{_retained_losses(rows)} of {rows.losses} losses detailed (finding ring full)"


def _check_summary(checks: Sequence[Tuple[str, int]]) -> str:
    if not checks:
        return "no checks ran"
    return ", ".join(f"{outcome} {count}" for outcome, count in checks)


def _from_evidence(base: CellResult, cell_dir: str, ev: CellEvidence, ctx: Context) -> CellResult:
    counters = ev.fidelity_counters
    base = replace(base, counters=tuple(sorted(counters.items())), ingress_kinds=ev.ingress_kinds)
    try:
        rows = _rows(ev)
    except _Shape as shape:
        return replace(base, reason=IncompleteReason.SNAPSHOT_SHAPE.value, detail=f"health.json {shape}")
    if PARSER_UNLOCALIZED not in counters or VERDICTS_LEARNED not in counters:
        return replace(base, reason=IncompleteReason.ARTIFACT_MISSING.value, detail="fidelity.log counters")
    entry = base.entry
    bedrock = entry.spec.lane in ctx.bedrock_lanes
    hit = refusal.first_match(ctx.rules, bedrock, ev.upstream_errors, _key_expired(entry, cell_dir, ctx))
    if hit is not None:
        rule, error = hit
        return replace(base, outcome=rule.outcome, reason=rule.reason, refused_by=error)
    if entry.spec.expected_ingress not in ev.ingress_kinds:
        return replace(base, outcome=Outcome.FAILED, reason="ingress")
    try:
        overflow = _overflow(rows)
    except _Shape as shape:
        return replace(base, reason=IncompleteReason.SNAPSHOT_SHAPE.value, detail=f"health.json {shape}")
    if overflow is None:
        return replace(base, reason=FINDINGS_TRUNCATED, detail=_loss_summary(rows))
    if overflow:
        return replace(base, outcome=Outcome.FINDING, findings=rows.findings + overflow,
                       detail=_overflow_summary(rows))
    if _has_finding(rows, counters):
        return replace(base, outcome=Outcome.FINDING, findings=rows.findings)
    if ev.driver_rc != 0:
        return replace(base, outcome=Outcome.FAILED, reason="driver")
    clean_runs = sum(count for outcome, count in rows.checks if outcome == CHECK_CLEAN)
    if clean_runs == 0 or any(outcome != CHECK_CLEAN for outcome, _ in rows.checks):
        return replace(base, reason="wire-check", detail=_check_summary(rows.checks))
    return replace(base, outcome=Outcome.PASS)


def classify(entry: Entry, cell_dir: str, ctx: Context) -> CellResult:
    fixed = _ledger_outcome(entry)
    if fixed is not None:
        return fixed
    base = CellResult(entry, Outcome.INCOMPLETE, legs=legs_captured(cell_dir, entry.spec.case))
    read = evidence.read_cell(cell_dir)
    if isinstance(read, Incomplete):
        detail = " ".join(p for p in (read.artifact, read.detail) if p) or None
        return replace(base, reason=read.reason.value, detail=detail)
    return _from_evidence(base, cell_dir, read, ctx)
