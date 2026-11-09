# The harness drivers

One file per harness. A driver maps ONE canonical interaction case (see
[cases/README.md](cases/README.md)) onto one client's argv and
environment, against the routectl that `scripts/capture_driver.sh` booted.

## Why one file per harness and never a dispatch

There is deliberately no `case "$harness"` statement anywhere here. Five
near-duplicate blocks in one script is a drift surface, and more
importantly "not yet drivable" has to degrade to NO FILE rather than to a
dead branch: a branch that cannot run still reads as coverage, and a
harness whose name cannot be committed could not be driven at all if the
enumeration lived in tracked content. Shared behavior lives in
[lib/common.sh](lib/common.sh) instead, which is what keeps the
per-harness files small enough for the rule to stay affordable.

## The files

| File | Client | Notes |
|---|---|---|
| `claude-code.sh` | `claude`, interactive | Types the case's turns into a pty; supports both connection modes |
| `claude-code-print.sh` | `claude -p` | Same binary, different wire shape: non-interactive turn structure and tool loop. Multi-turn resumes by session id |
| `external-agent-cli.sh` | named by `ROUTECTL_DRIVER_AGENT_BIN` | Any third-party Anthropic-dialect CLI with a one-shot flag; the driver never names its client |
| `codex.sh` | `codex exec` | Expected ingress `openai-responses`; points codex at the daemon through its own `-c model_providers.*` overrides |
| `hermes-anthropic.sh` | `hermes` from PATH | Expected ingress `anthropic`. Names the flags (`-z`, `-c`, `-m`, `--provider anthropic`) and execs `external-agent-cli.sh`, which owns the `ANTHROPIC_BASE_URL` carrier |
| `hermes-responses.sh` | `hermes` from PATH | Expected ingress `openai-responses`. Owns its carrier: a custom provider with `api_mode: codex_responses` in a config written under the run's HOME |
| `hermes-chat.sh` | `hermes` from PATH | Expected ingress `openai`. Same carrier as the responses driver, with `api_mode: chat_completions` |
| `pi-anthropic.sh` | `pi` from PATH | Expected ingress `anthropic`. Writes `providers.anthropic.baseUrl` into `models.json` (pi ignores `ANTHROPIC_BASE_URL`), names the flags (`-p`, `-c`, `--model`, `--thinking`, `--provider anthropic`), and execs `external-agent-cli.sh` with `lib/pi-continue-guard.sh` as its client |
| `pi-responses.sh` | `pi` from PATH | Expected ingress `openai-responses`. Owns its carrier: a `models.json` custom provider with `api: openai-responses` |
| `pi-chat.sh` | `pi` from PATH | Expected ingress `openai`. Same carrier as the responses driver, with `api: openai-completions` |

Every driver records its own basename (without `.sh`) as the client name
in the run record, so no two files share one. The runner forwards that
name as the driver token of the landing key
`<lane>/<case>__<driver>__<lane config stem>` (recorded as
`meta.client.driver`) and records the sha256 of `scripts/drivers/<name>.sh`
as `meta.client_config_sha` (the script only, not `lib/`; empty, with a
warning, when no committed script has that name). `hermes-anthropic.sh` and `pi-anthropic.sh` delegate to
`external-agent-cli.sh` and name themselves through
`ROUTECTL_DRIVER_AGENT_NAME`; run directly, that driver records
`external-agent-cli`. Two checks govern the name. Every recorded name is a
landing-key token, so the rig refuses one that does not match
`^[a-z0-9]+([.-][a-z0-9]+)*$` with exit 2 before it reads the trace; the
driver has already run by then, so that refusal costs the run. On top of
that, `external-agent-cli.sh` checks `ROUTECTL_DRIVER_AGENT_NAME` against
the stricter hyphen-only form `^[a-z0-9]+(-[a-z0-9]+)*$` (no `.`) and
refuses with exit 2 before the client runs.

The three hermes files share [lib/hermes.sh](lib/hermes.sh): it points
`HERMES_HOME` inside the run workspace, clears every inherited variable
hermes would route or authenticate by (an operator's `OPENAI_BASE_URL`
would otherwise win over the written config), and writes the config that
carries the case knobs hermes takes only from config. The config names the
local key's env var, never its value. All three are base-url only, and a
multi-turn case through the responses/chat drivers refuses turn 2 unless
turn 1 recorded a session for `-c` to resume.

The three pi files share [lib/pi.sh](lib/pi.sh). pi has no base-url
environment carrier on ANY dialect, so every pi driver writes
`$PI_CODING_AGENT_DIR/models.json` inside the run workspace (an inherited
`PI_CODING_AGENT_DIR` would otherwise hand pi the operator's profile).
Its `apiKey` is the variable NAME `$PI_ROUTECTL_LOCAL_API_KEY`, which pi
resolves at request time, never a value. `settings.json` beside it carries
the tools, thinking, and cache-warming knobs. pi's `-c` silently starts
fresh when there is nothing to continue, so all three refuse turn 2 unless
turn 1 left a session file (`pi-anthropic.sh` through
[lib/pi-continue-guard.sh](lib/pi-continue-guard.sh)), and all three are
base-url only.

A harness with no file here is not drivable on this box. Its missing
piece is filed to the `wild-evidence` pen rather than stubbed.

## A turn loop never lends the client its stdin

`claude-code-print.sh`, `external-agent-cli.sh`, `codex.sh` and the hermes
and pi turn loops in `lib/hermes.sh` and `lib/pi.sh` all feed the case's
turns through a `while read` loop, so the loop's stdin is a pipe holding
the REMAINING prompts. Each redirects the client's stdin from `/dev/null`:
a client that reads stdin otherwise drains that pipe, and a 2-turn case
runs ONE turn with every prompt concatenated into a single user text block
-- a fixture claiming a multi-turn shape it never carried.
`claude-code.sh` is structurally exempt (it pipes a generated keystroke
script into a pty, with no client sitting in the loop).

The self-test's stub client DRAINS stdin for exactly this reason. A stub
that ignored it cannot exhibit the bug, so the "once per turn" assertion
passed against a stub incapable of failing it -- and it does pass, measured,
with a driver's redirect removed.

## The dialect a driver's client speaks is the CALLER's pin

A case is deliberately dialect-agnostic and a lane config declares only the
EGRESS provider, so neither names which dialect the driven client reaches
routectl on. The (driver, lane) PAIRING does, and the caller chooses that
pairing -- so the dialect arrives as the runner's required
`--expected-ingress` and is compared against the traced
`meta.ingress_kind` before any fixture lands. Nothing in this directory
reads the pin; a driver's job is to point its client at the runner's daemon,
not to certify what it did once it got there.

The failure it catches is specific to this directory's shape: an
Anthropic-dialect driver handed a client that speaks another dialect ACCEPTS
the connection carriers and ignores them, so the run looks clean at every
env-check and lands a fixture that is evidence for the wrong dialect. That
is also why a client whose dialect differs earns its own driver file rather
than a flag on an existing one.

## Client profiles

`profiles/` is the seam for named, committed CLIENT settings; the
directory does not exist until a cell needs a profile. The closed-set
rule, the forbidden keys, and the ordering constraint the loader enforces
are in [../../docs/DEVELOPMENT.md](../../docs/DEVELOPMENT.md) "Client
profiles" -- read it before writing the first profile.

## The lane config and its variants

A driver never names a lane: the runner resolves
`config/<lane>.toml` and copies it into the run's config root. A second
DEPLOYMENT SHAPE of the same provider is a `<lane>.<variant>` filename
(`anthropic-api.front-proxy.toml` beside `anthropic-api.toml`), selected by
`--lane` and no other flag. Its stem is the third token of the landing
directory's name (`<lane>/<case>__<driver>__<config stem>`) and is recorded
as `meta.config_variant`; `config_sha` identifies its exact bytes. A
wire-affecting per-deployment value lives INSIDE the file, where
`config_sha` covers it.

Every lane config sets `[wire_check] sample_one_in = 1`, so the daemon a
driven run boots checks every attempt rather than a production sample.
On the Bedrock lanes (`bedrock-invoke`, `bedrock-converse`,
`bedrock-mantle`) the model is a matrix dimension: each candidate is a
`[models.<nick>]` entry, listed with its tier and source doc in
[../wire_watch/models.tsv](../wire_watch/models.tsv).
[config/lane_configs.test.sh](config/lane_configs.test.sh) welds those
together and pins each Bedrock lane to its region. The Bedrock lanes sign
with the AWS profile `routectl-cell`, which the capture-cell wrapper
generates from the operator's own profile (see
`scripts/container/run_capture.sh`, THE AWS PROFILE).

## Connection mode is a capture axis

A MITM front proxy carries `role:"system"` turns inside `messages[]`;
`base-url` mode inlines the same content as system-reminder text and sends
zero system turns. Same client, same case, two wire shapes -- which is why
`connection_mode` is a required fixture pin and why a driver refuses a
front-proxy run whose proxy URL or CA is unset instead of falling back to
base-url. A silent fallback would land a fixture labelled `front-proxy`
whose shape is `base-url`, and every later cross-mode diff would read as
client drift.

## Running one

See the driver-mode section of
[../../docs/DEVELOPMENT.md](../../docs/DEVELOPMENT.md).
`scripts/drivers.test.sh` covers the case set and every driver against a
STUB daemon and a STUB client -- a real run needs a credential.
