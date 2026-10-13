"""Render untrusted text inert inside a Markdown document.

Stdlib only. Everything a capture container writes (upstream error
excerpts, wire paths, ingress tokens, lane labels, artifact details)
reaches report.md through `esc`, the one escaper. Text this code spells
goes through `lit`, numbers through `num`, and pieces are combined with
`cat`. Each returns `Md`; `lines_text`, the only way report.py turns
lines into file text, refuses anything that is not `Md` or that holds a
character outside printable ASCII. Plain `+` or an f-string over `Md`
yields a plain str, which `lines_text` refuses, so text that skipped the
escaper cannot reach the file.

`esc(text, cap)`:
  - A plain token is kept as is: ASCII letters and digits in runs joined
    by single `_`, `-`, `/` or `->` (an optional leading `/`). No
    CommonMark or GFM construct can open from such a token at any
    position, line start included: `_` between two alphanumerics is never
    emphasis, and the token holds no space, `|`, backtick, bracket, `<`,
    `*`, `#`, `&`, `:`, `@` or `.`. With no `.` it holds no domain, so
    neither GFM's extended autolinks nor a linkify pass can link it.
  - Any other text is escaped character by character: ASCII letters,
    digits and space are kept; `.`, `:`, `@`, `#`, `$` and the backslash
    become `\\xNN`, so no domain, scheme, mail address, issue reference or
    math span is left for an autolink or math pass to find (GitHub's file
    view links `www\\.x` and `a\\@b\\.c` despite the backslashes); every other
    printable ASCII character gets a backslash (CommonMark renders a
    backslash-escaped ASCII punctuation character as itself and never as
    structure); every other character becomes its UTF-8 bytes as `\\xNN`
    (lowercase hex).
    A newline is `\\x0a`, so an escaped value never spans lines; ESC is
    `\\x1b`, so no ECMA-48 sequence (CSI, OSC, C1) reaches a terminal;
    the backslash is `\\x5c`, so every backslash in the rendered text
    begins a `\\xNN` escape and input that already reads `\\x1b`
    renders as `\\x5cx1b`, distinct from a real ESC.
  - The result is capped at `cap` characters; past it the rest is
    dropped and TRUNCATED (" (truncated)") appended.

Public API
  Md                    str subclass for text safe in report.md
  esc(text, cap)        Md for untrusted text
  lit(text)             Md for a constant this code spells; refuses
                        anything outside printable ASCII
  num(value)            Md for an int
  cat(*parts)           Md concatenation of Md parts
  join(parts, sep)      Md join of Md parts with an Md separator
  lines_text(lines)     file text for a sequence of Md lines, else
                        ValueError
  DEFAULT_CAP, TRUNCATED, JOIN_SEPARATOR
"""

from __future__ import annotations

import re
from typing import Iterable, Sequence

DEFAULT_CAP = 200
# Separated from the cut text by a space and free of . : @ # $ and the
# backslash, so it cannot extend a word the cap ended on into a link.
TRUNCATED = " (truncated)"
_PRINTABLE = frozenset(chr(c) for c in range(0x20, 0x7F))
_KEPT = frozenset("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ")
# GitHub's file-view renderer autolinks www., scheme: and user@host text
# even when the punctuation is backslash-escaped, and typesets $...$ as
# math; these leave the text as hex instead. The backslash is hexed too, so
# every backslash a reader sees begins a \xNN escape.
_HEXED = frozenset(".:@#$\\")
_PLAIN_TOKEN = re.compile(r"/?[A-Za-z0-9]+(?:(?:[_/-]|->)[A-Za-z0-9]+)*", re.ASCII)


class Md(str):
    """Text safe to place anywhere in report.md."""


JOIN_SEPARATOR = Md(", ")


def _escape_char(ch: str) -> str:
    if ch in _KEPT:
        return ch
    if ch in _PRINTABLE and ch not in _HEXED:
        return "\\" + ch
    return "".join("\\x%02x" % b for b in ch.encode("utf-8", "surrogatepass"))


def esc(text: str, cap: int = DEFAULT_CAP) -> Md:
    # Defensive boundary check: callers pass parsed JSON, and a non-str
    # sequence of one-character strs would otherwise walk the escape loop
    # and come back as Md.
    if not isinstance(text, str):  # pyright: ignore[reportUnnecessaryIsInstance]
        raise TypeError(f"esc takes str, got {type(text).__name__}")
    if len(text) <= cap and _PLAIN_TOKEN.fullmatch(text):
        return Md(text)
    out = []
    used = 0
    for ch in text:
        piece = _escape_char(ch)
        if used + len(piece) > cap:
            out.append(TRUNCATED)
            break
        out.append(piece)
        used += len(piece)
    return Md("".join(out))


def lit(text: str) -> Md:
    if not isinstance(text, str) or not all(ch in _PRINTABLE for ch in text):
        raise ValueError("a report literal must be printable ASCII on one line")
    return Md(text)


def num(value: int) -> Md:
    if not isinstance(value, int) or isinstance(value, bool):
        raise TypeError(f"num takes int, got {type(value).__name__}")
    return Md(str(value))


def cat(*parts: Md) -> Md:
    if not all(isinstance(p, Md) for p in parts):
        raise TypeError("cat takes Md parts")
    return Md("".join(parts))


def join(parts: Iterable[Md], sep: Md = JOIN_SEPARATOR) -> Md:
    items = list(parts)
    if not isinstance(sep, Md) or not all(isinstance(p, Md) for p in items):
        raise TypeError("join takes Md parts and an Md separator")
    return Md(sep.join(items))


def lines_text(lines: Sequence[Md]) -> str:
    for i, line in enumerate(lines):
        if not isinstance(line, Md):
            raise ValueError(f"report line {i} did not come from the escaper")
        if not all(ch in _PRINTABLE for ch in line):
            raise ValueError(f"report line {i} holds a character outside printable ASCII")
    return "".join(line + "\n" for line in lines)
