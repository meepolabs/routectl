"""Which of the four wire legs a cell's capture landed.

Stdlib only. The capture rig lands a cell's fixture at
`<cell dir>/<lane>/<case>/`, where `<lane>` is the rig's own lane name for
the traced provider kind (not the cells.tsv lane), so the case directory
is looked up under every lane directory. A leg is captured when its body
file is a non-empty regular file:

  leg 1  ingress_request.json    client -> routectl
  leg 2  outgoing_request.json   routectl -> upstream
  leg 3  upstream_response.json  upstream -> routectl
  leg 4  egress_response.json    routectl -> client

Everything under the cell directory is written from inside the capture
container, so nothing beneath it is followed through a symlink and
anything that is not a plain directory or file counts as not captured.
A rig that refused the capture lands no fixture, so all four legs read
as not captured; so do the stream legs an egress traces no body for.

Public API
  LEG_FILES              the four body file names, in leg order
  legs_captured(cell_dir, case)  tuple of four bools
"""

from __future__ import annotations

import os
import stat
import sys
from typing import List, Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import nofollow  # noqa: E402

LEG_FILES: Tuple[str, ...] = (
    "ingress_request.json",
    "outgoing_request.json",
    "upstream_response.json",
    "egress_response.json",
)
NONE_CAPTURED: Tuple[bool, ...] = (False,) * len(LEG_FILES)
# The rig lands only a handful of lane directories per cell; a container
# that planted thousands is not worth walking.
MAX_LANE_DIRS = 64

_CLOEXEC = getattr(os, "O_CLOEXEC", 0)
_SUBDIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | _CLOEXEC


def _open_subdir(parent_fd: int, name: str) -> Optional[int]:
    try:
        return os.open(name, _SUBDIR_FLAGS, dir_fd=parent_fd)
    except OSError:
        return None


def _leg_present(case_fd: int, name: str) -> bool:
    try:
        info = os.stat(name, dir_fd=case_fd, follow_symlinks=False)
    except OSError:
        return False
    return stat.S_ISREG(info.st_mode) and info.st_size > 0


def _lane_names(cell_fd: int) -> List[str]:
    try:
        names = sorted(n for n in os.listdir(cell_fd) if not n.startswith("."))
    except OSError:
        return []
    return names[:MAX_LANE_DIRS]


def _case_legs(cell_fd: int, lane: str, case: str) -> Tuple[bool, ...]:
    lane_fd = _open_subdir(cell_fd, lane)
    if lane_fd is None:
        return NONE_CAPTURED
    try:
        case_fd = _open_subdir(lane_fd, case)
    finally:
        os.close(lane_fd)
    if case_fd is None:
        return NONE_CAPTURED
    try:
        return tuple(_leg_present(case_fd, name) for name in LEG_FILES)
    finally:
        os.close(case_fd)


def legs_captured(cell_dir: str, case: str) -> Tuple[bool, ...]:
    if case in ("", ".", "..") or "/" in case:
        return NONE_CAPTURED
    try:
        cell_fd = nofollow.open_dir(cell_dir)
    except nofollow.DirRefused:
        return NONE_CAPTURED
    try:
        found = NONE_CAPTURED
        for lane in _lane_names(cell_fd):
            legs = _case_legs(cell_fd, lane, case)
            found = tuple(a or b for a, b in zip(found, legs))
        return found
    finally:
        os.close(cell_fd)
