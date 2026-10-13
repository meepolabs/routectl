"""Strict JSON decoding for every file the wire-watch scripts read back.

Stdlib only. `json.loads` accepts things no writer here produces and a
planted file can use to disagree with itself: a repeated key (the last one
silently wins, so two readers can see different values), the non-standard
constants NaN, Infinity and -Infinity, a float literal that overflows to
inf (1e999), and an integer of any length (bounded only by the
interpreter's int_max_str_digits, which a caller's environment can lift).
`loads` refuses all of them, and refuses nesting deeper than MAX_DEPTH
before decoding: the decoder's own bound is the interpreter's recursion
limit, which lets tens of thousands of levels through to a value that
later code cannot even repr.

Public API
  StrictJsonError   ValueError subclass; .detail is a short fixed token
                    ("duplicate key", "non-finite number",
                    "number too long", "too deep", "not json")
  loads(text)       the decoded value, or StrictJsonError
"""

from __future__ import annotations

import json
import math
import re
from typing import List, Tuple

DUPLICATE_KEY = "duplicate key"
NON_FINITE = "non-finite number"
NUMBER_TOO_LONG = "number too long"
# Integer literals the wire-watch files carry are exit codes, counts and
# timestamps; 20 characters covers every signed 64-bit value.
INT_MAX_CHARS = 20
# The deepest file the wire-watch scripts read back nests 5 levels (a
# health snapshot's data.wire_check.<rows>[i].<field>); 64 leaves room for
# the daemon's panels to grow without admitting pathological input.
MAX_DEPTH = 64

# Tokens the depth scan steps through: an escape pair, a quote, or a
# bracket. Each match is at most two characters; matching a whole string
# literal as one token would make the regex engine keep state per character
# (hundreds of MiB on a multi-MiB string).
_SCAN = re.compile(r'\\.|["\[\]{}]', re.S)
TOO_DEEP = "too deep"
NOT_JSON = "not json"


class StrictJsonError(ValueError):
    def __init__(self, detail: str) -> None:
        super().__init__(detail)
        self.detail = detail


def _object(pairs: List[Tuple[str, object]]) -> dict:
    out: dict = {}
    for key, value in pairs:
        if key in out:
            raise StrictJsonError(DUPLICATE_KEY)
        out[key] = value
    return out


def _constant(_name: str) -> object:
    raise StrictJsonError(NON_FINITE)


def _float(token: str) -> float:
    value = float(token)
    if not math.isfinite(value):
        raise StrictJsonError(NON_FINITE)
    return value


def _int(token: str) -> int:
    if len(token) > INT_MAX_CHARS:
        raise StrictJsonError(NUMBER_TOO_LONG)
    return int(token)


def _check_depth(text: str) -> None:
    depth = 0
    in_string = False
    for match in _SCAN.finditer(text):
        token = match.group()
        if in_string:
            # An escape pair is consumed whole, so an escaped quote never
            # ends the string. Outside a string a backslash is not JSON;
            # the decoder refuses it after this scan.
            if token == '"':
                in_string = False
        elif token == '"':
            in_string = True
        elif token in "[{":
            depth += 1
            if depth > MAX_DEPTH:
                raise StrictJsonError(TOO_DEEP)
        elif token in "]}":
            depth -= 1


def loads(text: str) -> object:
    _check_depth(text)
    try:
        return json.loads(
            text, object_pairs_hook=_object, parse_constant=_constant, parse_float=_float, parse_int=_int
        )
    except StrictJsonError:
        raise
    except RecursionError:
        raise StrictJsonError(TOO_DEEP) from None
    except ValueError:
        raise StrictJsonError(NOT_JSON) from None
