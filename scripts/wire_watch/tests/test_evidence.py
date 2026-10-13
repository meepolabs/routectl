"""evidence.py over directories the REAL writer produced.

Every evidence directory here is written by running
scripts/drivers/lib/cell_evidence.py `finalize` -- the same entry point
capture_driver.sh calls -- over a synthetic trace, so client_result.json
and the trace artifacts have the writer's shape by construction. The
snapshot bodies are the one hand-built input; their shape is welded to the
daemon's Rust source in WeldTests.
"""

import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import unittest
from typing import Optional
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
WIRE_WATCH = os.path.dirname(HERE)
REPO = os.path.abspath(os.path.join(WIRE_WATCH, "..", ".."))
WRITER = os.path.join(REPO, "scripts", "drivers", "lib", "cell_evidence.py")
CAPTURE_DRIVER = os.path.join(REPO, "scripts", "capture_driver.sh")
STATUS_SRC = os.path.join(REPO, "crates", "routectl-cli", "src", "handlers", "status")
sys.path.insert(0, WIRE_WATCH)

import evidence  # noqa: E402
from evidence import CellEvidence, Incomplete, IncompleteReason  # noqa: E402


def styled(target: str) -> str:
    return f"\x1b[2m{target}\x1b[0m\x1b[2m:\x1b[0m"


TRACE = (
    "2026-08-25T10:00:00.600000Z TRACE request{method=POST path=/v1/messages}: "
    + styled("routectl_core::log_safe")
    + ' ingress request body ingress="anthropic" body={} redact_prompts_enabled=false\n'
    + "2026-08-25T10:00:01.000000Z  INFO request{method=GET path=/status/health}: "
    + styled("routectl_cli::handlers::status::field_verdict_log")
    + " envelope field verdict snapshot rc_field_verdicts_learned_total=0 rc_parser_unlocalized_total=2\n"
)

ERROR_LINE = (
    "2026-08-25T10:00:00.700000Z DEBUG request{}: "
    + styled("routectl_core::log_safe")
    + ' upstream error body provider_kind="bedrock" provider="bedrock:p" status=403'
    + ' body={"message":"You do not have access to the model"}\n'
)


def health_body() -> dict:
    return {
        "schema_version": evidence.HEALTH_SCHEMA_VERSION,
        "as_of": "2026-08-25T10:00:01Z",
        "data": {
            "targets": [],
            "learned_negatives": [],
            "wire_check": {
                "window_secs": 3600,
                "loss": [],
                "loss_findings": [],
                "checks": [{"lane": "anthropic-api", "outcome": "clean", "count": 1, "last_seen_unix_secs": 1}],
                "unrepaired_rejections": [],
                "local_refusals": [],
            },
        },
        "unavailable": None,
    }


def usage_body() -> dict:
    return {
        "schema_version": evidence.USAGE_SCHEMA_VERSION,
        "as_of": "2026-08-25T10:00:01Z",
        "data": {"window": "all", "totals": {"requests": 2, "input_tokens": 17, "output_tokens": 5}},
        "unavailable": None,
    }


class Cell:
    """A landing root with one evidence directory written by the writer."""

    def __init__(self, root: str) -> None:
        self.out = root
        self.dir = os.path.join(root, evidence.EVIDENCE_DIRNAME)

    def path(self, name: str) -> str:
        return os.path.join(self.dir, name)

    def write(self, name: str, text: str) -> None:
        with open(self.path(name), "w", encoding="utf-8") as handle:
            handle.write(text)

    def read(self, name: str) -> str:
        with open(self.path(name), encoding="utf-8") as handle:
            return handle.read()


def make_cell(
    root: str,
    *,
    trace: str = TRACE,
    health: object = None,
    usage: object = None,
    health_status: str = "ok",
    extra: tuple = (),
) -> Cell:
    cell = Cell(root)
    os.makedirs(cell.dir)
    if health_status == "ok":
        cell.write("health.json", json.dumps(health if health is not None else health_body()))
    cell.write("usage.json", json.dumps(usage if usage is not None else usage_body()))
    trace_path = os.path.join(root, "trace.log")
    with open(trace_path, "w", encoding="utf-8") as handle:
        handle.write(trace)
    argv = [
        sys.executable, "-I", WRITER, "finalize",
        "--dir", cell.dir, "--trace", trace_path,
        "--snapshot", f"health={health_status}", "--snapshot", "usage=ok",
        "--exit", "0", "--driver-rc", "0", "--rig-rc", "0",
        "--lane", "anthropic-api", "--case", "plain-turn-01",
        "--expected-ingress", "anthropic", "--connection-mode", "base-url", "--turns", "1",
        *extra,
    ]
    subprocess.run(argv, check=True, capture_output=True, env={"PATH": os.environ.get("PATH", "")})
    return cell


class EvidenceTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def assertIncomplete(self, result: evidence.Result, reason: IncompleteReason, artifact: Optional[str]) -> None:
        if not isinstance(result, Incomplete):
            self.fail(f"want Incomplete, got {result!r}")
        self.assertEqual((result.reason, result.artifact), (reason, artifact))


class CompleteCellTests(EvidenceTestCase):
    def test_a_complete_cell_reads_as_typed_evidence(self) -> None:
        cell = make_cell(self.root)

        result = evidence.read_cell(cell.out)

        self.assertIsInstance(result, CellEvidence)
        assert isinstance(result, CellEvidence)
        self.assertEqual((result.lane, result.case, result.exit, result.turns), ("anthropic-api", "plain-turn-01", 0, 1))
        self.assertEqual(result.ingress_kinds, ("anthropic",))
        self.assertEqual(result.fidelity_counters["rc_parser_unlocalized_total"], 2)
        self.assertEqual(result.usage_totals["input_tokens"], 17)
        checks = result.wire_check["checks"]
        assert isinstance(checks, list)
        self.assertEqual(checks[0]["outcome"], "clean")
        self.assertEqual(result.upstream_errors, ())

    def test_upstream_error_rows_are_typed(self) -> None:
        cell = make_cell(self.root, trace=TRACE + ERROR_LINE)

        result = evidence.read_cell(cell.out)

        assert isinstance(result, CellEvidence), result
        self.assertEqual([(e.provider_kind, e.status) for e in result.upstream_errors], [("bedrock", 403)])


class IncompleteCellTests(EvidenceTestCase):
    def test_no_evidence_directory(self) -> None:
        self.assertIncomplete(evidence.read_cell(self.root), IncompleteReason.NO_EVIDENCE, None)

    def test_missing_health_json_as_the_writer_records_it(self) -> None:
        cell = make_cell(self.root, health_status="http 500")
        self.assertIn("health.json", json.loads(cell.read("client_result.json"))["missing"])

        result = evidence.read_cell(cell.out)

        self.assertIncomplete(result, IncompleteReason.ARTIFACT_MISSING, "health.json")

    def test_an_artifact_the_writer_named_missing_is_not_trusted_when_present(self) -> None:
        cell = make_cell(self.root, health_status="http 500")
        cell.write("health.json", json.dumps(health_body()))

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.ARTIFACT_MISSING, "health.json")

    def test_health_json_removed_after_the_writer_ran(self) -> None:
        cell = make_cell(self.root)
        os.remove(cell.path("health.json"))

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.ARTIFACT_MISSING, "health.json")

    def test_unknown_health_schema_version(self) -> None:
        body = health_body()
        body["schema_version"] = evidence.HEALTH_SCHEMA_VERSION + 1
        cell = make_cell(self.root, health=body)

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.SNAPSHOT_SCHEMA, "health.json")

    def test_truncated_health_json(self) -> None:
        cell = make_cell(self.root)
        text = cell.read("health.json")
        cell.write("health.json", text[: len(text) // 2])

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.ARTIFACT_CORRUPT, "health.json")

    def test_the_three_health_failures_have_distinct_reasons(self) -> None:
        reasons = set()
        for name, mutate in (
            ("missing", lambda c: os.remove(c.path("health.json"))),
            ("schema", lambda c: c.write("health.json", json.dumps({**health_body(), "schema_version": 99}))),
            ("truncated", lambda c: c.write("health.json", c.read("health.json")[:20])),
        ):
            with self.subTest(name):
                cell = make_cell(os.path.join(self.root, name))
                mutate(cell)
                result = evidence.read_cell(cell.out)
                assert isinstance(result, Incomplete), result
                reasons.add(result.reason)
        self.assertEqual(len(reasons), 3)

    def test_unavailable_health_panel(self) -> None:
        body = {"schema_version": evidence.HEALTH_SCHEMA_VERSION, "as_of": None, "data": None, "unavailable": "db_unavailable"}
        cell = make_cell(self.root, health=body)

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.SNAPSHOT_UNAVAILABLE, "health.json")

    def test_health_without_wire_check_rows(self) -> None:
        body = health_body()
        del body["data"]["wire_check"]["loss_findings"]
        cell = make_cell(self.root, health=body)

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.SNAPSHOT_SHAPE, "health.json")

    def test_unknown_usage_schema_version(self) -> None:
        body = usage_body()
        body["schema_version"] = 0
        cell = make_cell(self.root, usage=body)

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.SNAPSHOT_SCHEMA, "usage.json")

    def test_missing_client_result(self) -> None:
        cell = make_cell(self.root)
        os.remove(cell.path("client_result.json"))

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.RESULT_MISSING, "client_result.json")

    def test_truncated_client_result(self) -> None:
        cell = make_cell(self.root)
        cell.write("client_result.json", cell.read("client_result.json")[:30])

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.RESULT_CORRUPT, "client_result.json")

    def test_unknown_layout_marker(self) -> None:
        cell = make_cell(self.root)
        doc = json.loads(cell.read("client_result.json"))
        doc["schema"] = "routectl-cell-evidence/2"
        cell.write("client_result.json", json.dumps(doc))

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.RESULT_SCHEMA, "client_result.json")

    def test_a_truncated_trace_artifact(self) -> None:
        cell = make_cell(self.root)
        doc = json.loads(cell.read("client_result.json"))
        doc["truncated"] = ["fidelity.log"]
        cell.write("client_result.json", json.dumps(doc))

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.ARTIFACT_TRUNCATED, "fidelity.log")

    def test_a_live_trace_names_every_trace_artifact_missing(self) -> None:
        cell = make_cell(self.root, extra=("--trace-live",))

        result = evidence.read_cell(cell.out)

        self.assertIncomplete(result, IncompleteReason.ARTIFACT_MISSING, "fidelity.log")
        self.assertIsNone(evidence.read_ingress_kinds(cell.dir))

    def test_corrupt_upstream_error_row(self) -> None:
        cell = make_cell(self.root)
        cell.write("upstream_errors.jsonl", '{"status": 403}\n')

        self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.ARTIFACT_CORRUPT, "upstream_errors.jsonl")

    def test_deeply_nested_json_in_any_artifact_is_incomplete(self) -> None:
        # Unclosed, so it stays under the smallest cap and only the
        # decoder's recursion, not the closing brackets, can reject it.
        deep = "[" * 60_000
        with self.assertRaises(RecursionError, msg="the payload no longer exhausts this interpreter's decoder"):
            json.loads(deep)
        rows = [
            ("client_result.json", IncompleteReason.RESULT_CORRUPT),
            ("health.json", IncompleteReason.ARTIFACT_CORRUPT),
            ("usage.json", IncompleteReason.ARTIFACT_CORRUPT),
            ("upstream_errors.jsonl", IncompleteReason.ARTIFACT_CORRUPT),
        ]
        for name, reason in rows:
            with self.subTest(name):
                cell = make_cell(os.path.join(self.root, name))
                # client_result.json's cap is far below this depth; the
                # deepest payload that fits still has to come back Incomplete.
                payload = deep[: evidence.ARTIFACT_MAX_BYTES[name] - 1]
                cell.write(name, payload + "\n")

                self.assertIncomplete(evidence.read_cell(cell.out), reason, name)
                if name == "client_result.json":
                    self.assertEqual(evidence.read_ingress_kinds(cell.dir), ("anthropic",))

    def test_wrong_types_at_any_level_are_incomplete(self) -> None:
        def result_with(**changes: object):
            def plant(cell: Cell) -> None:
                doc = json.loads(cell.read("client_result.json"))
                doc.update(changes)
                cell.write("client_result.json", json.dumps(doc))
            return plant

        def health_with(path: tuple, value: object):
            def plant(cell: Cell) -> None:
                doc = health_body()
                node = doc
                for key in path[:-1]:
                    node = node[key]
                node[path[-1]] = value
                cell.write("health.json", json.dumps(doc))
            return plant

        rows = [
            ("result is a list", lambda c: c.write("client_result.json", "[]"), IncompleteReason.RESULT_CORRUPT, "client_result.json"),
            ("missing is a list", result_with(missing=["health.json"]), IncompleteReason.RESULT_CORRUPT, "client_result.json"),
            ("truncated holds an object", result_with(truncated=[{"a": 1}]), IncompleteReason.RESULT_CORRUPT, "client_result.json"),
            ("lane is a number", result_with(lane=7), IncompleteReason.RESULT_CORRUPT, "client_result.json"),
            ("exit is a bool", result_with(exit=True), IncompleteReason.RESULT_CORRUPT, "client_result.json"),
            ("health is a string", lambda c: c.write("health.json", '"x"'), IncompleteReason.ARTIFACT_CORRUPT, "health.json"),
            ("wire_check is a list", health_with(("data", "wire_check"), []), IncompleteReason.SNAPSHOT_SHAPE, "health.json"),
            ("loss is an object", health_with(("data", "wire_check", "loss"), {}), IncompleteReason.SNAPSHOT_SHAPE, "health.json"),
            ("error row is a list", lambda c: c.write("upstream_errors.jsonl", "[1, 2]\n"), IncompleteReason.ARTIFACT_CORRUPT, "upstream_errors.jsonl"),
            ("error status is a string", lambda c: c.write("upstream_errors.jsonl", '{"provider_kind": "x", "status": "403", "excerpt": ""}\n'), IncompleteReason.ARTIFACT_CORRUPT, "upstream_errors.jsonl"),
            ("error excerpt is nested", lambda c: c.write("upstream_errors.jsonl", '{"provider_kind": "x", "status": 403, "excerpt": [[1]]}\n'), IncompleteReason.ARTIFACT_CORRUPT, "upstream_errors.jsonl"),
        ]
        for name, plant, reason, artifact in rows:
            with self.subTest(name):
                cell = make_cell(os.path.join(self.root, name.replace(" ", "-")))
                plant(cell)

                self.assertIncomplete(evidence.read_cell(cell.out), reason, artifact)

    def test_ingress_kinds_are_readable_from_an_incomplete_cell(self) -> None:
        cell = make_cell(self.root, health_status="http 500")
        self.assertIsInstance(evidence.read_cell(cell.out), Incomplete)

        self.assertEqual(evidence.read_ingress_kinds(cell.dir), ("anthropic",))


class SizeCapTests(EvidenceTestCase):
    def oversize(self, cell: Cell, name: str) -> None:
        with open(cell.path(name), "wb") as handle:
            handle.write(b" " * (evidence.ARTIFACT_MAX_BYTES[name] + 1))

    def test_every_artifact_has_a_cap(self) -> None:
        names = {evidence.RESULT_NAME, *evidence.SNAPSHOT_ARTIFACTS, *evidence.TRACE_ARTIFACTS}
        self.assertEqual(set(evidence.ARTIFACT_MAX_BYTES), names)

    def test_an_artifact_over_its_cap_is_incomplete(self) -> None:
        for name in sorted(evidence.ARTIFACT_MAX_BYTES):
            with self.subTest(name):
                cell = make_cell(os.path.join(self.root, name))
                self.oversize(cell, name)

                self.assertIncomplete(evidence.read_cell(cell.out), IncompleteReason.ARTIFACT_OVERSIZE, name)

    def test_an_artifact_at_its_cap_is_read(self) -> None:
        cell = make_cell(self.root)
        with open(cell.path("fidelity.log"), "a", encoding="utf-8") as handle:
            pad = evidence.ARTIFACT_MAX_BYTES["fidelity.log"] - os.path.getsize(cell.path("fidelity.log"))
            handle.write("x" * (pad - 1) + "\n")

        self.assertIsInstance(evidence.read_cell(cell.out), CellEvidence)

    def read_sizes(self, out: str) -> tuple:
        reads = []
        real_read = os.read

        def spy(fd: int, n: int) -> bytes:
            reads.append(n)
            return real_read(fd, n)

        with mock.patch.object(evidence.os, "read", spy):
            result = evidence.read_cell(out)
        return result, reads

    def test_an_oversize_file_is_refused_before_any_read(self) -> None:
        cell = make_cell(self.root)
        self.oversize(cell, evidence.RESULT_NAME)

        result, reads = self.read_sizes(cell.out)

        self.assertIncomplete(result, IncompleteReason.ARTIFACT_OVERSIZE, evidence.RESULT_NAME)
        self.assertEqual(reads, [])

    def test_a_file_larger_than_fstat_said_is_read_to_cap_plus_one_only(self) -> None:
        cell = make_cell(self.root)
        self.oversize(cell, evidence.RESULT_NAME)
        real_fstat = os.fstat

        def stale(fd: int) -> os.stat_result:
            fields = list(real_fstat(fd))
            fields[6] = 1
            return os.stat_result(fields)

        with mock.patch.object(evidence.os, "fstat", stale):
            result, reads = self.read_sizes(cell.out)

        self.assertIncomplete(result, IncompleteReason.ARTIFACT_OVERSIZE, evidence.RESULT_NAME)
        self.assertLessEqual(sum(reads), evidence.ARTIFACT_MAX_BYTES[evidence.RESULT_NAME] + 1)

    def test_a_fifo_artifact_is_incomplete_without_blocking(self) -> None:
        cell = make_cell(self.root)
        os.remove(cell.path("fidelity.log"))
        os.mkfifo(cell.path("fidelity.log"))

        def blocked(*_args: object) -> None:
            raise AssertionError("reading a FIFO artifact blocked")

        previous = signal.signal(signal.SIGALRM, blocked)
        signal.alarm(10)
        try:
            result = evidence.read_cell(cell.out)
        finally:
            signal.alarm(0)
            signal.signal(signal.SIGALRM, previous)

        self.assertIncomplete(result, IncompleteReason.ARTIFACT_CORRUPT, "fidelity.log")


@unittest.skipUnless(os.path.isfile(CAPTURE_DRIVER), "capture_driver.sh absent")
class CapWeldTests(unittest.TestCase):
    def test_snapshot_cap_matches_the_fetch_cap(self) -> None:
        with open(CAPTURE_DRIVER, encoding="utf-8") as handle:
            match = re.search(r"^SNAPSHOT_MAX_BYTES=(\d+)$", handle.read(), re.MULTILINE)
        assert match is not None, "SNAPSHOT_MAX_BYTES not found"
        for name in evidence.SNAPSHOT_ARTIFACTS:
            self.assertEqual(evidence.ARTIFACT_MAX_BYTES[name], int(match.group(1)))

    def test_a_trace_artifact_just_over_the_writer_bound_is_oversize(self) -> None:
        sys.path.insert(0, os.path.dirname(WRITER))
        import cell_evidence

        rows = [
            ("fidelity.log", cell_evidence.FIDELITY_MAX_BYTES + 1),
            ("ingress_kinds.txt", cell_evidence.INGRESS_MAX_TOKENS * 65 + 1),
        ]
        with tempfile.TemporaryDirectory() as root:
            for name, size in rows:
                with self.subTest(name):
                    cell = make_cell(os.path.join(root, name))
                    with open(cell.path(name), "wb") as handle:
                        handle.write(b"x" * size)

                    result = evidence.read_cell(cell.out)

                    assert isinstance(result, Incomplete), result
                    self.assertEqual((result.reason, result.artifact), (IncompleteReason.ARTIFACT_OVERSIZE, name))

    def test_ingress_token_bound_matches_the_writer(self) -> None:
        sys.path.insert(0, os.path.dirname(WRITER))
        import cell_evidence

        self.assertIn("{1,%d}" % evidence.INGRESS_TOKEN_MAX_CHARS, cell_evidence.INGRESS_FIELD.pattern)


SECRET = "OPERATORSECRET-4c1d9e7a2b5f3086-must-not-surface"


class SymlinkTests(EvidenceTestCase):
    """The container writes the cell directory; the host must not follow
    a link it planted to read an operator file."""

    def secret_file(self) -> str:
        path = os.path.join(self.root, "operator-key")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(SECRET + "\n")
        os.chmod(path, 0o600)
        return path

    def assertNoSecret(self, result: object) -> None:
        self.assertNotIn(SECRET, repr(result))

    def test_a_symlinked_artifact_is_not_followed(self) -> None:
        names = (evidence.RESULT_NAME, *evidence.SNAPSHOT_ARTIFACTS, *evidence.TRACE_ARTIFACTS)
        reasons = {evidence.RESULT_NAME: IncompleteReason.RESULT_CORRUPT}
        for name in names:
            with self.subTest(name):
                cell = make_cell(os.path.join(self.root, name))
                os.remove(cell.path(name))
                os.symlink(self.secret_file(), cell.path(name))

                result = evidence.read_cell(cell.out)

                self.assertIncomplete(result, reasons.get(name, IncompleteReason.ARTIFACT_CORRUPT), name)
                assert isinstance(result, Incomplete)
                self.assertEqual(result.detail, "symlink")
                self.assertNoSecret(result)
                self.assertNoSecret(evidence.read_ingress_kinds(cell.dir))

    def test_a_symlinked_ingress_file_is_not_read_by_read_ingress_kinds(self) -> None:
        cell = make_cell(self.root)
        os.remove(cell.path("ingress_kinds.txt"))
        os.symlink(self.secret_file(), cell.path("ingress_kinds.txt"))

        self.assertIsNone(evidence.read_ingress_kinds(cell.dir))

    def test_a_dangling_symlink_artifact(self) -> None:
        cell = make_cell(self.root)
        os.remove(cell.path("fidelity.log"))
        os.symlink(os.path.join(self.root, "nowhere"), cell.path("fidelity.log"))

        result = evidence.read_cell(cell.out)

        self.assertIncomplete(result, IncompleteReason.ARTIFACT_CORRUPT, "fidelity.log")
        assert isinstance(result, Incomplete)
        self.assertEqual(result.detail, "symlink")

    def test_a_symlinked_evidence_directory_is_not_followed(self) -> None:
        real = make_cell(os.path.join(self.root, "real"))
        out = os.path.join(self.root, "planted")
        os.makedirs(out)
        os.symlink(real.dir, os.path.join(out, evidence.EVIDENCE_DIRNAME))

        for result in (evidence.read_cell(out), evidence.read_dir(os.path.join(out, evidence.EVIDENCE_DIRNAME))):
            self.assertIncomplete(result, IncompleteReason.NO_EVIDENCE, None)
            assert isinstance(result, Incomplete)
            self.assertEqual(result.detail, "symlink")
        self.assertIsNone(evidence.read_ingress_kinds(os.path.join(out, evidence.EVIDENCE_DIRNAME)))

    def test_a_symlinked_evidence_directory_is_not_followed_through_any_spelling(self) -> None:
        real = make_cell(os.path.join(self.root, "real"))
        out = os.path.join(self.root, "planted")
        os.makedirs(out)
        link = os.path.join(out, evidence.EVIDENCE_DIRNAME)
        os.symlink(real.dir, link)
        rows = [("slash", link + "/", "symlink"), ("double slash", link + "//", "symlink"), ("dot", link + "/.", "bad-path")]
        for name, spelled, detail in rows:
            with self.subTest(name):
                result = evidence.read_dir(spelled)

                self.assertIncomplete(result, IncompleteReason.NO_EVIDENCE, None)
                assert isinstance(result, Incomplete)
                self.assertEqual(result.detail, detail)
                self.assertIsNone(evidence.read_ingress_kinds(spelled))

    def test_dot_and_dotdot_evidence_paths_are_refused(self) -> None:
        cell = make_cell(self.root)
        for spelled in (cell.dir + "/.", cell.dir + "/..", cell.dir + "/sub/..", "/"):
            with self.subTest(spelled):
                result = evidence.read_dir(spelled)
                self.assertIncomplete(result, IncompleteReason.NO_EVIDENCE, None)
                assert isinstance(result, Incomplete)
                self.assertEqual(result.detail, "bad-path")

    @unittest.skipIf(os.geteuid() == 0, "root bypasses directory permissions")
    def test_an_evidence_dir_under_a_search_only_parent_reads(self) -> None:
        cell = make_cell(os.path.join(self.root, "out"))
        os.chmod(cell.out, 0o100)
        try:
            result = evidence.read_cell(cell.out)
        finally:
            os.chmod(cell.out, 0o700)
        self.assertIsInstance(result, CellEvidence)

    def test_a_trailing_slash_on_a_real_evidence_dir_still_reads(self) -> None:
        cell = make_cell(self.root)
        self.assertIsInstance(evidence.read_dir(cell.dir + "/"), CellEvidence)
        self.assertEqual(evidence.read_ingress_kinds(cell.dir + "//"), ("anthropic",))

    def test_no_descriptor_is_left_open(self) -> None:
        if not os.path.isdir("/proc/self/fd"):
            self.skipTest("no /proc/self/fd")
        cells = [make_cell(os.path.join(self.root, str(i))) for i in range(3)]
        os.remove(cells[1].path("usage.json"))
        os.symlink(self.secret_file(), cells[1].path("usage.json"))
        cells[2].write("health.json", "{")
        before = len(os.listdir("/proc/self/fd"))

        for cell in cells:
            evidence.read_cell(cell.out)
            evidence.read_ingress_kinds(cell.dir)

        self.assertEqual(len(os.listdir("/proc/self/fd")), before)


def _replace_once(cell: Cell, name: str, old: str, new: str) -> None:
    text = cell.read(name)
    assert text.count(old) == 1, (name, old)
    cell.write(name, text.replace(old, new))


class FidelityCounterTests(EvidenceTestCase):
    def with_last_line(self, line: str) -> evidence.Result:
        cell = make_cell(self.root)
        cell.write("fidelity.log", cell.read("fidelity.log") + line + "\n")
        return evidence.read_cell(cell.out)

    def test_a_counter_with_too_many_digits_is_corrupt(self) -> None:
        result = self.with_last_line("x envelope field verdict snapshot rc_planted_total=" + "9" * 5000)

        self.assertIncomplete(result, IncompleteReason.ARTIFACT_CORRUPT, "fidelity.log")
        assert isinstance(result, Incomplete)
        self.assertEqual(result.detail, "counter too long")

    def test_a_counter_one_digit_over_the_bound_is_corrupt(self) -> None:
        result = self.with_last_line("x rc_planted_total=" + "1" * 21)

        self.assertIncomplete(result, IncompleteReason.ARTIFACT_CORRUPT, "fidelity.log")

    def test_a_counter_at_the_bound_is_read(self) -> None:
        result = self.with_last_line("x rc_planted_total=" + "9" * 20)

        assert isinstance(result, CellEvidence), result
        self.assertEqual(result.fidelity_counters["rc_planted_total"], int("9" * 20))


class IngressTokenTests(EvidenceTestCase):
    def test_a_line_that_is_not_a_writer_token_is_corrupt(self) -> None:
        rows = [
            ("escape sequence", "\x1b[31manthropic\x1b[0m"),
            ("over-long token", "a" * 65),
            ("uppercase", "Anthropic"),
            ("embedded space", "anth ropic"),
        ]
        for name, line in rows:
            with self.subTest(name):
                cell = make_cell(os.path.join(self.root, name.replace(" ", "-")))
                cell.write("ingress_kinds.txt", "anthropic\n" + line + "\n")

                result = evidence.read_cell(cell.out)

                self.assertIncomplete(result, IncompleteReason.ARTIFACT_CORRUPT, "ingress_kinds.txt")
                assert isinstance(result, Incomplete)
                self.assertEqual(result.detail, "bad token")
                self.assertIsNone(evidence.read_ingress_kinds(cell.dir))

    def test_a_token_at_the_writer_length_bound_is_read(self) -> None:
        cell = make_cell(self.root)
        cell.write("ingress_kinds.txt", "a" * 64 + "\n")

        result = evidence.read_cell(cell.out)

        assert isinstance(result, CellEvidence), result
        self.assertEqual(result.ingress_kinds, ("a" * 64,))

    def test_the_token_rule_is_the_writer_rule(self) -> None:
        sys.path.insert(0, os.path.dirname(WRITER))
        import cell_evidence

        self.assertIs(evidence.INGRESS_TOKEN_RE.pattern, cell_evidence.INGRESS_TOKEN)
        self.assertIn(cell_evidence.INGRESS_TOKEN, cell_evidence.INGRESS_FIELD.pattern)


class StrictJsonTests(EvidenceTestCase):
    def test_duplicate_keys_and_non_finite_numbers_are_incomplete(self) -> None:
        result_reason = IncompleteReason.RESULT_CORRUPT
        corrupt = IncompleteReason.ARTIFACT_CORRUPT
        row = '{"excerpt": "", "provider_kind": "bedrock", "status": 403%s}\n'
        rows = [
            ("duplicate top-level key in result", evidence.RESULT_NAME, result_reason, "duplicate key",
             lambda c: _replace_once(c, evidence.RESULT_NAME, '"lane": "anthropic-api"', '"lane": "x", "lane": "anthropic-api"')),
            ("duplicate nested key in health", "health.json", corrupt, "duplicate key",
             lambda c: c.write("health.json", json.dumps(health_body()).replace('"loss": []', '"loss": [{"a": 1, "a": 2}]'))),
            ("duplicate top-level key in usage", "usage.json", corrupt, "duplicate key",
             lambda c: c.write("usage.json", json.dumps(usage_body())[:-1] + ', "data": null}')),
            ("duplicate key in a jsonl row", "upstream_errors.jsonl", corrupt, "duplicate key",
             lambda c: c.write("upstream_errors.jsonl", row % ', "status": 500')),
            ("NaN in result", evidence.RESULT_NAME, result_reason, "non-finite number",
             lambda c: _replace_once(c, evidence.RESULT_NAME, '"lane": ', '"extra": NaN, "lane": ')),
            ("Infinity in snapshot", "usage.json", corrupt, "non-finite number",
             lambda c: c.write("usage.json", json.dumps(usage_body()).replace('"requests": 2', '"requests": Infinity'))),
            ("-Infinity in jsonl", "upstream_errors.jsonl", corrupt, "non-finite number",
             lambda c: c.write("upstream_errors.jsonl", row % ', "extra": -Infinity')),
        ]
        for name, artifact, reason, detail, plant in rows:
            with self.subTest(name):
                cell = make_cell(os.path.join(self.root, name.replace(" ", "-")))
                plant(cell)

                result = evidence.read_cell(cell.out)

                self.assertIncomplete(result, reason, artifact)
                assert isinstance(result, Incomplete)
                self.assertEqual(result.detail, detail)


def finalize_argv(directory: str, trace: str, **overrides: str) -> list:
    opts = {
        "--lane": "anthropic-api", "--case": "plain-turn-01", "--expected-ingress": "anthropic",
        "--connection-mode": "base-url", "--turns": "1", "--exit": "0", "--driver-rc": "0", "--rig-rc": "0",
    }
    opts.update(overrides)
    argv = [sys.executable, "-I", WRITER, "finalize", "--dir", directory, "--trace", trace]
    for flag, value in opts.items():
        argv += [flag, value]
    return argv


class WriterBoundTests(EvidenceTestCase):
    """client_result.json's size is bounded by the writer's own field caps,
    and the reader's cap is derived from that bound."""

    def setUp(self) -> None:
        super().setUp()
        sys.path.insert(0, os.path.dirname(WRITER))
        import cell_evidence

        self.writer = cell_evidence

    def string_at(self, cap: int, extra: int = 0) -> str:
        return "x" * (cap - 2 + extra)

    def test_every_reason_the_writer_composes_fits_the_status_cap(self) -> None:
        reasons = ["daemon still running", "absent after fetch", "not fetched",
                   "trace unreadable: " + max((c.__name__ for c in OSError.__subclasses__()), key=len)]
        for reason in reasons:
            with self.subTest(reason):
                self.assertLessEqual(len(json.dumps(reason)), self.writer.STATUS_MAX_JSON_BYTES)

    def test_the_writer_int_bound_is_one_the_reader_decodes(self) -> None:
        import strict_json

        self.assertLessEqual(self.writer.INT_MAX_CHARS, strict_json.INT_MAX_CHARS)

    def test_the_reader_cap_is_the_writer_bound(self) -> None:
        self.assertEqual(evidence.ARTIFACT_MAX_BYTES[evidence.RESULT_NAME], self.writer.RESULT_MAX_BYTES)

    def test_string_fields_at_the_cap_read_as_complete(self) -> None:
        value = self.string_at(self.writer.FIELD_MAX_JSON_BYTES)
        extra = ("--lane", value, "--case", value, "--expected-ingress", value, "--connection-mode", value)

        result = evidence.read_cell(make_cell(self.root, extra=extra).out)

        assert isinstance(result, CellEvidence), result
        self.assertEqual((result.lane, result.case, result.expected_ingress, result.connection_mode), (value,) * 4)

    def test_the_largest_permitted_result_fits_the_reader_cap(self) -> None:
        cell = Cell(self.root)
        os.makedirs(cell.dir)
        field = self.string_at(self.writer.FIELD_MAX_JSON_BYTES)
        status = self.string_at(self.writer.STATUS_MAX_JSON_BYTES)
        number = "9" * self.writer.INT_MAX_CHARS
        argv = finalize_argv(
            cell.dir, os.path.join(self.root, "absent-trace"),
            **{"--lane": field, "--case": field, "--expected-ingress": field, "--connection-mode": field,
               "--turns": number, "--exit": number, "--driver-rc": number, "--rig-rc": number},
        ) + ["--trace-live", "--snapshot", f"health={status}", "--snapshot", f"usage={status}"]
        subprocess.run(argv, check=True, capture_output=True)

        size = os.path.getsize(cell.path(evidence.RESULT_NAME))
        result = evidence.read_cell(cell.out)

        self.assertLessEqual(size, self.writer.RESULT_MAX_BYTES)
        self.assertGreater(size, self.writer.RESULT_MAX_BYTES // 2, "the fixture is not near the bound")
        assert isinstance(result, Incomplete), result
        self.assertEqual(result.reason, IncompleteReason.ARTIFACT_MISSING)

    def test_one_byte_over_any_field_cap_is_refused_by_the_writer(self) -> None:
        field = self.string_at(self.writer.FIELD_MAX_JSON_BYTES, extra=1)
        status = self.string_at(self.writer.STATUS_MAX_JSON_BYTES, extra=1)
        number = "9" * (self.writer.INT_MAX_CHARS + 1)
        rows = [(flag, {flag: field}, []) for flag in ("--lane", "--case", "--expected-ingress", "--connection-mode")]
        rows += [(flag, {flag: number}, []) for flag in ("--turns", "--exit", "--driver-rc", "--rig-rc")]
        rows += [("--snapshot", {}, ["--snapshot", f"health={status}"])]
        rows += [("non-ascii lane", {"--lane": "\u00e9" * (self.writer.FIELD_MAX_JSON_BYTES // 6 + 1)}, [])]
        rows += [("the 64 KiB lane", {"--lane": "l" * 65536}, [])]
        for name, overrides, extra in rows:
            with self.subTest(name):
                directory = os.path.join(self.root, name.replace(" ", "-").strip("-"))
                os.makedirs(directory)
                trace = os.path.join(self.root, "trace.log")
                with open(trace, "w", encoding="utf-8") as handle:
                    handle.write(TRACE)

                proc = subprocess.run(finalize_argv(directory, trace, **overrides) + extra, capture_output=True, text=True)

                self.assertNotEqual(proc.returncode, 0)
                self.assertIn("field-too-long", proc.stderr)
                self.assertEqual(os.listdir(directory), [])


def _rust_const(path: str, name: str) -> int:
    with open(path, encoding="utf-8") as handle:
        match = re.search(rf"pub const {name}: u32 = (\d+);", handle.read())
    if match is None:
        raise AssertionError(f"{name} not found in {path}")
    return int(match.group(1))


def _rust_struct_fields(path: str, struct: str) -> list:
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    body = re.search(rf"struct {struct} \{{(.*?)\n\}}", text, re.DOTALL)
    if body is None:
        raise AssertionError(f"struct {struct} not found in {path}")
    return re.findall(r"^\s+(?:pub(?:\([a-z]+\))? )?([a-z_]+):", body.group(1), re.MULTILINE)


@unittest.skipUnless(os.path.isdir(STATUS_SRC), "crates/ absent: no source to weld against")
class WeldTests(unittest.TestCase):
    """The reader's replicas of the daemon's and the writer's spellings."""

    def test_layout_names_come_from_the_writer(self) -> None:
        sys.path.insert(0, os.path.dirname(WRITER))
        import cell_evidence

        self.assertEqual(evidence.RESULT_SCHEMA, cell_evidence.SCHEMA)
        self.assertEqual(set(evidence.TRACE_ARTIFACTS), set(cell_evidence.TRACE_ARTIFACTS))
        self.assertEqual(set(evidence.SNAPSHOT_ARTIFACTS), set(evidence.SNAPSHOT_SCHEMA_VERSIONS))

    def test_health_schema_version_matches_the_daemon(self) -> None:
        self.assertEqual(evidence.HEALTH_SCHEMA_VERSION, _rust_const(os.path.join(STATUS_SRC, "health.rs"), "SCHEMA_VERSION"))

    def test_usage_schema_version_matches_the_daemon(self) -> None:
        self.assertEqual(evidence.USAGE_SCHEMA_VERSION, _rust_const(os.path.join(STATUS_SRC, "usage.rs"), "SCHEMA_VERSION"))

    def test_wire_check_lists_match_the_daemon_rows(self) -> None:
        fields = _rust_struct_fields(os.path.join(STATUS_SRC, "wire_check_rows.rs"), "WireCheckRows")
        self.assertEqual(sorted(evidence.WIRE_CHECK_LISTS), sorted(f for f in fields if f != "window_secs"))

    def test_health_carries_wire_check_and_usage_carries_totals(self) -> None:
        self.assertIn("wire_check", _rust_struct_fields(os.path.join(STATUS_SRC, "health.rs"), "HealthPanel"))
        self.assertIn("totals", _rust_struct_fields(os.path.join(STATUS_SRC, "usage.rs"), "UsagePanel"))

    def test_the_test_health_body_carries_every_daemon_row(self) -> None:
        fields = _rust_struct_fields(os.path.join(STATUS_SRC, "wire_check_rows.rs"), "WireCheckRows")
        self.assertEqual(sorted(health_body()["data"]["wire_check"]), sorted(fields))


if __name__ == "__main__":
    unittest.main()
