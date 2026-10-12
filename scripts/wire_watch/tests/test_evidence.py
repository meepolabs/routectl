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
import subprocess
import sys
import tempfile
import unittest
from typing import Optional

HERE = os.path.dirname(os.path.abspath(__file__))
WIRE_WATCH = os.path.dirname(HERE)
REPO = os.path.abspath(os.path.join(WIRE_WATCH, "..", ".."))
WRITER = os.path.join(REPO, "scripts", "drivers", "lib", "cell_evidence.py")
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

    def test_ingress_kinds_are_readable_from_an_incomplete_cell(self) -> None:
        cell = make_cell(self.root, health_status="http 500")
        self.assertIsInstance(evidence.read_cell(cell.out), Incomplete)

        self.assertEqual(evidence.read_ingress_kinds(cell.dir), ("anthropic",))


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
