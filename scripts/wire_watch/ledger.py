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
a caller passes through it can carry a credential into run.json. An EXITED
cell may also carry `tokens`, the token counts its usage snapshot reported
(null when the snapshot could not be read); no other state carries them.

run.json is written by `write_atomic` (a fresh mkstemp temp file in the
same directory, fsync, rename, directory fsync; the temp is removed on any
failure before the rename): a crash at any point leaves either the previous
run.json or the new one, never a torn file. Its bytes depend only on the
ledger's content, so the same inputs give byte-identical files.

write_atomic makes each write atomic, not a read-modify-write: two writers
that each load, transition and write lose one update. Callers must hold
the run lock across load -> transition -> write_atomic.

Public API
  CellSpec, Entry, RunInputs, RunLedger, Tokens   frozen dataclasses
  CellState, Outcome, NotRunReason, CapReason, ModelTier, RunStop   enums
  LedgerError                              illegal transition / bad file
  DRIVER_RE, LANE_RE, KEY_FILE_RE, TOKEN_RE, CONNECTION_MODES
                                           the cell-field rules matrix.py
                                           validates cells.tsv with and
                                           from_json validates run.json with
  RunLedger.new(inputs, specs)             every cell PENDING
  RunLedger.transition(cell_id, to, ...)   new ledger with one cell moved
  RunLedger.entry(cell_id)                 one cell's entry
  RunLedger.run_stop                       None, or the RunStop that bound-
                                           stopped the run as a whole
  RunLedger.with_run_stop(run_stop)        new ledger with run_stop set
  RunLedger.with_tokens(cell_id, tokens)   new ledger with an EXITED cell's
                                           token counts set
  tokens_from_totals(totals)               Tokens from a usage snapshot's
                                           totals, or None when any field is
                                           absent or not a non-negative int
  RunLedger.to_json() / RunLedger.from_json(text)
                                           from_json decodes with strict_json:
                                           a duplicate key or NaN/Infinity ->
                                           LedgerError "run.json document: ..."
                                           from_json is strict: wrong type,
                                           unknown key or out-of-vocabulary
                                           value -> LedgerError naming the
                                           field; nothing is coerced
  write_atomic(path, ledger) / load(path)   caller holds the run lock for
                                           load -> transition -> write
  terminal_outcome(entry)                  Outcome for a terminal state
                                           other than EXITED, else None
  bound_stopped(entry)                     whether a bound or a signal ended
                                           the cell (any such cell makes the
                                           run INCOMPLETE)
  STOP_DETAILS                             the detail tokens that make an
                                           EXITED cell bound-stopped
"""

from __future__ import annotations

import enum
import json
import os
import re
import sys
import tempfile
from dataclasses import dataclass, replace
from typing import Dict, FrozenSet, Iterable, Mapping, Optional, Tuple, Type, TypeVar

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import strict_json  # noqa: E402

SCHEMA = "routectl-wire-watch-run/1"

# Cell ids name per-cell directories and container names downstream, so
# they are restricted to a charset that cannot spell a path separator, a
# leading dot or whitespace.
CELL_ID_RE = re.compile(r"^[a-z0-9]+(?:[._-][a-z0-9]+)*$")
CELL_ID_MAX = 200
# The cell fields downstream code turns into file names, directory names and
# argv: one definition, used both where cells.tsv is validated (matrix.py)
# and where run.json is read back, so the two cannot drift.
DRIVER_RE = re.compile(r"^[a-z0-9][a-z0-9-]*\.sh$")
LANE_RE = re.compile(r"^[a-z0-9][a-z0-9-]*(\.[a-z0-9][a-z0-9-]*)?$")
KEY_FILE_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
TOKEN_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")
CONNECTION_MODES = ("base-url", "front-proxy")
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


class RunStop(str, enum.Enum):
    """Why the run as a whole is INCOMPLETE whatever its cells say,
    recorded when it happens. Only the first is kept.

    RUN_SIZE             the cells' scratch trees reached the run cap
    LEFTOVER_CONTAINERS  the end-of-run sweep found (and removed) a
                         container of this run
    SWEEP_FAILED         the end-of-run sweep could not be completed
    """

    RUN_SIZE = "run-size"
    LEFTOVER_CONTAINERS = "leftover-containers"
    SWEEP_FAILED = "sweep-failed"


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
class Tokens:
    """A cell's token counts, from its usage snapshot's totals."""

    input_tokens: int
    output_tokens: int
    reasoning_tokens: int
    cache_read_billed: int


TOKEN_FIELDS: Tuple[str, ...] = ("input_tokens", "output_tokens", "reasoning_tokens", "cache_read_billed")


def _is_count(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def tokens_from_totals(totals: Mapping[str, object]) -> Optional[Tokens]:
    """Tokens from a usage `totals` object, or None unless every field is a
    non-negative integer."""
    counts: Dict[str, int] = {}
    for name in TOKEN_FIELDS:
        value = totals.get(name)
        if not isinstance(value, int) or not _is_count(value):
            return None
        counts[name] = value
    return Tokens(**counts)


@dataclass(frozen=True)
class Entry:
    spec: CellSpec
    state: CellState = CellState.PENDING
    rc: Optional[int] = None
    reason: Optional[str] = None
    detail: Optional[str] = None
    tokens: Optional[Tokens] = None


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
    if detail is not None and not (isinstance(detail, str) and len(detail) <= DETAIL_MAX and DETAIL_RE.fullmatch(detail)):
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


# An EXITED cell is bound-stopped when its detail is one of these: the
# runner stopped it (timeout, size, interrupted) but its daemon outlived
# SIGKILL, or it is the cell that recorded the aggregate capture cap being
# crossed (run-size) when no unstarted cell was left to carry
# CAP_REACHED(run-size).
STOP_DETAILS: FrozenSet[str] = frozenset({"timeout", "size", "interrupted", "run-size"})
_BOUND_STATES = frozenset({CellState.TIMEOUT, CellState.CAP_REACHED, CellState.INTERRUPTED})


def bound_stopped(entry: Entry) -> bool:
    if entry.state in _BOUND_STATES:
        return True
    return entry.state is CellState.EXITED and entry.detail in STOP_DETAILS


@dataclass(frozen=True)
class RunLedger:
    inputs: RunInputs
    entries: Tuple[Entry, ...]
    run_stop: Optional[RunStop] = None

    @staticmethod
    def new(inputs: RunInputs, specs: Iterable[CellSpec]) -> "RunLedger":
        entries = tuple(Entry(spec=spec) for spec in specs)
        seen: Dict[str, None] = {}
        for entry in entries:
            if len(entry.spec.id) > CELL_ID_MAX or not CELL_ID_RE.fullmatch(entry.spec.id):
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

    def with_tokens(self, cell_id: str, tokens: Tokens) -> "RunLedger":
        i = self._index(cell_id)
        entry = self.entries[i]
        if entry.state is not CellState.EXITED:
            raise LedgerError(f"cell {cell_id}: only an EXITED cell carries tokens")
        if not isinstance(tokens, Tokens) or not all(_is_count(getattr(tokens, f)) for f in TOKEN_FIELDS):
            raise LedgerError(f"cell {cell_id}: tokens must be non-negative integers")
        moved = replace(entry, tokens=tokens)
        return replace(self, entries=self.entries[:i] + (moved,) + self.entries[i + 1:])

    def with_run_stop(self, run_stop: RunStop) -> "RunLedger":
        if not isinstance(run_stop, RunStop):
            raise LedgerError(f"run_stop must be one of {[m.value for m in RunStop]}")
        return replace(self, run_stop=run_stop)

    def to_json(self) -> str:
        doc = {
            "schema": SCHEMA,
            "run_stop": None if self.run_stop is None else self.run_stop.value,
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
        """Parse run.json strictly: every field must already have the type
        and vocabulary to_json writes; nothing is coerced."""
        try:
            doc = strict_json.loads(text)
        except strict_json.StrictJsonError as exc:
            raise LedgerError(f"run.json document: {exc.detail}") from None
        doc = _object(doc, _DOC_KEYS, "run.json")
        if doc["schema"] != SCHEMA:
            raise LedgerError(f"schema is {doc['schema']!r}, want {SCHEMA}")
        inputs = _inputs_from_dict(doc["inputs"])
        cells = doc["cells"]
        if not isinstance(cells, list):
            raise LedgerError("cells must be a list")
        entries = tuple(_entry_from_dict(raw, f"cells[{i}]") for i, raw in enumerate(cells))
        run_stop = None if doc["run_stop"] is None else _member(RunStop, doc, "run_stop", "run.json")
        ledger = RunLedger.new(inputs, (e.spec for e in entries))
        return replace(ledger, entries=entries, run_stop=run_stop)


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
        "tokens": None if entry.tokens is None else {f: getattr(entry.tokens, f) for f in TOKEN_FIELDS},
    }


_DOC_KEYS = frozenset({"schema", "inputs", "cells", "run_stop"})
_INPUTS_KEYS = frozenset({"cells_sha256", "models_sha256", "cases"})
_SPEC_STR_KEYS = ("driver", "expected_ingress", "lane", "connection_mode", "model", "case")
_ENTRY_KEYS = frozenset(
    {"id", "providers", "key_file", "tier", "state", "rc", "reason", "detail", "tokens", *_SPEC_STR_KEYS}
)
_TOKENS_KEYS = frozenset(TOKEN_FIELDS)
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")


def _object(raw: object, keys: FrozenSet[str], where: str) -> dict:
    if not isinstance(raw, dict):
        raise LedgerError(f"{where} must be an object")
    if set(raw) != keys:
        missing, extra = sorted(keys - set(raw)), sorted(set(raw) - keys)
        raise LedgerError(f"{where} keys: missing {missing}, unexpected {extra}")
    return raw


def _str(raw: dict, key: str, where: str) -> str:
    value = raw[key]
    if not isinstance(value, str):
        raise LedgerError(f"{where}.{key} must be a string")
    return value


def _str_list(raw: dict, key: str, where: str) -> Tuple[str, ...]:
    value = raw[key]
    if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
        raise LedgerError(f"{where}.{key} must be a list of strings")
    return tuple(value)


def _optional_str(raw: dict, key: str, where: str) -> Optional[str]:
    return None if raw[key] is None else _str(raw, key, where)


_E = TypeVar("_E", bound=enum.Enum)


def _member(enum_type: Type[_E], raw: dict, key: str, where: str) -> _E:
    value = raw[key]
    if not isinstance(value, str):
        raise LedgerError(f"{where}.{key} must be a string")
    try:
        return enum_type(value)
    except ValueError:
        raise LedgerError(f"{where}.{key} {value!r} is not one of {[m.value for m in enum_type]}") from None


def _inputs_from_dict(raw: object) -> RunInputs:
    raw = _object(raw, _INPUTS_KEYS, "inputs")
    for key in ("cells_sha256", "models_sha256"):
        if not (isinstance(raw[key], str) and SHA256_RE.fullmatch(raw[key])):
            raise LedgerError(f"inputs.{key} must be 64 lowercase hex digits")
    return RunInputs(
        cells_sha256=raw["cells_sha256"],
        models_sha256=raw["models_sha256"],
        cases=_str_list(raw, "cases", "inputs"),
    )


_FIELD_PATTERNS = {
    "driver": DRIVER_RE,
    "lane": LANE_RE,
    "expected_ingress": TOKEN_RE,
    "model": TOKEN_RE,
    "case": TOKEN_RE,
}


def _check_names(fields: Mapping[str, str], raw: dict, where: str) -> None:
    for key, pattern in _FIELD_PATTERNS.items():
        if not pattern.fullmatch(fields[key]):
            raise LedgerError(f"{where}.{key} must match {pattern.pattern}")
    if fields["connection_mode"] not in CONNECTION_MODES:
        raise LedgerError(f"{where}.connection_mode must be one of {list(CONNECTION_MODES)}")
    providers = _str_list(raw, "providers", where)
    if not providers or not all(TOKEN_RE.fullmatch(p) for p in providers):
        raise LedgerError(f"{where}.providers must be a nonempty list matching {TOKEN_RE.pattern}")
    key_file = _optional_str(raw, "key_file", where)
    if key_file is not None and not KEY_FILE_RE.fullmatch(key_file):
        raise LedgerError(f"{where}.key_file must be null or match {KEY_FILE_RE.pattern}")


def _entry_from_dict(raw: object, where: str) -> Entry:
    raw = _object(raw, _ENTRY_KEYS, where)
    cell_id = _str(raw, "id", where)
    if len(cell_id) > CELL_ID_MAX or not CELL_ID_RE.fullmatch(cell_id):
        raise LedgerError(f"{where}.id is not a safe token")
    fields = {key: _str(raw, key, where) for key in _SPEC_STR_KEYS}
    _check_names(fields, raw, where)
    spec = CellSpec(
        id=cell_id,
        providers=_str_list(raw, "providers", where),
        key_file=_optional_str(raw, "key_file", where),
        tier=_member(ModelTier, raw, "tier", where),
        **fields,
    )
    state = _member(CellState, raw, "state", where)
    rc = raw["rc"]
    if rc is not None and (not isinstance(rc, int) or isinstance(rc, bool)):
        raise LedgerError(f"{where}.rc must be an integer or null")
    reason = _optional_str(raw, "reason", where)
    detail = _optional_str(raw, "detail", where)
    try:
        if state is not CellState.PENDING:
            _check_payload(state, rc, reason, detail)
        elif (rc, reason, detail) != (None, None, None):
            raise LedgerError("PENDING carries rc/reason/detail")
    except LedgerError as exc:
        raise LedgerError(f"{where} ({cell_id}): {exc}") from None
    tokens = _tokens_from_dict(raw["tokens"], f"{where}.tokens")
    if tokens is not None and state is not CellState.EXITED:
        raise LedgerError(f"{where}.tokens: only an EXITED cell carries tokens")
    return Entry(spec=spec, state=state, rc=rc, reason=reason, detail=detail, tokens=tokens)


def _tokens_from_dict(raw: object, where: str) -> Optional[Tokens]:
    if raw is None:
        return None
    raw = _object(raw, _TOKENS_KEYS, where)
    tokens = tokens_from_totals(raw)
    if tokens is None:
        raise LedgerError(f"{where} values must be non-negative integers")
    return tokens


def write_atomic(path: str, ledger: RunLedger) -> None:
    """Replace `path` with the ledger's JSON so that a crash at any point
    leaves either the old file or the new one."""
    directory = os.path.dirname(os.path.abspath(path))
    data = ledger.to_json().encode("utf-8")
    fd, tmp = tempfile.mkstemp(prefix="." + os.path.basename(path) + ".", suffix=".tmp", dir=directory)
    try:
        try:
            view = memoryview(data)
            while view:
                written = os.write(fd, view)
                view = view[written:]
            os.fsync(fd)
        finally:
            os.close(fd)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    dir_fd = os.open(directory, os.O_RDONLY)
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)


def load(path: str) -> RunLedger:
    with open(path, encoding="utf-8") as handle:
        return RunLedger.from_json(handle.read())
