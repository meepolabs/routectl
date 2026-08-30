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

A harness with no file here is not drivable on this box. Its missing
piece is filed to the `wild-evidence` pen rather than stubbed.

## A turn loop never lends the client its stdin

`claude-code-print.sh` and `external-agent-cli.sh` both feed the case's
turns through a `while read` loop, so the loop's stdin is a pipe holding
the REMAINING prompts. Both redirect the client's stdin from `/dev/null`:
a client that reads stdin otherwise drains that pipe, and a 2-turn case
runs ONE turn with every prompt concatenated into a single user text block
-- a fixture claiming a multi-turn shape it never carried.
`claude-code.sh` is structurally exempt (it pipes a generated keystroke
script into a pty, with no client sitting in the loop).

The self-test's stub client DRAINS stdin for exactly this reason. A stub
that ignored it cannot exhibit the bug, so the "once per turn" assertion
passed against a stub incapable of failing it -- and it does pass, measured,
with either driver's redirect removed.

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

[profiles/](profiles/README.md) is the seam for named, committed CLIENT
settings, and is deliberately empty of profiles until a cell needs one.
Its README carries the closed-set rule, the forbidden keys, and the
ordering constraint the loader enforces -- read it before writing the
first profile.

## The lane config and its variants

A driver never names a lane: the runner resolves
`config/<lane>.toml` and copies it into the run's config root. A second
DEPLOYMENT SHAPE of the same provider is a `<lane>.<variant>` filename
(`anthropic-api.front-proxy.toml` beside `anthropic-api.toml`) and nothing
more -- no path segment, no `meta.json` field, no flag. A wire-affecting
per-deployment value lives INSIDE the file, where `config_sha` covers it.

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
