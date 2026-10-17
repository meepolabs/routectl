#!/usr/bin/env python3
"""Extract ONE AWS shared-config profile into a minimal in-cell config.

Stdlib only; run it with `python3 -I`. Called by run_capture.sh on the
host, never inside a cell.

Usage
  aws_profile.py --config FILE --profile NAME --credential-file FILE
                 --cell-profile NAME --cell-credential PATH --out FILE

  --config           the operator's AWS shared-config file (read, never
                     copied)
  --profile          the profile to extract, by exact name
  --credential-file  the file that profile's credential_process reads.
                     It is stat'ed, never opened
  --cell-profile     the profile name the written config uses (the name the
                     lane configs reference)
  --cell-credential  where the credential file is mounted inside the cell
  --out              the config to write; must not exist yet

On success writes `[profile <cell-profile>]` with `credential_process =
/bin/cat <cell-credential>` (plus `region` when the profile sets one) to
--out, prints the credential file's resolved path on stdout, and exits 0.
On any problem exits 1 with one stderr line naming the profile and the
problem.

The operator's config is a format this repo does not own, so the reader is
strict and fails closed: duplicate sections or keys, a second header
that normalizes to the same profile, a bare section of that name, a key
outside the allowlist, or a credential_process of any shape other than
`/bin/cat <absolute path>` are all refused. No message carries a value or
a line of the file: configparser's own exception text quotes source lines,
so errors are mapped by type to fixed text.
"""

from __future__ import annotations

import argparse
import configparser
import os
import re
import stat
import sys
from typing import Dict, List, NoReturn, Optional, Tuple

PROFILE_NAME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}", re.ASCII)
CREDENTIAL_PROCESS_RE = re.compile(r"/bin/cat (/[A-Za-z0-9._/@+-]+)", re.ASCII)
REGION_RE = re.compile(r"[a-z]{2}(?:-[a-z]+)+-[0-9]", re.ASCII)
CELL_PATH_RE = re.compile(r"/[A-Za-z0-9._/-]+", re.ASCII)
# What a `docker run -v src:dst:ro` source can carry unambiguously: no `:`,
# no `,`, no whitespace or control character.
HOST_PATH_RE = re.compile(r"/[A-Za-z0-9._/@+-]+", re.ASCII)

ALLOWED_KEYS = frozenset({"credential_process", "region", "output"})
CONFIG_MAX_BYTES = 1024 * 1024

# configparser treats its default section's keys as inherited by every
# section. AWS has no such inheritance, so the default section is moved to
# a name no header can spell (headers cannot contain `]`); a literal
# `[DEFAULT]` or `[default]` is then an ordinary section nothing inherits.
NO_DEFAULT_SECTION = "]no-default["

PARSE_ERRORS: Tuple[Tuple[type, str], ...] = (
    (configparser.DuplicateSectionError, "the config repeats a section header"),
    (configparser.DuplicateOptionError, "a section of the config repeats a key"),
    (configparser.MissingSectionHeaderError, "the config has a line before its first section header"),
    (configparser.ParsingError, "the config has a line that is not a key = value pair"),
    (configparser.Error, "the config could not be parsed"),
)


class Refused(Exception):
    pass


def refuse(profile: str, problem: str) -> NoReturn:
    raise Refused(f"aws profile '{profile}': {problem}")


def read_config_text(path: str, profile: str) -> str:
    try:
        st = os.stat(path)
    except OSError:
        refuse(profile, "the config file cannot be read")
    if not stat.S_ISREG(st.st_mode):
        refuse(profile, "the config file is not a regular file")
    if st.st_size > CONFIG_MAX_BYTES:
        refuse(profile, "the config file is larger than 1 MiB")
    try:
        with open(path, "rb") as handle:
            data = handle.read(CONFIG_MAX_BYTES + 1)
    except OSError:
        refuse(profile, "the config file cannot be read")
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        refuse(profile, "the config file is not UTF-8")


def parse(text: str, profile: str) -> configparser.RawConfigParser:
    parser = configparser.RawConfigParser(
        strict=True,
        interpolation=None,
        default_section=NO_DEFAULT_SECTION,
        delimiters=("=",),
        comment_prefixes=("#", ";"),
        inline_comment_prefixes=None,
    )
    # Keys keep their spelling, so a key the SDK would not recognise is
    # refused by the allowlist rather than folded into one it would.
    parser.optionxform = str  # type: ignore[assignment,method-assign]
    try:
        parser.read_string(text)
    except configparser.Error as exc:
        for kind, problem in PARSE_ERRORS:
            if isinstance(exc, kind):
                refuse(profile, problem)
        refuse(profile, "the config could not be parsed")
    return parser


def find_section(parser: configparser.RawConfigParser, profile: str) -> str:
    """The one header that names `profile`, refusing every ambiguity."""
    exact: List[str] = []
    for section in parser.sections():
        words = section.split()
        if words == ["profile", profile]:
            exact.append(section)
        elif words == [profile]:
            refuse(profile, "the config also has a bare section of that name")
    if not exact:
        refuse(profile, "no such profile in the config")
    if len(exact) > 1 or exact[0] != f"profile {profile}":
        refuse(profile, "the profile header is ambiguous (repeated, or spelled with extra whitespace)")
    return exact[0]


def section_values(parser: configparser.RawConfigParser, section: str, profile: str) -> Dict[str, str]:
    values = dict(parser.items(section))
    unknown = sorted(set(values) - ALLOWED_KEYS)
    if unknown:
        refuse(profile, "the profile sets keys this cell cannot honor: " + ", ".join(unknown))
    if "credential_process" not in values:
        refuse(profile, "the profile has no credential_process")
    return values


def credential_target(values: Dict[str, str], profile: str) -> str:
    match = CREDENTIAL_PROCESS_RE.fullmatch(values["credential_process"])
    if match is None:
        refuse(profile, "credential_process is not of the form '/bin/cat <absolute path>'")
    return match.group(1)


def check_credential_file(target: str, given: str, profile: str) -> str:
    """The resolved path both names reach. Never opens the file."""
    resolved = os.path.realpath(given)
    if os.path.realpath(target) != resolved:
        refuse(profile, "credential_process reads a different file than --credential-file names")
    if not HOST_PATH_RE.fullmatch(resolved):
        refuse(profile, "the credential file's resolved path has characters a bind mount cannot carry")
    try:
        st = os.stat(resolved)
    except OSError:
        refuse(profile, "the credential file cannot be found")
    if not stat.S_ISREG(st.st_mode):
        refuse(profile, "the credential file is not a regular file")
    if st.st_uid != os.getuid():
        refuse(profile, "the credential file is not owned by this user")
    if st.st_mode & 0o077:
        refuse(profile, "the credential file is readable or writable by group or other (chmod 600 it)")
    return resolved


def region_of(values: Dict[str, str], profile: str) -> Optional[str]:
    region = values.get("region")
    if region is None:
        return None
    if not REGION_RE.fullmatch(region):
        refuse(profile, "region is not an AWS region identifier")
    return region


def render(cell_profile: str, cell_credential: str, region: Optional[str]) -> str:
    lines = [f"[profile {cell_profile}]", f"credential_process = /bin/cat {cell_credential}"]
    if region is not None:
        lines.append(f"region = {region}")
    return "\n".join(lines) + "\n"


def write_exclusive(path: str, text: str, profile: str) -> None:
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    except OSError:
        refuse(profile, "the cell config cannot be created")
    with os.fdopen(fd, "w", encoding="ascii") as handle:
        handle.write(text)


def extract(ns: argparse.Namespace) -> str:
    profile = ns.profile
    if not PROFILE_NAME_RE.fullmatch(profile) or profile == "default":
        raise Refused("aws profile name must match [A-Za-z0-9][A-Za-z0-9._-]{0,63} and not be 'default'")
    if not PROFILE_NAME_RE.fullmatch(ns.cell_profile) or not CELL_PATH_RE.fullmatch(ns.cell_credential):
        raise Refused("aws profile: the cell profile name or cell credential path is malformed")
    parser = parse(read_config_text(ns.config, profile), profile)
    values = section_values(parser, find_section(parser, profile), profile)
    target = credential_target(values, profile)
    resolved = check_credential_file(target, ns.credential_file, profile)
    region = region_of(values, profile)
    write_exclusive(ns.out, render(ns.cell_profile, ns.cell_credential, region), profile)
    return resolved


def main(argv: List[str]) -> int:
    p = argparse.ArgumentParser(prog="aws_profile.py", description="Extract one AWS profile for a capture cell.")
    for flag in ("--config", "--profile", "--credential-file", "--cell-profile", "--cell-credential", "--out"):
        p.add_argument(flag, required=True)
    ns = p.parse_args(argv)
    try:
        resolved = extract(ns)
    except Refused as exc:
        print(str(exc), file=sys.stderr)
        return 1
    sys.stdout.write(resolved + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
