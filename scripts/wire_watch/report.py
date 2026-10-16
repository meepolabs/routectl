#!/usr/bin/env python3
"""Turn a wire-watch run into report.md and findings.jsonl.

Stdlib only. Reads the run dir run.py wrote -- <report-dir>/run.json
(through ledger) and each cell's <report-dir>/cells/<cell-id>/ (evidence,
landed fixture legs) -- gives every cell exactly one final outcome
(classify.py), and publishes a new dated directory
<report-dir>/report-<UTC stamp>/ holding:

  report.md       header COMPLETE or INCOMPLETE with every reason; the
                  harness x ingress coverage table from cells.tsv; the
                  findings table; one row per cell with its outcome,
                  tokens and which of the four wire legs were captured
  findings.jsonl  one JSON object per (lane, path_class, leg): count,
                  up to 10 example paths, the cell ids; ASCII-only
                  (json.dumps ensure_ascii), one object per line. The
                  losses a full finding ring dropped count under leg
                  `unknown` with no example path, so the counts per
                  (lane, path_class) equal the daemon's loss counters

cells/<cell-id>.runner.log is never opened: it is the runner's raw
output and may hold the cell's key. Only the ledger and the files under
cells/<cell-id>/ are read.

Every container-sourced string reaches report.md through mdtext.esc, and
mdtext.lines_text refuses a line that did not come from mdtext, so the
document has one canonical shape. findings.jsonl values are the raw
strings, JSON-escaped; a reader that renders them as Markdown must pass
them through mdtext.esc itself.

The run is COMPLETE only when run.json and cells.tsv load, run.json was
expanded from the cells.tsv on disk, run.json records no run_stop (shown
as the reason), and no cell is INCOMPLETE, TIMEOUT, CAP_REACHED,
INTERRUPTED or non-terminal. FAILED and NOT_RUN cells do not
make a run incomplete; they leave their pairing uncovered. A pairing is
covered only by a PASS or FINDING cell on at least one egress.

The report is staged under a temp name and checked with
scripts/scrub-fixture.sh --check, together with a scratch copy of every
container-sourced string it shows in raw form (escaping can split a
key-shaped string past the check's patterns); the scratch copy is removed
and only a clean check renames the report into place, so a reader never
sees a half-made report or one carrying a key-shaped string.

Usage
  report.py --report-dir R [--key-dir K] [--root REPO]
    --key-dir lets a Bedrock 403 be told apart as an expired short-term
    key (refusal.py); without it a Bedrock 403 is NOT_RUN(model-access).
    A --key-dir the key reader would refuse (matrix.key_dir_problem) is a
    usage error.

Exit: 0 COMPLETE, 1 INCOMPLETE (report published), 2 usage, 3 the report
could not be published (stdout then carries nothing).
"""

from __future__ import annotations

import json
import logging
import os
import subprocess
import sys
import tempfile
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Dict, List, Optional, Sequence, Tuple

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import classify  # noqa: E402
import ledger  # noqa: E402
import matrix  # noqa: E402
import mdtext  # noqa: E402
import refusal  # noqa: E402
from classify import CellResult, Context  # noqa: E402
from ledger import Outcome, RunLedger  # noqa: E402
from mdtext import Md, cat, esc, join, lit, num  # noqa: E402

LOG = logging.getLogger("wire_watch.report")

RUN_JSON = "run.json"
# The per-cell directory run.py gives each cell: <run-dir>/cells/<cell-id>.
CELLS_DIR = "cells"
REPORT_MD = "report.md"
FINDINGS_JSONL = "findings.jsonl"
RAW_TEXTS = ".container-texts.jsonl"
RULES_TSV = os.path.join(HERE, "refusal_rules.tsv")
SCRUB = os.path.join(HERE, "..", "scrub-fixture.sh")
SCRUB_TIMEOUT_SECONDS = 60
REPORT_PREFIX = "report-"
EXAMPLE_PATHS_MAX = 10
EXCERPT_CAP = 160
INCOMPLETE_OUTCOMES = frozenset({Outcome.INCOMPLETE, Outcome.TIMEOUT, Outcome.CAP_REACHED, Outcome.INTERRUPTED})
COVERING = frozenset({Outcome.PASS, Outcome.FINDING})
LEG_NAMES = ("client request", "upstream request", "upstream response", "client response")
EXIT_COMPLETE, EXIT_INCOMPLETE, EXIT_USAGE, EXIT_PUBLISH = 0, 1, 2, 3


@dataclass(frozen=True)
class Run:
    led: Optional[RunLedger]
    rows: Tuple[matrix.CellRow, ...]
    problems: Tuple[Md, ...]


@dataclass(frozen=True)
class Pairing:
    harness: str
    ingress: str
    covered: bool
    egresses: Tuple[str, ...]
    outcomes: Tuple[Tuple[str, int], ...]


@dataclass(frozen=True)
class FindingRow:
    lane: str
    path_class: str
    leg: str
    count: int
    example_paths: Tuple[str, ...]
    cells: Tuple[str, ...]


# --- inputs ----------------------------------------------------------------

def load_run(run_dir: str, paths: matrix.Paths) -> Run:
    problems: List[Md] = []
    led: Optional[RunLedger] = None
    rows: Tuple[matrix.CellRow, ...] = ()
    try:
        led = ledger.load(os.path.join(run_dir, RUN_JSON))
    except (OSError, UnicodeDecodeError) as exc:
        problems.append(cat(lit("`run.json` unreadable: "), esc(exc.__class__.__name__)))
    except ledger.LedgerError as exc:
        problems.append(cat(lit("`run.json` refused: "), esc(str(exc))))
    try:
        rows = matrix.load_rows(paths)
    except matrix.MatrixError as exc:
        problems.append(cat(lit("`cells.tsv` refused: "), num(len(exc.failures)), lit(" bad row(s)")))
    except (OSError, matrix.lane_configs.ParseError) as exc:
        problems.append(cat(lit("`cells.tsv` unreadable: "), esc(exc.__class__.__name__)))
    if led is not None and rows:
        try:
            current: Optional[str] = matrix.run_inputs(paths).cells_sha256
        except (OSError, matrix.MatrixError):
            current = None
        if current != led.inputs.cells_sha256:
            problems.append(lit("`run.json` was expanded from a different `cells.tsv` than the one on disk"))
    return Run(led=led, rows=rows, problems=tuple(problems))


class KeyDirRefused(Exception):
    """--key-dir is a directory the key reader refuses; carries its detail."""

    def __init__(self, detail: str) -> None:
        super().__init__(detail)
        self.detail = detail


def context(paths: matrix.Paths, key_dir: Optional[str]) -> Context:
    lanes = frozenset(matrix.bedrock_lanes(paths))
    rules = refusal.load_rules(RULES_TSV)
    if key_dir is None:
        return Context(rules=rules, bedrock_lanes=lanes)
    problem = matrix.key_dir_problem(key_dir)
    if problem is not None:
        raise KeyDirRefused(problem)
    return Context(rules=rules, bedrock_lanes=lanes, key_dir=key_dir, key_expiry=refusal.key_expiry)


def classify_run(led: RunLedger, run_dir: str, ctx: Context) -> Tuple[CellResult, ...]:
    cells = os.path.join(run_dir, CELLS_DIR)
    return tuple(classify.classify(e, os.path.join(cells, e.spec.id), ctx) for e in led.entries)


# --- aggregation -----------------------------------------------------------

def run_problems(run: Run, results: Sequence[CellResult]) -> List[Md]:
    why = list(run.problems)
    if run.led is not None and not run.led.entries:
        why.append(lit("`run.json` holds no cells"))
    if run.led is not None and run.led.run_stop is not None:
        why.append(cat(lit("run stopped: "), lit(run.led.run_stop.value)))
    counts = Counter(r.outcome for r in results if r.outcome in INCOMPLETE_OUTCOMES)
    for outcome in sorted(counts, key=lambda o: o.value):
        why.append(cat(num(counts[outcome]), lit(" cell(s) "), lit(outcome.value)))
    return why


def _harness(driver: str) -> str:
    return driver[: -len(".sh")] if driver.endswith(".sh") else driver


def coverage(rows: Sequence[matrix.CellRow], results: Sequence[CellResult]) -> List[Pairing]:
    pairs = sorted({(r.driver, r.expected_ingress) for r in rows})
    out: List[Pairing] = []
    for driver, ingress in pairs:
        mine = [r for r in results if r.entry.spec.driver == driver and r.entry.spec.expected_ingress == ingress]
        covering = sorted({r.entry.spec.lane for r in mine if r.outcome in COVERING})
        tally = Counter(r.outcome.value for r in mine)
        out.append(Pairing(_harness(driver), ingress, bool(covering), tuple(covering), tuple(sorted(tally.items()))))
    return out


def finding_rows(results: Sequence[CellResult]) -> List[FindingRow]:
    counts: Dict[Tuple[str, str, str], int] = Counter()
    paths: Dict[Tuple[str, str, str], set] = defaultdict(set)
    cells: Dict[Tuple[str, str, str], set] = defaultdict(set)
    for result in results:
        if result.outcome is not Outcome.FINDING:
            continue
        for f in result.findings:
            key = (f.lane, f.path_class, f.leg)
            counts[key] += f.count
            if f.path is not None:
                paths[key].add(f.path)
            cells[key].add(result.entry.spec.id)
    return [
        FindingRow(lane, cls, leg, counts[(lane, cls, leg)],
                   tuple(sorted(paths[(lane, cls, leg)])[:EXAMPLE_PATHS_MAX]),
                   tuple(sorted(cells[(lane, cls, leg)])))
        for lane, cls, leg in sorted(counts)
    ]


# --- report.md -------------------------------------------------------------

def _row(cells: Sequence[Md]) -> Md:
    return cat(lit("| "), join(cells, lit(" | ")), lit(" |"))


def _rule(width: int) -> Md:
    return _row([lit("---")] * width)


def _header(complete: bool, why: Sequence[Md], generated: str) -> List[Md]:
    lines = [lit("# Wire watch report"), lit(""), cat(lit("Generated: "), lit(generated)), lit("")]
    lines.append(lit("Status: COMPLETE") if complete else lit("Status: INCOMPLETE"))
    lines.append(lit(""))
    lines += [cat(lit("- "), reason) for reason in why]
    return lines + ([lit("")] if why else [])


def _summary(results: Sequence[CellResult]) -> List[Md]:
    tally = Counter(r.outcome.value for r in results)
    parts = [cat(lit(o.value), lit(" "), num(tally.get(o.value, 0))) for o in Outcome]
    return [cat(lit("Cells: "), num(len(results)), lit(" ("), join(parts), lit(")")), lit("")]


def _coverage_table(pairings: Sequence[Pairing]) -> List[Md]:
    lines = [lit("## Coverage: harness x ingress"), lit("")]
    lines.append(lit("A pairing is covered only by a PASS or FINDING cell on at least one egress"))
    lines += [lit(""), _row([lit(h) for h in ("Harness", "Ingress", "Covered", "Covering egresses", "Cells")]), _rule(5)]
    for p in pairings:
        egresses = join([esc(e) for e in p.egresses]) if p.egresses else lit("-")
        tally = join([cat(esc(o), lit(" "), num(n)) for o, n in p.outcomes]) if p.outcomes else lit("no cells")
        lines.append(_row([esc(p.harness), esc(p.ingress), lit("yes" if p.covered else "no"), egresses, tally]))
    return lines + [lit("")]


def _findings_table(rows: Sequence[FindingRow]) -> List[Md]:
    lines = [lit("## Findings"), lit("")]
    if not rows:
        return lines + [lit("No findings"), lit("")]
    lines += [_row([lit(h) for h in ("Lane", "Path class", "Leg", "Count", "Example paths", "Cells")]), _rule(6)]
    for f in rows:
        lines.append(_row([
            esc(f.lane), esc(f.path_class), esc(f.leg), num(f.count),
            join([esc(p) for p in f.example_paths]), join([esc(c) for c in f.cells]),
        ]))
    return lines + [lit("")]


def _tokens(result: CellResult) -> Md:
    tokens = result.entry.tokens
    if tokens is None:
        return lit("-")
    return join([num(v) for v in (tokens.input_tokens, tokens.output_tokens,
                                  tokens.reasoning_tokens, tokens.cache_read_billed)], lit(" / "))


def _legs(result: CellResult) -> Md:
    return join([lit("yes" if got else "no") for got in result.legs], lit(" / "))


def _outcome(result: CellResult) -> Md:
    if result.reason is None:
        return lit(result.outcome.value)
    return cat(lit(result.outcome.value), lit("("), esc(result.reason), lit(")"))


def _detail(result: CellResult) -> Md:
    parts: List[Md] = []
    if result.refused_by is not None:
        error = result.refused_by
        parts.append(cat(lit("upstream "), num(error.status), lit(" "), esc(error.provider_kind),
                         lit(" - "), esc(error.excerpt, EXCERPT_CAP)))
    if result.detail:
        parts.append(esc(result.detail))
    if result.outcome is Outcome.FAILED and result.reason == "ingress":
        traced = join([esc(t) for t in result.ingress_kinds]) if result.ingress_kinds else lit("none")
        parts.append(cat(lit("traced ingress: "), traced))
    return join(parts, lit("; ")) if parts else lit("-")


def _cells_table(results: Sequence[CellResult]) -> List[Md]:
    lines = [lit("## Cells"), lit("")]
    lines.append(cat(lit("Tokens are input / output / reasoning / billed cache read; legs are "),
                     join([lit(n) for n in LEG_NAMES], lit(" / ")),
                     lit("; no means that leg's body was not captured")))
    lines += [lit(""), _row([lit(h) for h in ("Cell", "Outcome", "Tokens", "Legs captured", "Detail")]), _rule(5)]
    for r in results:
        lines.append(_row([esc(r.entry.spec.id), _outcome(r), _tokens(r), _legs(r), _detail(r)]))
    return lines + [lit("")]


def render(complete: bool, why: Sequence[Md], generated: str, pairings: Sequence[Pairing],
           rows: Sequence[FindingRow], results: Sequence[CellResult]) -> str:
    lines = _header(complete, why, generated) + _summary(results)
    lines += _coverage_table(pairings) + _findings_table(rows) + _cells_table(results)
    return mdtext.lines_text(lines)


def container_texts(results: Sequence[CellResult]) -> List[str]:
    """Every container-sourced string the report shows, unescaped. The
    escaper can split a key-shaped string (`sk\\-...`) so that the scrub
    check would no longer see it in report.md; checking the raw strings
    too closes that gap."""
    texts: List[str] = []
    for r in results:
        texts += [r.reason or "", r.detail or "", *r.ingress_kinds]
        texts += [f.path or "" for f in r.findings] + [f.lane for f in r.findings]
        if r.refused_by is not None:
            texts += [r.refused_by.provider_kind, r.refused_by.excerpt]
    return [t for t in texts if t]


# --- findings.jsonl --------------------------------------------------------

def jsonl_lines(values: Sequence[object]) -> str:
    """One ASCII JSON document per line: the only JSON this module writes."""
    return "".join(json.dumps(v, ensure_ascii=True, sort_keys=True) + "\n" for v in values)


def jsonl_text(rows: Sequence[FindingRow]) -> str:
    return jsonl_lines([
        {"lane": f.lane, "path_class": f.path_class, "leg": f.leg, "count": f.count,
         "example_paths": list(f.example_paths), "cells": list(f.cells)}
        for f in rows
    ])


# --- publishing ------------------------------------------------------------

def _put(directory: str, name: str, text: str) -> None:
    fd = os.open(os.path.join(directory, name), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    try:
        view = memoryview(text.encode("ascii"))
        while view:
            view = view[os.write(fd, view):]
        os.fsync(fd)
    finally:
        os.close(fd)


class Unpublishable(Exception):
    """The staged report could not be written or failed the scrub check."""


def _scrub_clean(directory: str) -> bool:
    try:
        done = subprocess.run(["bash", SCRUB, "--check", directory], stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL, timeout=SCRUB_TIMEOUT_SECONDS, check=False)
    except (OSError, subprocess.SubprocessError):
        return False
    return done.returncode == 0


def _discard(staging: str, names: Sequence[str]) -> None:
    for name in names:
        try:
            os.unlink(os.path.join(staging, name))
        except OSError:
            pass
    try:
        os.rmdir(staging)
    except OSError:
        pass


def publish(report_dir: str, stamp: str, files: Dict[str, str], raw_texts: Sequence[str] = ()) -> str:
    """Stage `files` with the raw container texts beside them, scrub-check
    the lot, drop the raw texts, and rename the staged dir into place."""
    final = os.path.join(report_dir, REPORT_PREFIX + stamp)
    if os.path.lexists(final):
        raise Unpublishable("a report with this stamp already exists")
    try:
        staging = tempfile.mkdtemp(prefix=".report-", dir=report_dir)
    except OSError as exc:
        raise Unpublishable(f"cannot stage: {exc.__class__.__name__}") from None
    try:
        for name, text in files.items():
            _put(staging, name, text)
        _put(staging, RAW_TEXTS, jsonl_lines(raw_texts))
        clean = _scrub_clean(staging)
        os.unlink(os.path.join(staging, RAW_TEXTS))
        if not clean:
            raise Unpublishable("the scrub check refused the report")
        os.chmod(staging, 0o755)
        os.rename(staging, final)
    except OSError as exc:
        _discard(staging, [*files, RAW_TEXTS])
        raise Unpublishable(f"cannot publish: {exc.__class__.__name__}") from None
    except BaseException:
        _discard(staging, [*files, RAW_TEXTS])
        raise
    return final


# --- entry point -----------------------------------------------------------

@dataclass(frozen=True)
class Built:
    complete: bool
    files: Dict[str, str]
    raw_texts: Tuple[str, ...]
    results: Tuple[CellResult, ...]


def build(run_dir: str, paths: matrix.Paths, ctx: Context, generated: str) -> Built:
    run = load_run(run_dir, paths)
    results = classify_run(run.led, run_dir, ctx) if run.led is not None else ()
    why = run_problems(run, results)
    rows = finding_rows(results)
    text = render(not why, why, generated, coverage(run.rows, results), rows, results)
    files = {REPORT_MD: text, FINDINGS_JSONL: jsonl_text(rows)}
    return Built(not why, files, tuple(container_texts(results)), tuple(results))


def _parse(argv: Sequence[str]) -> Dict[str, str]:
    allowed = ("--report-dir", "--key-dir", "--root")
    opts: Dict[str, str] = {}
    rest = list(argv)
    while rest:
        if rest[0] not in allowed or len(rest) < 2 or rest[0][2:] in opts:
            raise ValueError(f"unknown, repeated or incomplete option {rest[0]!r}")
        opts[rest[0][2:]] = rest[1]
        rest = rest[2:]
    if "report-dir" not in opts:
        raise ValueError("--report-dir is required")
    return opts


def main(argv: Sequence[str]) -> int:
    logging.basicConfig(level=logging.INFO, stream=sys.stderr, format="%(name)s: %(message)s")
    try:
        opts = _parse(argv)
    except ValueError as exc:
        LOG.error("event=usage error=%s", exc)
        return EXIT_USAGE
    paths = matrix.Paths.from_root(opts.get("root", os.path.join(HERE, "..", "..")))
    try:
        ctx = context(paths, opts.get("key-dir"))
    except KeyDirRefused as refused:
        LOG.error("event=key-dir-refused detail=%s", refused.detail)
        return EXIT_USAGE
    except (OSError, refusal.RefusalError, matrix.lane_configs.ParseError) as exc:
        LOG.error("event=inputs-refused error=%s", exc)
        return EXIT_USAGE
    now = datetime.now(timezone.utc)
    try:
        built = build(opts["report-dir"], paths, ctx, now.strftime("%Y-%m-%dT%H:%M:%SZ"))
        final = publish(opts["report-dir"], now.strftime("%Y%m%dT%H%M%SZ"), built.files, built.raw_texts)
    except Unpublishable as exc:
        LOG.error("event=publish-refused error=%s", exc)
        return EXIT_PUBLISH
    except Exception as exc:  # noqa: BLE001 -- exit 1 means "published INCOMPLETE"; nothing was published
        LOG.error("event=report-failed error_type=%s", exc.__class__.__name__)
        return EXIT_PUBLISH
    LOG.info("event=report-published status=%s", "COMPLETE" if built.complete else "INCOMPLETE")
    print(final)
    return EXIT_COMPLETE if built.complete else EXIT_INCOMPLETE


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
