"""report.py and classify.py over run directories built by the real writers.

Every run.json here comes from ledger.RunLedger and every .cell-evidence/
from scripts/drivers/lib/cell_evidence.py (see report_cells.py). The
planted-drop health snapshots are the ones RECORDED from a real hermetic
daemon by planted_drop.test.sh --record, copied byte for byte.

Upstream error samples and where each comes from:
  Bedrock 403   the evidence never holds the AWS body: routectl replaces a
                Bedrock 403 body with its own action-only string before the
                DEBUG line (crates/routectl-providers/src/aws_error.rs,
                access_denied_message / sanitized_debug_body). Status 403
                AccessDeniedException per
                https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html
  Mantle 404    the Mantle Anthropic vocabulary answers with the Anthropic
                error envelope; shape and not_found_error quoted from
                https://platform.claude.com/docs/en/api/errors ("The
                requested resource could not be found."); 404
                ResourceNotFound per
                https://docs.aws.amazon.com/bedrock/latest/userguide/troubleshooting-api-error-codes.html
  400 invalid   {"message":"The provided model identifier is invalid."},
  model id      recorded from bedrock-runtime and kept in
                crates/routectl-router/tests/fixtures/bedrock_validation_capture.json
                (supp-bad-model-id)
  400 other     {"__type":"com.amazon.coral.validate#ValidationException",
                "message":"The parameter response_schema is not supported
                for this model."} from
                crates/routectl-router/src/router/learn_capture_tests.rs;
                a ValidationException that mentions "model" yet is a wire
                result, not a refusal
  401 / 403     Anthropic envelope types authentication_error /
  non-Bedrock   permission_error from
                https://platform.claude.com/docs/en/api/errors
  Bedrock 401   NotAuthorized, HTTP Status Code 401, per
                https://docs.aws.amazon.com/bedrock/latest/APIReference/CommonErrors.html;
                the rule keys on status alone, so the body in that test (the
                Anthropic 401 envelope) is not asserted on
"""

from __future__ import annotations

import ast
import base64
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from datetime import datetime, timedelta, timezone
from dataclasses import replace
from typing import Any, Dict, List, Optional, Sequence, Tuple

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import report_cells as rc  # noqa: E402
from report_cells import CellPlan, plant  # noqa: E402

import classify  # noqa: E402
import matrix  # noqa: E402
import legs  # noqa: E402
import mdtext  # noqa: E402
import refusal  # noqa: E402
import report  # noqa: E402
import ledger  # noqa: E402
from ledger import CellState, Outcome, RunStop, Tokens  # noqa: E402

SCRUB = os.path.join(rc.REPO, "scripts", "scrub-fixture.sh")
GENERATED = "2026-10-12T18:00:00Z"
CORE_WIRE_CHECK = os.path.join(rc.REPO, "crates", "routectl-core", "src", "wire_check")
CAPTURE_FIXTURES = os.path.join(rc.REPO, "scripts", "capture_fixtures.sh")

BEDROCK_403 = "bedrock access denied: missing IAM action bedrock:InvokeModel"
MANTLE_404 = '{"type":"error","error":{"type":"not_found_error","message":"The requested resource could not be found."}}'
INVALID_MODEL_400 = '{"message":"The provided model identifier is invalid."}'
SCHEMA_400 = (
    '{"__type":"com.amazon.coral.validate#ValidationException",'
    '"message":"The parameter response_schema is not supported for this model."}'
)
AUTH_401 = '{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}'
PERMISSION_403 = '{"type":"error","error":{"type":"permission_error","message":"you do not have permission"}}'


def _with_loss(data: dict) -> None:
    data["wire_check"] = rc.recorded_doc("request-leg-plant")["data"]["wire_check"]


RING_LANE = "responses->openai-responses"


def ring_health(counters: Sequence[Tuple[str, int]], retained: Sequence[Tuple[str, int]], lane: str = RING_LANE,
                counter_lane: Optional[str] = None) -> str:
    """request-leg-plant with its loss counters and retained loss findings
    replaced: `counters` are (path_class, count) rows, `retained` are
    (path_class, how many findings) with one distinct indexed path each,
    the shape a long tool session leaves once the daemon's 128-entry
    finding ring is full."""
    def edit(data: dict) -> None:
        wc = data["wire_check"]
        template = wc["loss_findings"][0]
        wc["loss"] = [dict(wc["loss"][0], lane=counter_lane or lane, path_class=cls, count=n)
                      for cls, n in counters]
        wc["loss_findings"] = [dict(template, lane=lane, path_class=cls, path=f"/input/{i}/name")
                               for cls, n in retained for i in range(n)]

    return rc.edited_health("request-leg-plant", edit)


KEY_FILE_NAME = "bedrock-api-key.us-west-2"


def converse(**kw: Any) -> CellPlan:
    base = CellPlan(driver="claude-code-print.sh", ingress="anthropic", lane="bedrock-converse",
                    health=rc.recorded("clean-control"), key_file=KEY_FILE_NAME)
    return replace(base, **kw)


class RunCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.fresh()

    def fresh(self) -> None:
        """A new, empty run dir: one per subTest."""
        self.run_dir = tempfile.mkdtemp(prefix="run-", dir=self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def build(self, plans: Sequence[CellPlan], key_dir: Optional[str] = None, **kw: object) -> report.Built:
        rc.build_run(self.run_dir, plans, **kw)  # type: ignore[arg-type]
        ctx = report.context(rc.PATHS, key_dir)
        return report.build(self.run_dir, rc.PATHS, ctx, GENERATED)

    def one(self, plan: CellPlan, key_dir: Optional[str] = None) -> classify.CellResult:
        return self.build([plan], key_dir).results[0]

    def findings(self, built: report.Built) -> List[dict]:
        return [json.loads(line) for line in built.files[report.FINDINGS_JSONL].splitlines()]


class RecordedPlantTests(RunCase):
    def test_request_leg_plant_yields_one_request_row(self) -> None:
        built = self.build([plant("request-leg-plant")])

        rows = self.findings(built)

        self.assertEqual([(r["lane"], r["leg"], r["path_class"], r["example_paths"]) for r in rows],
                         [("responses->openai-responses", "request", "missing", ["/temperature"])])
        self.assertEqual(rows[0]["count"], 1)

    def test_response_leg_plant_yields_one_response_row(self) -> None:
        rows = self.findings(self.build([plant("response-leg-plant")]))

        self.assertEqual([(r["lane"], r["leg"], r["example_paths"]) for r in rows],
                         [("chat->openai-compat", "response", ["/system_fingerprint"])])

    def test_clean_control_passes_with_no_rows(self) -> None:
        built = self.build([plant("clean-control")])

        self.assertEqual(self.findings(built), [])
        self.assertTrue(built.complete)
        self.assertIn("| PASS |", built.files[report.REPORT_MD])

    def test_planted_cells_are_findings_despite_the_rig_refusal_exit(self) -> None:
        result = self.one(plant("request-leg-plant", rc=5))

        self.assertEqual(result.outcome, Outcome.FINDING)

    def test_a_refused_capture_leaves_every_leg_uncaptured(self) -> None:
        result = self.one(plant("request-leg-plant"))

        self.assertEqual(result.legs, (False, False, False, False))


class IncompleteEvidenceTests(RunCase):
    def assert_incomplete(self, plan: CellPlan, reason: str) -> None:
        built = self.build([plan])
        result = built.results[0]
        self.assertEqual((result.outcome, result.reason), (Outcome.INCOMPLETE, reason))
        self.assertFalse(built.complete)
        self.assertIn("Status: INCOMPLETE", built.files[report.REPORT_MD])

    def test_missing_health_json_is_incomplete(self) -> None:
        self.assert_incomplete(plant("clean-control", health_status="http 500"), "artifact-missing")

    def test_unknown_health_schema_version_is_incomplete(self) -> None:
        doc = rc.recorded_doc("clean-control")
        doc["schema_version"] = doc["schema_version"] + 1
        self.assert_incomplete(plant("clean-control", health=json.dumps(doc)), "snapshot-schema")

    def test_truncated_health_json_is_incomplete(self) -> None:
        text = rc.recorded("clean-control")
        self.assert_incomplete(plant("clean-control", health=text[: len(text) // 2]), "artifact-corrupt")

    def test_no_evidence_directory_is_incomplete(self) -> None:
        self.assert_incomplete(plant("clean-control", health=None), "no-evidence")

    def test_a_run_with_no_check_rows_is_not_a_pass(self) -> None:
        def no_checks(data: dict) -> None:
            data["wire_check"]["checks"] = []

        self.assert_incomplete(plant("clean-control", health=rc.edited_health("clean-control", no_checks)), "wire-check")

    def test_only_zero_count_clean_checks_is_not_a_pass(self) -> None:
        def zero_checks(data: dict) -> None:
            data["wire_check"]["checks"] = [dict(data["wire_check"]["checks"][0], count=0)]

        self.assert_incomplete(plant("clean-control", health=rc.edited_health("clean-control", zero_checks)),
                               "wire-check")

    def test_retained_findings_the_loss_counters_do_not_cover_are_incomplete(self) -> None:
        cases = [
            ("counter-below-retained", [("missing", 1)], [("missing", 2)], "1 losses counted, 2 findings retained"),
            ("retained-without-counter", [], [("missing", 1)], "0 losses counted, 1 findings retained"),
            ("zero-counter", [("missing", 0)], [("missing", 1)], "0 losses counted, 1 findings retained"),
            ("one-class-short-other-over", [("missing", 200), ("added", 4)], [("missing", 128), ("added", 5)],
             "204 losses counted, 133 findings retained"),
        ]
        for name, counters, retained, detail in cases:
            with self.subTest(name):
                self.fresh()
                built = self.build([plant("request-leg-plant", health=ring_health(counters, retained))])
                result = built.results[0]
                self.assertEqual((result.outcome, result.reason, result.detail),
                                 (Outcome.INCOMPLETE, "findings-truncated", detail + " (ring or window)"))
                self.assertFalse(built.complete)

    def test_a_path_outside_the_daemon_pointer_grammar_is_incomplete(self) -> None:
        hostile = (
            "/x_www.evil.com/login", "/[x](https://evil.example)", "/a b", "/\x1b]8;;x\x07", "x",
            "/", "//a", "/" + "a" * 33, "/a" * 9, "/$schema", "/-a", "/" + "1" * 21,
        )
        for path in hostile:
            with self.subTest(path):
                self.fresh()

                def plant_path(data: dict) -> None:
                    data["wire_check"]["loss_findings"][0]["path"] = path

                self.assert_incomplete(
                    plant("request-leg-plant", health=rc.edited_health("request-leg-plant", plant_path)),
                    "snapshot-shape")

    def test_paths_inside_the_daemon_pointer_grammar_are_read(self) -> None:
        for path in ("/temperature", "/system_fingerprint", "", "/tools/0/input_schema/properties/*",
                     "/tools/12/input_schema/$defs/*/$ref", "/a" * 8, "/" + "a" * 32):
            with self.subTest(path):
                self.assertTrue(classify.is_wire_path(path))

    def test_an_unknown_path_class_is_incomplete_not_a_finding(self) -> None:
        def odd_class(data: dict) -> None:
            data["wire_check"]["loss_findings"][0]["path_class"] = "vanished"

        self.assert_incomplete(plant("request-leg-plant", health=rc.edited_health("request-leg-plant", odd_class)),
                               "snapshot-shape")

    def test_a_bound_stopped_or_unfinished_cell_makes_the_run_incomplete(self) -> None:
        plans = [
            plant("clean-control"),
            CellPlan(driver="codex.sh", ingress="openai-responses", lane="anthropic-api", state=CellState.TIMEOUT),
            CellPlan(driver="pi-chat.sh", ingress="openai", lane="anthropic-api", state=CellState.RUNNING),
        ]
        built = self.build(plans)

        self.assertFalse(built.complete)
        text = built.files[report.REPORT_MD]
        self.assertIn("- 1 cell(s) TIMEOUT", text)
        self.assertIn("- 1 cell(s) INCOMPLETE", text)

    def test_run_json_from_another_cells_tsv_makes_the_run_incomplete(self) -> None:
        current = rc.matrix.run_inputs(rc.PATHS)
        stale = rc.ledger.RunInputs(cells_sha256="0" * 64, models_sha256=current.models_sha256, cases=current.cases)

        built = self.build([plant("clean-control")], inputs=stale)

        self.assertFalse(built.complete)
        self.assertIn("different `cells.tsv`", built.files[report.REPORT_MD])


class RingOverflowTests(RunCase):
    """The daemon bumps rc_wire_loss_total{lane,path_class} and pushes one
    finding per loss into a ring that keeps the newest 128 per kind
    (crates/routectl-router/src/wire_check_state.rs), so a long session's
    counters run past its retained findings."""

    def test_counters_past_the_retained_findings_are_a_finding_whose_totals_match_the_counters(self) -> None:
        health = ring_health([("missing", 200), ("added", 5)], [("missing", 128), ("added", 5)])

        built = self.build([plant("request-leg-plant", health=health)])

        self.assertEqual(built.results[0].outcome, Outcome.FINDING)
        totals: Dict[Tuple[str, str], int] = {}
        for row in self.findings(built):
            key = (row["lane"], row["path_class"])
            totals[key] = totals.get(key, 0) + row["count"]
        self.assertEqual(totals, {(RING_LANE, "missing"): 200, (RING_LANE, "added"): 5})

    def test_each_short_counter_gets_one_unknown_leg_row_without_example_paths(self) -> None:
        other_lane = "chat->openai-compat"
        cases = [
            ("one-class-short", RING_LANE, [("missing", 200), ("added", 5)], [("missing", 128), ("added", 5)],
             [("missing", 72)]),
            ("nothing-retained", RING_LANE, [("missing", 129)], [], [("missing", 129)]),
            ("two-classes-short", RING_LANE, [("missing", 300), ("changed", 40)],
             [("missing", 100), ("changed", 28)], [("changed", 12), ("missing", 200)]),
            ("lane-from-the-counter", other_lane, [("added", 9)], [], [("added", 9)]),
            ("count-at-the-bound", RING_LANE, [("missing", classify.COUNT_MAX)], [],
             [("missing", 2**32)]),
        ]
        for name, counter_lane, counters, retained, expected in cases:
            with self.subTest(name):
                self.fresh()
                health = ring_health(counters, retained, counter_lane=counter_lane)
                rows = self.findings(self.build([plant("request-leg-plant", health=health)]))
                overflow = [(r["lane"], r["path_class"], r["count"], r["example_paths"])
                            for r in rows if r["leg"] == classify.LEG_UNKNOWN]
                self.assertEqual(overflow, [(counter_lane, cls, n, []) for cls, n in expected])

    def test_a_count_past_the_bound_is_refused(self) -> None:
        def check_count(data: dict) -> None:
            data["wire_check"]["checks"][0]["count"] = 2**32 + 1

        def refusal_count(data: dict) -> None:
            data["wire_check"]["local_refusals"] = [{"lane": RING_LANE, "count": 2**32 + 1,
                                                     "last_seen_unix_secs": 1}]

        cases = [
            ("loss-past-2-32", ring_health([("missing", 2**32 + 1)], []), str(2**32 + 1)),
            ("loss-past-2-53", ring_health([("missing", 2**53 + 1)], []), str(2**53 + 1)),
            ("duplicate-loss-rows-summing-past-the-bound",
             ring_health([("missing", 2**32), ("missing", 2**32)], []), str(2**33)),
            ("checks-past-2-32", rc.edited_health("request-leg-plant", check_count), str(2**32 + 1)),
            ("local-refusals-past-2-32", rc.edited_health("request-leg-plant", refusal_count), str(2**32 + 1)),
        ]
        for name, health, absent in cases:
            with self.subTest(name):
                self.fresh()
                built = self.build([plant("request-leg-plant", health=health)])
                result = built.results[0]
                self.assertEqual((result.outcome, result.reason), (Outcome.INCOMPLETE, "snapshot-shape"))
                self.assertNotIn(absent, built.files[report.FINDINGS_JSONL])

    def test_counters_matching_the_retained_findings_add_no_overflow_row(self) -> None:
        built = self.build([plant("request-leg-plant", health=ring_health([("missing", 3)], [("missing", 3)]))])

        rows = self.findings(built)

        self.assertEqual([(r["leg"], r["count"]) for r in rows], [("request", 3)])
        self.assertIsNone(built.results[0].detail)

    def test_an_overflow_cell_leaves_the_report_complete_and_names_retained_of_counted(self) -> None:
        overflow = plant("request-leg-plant",
                         health=ring_health([("missing", 200), ("added", 5)], [("missing", 128), ("added", 5)]))

        built = self.build([overflow, plant("clean-control")])

        self.assertTrue(built.complete)
        text = built.files[report.REPORT_MD]
        self.assertIn("Status: COMPLETE", text)
        row = next(line for line in text.splitlines() if line.startswith("| " + mdtext.esc(overflow.cell_id) + " |"))
        self.assertIn("| FINDING |", row)
        self.assertIn(mdtext.esc("133 of 205 losses detailed (finding ring or status window)"), row)
        self.assertIn(f"| {mdtext.esc(RING_LANE)} | missing | unknown | 72 |  | ", text)

    def test_overflow_counter_labels_outside_the_daemon_vocabulary_are_refused_and_shown_nowhere(self) -> None:
        cases = [
            ("pipe-lane", "responses->x|y", "missing"),
            ("esc-lane", "responses->x\x1b]0;owned\x07", "missing"),
            ("newline-lane", "responses->x\n| forged |", "missing"),
            ("unknown-class", RING_LANE, "vanished|\x1b[31m"),
        ]
        for name, lane, path_class in cases:
            with self.subTest(name):
                self.fresh()

                def hostile(data: dict) -> None:
                    data["wire_check"]["loss"].append({"lane": lane, "path_class": path_class, "count": 7,
                                                       "last_seen_unix_secs": 1})

                health = rc.edited_health("request-leg-plant", hostile)
                self.assertIn(json.dumps(lane)[1:-1], health)
                self.assertIn(json.dumps(path_class)[1:-1], health)
                built = self.build([plant("request-leg-plant", health=health)])
                result = built.results[0]
                self.assertEqual((result.outcome, result.reason), (Outcome.INCOMPLETE, "snapshot-shape"))
                for text in built.files.values():
                    self.assertNotIn("\x1b", text)
                    self.assertNotIn("vanished", text)
                    self.assertNotIn("responses->x", text)
                    self.assertNotIn("forged", text)

    def test_rules_six_and_seven_still_win_over_an_overflow(self) -> None:
        health = ring_health([("missing", 200)], [("missing", 128)])
        cases = [
            ("refusal", converse(health=health, errors=(rc.error_line("bedrock-converse", 403, BEDROCK_403),)),
             (Outcome.NOT_RUN, "model-access")),
            ("ingress", plant("request-leg-plant", health=health, trace_ingress="anthropic"),
             (Outcome.FAILED, "ingress")),
        ]
        for name, plan, expected in cases:
            with self.subTest(name):
                self.fresh()
                built = self.build([plan])
                self.assertEqual((built.results[0].outcome, built.results[0].reason), expected)
                self.assertEqual(self.findings(built), [])


class FindingTests(RunCase):
    def test_a_newly_learned_verdict_is_a_finding(self) -> None:
        self.assertEqual(self.one(plant("clean-control", learned=1)).outcome, Outcome.FINDING)

    def test_a_nonzero_parser_unlocalized_counter_is_a_finding(self) -> None:
        self.assertEqual(self.one(plant("clean-control", unlocalized=2)).outcome, Outcome.FINDING)

    def test_an_unrepaired_rejection_is_a_finding_row(self) -> None:
        def rejected(data: dict) -> None:
            data["wire_check"]["unrepaired_rejections"] = [{
                "lane": "chat->openai-compat", "leg": "request", "path": "/tools", "provenance": "client_unchanged",
                "field_repair_outcome": "unrepaired", "ts_unix_secs": 1,
            }]

        built = self.build([plant("clean-control", health=rc.edited_health("clean-control", rejected))])

        self.assertEqual([(r["path_class"], r["leg"]) for r in self.findings(built)], [("unrepaired", "request")])

    def test_a_local_refusal_is_a_finding(self) -> None:
        def refused(data: dict) -> None:
            data["wire_check"]["local_refusals"] = [{"lane": "chat->openai-compat", "count": 1, "last_seen_unix_secs": 1}]

        self.assertEqual(self.one(plant("clean-control", health=rc.edited_health("clean-control", refused))).outcome,
                         Outcome.FINDING)

    def test_traced_ingress_other_than_expected_is_failed(self) -> None:
        result = self.one(plant("clean-control", trace_ingress="anthropic"))

        self.assertEqual((result.outcome, result.reason), (Outcome.FAILED, "ingress"))

    def test_a_driver_failure_with_a_clean_snapshot_is_failed(self) -> None:
        result = self.one(plant("clean-control", driver_rc=1))

        self.assertEqual((result.outcome, result.reason), (Outcome.FAILED, "driver"))


class RefusalRuleTests(RunCase):
    def test_model_access_refusals_are_not_run_and_hide_their_wire_rows(self) -> None:
        cases = [
            ("bedrock-403", converse(errors=(rc.error_line("bedrock-converse", 403, BEDROCK_403),))),
            ("mantle-404", converse(lane="bedrock-mantle", key_file="bedrock-api-key.us-east-1",
                                    errors=(rc.error_line("anthropic", 404, MANTLE_404),))),
            ("invalid-model-400", converse(errors=(rc.error_line("bedrock-converse", 400, INVALID_MODEL_400),))),
        ]
        for name, plan in cases:
            with self.subTest(name):
                plan.health = rc.edited_health("clean-control", _with_loss)
                self.fresh()
                built = self.build([plan])
                result = built.results[0]
                self.assertEqual((result.outcome, result.reason), (Outcome.NOT_RUN, "model-access"))
                self.assertEqual(self.findings(built), [])

    def test_bedrock_401_is_a_credential_failure(self) -> None:
        result = self.one(converse(errors=(rc.error_line("bedrock-converse", 401, AUTH_401),)))

        self.assertEqual((result.outcome, result.reason), (Outcome.FAILED, "credential"))

    def test_non_bedrock_403_is_failed(self) -> None:
        result = self.one(plant("clean-control", errors=(rc.error_line("openai-compat", 403, PERMISSION_403),)))

        self.assertEqual((result.outcome, result.reason), (Outcome.FAILED, "permission"))

    def test_a_validation_error_that_does_not_name_the_model_is_not_a_refusal(self) -> None:
        plan = converse(errors=(rc.error_line("bedrock-converse", 400, SCHEMA_400),),
                        health=rc.edited_health("clean-control", _with_loss))

        self.assertEqual(self.one(plan).outcome, Outcome.FINDING)

    def test_the_invalid_model_phrase_on_a_non_bedrock_lane_is_not_a_refusal(self) -> None:
        plan = plant("clean-control", errors=(rc.error_line("openai-compat", 400, INVALID_MODEL_400),))

        self.assertEqual(self.one(plan).outcome, Outcome.PASS)


def short_term_key(signed: datetime, seconds: int) -> str:
    query = "&".join([
        "Action=CallWithBearerToken", "X-Amz-Algorithm=AWS4-HMAC-SHA256",
        "X-Amz-Credential=TESTONLY%2F20261012%2Fus-west-2%2Fbedrock%2Faws4_request",
        "X-Amz-Date=" + signed.strftime("%Y%m%dT%H%M%SZ"), f"X-Amz-Expires={seconds}",
        "X-Amz-SignedHeaders=host", "X-Amz-Signature=" + "0" * 64,
    ])
    url = "bedrock.amazonaws.com/?" + query + "&Version=1"
    return refusal.SHORT_TERM_PREFIX + base64.b64encode(url.encode("ascii")).decode("ascii")


class KeyExpiryTests(RunCase):
    def key_dir(self, token: str) -> str:
        directory = os.path.join(self._tmp.name, "keys")
        os.mkdir(directory, 0o700)
        path = os.path.join(directory, KEY_FILE_NAME)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as handle:
            handle.write(token + "\n")
        return directory

    def test_a_403_with_a_key_past_its_encoded_expiry_is_a_credential_failure(self) -> None:
        keys = self.key_dir(short_term_key(datetime(2020, 1, 1, tzinfo=timezone.utc), 900))
        plan = converse(errors=(rc.error_line("bedrock-converse", 403, BEDROCK_403),))

        result = self.one(plan, key_dir=keys)

        self.assertEqual((result.outcome, result.reason), (Outcome.FAILED, "credential"))

    def test_a_403_with_a_live_key_stays_model_access(self) -> None:
        keys = self.key_dir(short_term_key(datetime.now(timezone.utc), 43200))
        plan = converse(errors=(rc.error_line("bedrock-converse", 403, BEDROCK_403),))

        result = self.one(plan, key_dir=keys)

        self.assertEqual((result.outcome, result.reason), (Outcome.NOT_RUN, "model-access"))

    def far_future_cell(self, keys: str) -> classify.CellResult:
        """A Bedrock 403 cell whose client_result.json mtime is 1e12 (year
        33658), as a container can set it. Built on tmpfs, which stores it;
        a filesystem that clamps it skips the test rather than pass."""
        shm = "/dev/shm"
        if not os.path.isdir(shm) or not os.access(shm, os.W_OK):
            self.skipTest("no writable tmpfs at /dev/shm")
        self.run_dir = tempfile.mkdtemp(prefix="run-", dir=shm)
        self.addCleanup(shutil.rmtree, self.run_dir, True)
        plan = converse(errors=(rc.error_line("bedrock-converse", 403, BEDROCK_403),))
        rc.build_run(self.run_dir, [plan])
        result_json = os.path.join(self.run_dir, "cells", plan.cell_id, rc.evidence.EVIDENCE_DIRNAME,
                                   rc.evidence.RESULT_NAME)
        os.utime(result_json, (1e12, 1e12))
        if os.stat(result_json).st_mtime != 1e12:
            self.skipTest("this tmpfs clamps the mtime")
        return report.build(self.run_dir, rc.PATHS, report.context(rc.PATHS, keys), GENERATED).results[0]

    def test_a_far_future_evidence_mtime_reads_as_no_end_time(self) -> None:
        keys = self.key_dir(short_term_key(datetime(2020, 1, 1, tzinfo=timezone.utc), 900))

        result = self.far_future_cell(keys)

        self.assertEqual((result.outcome, result.reason), (Outcome.NOT_RUN, "model-access"))

    def test_an_unreadable_mtime_reads_as_no_end_time(self) -> None:
        plan = plant("clean-control")
        rc.build_run(self.run_dir, [plan])
        cell_dir = os.path.join(self.run_dir, "cells", plan.cell_id)
        self.assertIsNotNone(classify._cell_end(cell_dir))
        for name, exc in (("overflow", OverflowError()), ("value", ValueError()), ("os", OSError(75, "EOVERFLOW"))):
            with self.subTest(name):
                def boom(_seconds: float, error: Exception = exc) -> datetime:
                    raise error

                with mock.patch.object(classify, "_utc_from_timestamp", boom):
                    self.assertIsNone(classify._cell_end(cell_dir))

    def test_decode_expiry_reads_the_signing_time_plus_validity(self) -> None:
        signed = datetime(2026, 10, 12, 1, 2, 3, tzinfo=timezone.utc)

        self.assertEqual(refusal.decode_expiry(short_term_key(signed, 900)), signed + timedelta(seconds=900))

    def test_decode_expiry_refuses_other_shapes(self) -> None:
        for name, token in [
            ("long-term", "ABSK" + "Q" * 120),
            ("not base64", refusal.SHORT_TERM_PREFIX + "%%%"),
            ("no query", refusal.SHORT_TERM_PREFIX + base64.b64encode(b"bedrock.amazonaws.com/").decode()),
            ("over seven days", short_term_key(datetime(2026, 1, 1, tzinfo=timezone.utc), 8 * 86400)),
            ("expiry past year 9999", short_term_key(datetime(9999, 12, 31, 23, 59, 59, tzinfo=timezone.utc), 604800)),
        ]:
            with self.subTest(name):
                self.assertIsNone(refusal.decode_expiry(token))


class CoverageTests(RunCase):
    def pairing(self, built: report.Built, harness: str, ingress: str) -> str:
        for line in built.files[report.REPORT_MD].splitlines():
            if line.startswith(f"| {harness} | {ingress} |"):
                return line
        self.fail(f"no coverage row for {harness} x {ingress}")

    def test_not_run_alone_leaves_a_pairing_uncovered(self) -> None:
        plans = [
            CellPlan(driver="pi-chat.sh", ingress="openai", lane="anthropic-api",
                     state=CellState.NOT_RUN, reason="credential", detail="missing"),
            CellPlan(driver="pi-chat.sh", ingress="openai", lane="openai-responses",
                     state=CellState.NOT_RUN, reason="driver", detail="missing"),
        ]

        line = self.pairing(self.build(plans), "pi-chat", "openai")

        self.assertIn("| no |", line)

    def test_one_passing_egress_covers_the_pairing(self) -> None:
        plans = [
            plant("clean-control"),
            CellPlan(driver="hermes-chat.sh", ingress="openai", lane="anthropic-api",
                     state=CellState.NOT_RUN, reason="credential", detail="missing"),
        ]

        line = self.pairing(self.build(plans), "hermes-chat", "openai")

        self.assertIn("| yes | openai\\-compat\\x2eollama |", line)

    def test_failed_alone_leaves_a_pairing_uncovered(self) -> None:
        line = self.pairing(self.build([plant("clean-control", driver_rc=1)]), "hermes-chat", "openai")

        self.assertIn("| no |", line)

    def test_every_cells_tsv_pairing_has_a_row(self) -> None:
        built = self.build([plant("clean-control")])
        pairs = {(r.driver[:-3], r.expected_ingress) for r in rc.matrix.load_rows(rc.PATHS)}

        for harness, ingress in sorted(pairs):
            with self.subTest(f"{harness} x {ingress}"):
                self.pairing(built, harness, ingress)


class LegCoverageTests(RunCase):
    def test_stream_legs_a_converse_egress_does_not_trace_show_as_not_captured(self) -> None:
        plan = converse(legs=legs.LEG_FILES[:2], fixture_lane="bedrock")

        result = self.one(plan)

        self.assertEqual(result.legs, (True, True, False, False))

    def test_all_four_legs_found_under_the_rig_lane_directory(self) -> None:
        plan = plant("clean-control", legs=legs.LEG_FILES, fixture_lane="openai-compat")

        self.assertEqual(self.one(plan).legs, (True, True, True, True))

    def test_a_symlinked_leg_is_not_captured(self) -> None:
        plan = plant("clean-control", legs=legs.LEG_FILES[1:], fixture_lane="openai-compat")
        rc.build_run(self.run_dir, [plan])
        target = os.path.join(self._tmp.name, "outside.json")
        with open(target, "w", encoding="utf-8") as handle:
            handle.write("{}\n")
        os.symlink(target, os.path.join(self.run_dir, "cells", plan.cell_id, "openai-compat", rc.CASE,
                                        legs.LEG_FILES[0]))

        captured = legs.legs_captured(os.path.join(self.run_dir, "cells", plan.cell_id), rc.CASE)

        self.assertEqual(captured, (False, True, True, True))

    def test_the_report_names_each_cell_leg(self) -> None:
        text = self.build([converse(legs=legs.LEG_FILES[:2], fixture_lane="bedrock")]).files[report.REPORT_MD]

        self.assertIn("| yes / yes / no / no |", text)


# Text GitHub's file-view renderer links (or typesets as math) even when
# each punctuation character is backslash-escaped, with the escaped form
# that renders inert.
AUTOLINK_ROWS = (
    ("contact admin@evil.example for access", r"contact admin\x40evil\x2eexample for access"),
    ("https://evil.com", r"https\x3a\/\/evil\x2ecom"),
    ("www.x", r"www\x2ex"),
    ("#12", r"\x2312"),
    ("$x$", r"\x24x\x24"),
)
# A value cut at the excerpt cap right after " www": the suffix esc appends
# must not complete "www." into a link.
CAP_BOUNDARY_WWW = "x" * (report.EXCERPT_CAP - 4) + " www" + ".evil.com/login"
# Characters that open or extend an autolink, a math span or an escape.
LINK_CHARS = frozenset(".:@#$\\")

HOSTILE = [
    ("esc-osc", "\x1b]0;owned\x07\x1b[31m"),
    ("pipe", "a|b"),
    ("backtick", "a`b"),
    ("newline", "a\nb"),
]


class HostileTextTests(RunCase):
    """Container-written text in each channel report.md shows."""

    def excerpt_cell(self, excerpt: str) -> CellPlan:
        row = json.dumps({"excerpt": excerpt, "provider_kind": "bedrock-converse", "status": 403})
        return converse(errors=(rc.error_line("bedrock-converse", 403, BEDROCK_403),),
                        extra_files={"upstream_errors.jsonl": row + "\n"})

    def path_cell(self, path: str) -> CellPlan:
        def hostile(data: dict) -> None:
            data["wire_check"]["loss_findings"][0]["path"] = path

        return plant("request-leg-plant", health=rc.edited_health("request-leg-plant", hostile))

    def assert_inert(self, text: str, raw: str) -> None:
        self.assertTrue(all(0x20 <= ord(c) < 0x7F for c in text.replace("\n", "")))
        self.assertNotIn(raw, text)

    def test_hostile_excerpts_render_inert(self) -> None:
        control = self.build([self.excerpt_cell("xy")]).files[report.REPORT_MD]
        for name, raw in HOSTILE:
            with self.subTest(name):
                self.fresh()
                text = self.build([self.excerpt_cell("x" + raw + "y")]).files[report.REPORT_MD]
                self.assert_inert(text, raw)
                self.assertEqual(len(text.splitlines()), len(control.splitlines()))

    def test_hostile_paths_are_refused_and_shown_nowhere(self) -> None:
        for name, raw in HOSTILE:
            with self.subTest(name):
                self.fresh()
                built = self.build([self.path_cell("/x" + raw)])

                self.assertEqual((built.results[0].outcome, built.results[0].reason),
                                 (Outcome.INCOMPLETE, "snapshot-shape"))
                self.assert_inert(built.files[report.REPORT_MD], raw)
                self.assertEqual(built.files[report.FINDINGS_JSONL], "")

    def test_link_shaped_excerpts_reach_the_cells_table_hex_escaped(self) -> None:
        for raw, expected in AUTOLINK_ROWS:
            with self.subTest(raw):
                self.fresh()
                text = self.build([self.excerpt_cell(raw)]).files[report.REPORT_MD]

                row = next(line for line in text.splitlines() if "upstream 403" in line)
                self.assertIn(expected, row)

    def test_a_cap_boundary_www_reaches_the_cells_table_unlinkable(self) -> None:
        text = self.build([self.excerpt_cell(CAP_BOUNDARY_WWW)]).files[report.REPORT_MD]

        row = next(line for line in text.splitlines() if "upstream 403" in line)
        self.assertIn(" www" + mdtext.TRUNCATED + " |", row)
        self.assertNotRegex(row, r"www[.:@]")

    def report_literals(self) -> List[str]:
        """Every lit() string a report over each cell shape emits, plus the
        suffix and separator esc and join add themselves."""
        literals: List[str] = []
        real_lit = report.lit

        def recording_lit(text: str) -> mdtext.Md:
            literals.append(text)
            return real_lit(text)

        plans = [
            plant("request-leg-plant"),
            plant("clean-control", model="a", driver_rc=1),
            plant("clean-control", model="b", trace_ingress="x"),
            plant("clean-control", model="c", health_status="http 500"),
            plant("clean-control", model="d", tokens=Tokens(1, 2, 3, 4)),
            converse(errors=(rc.error_line("bedrock-converse", 403, BEDROCK_403),)),
            CellPlan(driver="codex.sh", ingress="openai-responses", lane="anthropic-api", state=CellState.TIMEOUT),
        ]
        with mock.patch.object(report, "lit", recording_lit):
            self.build(plans, run_stop=RunStop.RUN_SIZE)
        self.assertIn(" | ", literals, "the recorder saw no table literal, so it proves nothing")
        return sorted(set(literals) | {mdtext.TRUNCATED, str(mdtext.JOIN_SEPARATOR)})

    def test_no_report_literal_can_complete_a_link_with_escaped_text(self) -> None:
        # Escaped text holds no raw . : @ # $ or backslash, so a link across a
        # literal/escaped boundary needs a literal to supply one at its edge;
        # a link wholly inside a literal needs www, a scheme, @, $, #N or a
        # dotted name, which literals keep inside code spans.
        with open(os.path.join(rc.WIRE_WATCH, "report.py"), encoding="utf-8") as handle:
            tree = ast.parse(handle.read())
        static = [node.args[0].value for node in ast.walk(tree)
                  if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "lit"
                  and node.args and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str)]
        self.assertIn("run stopped: ", static, "the static scan found no report literal, so it proves nothing")
        for text in sorted(set(static) | set(self.report_literals())):
            with self.subTest(text):
                inner = re.sub(r"^#{1,6} ", "", text)
                outside_code = re.sub(r"`[^`]*`", "", inner)
                self.assertNotIn(inner[:1], LINK_CHARS, "starts with a link character")
                self.assertNotIn(inner[-1:], LINK_CHARS, "ends with a link character")
                self.assertFalse(set("@$#\\") & set(inner))
                self.assertNotIn("www", inner.lower())
                self.assertNotIn("://", inner)
                self.assertNotIn(".", outside_code, "a dotted name outside a code span")

    def test_each_hostile_class_renders_as_escaped_text(self) -> None:
        text = self.build([self.excerpt_cell("x\x1b]0;t\x07|`\n")]).files[report.REPORT_MD]

        self.assertIn(r"x\x1b\]0\;t\x07\|\`\x0a", text)


def commonmark_text(md: str) -> str:
    """The text CommonMark shows for inline `md`: a backslash before ASCII
    punctuation is dropped, any other backslash is kept."""
    return re.sub(r"\\([!-/:-@\[-`{-~])", r"\1", md)


class EscaperTests(unittest.TestCase):
    def test_a_plain_str_line_is_refused(self) -> None:
        with self.assertRaisesRegex(ValueError, "did not come from the escaper"):
            mdtext.lines_text([mdtext.lit("ok"), "| raw |"])  # type: ignore[list-item]

    def test_a_plain_str_concatenation_loses_the_mark(self) -> None:
        with self.assertRaises(ValueError):
            mdtext.lines_text([mdtext.lit("a") + mdtext.esc("b")])  # type: ignore[list-item]

    def test_the_truncation_suffix_cannot_complete_a_link(self) -> None:
        out = mdtext.esc(CAP_BOUNDARY_WWW, report.EXCERPT_CAP)

        self.assertTrue(out.endswith(" www" + mdtext.TRUNCATED), out[-30:])
        self.assertFalse(LINK_CHARS & set(mdtext.TRUNCATED))
        self.assertTrue(mdtext.TRUNCATED[0].isspace())

    def test_untrusted_text_is_capped(self) -> None:
        out = mdtext.esc("x " * 500, cap=40)

        self.assertTrue(out.endswith(mdtext.TRUNCATED))
        self.assertLessEqual(len(out), 40 + len(mdtext.TRUNCATED))

    def test_a_literal_backslash_x_stays_distinct_from_an_escaped_byte(self) -> None:
        self.assertEqual(mdtext.esc("\\x1b \x1b"), "\\x5cx1b \\x1b")

    def test_every_rendered_backslash_starts_a_hex_escape(self) -> None:
        for raw in ("\\x2e", ".", "\\", "a\\b", "\\\\x2e", "\\-x", "a_b-c|d\\e"):
            with self.subTest(raw):
                self.assertRegex(commonmark_text(mdtext.esc(raw)), r"^(?:[^\\]|\\x[0-9a-f]{2})*$")

    def test_a_literal_hex_escape_and_the_character_render_differently(self) -> None:
        self.assertNotEqual(commonmark_text(mdtext.esc("\\x2e")), commonmark_text(mdtext.esc(".")))

    def test_non_str_input_is_refused(self) -> None:
        for value in (None, 5, b"ab", ["a"] * 300):
            with self.subTest(type(value).__name__):
                with self.assertRaises(TypeError):
                    mdtext.esc(value)  # type: ignore[arg-type]

    def test_link_and_math_characters_are_hex_escaped(self) -> None:
        for text, expected in AUTOLINK_ROWS + (
            ("x_www.evil.com", r"x\_www\x2eevil\x2ecom"),
            ("evil.com/path", r"evil\x2ecom\/path"),
            ("a.b-c.io", r"a\x2eb\-c\x2eio"),
        ):
            with self.subTest(text):
                self.assertEqual(mdtext.esc(text), expected)

    def test_plain_tokens_render_verbatim_and_others_do_not(self) -> None:
        for token in ("responses->openai-responses", "/temperature", "/tools/0/input_schema", "bedrock-converse"):
            with self.subTest(token):
                self.assertEqual(mdtext.esc(token), token)
        for text in ("<b>", "[x](y)", "*a*", "www.example.com", "&amp;", "_a_"):
            with self.subTest(text):
                self.assertNotEqual(mdtext.esc(text), text)


class OpenRecorder:
    """Records every path this process opens while active, via the "open"
    audit event (builtins.open and os.open both raise it). An audit hook
    cannot be removed, so one hook is installed per process and gated."""

    _installed = False
    _active: "Optional[List[str]]" = None

    @classmethod
    def _hook(cls, event: str, args: tuple) -> None:
        if event == "open" and cls._active is not None and args:
            path = args[0]
            if isinstance(path, (str, bytes)):
                cls._active.append(os.fsdecode(path))

    def __enter__(self) -> List[str]:
        if not OpenRecorder._installed:
            sys.addaudithook(OpenRecorder._hook)
            OpenRecorder._installed = True
        OpenRecorder._active = []
        return OpenRecorder._active

    def __exit__(self, *_exc: object) -> None:
        OpenRecorder._active = None


GH_RENDER_OPT_IN = "WIRE_WATCH_GH_RENDER"


@unittest.skipUnless(os.environ.get(GH_RENDER_OPT_IN) == "1",
                     f"opt-in: set {GH_RENDER_OPT_IN}=1 to render through GitHub's markdown API")
class GitHubRenderTests(unittest.TestCase):
    """The escaped hostile rows rendered by GitHub itself: the file-view
    renderer (markdown mode) and the comment renderer (gfm mode), in this
    repository's context so #N can resolve to an issue."""

    def gh(self, *args: str) -> str:
        if shutil.which("gh") is None:
            self.skipTest("gh is not installed")
        done = subprocess.run(["gh", *args], capture_output=True, text=True, check=False, timeout=60, cwd=rc.REPO)
        if done.returncode != 0:
            self.skipTest(f"gh {args[0]} is unavailable or unauthenticated")
        return done.stdout

    def repo(self) -> str:
        slug = json.loads(self.gh("repo", "view", "--json", "nameWithOwner")).get("nameWithOwner")
        if not isinstance(slug, str) or "/" not in slug:
            self.skipTest("no GitHub remote slug for this checkout")
        return slug

    def render(self, text: str, mode: str) -> str:
        return self.gh("api", "markdown", "-f", f"text={text}", "-f", f"mode={mode}", "-f", f"context={self.repo()}")

    def test_the_raw_rows_do_render_as_links_and_math(self) -> None:
        # Raw controls: if GitHub stopped linking these, the negative test
        # below would pass for nothing.
        self.assertIn("<a ", self.render("www.example.com", "markdown"))
        self.assertIn("<a ", self.render("contact admin@evil.example", "markdown"))
        self.assertIn("issue-link", self.render("#12", "gfm"))
        self.assertIn("math-renderer", self.render("$x$", "gfm"))

    def test_escaped_hostile_rows_render_without_links_or_math(self) -> None:
        rows = [raw for raw, _ in AUTOLINK_ROWS] + ["x_www.evil.com", "evil.com/path", "a.b-c.io"]
        escaped = [mdtext.esc(raw) for raw in rows] + [mdtext.esc(CAP_BOUNDARY_WWW, report.EXCERPT_CAP)]
        text = "\n\n".join(escaped)
        for mode in ("markdown", "gfm"):
            with self.subTest(mode):
                html = self.render(text, mode)
                self.assertNotIn("<a ", html)
                self.assertNotIn("math-renderer", html)


class RunnerSurfaceTests(RunCase):
    def test_each_run_stop_makes_the_run_incomplete_and_names_itself(self) -> None:
        for stop in RunStop:
            with self.subTest(stop.value):
                self.fresh()
                built = self.build([plant("clean-control")], run_stop=stop)

                self.assertFalse(built.complete)
                self.assertIn(f"- run stopped: {stop.value}\n", built.files[report.REPORT_MD])

    def test_no_run_stop_leaves_a_clean_run_complete(self) -> None:
        built = self.build([plant("clean-control")])

        self.assertTrue(built.complete)
        self.assertNotIn("run stopped", built.files[report.REPORT_MD])

    def test_a_cells_tokens_render_in_its_row(self) -> None:
        tokens = Tokens(input_tokens=1200, output_tokens=34, reasoning_tokens=5, cache_read_billed=678)
        built = self.build([plant("clean-control", tokens=tokens)])

        row = next(line for line in built.files[report.REPORT_MD].splitlines() if "| PASS |" in line)
        self.assertIn("| 1200 / 34 / 5 / 678 |", row)

    def test_a_cell_without_tokens_renders_a_dash(self) -> None:
        built = self.build([plant("clean-control")])

        row = next(line for line in built.files[report.REPORT_MD].splitlines() if "| PASS |" in line)
        self.assertIn("| PASS | - |", row)

    def test_bound_stops_map_to_their_outcome(self) -> None:
        cases = [
            ("cap-size", dict(state=CellState.CAP_REACHED, reason="size"), (Outcome.CAP_REACHED, "size")),
            ("cap-run-size", dict(state=CellState.CAP_REACHED, reason="run-size"), (Outcome.CAP_REACHED, "run-size")),
            ("exited-9-size", dict(rc=9, detail="size"), (Outcome.CAP_REACHED, "size")),
            ("exited-9-timeout", dict(rc=9, detail="timeout"), (Outcome.TIMEOUT, None)),
            ("exited-9-interrupted", dict(rc=9, detail="interrupted"), (Outcome.INTERRUPTED, None)),
            ("exited-run-size", dict(rc=0, detail="run-size"), (Outcome.CAP_REACHED, "run-size")),
        ]
        for name, kw, expected in cases:
            with self.subTest(name):
                self.fresh()
                built = self.build([plant("clean-control", **kw)])

                self.assertEqual((built.results[0].outcome, built.results[0].reason), expected)
                self.assertFalse(built.complete)

    def test_the_runner_log_is_never_opened(self) -> None:
        log = "export ROUTECTL_DRIVER_P_API_KEY=" + "sk-" + "ant-api03-" + "R" * 40 + "\n"
        plans = [plant("clean-control", runner_log=log), plant("request-leg-plant", runner_log=log)]
        rc.build_run(self.run_dir, plans)
        ctx = report.context(rc.PATHS, None)

        with OpenRecorder() as opened:
            built = report.build(self.run_dir, rc.PATHS, ctx, GENERATED)
            report.publish(self.run_dir, "20261012T180000Z", built.files, built.raw_texts)

        self.assertTrue(any(p.endswith("health.json") for p in opened),
                        "the recorder saw no evidence read, so it proves nothing")
        self.assertEqual([p for p in opened if p.endswith(".runner.log")], [])

    def test_every_stop_detail_has_its_own_outcome(self) -> None:
        self.assertEqual(set(classify.STOP_OUTCOMES), set(ledger.STOP_DETAILS))

    def test_findings_jsonl_holds_the_raw_values(self) -> None:
        path = "/tools/0/input_schema/$defs/*/$ref"

        def punctuated(data: dict) -> None:
            data["wire_check"]["loss_findings"][0]["path"] = path

        plan = plant("request-leg-plant", health=rc.edited_health("request-leg-plant", punctuated))
        self.assertIn(".", plan.cell_id)
        built = self.build([plan])

        rows = self.findings(built)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["cells"], [plan.cell_id])
        self.assertEqual(rows[0]["example_paths"], [path])
        self.assertEqual(rows[0]["lane"], "responses->openai-responses")
        self.assertNotEqual(mdtext.esc(plan.cell_id), plan.cell_id, "the escaper must change this id")

    def test_the_runner_log_is_never_read_into_the_report(self) -> None:
        planted = "sk-" + "ant-api03-" + "R" * 40
        log = f"export ROUTECTL_DRIVER_P_API_KEY={planted}\n"
        built = self.build([plant("clean-control", runner_log=log),
                            plant("request-leg-plant", runner_log=log)])
        runner_logs = [n for n in os.listdir(os.path.join(self.run_dir, "cells")) if n.endswith(".runner.log")]
        self.assertEqual(len(runner_logs), 2)

        body = "R" * 40
        for name, text in built.files.items():
            with self.subTest(name):
                self.assertNotIn(body, text)
        self.assertNotIn(body, "".join(built.raw_texts))
        final = report.publish(self.run_dir, "20261012T180000Z", built.files, built.raw_texts)
        self.assertEqual(sorted(os.listdir(final)), sorted(built.files))


class PublishTests(RunCase):
    def test_main_publishes_a_dated_report_and_exits_by_status(self) -> None:
        rc.build_run(self.run_dir, [plant("clean-control")])

        done = subprocess.run([sys.executable, "-I", os.path.join(rc.WIRE_WATCH, "report.py"),
                               "--report-dir", self.run_dir], capture_output=True, text=True, check=False)

        self.assertEqual(done.returncode, report.EXIT_COMPLETE, done.stderr)
        published = done.stdout.strip()
        self.assertRegex(os.path.basename(published), r"^report-\d{8}T\d{6}Z$")
        self.assertEqual(sorted(os.listdir(published)), [report.FINDINGS_JSONL, report.REPORT_MD])

    def test_main_exits_incomplete_for_an_incomplete_run(self) -> None:
        rc.build_run(self.run_dir, [plant("clean-control", health_status="http 500")])

        done = subprocess.run([sys.executable, "-I", os.path.join(rc.WIRE_WATCH, "report.py"),
                               "--report-dir", self.run_dir], capture_output=True, text=True, check=False)

        self.assertEqual(done.returncode, report.EXIT_INCOMPLETE, done.stderr)

    def key_dir_cases(self) -> List[tuple]:
        base = os.path.join(self._tmp.name, "key-dirs")
        os.mkdir(base, 0o700)
        a_file = os.path.join(base, "a-file")
        with open(a_file, "w", encoding="ascii") as handle:
            handle.write("x\n")
        real = os.path.join(base, "real")
        os.mkdir(real, 0o700)
        link = os.path.join(base, "link")
        os.symlink(real, link)
        group_writable = os.path.join(base, "group-writable")
        os.mkdir(group_writable)
        os.chmod(group_writable, 0o770)
        return [("missing", os.path.join(base, "nonexistent"), "missing"),
                ("file", a_file, "unreadable"),
                ("symlink", link, "symlink"),
                ("group-writable", group_writable, "bad-dir-mode")]

    def test_a_key_dir_the_key_reader_refuses_is_a_usage_error(self) -> None:
        rc.build_run(self.run_dir, [plant("clean-control")])
        for name, key_dir, detail in self.key_dir_cases():
            with self.subTest(name):
                with self.assertLogs(report.LOG, "ERROR") as logs:
                    code = report.main(["--report-dir", self.run_dir, "--key-dir", key_dir])

                self.assertEqual(code, report.EXIT_USAGE)
                self.assertIn(f"detail={detail}", "\n".join(logs.output))
        self.assertEqual([n for n in os.listdir(self.run_dir) if n.startswith((".report-", "report-"))], [])

    def test_key_dir_problem_is_the_key_readers_own_verdict(self) -> None:
        for name, key_dir, detail in self.key_dir_cases():
            with self.subTest(name):
                self.assertEqual(matrix.key_dir_problem(key_dir), detail)
                self.assertEqual(matrix.open_key_file(key_dir, KEY_FILE_NAME), (None, detail))

    def test_an_owner_only_key_dir_is_accepted(self) -> None:
        rc.build_run(self.run_dir, [plant("clean-control")])
        keys = os.path.join(self._tmp.name, "keys")
        os.mkdir(keys, 0o700)
        self.assertIsNone(matrix.key_dir_problem(keys))

        with self.assertLogs(report.LOG, "INFO"), mock.patch("builtins.print"):
            code = report.main(["--report-dir", self.run_dir, "--key-dir", keys])

        self.assertEqual(code, report.EXIT_COMPLETE)

    def test_an_unexpected_build_failure_exits_not_published(self) -> None:
        rc.build_run(self.run_dir, [plant("clean-control")])

        def boom(*_args: object) -> report.Built:
            raise ValueError("year 33658 is out of range")

        with mock.patch.object(report, "build", boom), self.assertLogs(report.LOG, "ERROR"):
            code = report.main(["--report-dir", self.run_dir])

        self.assertEqual(code, report.EXIT_PUBLISH)
        self.assertEqual([n for n in os.listdir(self.run_dir) if n.startswith((".report-", "report-"))], [])

    def test_the_published_report_passes_the_scrub_check(self) -> None:
        built = self.build([plant("request-leg-plant"), plant("response-leg-plant"), plant("clean-control")])
        final = report.publish(self.run_dir, "20261012T180000Z", built.files, built.raw_texts)

        done = subprocess.run(["bash", SCRUB, "--check", final], capture_output=True, text=True, check=False)

        self.assertEqual(done.returncode, 0, done.stderr)

    def test_a_key_shaped_excerpt_is_caught_and_nothing_is_published(self) -> None:
        planted = "sk-" + "ant-api03-" + "Q" * 40
        row = json.dumps({"excerpt": "echo " + planted, "provider_kind": "bedrock-converse", "status": 403})
        plan = converse(errors=(rc.error_line("bedrock-converse", 403, BEDROCK_403),),
                        extra_files={"upstream_errors.jsonl": row + "\n"})
        built = self.build([plan])
        self.assertNotIn(planted, built.files[report.REPORT_MD])

        with self.assertRaises(report.Unpublishable):
            report.publish(self.run_dir, "20261012T180000Z", built.files, built.raw_texts)

        self.assertEqual([n for n in os.listdir(self.run_dir) if n.startswith((".report-", "report-"))], [])

    def test_the_scrub_check_catches_the_planted_key_in_a_plain_file(self) -> None:
        directory = os.path.join(self._tmp.name, "control")
        os.mkdir(directory)
        with open(os.path.join(directory, "x.md"), "w", encoding="ascii") as handle:
            handle.write("sk-" + "ant-api03-" + "Q" * 40 + "\n")

        done = subprocess.run(["bash", SCRUB, "--check", directory], capture_output=True, text=True, check=False)

        self.assertEqual(done.returncode, 1)


def _rust_labels(path: str, enum: str) -> List[str]:
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    body = re.search(rf"impl {enum} \{{(.*?)\n\}}", text, re.DOTALL)
    if body is None:
        raise AssertionError(f"impl {enum} not found in {path}")
    return re.findall(r'Self::[A-Za-z]+ => "([a-z_]+)"', body.group(1))


@unittest.skipUnless(os.path.isdir(CORE_WIRE_CHECK), "crates/ absent: no source to weld against")
class WeldTests(unittest.TestCase):
    """The classifier's replicas of the daemon's closed labels."""

    def test_path_classes_match_the_daemon(self) -> None:
        self.assertEqual(list(classify.PATH_CLASSES), _rust_labels(os.path.join(CORE_WIRE_CHECK, "edits.rs"), "PathClass"))

    def test_legs_match_the_daemon(self) -> None:
        self.assertEqual(list(classify.LEGS), _rust_labels(os.path.join(CORE_WIRE_CHECK, "lane.rs"), "Leg"))

    def test_lane_ingress_labels_match_the_daemon(self) -> None:
        dialects = _rust_labels(os.path.join(CORE_WIRE_CHECK, "lane.rs"), "IngressDialect")
        self.assertEqual(classify.LANE_LABEL_RE.pattern.split("(")[1].split(")")[0].split("|"), dialects)

    def test_the_pointer_grammar_matches_the_daemon(self) -> None:
        with open(os.path.join(CORE_WIRE_CHECK, "path.rs"), encoding="utf-8") as handle:
            text = handle.read()

        def const(name: str) -> str:
            found = re.search(rf"const {name}: [^=]+= (.*?);", text, re.DOTALL)
            if found is None:
                raise AssertionError(f"{name} not found in path.rs")
            return found.group(1)

        self.assertEqual(int(const("MAX_WIRE_PATH_DEPTH")), classify.PATH_MAX_DEPTH)
        self.assertEqual(int(const("MAX_WIRE_PATH_LEN")), classify.PATH_MAX_CHARS)
        self.assertEqual(int(const("MAX_WIRE_KEY_LEN")), classify.PATH_KEY_MAX_CHARS)
        self.assertEqual(json.loads(const("WILDCARD_SEGMENT")), classify.PATH_WILDCARD)
        keywords = re.findall(r'"([^"]+)"', const("SCHEMA_KEYWORDS"))
        identifier = re.compile(r"[A-Za-z_][A-Za-z0-9_-]*")
        self.assertEqual(sorted(k for k in keywords if not identifier.fullmatch(k)),
                         sorted(classify.PATH_SCHEMA_SIGIL_KEYWORDS))
        self.assertIn("b.is_ascii_alphabetic() || b == b'_'", text)
        self.assertIn("b.is_ascii_alphanumeric() || b == b'_' || b == b'-'", text)

    def test_leg_files_match_what_the_rig_writes(self) -> None:
        with open(CAPTURE_FIXTURES, encoding="utf-8") as handle:
            text = handle.read()
        for name in legs.LEG_FILES:
            with self.subTest(name):
                self.assertIn(f'> "$tmp/{name}"', text)


class RulesTableTests(unittest.TestCase):
    def test_the_committed_table_loads(self) -> None:
        rules = refusal.load_rules(report.RULES_TSV)

        self.assertTrue(all(r.source for r in rules))

    @unittest.skipUnless(os.path.isdir(CORE_WIRE_CHECK), "crates/ absent: cited sources not in this tree")
    def test_every_cited_repo_path_exists(self) -> None:
        cited_paths = [r.source for r in refusal.load_rules(report.RULES_TSV) if not r.source.startswith("https://")]
        self.assertTrue(cited_paths)
        for path in cited_paths:
            with self.subTest(path):
                self.assertTrue(os.path.exists(os.path.join(rc.REPO, path)))

    def test_every_bedrock_rule_cites_aws_or_a_recording(self) -> None:
        aws_docs = re.compile(r"https://docs\.aws\.amazon\.com/bedrock/\S+")
        recording = re.compile(r"crates/[a-z0-9-]+/tests/fixtures/bedrock_[a-z0-9_]+\.json")
        bedrock = [r for r in refusal.load_rules(report.RULES_TSV) if r.bedrock]
        self.assertTrue(bedrock)
        for rule in bedrock:
            with self.subTest(rule.line):
                self.assertTrue(
                    aws_docs.fullmatch(rule.source)
                    or (rule.phrase is not None and recording.fullmatch(rule.source)),
                    rule.source,
                )

    def test_a_malformed_row_is_refused_naming_its_line(self) -> None:
        with tempfile.NamedTemporaryFile("w", suffix=".tsv", delete=False) as handle:
            handle.write("\t".join(refusal.COLUMNS) + "\n")
            handle.write("bedrock\t403\t-\tPASS\tx\tsrc\n")
        try:
            with self.assertRaisesRegex(refusal.RefusalError, r":2: outcome"):
                refusal.load_rules(handle.name)
        finally:
            os.unlink(handle.name)


if __name__ == "__main__":
    unittest.main()
