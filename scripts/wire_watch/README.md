# Wire watch

The weekly wire-watch run drives every installed harness on every lane it
speaks, through the hermetic driver runner (`scripts/capture_driver.sh`),
and reports what each cell found. Everything here is stdlib Python; none of
it needs a routectl binary, a network, or a credential to test.

| File | Role |
|---|---|
| `cells.tsv` | The committed denominator: one row per (driver, lane, connection mode) the run owes a result for |
| `models.tsv` | Bedrock candidate models per lane, with tier and thinking flag (welded to the lane configs by `scripts/drivers/config/lane_configs.test.sh`) |
| `matrix.py` | Validates `cells.tsv`, expands it into cells, preflights them |
| `ledger.py` | Cell vocabulary, state machine, atomic `run.json` |
| `evidence.py` | Reads a cell's `.cell-evidence/` into a typed result or INCOMPLETE |
| `wire_watch.test.sh` | Self-test (`python3 -m unittest` over `tests/`), run in CI |

## The denominator

A cell is one (driver, lane, connection mode, model, case). `matrix.py`
expands every `cells.tsv` row across every case in `scripts/drivers/cases/`
before anything runs, so a cell absent from a report is a row absent from
`cells.tsv`, never a silent omission. Each row is checked against closed
vocabularies and refused, naming its line, when the expected ingress is not
in `scripts/drivers/lib/ingress_kinds.sh`, the lane config does not exist,
the connection mode disagrees with the config's `[mitm]` block, or the
providers column disagrees with the config's credential refs.

A lane listed in `models.tsv` is a Bedrock lane. Every row on it runs the
lane's primary model on every case. The candidate sweep -- plain turn, one
tool case, and a thinking case where the model supports it -- runs on ONE
row per lane provider table (the `sweep` column), so each candidate is
covered once instead of once per harness. Every other lane runs its
`[aliases] default` on every case.

```
python3 scripts/wire_watch/matrix.py validate
python3 scripts/wire_watch/matrix.py expand --key-dir <dir>   # run.json to stdout
```

## Key files

Each `cells.tsv` row names its own key file (or `-` for an OAuth seat) in a
key directory given as an argument; no path to it is committed. Preflight
only `stat`s and opens the file -- it never reads the key -- and marks the
row's cells `NOT_RUN(credential)` when the file is missing, unreadable,
empty, not a regular file, or has any group/other permission bit
(`chmod 600` it). A missing or non-executable driver is `NOT_RUN(driver)`.

## Cell states

`run.json` records each cell's run state: `PENDING -> RUNNING ->
EXITED(rc) | TIMEOUT | CAP_REACHED(reason) | INTERRUPTED`, or `NOT_RUN(reason)`
from either non-terminal state; any other move raises. The report's final
vocabulary is `PASS | FINDING | NOT_RUN(reason) | FAILED | TIMEOUT |
CAP_REACHED | INTERRUPTED | INCOMPLETE`; `NOT_RUN` is never `PASS`, and an
exited cell's outcome comes from its evidence. `run.json` is replaced
atomically (temp file, fsync, rename), and its bytes depend only on its
content, so the same inputs give the same file.

## Evidence

`evidence.py` reads the `.cell-evidence/` directory the driver runner
writes (layout: `docs/DEVELOPMENT.md` "Per-cell evidence"), taking the
layout marker and artifact names from the writer itself. A missing
artifact, a truncated or unparseable file, an unknown layout marker, or a
snapshot at an unknown `schema_version` is INCOMPLETE with a distinct
reason -- never an empty, clean-looking result.
