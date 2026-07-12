# Claude Code Remote Control (RC) via the MITM front-proxy

Claude Code's Remote Control feature only works when Claude Code talks
directly to `api.anthropic.com`. Pointing Claude Code at routectl as an
LLM gateway (per [`docs/CONFIGURATION.md`](CONFIGURATION.md) "claude-code
as a gateway client") normally breaks that, because Claude Code's own
HTTP traffic never reaches Anthropic's servers. The `[mitm]` front-proxy
closes that gap: it sits between Claude Code and `api.anthropic.com`,
reroutes only the inference calls to routectl, and forwards every other
`api.anthropic.com` request (including whatever Remote Control needs)
straight through to Anthropic, untouched. The result is routectl-routed
inference with Remote Control still working.

Read [Responsible use and fragility](#responsible-use-and-fragility)
below before enabling this.

This doc covers two distinct features built on the same `[mitm]`
plumbing: Remote Control (below) keeps routectl authenticating
Anthropic inference with its own credential -- the default for any
`[providers.X]` `anthropic-api` entry (`credential_source = "own"`) --
while re-routing Claude Code's other `api.anthropic.com` traffic
untouched; [Pure-proxy mode](#pure-proxy-mode) instead configures a
provider with `credential_source = "forwarded"` so the client's own
claude.ai token authenticates the inference call too, and that
provider holds no Anthropic credential of its own. `credential_source`
is a per-`[providers.X]` choice, not a `[mitm]`-level one -- `[mitm]`
itself carries no credential knob (see
[`docs/CONFIGURATION.md`](CONFIGURATION.md) "credential_source").

## Prerequisites

- Claude Code must be logged in to claude.ai (`claude /login`), so it
  holds a full-scope claude.ai session token. That token is what makes
  Remote Control (and everything else this proxy passes through) work.
- Remote Control additionally depends on a server-side flag
  (`tengu_ccr_bridge`) that Anthropic controls and gates per account.
  routectl has no visibility into or control over this flag. If Remote
  Control does not come up for your account, this is not a routectl bug
  -- check with Anthropic or try a different account.

## Enable

Add a `[mitm]` block to your `config.toml`. Its presence enables the
feature; an absent block keeps zero MITM proxy startup and zero
behavior change (this is the same convention as `[server.auth]`).

Minimal example -- an empty block enables the proxy with every default:

```toml
[mitm]
```

Or spell out the fields (all optional; shown with their real defaults):

```toml
[mitm]
upstream_origin = "https://api.anthropic.com"
listen_port = 8443
cert_dir = "~/.config/routectl/mitm-certs"   # actual default resolves
                                              # via XDG_CONFIG_HOME/HOME,
                                              # never a literal "~"
mitm_host = "api.anthropic.com"
tested_cc_version = "2.1.143"                # optional; see below
```

`listen_port` must differ from `[server] port` -- the MITM listener and
routectl's own HTTP server are two separate bound sockets on the same
host. `[mitm]` is read once at daemon startup; adding, removing, or
editing this block while `routectl serve` is already running does not
take effect until you restart the process. routectl's live config
reload (file watch / SIGHUP) neither respawns the MITM proxy nor
reports the change as restart-required -- it is silently ignored, not
flagged. Restart `routectl serve` after any `[mitm]` edit.

## Launch Claude Code through the proxy

Print the two environment variables the proxy needs:

```bash
routectl rc env
```

This prints:

```
HTTPS_PROXY=http://127.0.0.1:8443
NODE_EXTRA_CA_CERTS=/home/you/.config/routectl/mitm-certs/ca.pem
```

(the port and path reflect your `[mitm]` config). If `[mitm]` is not
configured, `rc env` prints nothing to stdout and exits non-zero with
an explanatory message on stderr.

Use the two variables as an **inline prefix** on the `claude` command,
not `export`:

```bash
HTTPS_PROXY=http://127.0.0.1:8443 NODE_EXTRA_CA_CERTS=/home/you/.config/routectl/mitm-certs/ca.pem claude
```

This is deliberate, not a style preference: an inline prefix scopes
both variables to that one Claude Code process. `export`-ing them into
your shell would route every other program you launch from that shell
through the local MITM proxy and trust its CA, which is a much bigger
blast radius than intended.

## Limitation: Remote Control requires listener auth OFF

This feature is unsupported when routectl's own listener auth is
enabled (`[server.auth].tokens` set). The proxy re-injects the
Anthropic inference request into routectl's own listener carrying the
claude.ai session token, verbatim, as the `Authorization` header --
that is the client's own token, not one of your configured listener
tokens, so routectl's listener-auth middleware would reject it.

This applies identically to [Pure-proxy mode](#pure-proxy-mode): the
re-injection is the same regardless of `credential_source`, so a
populated `[server.auth].tokens` breaks pure-proxy mode the same way it
breaks Remote Control.

Remote Control assumes listener auth is off, which is routectl's
loopback default (an omitted or empty `[server.auth]` accepts
unauthenticated requests). Do not combine `[mitm]` with a populated
`[server.auth].tokens` list.

## `tested_cc_version`: runtime warn-and-proceed check

`tested_cc_version` records the Claude Code version you last verified
this setup against. Unlike a purely advisory field, routectl DOES
consult it at runtime: on each decrypted request, the proxy extracts
the observed Claude Code version from the `claude-cli/<version>`
`User-Agent` header and compares it against `tested_cc_version`. On a
mismatch it logs a WARNING once per distinct observed version (a
version change re-warns) -- it never refuses or alters the request.
Leaving `tested_cc_version` unset (the default) disables the check
entirely: no extraction, no comparison, no warning.

This is a signal, not a gate: Remote Control rides on Claude Code's and
Anthropic's own `HTTPS_PROXY` / `NODE_EXTRA_CA_CERTS` handling and the
server-side `tengu_ccr_bridge` flag, none of which routectl controls.
Any Claude Code update can change that behavior and break this feature
outright; the warning only tells you the CLI version moved, not
whether that move broke anything -- see the fragility caveat below.

## Rotating or regenerating the CA

```bash
routectl rc regen-ca
```

This re-mints the local CA and leaf certificate and prints the new CA
path. It is safe to re-run at any time. After regenerating, **restart
Claude Code** -- it reads `NODE_EXTRA_CA_CERTS` once at process start,
so a running Claude Code process keeps trusting the old CA until it is
relaunched with the new `NODE_EXTRA_CA_CERTS` value from `routectl rc
env`.

## Disable

Remove the `[mitm]` block from `config.toml` and restart `routectl
serve`. With the block absent, routectl starts no MITM listener, binds
no extra port, and behaves exactly as it did before this feature
existed.

## Pure-proxy mode

### What it is

Pure-proxy mode runs Claude Code through routectl with a
`credential_source = "forwarded"` `anthropic-api` provider configured:
the client's own claude.ai session token authenticates the Anthropic
inference call directly against `api.anthropic.com`, and that provider
never holds or resolves an Anthropic credential of its own for that
call. routectl still sits in the loop as a transparent HTTPS proxy --
it still terminates TLS locally, still keeps its usage accounting,
trim, and translation machinery in the request path, and still logs
the request the same way it logs every other route. Only the egress
credential changes: it lives in Claude Code, not in routectl's config
or credentials store.

Pure-proxy mode is a separate per-provider setting from Remote
Control's default `credential_source = "own"`, but it rides the same
`[mitm]` listener, CA, and re-injection plumbing described above.
`credential_source = "forwarded"` is a per-target choice: it coexists
freely with other `[providers.X]` entries, and a fallback chain may
mix forwarded and own-credential targets -- only the targets actually
marked `credential_source = "forwarded"` route on the client's bearer.

### How to enable

Add an `anthropic-api` provider with `credential_source = "forwarded"`:

```toml
[providers.anthropic-forwarded]
kind              = "anthropic-api"
base_url          = "https://api.anthropic.com"
credential_source = "forwarded"
```

Omit `api_key_ref` -- a forwarded provider has no configured credential
of its own. Validation rejects a non-empty `api_key_ref` on a
forwarded entry, and rejects any `base_url` whose host is not exactly
`api.anthropic.com` (see [`docs/CONFIGURATION.md`](CONFIGURATION.md)
"credential_source"). Route an alias or model to this provider the
same way you would any other. Unlike `[mitm]` itself, this is an
ordinary provider edit -- it hot-reloads on the next config swap, no
restart required. Launch
Claude Code through the proxy the same way as Remote Control:

```bash
HTTPS_PROXY=http://127.0.0.1:8443 NODE_EXTRA_CA_CERTS=/home/you/.config/routectl/mitm-certs/ca.pem claude
```

### Transparent identity

On a forwarded target the Anthropic-API egress presents Claude Code's
OWN identity, not routectl's minted fingerprint, and keeps the model
Claude Code actually requested verbatim rather than rewriting it to
the target's configured `upstream`. Claude Code's captured
`x-claude-code-*` headers (the whole set, forwarded transparently) and
`x-stainless-*` SDK-fingerprint headers, plus its own `anthropic-beta`
set, all reach Anthropic and override routectl's default cloak headers
and minted session id. This is deliberate: on this leg routectl is a
transparent forwarder and the client genuinely is Claude Code, so
Anthropic sees the real Claude Code request shape end to end.

`GET /v1/models` also proxies through to Anthropic's real model list
on this leg (arrived via the MITM reinject leg, carrying a captured
bearer, resolving to a forwarded provider pinned to
`api.anthropic.com`) instead of returning routectl's local alias list;
any other case, including a proxy-side failure, falls back to the
local list.

### Single-tenant boundary

routectl is a transparent forwarder on the forwarded leg, not a session
manager: it does not validate, track, or map the forwarded token to a
user identity. Each request carries and authenticates its own token
directly against Anthropic; routectl defers entirely to Anthropic's own
auth decision and keeps no per-user state. Sharing one routectl instance
across multiple users in pure-proxy mode is out of scope -- there is no
per-user accounting, isolation, or rate-limiting of forwarded
credentials, only the single per-request token that arrives on the
wire.

### Admission and failure behavior

Any request that arrives via the MITM reinject leg (identified by a
seam header matching this process's nonce, unspoofable by a direct
caller) is admitted or rejected at ingress, before dispatch, regardless
of which provider it ultimately resolves to:

| Condition | Result |
|---|---|
| No inbound `Authorization` bearer | HTTP 401 -- sign Claude Code into claude.ai (`claude /login`) |
| Bearer present, no `x-claude-code-session-id` identity header | HTTP 400 |

A request that did NOT arrive via that leg -- no matching seam header,
including every non-Anthropic-dialect request in practice -- is
admitted untouched by this gate regardless of dialect or provider
config.

Past admission, a further per-target guard runs inside the dispatch
chain walk: a `credential_source = "forwarded"` target with no
captured client bearer (most commonly because this particular request
never arrived via the MITM leg at all) is refused before egress with a
local HTTP 400, per-target -- a chain that never reaches that target is
unaffected.

Once a forwarded target is actually dispatched, an upstream 401/403/429
on that request is surfaced to the client verbatim: routectl does not
refresh the credential (it never held it to refresh) and does not fall
back to a sibling provider (a request-scoped forwarded token has no
sibling seat to fall back to). Claude Code owns its own retry/backoff
for these statuses.

## Responsible use and fragility

This feature MITMs Anthropic's own domain (`api.anthropic.com`),
terminates TLS locally with a certificate authority routectl generates
on your machine, and forwards your full-scope claude.ai session token
through a local proxy process. Before enabling it:

- The proxy is loopback-only. routectl hard-refuses to start the MITM
  listener on any non-loopback bind, and that refusal cannot be
  overridden -- not even with `--unsafe-public`.
- Your claude.ai token never egresses to anything other than
  Anthropic's own origin (`api.anthropic.com`) or routectl's own
  loopback listener, and it is never logged.
- This depends entirely on behavior Anthropic and Claude Code control,
  not routectl: whether they continue honoring `HTTPS_PROXY` and
  `NODE_EXTRA_CA_CERTS`, and whether the `tengu_ccr_bridge` flag stays
  on for your account. Any of that can change on any Claude Code or
  Anthropic-side update, with no advance notice, and break Remote
  Control -- or this whole feature -- without warning.
- Read [Responsible use](../README.md#responsible-use) in the main
  README before pointing this at anything beyond your own personal
  Claude Code session.

Use this deliberately, and expect to revisit it when Claude Code
updates.
