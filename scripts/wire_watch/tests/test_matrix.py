"""matrix.py: cells.tsv validation, expansion, and preflight.

Every test runs against a throwaway copy of the COMMITTED inputs
(cells.tsv, models.tsv, lane configs, cases, drivers, ingress vocabulary),
so the real denominator is what is validated and counted, and a planted
defect is applied to the copy only.
"""

import csv
import io
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
WIRE_WATCH = os.path.dirname(HERE)
REPO = os.path.abspath(os.path.join(WIRE_WATCH, "..", ".."))
MATRIX = os.path.join(WIRE_WATCH, "matrix.py")
sys.path.insert(0, WIRE_WATCH)

import ledger  # noqa: E402
import matrix  # noqa: E402
from ledger import CELL_ID_MAX, CELL_ID_RE, CellState, ModelTier, Outcome  # noqa: E402

PLANTED_KEY = "PLANTEDKEYVALUE-7f3a9c1e5b2d8046-do-not-echo"


def copy_inputs(root: str) -> matrix.Paths:
    real = matrix.Paths.from_root(REPO)
    paths = matrix.Paths.from_root(root)
    os.makedirs(os.path.dirname(paths.cells_tsv))
    shutil.copy(real.cells_tsv, paths.cells_tsv)
    shutil.copy(real.models_tsv, paths.models_tsv)
    shutil.copytree(real.config_dir, paths.config_dir, ignore=shutil.ignore_patterns("*.sh", "*.py"))
    shutil.copytree(real.cases_dir, paths.cases_dir)
    os.makedirs(os.path.dirname(paths.ingress_kinds_sh))
    shutil.copy(real.ingress_kinds_sh, paths.ingress_kinds_sh)
    for name in os.listdir(real.drivers_dir):
        if name.endswith(".sh"):
            shutil.copy2(os.path.join(real.drivers_dir, name), os.path.join(paths.drivers_dir, name))
    return paths


def data_rows(path: str) -> list:
    with open(path, encoding="utf-8") as handle:
        lines = [line for line in handle if not line.startswith("#") and line.strip()]
    return list(csv.DictReader(io.StringIO("".join(lines)), delimiter="\t"))


def write_keys(key_dir: str, names: set, mode: int = 0o600) -> None:
    os.makedirs(key_dir, exist_ok=True)
    for name in names:
        path = os.path.join(key_dir, name)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(PLANTED_KEY + "\n")
        os.chmod(path, mode)


class MatrixTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = os.path.join(self._tmp.name, "repo")
        self.paths = copy_inputs(self.root)
        self.key_dir = os.path.join(self._tmp.name, "keys")
        self.key_names = {r["key_file"] for r in data_rows(self.paths.cells_tsv) if r["key_file"] != "-"}
        write_keys(self.key_dir, self.key_names)

    def tearDown(self) -> None:
        for dirpath, _, files in os.walk(self.key_dir):
            for name in files:
                os.chmod(os.path.join(dirpath, name), 0o600)
        self._tmp.cleanup()

    def edit_cells(self, line_no: int, **changes: str) -> None:
        """Change fields of the line_no-th DATA row (1-based)."""
        with open(self.paths.cells_tsv, encoding="utf-8") as handle:
            lines = handle.read().split("\n")
        data = [i for i, line in enumerate(lines) if line and not line.startswith("#")]
        header = lines[data[0]].split("\t")
        target = data[line_no]
        fields = dict(zip(header, lines[target].split("\t")))
        fields.update(changes)
        lines[target] = "\t".join(fields[c] for c in header)
        with open(self.paths.cells_tsv, "w", encoding="utf-8") as handle:
            handle.write("\n".join(lines))
        self.edited_line = target + 1

    def refusals(self) -> tuple:
        with self.assertRaises(matrix.MatrixError) as caught:
            matrix.load_rows(self.paths)
        return caught.exception.failures


class CommittedRowsTests(MatrixTestCase):
    def test_every_committed_row_validates(self) -> None:
        rows = matrix.load_rows(self.paths)
        self.assertEqual(len(rows), len(data_rows(self.paths.cells_tsv)))

    def test_the_cli_validates_the_real_tree(self) -> None:
        proc = subprocess.run([sys.executable, "-I", MATRIX, "validate", "--root", REPO], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_every_harness_ingress_pairing_has_a_row(self) -> None:
        drivers = {name for name in os.listdir(self.paths.drivers_dir) if name.endswith(".sh")}
        drivers.discard("external-agent-cli.sh")
        self.assertEqual({r.driver for r in matrix.load_rows(self.paths)}, drivers)


class RefusalTests(MatrixTestCase):
    def assertRefusedNamingRow(self, needle: str) -> None:
        failures = self.refusals()
        where = f"{self.paths.cells_tsv}:{self.edited_line}:"
        self.assertTrue(
            any(f.startswith(where) and needle in f for f in failures),
            f"no failure names {where} with {needle!r}: {failures}",
        )

    def test_unknown_ingress(self) -> None:
        self.edit_cells(1, expected_ingress="gemini")
        self.assertRefusedNamingRow("unknown expected_ingress 'gemini'")

    def test_missing_lane_config(self) -> None:
        self.edit_cells(1, lane="no-such-lane")
        self.assertRefusedNamingRow("no lane config for lane 'no-such-lane'")

    def test_front_proxy_without_mitm(self) -> None:
        self.edit_cells(1, lane="anthropic-api", connection_mode="front-proxy", providers="anthropic", key_file="-", sweep="-")
        self.assertRefusedNamingRow("front-proxy needs a [mitm] block")

    def test_base_url_on_a_mitm_lane(self) -> None:
        self.edit_cells(1, lane="anthropic-api.front-proxy", connection_mode="base-url", providers="anthropic", key_file="-", sweep="-")
        self.assertRefusedNamingRow("carries a [mitm] block")

    def test_providers_that_disagree_with_the_lane_config(self) -> None:
        self.edit_cells(1, lane="anthropic-api", providers="bedrock", key_file="-", sweep="-")
        self.assertRefusedNamingRow("must name exactly ['anthropic']")

    def test_unknown_connection_mode(self) -> None:
        self.edit_cells(1, connection_mode="direct")
        self.assertRefusedNamingRow("unknown connection_mode 'direct'")

    def test_key_file_with_a_path(self) -> None:
        self.edit_cells(1, lane="openai-compat.ollama", providers="ollama", key_file="../ollama-api-key", sweep="-")
        self.assertRefusedNamingRow("plain file name")

    def test_sweep_on_a_lane_without_candidates(self) -> None:
        self.edit_cells(1, lane="anthropic-api", providers="anthropic", key_file="-", sweep="anthropic")
        self.assertRefusedNamingRow("no models.tsv candidates")

    def test_a_second_sweeper_for_one_provider_table(self) -> None:
        rows = data_rows(self.paths.cells_tsv)
        index = next(
            i for i, r in enumerate(rows)
            if r["lane"] == "bedrock-invoke" and r["sweep"] == "-"
        )
        self.edit_cells(index + 1, sweep="bedrock")
        self.assertTrue(any("must be swept by exactly one row" in f for f in self.refusals()))

    def test_an_unswept_provider_table(self) -> None:
        rows = data_rows(self.paths.cells_tsv)
        index = next(i for i, r in enumerate(rows) if r["sweep"] == "mantle-chat")
        self.edit_cells(index + 1, sweep="-")
        self.assertTrue(any("'mantle-chat' must be swept by exactly one row" in f for f in self.refusals()))


def expected_counts(paths: matrix.Paths) -> dict:
    """The count formula, computed from the raw files without matrix.py."""
    cases = sorted(n[: -len(".json")] for n in os.listdir(paths.cases_dir) if n.endswith(".json"))
    models = data_rows(paths.models_tsv)
    providers = {}
    for lane in {m["lane"] for m in models}:
        with open(os.path.join(paths.config_dir, lane + ".toml"), encoding="utf-8") as handle:
            text = handle.read()
        for nick, provider in re.findall(r'^\[models\.([^\]]+)\]\nprovider = "([^"]+)"', text, re.MULTILINE):
            providers[(lane, nick)] = provider
    bedrock, other = 0, 0
    for row in data_rows(paths.cells_tsv):
        lane_models = [m for m in models if m["lane"] == row["lane"]]
        if not lane_models:
            other += len(cases)
            continue
        bedrock += len(cases) * sum(1 for m in lane_models if m["tier"] == "primary")
        swept = set() if row["sweep"] == "-" else set(row["sweep"].split(","))
        bedrock += sum(
            2 + (1 if m["thinking"] == "yes" else 0)
            for m in lane_models
            if m["tier"] == "sweep" and providers[(row["lane"], m["nickname"])] in swept
        )
    return {"bedrock": bedrock, "other": other}


class ExpansionTests(MatrixTestCase):
    def expand(self) -> tuple:
        return matrix.expand(self.paths, matrix.load_rows(self.paths))

    def test_count_matches_the_formula(self) -> None:
        specs = self.expand()
        bedrock_lanes = matrix.bedrock_lanes(self.paths)
        want = expected_counts(self.paths)

        got_bedrock = sum(1 for s in specs if s.lane in bedrock_lanes)
        got_other = sum(1 for s in specs if s.lane not in bedrock_lanes)

        self.assertEqual((got_bedrock, got_other), (want["bedrock"], want["other"]))
        self.assertGreater(want["bedrock"], 0)
        self.assertGreater(want["other"], 0)

    def test_each_sweep_candidate_runs_on_one_harness_row_only(self) -> None:
        models = data_rows(self.paths.models_tsv)
        sweep_nicks = {(m["lane"], m["nickname"]): m["thinking"] for m in models if m["tier"] == "sweep"}
        rows_per_candidate = {}
        cases_per_candidate = {}
        for s in self.expand():
            if s.tier is ModelTier.SWEEP:
                rows_per_candidate.setdefault((s.lane, s.model), set()).add((s.driver, s.connection_mode))
                cases_per_candidate.setdefault((s.lane, s.model), []).append(s.case)

        self.assertEqual(set(rows_per_candidate), set(sweep_nicks))
        for key, drivers in rows_per_candidate.items():
            with self.subTest(key):
                self.assertEqual(len(drivers), 1)
                want = 3 if sweep_nicks[key] == "yes" else 2
                self.assertEqual(len(cases_per_candidate[key]), want)

    def test_the_sweep_set_is_baseline_tool_and_thinking_without_tools(self) -> None:
        cases = matrix.load_cases(self.paths.cases_dir)
        self.assertEqual(matrix.sweep_cases(cases, True), ("plain-turn-01", "tools-multiturn-01", "thinking-01"))
        self.assertEqual(matrix.sweep_cases(cases, False), ("plain-turn-01", "tools-multiturn-01"))

    def test_every_bedrock_row_runs_the_primary_on_every_case(self) -> None:
        cases = {c.case_id for c in matrix.load_cases(self.paths.cases_dir)}
        bedrock_lanes = matrix.bedrock_lanes(self.paths)
        for row in matrix.load_rows(self.paths):
            if row.lane not in bedrock_lanes:
                continue
            with self.subTest(row.driver + " " + row.lane):
                got = {s.case for s in self.expand() if s.driver == row.driver and s.lane == row.lane and s.tier is ModelTier.PRIMARY}
                self.assertEqual(got, cases)

    def test_cell_ids_are_safe_tokens(self) -> None:
        for s in self.expand():
            self.assertRegex(s.id, CELL_ID_RE)
            self.assertLessEqual(len(s.id), CELL_ID_MAX)

    def test_expansion_is_byte_identical_for_the_same_inputs(self) -> None:
        first = matrix.build_ledger(self.paths, self.key_dir).to_json()
        second = matrix.build_ledger(self.paths, self.key_dir).to_json()
        self.assertEqual(first, second)

    def test_the_cli_writes_byte_identical_run_json(self) -> None:
        argv = [sys.executable, "-I", MATRIX, "expand", "--root", self.root, "--key-dir", self.key_dir]
        outs = [subprocess.run(argv, capture_output=True, check=True).stdout for _ in range(2)]
        self.assertEqual(outs[0], outs[1])
        self.assertTrue(outs[0])

    def test_a_case_change_changes_the_run(self) -> None:
        before = matrix.build_ledger(self.paths, self.key_dir).to_json()
        os.remove(os.path.join(self.paths.cases_dir, "large-context-01.json"))
        self.assertNotEqual(matrix.build_ledger(self.paths, self.key_dir).to_json(), before)


class PreflightTests(MatrixTestCase):
    def states_for(self, predicate) -> set:
        led = matrix.build_ledger(self.paths, self.key_dir)
        return {(e.state, e.reason, e.detail) for e in led.entries if predicate(e.spec)}

    def key_cells(self, name: str):
        return lambda s: s.key_file == name

    def test_usable_keys_leave_cells_pending(self) -> None:
        self.assertEqual(self.states_for(lambda _: True), {(CellState.PENDING, None, None)})

    def test_missing_driver(self) -> None:
        os.remove(os.path.join(self.paths.drivers_dir, "pi-chat.sh"))
        self.assertEqual(self.states_for(lambda s: s.driver == "pi-chat.sh"), {(CellState.NOT_RUN, "driver", "missing")})
        self.assertEqual(self.states_for(lambda s: s.driver == "codex.sh"), {(CellState.PENDING, None, None)})

    def test_non_executable_driver(self) -> None:
        os.chmod(os.path.join(self.paths.drivers_dir, "codex.sh"), 0o644)
        self.assertEqual(self.states_for(lambda s: s.driver == "codex.sh"), {(CellState.NOT_RUN, "driver", "missing")})

    def test_credential_rows(self) -> None:
        name = "ollama-api-key"
        path = os.path.join(self.key_dir, name)
        rows = [
            ("missing", lambda: os.remove(path)),
            ("insecure-mode", lambda: os.chmod(path, 0o644)),
            ("insecure-mode", lambda: os.chmod(path, 0o640)),
            ("not-a-file", lambda: (os.remove(path), os.mkdir(path, 0o700))),
            ("empty", lambda: open(path, "w").close()),
        ]
        if os.geteuid() != 0:
            rows.append(("unreadable", lambda: os.chmod(path, 0o200)))
        for detail, plant in rows:
            with self.subTest(detail):
                if os.path.isdir(path):
                    os.rmdir(path)
                write_keys(self.key_dir, {name})
                plant()

                self.assertEqual(self.states_for(self.key_cells(name)), {(CellState.NOT_RUN, "credential", detail)})
                self.assertEqual(self.states_for(lambda s: s.key_file is None), {(CellState.PENDING, None, None)})

    def test_not_run_cells_are_never_pass(self) -> None:
        os.chmod(os.path.join(self.key_dir, "ollama-api-key"), 0o644)
        os.remove(os.path.join(self.paths.drivers_dir, "codex.sh"))
        led = matrix.build_ledger(self.paths, self.key_dir)
        not_run = [e for e in led.entries if e.state is CellState.NOT_RUN]
        self.assertTrue(not_run)
        self.assertEqual({ledger.terminal_outcome(e) for e in not_run}, {Outcome.NOT_RUN})

    def test_no_key_value_in_run_json_or_messages(self) -> None:
        os.chmod(os.path.join(self.key_dir, "ollama-api-key"), 0o644)
        os.chmod(os.path.join(self.key_dir, sorted(self.key_names)[0]), 0o604)
        with open(os.path.join(self.key_dir, "ollama-api-key"), encoding="utf-8") as handle:
            self.assertIn(PLANTED_KEY, handle.read(), "the planted value is not in the key file")
        outputs = []
        for command in ("validate", "expand"):
            proc = subprocess.run(
                [sys.executable, "-I", MATRIX, command, "--root", self.root, "--key-dir", self.key_dir],
                capture_output=True, text=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            outputs.extend([proc.stdout, proc.stderr])
        run_json = outputs[2]
        self.assertIn('"credential"', run_json, "the planted insecure keys produced no credential row")
        for text in outputs:
            self.assertNotIn(PLANTED_KEY, text)
            self.assertNotIn(PLANTED_KEY[:16], text)

    def test_the_key_file_is_never_read(self) -> None:
        opened = []
        real_open = open

        def spy(path, *args, **kwargs):
            if str(path).startswith(self.key_dir):
                opened.append(path)
            return real_open(path, *args, **kwargs)

        import builtins
        from unittest import mock

        with mock.patch.object(builtins, "open", spy):
            matrix.build_ledger(self.paths, self.key_dir)
        self.assertEqual(opened, [])


if __name__ == "__main__":
    unittest.main()
