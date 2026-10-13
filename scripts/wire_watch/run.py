#!/usr/bin/env python3
"""Execute the wire-watch matrix, one capture container per cell, under bounds.

Stdlib only. Expands the committed denominator through matrix.py, writes
run.json before anything launches, then runs each PENDING cell through
scripts/container/run_capture.sh and records every move in run.json
(atomically, after each transition). No cell is ever retried: a cell's
first result is its result.

Usage
  run.py --report-dir D [--key-dir K] [--image TAG] [--concurrency N]
         [--cell-timeout S] [--deadline S] [--cell-cap-bytes N]
         [--run-cap-bytes N] [--seat FILE] [--only GLOB]...
         [--run-capture PATH] [--kill-grace S] [--poll-seconds S]
         [--size-poll-seconds S]

  --report-dir   where run.json, run.lock and cells/<cell-id>/ go. Created
                 0700 when absent; must be owned by this user, writable by
                 no one else, outside this repo, and hold no earlier run
  --key-dir      the key directory cells.tsv's key_file names resolve in.
                 Default $XDG_CONFIG_HOME/routectl-wire-watch, with
                 ~/.config standing in for an unset XDG_CONFIG_HOME
  --image        capture image tag, passed to run_capture.sh
  --concurrency  cells running at once (default 2)
  --cell-timeout seconds one cell may run before it is stopped -> TIMEOUT
  --deadline     seconds after the run starts past which no cell starts;
                 every cell still PENDING then is CAP_REACHED(deadline)
  --cell-cap-bytes  a cell whose scratch tree plus cells/<id>.runner.log
                 grows past this is stopped -> CAP_REACHED(size)
  --run-cap-bytes   once every cell's scratch tree plus runner.log together
                 reach this, no further cell starts -> CAP_REACHED(run-size)
  --seat         seat file run_capture.sh reads OAuth tokens from
                 (ROUTECTL_CAPTURE_CELL_SEAT); default is its own lookup
  --only         run only cells whose id matches this fnmatch glob
                 (repeatable); the others are not in run.json at all
  --run-capture  the runner to launch per cell (self-test seam)
  --kill-grace, --poll-seconds, --size-poll-seconds
                 tuning: the SIGTERM -> SIGKILL grace, and how often the
                 children and the scratch sizes are polled

Per cell. The cell gets its own scratch directory, <report-dir>/cells/<id>,
created 0700 here and passed as run_capture.sh's --scratch (the runner's
stdout and stderr go to cells/<id>.runner.log, 0600, beside it); evidence is
read from that HOST path only, never from a path the container named. The
container is named routectl-cell-<run-id>-<n>, n the cell's index in
run.json. A cell with a key file has its key re-checked and read through
matrix.open_key_file at launch and exported to the child as
ROUTECTL_DRIVER_<PROVIDER>_API_KEY; the child's environment is built from
an allowlist, so no other credential reaches it. The key is never placed
in an argument vector or in run.py's own log. cells/<id>.runner.log is
untrusted container output and may contain the cell's key: it is 0600, must
not be shared, and must not be printed to a terminal.

Stop sequence (timeout, size cap, interrupt), in cell_process.py: SIGTERM
to the cell's process group, grace, SIGKILL, `docker rm -f <name>`, then a
bounded reap. A runner that ended on its own other than cleanly (0, 2-20)
also has its container removed. However the run ends (normally, on a
signal, or on an exception) one sweep removes every container named
routectl-cell-<run-id>-* and records "leftover-containers" or
"sweep-failed" as run_stop; the first run_stop stands, later ones are
logged. A signal received at any point gives that signal's exit code.

Exit code -> state, in precedence order:
  9 (the daemon survived SIGKILL)  EXITED(9); a stop this script made
                                   rides in detail
  stopped here                     TIMEOUT | CAP_REACHED(size) | INTERRUPTED
  leader not reaped                INTERRUPTED, detail unreaped
  20 (credential refused by name)  NOT_RUN(credential)
  129 / 130 / 143, or a signal     INTERRUPTED (signalled by someone else)
  anything else                    EXITED(rc), recorded as is
An EXITED(5) cell whose traced ingress tokens lack its expected ingress
marks every PENDING cell of the same (driver, lane, connection mode)
NOT_RUN(ingress). An EXITED cell's token counts come from its usage
snapshot (run.json `tokens`, null when its evidence is incomplete).

Caps: each cell is charged its scratch tree plus cells/<id>.runner.log.
Sizes are polled, not enforced per write, so a cell can overshoot a cap by
about its write rate times --size-poll-seconds. Every running cell is
re-measured before a launch; once the total reaches --run-cap-bytes, every unstarted cell is CAP_REACHED(run-size); when
none is left to carry that, the next cell to exit cleanly is recorded with
detail run-size, which ledger.bound_stopped counts. Either way the first
crossing sets run.json's top-level run_stop to "run-size".

Exit status: 0 when every cell is terminal, none was stopped by a bound or
a signal and run_stop is null; 1 otherwise; 2 usage; 3 another run holds
the lock; 4 the report dir is refused; 128 + signum on SIGHUP, SIGINT,
SIGQUIT, SIGUSR1, SIGUSR2, SIGALRM or SIGTERM (e.g. 129 / 130 / 131 / 143),
each of which stops every running cell and sweeps first.
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import fnmatch
import logging
import math
import os
import re
import secrets
import signal
import stat
import subprocess
import sys
import time
from dataclasses import dataclass, replace
from typing import Dict, List, Optional, Sequence, Tuple

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, HERE)

import cell_process  # noqa: E402
import evidence  # noqa: E402
import launch_key  # noqa: E402
import ledger  # noqa: E402
import matrix  # noqa: E402
import scratch_size  # noqa: E402
from ledger import CapReason, CellSpec, CellState, NotRunReason, RunLedger  # noqa: E402

LOG = logging.getLogger("wire_watch.run")

EXIT_OK = 0
EXIT_INCOMPLETE = 1
EXIT_USAGE = 2
EXIT_LOCKED = 3
EXIT_REPORT_DIR = 4
# Every signal whose default action would end run.py without its sweep.
EXIT_FOR_SIGNAL = {
    sig: 128 + int(sig)
    for sig in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGUSR1, signal.SIGUSR2, signal.SIGALRM, signal.SIGTERM)
}

DEFAULT_RUN_CAPTURE = os.path.join(REPO, "scripts", "container", "run_capture.sh")
DEFAULT_IMAGE = "routectl-capture:default"
KEY_DIR_NAME = "routectl-wire-watch"
DEFAULT_CONCURRENCY = 2
DEFAULT_CELL_TIMEOUT_S = 30 * 60
DEFAULT_DEADLINE_S = 12 * 60 * 60
DEFAULT_CELL_CAP_BYTES = 256 * 1024 * 1024
DEFAULT_RUN_CAP_BYTES = 8 * 1024 * 1024 * 1024
DEFAULT_KILL_GRACE_S = 10.0
DEFAULT_POLL_S = 0.2
DEFAULT_SIZE_POLL_S = 2.0

RUN_JSON = "run.json"
RUN_LOCK = "run.lock"
CELLS_DIR = "cells"
IN_CONTAINER_DRIVERS = "/workspace/scripts/drivers"

# run_capture.sh's --name grammar; a name outside it is a usage error there.
CELL_NAME_PREFIX = "routectl-cell-"
CELL_NAME_RE = re.compile(r"^routectl-cell-[a-z0-9][a-z0-9_.-]{0,62}$")

# Exit codes with a fixed meaning (capture_driver.sh / run_capture.sh).
RC_DAEMON_SURVIVED = 9
RC_INGRESS_OR_REFUSAL = 5
RC_CREDENTIAL_REFUSED = 20
RC_SIGNALLED = frozenset({129, 130, 143})
# The codes a runner that ended on its own returns after its container is
# gone (or before it ever started): capture_driver.sh's 0 and 2-9 and
# run_capture.sh's pre-docker refusals 10-20. Any other end, a signal from
# outside included, may have left the container running.
RC_CLEAN_EXIT = frozenset({0, *range(2, 21)})

# What the child (run_capture.sh, then docker) may inherit: docker's own
# client settings and the routectl binary selector. Everything else,
# credentials included, is dropped.
CHILD_ENV_ALLOW = (
    "PATH",
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    "DOCKER_CONTEXT",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "ROUTECTL_BIN",
)
SEAT_ENV = "ROUTECTL_CAPTURE_CELL_SEAT"


class Refused(Exception):
    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class Options:
    report_dir: str
    key_dir: str
    image: str
    concurrency: int
    cell_timeout: float
    deadline: float
    cell_cap_bytes: int
    run_cap_bytes: int
    seat: Optional[str]
    only: Tuple[str, ...]
    run_capture: str
    kill_grace: float
    poll: float
    size_poll: float


@dataclass
class Running:
    """One launched cell. Mutable: only the run loop owns it."""

    spec: CellSpec
    name: str
    scratch: str
    proc: "subprocess.Popen[bytes]"
    started: float
    size: int = 0
    measured: float = 0.0


def default_key_dir(environ: Dict[str, str]) -> str:
    base = environ.get("XDG_CONFIG_HOME") or os.path.join(os.path.expanduser("~"), ".config")
    return os.path.join(base, KEY_DIR_NAME)


def new_run_id() -> str:
    """Lowercase UTC timestamp plus a nonce: [0-9a-z-] only, 22 chars."""
    return time.strftime("%Y%m%dt%H%M%S", time.gmtime()) + "-" + secrets.token_hex(3)


def container_name(run_id: str, index: int) -> str:
    name = f"{CELL_NAME_PREFIX}{run_id}-{index}"
    if not CELL_NAME_RE.fullmatch(name):
        raise ValueError(f"container name {name!r} is outside run_capture.sh's --name grammar")
    return name


def key_env_name(provider: str) -> str:
    return "ROUTECTL_DRIVER_%s_API_KEY" % provider.upper().replace("-", "_")


def child_env(base: Dict[str, str], seat: Optional[str], key: Optional[Tuple[str, str]]) -> Dict[str, str]:
    env = {name: base[name] for name in CHILD_ENV_ALLOW if name in base}
    if seat is not None:
        env[SEAT_ENV] = seat
    if key is not None:
        env[key[0]] = key[1]
    return env


def capture_argv(opts: Options, spec: CellSpec, name: str, scratch: str) -> List[str]:
    argv = [opts.run_capture, "--scratch", scratch, "--image", opts.image, "--name", name]
    for provider in spec.providers:
        argv += ["--provider", provider]
    argv += [
        "--",
        "--lane", spec.lane,
        "--case", spec.case,
        "--expected-ingress", spec.expected_ingress,
        "--connection-mode", spec.connection_mode,
        "--",
        "env", f"ROUTECTL_DRIVER_REQUEST_MODEL={spec.model}",
        f"{IN_CONTAINER_DRIVERS}/{spec.driver}",
    ]
    return argv


# ---------------------------------------------------------------------
# Exit classification
# ---------------------------------------------------------------------

STOP_TIMEOUT = "timeout"
STOP_SIZE = "size"
STOP_INTERRUPT = "interrupted"
OVER_RUN_CAP = CapReason.RUN_SIZE.value


@dataclass(frozen=True)
class Verdict:
    state: CellState
    rc: Optional[int] = None
    reason: Optional[str] = None
    detail: Optional[str] = None


def classify(rc: Optional[int], stopped: Optional[str]) -> Verdict:
    """The ledger move for a cell whose runner ended with `rc` (None when
    its leader could not be reaped), given the stop this script made (None
    when it ended on its own)."""
    if rc == RC_DAEMON_SURVIVED:
        return Verdict(CellState.EXITED, rc=rc, detail=stopped)
    if stopped == STOP_TIMEOUT:
        return Verdict(CellState.TIMEOUT)
    if stopped == STOP_SIZE:
        return Verdict(CellState.CAP_REACHED, reason=CapReason.SIZE.value)
    if stopped == STOP_INTERRUPT:
        return Verdict(CellState.INTERRUPTED)
    if rc is None:
        return Verdict(CellState.INTERRUPTED, detail="unreaped")
    if rc == RC_CREDENTIAL_REFUSED:
        return Verdict(CellState.NOT_RUN, reason=NotRunReason.CREDENTIAL.value, detail="refused")
    if rc < 0:
        return Verdict(CellState.INTERRUPTED, detail=f"signal-{-rc}")
    if rc in RC_SIGNALLED:
        return Verdict(CellState.INTERRUPTED, detail=f"rc-{rc}")
    return Verdict(CellState.EXITED, rc=rc)


def ingress_mismatch(scratch: str, expected: str) -> bool:
    """True only when the cell's traced ingress tokens are readable and lack
    the expected one; unreadable evidence proves nothing either way."""
    kinds = evidence.read_ingress_kinds(os.path.join(scratch, evidence.EVIDENCE_DIRNAME))
    return kinds is not None and expected not in kinds


def cell_tokens(scratch: str) -> Optional[ledger.Tokens]:
    result = evidence.read_cell(scratch)
    if isinstance(result, evidence.Incomplete):
        return None
    return ledger.tokens_from_totals(result.usage_totals)


# ---------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------

SWEEP_STOPS = {
    cell_process.SWEEP_FAILED: ledger.RunStop.SWEEP_FAILED,
    cell_process.SWEEP_LEFTOVERS: ledger.RunStop.LEFTOVER_CONTAINERS,
}


class Signals:
    """The first EXIT_FOR_SIGNAL signal received. The handler only records
    it; the run loop acts on it, so no transition is ever cut in half."""

    def __init__(self) -> None:
        self.received: Optional[int] = None

    def install(self) -> None:
        for signum in EXIT_FOR_SIGNAL:
            signal.signal(signum, self._handle)

    def _handle(self, signum: int, frame: object) -> None:
        del frame
        if self.received is None:
            self.received = signum


class Run:
    def __init__(
        self, opts: Options, led: RunLedger, run_id: str, environ: Dict[str, str], signals: Signals
    ) -> None:
        self.opts = opts
        self.led = led
        self.run_id = run_id
        self.environ = environ
        self.path = os.path.join(opts.report_dir, RUN_JSON)
        self.cells_dir = os.path.join(opts.report_dir, CELLS_DIR)
        self.index = {e.spec.id: i for i, e in enumerate(led.entries)}
        self.pending = [e.spec.id for e in led.entries if e.state is CellState.PENDING]
        self.running: Dict[str, Running] = {}
        self.done_bytes = 0
        self.signals = signals
        self.run_cap_marked_on_cell = False
        self.run_cap_crossed = False

    def move(self, cell_id: str, verdict: Verdict) -> None:
        self.led = self.led.transition(
            cell_id, verdict.state, rc=verdict.rc, reason=verdict.reason, detail=verdict.detail
        )
        ledger.write_atomic(self.path, self.led)
        entry = self.led.entry(cell_id)
        LOG.info(
            "event=cell-state cell=%s state=%s rc=%s reason=%s detail=%s",
            cell_id, entry.state.value, entry.rc, entry.reason, entry.detail,
        )

    def cap_pending(self, reason: CapReason) -> None:
        for cell_id in self.pending:
            self.move(cell_id, Verdict(CellState.CAP_REACHED, reason=reason.value))
            if reason is CapReason.RUN_SIZE:
                self.run_cap_marked_on_cell = True
        self.pending = []

    def note_run_size(self) -> bool:
        """Whether the cells' scratch trees together have reached the run
        cap, from the sizes as last measured. The first crossing is written
        to run.json as run_stop, so it is on record whatever any cell says."""
        if self.run_cap_crossed:
            return True
        used = self.done_bytes + sum(r.size for r in self.running.values())
        if used < self.opts.run_cap_bytes:
            return False
        self.run_cap_crossed = True
        LOG.error("event=run-cap-crossed run=%s used=%d cap=%d", self.run_id, used, self.opts.run_cap_bytes)
        self.record_run_stop(ledger.RunStop.RUN_SIZE)
        return True

    def record_run_stop(self, stop: ledger.RunStop) -> None:
        """Write the run-level stop to run.json now. The first one stands;
        a later one is only logged."""
        if self.led.run_stop is not None:
            LOG.error(
                "event=run-stop-not-recorded run=%s kept=%s also=%s", self.run_id, self.led.run_stop.value, stop.value
            )
            return
        self.led = self.led.with_run_stop(stop)
        ledger.write_atomic(self.path, self.led)

    def measure(self, run: Running) -> int:
        """The cell's scratch bytes plus its runner log, bounded only by the
        larger cap, so the figure that feeds the run total is never the cell
        cap's sentinel. The log is run.py's own file, out of the container's
        reach, so a plain lstat of it is trusted."""
        bound = max(self.opts.cell_cap_bytes, self.opts.run_cap_bytes)
        run.size = scratch_size.tree_size(run.scratch, bound) + scratch_size.runner_log_bytes(self.cells_dir, run.spec.id)
        run.measured = time.monotonic()
        return run.size

    def refresh_sizes(self) -> None:
        for run in self.running.values():
            self.measure(run)

    def admission_closed(self, deadline: float) -> bool:
        """Read at the last moment before a runner starts: a signal leaves
        the pending cells to interrupt(), a passed deadline caps them."""
        if self.signals.received is not None:
            return True
        if time.monotonic() >= deadline:
            self.cap_pending(CapReason.DEADLINE)
            return True
        return False

    def launch(self, spec: CellSpec, deadline: float) -> None:
        """Start one cell, or decline to. Every preparation step (key read,
        scratch dir, runner log) comes first. Then the handled signals are
        blocked, admission_closed() is checked a final time, the runner is
        spawned and the cell registered in self.running, and the mask is
        restored. That final check is the linearization point: a cell is
        never started after a deadline or a signal observable there, and a
        signal arriving after it is delivered only once the cell is
        tracked, so interrupt() stops it and the sweep removes its
        container. RUNNING is recorded only after the runner started."""
        key: Optional[Tuple[str, str]] = None
        if spec.key_file is not None:
            value, problem = launch_key.read_key(self.opts.key_dir, spec.key_file)
            if value is None:
                self.move(spec.id, Verdict(CellState.NOT_RUN, reason=NotRunReason.CREDENTIAL.value, detail=problem))
                return
            key = (key_env_name(spec.providers[0]), value)
        scratch = os.path.join(self.cells_dir, spec.id)
        name = container_name(self.run_id, self.index[spec.id])
        os.mkdir(scratch, 0o700)
        log_fd: Optional[int] = None
        try:
            log_fd = cell_process.open_runner_log(self.cells_dir, spec.id)
            argv, env = capture_argv(self.opts, spec, name, scratch), child_env(self.environ, self.opts.seat, key)
            if not self.spawn_admitted(spec, name, scratch, argv, env, log_fd, deadline):
                return
        except OSError as exc:
            LOG.error("event=launch-failed cell=%s error=%s", spec.id, exc.__class__.__name__)
            self.move(spec.id, Verdict(CellState.NOT_RUN, reason=NotRunReason.DRIVER.value, detail="launch-failed"))
            return
        finally:
            self.close_runner_log(spec.id, log_fd)
        self.move(spec.id, Verdict(CellState.RUNNING))
        LOG.info("event=launch cell=%s name=%s pid=%d", spec.id, name, self.running[spec.id].proc.pid)

    def spawn_admitted(
        self, spec: CellSpec, name: str, scratch: str, argv: List[str], env: Dict[str, str], log_fd: int,
        deadline: float,
    ) -> bool:
        """The final check, the spawn and the registration, with the handled
        signals deferred across all three. False when admission is closed:
        the cell is left PENDING for interrupt() or capped by the deadline."""
        blocked = signal.pthread_sigmask(signal.SIG_BLOCK, EXIT_FOR_SIGNAL)
        try:
            self.pending.insert(0, spec.id)
            if self.admission_closed(deadline):
                return False
            self.pending.pop(0)
            proc = cell_process.spawn_runner(argv, env, log_fd, EXIT_FOR_SIGNAL)
            self.running[spec.id] = Running(spec=spec, name=name, scratch=scratch, proc=proc, started=time.monotonic())
            return True
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, blocked)

    @staticmethod
    def close_runner_log(cell_id: str, log_fd: Optional[int]) -> None:
        if log_fd is None:
            return
        try:
            os.close(log_fd)
        except OSError as exc:
            LOG.error("event=runner-log-close-failed cell=%s error=%s", cell_id, exc.__class__.__name__)

    def finish(self, run: Running, stopped: Optional[str]) -> None:
        if stopped is None:
            rc = cell_process.end_exited(run.proc)
            if rc not in RC_CLEAN_EXIT:
                cell_process.remove_container(run.name, child_env(self.environ, None, None))
        else:
            rc = self.stop_cell(run)
        del self.running[run.spec.id]
        self.done_bytes += self.measure(run)
        if stopped is None and run.size > self.opts.cell_cap_bytes and rc != RC_DAEMON_SURVIVED:
            stopped = STOP_SIZE
        verdict = classify(rc, stopped)
        if (
            self.note_run_size()
            and not self.run_cap_marked_on_cell
            and not self.pending
            and verdict.state is CellState.EXITED
            and verdict.detail is None
        ):
            verdict = replace(verdict, detail=OVER_RUN_CAP)
            self.run_cap_marked_on_cell = True
        self.move(run.spec.id, verdict)
        if verdict.state is not CellState.EXITED:
            return
        tokens = cell_tokens(run.scratch)
        if tokens is not None:
            self.led = self.led.with_tokens(run.spec.id, tokens)
            ledger.write_atomic(self.path, self.led)
        if rc == RC_INGRESS_OR_REFUSAL and ingress_mismatch(run.scratch, run.spec.expected_ingress):
            self.mark_pairing_not_run(run.spec)

    def stop_cell(self, run: Running) -> Optional[int]:
        return cell_process.stop(run.proc, self.opts.kill_grace, self.opts.poll, run.name, child_env(self.environ, None, None))

    def mark_pairing_not_run(self, spec: CellSpec) -> None:
        pairing = (spec.driver, spec.lane, spec.connection_mode)
        keep = []
        for cell_id in self.pending:
            other = self.led.entry(cell_id).spec
            if (other.driver, other.lane, other.connection_mode) == pairing:
                self.move(cell_id, Verdict(CellState.NOT_RUN, reason=NotRunReason.INGRESS.value, detail="mismatch"))
            else:
                keep.append(cell_id)
        self.pending = keep

    def poll_running(self) -> None:
        """The clock and the signal flag are re-read before each cell: one
        cell's size walk must not delay another's timeout or a signal."""
        for run in list(self.running.values()):
            if self.signals.received is not None:
                return
            now = time.monotonic()
            if cell_process.leader_exited(run.proc):
                self.finish(run, None)
                continue
            if now - run.started >= self.opts.cell_timeout:
                self.finish(run, STOP_TIMEOUT)
                continue
            if now - run.measured >= self.opts.size_poll:
                if self.measure(run) > self.opts.cell_cap_bytes:
                    self.finish(run, STOP_SIZE)
        self.note_run_size()

    def fill_slots(self, deadline: float) -> None:
        while self.pending and len(self.running) < self.opts.concurrency:
            if self.admission_closed(deadline):
                return
            # The walk (and the key read in launch) can take long enough for
            # the deadline to pass or a signal to arrive; launch() decides
            # admission again right before the runner starts.
            self.refresh_sizes()
            if self.note_run_size():
                self.cap_pending(CapReason.RUN_SIZE)
                return
            spec = self.led.entry(self.pending.pop(0)).spec
            self.launch(spec, deadline)

    def interrupt(self) -> None:
        for run in list(self.running.values()):
            self.finish(run, STOP_INTERRUPT)
        for cell_id in self.pending:
            self.move(cell_id, Verdict(CellState.INTERRUPTED))
        self.pending = []

    def execute(self) -> int:
        """Run the loop, then, on every way out of it (normal, signal or
        exception), stop what is still running and sweep this run's
        containers exactly once. A failure while closing out is logged and
        never replaces an exception the loop raised."""
        try:
            self._execute()
        finally:
            closed_clean = self.close_out()
        if self.signals.received is not None:
            return EXIT_FOR_SIGNAL.get(signal.Signals(self.signals.received), EXIT_INCOMPLETE)
        status = self.exit_status()
        return status if closed_clean else EXIT_INCOMPLETE

    def close_out(self) -> bool:
        """True only when the sweep confirmed no container of this run."""
        prefix = f"{CELL_NAME_PREFIX}{self.run_id}-"
        result = cell_process.close_out(self.abandon_running, prefix, child_env(self.environ, None, None))
        stop = SWEEP_STOPS.get(result)
        if stop is None:
            return True
        LOG.error("event=run-end run=%s status=incomplete why=%s", self.run_id, stop.value)
        try:
            self.record_run_stop(stop)
        except Exception as exc:
            # Best effort: the write can fail as the loop did (a full disk),
            # and the loop's own exception must be the one that propagates.
            LOG.error("event=run-stop-write-failed run=%s stop=%s error=%s", self.run_id, stop.value, exc.__class__.__name__)
        return False

    def abandon_running(self) -> None:
        """Stop any cell the loop left running, without touching run.json."""
        for run in list(self.running.values()):
            self.stop_cell(run)
            del self.running[run.spec.id]
            LOG.error("event=cell-abandoned cell=%s", run.spec.id)

    def _execute(self) -> None:
        deadline = time.monotonic() + self.opts.deadline
        LOG.info("event=run-start run=%s cells=%d pending=%d", self.run_id, len(self.led.entries), len(self.pending))
        while (self.pending or self.running) and self.signals.received is None:
            self.fill_slots(deadline)
            self.poll_running()
            if self.pending or self.running:
                time.sleep(self.opts.poll)
        if self.signals.received is not None:
            self.interrupt()

    def exit_status(self) -> int:
        entries = self.led.entries
        if self.led.run_stop is not None:
            LOG.info("event=run-end run=%s status=incomplete run_stop=%s", self.run_id, self.led.run_stop.value)
            return EXIT_INCOMPLETE
        if all(e.state in ledger.TERMINAL for e in entries) and not any(ledger.bound_stopped(e) for e in entries):
            LOG.info("event=run-end run=%s status=complete", self.run_id)
            return EXIT_OK
        LOG.info("event=run-end run=%s status=incomplete", self.run_id)
        return EXIT_INCOMPLETE


# ---------------------------------------------------------------------
# Report dir and lock
# ---------------------------------------------------------------------


def prepare_report_dir(path: str) -> str:
    absolute = os.path.abspath(path)
    real = os.path.realpath(absolute)
    repo = os.path.realpath(REPO)
    if real == repo or real.startswith(repo + os.sep):
        raise Refused(EXIT_REPORT_DIR, "the report dir is inside this repo, which the capture cell mounts read-only")
    try:
        os.makedirs(absolute, mode=0o700, exist_ok=True)
        info = os.lstat(absolute)
    except OSError as exc:
        raise Refused(EXIT_REPORT_DIR, f"the report dir cannot be created: {exc.strerror}") from None
    if not stat.S_ISDIR(info.st_mode):
        raise Refused(EXIT_REPORT_DIR, "the report dir is not a directory (or is a symlink)")
    if info.st_uid != os.geteuid() or info.st_mode & 0o022:
        raise Refused(EXIT_REPORT_DIR, "the report dir must be owned by this user and writable by no one else")
    return absolute


def take_lock(report_dir: str) -> int:
    flags = os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | getattr(os, "O_CLOEXEC", 0)
    try:
        fd = os.open(os.path.join(report_dir, RUN_LOCK), flags, 0o600)
    except OSError as exc:
        raise Refused(EXIT_REPORT_DIR, f"the run lock cannot be opened: {exc.strerror}") from None
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as exc:
        os.close(fd)
        if exc.errno in (errno.EWOULDBLOCK, errno.EAGAIN):
            raise Refused(EXIT_LOCKED, "another run holds this report dir") from None
        raise Refused(EXIT_REPORT_DIR, f"the run lock cannot be taken: {exc.strerror}") from None
    return fd


def refuse_earlier_run(report_dir: str) -> None:
    for name in (RUN_JSON, CELLS_DIR):
        if os.path.lexists(os.path.join(report_dir, name)):
            raise Refused(EXIT_REPORT_DIR, f"the report dir already holds {name}; a paid cell is never re-run")


def plan(opts: Options) -> RunLedger:
    paths = matrix.Paths.from_root(REPO)
    specs = matrix.expand(paths, matrix.load_rows(paths))
    if opts.only:
        specs = tuple(s for s in specs if any(fnmatch.fnmatchcase(s.id, g) for g in opts.only))
        if not specs:
            raise Refused(EXIT_USAGE, "--only matches no cell")
    led = RunLedger.new(matrix.run_inputs(paths), specs)
    return matrix.preflight(led, paths, opts.key_dir)


# ---------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------


def _positive(kind: type):
    def parse(text: str):
        value = kind(text)
        # nan compares false with everything, so finiteness is checked first.
        if not math.isfinite(value) or value <= 0:
            raise argparse.ArgumentTypeError("must be a finite positive number")
        return value

    return parse


def parse_args(argv: Sequence[str], environ: Dict[str, str]) -> Options:
    p = argparse.ArgumentParser(prog="run.py", description="Run the wire-watch matrix under bounds.")
    p.add_argument("--report-dir", required=True)
    p.add_argument("--key-dir", default=None)
    p.add_argument("--image", default=DEFAULT_IMAGE)
    p.add_argument("--concurrency", type=_positive(int), default=DEFAULT_CONCURRENCY)
    p.add_argument("--cell-timeout", type=_positive(float), default=DEFAULT_CELL_TIMEOUT_S)
    p.add_argument("--deadline", type=_positive(float), default=DEFAULT_DEADLINE_S)
    p.add_argument("--cell-cap-bytes", type=_positive(int), default=DEFAULT_CELL_CAP_BYTES)
    p.add_argument("--run-cap-bytes", type=_positive(int), default=DEFAULT_RUN_CAP_BYTES)
    p.add_argument("--seat", default=None)
    p.add_argument("--only", action="append", default=[])
    p.add_argument("--run-capture", default=DEFAULT_RUN_CAPTURE)
    p.add_argument("--kill-grace", type=_positive(float), default=DEFAULT_KILL_GRACE_S)
    p.add_argument("--poll-seconds", type=_positive(float), default=DEFAULT_POLL_S)
    p.add_argument("--size-poll-seconds", type=_positive(float), default=DEFAULT_SIZE_POLL_S)
    ns = p.parse_args(list(argv))
    run_capture = os.path.abspath(ns.run_capture)
    if not (os.path.isfile(run_capture) and os.access(run_capture, os.X_OK)):
        p.error("--run-capture is not an executable file")
    return Options(
        report_dir=ns.report_dir,
        key_dir=ns.key_dir if ns.key_dir is not None else default_key_dir(environ),
        image=ns.image,
        concurrency=ns.concurrency,
        cell_timeout=ns.cell_timeout,
        deadline=ns.deadline,
        cell_cap_bytes=ns.cell_cap_bytes,
        run_cap_bytes=ns.run_cap_bytes,
        seat=os.path.abspath(ns.seat) if ns.seat is not None else None,
        only=tuple(ns.only),
        run_capture=run_capture,
        kill_grace=ns.kill_grace,
        poll=ns.poll_seconds,
        size_poll=ns.size_poll_seconds,
    )


def main(argv: Sequence[str]) -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s %(message)s", stream=sys.stderr)
    environ = dict(os.environ)
    opts = parse_args(argv, environ)
    try:
        report_dir = prepare_report_dir(opts.report_dir)
        opts = replace(opts, report_dir=report_dir)
        lock_fd = take_lock(report_dir)
    except Refused as refused:
        LOG.error("event=refused code=%d why=%r", refused.code, str(refused))
        return refused.code
    signals = Signals()
    signals.install()
    try:
        refuse_earlier_run(report_dir)
        led = plan(opts)
        run = Run(opts, led, new_run_id(), environ, signals)
        ledger.write_atomic(run.path, led)
        os.mkdir(run.cells_dir, 0o700)
        return run.execute()
    except Refused as refused:
        LOG.error("event=refused code=%d why=%r", refused.code, str(refused))
        return refused.code
    except matrix.MatrixError as exc:
        for failure in exc.failures:
            LOG.error("event=matrix-refused failure=%r", failure)
        return EXIT_USAGE
    except (ledger.LedgerError, matrix.lane_configs.ParseError) as exc:
        LOG.error("event=input-refused error=%r", str(exc))
        return EXIT_USAGE
    except OSError as exc:
        LOG.error("event=io-failed error=%s path=%r", exc.__class__.__name__, exc.filename)
        return EXIT_INCOMPLETE
    finally:
        os.close(lock_fd)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
