"""ledger.py: the cell state machine and the atomic run.json."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
WIRE_WATCH = os.path.dirname(HERE)
sys.path.insert(0, WIRE_WATCH)

import ledger  # noqa: E402
from ledger import CellSpec, CellState, LedgerError, ModelTier, Outcome, RunInputs, RunLedger  # noqa: E402

INPUTS = RunInputs(cells_sha256="a" * 64, models_sha256="b" * 64, cases=("plain-turn-01",))


def spec(cell_id: str = "codex.openai-responses.base-url.fixture-codex.plain-turn-01") -> CellSpec:
    return CellSpec(
        id=cell_id,
        driver="codex.sh",
        expected_ingress="openai-responses",
        lane="openai-responses",
        connection_mode="base-url",
        providers=("openai",),
        key_file=None,
        model="fixture-codex",
        tier=ModelTier.LANE_DEFAULT,
        case="plain-turn-01",
    )


def one_cell() -> RunLedger:
    return RunLedger.new(INPUTS, [spec()])


CELL = spec().id

# (path of transitions from PENDING, final kwargs) for every reachable state.
REACH = {
    CellState.PENDING: [],
    CellState.RUNNING: [(CellState.RUNNING, {})],
    CellState.EXITED: [(CellState.RUNNING, {}), (CellState.EXITED, {"rc": 0})],
    CellState.TIMEOUT: [(CellState.RUNNING, {}), (CellState.TIMEOUT, {})],
    CellState.CAP_REACHED: [(CellState.CAP_REACHED, {"reason": "deadline"})],
    CellState.INTERRUPTED: [(CellState.INTERRUPTED, {})],
    CellState.NOT_RUN: [(CellState.NOT_RUN, {"reason": "driver"})],
}
PAYLOAD = {
    CellState.EXITED: {"rc": 1},
    CellState.CAP_REACHED: {"reason": "size"},
    CellState.NOT_RUN: {"reason": "credential"},
}


def reach(state: CellState) -> RunLedger:
    led = one_cell()
    for to, kwargs in REACH[state]:
        led = led.transition(CELL, to, **kwargs)
    return led


class TransitionTests(unittest.TestCase):
    def test_every_pair_is_legal_exactly_when_the_table_says_so(self) -> None:
        for source in CellState:
            for target in CellState:
                legal = target in ledger.ALLOWED.get(source, frozenset())
                with self.subTest(f"{source.value} -> {target.value}"):
                    led = reach(source)
                    kwargs = PAYLOAD.get(target, {})
                    if legal:
                        self.assertIs(led.transition(CELL, target, **kwargs).entry(CELL).state, target)
                    else:
                        with self.assertRaises(LedgerError):
                            led.transition(CELL, target, **kwargs)

    def test_terminal_states_have_no_exit(self) -> None:
        for state in ledger.TERMINAL:
            self.assertNotIn(state, ledger.ALLOWED)

    def test_transition_returns_a_new_ledger(self) -> None:
        before = one_cell()
        after = before.transition(CELL, CellState.RUNNING)
        self.assertIs(before.entry(CELL).state, CellState.PENDING)
        self.assertIs(after.entry(CELL).state, CellState.RUNNING)


class PayloadTests(unittest.TestCase):
    def test_payload_rules(self) -> None:
        rows = [
            ("exited without rc", CellState.RUNNING, CellState.EXITED, {}),
            ("exited with bool rc", CellState.RUNNING, CellState.EXITED, {"rc": True}),
            ("timeout with rc", CellState.RUNNING, CellState.TIMEOUT, {"rc": 1}),
            ("not-run without reason", CellState.PENDING, CellState.NOT_RUN, {}),
            ("not-run unknown reason", CellState.PENDING, CellState.NOT_RUN, {"reason": "flaky"}),
            ("cap without reason", CellState.PENDING, CellState.CAP_REACHED, {}),
            ("cap with a not-run reason", CellState.PENDING, CellState.CAP_REACHED, {"reason": "driver"}),
            ("interrupted with reason", CellState.PENDING, CellState.INTERRUPTED, {"reason": "deadline"}),
            ("free-text detail", CellState.PENDING, CellState.NOT_RUN, {"reason": "credential", "detail": "key sk-123 bad"}),
            ("over-long detail", CellState.PENDING, CellState.NOT_RUN, {"reason": "credential", "detail": "a" * 65}),
        ]
        for name, source, target, kwargs in rows:
            with self.subTest(name):
                with self.assertRaises(LedgerError):
                    reach(source).transition(CELL, target, **kwargs)

    def test_not_run_is_never_pass(self) -> None:
        for reason in ledger.NotRunReason:
            led = one_cell().transition(CELL, CellState.NOT_RUN, reason=reason.value)
            self.assertIs(ledger.terminal_outcome(led.entry(CELL)), Outcome.NOT_RUN)

    def test_exited_needs_evidence_for_its_outcome(self) -> None:
        self.assertIsNone(ledger.terminal_outcome(reach(CellState.EXITED).entry(CELL)))


class CellIdTests(unittest.TestCase):
    def test_unsafe_ids_are_refused(self) -> None:
        for bad in ("../x", "a/b", ".hidden", "A-upper", "a b", "", "a..b", "x" * 201, "trailing-"):
            with self.subTest(bad):
                with self.assertRaises(LedgerError):
                    RunLedger.new(INPUTS, [spec(bad)])

    def test_duplicate_ids_are_refused(self) -> None:
        with self.assertRaises(LedgerError):
            RunLedger.new(INPUTS, [spec(), spec()])


class SerializationTests(unittest.TestCase):
    def test_round_trip(self) -> None:
        led = reach(CellState.EXITED)
        self.assertEqual(RunLedger.from_json(led.to_json()), led)

    def test_bytes_depend_only_on_content(self) -> None:
        self.assertEqual(reach(CellState.TIMEOUT).to_json(), reach(CellState.TIMEOUT).to_json())

    def test_a_file_carrying_an_illegal_payload_is_refused(self) -> None:
        doc = json.loads(reach(CellState.EXITED).to_json())
        doc["cells"][0]["rc"] = None
        with self.assertRaises(LedgerError):
            RunLedger.from_json(json.dumps(doc))

    def test_unknown_schema_is_refused(self) -> None:
        doc = json.loads(one_cell().to_json())
        doc["schema"] = "something-else/1"
        with self.assertRaises(LedgerError):
            RunLedger.from_json(json.dumps(doc))

    def test_truncated_file_is_refused(self) -> None:
        with self.assertRaises(LedgerError):
            RunLedger.from_json(one_cell().to_json()[:40])


class AtomicWriteTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self._tmp.name, "run.json")
        ledger.write_atomic(self.path, one_cell())
        with open(self.path, "rb") as handle:
            self.before = handle.read()

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_a_write_replaces_the_file(self) -> None:
        ledger.write_atomic(self.path, reach(CellState.RUNNING))
        self.assertIs(ledger.load(self.path).entry(CELL).state, CellState.RUNNING)

    def test_failure_before_rename_leaves_the_previous_file(self) -> None:
        with mock.patch.object(ledger.os, "replace", side_effect=OSError("injected")):
            with self.assertRaises(OSError):
                ledger.write_atomic(self.path, reach(CellState.RUNNING))
        with open(self.path, "rb") as handle:
            self.assertEqual(handle.read(), self.before)

    def test_process_death_between_temp_write_and_rename_leaves_the_previous_file(self) -> None:
        child = (
            "import os, sys\n"
            "sys.path.insert(0, sys.argv[1])\n"
            "import ledger\n"
            "def die(*_a, **_k):\n"
            "    os._exit(9)\n"
            "ledger.os.replace = die\n"
            "led = ledger.load(sys.argv[2])\n"
            "cell = led.entries[0].spec.id\n"
            "ledger.write_atomic(sys.argv[2], led.transition(cell, ledger.CellState.RUNNING))\n"
        )
        proc = subprocess.run([sys.executable, "-I", "-c", child, WIRE_WATCH, self.path])

        self.assertEqual(proc.returncode, 9)
        tmp = os.path.join(self._tmp.name, ".run.json.tmp")
        self.assertTrue(os.path.exists(tmp), "the temp file was never written, so nothing was injected")
        self.assertIs(ledger.load(tmp).entry(CELL).state, CellState.RUNNING)
        with open(self.path, "rb") as handle:
            self.assertEqual(handle.read(), self.before)
        self.assertIs(ledger.load(self.path).entry(CELL).state, CellState.PENDING)


if __name__ == "__main__":
    unittest.main()
