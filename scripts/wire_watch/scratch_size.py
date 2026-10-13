"""Bytes a cell's scratch tree holds, measured without trusting the tree.

Stdlib only. The tree is written from inside the capture container.

Public API
  SIZE_WALK_MAX_ENTRIES   entry bound past which a tree counts as over cap
  SIZE_WALK_MAX_DEPTH     depth bound, likewise
  tree_size(root, cap[, max_entries])
                          bytes under root, or cap + 1
  charge(stat_result)     what one inode costs: max(size, allocated blocks)
  runner_log_bytes(cells_dir, cell_id)
                          what the cell's runner log costs (0 if absent)
  RUNNER_LOG_SUFFIX       the runner log is cells/<id> + this
"""

from __future__ import annotations

import os
import stat
import sys
from dataclasses import dataclass, field
from typing import Optional, Set, Tuple

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import nofollow  # noqa: E402

# The scratch tree is written by the container, so its size walk never
# follows a link and gives up past a fixed entry count (an unbounded tree
# of empty files is as much a runaway as a large one); an unwalkable tree
# counts as over any cap. A capture cell legitimately writes a fixture, its
# evidence and a few traces: a few thousand entries at most. 20000 leaves
# ample headroom and keeps one live walk to about 20 ms (measured on ext4:
# 20000 entries ~20 ms, 200000 entries ~280 ms), so a size poll cannot
# starve the loop that enforces timeouts and signals.
SIZE_WALK_MAX_ENTRIES = 20_000
# Each level of the walk holds two descriptors open (the directory and its
# listing), so depth is bounded like the entry count.
SIZE_WALK_MAX_DEPTH = 128
_CHILD_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | getattr(os, "O_CLOEXEC", 0)
RUNNER_LOG_SUFFIX = ".runner.log"
# st_blocks is in 512-byte units on every platform this runs on.
BLOCK_BYTES = 512


def charge(info: os.stat_result) -> int:
    """What one inode costs: its apparent size, or the blocks it holds when
    those are more (an extent reserved with FALLOC_FL_KEEP_SIZE has no
    apparent size)."""
    return max(info.st_size, info.st_blocks * BLOCK_BYTES)


class _Over(Exception):
    """The walk is over cap, over a bound, or cannot go on safely."""


@dataclass
class _Tally:
    cap: int
    max_entries: int
    total: int = 0
    entries: int = 0
    seen: Set[Tuple[int, int]] = field(default_factory=set)

    def add(self, info: os.stat_result) -> None:
        self.entries += 1
        if self.entries > self.max_entries:
            raise _Over()
        key = (info.st_dev, info.st_ino)
        if key not in self.seen:
            self.seen.add(key)
            self.total += charge(info)
        if self.total > self.cap:
            raise _Over()


def _walk(dir_fd: int, depth: int, tally: _Tally) -> None:
    """Each subdirectory is opened by name relative to its parent's
    descriptor with O_NOFOLLOW: a directory the container swaps for a link
    between the listing and the descent is refused, never followed."""
    if depth > SIZE_WALK_MAX_DEPTH:
        raise _Over()
    with os.scandir(dir_fd) as listing:
        for item in listing:
            info = item.stat(follow_symlinks=False)
            tally.add(info)
            if not stat.S_ISDIR(info.st_mode):
                continue
            child = os.open(item.name, _CHILD_FLAGS, dir_fd=dir_fd)
            try:
                _walk(child, depth + 1, tally)
            finally:
                os.close(child)


def tree_size(root: str, cap: int, max_entries: Optional[int] = None) -> int:
    """Bytes under root without following a link, each inode counted once;
    cap + 1 once the walk is over cap, too large or too deep to walk, or
    cannot be walked. Only `root` itself is trusted."""
    try:
        root_fd = nofollow.open_dir(root)
    except nofollow.DirRefused:
        return cap + 1
    tally = _Tally(cap, SIZE_WALK_MAX_ENTRIES if max_entries is None else max_entries)
    try:
        # A directory's own size grows with its entries and never shrinks.
        tally.add(os.fstat(root_fd))
        _walk(root_fd, 0, tally)
    except (_Over, OSError):
        return cap + 1
    finally:
        os.close(root_fd)
    return tally.total


def runner_log_bytes(cells_dir: str, cell_id: str) -> int:
    """The log is run.py's own file, out of the container's reach, so a
    plain lstat of it is trusted."""
    try:
        info = os.lstat(os.path.join(cells_dir, cell_id + RUNNER_LOG_SUFFIX))
    except FileNotFoundError:
        return 0
    return charge(info)
