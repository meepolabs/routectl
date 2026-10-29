"""Run directories for the report tests, built only through real writers.

run.json goes through ledger.RunLedger (new / transition / write_atomic),
never by hand. Each cell's .cell-evidence/ is written by
scripts/drivers/lib/cell_evidence.py `finalize`, the entry point
capture_driver.sh calls, over a synthetic trace. health.json is either one
of the snapshots RECORDED from a real hermetic daemon (testdata/) copied
byte for byte, or that recording with rows edited; usage.json has the
daemon's usage shape (welded in test_evidence).
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from dataclasses import dataclass, field, replace
from typing import Any, Dict, List, Optional, Sequence, Tuple

HERE = os.path.dirname(os.path.abspath(__file__))
WIRE_WATCH = os.path.dirname(HERE)
REPO = os.path.abspath(os.path.join(WIRE_WATCH, "..", ".."))
WRITER = os.path.join(REPO, "scripts", "drivers", "lib", "cell_evidence.py")
TESTDATA = os.path.join(WIRE_WATCH, "testdata")
sys.path.insert(0, WIRE_WATCH)

import evidence  # noqa: E402
import ledger  # noqa: E402
import matrix  # noqa: E402
from ledger import CapReason, CellSpec, CellState, ModelTier, RunLedger, RunStop, Tokens  # noqa: E402

PATHS = matrix.Paths.from_root(REPO)
CASE = "plain-turn-01"


def styled(target: str) -> str:
    return f"\x1b[2m{target}\x1b[0m\x1b[2m:\x1b[0m"


def ingress_line(token: str) -> str:
    return (
        "2026-10-12T17:52:46.100000Z TRACE request{method=POST path=/v1/x}: "
        + styled("routectl_core::log_safe")
        + f' ingress request body ingress="{token}" body={{}} redact_prompts_enabled=false\n'
    )


def fidelity_line(learned: int = 0, unlocalized: int = 0) -> str:
    return (
        "2026-10-12T17:52:46.900000Z  INFO request{method=GET path=/status/health}: "
        + styled("routectl_cli::handlers::status::field_verdict_log")
        + f" envelope field verdict snapshot rc_field_verdicts_learned_total={learned}"
        + f" rc_parser_unlocalized_total={unlocalized}\n"
    )


def error_line(provider_kind: str, status: int, body: str) -> str:
    return (
        "2026-10-12T17:52:46.500000Z DEBUG request{}: "
        + styled("routectl_core::log_safe")
        + f' upstream error body provider_kind="{provider_kind}" provider="p" status={status} body={body}\n'
    )


def recorded(name: str) -> str:
    with open(os.path.join(TESTDATA, f"{name}.health.json"), encoding="utf-8") as handle:
        return handle.read()


def recorded_doc(name: str) -> dict:
    return json.loads(recorded(name))


def usage_text() -> str:
    totals = {
        "requests": 1, "ok": 1, "errors": 0, "input_tokens": 3, "output_tokens": 1,
        "reasoning_tokens": 0, "cache_read_billed": 0, "server_tool_calls": 0,
        "errors_by_class": {}, "client_disconnect_total": 0, "cache_read_present": 0,
    }
    return json.dumps({
        "schema_version": evidence.USAGE_SCHEMA_VERSION,
        "as_of": "2026-10-12T17:52:46Z",
        "data": {"window": "all", "totals": totals},
        "unavailable": None,
    })


@dataclass
class CellPlan:
    """One cell: its ledger spec and what its evidence directory holds."""

    driver: str
    ingress: str
    lane: str
    model: str = "m"
    state: CellState = CellState.EXITED
    rc: Optional[int] = 5
    reason: Optional[str] = None
    detail: Optional[str] = None
    key_file: Optional[str] = None
    # None = no evidence directory at all.
    health: Optional[str] = None
    health_status: str = "ok"
    trace_ingress: Optional[str] = "same"
    errors: Tuple[str, ...] = ()
    learned: int = 0
    unlocalized: int = 0
    driver_rc: int = 0
    legs: Tuple[str, ...] = ()
    fixture_lane: str = "lane"
    tokens: Optional[Tokens] = None
    # Written to cells/<cell-id>.runner.log beside the cell dir, as run.py does.
    runner_log: Optional[str] = None
    extra_files: Dict[str, str] = field(default_factory=dict)

    @property
    def cell_id(self) -> str:
        return matrix.cell_id(_row(self), self.model, CASE)

    def spec(self) -> CellSpec:
        return CellSpec(
            id=self.cell_id, driver=self.driver, expected_ingress=self.ingress, lane=self.lane,
            connection_mode="base-url", providers=("p",), key_file=self.key_file,
            model=self.model, tier=ModelTier.LANE_DEFAULT, case=CASE,
        )


def _row(plan: CellPlan) -> matrix.CellRow:
    return matrix.CellRow(line=0, driver=plan.driver, expected_ingress=plan.ingress, lane=plan.lane,
                          connection_mode="base-url", providers=("p",), key_file=None, sweep=())


def _trace(plan: CellPlan) -> str:
    token = plan.ingress if plan.trace_ingress == "same" else plan.trace_ingress
    out = ingress_line(token) if token else ""
    out += "".join(plan.errors)
    return out + fidelity_line(plan.learned, plan.unlocalized)


def write_evidence(cell_dir: str, plan: CellPlan) -> None:
    ev = os.path.join(cell_dir, evidence.EVIDENCE_DIRNAME)
    os.makedirs(ev)
    if plan.health_status == "ok":
        with open(os.path.join(ev, "health.json"), "w", encoding="utf-8") as handle:
            handle.write(plan.health or recorded("clean-control"))
    with open(os.path.join(ev, "usage.json"), "w", encoding="utf-8") as handle:
        handle.write(usage_text())
    trace = os.path.join(cell_dir, "trace.log")
    with open(trace, "w", encoding="utf-8") as handle:
        handle.write(_trace(plan))
    argv = [
        sys.executable, "-I", WRITER, "finalize", "--dir", ev, "--trace", trace,
        "--snapshot", f"health={plan.health_status}", "--snapshot", "usage=ok",
        "--exit", str(plan.rc if plan.rc is not None else 0), "--driver-rc", str(plan.driver_rc),
        "--rig-rc", "1", "--lane", plan.lane, "--case", CASE, "--expected-ingress", plan.ingress,
        "--connection-mode", "base-url", "--turns", "1",
    ]
    subprocess.run(argv, check=True, capture_output=True, env={"PATH": os.environ.get("PATH", "")})
    os.remove(trace)
    for name, text in plan.extra_files.items():
        with open(os.path.join(ev, name), "w", encoding="utf-8") as handle:
            handle.write(text)


def write_legs(cell_dir: str, plan: CellPlan) -> None:
    if not plan.legs:
        return
    case_dir = os.path.join(cell_dir, plan.fixture_lane, CASE)
    os.makedirs(case_dir)
    for name in plan.legs:
        with open(os.path.join(case_dir, name), "w", encoding="utf-8") as handle:
            handle.write("{}\n")


def _advance(led: RunLedger, plan: CellPlan) -> RunLedger:
    cid = plan.cell_id
    if plan.state is CellState.PENDING:
        return led
    if plan.state is CellState.NOT_RUN or (plan.state is CellState.CAP_REACHED and plan.reason != CapReason.SIZE.value):
        return led.transition(cid, plan.state, reason=plan.reason, detail=plan.detail)
    led = led.transition(cid, CellState.RUNNING)
    if plan.state is CellState.RUNNING:
        return led
    if plan.state is CellState.EXITED:
        led = led.transition(cid, CellState.EXITED, rc=plan.rc, detail=plan.detail)
        return led if plan.tokens is None else led.with_tokens(cid, plan.tokens)
    return led.transition(cid, plan.state, reason=plan.reason, detail=plan.detail)


def build_run(
    run_dir: str,
    plans: Sequence[CellPlan],
    inputs: Optional[ledger.RunInputs] = None,
    run_stop: Optional[RunStop] = None,
) -> RunLedger:
    led = RunLedger.new(inputs or matrix.run_inputs(PATHS), [p.spec() for p in plans])
    if run_stop is not None:
        led = led.with_run_stop(run_stop)
    for plan in plans:
        led = _advance(led, plan)
        cell_dir = os.path.join(run_dir, "cells", plan.cell_id)
        os.makedirs(cell_dir)
        if plan.state is CellState.EXITED and (plan.health is not None or plan.health_status != "ok" or plan.errors):
            write_evidence(cell_dir, plan)
        write_legs(cell_dir, plan)
        if plan.runner_log is not None:
            with open(cell_dir + ".runner.log", "w", encoding="utf-8") as handle:
                handle.write(plan.runner_log)
    ledger.write_atomic(os.path.join(run_dir, "run.json"), led)
    return led


def plant(name: str, **kw: Any) -> CellPlan:
    """A cell carrying one of the recorded planted-drop snapshots."""
    responses = name in ("request-leg-plant", "response-leg-plant")
    base = CellPlan(
        driver="codex.sh" if responses else "hermes-chat.sh",
        ingress="openai-responses" if responses else "openai",
        lane="openai-responses" if responses else "openai-compat.ollama",
        model=name,
        health=recorded(name),
    )
    return replace(base, **kw)


def edited_health(name: str, edit: "callable") -> str:  # type: ignore[valid-type]
    doc = recorded_doc(name)
    edit(doc["data"])
    return json.dumps(doc)


def read_jsonl(path: str) -> List[dict]:
    with open(path, encoding="ascii") as handle:
        return [json.loads(line) for line in handle]
