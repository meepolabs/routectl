"""Read one cell's evidence directory into a typed result, or INCOMPLETE.

Stdlib only. The directory is the one scripts/capture_driver.sh leaves
at `<out>/.cell-evidence/` through scripts/drivers/lib/cell_evidence.py
(layout: docs/DEVELOPMENT.md "Per-cell evidence"). The layout marker, the
artifact names and the client_result keys are imported from that writer,
not re-spelled here, so the reader cannot drift from what is written.

A cell's evidence is COMPLETE only when client_result.json parses with
the writer's schema marker, names nothing missing and nothing truncated,
every artifact is on disk, and both snapshots parse as an available panel
at the schema version this reader knows. Anything else is Incomplete with
one IncompleteReason and the artifact it concerns -- never an empty
result, so an aggregator cannot read a broken cell as a clean one.

The snapshot schema versions are replicas of the daemon's
`handlers/status/health.rs` / `usage.rs` SCHEMA_VERSION constants (the
rig's runtime trees carry no crates/); wire_watch.test.sh welds them.

Public API
  EVIDENCE_DIRNAME              ".cell-evidence"
  IncompleteReason              enum of why a cell's evidence is unusable
  Incomplete                    (reason, artifact, detail)
  UpstreamError                 one upstream_errors.jsonl row
  CellEvidence                  the typed result
  read_cell(out_dir)            CellEvidence | Incomplete for <out_dir>/.cell-evidence
  read_dir(evidence_dir)        same, given the evidence directory itself
  read_ingress_kinds(evidence_dir)  traced ingress tokens, or None when the
                                file is absent or the trace was not read;
                                usable even when the cell is Incomplete
"""

from __future__ import annotations

import enum
import json
import os
import re
import sys
from dataclasses import dataclass
from typing import Dict, Mapping, Optional, Tuple, Union

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "drivers", "lib"))

import cell_evidence  # noqa: E402

EVIDENCE_DIRNAME = ".cell-evidence"
RESULT_NAME = "client_result.json"
RESULT_SCHEMA = cell_evidence.SCHEMA
TRACE_ARTIFACTS: Tuple[str, ...] = tuple(cell_evidence.TRACE_ARTIFACTS)
SNAPSHOT_ARTIFACTS: Tuple[str, ...] = tuple(cell_evidence.SNAPSHOT_ARTIFACTS)

HEALTH_SCHEMA_VERSION = 6
USAGE_SCHEMA_VERSION = 3
SNAPSHOT_SCHEMA_VERSIONS: Mapping[str, int] = {
    "health.json": HEALTH_SCHEMA_VERSION,
    "usage.json": USAGE_SCHEMA_VERSION,
}
# The wire-check rows the report classifies on; each must be a list.
WIRE_CHECK_LISTS = ("loss", "loss_findings", "checks", "unrepaired_rejections", "local_refusals")

RESULT_INT_OR_NULL = ("turns", "driver_rc", "rig_rc", "exit")
RESULT_STR = ("lane", "case", "expected_ingress", "connection_mode")
FIDELITY_COUNTER_RE = re.compile(r"\b(rc_[a-z_]+_total)=(\d+)\b")


class IncompleteReason(str, enum.Enum):
    NO_EVIDENCE = "no-evidence"
    RESULT_MISSING = "result-missing"
    RESULT_CORRUPT = "result-corrupt"
    RESULT_SCHEMA = "result-schema"
    ARTIFACT_MISSING = "artifact-missing"
    ARTIFACT_TRUNCATED = "artifact-truncated"
    ARTIFACT_CORRUPT = "artifact-corrupt"
    SNAPSHOT_SCHEMA = "snapshot-schema"
    SNAPSHOT_UNAVAILABLE = "snapshot-unavailable"
    SNAPSHOT_SHAPE = "snapshot-shape"


@dataclass(frozen=True)
class Incomplete:
    reason: IncompleteReason
    artifact: Optional[str] = None
    detail: Optional[str] = None


@dataclass(frozen=True)
class UpstreamError:
    provider_kind: str
    status: int
    excerpt: str


@dataclass(frozen=True)
class CellEvidence:
    lane: str
    case: str
    expected_ingress: str
    connection_mode: str
    turns: Optional[int]
    driver_rc: Optional[int]
    rig_rc: Optional[int]
    exit: Optional[int]
    health: Mapping[str, object]
    usage: Mapping[str, object]
    ingress_kinds: Tuple[str, ...]
    upstream_errors: Tuple[UpstreamError, ...]
    fidelity_lines: Tuple[str, ...]
    fidelity_counters: Mapping[str, int]

    @property
    def wire_check(self) -> Mapping[str, object]:
        value = self.health.get("wire_check")
        return value if isinstance(value, dict) else {}

    @property
    def usage_totals(self) -> Mapping[str, object]:
        value = self.usage.get("totals")
        return value if isinstance(value, dict) else {}


Result = Union[CellEvidence, Incomplete]


class _Stop(Exception):
    def __init__(self, incomplete: Incomplete) -> None:
        super().__init__(incomplete.reason.value)
        self.incomplete = incomplete


def _stop(reason: IncompleteReason, artifact: Optional[str] = None, detail: Optional[str] = None) -> _Stop:
    return _Stop(Incomplete(reason, artifact, detail))


def _read_text(directory: str, name: str) -> str:
    try:
        with open(os.path.join(directory, name), encoding="utf-8") as handle:
            return handle.read()
    except FileNotFoundError:
        raise _stop(IncompleteReason.ARTIFACT_MISSING, name, "absent") from None
    except (OSError, UnicodeDecodeError) as exc:
        raise _stop(IncompleteReason.ARTIFACT_CORRUPT, name, exc.__class__.__name__) from None


def _is_int_or_none(value: object) -> bool:
    return value is None or (isinstance(value, int) and not isinstance(value, bool))


def _read_result(directory: str) -> dict:
    path = os.path.join(directory, RESULT_NAME)
    try:
        with open(path, encoding="utf-8") as handle:
            doc = json.load(handle)
    except FileNotFoundError:
        raise _stop(IncompleteReason.RESULT_MISSING, RESULT_NAME) from None
    except (OSError, ValueError) as exc:
        raise _stop(IncompleteReason.RESULT_CORRUPT, RESULT_NAME, exc.__class__.__name__) from None
    if not isinstance(doc, dict):
        raise _stop(IncompleteReason.RESULT_CORRUPT, RESULT_NAME, "not an object")
    if doc.get("schema") != RESULT_SCHEMA:
        raise _stop(IncompleteReason.RESULT_SCHEMA, RESULT_NAME, "unknown layout marker")
    shape_ok = (
        all(isinstance(doc.get(k), str) for k in RESULT_STR)
        and all(k in doc and _is_int_or_none(doc[k]) for k in RESULT_INT_OR_NULL)
        and isinstance(doc.get("missing"), dict)
        and isinstance(doc.get("truncated"), list)
    )
    if not shape_ok:
        raise _stop(IncompleteReason.RESULT_CORRUPT, RESULT_NAME, "unexpected shape")
    return doc


def _check_listed_gaps(doc: dict) -> None:
    for name in sorted(doc["missing"]):
        raise _stop(IncompleteReason.ARTIFACT_MISSING, str(name), "named missing by the writer")
    for name in sorted(str(n) for n in doc["truncated"]):
        raise _stop(IncompleteReason.ARTIFACT_TRUNCATED, name)


def _read_snapshot(directory: str, name: str) -> dict:
    text = _read_text(directory, name)
    try:
        doc = json.loads(text)
    except ValueError as exc:
        raise _stop(IncompleteReason.ARTIFACT_CORRUPT, name, exc.__class__.__name__) from None
    if not isinstance(doc, dict):
        raise _stop(IncompleteReason.ARTIFACT_CORRUPT, name, "not an object")
    version = doc.get("schema_version")
    if version != SNAPSHOT_SCHEMA_VERSIONS[name] or isinstance(version, bool):
        raise _stop(IncompleteReason.SNAPSHOT_SCHEMA, name, "unknown schema_version")
    data = doc.get("data")
    if doc.get("unavailable") is not None or not isinstance(data, dict):
        raise _stop(IncompleteReason.SNAPSHOT_UNAVAILABLE, name)
    return data


def _check_health_shape(health: dict) -> None:
    rows = health.get("wire_check")
    if not isinstance(rows, dict) or not all(isinstance(rows.get(k), list) for k in WIRE_CHECK_LISTS):
        raise _stop(IncompleteReason.SNAPSHOT_SHAPE, "health.json", "wire_check rows")


def _check_usage_shape(usage: dict) -> None:
    if not isinstance(usage.get("totals"), dict):
        raise _stop(IncompleteReason.SNAPSHOT_SHAPE, "usage.json", "totals")


def _lines(text: str) -> Tuple[str, ...]:
    return tuple(line for line in text.splitlines() if line)


def _upstream_errors(text: str) -> Tuple[UpstreamError, ...]:
    rows = []
    for line in _lines(text):
        try:
            raw = json.loads(line)
            row = UpstreamError(raw["provider_kind"], raw["status"], raw["excerpt"])
        except (ValueError, KeyError, TypeError):
            raise _stop(IncompleteReason.ARTIFACT_CORRUPT, "upstream_errors.jsonl", "row shape") from None
        if not (isinstance(row.provider_kind, str) and isinstance(row.excerpt, str)) or not _is_int_or_none(row.status) or row.status is None:
            raise _stop(IncompleteReason.ARTIFACT_CORRUPT, "upstream_errors.jsonl", "row shape")
        rows.append(row)
    return tuple(rows)


def _fidelity_counters(lines: Tuple[str, ...]) -> Dict[str, int]:
    """Counters from the LAST snapshot line: the line is emitted each time
    /status/health is served, so the last one is the end-of-cell state."""
    if not lines:
        return {}
    return {m.group(1): int(m.group(2)) for m in FIDELITY_COUNTER_RE.finditer(lines[-1])}


def _read(directory: str) -> CellEvidence:
    if not os.path.isdir(directory):
        raise _stop(IncompleteReason.NO_EVIDENCE)
    doc = _read_result(directory)
    _check_listed_gaps(doc)
    health = _read_snapshot(directory, "health.json")
    _check_health_shape(health)
    usage = _read_snapshot(directory, "usage.json")
    _check_usage_shape(usage)
    texts = {name: _read_text(directory, name) for name in TRACE_ARTIFACTS}
    fidelity = _lines(texts["fidelity.log"])
    return CellEvidence(
        lane=doc["lane"],
        case=doc["case"],
        expected_ingress=doc["expected_ingress"],
        connection_mode=doc["connection_mode"],
        turns=doc["turns"],
        driver_rc=doc["driver_rc"],
        rig_rc=doc["rig_rc"],
        exit=doc["exit"],
        health=health,
        usage=usage,
        ingress_kinds=_lines(texts["ingress_kinds.txt"]),
        upstream_errors=_upstream_errors(texts["upstream_errors.jsonl"]),
        fidelity_lines=fidelity,
        fidelity_counters=_fidelity_counters(fidelity),
    )


def read_dir(evidence_dir: str) -> Result:
    try:
        return _read(evidence_dir)
    except _Stop as stop:
        return stop.incomplete


def read_cell(out_dir: str) -> Result:
    return read_dir(os.path.join(out_dir, EVIDENCE_DIRNAME))


def read_ingress_kinds(evidence_dir: str) -> Optional[Tuple[str, ...]]:
    try:
        doc = _read_result(evidence_dir)
    except _Stop:
        doc = None
    if doc is not None and "ingress_kinds.txt" in doc["missing"]:
        return None
    try:
        return _lines(_read_text(evidence_dir, "ingress_kinds.txt"))
    except _Stop:
        return None
