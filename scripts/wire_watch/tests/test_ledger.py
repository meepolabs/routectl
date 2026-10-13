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


def _set(path: tuple, value: object):
    def plant(doc: dict) -> None:
        node = doc
        for key in path[:-1]:
            node = node[key]
        node[path[-1]] = value
    return plant


def _drop(path: tuple):
    def plant(doc: dict) -> None:
        node = doc
        for key in path[:-1]:
            node = node[key]
        del node[path[-1]]
    return plant


# (row name, field the error must name, mutation of a valid EXITED doc)
TOKENS_DOC = {"input_tokens": 17, "output_tokens": 5, "reasoning_tokens": 0, "cache_read_billed": 3}

STRICT_ROWS = [
    ("schema absent", "schema", _drop(("schema",))),
    ("unknown top-level key", "run.json", _set(("extra",), 1)),
    ("run_stop absent", "run.json", _drop(("run_stop",))),
    ("run_stop is a number", "run_stop", _set(("run_stop",), 1)),
    ("run_stop outside its vocabulary", "run_stop", _set(("run_stop",), "deadline")),
    ("run_stop with a trailing newline", "run_stop", _set(("run_stop",), "run-size\n")),
    ("inputs is a list", "inputs", _set(("inputs",), [])),
    ("cells_sha256 is a number", "inputs.cells_sha256", _set(("inputs", "cells_sha256"), 7)),
    ("cells_sha256 is uppercase hex", "inputs.cells_sha256", _set(("inputs", "cells_sha256"), "A" * 64)),
    ("cells_sha256 is short", "inputs.cells_sha256", _set(("inputs", "cells_sha256"), "a" * 63)),
    ("models_sha256 is not hex", "inputs.models_sha256", _set(("inputs", "models_sha256"), "g" * 64)),
    ("models_sha256 is null", "inputs.models_sha256", _set(("inputs", "models_sha256"), None)),
    ("cases is a string", "inputs.cases", _set(("inputs", "cases"), "plain-turn-01")),
    ("cases holds a number", "inputs.cases", _set(("inputs", "cases"), [1])),
    ("unknown inputs key", "inputs", _set(("inputs", "extra"), "x")),
    ("cells is an object", "cells", _set(("cells",), {"a": 1})),
    ("cell is a list", "cells[0]", _set(("cells", 0), [])),
    ("cell key missing", "cells[0]", _drop(("cells", 0, "driver"))),
    ("unknown cell key", "cells[0]", _set(("cells", 0, "extra"), 1)),
    ("id is a number", "cells[0].id", _set(("cells", 0, "id"), 5)),
    ("id is not a safe token", "cells[0].id", _set(("cells", 0, "id"), "../x")),
    ("driver is a number", "cells[0].driver", _set(("cells", 0, "driver"), 1)),
    ("expected_ingress is a list", "cells[0].expected_ingress", _set(("cells", 0, "expected_ingress"), ["a"])),
    ("lane is null", "cells[0].lane", _set(("cells", 0, "lane"), None)),
    ("connection_mode is a bool", "cells[0].connection_mode", _set(("cells", 0, "connection_mode"), True)),
    ("model is an object", "cells[0].model", _set(("cells", 0, "model"), {})),
    ("case is a number", "cells[0].case", _set(("cells", 0, "case"), 3)),
    ("providers is a string", "cells[0].providers", _set(("cells", 0, "providers"), "openai")),
    ("providers is an object", "cells[0].providers", _set(("cells", 0, "providers"), {"openai": 1})),
    ("providers holds a number", "cells[0].providers", _set(("cells", 0, "providers"), [1])),
    ("key_file is a number", "cells[0].key_file", _set(("cells", 0, "key_file"), 0)),
    ("key_file is a list", "cells[0].key_file", _set(("cells", 0, "key_file"), ["k"])),
    ("tier is unknown", "cells[0].tier", _set(("cells", 0, "tier"), "gold")),
    ("tier is a number", "cells[0].tier", _set(("cells", 0, "tier"), 1)),
    ("state is unknown", "cells[0].state", _set(("cells", 0, "state"), "DONE")),
    ("state is a list", "cells[0].state", _set(("cells", 0, "state"), ["EXITED"])),
    ("rc is a string", "cells[0].rc", _set(("cells", 0, "rc"), "0")),
    ("rc is a float", "cells[0].rc", _set(("cells", 0, "rc"), 0.0)),
    ("rc is a bool", "cells[0].rc", _set(("cells", 0, "rc"), False)),
    ("reason is a number", "cells[0].reason", _set(("cells", 0, "reason"), 1)),
    ("detail is a number", "cells[0].detail", _set(("cells", 0, "detail"), 1)),
    ("tokens key missing", "cells[0]", _drop(("cells", 0, "tokens"))),
    ("tokens is a list", "cells[0].tokens", _set(("cells", 0, "tokens"), [])),
    ("tokens field missing", "cells[0].tokens", _set(("cells", 0, "tokens"), {"input_tokens": 1})),
    ("unknown tokens key", "cells[0].tokens", _set(("cells", 0, "tokens"), dict(TOKENS_DOC, extra=1))),
    ("tokens value negative", "cells[0].tokens", _set(("cells", 0, "tokens"), dict(TOKENS_DOC, input_tokens=-1))),
    ("tokens value a bool", "cells[0].tokens", _set(("cells", 0, "tokens"), dict(TOKENS_DOC, output_tokens=True))),
    ("tokens value a string", "cells[0].tokens", _set(("cells", 0, "tokens"), dict(TOKENS_DOC, reasoning_tokens="1"))),
]


NAME_FIELDS = ("driver", "lane", "case", "model", "expected_ingress", "connection_mode", "key_file")
HOSTILE = ("../x", "$(id)", "a/b", "")


class NameFieldTests(unittest.TestCase):
    """Fields that downstream code turns into paths, names or argv."""

    def doc(self) -> dict:
        return json.loads(reach(CellState.EXITED).to_json())

    def test_hostile_values_are_refused_naming_the_field(self) -> None:
        for field in NAME_FIELDS + ("providers",):
            for value in HOSTILE:
                with self.subTest(field=field, value=value):
                    doc = self.doc()
                    doc["cells"][0][field] = [value] if field == "providers" else value

                    with self.assertRaises(LedgerError) as caught:
                        RunLedger.from_json(json.dumps(doc))
                    self.assertIn(f"cells[0].{field}", str(caught.exception))

    def test_every_value_the_committed_matrix_produces_is_accepted(self) -> None:
        import matrix

        paths = matrix.Paths.from_root(os.path.abspath(os.path.join(WIRE_WATCH, "..", "..")))
        specs = matrix.expand(paths, matrix.load_rows(paths))
        led = RunLedger.new(INPUTS, specs)
        self.assertTrue(any(s.key_file for s in specs), "no committed row has a key file")

        self.assertEqual(RunLedger.from_json(led.to_json()), led)

    def test_matrix_uses_the_ledger_patterns(self) -> None:
        import matrix

        for name in ("DRIVER_RE", "LANE_RE", "KEY_FILE_RE", "TOKEN_RE", "CONNECTION_MODES"):
            with self.subTest(name):
                self.assertIs(getattr(matrix, name), getattr(ledger, name))


class TrailingNewlineTests(unittest.TestCase):
    """`$` matches before a final newline; every validator must refuse it."""

    def test_run_json_fields_with_a_trailing_newline_are_refused(self) -> None:
        def cell_field(field: str):
            return lambda d: d["cells"][0].__setitem__(field, d["cells"][0][field] + "\n")

        rows = [(f"cells[0].{f}", cell_field(f)) for f in ("id", "driver", "lane", "case", "model", "expected_ingress")]
        rows += [
            ("cells[0].providers", lambda d: d["cells"][0].__setitem__("providers", ["openai\n"])),
            ("cells[0].key_file", lambda d: d["cells"][0].__setitem__("key_file", "some-key\n")),
            ("inputs.cells_sha256", lambda d: d["inputs"].__setitem__("cells_sha256", "a" * 64 + "\n")),
            ("cells[0]", lambda d: d["cells"][0].update(state="NOT_RUN", rc=None, reason="credential", detail="missing\n")),
        ]
        for field, plant in rows:
            with self.subTest(field):
                doc = json.loads(reach(CellState.EXITED).to_json())
                plant(doc)

                with self.assertRaises(LedgerError) as caught:
                    RunLedger.from_json(json.dumps(doc))
                self.assertIn(field, str(caught.exception))

    def test_a_new_cell_id_with_a_trailing_newline_is_refused(self) -> None:
        with self.assertRaises(LedgerError):
            RunLedger.new(INPUTS, [spec(CELL + "\n")])

    def test_a_detail_with_a_trailing_newline_is_refused(self) -> None:
        with self.assertRaises(LedgerError):
            one_cell().transition(CELL, CellState.NOT_RUN, reason="credential", detail="missing\n")


class StrictParseTests(unittest.TestCase):
    def test_a_valid_document_parses(self) -> None:
        led = reach(CellState.EXITED)
        self.assertEqual(RunLedger.from_json(led.to_json()), led)

    def test_each_malformed_field_is_refused_by_name(self) -> None:
        for name, field, plant in STRICT_ROWS:
            with self.subTest(name):
                doc = json.loads(reach(CellState.EXITED).to_json())
                plant(doc)

                with self.assertRaises(LedgerError) as caught:
                    RunLedger.from_json(json.dumps(doc))
                self.assertIn(field, str(caught.exception))

    def test_duplicate_keys_and_non_finite_numbers_are_refused(self) -> None:
        text = reach(CellState.EXITED).to_json()
        rows = [
            ("duplicate top-level key", text.replace('"schema":', '"cells": [],\n  "schema":', 1), "duplicate key"),
            ("duplicate key in inputs", text.replace('"cases":', '"cases": [],\n    "cases":', 1), "duplicate key"),
            ("duplicate key in a cell", text.replace('"rc": 0', '"rc": 1, "rc": 0', 1), "duplicate key"),
            ("NaN rc", text.replace('"rc": 0', '"rc": NaN', 1), "non-finite number"),
            ("Infinity rc", text.replace('"rc": 0', '"rc": Infinity', 1), "non-finite number"),
            ("-Infinity rc", text.replace('"rc": 0', '"rc": -Infinity', 1), "non-finite number"),
        ]
        for name, planted, detail in rows:
            with self.subTest(name):
                self.assertNotEqual(planted, text, "the plant did not apply")
                with self.assertRaises(LedgerError) as caught:
                    RunLedger.from_json(planted)
                self.assertIn("document", str(caught.exception))
                self.assertIn(detail, str(caught.exception))

    def test_reason_outside_its_vocabulary_is_refused(self) -> None:
        doc = json.loads(reach(CellState.NOT_RUN).to_json())
        doc["cells"][0]["reason"] = "flaky"
        with self.assertRaises(LedgerError):
            RunLedger.from_json(json.dumps(doc))

    def test_a_document_that_is_not_an_object_is_refused(self) -> None:
        for text in ("[]", "null", '"x"', "1"):
            with self.subTest(text):
                with self.assertRaises(LedgerError) as caught:
                    RunLedger.from_json(text)
                self.assertIn("run.json", str(caught.exception))

    def test_deeply_nested_json_is_refused(self) -> None:
        with self.assertRaises(LedgerError):
            RunLedger.from_json("[" * 60_000)


class BoundStoppedTests(unittest.TestCase):
    def test_bound_stopped_by_state_and_stop_detail(self) -> None:
        running = reach(CellState.RUNNING)
        rows = [
            ("pending", reach(CellState.PENDING), False),
            ("running", running, False),
            ("exited", reach(CellState.EXITED), False),
            ("not run", reach(CellState.NOT_RUN), False),
            ("timeout", reach(CellState.TIMEOUT), True),
            ("cap reached", reach(CellState.CAP_REACHED), True),
            ("interrupted", reach(CellState.INTERRUPTED), True),
            ("exited 9 on its own", running.transition(CELL, CellState.EXITED, rc=9), False),
            ("exited 9 with an unrelated detail", running.transition(CELL, CellState.EXITED, rc=9, detail="other"), False),
        ]
        rows += [
            (f"exited 9 after a {d} stop", running.transition(CELL, CellState.EXITED, rc=9, detail=d), True)
            for d in sorted(ledger.STOP_DETAILS)
        ]
        for name, led, want in rows:
            with self.subTest(name):
                self.assertIs(ledger.bound_stopped(led.entry(CELL)), want)


class RunStopTests(unittest.TestCase):
    def test_a_new_ledger_has_no_run_stop(self) -> None:
        led = one_cell()
        self.assertIsNone(led.run_stop)
        self.assertIsNone(json.loads(led.to_json())["run_stop"])

    def test_run_stop_round_trips(self) -> None:
        led = reach(CellState.EXITED).with_run_stop(ledger.RunStop.RUN_SIZE)

        self.assertIs(led.run_stop, ledger.RunStop.RUN_SIZE)
        self.assertEqual(json.loads(led.to_json())["run_stop"], "run-size")
        self.assertEqual(RunLedger.from_json(led.to_json()), led)

    def test_run_stop_survives_a_later_transition(self) -> None:
        led = reach(CellState.RUNNING).with_run_stop(ledger.RunStop.RUN_SIZE).transition(CELL, CellState.EXITED, rc=0)
        self.assertIs(led.run_stop, ledger.RunStop.RUN_SIZE)

    def test_every_run_stop_round_trips_and_is_a_detail_shaped_token(self) -> None:
        self.assertEqual(
            {m.value for m in ledger.RunStop}, {"run-size", "leftover-containers", "sweep-failed"}
        )
        for member in ledger.RunStop:
            with self.subTest(member.value):
                self.assertRegex(member.value, ledger.DETAIL_RE)
                led = one_cell().with_run_stop(member)
                self.assertEqual(RunLedger.from_json(led.to_json()), led)

    def test_with_run_stop_refuses_anything_outside_the_vocabulary(self) -> None:
        with self.assertRaises(LedgerError):
            one_cell().with_run_stop("run-size")  # type: ignore[arg-type]


class TokensTests(unittest.TestCase):
    def test_an_exited_cell_round_trips_its_tokens(self) -> None:
        led = reach(CellState.EXITED).with_tokens(CELL, ledger.Tokens(**TOKENS_DOC))

        doc = json.loads(led.to_json())
        self.assertEqual(doc["cells"][0]["tokens"], TOKENS_DOC)
        self.assertEqual(RunLedger.from_json(led.to_json()), led)

    def test_only_an_exited_cell_carries_tokens(self) -> None:
        for state in CellState:
            if state is CellState.EXITED:
                continue
            with self.subTest(state.value):
                with self.assertRaises(LedgerError):
                    reach(state).with_tokens(CELL, ledger.Tokens(**TOKENS_DOC))
                doc = json.loads(reach(state).to_json())
                doc["cells"][0]["tokens"] = TOKENS_DOC
                with self.assertRaises(LedgerError) as caught:
                    RunLedger.from_json(json.dumps(doc))
                self.assertIn("cells[0].tokens", str(caught.exception))

    def test_tokens_from_totals_needs_every_field_as_a_count(self) -> None:
        self.assertEqual(ledger.tokens_from_totals(dict(TOKENS_DOC, requests=2)), ledger.Tokens(**TOKENS_DOC))
        rows = [
            ("field absent", {k: v for k, v in TOKENS_DOC.items() if k != "cache_read_billed"}),
            ("negative", dict(TOKENS_DOC, input_tokens=-1)),
            ("bool", dict(TOKENS_DOC, input_tokens=True)),
            ("float", dict(TOKENS_DOC, input_tokens=1.0)),
            ("null", dict(TOKENS_DOC, input_tokens=None)),
        ]
        for name, totals in rows:
            with self.subTest(name):
                self.assertIsNone(ledger.tokens_from_totals(totals))


class AtomicWriteTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self._tmp.name, "run.json")
        ledger.write_atomic(self.path, one_cell())
        with open(self.path, "rb") as handle:
            self.before = handle.read()

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def temps(self) -> list:
        return [
            os.path.join(self._tmp.name, n)
            for n in sorted(os.listdir(self._tmp.name))
            if n != "run.json"
        ]

    def test_each_write_uses_its_own_temp_file(self) -> None:
        made = []
        real_mkstemp = tempfile.mkstemp

        def spy(*args, **kwargs):
            fd, name = real_mkstemp(*args, **kwargs)
            made.append(name)
            return fd, name

        with mock.patch.object(ledger.tempfile, "mkstemp", spy):
            ledger.write_atomic(self.path, reach(CellState.RUNNING))
            ledger.write_atomic(self.path, reach(CellState.TIMEOUT))

        self.assertEqual(len(set(made)), 2)
        self.assertEqual({os.path.dirname(n) for n in made}, {self._tmp.name})

    def test_a_stale_temp_of_the_old_fixed_name_is_not_reused(self) -> None:
        stale = os.path.join(self._tmp.name, ".run.json.tmp")
        os.symlink(os.path.join(self._tmp.name, "elsewhere"), stale)

        ledger.write_atomic(self.path, reach(CellState.RUNNING))

        self.assertFalse(os.path.exists(os.path.join(self._tmp.name, "elsewhere")))
        self.assertIs(ledger.load(self.path).entry(CELL).state, CellState.RUNNING)

    def test_a_failed_write_removes_its_temp_file(self) -> None:
        for name, target in (("fsync", "fsync"), ("rename", "replace"), ("write", "write")):
            with self.subTest(name):
                with mock.patch.object(ledger.os, target, side_effect=OSError("injected")):
                    with self.assertRaises(OSError):
                        ledger.write_atomic(self.path, reach(CellState.RUNNING))

                self.assertEqual(self.temps(), [])
                with open(self.path, "rb") as handle:
                    self.assertEqual(handle.read(), self.before)

    def test_the_directory_is_fsynced_after_the_rename(self) -> None:
        calls = []
        real_replace, real_fsync = os.replace, os.fsync

        def on_replace(*args):
            calls.append("replace")
            return real_replace(*args)

        def on_fsync(fd):
            calls.append("fsync-dir" if os.path.isdir(f"/proc/self/fd/{fd}") else "fsync-file")
            return real_fsync(fd)

        if not os.path.isdir("/proc/self/fd"):
            self.skipTest("no /proc/self/fd")
        with mock.patch.object(ledger.os, "replace", on_replace), mock.patch.object(ledger.os, "fsync", on_fsync):
            ledger.write_atomic(self.path, reach(CellState.RUNNING))

        self.assertEqual(calls, ["fsync-file", "replace", "fsync-dir"])

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
        temps = self.temps()
        self.assertEqual(len(temps), 1, "the temp file was never written, so nothing was injected")
        self.assertIs(ledger.load(temps[0]).entry(CELL).state, CellState.RUNNING)
        with open(self.path, "rb") as handle:
            self.assertEqual(handle.read(), self.before)
        self.assertIs(ledger.load(self.path).entry(CELL).state, CellState.PENDING)


if __name__ == "__main__":
    unittest.main()
