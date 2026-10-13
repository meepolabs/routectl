"""planted_drop.py `check`: the snapshot decode is strict.

The recorded snapshots under testdata/ pass the check; the same snapshot
with a duplicate key or an overflowing float must fail it, through the
same unreadable-snapshot path a truncated file takes.
"""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
WIRE_WATCH = os.path.dirname(HERE)
TESTDATA = os.path.join(WIRE_WATCH, "testdata")
sys.path.insert(0, WIRE_WATCH)

import planted_drop  # noqa: E402

CELL = "clean-control"
RECORDED = os.path.join(TESTDATA, CELL + ".health.json")


def run_check(path: str) -> tuple:
    err = io.StringIO()
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
        rc = planted_drop.main(["check", CELL, path])
    return rc, err.getvalue()


class SnapshotTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        with open(RECORDED, encoding="utf-8") as handle:
            self.text = handle.read()

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def planted(self, text: str) -> str:
        path = os.path.join(self._tmp.name, "health.json")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path


class StrictSnapshotTests(SnapshotTestCase):
    def test_the_recorded_snapshot_passes(self) -> None:
        self.assertEqual(run_check(self.planted(self.text)), (0, ""))

    def test_a_malformed_snapshot_fails_the_check(self) -> None:
        doc = json.loads(self.text)
        body = json.dumps(doc, indent=2, sort_keys=True)
        rows = [
            ("duplicate top-level key", body.replace('"as_of":', '"as_of": "x",\n  "as_of":', 1), "duplicate key"),
            ("duplicate nested key", body.replace('"wire_check": {', '"wire_check": {"checks": [], ', 1), "duplicate key"),
            ("overflowing float", body.replace('"data": {', '"data": {"overflow": 1e999, ', 1), "non-finite number"),
        ]
        for name, text, detail in rows:
            with self.subTest(name):
                self.assertNotEqual(text, body, "the plant did not apply")
                self.assertEqual(json.loads(text)["data"]["wire_check"].keys() >= {"checks"}, True)

                rc, err = run_check(self.planted(text))

                self.assertEqual(rc, 1)
                self.assertIn("unreadable snapshot", err)
                self.assertIn(detail, err)


class SnapshotShapeTests(SnapshotTestCase):
    def test_a_non_object_data_member_fails_through_the_unreadable_path(self) -> None:
        for name, data in (("list", [1]), ("string", "s"), ("number", 3)):
            with self.subTest(name):
                rc, err = run_check(self.planted(json.dumps({"schema_version": 6, "data": data})))

                self.assertEqual(rc, 1)
                self.assertIn("unreadable snapshot", err)
                self.assertNotIn("Traceback", err)


if __name__ == "__main__":
    unittest.main()
