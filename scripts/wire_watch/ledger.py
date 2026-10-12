"""The wire-watch run ledger: cell vocabulary, state machine, run.json.

Stdlib only. This module is the lowest layer of the wire-watch scripts:
matrix.py builds a ledger from the committed denominator, the runner
advances it, and the report reads it back.

Vocabulary
  CellState    where a cell is in the run: PENDING, RUNNING, EXITED(rc),
               TIMEOUT, CAP_REACHED(reason), INTERRUPTED, NOT_RUN(reason).
  Outcome      what a cell ended as in the report: PASS, FINDING,
               NOT_RUN(reason), FAILED, TIMEOUT, CAP_REACHED, INTERRUPTED,
               INCOMPLETE. NOT_RUN is never PASS.
  NotRunReason driver | credential | ingress | model-access.
  CapReason    deadline | size | run-size.

State machine (anything else raises LedgerError):
  PENDING -> RUNNING | NOT_RUN | CAP_REACHED | INTERRUPTED
  RUNNING -> EXITED | TIMEOUT | CAP_REACHED | INTERRUPTED | NOT_RUN
  EXITED, TIMEOUT, CAP_REACHED, INTERRUPTED, NOT_RUN are terminal.
EXITED carries an integer rc; CAP_REACHED a CapReason; NOT_RUN a
NotRunReason; every other state carries neither. `detail` is an optional
short token (lowercase letters, digits, `-`), never free text, so nothing
a caller passes through it can carry a credential into run.json.

run.json is written by `write_atomic` (temp file in the same directory,
fsync, rename, directory fsync): a crash at any point leaves either the
previous run.json or the new one, never a torn file. Its bytes depend only
on the ledger's content, so the same inputs give byte-identical files.

Public API
  CellSpec, Entry, RunInputs, RunLedger   frozen dataclasses
  CellState, Outcome, NotRunReason, CapReason, ModelTier   enums
  LedgerError                              illegal transition / bad file
  RunLedger.new(inputs, specs)             every cell PENDING
  RunLedger.transition(cell_id, to, ...)   new ledger with one cell moved
  RunLedger.entry(cell_id)                 one cell's entry
  RunLedger.to_json() / RunLedger.from_json(text)
  write_atomic(path, ledger) / load(path)
  terminal_outcome(entry)                  Outcome for a terminal state
                                           other than EXITED, else None
"""

from __future__ import annotations

import enum
import json
import os
import re
from dataclasses import dataclass, replace
from typing import Dict, FrozenSet, Iterable, Mapping, Optional, Tuple

SCHEMA = "routectl-wire-watch-run/1"

# Cell ids name per-cell directories and container names downstream, so
# they are restricted to a charset that cannot spell a path separator, a
# leading dot or whitespace.
CELL_ID_RE = re.compile(r"^[a-z0-9]+(?:[._-][a-z0-9]+)*$")
CELL_ID_MAX = 200
DETAIL_RE = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
DETAIL_MAX = 64


class LedgerError(Exception):
    """An illegal transition, or a run.json that does not parse as a ledger."""


class CellState(str, enum.Enum):
    PENDING = "PENDING"
    RUNNING = "RUNNING"
    EXITED = "EXITED"
    TIMEOUT = "TIMEOUT"
    CAP_REACHED = "CAP_REACHED"
    INTERRUPTED = "INTERRUPTED"
    NOT_RUN = "NOT_RUN"


class Outcome(str, enum.Enum):
    PASS = "PASS"
    FINDING = "FINDING"
    NOT_RUN = "NOT_RUN"
    FAILED = "FAILED"
    TIMEOUT = "TIMEOUT"
    CAP_REACHED = "CAP_REACHED"
    INTERRUPTED = "INTERRUPTED"
    INCOMPLETE = "INCOMPLETE"


class NotRunReason(str, enum.Enum):
    DRIVER = "driver"
    CREDENTIAL = "credential"
    INGRESS = "ingress"
    MODEL_ACCESS = "model-access"


class CapReason(str, enum.Enum):
    DEADLINE = "deadline"
    SIZE = "size"
    RUN_SIZE = "run-size"


class ModelTier(str, enum.Enum):
    PRIMARY = "primary"
    SWEEP = "sweep"
    LANE_DEFAULT = "lane-default"


TERMINAL: FrozenSet[CellState] = frozenset(
    {
        CellState.EXITED,
        CellState.TIMEOUT,
        CellState.CAP_REACHED,
        CellState.INTERRUPTED,
        CellState.NOT_RUN,
    }
)

ALLOWED: Mapping[CellState, FrozenSet[CellState]] = {
    CellState.PENDING: frozenset(
        {CellState.RUNNING, CellState.NOT_RUN, CellState.CAP_REACHED, CellState.INTERRUPTED}
    ),
    CellState.RUNNING: frozenset(
        {
            CellState.EXITED,
            CellState.TIMEOUT,
            CellState.CAP_REACHED,
            CellState.INTERRUPTED,
            CellState.NOT_RUN,
        }
    ),
}

_REASON_VOCAB = {
    CellState.NOT_RUN: frozenset(r.value for r in NotRunReason),
    CellState.CAP_REACHED: frozenset(r.value for r in CapReason),
}


@dataclass(frozen=True)
class CellSpec:
    """What one cell runs. Identity only: no path, no credential value."""

    id: str
    driver: str
    expected_ingress: str
    lane: str
    connection_mode: str
    providers: Tuple[str, ...]
    key_file: Optional[str]
    model: str
    tier: ModelTier
    case: str


@dataclass(frozen=True)
class Entry:
    spec: CellSpec
    state: CellState = CellState.PENDING
    rc: Optional[int] = None
    reason: Optional[str] = None
    detail: Optional[str] = None


@dataclass(frozen=True)
class RunInputs:
    """Digests of the committed inputs the cells were expanded from."""

    cells_sha256: str
    models_sha256: str
    cases: Tuple[str, ...]


def _check_payload(to: CellState, rc: Optional[int], reason: Optional[str], detail: Optional[str]) -> None:
    if to is CellState.EXITED:
        if not isinstance(rc, int) or isinstance(rc, bool):
            raise LedgerError("EXITED requires an integer rc")
    elif rc is not None:
        raise LedgerError(f"{to.value} carries no rc")
    vocab = _REASON_VOCAB.get(to)
    if vocab is not None:
        if reason not in vocab:
            raise LedgerError(f"{to.value} requires a reason in {sorted(vocab)}, got {reason!r}")
    elif reason is not None:
        raise LedgerError(f"{to.value} carries no reason")
    if detail is not None and not (isinstance(detail, str) and len(detail) <= DETAIL_MAX and DETAIL_RE.match(detail)):
        raise LedgerError(f"detail must match {DETAIL_RE.pattern}")


def advance(
    entry: Entry,
    to: CellState,
    *,
    rc: Optional[int] = None,
    reason: Optional[str] = None,
    detail: Optional[str] = None,
) -> Entry:
    """Return `entry` moved to `to`, or raise LedgerError."""
    if to not in ALLOWED.get(entry.state, frozenset()):
        raise LedgerError(f"cell {entry.spec.id}: illegal transition {entry.state.value} -> {to.value}")
    _check_payload(to, rc, reason, detail)
    return replace(entry, state=to, rc=rc, reason=reason, detail=detail)


def terminal_outcome(entry: Entry) -> Optional[Outcome]:
    """The report outcome a terminal state fixes on its own. EXITED and
    non-terminal states return None: an exited cell's outcome needs its
    evidence."""
    return {
        CellState.TIMEOUT: Outcome.TIMEOUT,
        CellState.CAP_REACHED: Outcome.CAP_REACHED,
        CellState.INTERRUPTED: Outcome.INTERRUPTED,
        CellState.NOT_RUN: Outcome.NOT_RUN,
    }.get(entry.state)


@dataclass(frozen=True)
class RunLedger:
    inputs: RunInputs
    entries: Tuple[Entry, ...]

    @staticmethod
    def new(inputs: RunInputs, specs: Iterable[CellSpec]) -> "RunLedger":
        entries = tuple(Entry(spec=spec) for spec in specs)
        seen: Dict[str, None] = {}
        for entry in entries:
            if len(entry.spec.id) > CELL_ID_MAX or not CELL_ID_RE.match(entry.spec.id):
                raise LedgerError(f"cell id {entry.spec.id!r} is not a safe token")
            if entry.spec.id in seen:
                raise LedgerError(f"duplicate cell id {entry.spec.id}")
            seen[entry.spec.id] = None
        return RunLedger(inputs=inputs, entries=entries)

    def _index(self, cell_id: str) -> int:
        for i, entry in enumerate(self.entries):
            if entry.spec.id == cell_id:
                return i
        raise LedgerError(f"no cell {cell_id!r}")

    def entry(self, cell_id: str) -> Entry:
        return self.entries[self._index(cell_id)]

    def transition(
        self,
        cell_id: str,
        to: CellState,
        *,
        rc: Optional[int] = None,
        reason: Optional[str] = None,
        detail: Optional[str] = None,
    ) -> "RunLedger":
        i = self._index(cell_id)
        moved = advance(self.entries[i], to, rc=rc, reason=reason, detail=detail)
        return replace(self, entries=self.entries[:i] + (moved,) + self.entries[i + 1:])

    def to_json(self) -> str:
        doc = {
            "schema": SCHEMA,
            "inputs": {
                "cells_sha256": self.inputs.cells_sha256,
                "models_sha256": self.inputs.models_sha256,
                "cases": list(self.inputs.cases),
            },
            "cells": [_entry_to_dict(e) for e in self.entries],
        }
        return json.dumps(doc, indent=2, sort_keys=True) + "\n"

    @staticmethod
    def from_json(text: str) -> "RunLedger":
        try:
            doc = json.loads(text)
            if doc.get("schema") != SCHEMA:
                raise LedgerError(f"run.json schema is {doc.get('schema')!r}, want {SCHEMA}")
            raw_inputs = doc["inputs"]
            inputs = RunInputs(
                cells_sha256=str(raw_inputs["cells_sha256"]),
                models_sha256=str(raw_inputs["models_sha256"]),
                cases=tuple(str(c) for c in raw_inputs["cases"]),
            )
            entries = tuple(_entry_from_dict(raw) for raw in doc["cells"])
        except LedgerError:
            raise
        except (ValueError, KeyError, TypeError, AttributeError) as exc:
            raise LedgerError(f"run.json does not parse: {exc.__class__.__name__}: {exc}") from exc
        ledger = RunLedger.new(inputs, (e.spec for e in entries))
        return replace(ledger, entries=entries)


def _entry_to_dict(entry: Entry) -> dict:
    spec = entry.spec
    return {
        "id": spec.id,
        "driver": spec.driver,
        "expected_ingress": spec.expected_ingress,
        "lane": spec.lane,
        "connection_mode": spec.connection_mode,
        "providers": list(spec.providers),
        "key_file": spec.key_file,
        "model": spec.model,
        "tier": spec.tier.value,
        "case": spec.case,
        "state": entry.state.value,
        "rc": entry.rc,
        "reason": entry.reason,
        "detail": entry.detail,
    }


def _entry_from_dict(raw: dict) -> Entry:
    spec = CellSpec(
        id=str(raw["id"]),
        driver=str(raw["driver"]),
        expected_ingress=str(raw["expected_ingress"]),
        lane=str(raw["lane"]),
        connection_mode=str(raw["connection_mode"]),
        providers=tuple(str(p) for p in raw["providers"]),
        key_file=None if raw["key_file"] is None else str(raw["key_file"]),
        model=str(raw["model"]),
        tier=ModelTier(raw["tier"]),
        case=str(raw["case"]),
    )
    state = CellState(raw["state"])
    rc, reason, detail = raw["rc"], raw["reason"], raw["detail"]
    if state is not CellState.PENDING:
        _check_payload(state, rc, reason, detail)
    elif (rc, reason, detail) != (None, None, None):
        raise LedgerError(f"cell {spec.id}: PENDING carries rc/reason/detail")
    return Entry(spec=spec, state=state, rc=rc, reason=reason, detail=detail)


def write_atomic(path: str, ledger: RunLedger) -> None:
    """Replace `path` with the ledger's JSON so that a crash at any point
    leaves either the old file or the new one."""
    directory = os.path.dirname(os.path.abspath(path))
    tmp = os.path.join(directory, "." + os.path.basename(path) + ".tmp")
    data = ledger.to_json().encode("utf-8")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        view = memoryview(data)
        while view:
            written = os.write(fd, view)
            view = view[written:]
        os.fsync(fd)
    finally:
        os.close(fd)
    os.replace(tmp, path)
    dir_fd = os.open(directory, os.O_RDONLY)
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)


def load(path: str) -> RunLedger:
    with open(path, encoding="utf-8") as handle:
        return RunLedger.from_json(handle.read())
