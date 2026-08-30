# Client profiles

A profile is a named, committed set of CLIENT settings a driver applies
before it launches its client: `profiles/<client>.<profile>.env`, loaded by
`driver_load_client_profile` in [../lib/common.sh](../lib/common.sh).

**This directory is deliberately EMPTY of profiles.** The seam and its
rules ship first, populated later by whoever has a cell that needs one.
The rules are here rather than in that future change because the ordering
constraint below is a rule about a mechanism with no users yet, and the
first profile author must not have to rediscover it.

## The closed-set rule

The set of profiles is CLOSED and committed. A driver names a profile that
exists in this directory or the run fails; there is no path argument, no
environment seam naming an out-of-tree file, and no default. A profile is
part of what a fixture was captured under, so a run must not be able to
apply one nobody can read afterwards.

A profile earns a file only if it changes the request body SHAPE rather
than its content -- the same rule `StructuralSummary` already defines:
thinking tier, tool permission, an MCP server list, the model. Settings
that are set once and never varied (telemetry and updater flags, pty
pacing, session-id derivation) belong in the driver, not in a profile:
varying one doubles the corpus and buys no evidence.

## key=value only

A profile is PARSED, not sourced. Every line is `KEY=VALUE`, `#` comments
and blank lines aside; the value is taken literally with no expansion and
no quote stripping. A profile therefore cannot run a command, and the
forbidden classes below are checkable rather than a convention.

Forbidden, each refused by name at load time:

- **Connection carriers** -- `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`,
  the proxy variables, `NODE_EXTRA_CA_CERTS`. The connection MODE owns
  these; see the ordering constraint.
- **Provider credentials** -- any key ENDING in `_API_KEY`, `_TOKEN`,
  `_SECRET`, `_BEARER`, `_PASSWORD`, or `_CREDENTIALS`. The real upstream
  credential reaches routectl through the lane config's `api_key_ref` and
  never through committed content. The match is on the suffix rather than
  anywhere in the name, because `MAX_THINKING_TOKENS` is a body-shape knob
  a profile legitimately carries and a substring test would refuse it.
- **Shell substitution** -- a `$` or a backtick anywhere in a value.

## The ordering constraint (ENFORCED, not advisory)

**A profile is loaded BEFORE `driver_apply_anthropic_connection_mode`. A
load after it is REFUSED.**

That function's first act is to unset BOTH modes' connection carriers,
because the runner forwards the caller's environment and an operator who
routes their own client through routectl already has `ANTHROPIC_BASE_URL`
set. A profile sourced after the apply could re-set that carrier, and the
run would capture the operator's LIVE daemon while landing a fixture
labelled as a hermetic one. This project caught exactly that fault once,
which is why the loader carries a latch and refuses the late call instead
of trusting the order.

The forbidden-carrier check above and the latch are two guards on the same
fault, and both are load-bearing: the check stops a profile that names a
carrier, the latch stops a correctly-written profile applied at the wrong
moment.

## What a profile does NOT get

No `client_config_sha` pin exists yet. It lands in the same change as the
FIRST profile, because an always-empty second pin answers neither of the
two questions two pins exist to separate -- `config_sha` covers the
routectl lane config, and the client-side hash covers the profile. Until a
profile exists there is nothing for it to hash.
