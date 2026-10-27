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
  ARTIFACT_MAX_BYTES            per-file byte cap; over it -> ARTIFACT_OVERSIZE
  Incomplete                    (reason, artifact, detail)
  UpstreamError                 one upstream_errors.jsonl row
  CellEvidence                  the typed result
  read_cell(out_dir)            CellEvidence | Incomplete for <out_dir>/.cell-evidence;
                                the parent of the evidence dir is trusted,
                                nothing beneath it is: a symlinked evidence
                                dir or artifact is Incomplete with detail
                                "symlink", however the path is spelled
  read_dir(evidence_dir)        same, given the evidence directory itself
  read_ingress_kinds(evidence_dir)  traced ingress tokens, or None when the
                                file is absent or the trace was not read;
                                usable even when the cell is Incomplete
"""

from __future__ import annotations

import enum
import errno
import os
import re
import stat
import sys
from dataclasses import dataclass
from typing import Dict, Mapping, Optional, Tuple, Union

_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, _HERE)
sys.path.insert(0, os.path.join(_HERE, "..", "drivers", "lib"))

import cell_evidence  # noqa: E402
import nofollow  # noqa: E402
import strict_json  # noqa: E402

EVIDENCE_DIRNAME = ".cell-evidence"
RESULT_NAME = "client_result.json"
RESULT_SCHEMA = cell_evidence.SCHEMA
TRACE_ARTIFACTS: Tuple[str, ...] = tuple(cell_evidence.TRACE_ARTIFACTS)
SNAPSHOT_ARTIFACTS: Tuple[str, ...] = tuple(cell_evidence.SNAPSHOT_ARTIFACTS)

HEALTH_SCHEMA_VERSION = 8
USAGE_SCHEMA_VERSION = 3
SNAPSHOT_SCHEMA_VERSIONS: Mapping[str, int] = {
    "health.json": HEALTH_SCHEMA_VERSION,
    "usage.json": USAGE_SCHEMA_VERSION,
}
# The wire-check rows the report classifies on; each must be a list.
WIRE_CHECK_LISTS = ("loss", "loss_findings", "checks", "unrepaired_rejections", "local_refusals")

# Per-artifact byte caps, each the most the writer can legitimately produce
# (plus headroom where the bound is loose). A file over its cap is
# ARTIFACT_OVERSIZE, decided from fstat before any byte is read and
# re-checked by reading at most cap+1 bytes, so a file that grows after the
# fstat still costs no more than its cap.
#   client_result.json  the writer's RESULT_MAX_BYTES, computed from its
#                       own per-field caps
#   health/usage.json   the runner's fetch cap (curl --max-filesize,
#                       SNAPSHOT_MAX_BYTES in scripts/capture_driver.sh)
#   fidelity.log        the writer's FIDELITY_MAX_BYTES
#   ingress_kinds.txt   INGRESS_MAX_TOKENS tokens of the writer's
#                       INGRESS_TOKEN length bound, one per line
#   upstream_errors.jsonl  ERROR_MAX_ROWS rows; json.dumps escapes each
#                       non-printable byte of an EXCERPT_MAX_BYTES excerpt
#                       or a provider kind as \uXXXX (6 bytes per byte)
RESULT_MAX_BYTES = cell_evidence.RESULT_MAX_BYTES
SNAPSHOT_MAX_BYTES = 8 * 1024 * 1024
INGRESS_TOKEN_MAX_CHARS = 64
PROVIDER_KIND_MAX_CHARS = 64
JSON_ESCAPE_FACTOR = 6
ERROR_ROW_OVERHEAD_BYTES = 128
ERROR_ROW_MAX_BYTES = (
    JSON_ESCAPE_FACTOR * (cell_evidence.EXCERPT_MAX_BYTES + PROVIDER_KIND_MAX_CHARS) + ERROR_ROW_OVERHEAD_BYTES
)
ARTIFACT_MAX_BYTES: Mapping[str, int] = {
    RESULT_NAME: RESULT_MAX_BYTES,
    "health.json": SNAPSHOT_MAX_BYTES,
    "usage.json": SNAPSHOT_MAX_BYTES,
    "fidelity.log": cell_evidence.FIDELITY_MAX_BYTES,
    "ingress_kinds.txt": cell_evidence.INGRESS_MAX_TOKENS * (INGRESS_TOKEN_MAX_CHARS + 1),
    "upstream_errors.jsonl": cell_evidence.ERROR_MAX_ROWS * ERROR_ROW_MAX_BYTES,
}

RESULT_INT_OR_NULL = ("turns", "driver_rc", "rig_rc", "exit")
RESULT_STR = ("lane", "case", "expected_ingress", "connection_mode")
# A counter is a u64 in the daemon (at most 20 digits). A longer run of
# digits is not a value to clip or skip but a fidelity.log this reader
# cannot trust, so it is detected separately and refused.
COUNTER_MAX_DIGITS = 20
INGRESS_TOKEN_RE = re.compile(cell_evidence.INGRESS_TOKEN)
FIDELITY_COUNTER_RE = re.compile(r"\b(rc_[a-z_]+_total)=(\d{1,%d})\b" % COUNTER_MAX_DIGITS)
FIDELITY_COUNTER_OVERLONG_RE = re.compile(r"\brc_[a-z_]+_total=\d{%d}" % (COUNTER_MAX_DIGITS + 1))


class IncompleteReason(str, enum.Enum):
    """Why a cell's evidence is unusable; each value names one cause.

    NO_EVIDENCE           the evidence directory does not exist
    RESULT_MISSING        client_result.json is absent
    RESULT_CORRUPT        client_result.json is unreadable, not JSON, or
                          has the wrong shape
    RESULT_SCHEMA         client_result.json carries an unknown layout marker
    ARTIFACT_MISSING      an artifact is absent or the writer named it missing
    ARTIFACT_TRUNCATED    the writer capped an artifact and said so
    ARTIFACT_CORRUPT      an artifact is unreadable, not a regular file, not
                          UTF-8, not JSON, or has the wrong row shape
    ARTIFACT_OVERSIZE     any file, client_result.json included, is over its
                          ARTIFACT_MAX_BYTES cap
    SNAPSHOT_SCHEMA       a snapshot's schema_version is not the known one
    SNAPSHOT_UNAVAILABLE  a snapshot reports its panel unavailable
    SNAPSHOT_SHAPE        a snapshot panel lacks the rows the report reads
    """

    NO_EVIDENCE = "no-evidence"
    RESULT_MISSING = "result-missing"
    RESULT_CORRUPT = "result-corrupt"
    RESULT_SCHEMA = "result-schema"
    ARTIFACT_MISSING = "artifact-missing"
    ARTIFACT_TRUNCATED = "artifact-truncated"
    ARTIFACT_CORRUPT = "artifact-corrupt"
    ARTIFACT_OVERSIZE = "artifact-oversize"
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


def _read_capped(fd: int, cap: int) -> bytes:
    chunks = []
    left = cap + 1
    while left > 0:
        chunk = os.read(fd, min(left, 1 << 16))
        if not chunk:
            break
        chunks.append(chunk)
        left -= len(chunk)
    return b"".join(chunks)


# The evidence directory is written from inside the capture container, so
# nothing under it is trusted: neither it nor any artifact may be a symlink
# (a planted link would make the host read an operator file into the
# result). Only the path the caller passes, up to the directory itself, is
# trusted. O_NONBLOCK: opening a planted FIFO would otherwise block until a
# writer appears; O_NOCTTY: a planted terminal device must not become this
# process's controlling terminal.
_CLOEXEC = getattr(os, "O_CLOEXEC", 0)
_ARTIFACT_FLAGS = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_NOCTTY | _CLOEXEC


def _open_evidence_dir(evidence_dir: str) -> int:
    """Descriptor for the evidence directory itself, never a link to one,
    however the path is spelled (see nofollow.py)."""
    try:
        return nofollow.open_dir(evidence_dir)
    except nofollow.DirRefused as refused:
        detail = None if refused.detail == nofollow.MISSING else refused.detail
        raise _stop(IncompleteReason.NO_EVIDENCE, None, detail) from None


def _read_bounded(dir_fd: int, name: str, missing: IncompleteReason, corrupt: IncompleteReason) -> str:
    cap = ARTIFACT_MAX_BYTES[name]
    try:
        fd = os.open(name, _ARTIFACT_FLAGS, dir_fd=dir_fd)
    except FileNotFoundError:
        raise _stop(missing, name, "absent") from None
    except OSError as exc:
        detail = "symlink" if exc.errno == errno.ELOOP else exc.__class__.__name__
        raise _stop(corrupt, name, detail) from None
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode):
            raise _stop(corrupt, name, "not a regular file")
        if info.st_size > cap:
            raise _stop(IncompleteReason.ARTIFACT_OVERSIZE, name, f"over {cap} bytes")
        data = _read_capped(fd, cap)
    except OSError as exc:
        raise _stop(corrupt, name, exc.__class__.__name__) from None
    finally:
        os.close(fd)
    if len(data) > cap:
        raise _stop(IncompleteReason.ARTIFACT_OVERSIZE, name, f"over {cap} bytes")
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise _stop(corrupt, name, exc.__class__.__name__) from None


def _read_text(dir_fd: int, name: str) -> str:
    return _read_bounded(dir_fd, name, IncompleteReason.ARTIFACT_MISSING, IncompleteReason.ARTIFACT_CORRUPT)


def _decode(text: str, name: str, corrupt: IncompleteReason) -> object:
    try:
        return strict_json.loads(text)
    except strict_json.StrictJsonError as exc:
        raise _stop(corrupt, name, exc.detail) from None


def _is_int_or_none(value: object) -> bool:
    return value is None or (isinstance(value, int) and not isinstance(value, bool))


def _read_result(dir_fd: int) -> dict:
    text = _read_bounded(dir_fd, RESULT_NAME, IncompleteReason.RESULT_MISSING, IncompleteReason.RESULT_CORRUPT)
    doc = _decode(text, RESULT_NAME, IncompleteReason.RESULT_CORRUPT)
    if not isinstance(doc, dict):
        raise _stop(IncompleteReason.RESULT_CORRUPT, RESULT_NAME, "not an object")
    if doc.get("schema") != RESULT_SCHEMA:
        raise _stop(IncompleteReason.RESULT_SCHEMA, RESULT_NAME, "unknown layout marker")
    shape_ok = (
        all(isinstance(doc.get(k), str) for k in RESULT_STR)
        and all(k in doc and _is_int_or_none(doc[k]) for k in RESULT_INT_OR_NULL)
        and isinstance(doc.get("missing"), dict)
        and isinstance(doc.get("truncated"), list)
        and all(isinstance(n, str) for n in doc["truncated"])
    )
    if not shape_ok:
        raise _stop(IncompleteReason.RESULT_CORRUPT, RESULT_NAME, "unexpected shape")
    return doc


def _check_listed_gaps(doc: dict) -> None:
    for name in sorted(doc["missing"]):
        raise _stop(IncompleteReason.ARTIFACT_MISSING, name, "named missing by the writer")
    for name in sorted(doc["truncated"]):
        raise _stop(IncompleteReason.ARTIFACT_TRUNCATED, name)


def _read_snapshot(dir_fd: int, name: str) -> dict:
    doc = _decode(_read_text(dir_fd, name), name, IncompleteReason.ARTIFACT_CORRUPT)
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
    name = "upstream_errors.jsonl"
    rows = []
    for line in _lines(text):
        raw = _decode(line, name, IncompleteReason.ARTIFACT_CORRUPT)
        if not isinstance(raw, dict):
            raise _stop(IncompleteReason.ARTIFACT_CORRUPT, name, "row shape")
        try:
            row = UpstreamError(raw["provider_kind"], raw["status"], raw["excerpt"])
        except KeyError:
            raise _stop(IncompleteReason.ARTIFACT_CORRUPT, name, "row shape") from None
        if not (isinstance(row.provider_kind, str) and isinstance(row.excerpt, str)) or not _is_int_or_none(row.status) or row.status is None:
            raise _stop(IncompleteReason.ARTIFACT_CORRUPT, name, "row shape")
        rows.append(row)
    return tuple(rows)


def _ingress_tokens(text: str) -> Tuple[str, ...]:
    tokens = _lines(text)
    if not all(INGRESS_TOKEN_RE.fullmatch(t) for t in tokens):
        raise _stop(IncompleteReason.ARTIFACT_CORRUPT, "ingress_kinds.txt", "bad token")
    return tokens


def _fidelity_counters(lines: Tuple[str, ...]) -> Dict[str, int]:
    """Counters from the LAST snapshot line: the line is emitted each time
    /status/health is served, so the last one is the end-of-cell state."""
    if not lines:
        return {}
    if FIDELITY_COUNTER_OVERLONG_RE.search(lines[-1]):
        raise _stop(IncompleteReason.ARTIFACT_CORRUPT, "fidelity.log", "counter too long")
    return {m.group(1): int(m.group(2)) for m in FIDELITY_COUNTER_RE.finditer(lines[-1])}


def _read(dir_fd: int) -> CellEvidence:
    doc = _read_result(dir_fd)
    _check_listed_gaps(doc)
    health = _read_snapshot(dir_fd, "health.json")
    _check_health_shape(health)
    usage = _read_snapshot(dir_fd, "usage.json")
    _check_usage_shape(usage)
    texts = {name: _read_text(dir_fd, name) for name in TRACE_ARTIFACTS}
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
        ingress_kinds=_ingress_tokens(texts["ingress_kinds.txt"]),
        upstream_errors=_upstream_errors(texts["upstream_errors.jsonl"]),
        fidelity_lines=fidelity,
        fidelity_counters=_fidelity_counters(fidelity),
    )


def read_dir(evidence_dir: str) -> Result:
    try:
        dir_fd = _open_evidence_dir(evidence_dir)
    except _Stop as stop:
        return stop.incomplete
    try:
        return _read(dir_fd)
    except _Stop as stop:
        return stop.incomplete
    finally:
        os.close(dir_fd)


def read_cell(out_dir: str) -> Result:
    return read_dir(os.path.join(out_dir, EVIDENCE_DIRNAME))


def read_ingress_kinds(evidence_dir: str) -> Optional[Tuple[str, ...]]:
    try:
        dir_fd = _open_evidence_dir(evidence_dir)
    except _Stop:
        return None
    try:
        return _ingress_kinds(dir_fd)
    finally:
        os.close(dir_fd)


def _ingress_kinds(dir_fd: int) -> Optional[Tuple[str, ...]]:
    try:
        doc = _read_result(dir_fd)
    except _Stop:
        doc = None
    if doc is not None and "ingress_kinds.txt" in doc["missing"]:
        return None
    try:
        return _ingress_tokens(_read_text(dir_fd, "ingress_kinds.txt"))
    except _Stop:
        return None
