"""Open a directory the caller names without following a symlink at it.

Stdlib only. O_NOFOLLOW applies to the last path component only, and how a
path is spelled decides what the last component is: `link/`, `link//` and
`link/.` all resolve `link` as an intermediate component, so the link is
followed. `open_dir` therefore splits the path itself: the parent is
trusted and opened normally (a symlink there is the caller's choice), and
the final name is opened relative to the parent's descriptor with
O_NOFOLLOW. Trailing slashes are stripped only to find that final name.
`open_dir` refuses a final name of `.` or `..` (BAD_PATH) rather than
resolving it, since it names some other directory than the one spelled
and O_NOFOLLOW would not apply to the directory actually opened. A caller
that wants to accept such paths must normalize them first, as
matrix.open_key_file does with os.path.abspath; the normalized path's
parent is then trusted like any other.

Public API
  DirRefused        OSError-free refusal; .detail is one of BAD_PATH,
                    MISSING, SYMLINK, NOT_A_DIRECTORY, UNREADABLE
  open_dir(path)    an O_RDONLY directory descriptor the caller must close,
                    or DirRefused
"""

from __future__ import annotations

import errno
import os
import stat

BAD_PATH = "bad-path"
MISSING = "missing"
SYMLINK = "symlink"
NOT_A_DIRECTORY = "not-a-directory"
UNREADABLE = "unreadable"

_CLOEXEC = getattr(os, "O_CLOEXEC", 0)
# O_PATH where available: the parent is only ever a dir_fd, so it needs
# search permission, not read permission, as a path lookup through it would.
_PARENT_FLAGS = getattr(os, "O_PATH", os.O_RDONLY) | os.O_DIRECTORY | _CLOEXEC
_FINAL_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | _CLOEXEC


class DirRefused(Exception):
    def __init__(self, detail: str) -> None:
        super().__init__(detail)
        self.detail = detail


def _split(path: str) -> tuple[str, str]:
    stripped = path.rstrip("/")
    name = os.path.basename(stripped)
    if name in ("", ".", ".."):
        raise DirRefused(BAD_PATH)
    return os.path.dirname(stripped) or ".", name


def _final_refusal(parent_fd: int, name: str, exc: OSError) -> str:
    if isinstance(exc, FileNotFoundError):
        return MISSING
    if exc.errno == errno.ELOOP:
        return SYMLINK
    # Linux reports O_DIRECTORY|O_NOFOLLOW on a symlink as ENOTDIR, as for a
    # plain file. The lstat only names a refusal the open already made.
    if exc.errno == errno.ENOTDIR:
        try:
            info = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        except OSError:
            return NOT_A_DIRECTORY
        return SYMLINK if stat.S_ISLNK(info.st_mode) else NOT_A_DIRECTORY
    return UNREADABLE


def open_dir(path: str) -> int:
    parent, name = _split(path)
    try:
        parent_fd = os.open(parent, _PARENT_FLAGS)
    except FileNotFoundError:
        raise DirRefused(MISSING) from None
    except OSError as exc:
        raise DirRefused(NOT_A_DIRECTORY if exc.errno == errno.ENOTDIR else UNREADABLE) from None
    try:
        return os.open(name, _FINAL_FLAGS, dir_fd=parent_fd)
    except OSError as exc:
        raise DirRefused(_final_refusal(parent_fd, name, exc)) from None
    finally:
        os.close(parent_fd)
