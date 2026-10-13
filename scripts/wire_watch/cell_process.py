"""Stop and reap one capture cell's process group and remove its container.

Stdlib only. Each cell's runner is the leader of its own process group
(started with start_new_session), so its pid is also the group id.

Public API
  leader_exited(proc)            exited, without reaping it
  end_exited(proc)               SIGKILL what is left of an exited leader's
                                 group, then reap the leader
  stop(proc, grace, poll, name, env)
                                 SIGTERM, grace, SIGKILL, `docker rm -f`, reap
  remove_container(name, env)    `docker rm -f <name>`; failures are logged
  leftover_containers(prefix, env)
                                 ids of containers named prefix*, any state
  sweep(prefix, env)             remove every container named prefix*;
                                 SWEEP_CLEAN, SWEEP_LEFTOVERS or SWEEP_FAILED
  open_runner_log(cells_dir, cell_id)
                                 a fresh 0600 fd for the runner's output
  spawn_runner(argv, env, log_fd, unblock)
                                 start a runner as its own group leader
                                 with the signals in `unblock` unblocked
  close_out(stop_running, prefix, env)
                                 stop_running(), then sweep(); neither
                                 failing keeps the other from running
Every docker call is bounded and runs in a session of its own.
Every reap is bounded: a leader that cannot be reaped (stuck in
uninterruptible sleep) is logged and left, and its code is None.
"""

from __future__ import annotations

import logging
import os
import re
import signal
import subprocess
import sys
import time
from typing import Callable, Dict, Iterable, List, Optional

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from scratch_size import RUNNER_LOG_SUFFIX  # noqa: E402

LOG = logging.getLogger("wire_watch.cell_process")

DOCKER_TIMEOUT_S = 30
# Full ids only: `docker rm -f X` resolves X as a name before an id prefix,
# so a short id could name a decoy container called like it.
CONTAINER_ID_RE = re.compile(r"^[0-9a-f]{64}$")
REAP_TIMEOUT_S = 5.0

SWEEP_CLEAN = "clean"
SWEEP_LEFTOVERS = "leftovers"
SWEEP_FAILED = "failed"


def _signal_group(pgid: int, sig: int) -> None:
    try:
        os.killpg(pgid, sig)
    except ProcessLookupError:
        pass
    except PermissionError:
        LOG.warning("event=signal-refused pgid=%d signal=%d", pgid, sig)


def leader_exited(proc: "subprocess.Popen[bytes]") -> bool:
    """Whether the leader has exited, WITHOUT reaping it: an unreaped
    leader keeps its pid, and so its process group id, from being reused
    before the group is killed."""
    try:
        info = os.waitid(os.P_PID, proc.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    except ChildProcessError:
        return True
    return info is not None


def _reap(proc: "subprocess.Popen[bytes]") -> Optional[int]:
    try:
        return proc.wait(timeout=REAP_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        LOG.error("event=reap-timeout pid=%d", proc.pid)
        return None


def end_exited(proc: "subprocess.Popen[bytes]") -> Optional[int]:
    _signal_group(proc.pid, signal.SIGKILL)
    return _reap(proc)


def stop(proc: "subprocess.Popen[bytes]", grace: float, poll: float, name: str, env: Dict[str, str]) -> Optional[int]:
    """The container is removed before the leader is reaped: a leader stuck
    in uninterruptible sleep would otherwise hold the kill back forever."""
    _signal_group(proc.pid, signal.SIGTERM)
    end = time.monotonic() + grace
    while not leader_exited(proc) and time.monotonic() < end:
        time.sleep(min(poll, grace))
    _signal_group(proc.pid, signal.SIGKILL)
    remove_container(name, env)
    return _reap(proc)


def _docker(args: List[str], env: Dict[str, str]) -> "subprocess.CompletedProcess[str]":
    """One bounded docker client call, in a session of its own so a second
    Ctrl-C at run.py's terminal cannot kill it midway."""
    return subprocess.run(
        ["docker", *args],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        timeout=DOCKER_TIMEOUT_S,
        check=False,
        start_new_session=True,
    )


def remove_container(name: str, env: Dict[str, str]) -> bool:
    """`docker rm -f <name>`: kills a running container and removes a
    created-but-never-started one, which `docker kill` refuses and --rm
    never removes (its environment still holds the cell's key). An absent
    container is not an error."""
    try:
        done = _docker(["rm", "-f", name], env)
    except (OSError, subprocess.TimeoutExpired) as exc:
        LOG.error("event=container-remove-failed name=%s error=%s", name, exc.__class__.__name__)
        return False
    if done.returncode != 0:
        LOG.error("event=container-remove-failed name=%s rc=%d", name, done.returncode)
        return False
    LOG.info("event=container-removed name=%s", name)
    return True


def leftover_containers(name_prefix: str, env: Dict[str, str]) -> Optional[List[str]]:
    """Ids of every container, in any state, whose name starts with
    name_prefix; None when docker could not be asked."""
    try:
        done = _docker(["ps", "-aq", "--no-trunc", "--filter", f"name=^/{name_prefix}"], env)
    except (OSError, subprocess.TimeoutExpired) as exc:
        LOG.error("event=container-sweep-failed error=%s", exc.__class__.__name__)
        return None
    if done.returncode != 0:
        LOG.error("event=container-sweep-failed rc=%d", done.returncode)
        return None
    ids = [line.strip() for line in done.stdout.splitlines() if line.strip()]
    if not all(CONTAINER_ID_RE.fullmatch(i) for i in ids):
        LOG.error("event=container-sweep-failed why=unexpected-output")
        return None
    return ids


def sweep(name_prefix: str, env: Dict[str, str]) -> str:
    """Remove every container whose name starts with name_prefix, then list
    again. SWEEP_CLEAN when docker reports none, SWEEP_LEFTOVERS when some
    were found and are confirmed gone, SWEEP_FAILED when docker could not be
    asked, a removal failed, or a container is still listed afterwards."""
    leftovers = leftover_containers(name_prefix, env)
    if leftovers is None:
        return SWEEP_FAILED
    if not leftovers:
        return SWEEP_CLEAN
    all_removed = True
    for container in leftovers:
        LOG.error("event=leftover-container id=%s", container)
        all_removed = remove_container(container, env) and all_removed
    remaining = leftover_containers(name_prefix, env)
    if remaining is None:
        return SWEEP_FAILED
    for container in remaining:
        LOG.error("event=container-survived-removal id=%s", container)
    return SWEEP_LEFTOVERS if all_removed and not remaining else SWEEP_FAILED


def close_out(stop_running: Callable[[], None], name_prefix: str, env: Dict[str, str]) -> str:
    """Run on every way out of a run, an exception included, so neither
    step may raise: a crashed stop must not skip the sweep, and a crashed
    sweep reads as SWEEP_FAILED."""
    try:
        stop_running()
    except Exception:
        LOG.exception("event=stop-running-failed")
    try:
        return sweep(name_prefix, env)
    except Exception:
        LOG.exception("event=sweep-crashed")
        return SWEEP_FAILED


def open_runner_log(cells_dir: str, cell_id: str) -> int:
    """A fresh 0600 file for the runner's stdout and stderr, beside the
    cell's scratch and not inside it: the scratch is the container's
    writable mount, and container output must not reach the operator's
    terminal."""
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | getattr(os, "O_CLOEXEC", 0)
    return os.open(os.path.join(cells_dir, cell_id + RUNNER_LOG_SUFFIX), flags, 0o600)


def spawn_runner(
    argv: List[str], env: Dict[str, str], log_fd: int, unblock: Iterable[int]
) -> "subprocess.Popen[bytes]":
    """The caller may hold signals blocked across this call; a child
    inherits the blocked mask through fork and exec (restore_signals does
    not touch it), so the child unblocks them itself before exec."""
    to_unblock = list(unblock)

    def unblock_in_child() -> None:
        signal.pthread_sigmask(signal.SIG_UNBLOCK, to_unblock)

    return subprocess.Popen(
        argv,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=log_fd,
        stderr=log_fd,
        start_new_session=True,
        preexec_fn=unblock_in_child,
    )
