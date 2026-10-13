"""Read one cell's key at launch, from the descriptor matrix.py checked.

Stdlib only.

Public API
  KEY_MAX_BYTES, KEY_CHAR_RE   what a key may be: one line of printable
                               ASCII, at most KEY_MAX_BYTES
  read_key(key_dir, name)      (key, None), or (None, NOT_RUN(credential)
                               detail)
"""

from __future__ import annotations

import os
import re
import sys
from typing import Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import matrix  # noqa: E402

# A key is one line of printable ASCII; a longer file is not a key.
KEY_MAX_BYTES = 16 * 1024
KEY_CHAR_RE = re.compile(rb"^[\x21-\x7e]+$")


def read_key(key_dir: str, name: str) -> Tuple[Optional[str], Optional[str]]:
    """(key, None), or (None, NOT_RUN(credential) detail). Opens the key
    through matrix.open_key_file, reads it from that descriptor, closes it."""
    fd, problem = matrix.open_key_file(key_dir, name)
    if fd is None:
        return None, problem
    try:
        data = _read_at_most(fd, KEY_MAX_BYTES + 1)
    except OSError:
        return None, "unreadable"
    finally:
        os.close(fd)
    if data.endswith(b"\r\n"):
        data = data[:-2]
    elif data.endswith(b"\n"):
        data = data[:-1]
    if len(data) > KEY_MAX_BYTES or not KEY_CHAR_RE.fullmatch(data):
        return None, "malformed"
    return data.decode("ascii"), None


def _read_at_most(fd: int, limit: int) -> bytes:
    chunks = []
    left = limit
    while left > 0:
        chunk = os.read(fd, left)
        if not chunk:
            break
        chunks.append(chunk)
        left -= len(chunk)
    return b"".join(chunks)
