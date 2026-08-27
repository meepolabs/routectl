# The canonical interaction set

One JSON file per case, `<case_id>.json`, describing ONE interaction to
drive a client through. A case file describes the interaction and nothing
else: it names no binary, no flag, and no harness. Mapping a case onto a
client's argv is the job of a driver under `scripts/drivers/`, which is
why the same case can be replayed through every drivable harness and
produce comparable fixtures.

The cases cover wire PATTERNS, not models. A model name is a routing
detail the lane config already pins (`scripts/drivers/config/<lane>.toml`);
what a corpus needs pinned is the request SHAPES a client emits --
multi-turn tool loops, cache breakpoints, thinking blocks, and a context
large enough to change the client's own framing.

## A driver corpus is a snapshot of a client VERSION

Client wire shape moves under you. Claude Code auto-updated 2.1.169 ->
2.1.245 across a single restart and changed its request shape mid-week; a
later update to 2.1.247 landed before this set was written. That is why
every case carries a stable `case_id`: the landing directory keys on
`(lane, case_id)`, so a rerun of the same case RE-LANDS on the same path
and the version's change shows up as a DIFF instead of as a fresh sibling
nobody compares. `meta.client.version` on each landed fixture is the
decay clock; the case id is what makes it readable.

Editing a case file therefore invalidates the comparability of every
fixture previously captured for that case, exactly as editing a lane
config does. Add a new case id instead of redefining an old one whenever
the old shape is still worth keeping.

## Schema (version 1)

No key outside this list is accepted. Every string value is single-line:
a raw newline or other control character in a prompt is refused, because
prompts cross a shell argv and a trace log where a newline is a record
separator.

| Key | Type | Rule |
|---|---|---|
| `schema_version` | integer | must be `1` |
| `case_id` | string | `^[a-z0-9]+(-[a-z0-9]+)*$`, and equal to the filename stem |
| `title` | string | non-empty, single-line; a human summary of the interaction |
| `wire_pattern` | string | one of `baseline`, `tool-use-multiturn`, `cache-breakpoints`, `thinking`, `large-context` |
| `lane` | string | `^[a-z0-9]+(-[a-z0-9]+)*$`, and `scripts/drivers/config/<lane>.toml` must exist |
| `turns` | array | at least one object, each with exactly one key `prompt`: a non-empty single-line string |
| `knobs` | object | exactly the four keys below |
| `notes` | string | OPTIONAL, single-line |

`knobs` is the neutral vocabulary a driver translates into its client's
own flags. It carries capability INTENT, never a flag spelling:

| Key | Type | Meaning for a driver |
|---|---|---|
| `tools` | boolean | the client must be allowed to run tools, so the trace carries a tool loop |
| `thinking` | boolean | the client must request extended thinking |
| `cache_breakpoints` | boolean | the interaction must repeat a long stable prefix so the client sets cache control |
| `context_padding_bytes` | integer | `>= 0`; synthetic filler the driver materializes in the run's throwaway cwd for the client to read, to reach a large-context shape without shipping a large file in git |

### Case ids are neutral by construction

A `case_id` names the landing directory, and driver mode runs
`scrub-fixture.sh --check` over the whole staged fixture -- `meta.json`
included -- before promoting it. An id derived from a hostname or a real
path is therefore refused by the landing gate itself. Ids are scenario
names (`tools-multiturn-01`), never environment-derived, and the charset
rule above also makes a separator or a `..` segment unrepresentable.

### Everything in a case file is synthetic

No prompt here is a captured prompt, and no path in one is a real path.
The interactions are invented to exercise a wire shape. That is a
committability requirement, not a stylistic one: a case file is tracked
content in a public repo, and a driven client reads its own prompts back
into the request bodies a fixture pins.

## Validating

`scripts/drivers/lib/validate_case.py` is the single enforcement point --
the drivers read their case through it, and `scripts/drivers.test.sh`
checks every committed case against it:

```
python3 scripts/drivers/lib/validate_case.py --check scripts/drivers/cases/thinking-01.json
python3 scripts/drivers/lib/validate_case.py --field wire_pattern <case>
python3 scripts/drivers/lib/validate_case.py --turns <case>
```

## Running a case

A runner covering a whole lane derives the set of cases it expects at run
time -- `cases/*.json` filtered on `lane` -- rather than reading a
committed manifest, so adding or moving a case needs no second file kept
in step with this directory.

See the driver-mode section of [../../../docs/DEVELOPMENT.md](../../../docs/DEVELOPMENT.md).
