#!/usr/bin/env python3
"""The wire-watch denominator: validate cells.tsv, expand it, preflight it.

Stdlib only (CI's Python predates tomllib). Reads the committed inputs
and produces a ledger.RunLedger whose cells are every (row, model, case)
the run owes, with the cells that cannot run already marked NOT_RUN.

cells.tsv (tab-separated, `#` comments, one header row) has one row per
(driver, lane, connection mode):
  driver            a file under scripts/drivers/ (e.g. codex.sh)
  expected_ingress  a member of scripts/drivers/lib/ingress_kinds.sh
  lane              scripts/drivers/config/<lane>.toml must exist
  connection_mode   base-url (config has no [mitm]) or front-proxy (has one)
  providers         comma list; exactly the providers whose
                    ROUTECTL_DRIVER_<PROVIDER>_* env refs the lane config
                    declares
  key_file          a file name in the key directory holding the one
                    provider's API key, or `-` when the credential is a seat
  sweep             `-`, or the comma list of the lane config's provider
                    tables whose sweep-tier models this row runs

Expansion. A lane listed in models.tsv is a Bedrock lane: every row on it
runs the lane's primary model on every case, and the ONE row naming a
provider table in `sweep` runs each sweep-tier model of that table on the
sweep set -- the first baseline case, the first tool-use-multiturn case,
and (only where models.tsv says thinking=yes) the first thinking case
without tools. Every other lane runs its [aliases] default on every case.
Each provider table of a Bedrock lane that has sweep-tier models must be
swept by exactly one row, so the candidate list is covered once and never
multiplied by the harness count.

Preflight, per cell, in this order: the driver file is missing or not
executable -> NOT_RUN(driver); the key file fails `open_key_file` ->
NOT_RUN(credential) with that function's detail token. Preflight opens and
fstat()s the key file and closes it unread, so no key value can reach the
ledger or a message.

Key files. `open_key_file(key_dir, name)` opens the key directory and then
the key relative to that directory's descriptor, and judges both open
descriptors, never a path, so what is checked is what is read. Neither the
key directory nor the key may be a symlink (O_NOFOLLOW on each): following
a link would let whoever can write it choose which file is exported. The
directory must be owned by the current user and writable by no one else,
so no other user can swap a key in after the check; the key must be a
regular, nonempty file owned by the current user with no group/other
permission bit. Path components above key_dir are trusted. key_dir is
first made absolute with os.path.abspath, which resolves `.` and `..`
lexically against the physical working directory, so `link/../keys` names
`<cwd>/keys`; whatever directory that names still has to pass the owner,
mode and no-symlink checks.

Preflight's verdict is only a forecast. The runner MUST call
open_key_file again at launch, unchanged, and read the key from the
descriptor it returns immediately before export, since the files can
change between preflight and launch.

Public API
  Paths.from_root(root)          the committed input locations
  CellRow                        one validated cells.tsv row
  MatrixError                    refusal; .failures names each bad row
  load_rows(paths)               validated rows, or MatrixError
  expand(paths, rows)            ordered ledger.CellSpec tuple
  open_key_file(key_dir, name)   (fd, None) for a usable key -- caller reads
                                 then closes fd -- or (None, detail token);
                                 checks key_dir and key by descriptor
  key_file_problem(key_dir, name)  None, or the NOT_RUN(credential) detail;
                                 open_key_file with the descriptor closed
  preflight(ledger, paths, key_dir)  ledger with unrunnable cells NOT_RUN
  build_ledger(paths, key_dir)   load_rows + expand + preflight
  run_inputs(paths)              ledger.RunInputs digests

CLI
  matrix.py validate [--root R]            exit 1 on any refused row
  matrix.py expand --key-dir D [--root R]  print run.json to stdout
"""

from __future__ import annotations

import errno
import hashlib
import os
import re
import stat
import sys
from dataclasses import dataclass
from typing import Dict, List, Optional, Sequence, Set, Tuple

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "..", "drivers", "config"))
sys.path.insert(0, os.path.join(HERE, "..", "drivers", "lib"))

import lane_configs  # noqa: E402
import nofollow  # noqa: E402
import validate_case  # noqa: E402
from ledger import (  # noqa: E402
    CONNECTION_MODES,
    DRIVER_RE,
    KEY_FILE_RE,
    LANE_RE,
    TOKEN_RE,
    CellSpec,
    CellState,
    ModelTier,
    NotRunReason,
    RunInputs,
    RunLedger,
)

CELLS_COLUMNS = [
    "driver",
    "expected_ingress",
    "lane",
    "connection_mode",
    "providers",
    "key_file",
    "sweep",
]
NONE = "-"

PROVIDER_REF_RE = re.compile(r"env://ROUTECTL_DRIVER_([A-Z0-9]+)_(?:API_KEY|ACCOUNT_ID|TOKEN)\b")
MITM_RE = re.compile(r"^\[mitm\]$", re.MULTILINE)
INGRESS_BLOCK_RE = re.compile(
    r"^# --- BEGIN INGRESS_KINDS ---\n(.*?)^# --- END INGRESS_KINDS ---$",
    re.MULTILINE | re.DOTALL,
)
INGRESS_ITEM_RE = re.compile(r'^\s*"([a-z0-9-]+)"\s*$')

# The sweep set, by the case's wire_pattern: the first case id in sorted
# order that matches is the one swept.
SWEEP_BASELINE = "baseline"
SWEEP_TOOL = "tool-use-multiturn"
SWEEP_THINKING = "thinking"

# Any group/other bit: the key must be readable by its owner alone.
INSECURE_MODE_BITS = 0o077


class MatrixError(Exception):
    def __init__(self, failures: Sequence[str]) -> None:
        super().__init__("\n".join(failures))
        self.failures = tuple(failures)


@dataclass(frozen=True)
class Paths:
    cells_tsv: str
    models_tsv: str
    config_dir: str
    cases_dir: str
    drivers_dir: str
    ingress_kinds_sh: str

    @staticmethod
    def from_root(root: str) -> "Paths":
        scripts = os.path.join(root, "scripts")
        return Paths(
            cells_tsv=os.path.join(scripts, "wire_watch", "cells.tsv"),
            models_tsv=os.path.join(scripts, "wire_watch", "models.tsv"),
            config_dir=os.path.join(scripts, "drivers", "config"),
            cases_dir=os.path.join(scripts, "drivers", "cases"),
            drivers_dir=os.path.join(scripts, "drivers"),
            ingress_kinds_sh=os.path.join(scripts, "drivers", "lib", "ingress_kinds.sh"),
        )


@dataclass(frozen=True)
class CellRow:
    line: int
    driver: str
    expected_ingress: str
    lane: str
    connection_mode: str
    providers: Tuple[str, ...]
    key_file: Optional[str]
    sweep: Tuple[str, ...]


@dataclass(frozen=True)
class Candidate:
    nickname: str
    provider: str
    tier: ModelTier
    thinking: bool


@dataclass(frozen=True)
class Case:
    case_id: str
    wire_pattern: str
    tools: bool


def read_ingress_kinds(path: str) -> Tuple[str, ...]:
    with open(path, encoding="utf-8") as handle:
        block = INGRESS_BLOCK_RE.search(handle.read())
    if block is None:
        raise MatrixError([f"{path}: INGRESS_KINDS sentinels not found"])
    kinds = tuple(m.group(1) for m in map(INGRESS_ITEM_RE.match, block.group(1).splitlines()) if m)
    if not kinds:
        raise MatrixError([f"{path}: INGRESS_KINDS is empty"])
    return kinds


def _read_tsv(path: str, columns: List[str]) -> List[Tuple[int, Dict[str, str]]]:
    rows: List[Tuple[int, Dict[str, str]]] = []
    header_seen = False
    with open(path, encoding="utf-8") as handle:
        for number, raw in enumerate(handle, start=1):
            line = raw.rstrip("\n")
            if line.startswith("#") or not line.strip():
                continue
            fields = line.split("\t")
            if not header_seen:
                if fields != columns:
                    raise MatrixError([f"{path}:{number}: header must be {columns}, got {fields}"])
                header_seen = True
                continue
            if len(fields) != len(columns):
                raise MatrixError([f"{path}:{number}: expected {len(columns)} columns, got {len(fields)}"])
            rows.append((number, dict(zip(columns, fields))))
    if not header_seen:
        raise MatrixError([f"{path}: no header row"])
    return rows


def _split_list(value: str) -> Tuple[str, ...]:
    return () if value == NONE else tuple(value.split(","))


def _config_providers(text: str) -> Set[str]:
    return {m.group(1).lower() for m in PROVIDER_REF_RE.finditer(text)}


def _provider_tables(tables: lane_configs.Tables) -> Set[str]:
    prefix = "providers."
    return {
        name[len(prefix):]
        for name in tables
        if name.startswith(prefix) and "." not in name[len(prefix):]
    }


def bedrock_lanes(paths: Paths) -> Set[str]:
    rows, _ = lane_configs.read_models_tsv(paths.models_tsv)
    return {row["lane"] for _, row in rows}


def _row_failures(paths: Paths, raw: Dict[str, str], kinds: Tuple[str, ...], bedrock: Set[str]) -> List[str]:
    failures: List[str] = []
    if not DRIVER_RE.fullmatch(raw["driver"]):
        failures.append(f"driver {raw['driver']!r} must match {DRIVER_RE.pattern}")
    if raw["expected_ingress"] not in kinds:
        failures.append(f"unknown expected_ingress {raw['expected_ingress']!r} (known: {', '.join(kinds)})")
    mode = raw["connection_mode"]
    if mode not in CONNECTION_MODES:
        failures.append(f"unknown connection_mode {mode!r} (known: {', '.join(CONNECTION_MODES)})")
    key_file = raw["key_file"]
    providers = _split_list(raw["providers"])
    if key_file != NONE and not KEY_FILE_RE.fullmatch(key_file):
        failures.append(f"key_file {key_file!r} must be '-' or a plain file name")
    if key_file != NONE and len(providers) != 1:
        failures.append("a key_file serves exactly one provider")
    if not LANE_RE.fullmatch(raw["lane"]):
        failures.append(f"lane {raw['lane']!r} must match {LANE_RE.pattern}")
        return failures
    config = os.path.join(paths.config_dir, raw["lane"] + ".toml")
    if not os.path.isfile(config):
        failures.append(f"no lane config for lane {raw['lane']!r} at {config}")
        return failures
    with open(config, encoding="utf-8") as handle:
        text = handle.read()
    has_mitm = MITM_RE.search(text) is not None
    if mode == "front-proxy" and not has_mitm:
        failures.append(f"connection_mode front-proxy needs a [mitm] block in {config}, which has none")
    if mode == "base-url" and has_mitm:
        failures.append(f"connection_mode base-url refuses {config}: it carries a [mitm] block")
    declared = _config_providers(text)
    if not providers or set(providers) != declared or len(set(providers)) != len(providers):
        failures.append(f"providers {raw['providers']!r} must name exactly {sorted(declared)}")
    sweep = _split_list(raw["sweep"])
    if sweep and raw["lane"] not in bedrock:
        failures.append(f"sweep {raw['sweep']!r} on lane {raw['lane']!r}, which has no models.tsv candidates")
    elif sweep:
        tables = _provider_tables(lane_configs.read_config(config))
        for name in sweep:
            if name not in tables:
                failures.append(f"sweep names {name!r}, not a provider table of {config}")
    return failures


def _sweep_coverage(paths: Paths, rows: List[CellRow]) -> List[str]:
    failures: List[str] = []
    by_lane = _candidates_by_lane(paths)
    for lane in sorted(by_lane):
        need = sorted({c.provider for c in by_lane[lane] if c.tier is ModelTier.SWEEP})
        for provider in need:
            sweepers = [r.line for r in rows if r.lane == lane and provider in r.sweep]
            if len(sweepers) != 1:
                failures.append(
                    f"lane {lane}: provider {provider!r} must be swept by exactly one row, got lines {sweepers}"
                )
    return failures


def load_rows(paths: Paths) -> Tuple[CellRow, ...]:
    """Validated cells.tsv rows. Raises MatrixError naming every bad row."""
    kinds = read_ingress_kinds(paths.ingress_kinds_sh)
    bedrock = bedrock_lanes(paths)
    failures: List[str] = []
    rows: List[CellRow] = []
    seen: Dict[Tuple[str, str, str], int] = {}
    for number, raw in _read_tsv(paths.cells_tsv, CELLS_COLUMNS):
        where = f"{paths.cells_tsv}:{number}"
        problems = _row_failures(paths, raw, kinds, bedrock)
        key = (raw["driver"], raw["lane"], raw["connection_mode"])
        if key in seen:
            problems.append(f"duplicates line {seen[key]} (same driver, lane, connection_mode)")
        seen.setdefault(key, number)
        failures.extend(f"{where}: {p}" for p in problems)
        if not problems:
            rows.append(
                CellRow(
                    line=number,
                    driver=raw["driver"],
                    expected_ingress=raw["expected_ingress"],
                    lane=raw["lane"],
                    connection_mode=raw["connection_mode"],
                    providers=_split_list(raw["providers"]),
                    key_file=None if raw["key_file"] == NONE else raw["key_file"],
                    sweep=_split_list(raw["sweep"]),
                )
            )
    if not rows and not failures:
        failures.append(f"{paths.cells_tsv}: no rows")
    if not failures:
        failures.extend(_sweep_coverage(paths, rows))
    if failures:
        raise MatrixError(failures)
    return tuple(rows)


def _candidates_by_lane(paths: Paths) -> Dict[str, List[Candidate]]:
    rows, _ = lane_configs.read_models_tsv(paths.models_tsv)
    configs: Dict[str, lane_configs.Tables] = {}
    out: Dict[str, List[Candidate]] = {}
    for number, row in rows:
        lane = row["lane"]
        if lane not in configs:
            configs[lane] = lane_configs.model_tables(
                lane_configs.read_config(os.path.join(paths.config_dir, lane + ".toml"))
            )
        model = configs[lane].get(row["nickname"])
        provider = lane_configs.string_value(model.get("provider", "")) if model else None
        if provider is None:
            raise MatrixError([f"{paths.models_tsv}:{number}: {row['nickname']} has no provider in its lane config"])
        out.setdefault(lane, []).append(
            Candidate(
                nickname=row["nickname"],
                provider=provider,
                tier=ModelTier(row["tier"]),
                thinking=row["thinking"] == "yes",
            )
        )
    return out


def load_cases(cases_dir: str) -> Tuple[Case, ...]:
    names = sorted(n for n in os.listdir(cases_dir) if n.endswith(".json"))
    cases: List[Case] = []
    for name in names:
        try:
            parsed = validate_case.validate(os.path.join(cases_dir, name))
        except validate_case.CaseError as exc:
            raise MatrixError([f"{os.path.join(cases_dir, name)}: {exc}"]) from exc
        cases.append(Case(parsed["case_id"], parsed["wire_pattern"], parsed["knobs"]["tools"]))
    if not cases:
        raise MatrixError([f"{cases_dir}: no case files"])
    # By case id, not file name: `plain-turn-01-fp.json` sorts before
    # `plain-turn-01.json`, and the sweep set takes the first match.
    return tuple(sorted(cases, key=lambda c: c.case_id))


def sweep_cases(cases: Sequence[Case], thinking: bool) -> Tuple[str, ...]:
    """The sweep set for one candidate, in a fixed order."""

    def first(pattern: str, tools: Optional[bool] = None) -> str:
        for case in cases:
            if case.wire_pattern == pattern and (tools is None or case.tools == tools):
                return case.case_id
        raise MatrixError([f"no {pattern} case for the sweep set"])

    picked = [first(SWEEP_BASELINE), first(SWEEP_TOOL)]
    if thinking:
        picked.append(first(SWEEP_THINKING, tools=False))
    return tuple(picked)


def _lane_default(paths: Paths, lane: str) -> str:
    tables = lane_configs.read_config(os.path.join(paths.config_dir, lane + ".toml"))
    default = lane_configs.string_value(tables.get("aliases", {}).get("default", ""))
    if not default or not TOKEN_RE.fullmatch(default):
        raise MatrixError([f"lane {lane}: [aliases] default is missing or not a plain nickname"])
    return default


def _models_for(paths: Paths, row: CellRow, candidates: Optional[List[Candidate]]) -> List[Tuple[str, ModelTier, Optional[bool]]]:
    """(nickname, tier, thinking) for one row; thinking None = every case."""
    if candidates is None:
        return [(_lane_default(paths, row.lane), ModelTier.LANE_DEFAULT, None)]
    picked: List[Tuple[str, ModelTier, Optional[bool]]] = [
        (c.nickname, c.tier, None) for c in candidates if c.tier is ModelTier.PRIMARY
    ]
    picked.extend(
        (c.nickname, c.tier, c.thinking)
        for c in candidates
        if c.tier is ModelTier.SWEEP and c.provider in row.sweep
    )
    return picked


def cell_id(row: CellRow, model: str, case: str) -> str:
    return ".".join((row.driver[: -len(".sh")], row.lane, row.connection_mode, model, case))


def expand(paths: Paths, rows: Sequence[CellRow]) -> Tuple[CellSpec, ...]:
    cases = load_cases(paths.cases_dir)
    every_case = tuple(c.case_id for c in cases)
    by_lane = _candidates_by_lane(paths)
    specs: List[CellSpec] = []
    for row in rows:
        for model, tier, thinking in _models_for(paths, row, by_lane.get(row.lane)):
            if not TOKEN_RE.fullmatch(model):
                raise MatrixError([f"model nickname {model!r} is not a safe token"])
            chosen = every_case if thinking is None else sweep_cases(cases, thinking)
            specs.extend(
                CellSpec(
                    id=cell_id(row, model, case),
                    driver=row.driver,
                    expected_ingress=row.expected_ingress,
                    lane=row.lane,
                    connection_mode=row.connection_mode,
                    providers=row.providers,
                    key_file=row.key_file,
                    model=model,
                    tier=tier,
                    case=case,
                )
                for case in chosen
            )
    return tuple(specs)


def _descriptor_problem(fd: int) -> Optional[str]:
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode):
        return "not-a-file"
    if info.st_uid != os.geteuid():
        return "wrong-owner"
    if info.st_mode & INSECURE_MODE_BITS:
        return "insecure-mode"
    if info.st_size == 0:
        return "empty"
    return None


_CLOEXEC = getattr(os, "O_CLOEXEC", 0)
# O_NONBLOCK: opening a FIFO planted under the key's name would otherwise
# block until a writer appears; it is then refused as not-a-file. Reads
# from a regular file ignore the flag.
_KEY_FLAGS = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_NOCTTY | _CLOEXEC
# Group/other write on the key directory would let another user replace a
# key between the check and the read.
KEY_DIR_WRITE_BITS = 0o022


_KEY_DIR_REFUSALS = {
    nofollow.BAD_PATH: "bad-dir-path",
    nofollow.MISSING: "missing",
    nofollow.SYMLINK: "symlink",
    nofollow.NOT_A_DIRECTORY: "unreadable",
    nofollow.UNREADABLE: "unreadable",
}


def _key_dir_problem(dir_fd: int) -> Optional[str]:
    info = os.fstat(dir_fd)
    if info.st_uid != os.geteuid():
        return "wrong-owner"
    if info.st_mode & KEY_DIR_WRITE_BITS:
        return "bad-dir-mode"
    return None


def _open_key_in(dir_fd: int, name: str) -> Tuple[Optional[int], Optional[str]]:
    try:
        fd = os.open(name, _KEY_FLAGS, dir_fd=dir_fd)
    except FileNotFoundError:
        return None, "missing"
    except OSError as exc:
        return None, "symlink" if exc.errno == errno.ELOOP else "unreadable"
    try:
        problem = _descriptor_problem(fd)
    except OSError:
        problem = "unreadable"
    if problem is not None:
        os.close(fd)
        return None, problem
    return fd, None


def open_key_file(key_dir: str, name: str) -> Tuple[Optional[int], Optional[str]]:
    """(fd, None) when the key file is usable, else (None, detail token).

    The caller owns the returned descriptor and must close it; on every
    other path nothing is left open. Details: bad-name, bad-dir-path,
    missing, symlink, unreadable, not-a-file, wrong-owner, bad-dir-mode,
    insecure-mode, empty. The key directory is judged first, so a detail other than
    insecure-mode, empty or not-a-file may concern it rather than the key.
    Never reads the file's content."""
    if not KEY_FILE_RE.fullmatch(name):
        return None, "bad-name"
    if not key_dir:
        return None, "bad-dir-path"
    try:
        # nofollow.open_dir refuses a final `.` or `..`. abspath resolves
        # them lexically against the physical working directory (getcwd
        # never returns a symlink), so `.` is the real working directory and
        # `link/.` ends in `link`, which open_dir then refuses as a symlink.
        absolute = os.path.abspath(key_dir)
    except OSError:
        return None, "unreadable"
    try:
        dir_fd = nofollow.open_dir(absolute)
    except nofollow.DirRefused as refused:
        return None, _KEY_DIR_REFUSALS[refused.detail]
    try:
        try:
            problem = _key_dir_problem(dir_fd)
        except OSError:
            problem = "unreadable"
        if problem is not None:
            return None, problem
        return _open_key_in(dir_fd, name)
    finally:
        os.close(dir_fd)


def key_file_problem(key_dir: str, name: str) -> Optional[str]:
    """None when the key file is usable; else the NOT_RUN(credential) detail.
    Never reads the file's content."""
    fd, problem = open_key_file(key_dir, name)
    if fd is not None:
        os.close(fd)
    return problem


def _driver_missing(drivers_dir: str, driver: str) -> bool:
    path = os.path.join(drivers_dir, driver)
    return not (os.path.isfile(path) and os.access(path, os.X_OK))


def preflight(ledger: RunLedger, paths: Paths, key_dir: str) -> RunLedger:
    """Mark every cell whose driver or key file cannot serve it NOT_RUN."""
    driver_missing: Dict[str, bool] = {}
    key_problem: Dict[str, Optional[str]] = {}
    out = ledger
    for entry in ledger.entries:
        spec = entry.spec
        if spec.driver not in driver_missing:
            driver_missing[spec.driver] = _driver_missing(paths.drivers_dir, spec.driver)
        if driver_missing[spec.driver]:
            out = out.transition(spec.id, CellState.NOT_RUN, reason=NotRunReason.DRIVER.value, detail="missing")
            continue
        if spec.key_file is None:
            continue
        if spec.key_file not in key_problem:
            key_problem[spec.key_file] = key_file_problem(key_dir, spec.key_file)
        problem = key_problem[spec.key_file]
        if problem is not None:
            out = out.transition(spec.id, CellState.NOT_RUN, reason=NotRunReason.CREDENTIAL.value, detail=problem)
    return out


def _sha256(path: str) -> str:
    with open(path, "rb") as handle:
        return hashlib.sha256(handle.read()).hexdigest()


def run_inputs(paths: Paths) -> RunInputs:
    return RunInputs(
        cells_sha256=_sha256(paths.cells_tsv),
        models_sha256=_sha256(paths.models_tsv),
        cases=tuple(c.case_id for c in load_cases(paths.cases_dir)),
    )


def build_ledger(paths: Paths, key_dir: str) -> RunLedger:
    rows = load_rows(paths)
    ledger = RunLedger.new(run_inputs(paths), expand(paths, rows))
    return preflight(ledger, paths, key_dir)


def _parse_cli(argv: List[str]) -> Tuple[str, Dict[str, str]]:
    if not argv or argv[0] not in ("validate", "expand"):
        raise ValueError("usage: see the module docstring")
    opts: Dict[str, str] = {}
    rest = argv[1:]
    while rest:
        if rest[0] not in ("--root", "--key-dir") or len(rest) < 2:
            raise ValueError(f"unknown or incomplete option {rest[0]!r}")
        opts[rest[0][2:]] = rest[1]
        rest = rest[2:]
    if argv[0] == "expand" and "key-dir" not in opts:
        raise ValueError("expand requires --key-dir")
    return argv[0], opts


def main(argv: List[str]) -> int:
    try:
        command, opts = _parse_cli(argv)
    except ValueError as exc:
        print(f"matrix: {exc}", file=sys.stderr)
        return 2
    paths = Paths.from_root(opts.get("root", os.path.join(HERE, "..", "..")))
    try:
        if command == "validate":
            load_rows(paths)
        else:
            sys.stdout.write(build_ledger(paths, opts["key-dir"]).to_json())
    except MatrixError as exc:
        for failure in exc.failures:
            print(f"FAIL: {failure}")
        return 1
    except (OSError, lane_configs.ParseError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
