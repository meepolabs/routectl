"""Structural checks over the committed driver lane configs.

Stdlib only, and no `tomllib`: CI's Python predates it. The reader below
accepts only the TOML subset the lane configs use -- table headers, bare
or double-quoted keys, and single-line values -- and rejects any other
line, so a construct it does not understand fails the check instead of
being silently skipped.

Usage:
  lane_configs.py wire-check CONFIG...
  lane_configs.py weld MODELS_TSV CONFIG_DIR
  lane_configs.py regions CONFIG_DIR

Each subcommand prints one `FAIL:` line per violation and exits 1 if
there was any, 0 otherwise; 2 is a usage or parse error.
"""

import os
import re
import sys
from typing import Dict, List, Optional, Set, Tuple

Tables = Dict[str, Dict[str, str]]
Row = Tuple[int, Dict[str, str]]

TABLE_RE = re.compile(r"^\[([A-Za-z0-9_.-]+)\]$")
KEY_RE = re.compile(r'^("(?:[^"\\]|\\.)*"|[A-Za-z0-9_-]+)\s*=\s*(.+)$')
STRING_RE = re.compile(r'^"((?:[^"\\]|\\.)*)"\s*(?:#.*)?$')

# Region scope per Bedrock lane: each lane is exercised in one region,
# and a config naming any other region would move the capture somewhere
# the wire watch never measured.
LANE_REGIONS = {
    "bedrock-invoke": "us-west-2",
    "bedrock-converse": "us-west-2",
    "bedrock-mantle": "us-east-1",
}

TSV_COLUMNS = ["lane", "nickname", "upstream", "tier", "thinking"]
TIERS = {"primary", "sweep"}
THINKING = {"yes", "no"}


class ParseError(Exception):
    pass


def read_config(path: str) -> Tables:
    """Return {table_name: {key: raw_value}} for the supported subset."""
    tables: Tables = {"": {}}
    current = ""
    with open(path, encoding="utf-8") as handle:
        for number, raw in enumerate(handle, start=1):
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            header = TABLE_RE.match(line)
            if header:
                current = header.group(1)
                if current in tables:
                    raise ParseError(f"{path}:{number}: duplicate table [{current}]")
                tables[current] = {}
                continue
            pair = KEY_RE.match(line)
            if not pair:
                raise ParseError(f"{path}:{number}: unsupported line: {line}")
            key = pair.group(1)
            if key.startswith('"'):
                key = key[1:-1]
            if key in tables[current]:
                raise ParseError(f"{path}:{number}: duplicate key {key!r}")
            tables[current][key] = pair.group(2).strip()
    return tables


def string_value(raw: str) -> Optional[str]:
    match = STRING_RE.match(raw)
    return match.group(1) if match else None


def model_tables(tables: Tables) -> Tables:
    prefix = "models."
    return {
        name[len(prefix):]: body
        for name, body in tables.items()
        if name.startswith(prefix) and "." not in name[len(prefix):]
    }


def read_models_tsv(path: str) -> Tuple[List[Row], Set[str]]:
    """Return (rows, cited_upstreams). Comment lines carry the citations."""
    rows: List[Row] = []
    cited: Set[str] = set()
    header_seen = False
    with open(path, encoding="utf-8") as handle:
        for number, raw in enumerate(handle, start=1):
            line = raw.rstrip("\n")
            if line.startswith("#"):
                parts = line.split()
                if len(parts) >= 4 and parts[1] == "doc" and parts[2].startswith("https://"):
                    cited.update(parts[3:])
                continue
            if not line.strip():
                continue
            fields = line.split("\t")
            if not header_seen:
                if fields != TSV_COLUMNS:
                    raise ParseError(f"{path}:{number}: header must be {TSV_COLUMNS}, got {fields}")
                header_seen = True
                continue
            if len(fields) != len(TSV_COLUMNS):
                raise ParseError(f"{path}:{number}: expected {len(TSV_COLUMNS)} columns, got {len(fields)}")
            rows.append((number, dict(zip(TSV_COLUMNS, fields))))
    if not header_seen:
        raise ParseError(f"{path}: no header row")
    return rows, cited


def check_wire_check(paths: List[str]) -> List[str]:
    failures: List[str] = []
    for path in paths:
        section = read_config(path).get("wire_check")
        if section is None:
            failures.append(f"{path}: no [wire_check] table")
        elif section.get("sample_one_in") != "1":
            failures.append(
                f"{path}: [wire_check] sample_one_in is {section.get('sample_one_in')!r}, want 1"
            )
    return failures


def glob_shadows(pattern: str, nickname: str) -> bool:
    return pattern.endswith("*") and nickname.startswith(pattern[:-1])


def check_lane_weld(lane: str, config_path: str, lane_rows: List[Row], cited: Set[str]) -> List[str]:
    failures: List[str] = []
    tables = read_config(config_path)
    models = model_tables(tables)
    tsv_by_nick: Dict[str, Row] = {}
    for number, row in lane_rows:
        nick = row["nickname"]
        if nick in tsv_by_nick:
            failures.append(f"models.tsv:{number}: duplicate nickname {nick} on lane {lane}")
        tsv_by_nick[nick] = (number, row)

    for nick in sorted(set(models) - set(tsv_by_nick)):
        failures.append(f"{config_path}: [models.{nick}] has no models.tsv row on lane {lane}")
    for nick in sorted(set(tsv_by_nick) - set(models)):
        number = tsv_by_nick[nick][0]
        failures.append(f"models.tsv:{number}: {nick} has no [models.{nick}] in {config_path}")

    for nick in sorted(set(models) & set(tsv_by_nick)):
        number, row = tsv_by_nick[nick]
        configured = string_value(models[nick].get("upstream", ""))
        if configured != row["upstream"]:
            failures.append(
                f"models.tsv:{number}: {nick} upstream {row['upstream']!r} != config {configured!r}"
            )

    for nick, (number, row) in sorted(tsv_by_nick.items()):
        if row["tier"] not in TIERS:
            failures.append(f"models.tsv:{number}: tier {row['tier']!r} not in {sorted(TIERS)}")
        if row["thinking"] not in THINKING:
            failures.append(f"models.tsv:{number}: thinking {row['thinking']!r} not in {sorted(THINKING)}")
        if row["upstream"] not in cited:
            failures.append(f"models.tsv:{number}: upstream {row['upstream']} is not cited on a '# doc' line")

    primaries = sorted(n for n, (_, r) in tsv_by_nick.items() if r["tier"] == "primary")
    if len(primaries) != 1:
        failures.append(f"lane {lane}: want exactly one primary, got {primaries}")

    aliases = tables.get("aliases", {})
    default = string_value(aliases.get("default", ""))
    if len(primaries) == 1 and default != primaries[0]:
        failures.append(f"{config_path}: [aliases] default is {default!r}, want primary {primaries[0]!r}")

    for pattern in sorted(aliases):
        for nick in sorted(models):
            if glob_shadows(pattern, nick):
                failures.append(f"{config_path}: alias glob {pattern!r} shadows nickname {nick!r}")
    return failures


def check_weld(tsv_path: str, config_dir: str) -> List[str]:
    rows, cited = read_models_tsv(tsv_path)
    failures: List[str] = []
    by_lane: Dict[str, List[Row]] = {}
    for number, row in rows:
        by_lane.setdefault(row["lane"], []).append((number, row))
    for lane in sorted(set(by_lane) - set(LANE_REGIONS)):
        failures.append(f"models.tsv: lane {lane!r} is not a Bedrock lane ({sorted(LANE_REGIONS)})")
    for lane in sorted(LANE_REGIONS):
        config_path = os.path.join(config_dir, lane + ".toml")
        if not os.path.isfile(config_path):
            failures.append(f"{config_path}: missing")
            continue
        if lane not in by_lane:
            failures.append(f"models.tsv: no rows for lane {lane}")
        failures.extend(check_lane_weld(lane, config_path, by_lane.get(lane, []), cited))
    return failures


def check_regions(config_dir: str) -> List[str]:
    failures: List[str] = []
    for lane, want in sorted(LANE_REGIONS.items()):
        path = os.path.join(config_dir, lane + ".toml")
        if not os.path.isfile(path):
            failures.append(f"{path}: missing")
            continue
        regions = [
            (table, string_value(body["region"]))
            for table, body in read_config(path).items()
            if "region" in body
        ]
        if not regions:
            failures.append(f"{path}: names no region")
        for table, region in regions:
            if region != want:
                failures.append(f"{path}: [{table}] region {region!r}, lane allows only {want!r}")
    return failures


def main(argv: List[str]) -> int:
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    command, args = argv[1], argv[2:]
    try:
        if command == "wire-check" and args:
            failures = check_wire_check(args)
        elif command == "weld" and len(args) == 2:
            failures = check_weld(args[0], args[1])
        elif command == "regions" and len(args) == 1:
            failures = check_regions(args[0])
        else:
            print(__doc__, file=sys.stderr)
            return 2
    except (OSError, ParseError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2
    for failure in failures:
        print(f"FAIL: {failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
