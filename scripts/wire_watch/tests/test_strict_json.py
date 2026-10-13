"""strict_json.py: the one decoder every wire-watch reader uses."""

import json
import os
import subprocess
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import strict_json  # noqa: E402
from strict_json import StrictJsonError  # noqa: E402


# Peak RSS growth the depth scan may add over the 8 MB input itself; a scan
# that keeps state per character grows by hundreds of MiB here.
SCAN_GROWTH_LIMIT_KIB = 64 * 1024


class StrictJsonTests(unittest.TestCase):
    def test_refusals_carry_a_fixed_detail(self) -> None:
        deep = "[" * 60_000
        with self.assertRaises(RecursionError, msg="the payload no longer exhausts this interpreter's decoder"):
            json.loads(deep)
        rows = [
            ("deep nesting", deep, strict_json.TOO_DEEP),
            ("duplicate key", '{"a": 1, "a": 1}', strict_json.DUPLICATE_KEY),
            ("nested duplicate key", '[{"b": {"a": 1, "a": 2}}]', strict_json.DUPLICATE_KEY),
            ("NaN", "[NaN]", strict_json.NON_FINITE),
            ("Infinity", '{"a": Infinity}', strict_json.NON_FINITE),
            ("-Infinity", "-Infinity", strict_json.NON_FINITE),
            ("truncated", '{"a": ', strict_json.NOT_JSON),
        ]
        for name, text, detail in rows:
            with self.subTest(name):
                with self.assertRaises(StrictJsonError) as caught:
                    strict_json.loads(text)
                self.assertEqual(caught.exception.detail, detail)

    def test_numbers_beyond_the_module_bounds_are_refused(self) -> None:
        rows = [
            ("1e999", '{"a": 1e999}', strict_json.NON_FINITE),
            ("-1e400", "[-1e400]", strict_json.NON_FINITE),
            ("1E999", "1E999", strict_json.NON_FINITE),
            ("5000-digit int", "9" * 5000, strict_json.NUMBER_TOO_LONG),
            ("21-digit int", "1" * 21, strict_json.NUMBER_TOO_LONG),
            ("21-char negative int", "-" + "1" * 20, strict_json.NUMBER_TOO_LONG),
        ]
        for name, text, detail in rows:
            with self.subTest(name):
                with self.assertRaises(StrictJsonError) as caught:
                    strict_json.loads(text)
                self.assertEqual(caught.exception.detail, detail)

    def test_numbers_at_the_bound_are_accepted(self) -> None:
        self.assertEqual(strict_json.loads("9" * 20), int("9" * 20))
        self.assertEqual(strict_json.loads("-" + "9" * 19), -int("9" * 19))
        self.assertEqual(strict_json.loads("1.7976931348623157e308"), 1.7976931348623157e308)

    def test_the_int_bound_holds_without_the_interpreter_digit_limit(self) -> None:
        child = (
            "import sys\n"
            "sys.path.insert(0, sys.argv[1])\n"
            "import strict_json\n"
            "try:\n"
            "    strict_json.loads('9' * 5000)\n"
            "except strict_json.StrictJsonError as exc:\n"
            "    print(exc.detail)\n"
        )
        env = {**os.environ, "PYTHONINTMAXSTRDIGITS": "0"}
        proc = subprocess.run(
            [sys.executable, "-c", child, os.path.dirname(HERE)], env=env, capture_output=True, text=True, check=True
        )
        self.assertEqual(proc.stdout.strip(), strict_json.NUMBER_TOO_LONG)

    def test_depth_is_bounded_by_the_module(self) -> None:
        def arrays(n: int) -> str:
            return "[" * n + "]" * n

        def objects(n: int) -> str:
            return '{"a":' * n + "1" + "}" * n

        limit = strict_json.MAX_DEPTH
        for name, build in (("arrays", arrays), ("objects", objects)):
            with self.subTest(name, depth=limit):
                strict_json.loads(build(limit))
            with self.subTest(name, depth=limit + 1):
                with self.assertRaises(StrictJsonError) as caught:
                    strict_json.loads(build(limit + 1))
                self.assertEqual(caught.exception.detail, strict_json.TOO_DEEP)

    def test_depth_well_inside_the_interpreter_limit_is_still_refused(self) -> None:
        # 500 decodes under the stdlib on every supported interpreter (3.10
        # recurses out near 1000), so only the module bound can refuse it.
        text = "[" * 500 + "]" * 500
        json.loads(text)
        with self.assertRaises(StrictJsonError) as caught:
            strict_json.loads(text)
        self.assertEqual(caught.exception.detail, strict_json.TOO_DEEP)

    def test_brackets_inside_strings_do_not_count(self) -> None:
        rows = [
            ("brackets", '{"a": "' + "[" * 100 + '"}'),
            ("braces", '["' + "{" * 100 + '"]'),
            ("escaped quote then brackets", '{"a": "\\" ' + "[" * 100 + '"}'),
            ("escaped backslash before quote", '["\\\\", "' + "[" * 100 + '"]'),
        ]
        for name, text in rows:
            with self.subTest(name):
                self.assertEqual(strict_json.loads(text), json.loads(text))

    def test_the_depth_scan_memory_is_flat_on_a_long_string(self) -> None:
        child = (
            "import resource, sys\n"
            "sys.path.insert(0, sys.argv[1])\n"
            "import strict_json\n"
            "text = '[\"' + 'x' * (8 << 20) + '\"]'\n"
            "base = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss\n"
            "strict_json._check_depth(text)\n"
            "print(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss - base)\n"
        )
        proc = subprocess.run(
            [sys.executable, "-c", child, os.path.dirname(HERE)], capture_output=True, text=True, check=True
        )
        grown_kib = int(proc.stdout.strip())
        self.assertLess(grown_kib, SCAN_GROWTH_LIMIT_KIB, f"the depth scan grew peak RSS by {grown_kib} KiB")

    def test_ordinary_json_decodes_as_json_loads_does(self) -> None:
        text = '{"a": [1, 2.5, "x", null, true], "b": {"c": {}}, "d": 1e308}'
        self.assertEqual(strict_json.loads(text), json.loads(text))


if __name__ == "__main__":
    unittest.main()
