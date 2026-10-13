"""Upstream refusals that fix a cell's outcome: refusal_rules.tsv.

Stdlib only. A cell whose upstream refused it (a credential the upstream
rejected, a model the account cannot use) has no wire result to report,
so its outcome comes from the first matching row of refusal_rules.tsv
(table order) before any wire row is read. The table's columns and match
kinds are documented in the file itself.

A Bedrock short-term API key is `bedrock-api-key-` followed by the base64
of a SigV4 presigned URL, whose query carries the signing time
(`X-Amz-Date`, `YYYYMMDDTHHMMSSZ`) and the validity in seconds
(`X-Amz-Expires`); see aws-bedrock-token-generator-python's
token_generator.py. `key_expiry` reads the key file through
matrix.open_key_file (same descriptor checks as the runner), keeps only
those two values, and returns the expiry time; the key text never leaves
the function. Any other key shape (a long-term key, a malformed token)
has no encoded expiry and returns None.

Public API
  RefusalError                     the rules file is malformed
  Rule                             one table row
  load_rules(path)                 the rows, in table order
  first_match(rules, bedrock, errors, key_expired)
                                   (rule, error row) for the first rule
                                   that matches any error row, else None
  decode_expiry(token)             expiry datetime (UTC) encoded in a
                                   short-term key, else None
  key_expiry(key_dir, name)        decode_expiry of a key file's content,
                                   else None
"""

from __future__ import annotations

import base64
import binascii
import os
import re
import sys
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Callable, List, Optional, Sequence, Tuple
from urllib.parse import parse_qs

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import matrix  # noqa: E402
from evidence import UpstreamError  # noqa: E402
from ledger import NotRunReason, Outcome  # noqa: E402

COLUMNS = ["egress", "status", "match", "outcome", "reason", "source"]
EGRESS = ("bedrock", "other")
ANY_STATUS = "*"
MATCH_STATUS = "-"
MATCH_KEY_EXPIRED = "key-expired"
PHRASE_PREFIX = "phrase:"
REASON_RE = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
STATUS_RE = re.compile(r"^[1-5][0-9]{2}$")

SHORT_TERM_PREFIX = "bedrock-api-key-"
# A short-term key is a presigned URL of a few hundred bytes plus a
# session token; anything far longer is not one.
KEY_MAX_BYTES = 16 * 1024
AMZ_DATE_RE = re.compile(r"^[0-9]{8}T[0-9]{6}Z$")
AMZ_EXPIRES_RE = re.compile(r"^[0-9]{1,7}$")
# SigV4 presigned URLs are valid for at most seven days.
AMZ_EXPIRES_MAX = 7 * 24 * 3600


class RefusalError(Exception):
    """refusal_rules.tsv does not parse."""


@dataclass(frozen=True)
class Rule:
    line: int
    bedrock: bool
    status: Optional[int]
    match: str
    phrase: Optional[str]
    outcome: Outcome
    reason: str
    source: str

    def matches(self, bedrock: bool, error: UpstreamError, key_expired: Callable[[], bool]) -> bool:
        if self.bedrock != bedrock:
            return False
        if self.status is not None and self.status != error.status:
            return False
        if self.match == MATCH_KEY_EXPIRED:
            return key_expired()
        if self.phrase is not None:
            return self.phrase in error.excerpt
        return True


def _parse_match(raw: str, where: str) -> Tuple[str, Optional[str]]:
    if raw in (MATCH_STATUS, MATCH_KEY_EXPIRED):
        return raw, None
    if raw.startswith(PHRASE_PREFIX) and len(raw) > len(PHRASE_PREFIX):
        return PHRASE_PREFIX, raw[len(PHRASE_PREFIX):]
    raise RefusalError(f"{where}: match must be '-', 'key-expired' or 'phrase:<text>', got {raw!r}")


def _parse_outcome(outcome: str, reason: str, where: str) -> Outcome:
    if outcome == Outcome.NOT_RUN.value:
        if reason not in {r.value for r in NotRunReason}:
            raise RefusalError(f"{where}: NOT_RUN reason {reason!r} is not a NotRunReason")
        return Outcome.NOT_RUN
    if outcome == Outcome.FAILED.value:
        if not REASON_RE.fullmatch(reason):
            raise RefusalError(f"{where}: FAILED reason {reason!r} must match {REASON_RE.pattern}")
        return Outcome.FAILED
    raise RefusalError(f"{where}: outcome must be NOT_RUN or FAILED, got {outcome!r}")


def _parse_row(number: int, fields: List[str], where: str) -> Rule:
    row = dict(zip(COLUMNS, fields))
    if row["egress"] not in EGRESS:
        raise RefusalError(f"{where}: egress must be one of {list(EGRESS)}")
    if row["status"] != ANY_STATUS and not STATUS_RE.fullmatch(row["status"]):
        raise RefusalError(f"{where}: status must be an HTTP status or '*'")
    match, phrase = _parse_match(row["match"], where)
    if not row["source"]:
        raise RefusalError(f"{where}: every rule cites a source")
    return Rule(
        line=number,
        bedrock=row["egress"] == "bedrock",
        status=None if row["status"] == ANY_STATUS else int(row["status"]),
        match=match,
        phrase=phrase,
        outcome=_parse_outcome(row["outcome"], row["reason"], where),
        reason=row["reason"],
        source=row["source"],
    )


def load_rules(path: str) -> Tuple[Rule, ...]:
    rules: List[Rule] = []
    header_seen = False
    with open(path, encoding="utf-8") as handle:
        for number, raw in enumerate(handle, start=1):
            line = raw.rstrip("\n")
            if line.startswith("#") or not line.strip():
                continue
            fields = line.split("\t")
            where = f"{os.path.basename(path)}:{number}"
            if not header_seen:
                if fields != COLUMNS:
                    raise RefusalError(f"{where}: header must be {COLUMNS}")
                header_seen = True
                continue
            if len(fields) != len(COLUMNS):
                raise RefusalError(f"{where}: expected {len(COLUMNS)} columns, got {len(fields)}")
            rules.append(_parse_row(number, fields, where))
    if not rules:
        raise RefusalError(f"{path}: no rules")
    return tuple(rules)


def first_match(
    rules: Sequence[Rule],
    bedrock: bool,
    errors: Sequence[UpstreamError],
    key_expired: Callable[[], bool],
) -> Optional[Tuple[Rule, UpstreamError]]:
    for rule in rules:
        for error in errors:
            if rule.matches(bedrock, error, key_expired):
                return rule, error
    return None


def _single(query: dict, name: str, pattern: re.Pattern) -> Optional[str]:
    values = query.get(name)
    if not values or len(values) != 1 or not pattern.fullmatch(values[0]):
        return None
    return values[0]


def decode_expiry(token: str) -> Optional[datetime]:
    if not token.startswith(SHORT_TERM_PREFIX):
        return None
    try:
        url = base64.b64decode(token[len(SHORT_TERM_PREFIX):], validate=True).decode("ascii")
    except (binascii.Error, ValueError):
        return None
    _, sep, query_text = url.partition("?")
    if not sep:
        return None
    query = parse_qs(query_text, keep_blank_values=True)
    signed = _single(query, "X-Amz-Date", AMZ_DATE_RE)
    expires = _single(query, "X-Amz-Expires", AMZ_EXPIRES_RE)
    if signed is None or expires is None or int(expires) > AMZ_EXPIRES_MAX:
        return None
    try:
        start = datetime.strptime(signed, "%Y%m%dT%H%M%SZ").replace(tzinfo=timezone.utc)
        return start + timedelta(seconds=int(expires))
    except (OverflowError, ValueError):
        return None


def _read_capped(fd: int) -> bytes:
    chunks = []
    left = KEY_MAX_BYTES + 1
    while left > 0:
        chunk = os.read(fd, left)
        if not chunk:
            break
        chunks.append(chunk)
        left -= len(chunk)
    return b"".join(chunks)


def key_expiry(key_dir: str, name: str) -> Optional[datetime]:
    fd, _ = matrix.open_key_file(key_dir, name)
    if fd is None:
        return None
    try:
        data = _read_capped(fd)
    except OSError:
        return None
    finally:
        os.close(fd)
    if len(data) > KEY_MAX_BYTES:
        return None
    try:
        return decode_expiry(data.decode("ascii").strip())
    except UnicodeDecodeError:
        return None
