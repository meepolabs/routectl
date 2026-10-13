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
| `run.py` | Runs the matrix, one capture container per cell, under bounds |
| `cell_process.py` | Stops and reaps one cell's process group and removes its container |
| `launch_key.py` | Reads one cell's key at launch from the descriptor `matrix.open_key_file` checked |
| `scratch_size.py` | Measures a cell's scratch tree without trusting it: walked by descriptor, never through a link, each inode once, by allocated blocks |
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

## Running

```
python3 scripts/wire_watch/run.py --report-dir <dir> [--key-dir <dir>] \
    [--image <tag>] [--concurrency 2] [--cell-timeout <s>] [--deadline <s>] \
    [--cell-cap-bytes <n>] [--run-cap-bytes <n>] [--seat <file>] [--only <glob>]...
```

`run.py` takes a lock on `<report-dir>/run.lock` for the whole run (a
second run on the same dir exits 3 and touches nothing), refuses a report
dir that already holds a run (a paid cell is never re-run), expands the
matrix, and writes `run.json` before the first cell starts. The key
directory defaults to `$XDG_CONFIG_HOME/routectl-wire-watch` (`~/.config`
when `XDG_CONFIG_HOME` is unset). `--only` keeps the cells whose id
matches a glob, e.g. `--only 'codex.openai-responses.*'`.

Each cell runs `scripts/container/run_capture.sh` with its own scratch
directory, `<report-dir>/cells/<cell-id>/` (mode 0700; its
`.cell-evidence/` is what the report reads; the runner's own stdout and
stderr go to `<report-dir>/cells/<cell-id>.runner.log`, mode 0600, beside
the scratch rather than in it, and never to the terminal), a container named
`routectl-cell-<run-id>-<n>`, and the driver command
`env ROUTECTL_DRIVER_REQUEST_MODEL=<model> /workspace/scripts/drivers/<driver>`.
A cell with a key file has the key re-checked and read at launch and
exported to that one child as `ROUTECTL_DRIVER_<PROVIDER>_API_KEY`; the
child's environment is otherwise an allowlist (PATH, HOME, XDG and docker
client settings, `ROUTECTL_BIN`), so no other credential reaches it. The key
is never put on a command line or in `run.py`'s own log output.

`cells/<cell-id>.runner.log` is untrusted container output and may contain
the cell's key: it is created 0600, must not be shared, and must not be
printed to a terminal (it can carry terminal escape sequences).

Bounds:

| Bound | Effect |
|---|---|
| `--concurrency` | at most this many cells run at once |
| `--cell-timeout` | SIGTERM to the cell's process group, a grace period, SIGKILL, `docker rm -f <name>`, then a bounded reap -> `TIMEOUT` |
| `--deadline` | no cell starts after it; the rest are `CAP_REACHED(deadline)` |
| `--cell-cap-bytes` | a cell whose scratch tree plus `cells/<cell-id>.runner.log` grows past it, or whose tree has more than 20000 entries, is stopped the same way -> `CAP_REACHED(size)` |
| `--run-cap-bytes` | once every cell's scratch tree plus `runner.log` together reach it (measured afresh before every launch), no cell starts; the rest are `CAP_REACHED(run-size)` |

Sizes are polled every `--size-poll-seconds`, not enforced per write, so a
cell can overshoot either cap by about its write rate times --size-poll-seconds.

The runner's exit code becomes the cell's state: 9 (the daemon outlived
SIGKILL) is always `EXITED(9)`; a stop made by `run.py` is `TIMEOUT`,
`CAP_REACHED(size)` or `INTERRUPTED`; 20 (a credential refused by name) is
`NOT_RUN(credential)`; 129 / 130 / 143 are `INTERRUPTED`; anything else is
`EXITED(rc)` as is. A cell that ended any way but a clean exit (0 or 2-20)
also has its container removed with `docker rm -f`, so a runner killed from
outside leaves no container behind; `rm -f` also removes a container that
was created but never started, which still holds the key in its
environment. However the run ends (normally, on a signal, or on an
exception), one sweep removes any container named `routectl-cell-<run-id>-*`
that is still present (by full id), lists again to confirm, and records
`leftover-containers` as `run_stop`; when docker cannot answer, a removal
fails, or a container is still listed afterwards, it records `sweep-failed`.
The run is then INCOMPLETE. A
signal received at any point, during the sweep included, gives that
signal's exit code. A
leader that cannot be reaped is logged and left; its cell is
`INTERRUPTED` with detail `unreaped`. An `EXITED(5)` cell whose traced ingress tokens lack
its expected ingress marks the unstarted cells of the same (driver, lane,
connection mode) `NOT_RUN(ingress)`. An exited cell's `tokens` in run.json
are its usage snapshot's input, output, reasoning and billed cache-read
counts, or null when its evidence is incomplete. Nothing is retried.

SIGHUP, SIGINT, SIGQUIT, SIGUSR1, SIGUSR2, SIGALRM or SIGTERM stops every
running cell the same way, marks it and every unstarted cell
`INTERRUPTED`, sweeps, leaves `run.json` valid, and exits 128 + the signal
number (e.g. 129 / 130 / 131 / 143).
Otherwise the exit is 0 only when every cell is terminal and none was
stopped by a bound (a cell with detail `run-size` marks a run that
crossed the aggregate cap with nothing left unstarted); 1 when the run finished with a bound-stopped cell; 2
usage; 4 a refused report dir.

`run.json` carries a top-level `run_stop`: `null`, or the first reason the
run as a whole is INCOMPLETE whatever its cells say. Later reasons are
logged, not recorded.

| `run_stop` | Meaning |
|---|---|
| `run-size` | the cells' scratch trees and runner logs together reached `--run-cap-bytes` |
| `leftover-containers` | the end-of-run sweep found (and removed) a container of this run |
| `sweep-failed` | the end-of-run sweep could not ask docker, could not read its answer, failed to remove a container, or still saw one after removing |
