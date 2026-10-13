"""run.py end to end against a stub run_capture and a PATH-stub docker.

No container starts and nothing reaches a provider: every cell is launched
through --run-capture, a stub that records what it was given, behaves as
behavior.json (beside it) says for its cell, and exits. The stub reads its
instructions from a file because run.py hands the child an allowlisted
environment. Keys are fake values in throwaway key directories.
"""

import fnmatch
import json
import os
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from typing import Dict, List, Optional

HERE = os.path.dirname(os.path.abspath(__file__))
WIRE_WATCH = os.path.dirname(HERE)
REPO = os.path.abspath(os.path.join(WIRE_WATCH, "..", ".."))
RUN_PY = os.path.join(WIRE_WATCH, "run.py")
RUN_CAPTURE = os.path.join(REPO, "scripts", "container", "run_capture.sh")
sys.path.insert(0, WIRE_WATCH)

import ledger  # noqa: E402
import matrix  # noqa: E402
import run  # noqa: E402
from ledger import CellState  # noqa: E402

LEFTOVER_ID = "0123456789ab" + "cdef" * 13
PLANTED_KEY = "PLANTEDRUNKEY-4c1d7e9a2b6f3058-do-not-echo"
PARENT_KEY = "PLANTEDPARENTKEY-93b0e6a1c7d24f58-do-not-forward"

SEAT_PAIRING = "claude-code-print.anthropic-api.base-url.*"
OTHER_SEAT_PAIRING = "codex.openai-responses.base-url.*"
KEYED_PAIRING = "claude-code-print.openai-compat.ollama.base-url.*"

STUB = r'''#!/usr/bin/env python3
import fnmatch, json, os, signal, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))


def write_json(path, doc):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as handle:
        json.dump(doc, handle)
    os.replace(tmp, path)


def cmdline_hits(needle):
    hits = 0
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            with open("/proc/%s/cmdline" % pid, "rb") as handle:
                data = handle.read()
        except OSError:
            continue
        if needle.encode() in data:
            hits += 1
    return hits


args = sys.argv[1:]
sep = args.index("--")
own, rest = args[:sep], args[sep + 1:]
opts, providers = {}, []
for i in range(0, len(own), 2):
    if own[i] == "--provider":
        providers.append(own[i + 1])
    else:
        opts[own[i]] = own[i + 1]
scratch = opts["--scratch"]
cell = os.path.basename(scratch)
with open(os.path.join(HERE, "behavior.json"), encoding="utf-8") as handle:
    cfg = json.load(handle)
b = dict(cfg.get("default", {}))
for pattern, override in cfg.get("cells", {}).items():
    if fnmatch.fnmatchcase(cell, pattern):
        b.update(override)

record = {
    "cell": cell,
    "name": opts["--name"],
    "image": opts.get("--image"),
    "providers": providers,
    "rest": rest,
    "start": time.time(),
    "pid": os.getpid(),
    "scratch": scratch,
    "scratch_mode": os.stat(scratch).st_mode & 0o777,
    "sigblk": [l.split()[1] for l in open("/proc/self/status") if l.startswith("SigBlk:")][0],
    "driver_env": sorted(k for k in os.environ if k.startswith("ROUTECTL_")),
    "env": sorted(os.environ),
}
for needle in cfg.get("needles", []):
    record.setdefault("env_holds", {})[needle] = needle in os.environ.values()
    record.setdefault("cmdline_hits", {})[needle] = cmdline_hits(needle)
record["name_cmdline_hits"] = cmdline_hits(opts["--name"])
if b.get("ignore_term"):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
if b.get("orphan"):
    record["orphan"] = subprocess.Popen(["sleep", "300"]).pid
if b.get("watch_reap"):
    record["watcher"] = subprocess.Popen(
        [sys.executable, "-c", cfg["reap_watcher"], str(os.getpid()), os.path.join(HERE, "log", cell + ".reaped")],
        start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    ).pid
if "ingress" in b:
    os.makedirs(os.path.join(scratch, ".cell-evidence"), exist_ok=True)
    with open(os.path.join(scratch, ".cell-evidence", "ingress_kinds.txt"), "w") as handle:
        handle.write("".join(t + "\n" for t in b["ingress"]))
if b.get("full_evidence"):
    sys.path.insert(0, cfg["tests_dir"])
    import test_evidence
    usage = test_evidence.usage_body()
    usage["data"]["totals"].update(b["full_evidence"])
    test_evidence.make_cell(scratch, usage=usage)
write_json(os.path.join(HERE, "log", cell + ".start.json"), record)
for other in b.get("precreate", []):
    os.mkdir(os.path.join(os.path.dirname(scratch), other))
if b.get("files"):
    many = os.path.join(scratch, "many")
    os.mkdir(many)
    for i in range(b["files"]):
        open(os.path.join(many, "f%06d" % i), "wb").close()
if b.get("chatter"):
    sys.stdout.buffer.write(b"y" * b["chatter"])
    sys.stdout.flush()
if b.get("say"):
    sys.stdout.write(b["say"] + "\n")
    sys.stdout.flush()
    sys.stderr.write(b["say"] + "-stderr\n")
    sys.stderr.flush()
time.sleep(b.get("delay", 0))
if b.get("bytes"):
    with open(os.path.join(scratch, "blob"), "wb") as handle:
        handle.write(b"x" * b["bytes"])
time.sleep(b.get("sleep", 0))
write_json(os.path.join(HERE, "log", cell + ".end.json"), {"end": time.time()})
sys.exit(b.get("rc", 0))
'''

DOCKER_STUB = """#!/usr/bin/env bash
root="$(dirname "$0")/.."
printf '%s\\n' "$*" >>"$root/docker.log"
printf '%s %s\\n' "$(date +%s.%N)" "$*" >>"$root/docker.times"
if [ "$1" = ps ] && [ -f "$root/docker.ps_sleep" ]; then
    printf '%s\\n' "$*" >>"$root/docker.ps_started"
    sleep "$(cat "$root/docker.ps_sleep")"
fi
if [ -f "$root/docker.sleep" ]; then
    printf '%s\\n' "$*" >>"$root/docker.started"
    sleep "$(cat "$root/docker.sleep")"
fi
printf '%s\\n' "$*" >>"$root/docker.done"
if [ "$1" = ps ]; then
    if [ -f "$root/docker.ps_seen" ] && [ -f "$root/docker.ps2" ]; then
        cat "$root/docker.ps2"
    else
        cat "$root/docker.ps" 2>/dev/null || true
    fi
    : >"$root/docker.ps_seen"
    exit 0
fi
exit "$(cat "$root/docker.rc" 2>/dev/null || echo 0)"
"""

# Started by the stub in a session of its own, so the cell's group kill does
# not reach it: records when the leader's pid disappears, which is when it
# was reaped (an exited, unreaped leader stays a zombie).
REAP_WATCHER = r"""
import os, sys, time
pid, out = sys.argv[1], sys.argv[2]
end = time.time() + 60
while time.time() < end:
    if not os.path.exists("/proc/%s" % pid):
        with open(out, "w") as handle:
            handle.write(repr(time.time()))
        break
    time.sleep(0.005)
"""


def all_specs() -> tuple:
    paths = matrix.Paths.from_root(REPO)
    return matrix.expand(paths, matrix.load_rows(paths))


def ids_matching(pattern: str) -> List[str]:
    return [s.id for s in all_specs() if fnmatch.fnmatchcase(s.id, pattern)]


def alive(pid: int) -> bool:
    """Running, as opposed to gone or a zombie waiting on a reaper (inside a
    container the test's own shell may be the reaper of last resort)."""
    try:
        with open(f"/proc/{pid}/stat", encoding="utf-8") as handle:
            text = handle.read()
    except (FileNotFoundError, ProcessLookupError):
        return False
    return text.rsplit(")", 1)[1].split()[0] not in ("Z", "X")


class Rig:
    """A throwaway report dir, key dir, stub run_capture and docker."""

    def __init__(self, root: str) -> None:
        self.root = root
        self.report = os.path.join(root, "report")
        self.keys = os.path.join(root, "keys")
        self.stub_dir = os.path.join(root, "stub")
        self.bin = os.path.join(root, "bin")
        self.stub = os.path.join(self.stub_dir, "run_capture")
        for d in (self.keys, self.stub_dir, self.bin, os.path.join(self.stub_dir, "log")):
            os.makedirs(d, mode=0o700)
        self._write_exec(self.stub, STUB)
        self._write_exec(os.path.join(self.bin, "docker"), DOCKER_STUB)
        self.behave({})

    @staticmethod
    def _write_exec(path: str, text: str) -> None:
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        os.chmod(path, 0o700)

    def behave(self, cells: Dict[str, dict], default: Optional[dict] = None, needles: tuple = ()) -> None:
        doc = {
            "default": default or {}, "cells": cells, "needles": list(needles), "tests_dir": HERE,
            "reap_watcher": REAP_WATCHER,
        }
        with open(os.path.join(self.stub_dir, "behavior.json"), "w", encoding="utf-8") as handle:
            json.dump(doc, handle)

    def key(self, name: str, value: str) -> None:
        path = os.path.join(self.keys, name)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(value + "\n")
        os.chmod(path, 0o600)

    def env(self, extra: Optional[Dict[str, str]] = None) -> Dict[str, str]:
        env = {
            "PATH": self.bin + os.pathsep + os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": self.root,
            "PYTHONDONTWRITEBYTECODE": "1",
        }
        env.update(extra or {})
        return env

    def argv(self, only: List[str], *flags: str, report: Optional[str] = None) -> List[str]:
        argv = [
            sys.executable, RUN_PY,
            "--report-dir", report or self.report,
            "--key-dir", self.keys,
            "--run-capture", self.stub,
            "--kill-grace", "0.3",
            "--poll-seconds", "0.05",
            "--size-poll-seconds", "0.05",
        ]
        for pattern in only:
            argv += ["--only", pattern]
        return argv + list(flags)

    def run(self, only: List[str], *flags: str, extra_env: Optional[Dict[str, str]] = None, timeout: float = 120):
        return subprocess.run(
            self.argv(only, *flags), env=self.env(extra_env), capture_output=True, text=True, timeout=timeout
        )

    def start(
        self, only: List[str], *flags: str, report: Optional[str] = None, own_group: bool = False
    ) -> "subprocess.Popen[bytes]":
        # Output to a file, not a pipe: a cell left running by a failed
        # assertion would hold a pipe open and hang the test.
        with open(os.path.join(self.root, "started.log"), "a", encoding="utf-8") as out:
            return subprocess.Popen(
                self.argv(only, *flags, report=report), env=self.env(), stdout=out, stderr=out,
                start_new_session=own_group,
            )

    def docker_file(self, name: str) -> List[str]:
        path = os.path.join(self.root, name)
        if not os.path.exists(path):
            return []
        with open(path, encoding="utf-8") as handle:
            return handle.read().splitlines()

    def docker_set(self, name: str, value: str) -> None:
        with open(os.path.join(self.root, name), "w", encoding="utf-8") as handle:
            handle.write(value)

    def kill_leftovers(self) -> None:
        for record in self.records("start").values():
            for pid in (record.get("pid"), record.get("orphan"), record.get("watcher")):
                if pid and alive(pid):
                    os.kill(pid, signal.SIGKILL)

    def ledger(self) -> ledger.RunLedger:
        return ledger.load(os.path.join(self.report, "run.json"))

    def records(self, kind: str) -> Dict[str, dict]:
        out = {}
        log = os.path.join(self.stub_dir, "log")
        for name in os.listdir(log):
            if name.endswith(f".{kind}.json"):
                with open(os.path.join(log, name), encoding="utf-8") as handle:
                    out[name[: -len(f".{kind}.json")]] = json.load(handle)
        return out

    def docker_log(self) -> List[str]:
        path = os.path.join(self.root, "docker.log")
        if not os.path.exists(path):
            return []
        with open(path, encoding="utf-8") as handle:
            return handle.read().splitlines()

    def container_ops(self) -> List[str]:
        """docker invocations other than the end-of-run sweep's `ps`."""
        return [line for line in self.docker_log() if not line.startswith("ps ")]

    def container_removed_time(self, name: str) -> Optional[float]:
        path = os.path.join(self.root, "docker.times")
        if not os.path.exists(path):
            return None
        with open(path, encoding="utf-8") as handle:
            for line in handle:
                stamp, _, args = line.rstrip("\n").partition(" ")
                if args == f"rm -f {name}":
                    return float(stamp)
        return None

    def reaped_time(self, cell: str) -> float:
        path = os.path.join(self.stub_dir, "log", cell + ".reaped")
        self.wait_for(lambda: os.path.exists(path))
        with open(path, encoding="utf-8") as handle:
            return float(handle.read())

    def wait_for(self, predicate, timeout: float = 30) -> None:
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if predicate():
                return
            time.sleep(0.05)
        raise AssertionError("condition not reached in time")


class RunTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.rig = Rig(self._tmp.name)

    def tearDown(self) -> None:
        self.rig.kill_leftovers()
        self._tmp.cleanup()

    def states(self) -> Dict[str, ledger.Entry]:
        return {e.spec.id: e for e in self.rig.ledger().entries}


class ConcurrencyTests(RunTestCase):
    def test_peak_overlap_never_exceeds_concurrency(self) -> None:
        cells = ids_matching(SEAT_PAIRING) + ids_matching(OTHER_SEAT_PAIRING)[:4]
        self.assertEqual(len(cells), 12)
        self.rig.behave({}, default={"sleep": 0.3})

        done = self.rig.run(cells, "--concurrency", "2")

        self.assertEqual(done.returncode, 0, done.stderr)
        starts, ends = self.rig.records("start"), self.rig.records("end")
        self.assertEqual(sorted(starts), sorted(cells))
        events = sorted(
            [(starts[c]["start"], 1) for c in cells] + [(ends[c]["end"], -1) for c in cells],
            key=lambda e: (e[0], e[1]),
        )
        live = peak = 0
        for _, delta in events:
            live += delta
            peak = max(peak, live)
        self.assertLessEqual(peak, 2)
        self.assertEqual(peak, 2, "the cells never overlapped, so the bound was not exercised")
        self.assertTrue(all(e.state is CellState.EXITED and e.rc == 0 for e in self.states().values()))


class TimeoutTests(RunTestCase):
    def test_a_sleeping_cell_times_out_has_its_container_removed_and_leaves_nothing_running(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"sleep": 300, "ignore_term": True, "orphan": True})

        done = self.rig.run([cell], "--cell-timeout", "1")

        self.assertNotEqual(done.returncode, 0)
        entry = self.states()[cell]
        self.assertIs(entry.state, CellState.TIMEOUT)
        record = self.rig.records("start")[cell]
        self.assertTrue(record["name"].startswith("routectl-cell-"))
        self.assertIn(f"rm -f {record['name']}", self.rig.docker_log())
        for pid in (record["pid"], record["orphan"]):
            with self.subTest(pid=pid):
                self.assertFalse(alive(pid), f"pid {pid} outlived the run")

    def test_container_removal_comes_before_the_leader_is_reaped(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"sleep": 300, "watch_reap": True})

        self.rig.run([cell], "--cell-timeout", "1")

        name = self.rig.records("start")[cell]["name"]
        killed = self.rig.container_removed_time(name)
        self.assertIsNotNone(killed, "docker rm -f was never called")
        assert killed is not None
        self.assertLess(killed, self.rig.reaped_time(cell))

    def test_a_group_killed_from_outside_still_has_its_container_removed(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"sleep": 300})
        proc = self.rig.start([cell], "--cell-timeout", "120")
        try:
            self.rig.wait_for(lambda: cell in self.rig.records("start"))
            os.killpg(self.rig.records("start")[cell]["pid"], signal.SIGKILL)
            proc.wait(timeout=60)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

        entry = self.states()[cell]
        self.assertEqual((entry.state, entry.detail), (CellState.INTERRUPTED, "signal-9"))
        self.assertIn(f"rm -f {self.rig.records('start')[cell]['name']}", self.rig.docker_log())

    def test_a_clean_self_exit_has_no_container_removed(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:2]
        self.rig.behave({cells[1]: {"rc": 5}})

        self.rig.run(cells, "--concurrency", "1")

        self.assertEqual(self.rig.container_ops(), [])

    def test_a_second_signal_to_run_pys_group_does_not_cut_the_container_removal_short(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"sleep": 300})
        self.rig.docker_set("docker.sleep", "1.5")
        proc = self.rig.start([cell], "--cell-timeout", "1", own_group=True)
        try:
            self.rig.wait_for(lambda: bool(self.rig.docker_file("docker.started")))
            os.killpg(proc.pid, signal.SIGINT)
            proc.wait(timeout=60)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

        self.assertEqual(self.rig.docker_file("docker.done"), self.rig.docker_file("docker.started"))

    def test_a_failed_container_removal_is_logged_as_an_error(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"sleep": 300})
        self.rig.docker_set("docker.rc", "1")

        done = self.rig.run([cell], "--cell-timeout", "1")

        name = self.rig.records("start")[cell]["name"]
        lines = [l for l in done.stderr.splitlines() if name in l and "event=container-remove" in l]
        self.assertTrue(lines, done.stderr)
        self.assertTrue(all(l.startswith("ERROR ") for l in lines), lines)

    def test_the_end_of_run_sweep_removes_a_leftover_and_forces_incomplete(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.docker_set("docker.ps", LEFTOVER_ID + "\n")
        self.rig.docker_set("docker.ps2", "")

        done = self.rig.run([cell])

        self.assertEqual(done.returncode, run.EXIT_INCOMPLETE, done.stderr)
        self.assertIs(self.states()[cell].state, CellState.EXITED)
        sweeps = [l for l in self.rig.docker_log() if l.startswith("ps ")]
        self.assertEqual(len(sweeps), 2, "one listing, and one re-listing after the removal")
        run_id = self.rig.records("start")[cell]["name"][len(run.CELL_NAME_PREFIX):].rsplit("-", 1)[0]
        self.assertIn(f"name=^/routectl-cell-{run_id}-", sweeps[0])
        self.assertIn("rm -f " + LEFTOVER_ID, self.rig.docker_log())
        self.assertIn("event=leftover-container", done.stderr)
        self.assertIs(self.rig.ledger().run_stop, ledger.RunStop.LEFTOVER_CONTAINERS)

    def test_the_sweep_lists_full_ids_and_refuses_short_ones(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.docker_set("docker.ps", "0123456789ab\n")

        done = self.rig.run([cell])

        sweeps = [l for l in self.rig.docker_log() if l.startswith("ps ")]
        self.assertTrue(sweeps and all("--no-trunc" in l for l in sweeps), sweeps)
        self.assertIs(self.rig.ledger().run_stop, ledger.RunStop.SWEEP_FAILED, done.stderr)
        self.assertNotIn("rm -f 0123456789ab", self.rig.docker_log())

    def test_a_container_that_survives_removal_is_sweep_failed(self) -> None:
        rows = [
            # (name, rm -f exit code, what the re-listing shows)
            ("rm -f fails and the container is still listed", "1", LEFTOVER_ID + "\n"),
            ("rm -f fails though the re-listing is empty", "1", ""),
            ("rm -f succeeds but the container is still listed", "0", LEFTOVER_ID + "\n"),
        ]
        for name, rm_rc, relisted in rows:
            with self.subTest(name):
                rig = Rig(tempfile.mkdtemp(dir=self._tmp.name))
                cell = ids_matching(SEAT_PAIRING)[0]
                rig.docker_set("docker.ps", LEFTOVER_ID + "\n")
                rig.docker_set("docker.ps2", relisted)
                rig.docker_set("docker.rc", rm_rc)

                done = rig.run([cell])

                self.assertEqual(done.returncode, run.EXIT_INCOMPLETE, done.stderr)
                self.assertIs(rig.ledger().run_stop, ledger.RunStop.SWEEP_FAILED)
                self.assertEqual(len([l for l in rig.docker_log() if l.startswith("ps ")]), 2)

    def test_a_removal_that_succeeds_is_rechecked_and_stays_leftover_containers(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.docker_set("docker.ps", LEFTOVER_ID + "\n")
        self.rig.docker_set("docker.ps2", "")

        self.rig.run([cell])

        self.assertIs(self.rig.ledger().run_stop, ledger.RunStop.LEFTOVER_CONTAINERS)
        self.assertEqual(len([l for l in self.rig.docker_log() if l.startswith("ps ")]), 2)

    def test_a_sweep_docker_cannot_answer_forces_incomplete(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.docker_set("docker.ps", "Cannot connect to the Docker daemon\n")

        done = self.rig.run([cell])

        self.assertEqual(done.returncode, run.EXIT_INCOMPLETE, done.stderr)
        self.assertIn("why=sweep-failed", done.stderr)
        self.assertIs(self.rig.ledger().run_stop, ledger.RunStop.SWEEP_FAILED)

    def test_a_sweep_stop_after_a_run_size_stop_keeps_the_first(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"bytes": 200_000})
        self.rig.docker_set("docker.ps", LEFTOVER_ID + "\n")
        self.rig.docker_set("docker.ps2", "")

        done = self.rig.run([cell], "--run-cap-bytes", "100000", "--size-poll-seconds", "100")

        self.assertEqual(done.returncode, run.EXIT_INCOMPLETE, done.stderr)
        self.assertIs(self.rig.ledger().run_stop, ledger.RunStop.RUN_SIZE)
        self.assertIn("event=run-stop-not-recorded", done.stderr)
        self.assertIn("rm -f " + LEFTOVER_ID, self.rig.docker_log())

    def test_a_sweep_after_a_signal_is_recorded_too(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"sleep": 300})
        self.rig.docker_set("docker.ps", LEFTOVER_ID + "\n")
        self.rig.docker_set("docker.ps2", "")
        proc = self.rig.start([cell])
        try:
            self.rig.wait_for(lambda: cell in self.rig.records("start"))
            proc.send_signal(signal.SIGINT)
            proc.wait(timeout=60)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

        self.assertEqual(proc.returncode, 130)
        self.assertIs(self.rig.ledger().run_stop, ledger.RunStop.LEFTOVER_CONTAINERS)

    def test_an_empty_sweep_leaves_the_outcome_alone(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]

        done = self.rig.run([cell])

        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len([l for l in self.rig.docker_log() if l.startswith("ps ")]), 1)
        self.assertEqual(self.rig.container_ops(), [])
        self.assertIsNone(self.rig.ledger().run_stop)

    def test_an_exception_after_launch_still_sweeps_and_keeps_its_exit_code(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:2]
        self.rig.behave({cells[0]: {"precreate": [cells[1]]}})
        self.rig.docker_set("docker.ps", LEFTOVER_ID + "\n")
        self.rig.docker_set("docker.ps2", "")

        done = self.rig.run(cells, "--concurrency", "1")

        self.assertEqual(done.returncode, run.EXIT_INCOMPLETE, done.stderr)
        self.assertIn("event=io-failed error=FileExistsError", done.stderr)
        self.assertIn("rm -f " + LEFTOVER_ID, self.rig.docker_log())
        self.assertIs(self.rig.ledger().run_stop, ledger.RunStop.LEFTOVER_CONTAINERS)

    def test_a_signal_during_the_sweep_gives_that_signals_exit_code(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.docker_set("docker.ps_sleep", "2")
        proc = self.rig.start([cell])
        try:
            self.rig.wait_for(lambda: bool(self.rig.docker_file("docker.ps_started")))
            proc.send_signal(signal.SIGHUP)
            proc.wait(timeout=60)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

        self.assertEqual(proc.returncode, 129)
        self.assertIs(self.states()[cell].state, CellState.EXITED)

    def test_the_pid_check_sees_a_live_process(self) -> None:
        child = subprocess.Popen(["sleep", "30"])
        try:
            self.assertTrue(alive(child.pid))
        finally:
            child.kill()
            child.wait()
        self.assertFalse(alive(child.pid))


class BoundTests(RunTestCase):
    def test_deadline_caps_unstarted_cells_and_exits_nonzero(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:4]
        self.rig.behave({}, default={"sleep": 0.6})

        done = self.rig.run(cells, "--concurrency", "1", "--deadline", "1")

        self.assertNotEqual(done.returncode, 0)
        states = self.states()
        capped = [c for c in cells if states[c].state is CellState.CAP_REACHED]
        self.assertTrue(capped, "no cell was stopped by the deadline")
        self.assertTrue(all(states[c].reason == "deadline" for c in capped))
        self.assertTrue(set(capped).isdisjoint(self.rig.records("start")), "a capped cell was launched")
        self.assertIs(states[cells[0]].state, CellState.EXITED)

    def test_aggregate_cap_stops_launches(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:4]
        self.rig.behave({}, default={"bytes": 40_000})

        done = self.rig.run(cells, "--concurrency", "1", "--run-cap-bytes", "60000")

        self.assertNotEqual(done.returncode, 0)
        states = self.states()
        self.assertEqual([states[c].state for c in cells[:2]], [CellState.EXITED] * 2)
        self.assertEqual({(states[c].state, states[c].reason) for c in cells[2:]}, {(CellState.CAP_REACHED, "run-size")})
        self.assertEqual(sorted(self.rig.records("start")), sorted(cells[:2]))

    def test_admission_sees_a_running_cells_late_writes(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:3]
        self.rig.behave({
            cells[0]: {"delay": 0.5, "bytes": 70_000, "sleep": 2},
            cells[1]: {"sleep": 1.2},
        })

        done = self.rig.run(cells, "--concurrency", "2", "--run-cap-bytes", "60000", "--size-poll-seconds", "100")

        self.assertNotEqual(done.returncode, 0)
        states = self.states()
        self.assertEqual((states[cells[2]].state, states[cells[2]].reason), (CellState.CAP_REACHED, "run-size"))
        self.assertNotIn(cells[2], self.rig.records("start"))

    def test_crossing_the_run_cap_with_nothing_pending_is_still_incomplete(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:2]
        self.rig.behave({cells[0]: {"delay": 0.3, "bytes": 70_000}, cells[1]: {"sleep": 1}})

        done = self.rig.run(cells, "--concurrency", "2", "--run-cap-bytes", "60000", "--size-poll-seconds", "100")

        self.assertEqual(done.returncode, run.EXIT_INCOMPLETE)
        crossed = self.states()[cells[0]]
        self.assertEqual((crossed.state, crossed.rc, crossed.detail), (CellState.EXITED, 0, "run-size"))
        self.assertTrue(ledger.bound_stopped(crossed))

    def test_a_cell_far_over_its_own_cap_counts_in_full_toward_the_run_cap(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:2]
        self.rig.behave({cells[0]: {"bytes": 2_000_000}})

        done = self.rig.run(
            cells, "--concurrency", "1", "--cell-cap-bytes", "100000", "--run-cap-bytes", "1000000",
        )

        self.assertNotEqual(done.returncode, 0)
        states = self.states()
        self.assertEqual((states[cells[0]].state, states[cells[0]].reason), (CellState.CAP_REACHED, "size"))
        self.assertEqual((states[cells[1]].state, states[cells[1]].reason), (CellState.CAP_REACHED, "run-size"))
        self.assertNotIn(cells[1], self.rig.records("start"))

    def test_a_run_cap_crossing_is_recorded_when_every_finisher_already_has_a_detail(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"bytes": 200_000, "rc": 20})

        done = self.rig.run([cell], "--run-cap-bytes", "100000", "--size-poll-seconds", "100")

        self.assertEqual(done.returncode, run.EXIT_INCOMPLETE)
        entry = self.states()[cell]
        self.assertEqual((entry.state, entry.detail), (CellState.NOT_RUN, "refused"))
        self.assertFalse(ledger.bound_stopped(entry), "the fixture must leave no cell to carry the crossing")
        self.assertIs(self.rig.ledger().run_stop, ledger.RunStop.RUN_SIZE)

    def test_a_run_under_its_cap_has_no_run_stop(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]

        self.assertEqual(self.rig.run([cell]).returncode, 0)
        self.assertIsNone(self.rig.ledger().run_stop)

    def test_runner_output_counts_toward_the_cell_cap(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"chatter": 2_000_000, "sleep": 300})

        done = self.rig.run([cell], "--cell-cap-bytes", "1000000", "--cell-timeout", "60")

        self.assertNotEqual(done.returncode, 0)
        entry = self.states()[cell]
        self.assertEqual((entry.state, entry.reason), (CellState.CAP_REACHED, "size"))
        # The stop can land mid-write, so the log is only known to be past the cap.
        self.assertGreater(os.path.getsize(os.path.join(self.rig.report, "cells", cell + ".runner.log")), 1_000_000)

    def test_runner_output_counts_toward_the_run_cap(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"chatter": 2_000_000})

        done = self.rig.run([cell], "--cell-cap-bytes", "5000000", "--run-cap-bytes", "1000000")

        self.assertEqual(done.returncode, run.EXIT_INCOMPLETE, done.stderr)
        self.assertIs(self.rig.ledger().run_stop, ledger.RunStop.RUN_SIZE)

    def test_a_tree_over_the_entry_budget_is_cap_reached_size(self) -> None:
        self.assertLessEqual(run.scratch_size.SIZE_WALK_MAX_ENTRIES, 20_000, "a live walk must stay cheap")
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"files": run.scratch_size.SIZE_WALK_MAX_ENTRIES + 10, "sleep": 300})

        done = self.rig.run([cell], "--cell-timeout", "60")

        self.assertNotEqual(done.returncode, 0)
        entry = self.states()[cell]
        self.assertEqual((entry.state, entry.reason), (CellState.CAP_REACHED, "size"))

    def test_a_timeout_fires_on_time_beside_a_tree_at_the_entry_budget(self) -> None:
        big, small = ids_matching(SEAT_PAIRING)[:2]
        self.rig.behave({
            big: {"files": run.scratch_size.SIZE_WALK_MAX_ENTRIES - 50, "sleep": 300},
            small: {"delay": 1.0, "sleep": 300},
        })

        started = time.monotonic()
        done = self.rig.run(
            [big, small], "--concurrency", "2", "--cell-timeout", "3", "--size-poll-seconds", "0.05",
            "--poll-seconds", "0.05",
        )
        elapsed = time.monotonic() - started

        states = self.states()
        self.assertEqual((states[big].state, states[small].state), (CellState.TIMEOUT, CellState.TIMEOUT), done.stderr)
        self.assertLess(elapsed, 3 + 5, "the timeouts were held back by the size polls")

    def test_a_cell_over_its_size_cap_is_stopped(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"bytes": 80_000, "sleep": 300})

        done = self.rig.run([cell], "--cell-cap-bytes", "40000", "--cell-timeout", "60")

        self.assertNotEqual(done.returncode, 0)
        entry = self.states()[cell]
        self.assertEqual((entry.state, entry.reason), (CellState.CAP_REACHED, "size"))
        self.assertFalse(alive(self.rig.records("start")[cell]["pid"]))


class IngressTests(RunTestCase):
    def test_a_mismatched_exit_5_stops_only_its_pairing(self) -> None:
        mismatched = ids_matching(SEAT_PAIRING)
        matched = ids_matching(OTHER_SEAT_PAIRING)
        self.rig.behave({
            mismatched[0]: {"rc": 5, "ingress": ["openai"]},
            matched[0]: {"rc": 5, "ingress": ["openai-responses"]},
        })

        self.rig.run(mismatched + matched, "--concurrency", "1")

        states = self.states()
        self.assertEqual((states[mismatched[0]].state, states[mismatched[0]].rc), (CellState.EXITED, 5))
        for cell in mismatched[1:]:
            with self.subTest(cell=cell):
                self.assertEqual((states[cell].state, states[cell].reason), (CellState.NOT_RUN, "ingress"))
        self.assertEqual((states[matched[0]].state, states[matched[0]].rc), (CellState.EXITED, 5))
        for cell in matched[1:]:
            with self.subTest(control=cell):
                self.assertEqual((states[cell].state, states[cell].rc), (CellState.EXITED, 0))

    def test_exit_5_without_readable_ingress_evidence_marks_nothing(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:3]
        self.rig.behave({cells[0]: {"rc": 5}})

        self.rig.run(cells, "--concurrency", "1")

        states = self.states()
        self.assertEqual([states[c].state for c in cells], [CellState.EXITED] * 3)


class LockAndSignalTests(RunTestCase):
    def test_second_run_is_refused_and_sigint_interrupts_cleanly(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:4]
        self.rig.behave({}, default={"sleep": 300, "orphan": True})
        first = self.rig.start(cells, "--concurrency", "2")
        try:
            self.rig.wait_for(lambda: len(self.rig.records("start")) == 2)
            run_json = os.path.join(self.rig.report, "run.json")
            with open(run_json, "rb") as handle:
                before = handle.read()

            second = self.rig.run(cells)

            self.assertEqual(second.returncode, run.EXIT_LOCKED, second.stderr)
            with open(run_json, "rb") as handle:
                self.assertEqual(handle.read(), before)
            orphans = [r["orphan"] for r in self.rig.records("start").values()]
            self.assertTrue(all(alive(p) for p in orphans), "the cells were not running before the signal")

            first.send_signal(signal.SIGINT)
            first.wait(timeout=60)
        finally:
            if first.poll() is None:
                first.kill()
                first.wait()

        self.assertEqual(first.returncode, 130)
        states = self.states()
        self.assertEqual({states[c].state for c in cells}, {CellState.INTERRUPTED})
        for record in self.rig.records("start").values():
            with self.subTest(name=record["name"]):
                self.assertIn(f"rm -f {record['name']}", self.rig.docker_log())
        for pid in orphans + [r["pid"] for r in self.rig.records("start").values()]:
            with self.subTest(pid=pid):
                self.assertFalse(alive(pid))

    def test_each_terminating_signal_interrupts_and_removes_every_running_container(self) -> None:
        rows = [
            ("SIGHUP", signal.SIGHUP, 129),
            ("SIGQUIT", signal.SIGQUIT, 131),
            ("SIGUSR1", signal.SIGUSR1, 128 + signal.SIGUSR1),
            ("SIGUSR2", signal.SIGUSR2, 128 + signal.SIGUSR2),
            ("SIGALRM", signal.SIGALRM, 128 + signal.SIGALRM),
        ]
        for name, signum, code in rows:
            with self.subTest(name):
                rig = Rig(tempfile.mkdtemp(dir=self._tmp.name))
                cells = ids_matching(SEAT_PAIRING)[:3]
                rig.behave({}, default={"sleep": 300, "orphan": True})
                proc = rig.start(cells, "--concurrency", "2")
                try:
                    rig.wait_for(lambda: len(rig.records("start")) == 2)
                    proc.send_signal(signum)
                    proc.wait(timeout=60)
                finally:
                    if proc.poll() is None:
                        proc.kill()
                        proc.wait()
                    rig.kill_leftovers()

                self.assertEqual(proc.returncode, code)
                states = {e.spec.id: e for e in rig.ledger().entries}
                self.assertEqual({states[c].state for c in cells}, {CellState.INTERRUPTED})
                for record in rig.records("start").values():
                    self.assertIn(f"rm -f {record['name']}", rig.docker_log())
                    self.assertFalse(alive(record["pid"]))
                    self.assertFalse(alive(record["orphan"]))

    def test_a_report_dir_with_an_earlier_run_is_refused(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.assertEqual(self.rig.run([cell]).returncode, 0)
        with open(os.path.join(self.rig.report, "run.json"), "rb") as handle:
            before = handle.read()

        again = self.rig.run([cell])

        self.assertEqual(again.returncode, run.EXIT_REPORT_DIR)
        with open(os.path.join(self.rig.report, "run.json"), "rb") as handle:
            self.assertEqual(handle.read(), before)

    def test_run_json_lists_every_cell_before_the_first_launch(self) -> None:
        cells = ids_matching(SEAT_PAIRING)[:3]
        self.rig.behave({}, default={"sleep": 300})
        proc = self.rig.start(cells, "--concurrency", "1")
        try:
            self.rig.wait_for(lambda: len(self.rig.records("start")) == 1)
            led = self.rig.ledger()
        finally:
            proc.send_signal(signal.SIGTERM)
            proc.wait(timeout=60)
        self.assertEqual([e.spec.id for e in led.entries], cells)
        self.assertEqual(proc.returncode, 143)


class CredentialTests(RunTestCase):
    def test_the_key_travels_only_in_the_child_environment(self) -> None:
        keyed = ids_matching(KEYED_PAIRING)[:1]
        seat = ids_matching(SEAT_PAIRING)[:1]
        self.rig.key("ollama-api-key", PLANTED_KEY)
        self.rig.behave({}, needles=(PLANTED_KEY, PARENT_KEY))

        done = self.rig.run(
            keyed + seat, "--concurrency", "1",
            extra_env={"ROUTECTL_DRIVER_ANTHROPIC_API_KEY": PARENT_KEY, "AWS_BEARER_TOKEN_BEDROCK": PARENT_KEY},
        )

        self.assertEqual(done.returncode, 0, done.stderr)
        records = self.rig.records("start")
        k, s = records[keyed[0]], records[seat[0]]
        self.assertTrue(k["env_holds"][PLANTED_KEY], "the key never reached the child, so the argv check is vacuous")
        self.assertEqual(k["driver_env"], ["ROUTECTL_DRIVER_OLLAMA_API_KEY"])
        self.assertEqual(k["cmdline_hits"][PLANTED_KEY], 0)
        self.assertGreater(k["name_cmdline_hits"], 0, "the /proc scan cannot see argv")
        self.assertEqual(s["driver_env"], [])
        self.assertFalse(s["env_holds"][PARENT_KEY])
        self.assertNotIn("AWS_BEARER_TOKEN_BEDROCK", s["env"])
        with open(os.path.join(self.rig.report, "run.json"), encoding="utf-8") as handle:
            run_json = handle.read()
        for text in (done.stdout, done.stderr, run_json):
            self.assertNotIn(PLANTED_KEY, text)
            self.assertNotIn(PARENT_KEY, text)

    def test_a_key_that_went_bad_after_preflight_is_not_run(self) -> None:
        cells = ids_matching(KEYED_PAIRING)[:2]
        self.rig.key("ollama-api-key", PLANTED_KEY)
        marker = os.path.join(self.rig.keys, "ollama-api-key")
        self.rig.behave({cells[0]: {"sleep": 0.5}})
        proc = self.rig.start(cells, "--concurrency", "1")
        try:
            self.rig.wait_for(lambda: len(self.rig.records("start")) == 1)
            os.chmod(marker, 0o644)
            proc.wait(timeout=60)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

        entry = self.states()[cells[1]]
        self.assertEqual((entry.state, entry.reason, entry.detail), (CellState.NOT_RUN, "credential", "insecure-mode"))

    def test_runner_exit_20_is_not_run_credential(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"rc": 20})

        self.rig.run([cell])

        entry = self.states()[cell]
        self.assertEqual((entry.state, entry.reason), (CellState.NOT_RUN, "credential"))


class LaunchShapeTests(RunTestCase):
    def test_each_cell_gets_its_own_0700_scratch_and_the_driver_command(self) -> None:
        cells = ids_matching(KEYED_PAIRING)[:2]
        self.rig.key("ollama-api-key", PLANTED_KEY)
        by_id = {s.id: s for s in all_specs()}

        self.rig.run(cells, "--image", "routectl-capture:test")

        records = self.rig.records("start")
        for cell in cells:
            with self.subTest(cell=cell):
                r, spec = records[cell], by_id[cell]
                self.assertEqual(r["scratch_mode"], 0o700)
                self.assertEqual(r["image"], "routectl-capture:test")
                self.assertEqual(r["providers"], list(spec.providers))
                self.assertEqual(r["rest"][-3:], [
                    "env", f"ROUTECTL_DRIVER_REQUEST_MODEL={spec.model}", f"/workspace/scripts/drivers/{spec.driver}",
                ])
                self.assertIn("--expected-ingress", r["rest"])
        self.assertNotEqual(records[cells[0]]["name"], records[cells[1]]["name"])
        self.assertEqual(stat.S_IMODE(os.stat(os.path.join(self.rig.report, "cells")).st_mode), 0o700)

    def test_runner_output_goes_to_a_private_log_beside_the_scratch(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        marker = "RUNNEROUTPUT-5b1e8c2d"
        self.rig.behave({}, default={"say": marker})

        done = self.rig.run([cell])

        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertNotIn(marker, done.stdout)
        self.assertNotIn(marker, done.stderr)
        log = os.path.join(self.rig.report, "cells", cell + ".runner.log")
        self.assertEqual(stat.S_IMODE(os.lstat(log).st_mode), 0o600)
        with open(log, encoding="utf-8") as handle:
            text = handle.read()
        self.assertIn(marker + "\n", text)
        self.assertIn(marker + "-stderr\n", text)
        mounted = self.rig.records("start")[cell]["scratch"]
        self.assertEqual(mounted, os.path.join(self.rig.report, "cells", cell))
        self.assertNotEqual(os.path.commonpath([mounted, log]), mounted, "the log is inside the mounted scratch")

    def test_tokens_come_from_the_cells_usage_snapshot(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"full_evidence": {"reasoning_tokens": 3, "cache_read_billed": 11}})

        self.rig.run([cell])

        entry = self.states()[cell]
        self.assertIs(entry.state, CellState.EXITED)
        self.assertEqual(entry.tokens, ledger.Tokens(input_tokens=17, output_tokens=5, reasoning_tokens=3, cache_read_billed=11))

    def test_an_exited_cell_without_evidence_has_null_tokens(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]

        self.rig.run([cell])

        self.assertIsNone(self.states()[cell].tokens)


class ClassifyTests(unittest.TestCase):
    def test_exit_code_table(self) -> None:
        rows = [
            ("clean exit", 0, None, (CellState.EXITED, 0, None, None)),
            ("rig refusal", 5, None, (CellState.EXITED, 5, None, None)),
            ("driver failed", 4, None, (CellState.EXITED, 4, None, None)),
            ("usage", 2, None, (CellState.EXITED, 2, None, None)),
            ("daemon survived", 9, None, (CellState.EXITED, 9, None, None)),
            ("daemon survived a timeout", 9, run.STOP_TIMEOUT, (CellState.EXITED, 9, None, "timeout")),
            ("daemon survived an interrupt", 9, run.STOP_INTERRUPT, (CellState.EXITED, 9, None, "interrupted")),
            ("credential refused", 20, None, (CellState.NOT_RUN, None, "credential", "refused")),
            ("timed out", -9, run.STOP_TIMEOUT, (CellState.TIMEOUT, None, None, None)),
            ("timed out but exit 20", 20, run.STOP_TIMEOUT, (CellState.TIMEOUT, None, None, None)),
            ("size cap", 143, run.STOP_SIZE, (CellState.CAP_REACHED, None, "size", None)),
            ("we interrupted", 130, run.STOP_INTERRUPT, (CellState.INTERRUPTED, None, None, None)),
            ("runner saw SIGHUP", 129, None, (CellState.INTERRUPTED, None, None, "rc-129")),
            ("runner saw SIGINT", 130, None, (CellState.INTERRUPTED, None, None, "rc-130")),
            ("runner saw SIGTERM", 143, None, (CellState.INTERRUPTED, None, None, "rc-143")),
            ("killed by a signal", -15, None, (CellState.INTERRUPTED, None, None, "signal-15")),
            ("image missing", 18, None, (CellState.EXITED, 18, None, None)),
            ("leader never reaped", None, None, (CellState.INTERRUPTED, None, None, "unreaped")),
            ("leader never reaped after a timeout", None, run.STOP_TIMEOUT, (CellState.TIMEOUT, None, None, None)),
        ]
        for name, rc, stopped, want in rows:
            with self.subTest(name):
                v = run.classify(rc, stopped)
                self.assertEqual((v.state, v.rc, v.reason, v.detail), want)

    def test_the_runners_stop_details_are_the_ledgers(self) -> None:
        self.assertEqual({run.STOP_TIMEOUT, run.STOP_SIZE, run.STOP_INTERRUPT, run.OVER_RUN_CAP}, set(ledger.STOP_DETAILS))

    def test_every_verdict_is_a_legal_move_from_running(self) -> None:
        spec = all_specs()[0]
        for rc in (0, 2, 5, 9, 20, 129, -9, None):
            for stopped in (None, run.STOP_TIMEOUT, run.STOP_SIZE, run.STOP_INTERRUPT):
                with self.subTest(rc=rc, stopped=stopped):
                    v = run.classify(rc, stopped)
                    led = ledger.RunLedger.new(ledger.RunInputs("a" * 64, "b" * 64, ()), [spec])
                    led.transition(spec.id, CellState.RUNNING).transition(
                        spec.id, v.state, rc=v.rc, reason=v.reason, detail=v.detail
                    )


class CellProcessTests(unittest.TestCase):
    def test_a_leader_that_cannot_be_reaped_is_left_and_the_container_still_removed(self) -> None:
        import cell_process
        from unittest import mock

        def stuck_wait(timeout: Optional[float] = None) -> int:
            if timeout is None:
                raise AssertionError("an unbounded wait on a stuck leader never returns")
            raise subprocess.TimeoutExpired("sleep", timeout)

        proc = subprocess.Popen(["sleep", "30"], start_new_session=True)
        kills: List[str] = []
        try:
            with mock.patch.object(proc, "wait", stuck_wait), \
                    mock.patch.object(cell_process, "remove_container", lambda name, env: kills.append(name)):
                rc = cell_process.stop(proc, 0.1, 0.02, "routectl-cell-x", {})
        finally:
            proc.kill()
            proc.wait()

        self.assertIsNone(rc)
        self.assertEqual(kills, ["routectl-cell-x"])


class PollTests(unittest.TestCase):
    """poll_running re-reads the clock and the signal flag before each
    cell, so one slow walk cannot hold back another cell's timeout or a
    signal."""

    def setUp(self) -> None:
        from unittest import mock

        self._tmp = tempfile.TemporaryDirectory()
        report = os.path.join(self._tmp.name, "report")
        os.mkdir(report, 0o700)
        opts = run.parse_args(["--report-dir", report, "--cell-timeout", "10", "--size-poll-seconds", "1"], {})
        specs = all_specs()[:2]
        led = ledger.RunLedger.new(ledger.RunInputs("a" * 64, "b" * 64, ()), specs)
        self.run_ = run.Run(opts, led, "20261012t000000-abcdef", {}, run.Signals())
        self.clock = [100.0]
        self.ids = [s.id for s in specs]
        for spec in specs:
            self.run_.running[spec.id] = run.Running(
                spec=spec, name="routectl-cell-x", scratch="/nonexistent", proc=mock.Mock(), started=95.0
            )
        self.finished: List[tuple] = []
        patches = [
            mock.patch.object(run.time, "monotonic", lambda: self.clock[0]),
            mock.patch.object(run.cell_process, "leader_exited", lambda proc: False),
            mock.patch.object(self.run_, "finish", lambda r, stopped: self.finished.append((r.spec.id, stopped))),
            mock.patch.object(self.run_, "note_run_size", lambda: False),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_a_slow_walk_does_not_hold_back_the_next_cells_timeout(self) -> None:
        def slow_measure(r):
            self.clock[0] += 6.0
            return 0

        self.run_.measure = slow_measure  # type: ignore[method-assign]

        self.run_.poll_running()

        self.assertEqual(self.finished, [(self.ids[1], run.STOP_TIMEOUT)])

    def test_a_signal_during_a_walk_stops_the_poll(self) -> None:
        walked: List[str] = []

        def measure_then_signal(r):
            walked.append(r.spec.id)
            self.run_.signals.received = signal.SIGHUP
            return 0

        self.run_.measure = measure_then_signal  # type: ignore[method-assign]

        self.run_.poll_running()

        self.assertEqual(walked, [self.ids[0]])


class AdmissionTests(unittest.TestCase):
    """The size walk before an admission can be slow; the deadline and the
    signal flag are read again after it, before any runner starts."""

    def setUp(self) -> None:
        from unittest import mock

        self._tmp = tempfile.TemporaryDirectory()
        report = os.path.join(self._tmp.name, "report")
        os.mkdir(report, 0o700)
        os.mkdir(os.path.join(report, "cells"), 0o700)
        opts = run.parse_args(["--report-dir", report], {})
        specs = [s for s in all_specs() if s.key_file is None][:2]
        led = ledger.RunLedger.new(ledger.RunInputs("a" * 64, "b" * 64, ()), specs)
        self.run_ = run.Run(opts, led, "20261012t000000-abcdef", {}, run.Signals())
        self.ids = [s.id for s in specs]
        self.clock = [100.0]
        self.popen = mock.Mock()
        for p in (
            mock.patch.object(run.time, "monotonic", lambda: self.clock[0]),
            mock.patch.object(run.subprocess, "Popen", self.popen),
        ):
            p.start()
            self.addCleanup(p.stop)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_a_walk_that_crosses_the_deadline_launches_nothing(self) -> None:
        def slow_walk() -> None:
            self.clock[0] = 200.0

        self.run_.refresh_sizes = slow_walk  # type: ignore[method-assign]

        self.run_.fill_slots(deadline=150.0)

        self.popen.assert_not_called()
        for cell in self.ids:
            entry = self.run_.led.entry(cell)
            self.assertEqual((entry.state, entry.reason), (CellState.CAP_REACHED, "deadline"))

    def test_a_key_read_that_crosses_the_deadline_launches_nothing(self) -> None:
        from unittest import mock

        def slow_key_read(key_dir: str, name: str):
            self.clock[0] = 200.0
            return "fake-key-value", None

        keyed = [s for s in all_specs() if s.key_file is not None][:1]
        led = ledger.RunLedger.new(ledger.RunInputs("a" * 64, "b" * 64, ()), keyed)
        self.run_ = run.Run(self.run_.opts, led, "20261012t000000-abcdef", {}, run.Signals())
        self.run_.refresh_sizes = lambda: None  # type: ignore[method-assign]

        with mock.patch.object(run.launch_key, "read_key", slow_key_read):
            self.run_.fill_slots(deadline=150.0)

        self.popen.assert_not_called()
        entry = self.run_.led.entry(keyed[0].id)
        self.assertEqual((entry.state, entry.reason), (CellState.CAP_REACHED, "deadline"))

    def test_a_failed_log_close_after_a_good_launch_still_tracks_the_cell(self) -> None:
        from unittest import mock

        self.run_.refresh_sizes = lambda: None  # type: ignore[method-assign]
        self.popen.return_value = mock.Mock(pid=4242)
        real_close = os.close
        log_fds: List[int] = []
        real_open = run.cell_process.open_runner_log

        def open_log(cells_dir: str, cell_id: str) -> int:
            fd = real_open(cells_dir, cell_id)
            log_fds.append(fd)
            return fd

        tracked_at_close: List[bool] = []

        def failing_close(fd: int) -> None:
            real_close(fd)
            if fd in log_fds:
                log_fds.remove(fd)
                tracked_at_close.append(self.ids[0] in self.run_.running)
                raise OSError(5, "Input/output error")

        with mock.patch.object(run.cell_process, "open_runner_log", open_log), \
                mock.patch.object(run.os, "close", failing_close), \
                self.assertLogs("wire_watch.run", level="ERROR") as logs:
            self.run_.launch(self.run_.led.entry(self.run_.pending.pop(0)).spec, deadline=1000.0)

        self.popen.assert_called_once()
        self.assertEqual(tracked_at_close, [True], "the log was closed before the started runner was tracked")
        self.assertEqual(list(self.run_.running), [self.ids[0]])
        self.assertIs(self.run_.led.entry(self.ids[0]).state, CellState.RUNNING)
        self.assertTrue(any("event=runner-log-close-failed" in line for line in logs.output), logs.output)

    def test_a_signal_during_launch_preparation_launches_nothing(self) -> None:
        from unittest import mock

        self.run_.refresh_sizes = lambda: None  # type: ignore[method-assign]
        real_open = run.cell_process.open_runner_log

        def open_then_signal(cells_dir: str, cell_id: str) -> int:
            fd = real_open(cells_dir, cell_id)
            self.run_.signals.received = signal.SIGTERM
            return fd

        with mock.patch.object(run.cell_process, "open_runner_log", open_then_signal):
            self.run_.fill_slots(deadline=1000.0)

        self.popen.assert_not_called()
        self.assertEqual(self.run_.pending, self.ids)
        self.assertEqual({e.state for e in self.run_.led.entries}, {CellState.PENDING})
        self.assertEqual(self.run_.running, {})

    def test_a_deadline_crossed_during_launch_preparation_launches_nothing(self) -> None:
        from unittest import mock

        self.run_.refresh_sizes = lambda: None  # type: ignore[method-assign]
        real_open = run.cell_process.open_runner_log

        def slow_open(cells_dir: str, cell_id: str) -> int:
            self.clock[0] = 200.0
            return real_open(cells_dir, cell_id)

        with mock.patch.object(run.cell_process, "open_runner_log", slow_open):
            self.run_.fill_slots(deadline=150.0)

        self.popen.assert_not_called()
        for cell in self.ids:
            entry = self.run_.led.entry(cell)
            self.assertEqual((entry.state, entry.reason), (CellState.CAP_REACHED, "deadline"))

    def test_a_signal_during_the_walk_launches_nothing(self) -> None:
        def walk_then_signal() -> None:
            self.run_.signals.received = signal.SIGTERM

        self.run_.refresh_sizes = walk_then_signal  # type: ignore[method-assign]

        self.run_.fill_slots(deadline=1000.0)

        self.popen.assert_not_called()
        self.assertEqual(self.run_.pending, self.ids)
        self.assertEqual({e.state for e in self.run_.led.entries}, {CellState.PENDING})


class SpawnWindowTests(RunTestCase):
    """A real signal delivered between the final admission check and the
    cell being tracked is held until the cell is tracked, then stops it."""

    def test_a_sigterm_inside_the_spawn_window_still_interrupts_and_removes_the_cell(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]
        self.rig.behave({}, default={"sleep": 300, "orphan": True})
        os.mkdir(self.rig.report, 0o700)
        opts = run.parse_args(self.rig.argv([cell])[2:], {})
        specs = [s for s in all_specs() if s.id == cell]
        led = ledger.RunLedger.new(ledger.RunInputs("a" * 64, "b" * 64, ()), specs)
        signals = run.Signals()
        saved = {sig: signal.getsignal(sig) for sig in run.EXIT_FOR_SIGNAL}
        for sig, handler in saved.items():
            self.addCleanup(signal.signal, sig, handler)
        signals.install()
        run_ = run.Run(opts, led, run.new_run_id(), self.rig.env(), signals)
        ledger.write_atomic(run_.path, led)
        os.mkdir(run_.cells_dir, 0o700)
        seen: List[tuple] = []
        real_spawn = run.cell_process.spawn_runner

        def spawn_then_sigterm(*args, **kwargs):
            seen.append(("state at spawn", run_.led.entry(cell).state))
            proc = real_spawn(*args, **kwargs)
            os.kill(os.getpid(), signal.SIGTERM)
            seen.append(("flag right after the kill", signals.received))
            return proc

        from unittest import mock

        with mock.patch.object(run.cell_process, "spawn_runner", spawn_then_sigterm):
            code = run_.execute()

        self.assertEqual(seen, [("state at spawn", CellState.PENDING), ("flag right after the kill", None)])
        self.assertEqual(code, 143)
        self.assertIs(run_.led.entry(cell).state, CellState.INTERRUPTED)
        record = self.rig.records("start")[cell]
        self.assertIn(f"rm -f {record['name']}", self.rig.docker_log())
        self.assertFalse(alive(record["pid"]))
        self.assertFalse(alive(record["orphan"]))

    def test_the_runner_starts_with_no_signal_blocked(self) -> None:
        cell = ids_matching(SEAT_PAIRING)[0]

        done = self.rig.run([cell])

        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(int(self.rig.records("start")[cell]["sigblk"], 16), 0)


class CloseOutTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        report = os.path.join(self._tmp.name, "report")
        os.mkdir(report, 0o700)
        opts = run.parse_args(["--report-dir", report], {})
        led = ledger.RunLedger.new(ledger.RunInputs("a" * 64, "b" * 64, ()), [all_specs()[0]])
        self.run_ = run.Run(opts, led, "20261012t000000-abcdef", {}, run.Signals())

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_a_failing_run_stop_write_does_not_mask_the_original_error(self) -> None:
        from unittest import mock
        import cell_process

        class Original(Exception):
            pass

        with mock.patch.object(self.run_, "_execute", side_effect=Original()), \
                mock.patch.object(cell_process, "sweep", return_value=cell_process.SWEEP_LEFTOVERS), \
                mock.patch.object(run.ledger, "write_atomic", side_effect=OSError(28, "No space left on device")), \
                self.assertLogs("wire_watch.run", level="ERROR") as logs:
            with self.assertRaises(Original):
                self.run_.execute()

        self.assertTrue(any("event=run-stop-write-failed" in line for line in logs.output), logs.output)

    def test_a_failing_sweep_does_not_mask_the_original_error(self) -> None:
        from unittest import mock
        import cell_process

        class Original(Exception):
            pass

        with mock.patch.object(self.run_, "_execute", side_effect=Original()), \
                mock.patch.object(cell_process, "sweep", side_effect=RuntimeError("sweep broke")), \
                mock.patch.object(run.ledger, "write_atomic"):
            with self.assertRaises(Original):
                self.run_.execute()
        self.assertIs(self.run_.led.run_stop, ledger.RunStop.SWEEP_FAILED)

    def test_an_unrecorded_sweep_failure_still_exits_incomplete(self) -> None:
        from unittest import mock
        import cell_process

        with mock.patch.object(self.run_, "_execute"), \
                mock.patch.object(self.run_, "exit_status", return_value=run.EXIT_OK), \
                mock.patch.object(cell_process, "sweep", return_value=cell_process.SWEEP_FAILED), \
                mock.patch.object(run.ledger, "write_atomic", side_effect=OSError(28, "No space left on device")):
            self.assertEqual(self.run_.execute(), run.EXIT_INCOMPLETE)


class NameTests(unittest.TestCase):
    def test_every_container_name_fits_run_captures_grammar(self) -> None:
        count = len(all_specs())
        run_ids = [run.new_run_id() for _ in range(32)]
        self.assertTrue(all(len(r) == len(run_ids[0]) for r in run_ids))
        for run_id in run_ids:
            for index in (0, count - 1, 10 * count):
                with self.subTest(run_id=run_id, index=index):
                    self.assertRegex(run.container_name(run_id, index), run.CELL_NAME_RE)

    def test_the_grammar_agrees_with_run_capture(self) -> None:
        good = run.container_name(run.new_run_id(), 10 * len(all_specs()))
        bad = run.CELL_NAME_PREFIX + "x" * 64
        self.assertIsNone(run.CELL_NAME_RE.fullmatch(bad))
        with tempfile.TemporaryDirectory() as scratch:
            env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "ROUTECTL_BIN": os.path.join(scratch, "absent")}
            for name, accepted in ((good, True), (bad, False)):
                with self.subTest(name=name):
                    done = subprocess.run(
                        ["bash", RUN_CAPTURE, "--scratch", os.path.join(scratch, "s"), "--name", name, "--", "x"],
                        env=env, capture_output=True, text=True, timeout=60,
                    )
                    self.assertEqual(done.returncode != 2, accepted, done.stderr)
                    if accepted:
                        self.assertEqual(done.returncode, 15, done.stderr)


class OptionTests(unittest.TestCase):
    FLOAT_OPTIONS = ("--cell-timeout", "--deadline", "--kill-grace", "--poll-seconds", "--size-poll-seconds")

    def test_non_finite_float_options_are_refused(self) -> None:
        for option in self.FLOAT_OPTIONS:
            for value in ("inf", "-inf", "nan", "Infinity", "NaN"):
                with self.subTest(option=option, value=value):
                    with self.assertRaises(SystemExit) as caught:
                        run.parse_args(["--report-dir", "x", option, value], {})
                    self.assertEqual(caught.exception.code, run.EXIT_USAGE)

    def test_finite_positive_floats_are_accepted(self) -> None:
        for option in self.FLOAT_OPTIONS:
            with self.subTest(option=option):
                opts = run.parse_args(["--report-dir", "x", option, "1.5"], {})
                self.assertIn(1.5, (opts.cell_timeout, opts.deadline, opts.kill_grace, opts.poll, opts.size_poll))


class RunnerLogDocTests(unittest.TestCase):
    """The runner log is container output and can hold the cell's key; the
    docs must say so instead of promising that no log holds a key."""

    DOCS = (RUN_PY, os.path.join(WIRE_WATCH, "README.md"))

    # A sentence promising that a key never reaches any log.
    NO_KEY_IN_LOG_RE = r"(?i)(?:\bno key\b|\bnever\b)[^.]*\b(?:in a log|a log line)\b"

    def test_no_doc_claims_a_key_never_reaches_a_log(self) -> None:
        for path in self.DOCS:
            with self.subTest(os.path.basename(path)):
                with open(path, encoding="utf-8") as handle:
                    text = " ".join(handle.read().split())
                self.assertNotRegex(text, self.NO_KEY_IN_LOG_RE)

    def test_the_claim_pattern_fires_on_both_old_spellings(self) -> None:
        for planted in ("No key is ever put on a command line or in a log.",
                        "no key is ever placed in an argument vector or a log line."):
            with self.subTest(planted):
                self.assertRegex(planted, self.NO_KEY_IN_LOG_RE)

    def test_every_doc_warns_that_the_runner_log_is_untrusted_and_may_hold_the_key(self) -> None:
        for path in self.DOCS:
            with self.subTest(os.path.basename(path)):
                with open(path, encoding="utf-8") as handle:
                    text = " ".join(handle.read().split()).lower()
                for phrase in ("untrusted container output", "may contain the cell's key", "0600",
                               "must not be shared", "must not be printed to a terminal"):
                    self.assertIn(phrase, text)


class CapDocTests(unittest.TestCase):
    """Both caps count the scratch tree AND the runner log, and are polled;
    the docs must not describe them as scratch-only."""

    def doc_text(self, path: str) -> str:
        with open(path, encoding="utf-8") as handle:
            return " ".join(handle.read().split())

    def test_run_py_usage_names_the_runner_log_for_both_caps(self) -> None:
        with open(RUN_PY, encoding="utf-8") as handle:
            lines = handle.read().splitlines()
        for option in ("--cell-cap-bytes", "--run-cap-bytes"):
            with self.subTest(option):
                start = next(i for i, l in enumerate(lines) if l.startswith("  " + option))
                end = next(i for i in range(start + 1, len(lines)) if lines[i].startswith("  --"))
                self.assertIn("runner.log", " ".join(lines[start:end]))

    def test_readme_cap_rows_name_the_runner_log(self) -> None:
        with open(os.path.join(WIRE_WATCH, "README.md"), encoding="utf-8") as handle:
            rows = [l for l in handle if l.startswith("| `--cell-cap-bytes`") or l.startswith("| `--run-cap-bytes`")]
        self.assertEqual(len(rows), 2)
        for row in rows:
            with self.subTest(row[:20]):
                self.assertIn("runner.log", row)

    def test_both_docs_state_the_poll_overshoot(self) -> None:
        for path in (RUN_PY, os.path.join(WIRE_WATCH, "README.md")):
            with self.subTest(os.path.basename(path)):
                self.assertIn("write rate times --size-poll-seconds", self.doc_text(path))

    def test_no_doc_calls_a_cap_scratch_only(self) -> None:
        for path in (RUN_PY, os.path.join(WIRE_WATCH, "README.md")):
            with self.subTest(os.path.basename(path)):
                text = self.doc_text(path)
                self.assertNotIn("whose scratch tree grows past this", text)
                self.assertNotIn("the cells' scratch trees together reach this", text)
                self.assertNotIn("Run cap: the scratch trees are re-measured", text)


class KeyDirTests(unittest.TestCase):
    def test_default_key_dir_follows_xdg_then_home(self) -> None:
        self.assertEqual(run.default_key_dir({"XDG_CONFIG_HOME": "/x"}), "/x/routectl-wire-watch")
        self.assertEqual(run.default_key_dir({"XDG_CONFIG_HOME": ""}),
                         os.path.join(os.path.expanduser("~"), ".config", "routectl-wire-watch"))


STATUS_SRC = os.path.join(REPO, "crates", "routectl-cli", "src", "handlers", "status")


@unittest.skipUnless(os.path.isdir(STATUS_SRC), "crates/ absent: no source to weld against")
class TokenWeldTests(unittest.TestCase):
    def test_token_fields_are_usage_totals_fields(self) -> None:
        from test_evidence import _rust_struct_fields

        fields = _rust_struct_fields(os.path.join(STATUS_SRC, "usage.rs"), "UsageTotals")
        for name in ledger.TOKEN_FIELDS:
            with self.subTest(name):
                self.assertIn(name, fields)


# Imported by a fresh interpreter, so what the runner pulls in is measured,
# not what its source happens to spell.
NETWORK_PREFIXES = ("socket", "_socket", "http", "urllib", "ssl")
LOADED_NETWORK_MODULES = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
__import__(sys.argv[2])
prefixes = tuple(sys.argv[3:])
print(json.dumps(sorted(m for m in sys.modules if m.split(".")[0] in prefixes)))
"""
RUNNER_MODULES = ("run.py", "cell_process.py", "scratch_size.py", "launch_key.py")


def loaded_network_modules(directory: str, module: str) -> List[str]:
    done = subprocess.run(
        [sys.executable, "-I", "-c", LOADED_NETWORK_MODULES, directory, module, *NETWORK_PREFIXES],
        capture_output=True, text=True, timeout=60, check=True,
    )
    return json.loads(done.stdout)


class SourceHygieneTests(unittest.TestCase):
    # Assembled at run time, so this file never spells what it looks for.
    PORTS = (str(9000 + 100), str(8000 + 889))

    def port_hits(self, text: str) -> List[str]:
        return [p for p in self.PORTS if p in text]

    def test_importing_the_runner_loads_no_network_module(self) -> None:
        self.assertEqual(loaded_network_modules(WIRE_WATCH, "run"), [])

    def test_the_import_check_fires_on_a_planted_network_import(self) -> None:
        with tempfile.TemporaryDirectory() as planted:
            with open(os.path.join(planted, "planted.py"), "w", encoding="utf-8") as handle:
                handle.write("import os, %s\n" % ("soc" "ket"))
            self.assertIn("socket", loaded_network_modules(planted, "planted"))

    def test_the_runner_source_names_no_live_port(self) -> None:
        for name in RUNNER_MODULES:
            with self.subTest(name):
                with open(os.path.join(WIRE_WATCH, name), encoding="utf-8") as handle:
                    self.assertEqual(self.port_hits(handle.read()), [])

    def test_the_port_scan_fires_on_a_planted_control(self) -> None:
        self.assertEqual(len(self.port_hits("port = %s\nalt = %s\n" % self.PORTS)), 2)


if __name__ == "__main__":
    unittest.main()
