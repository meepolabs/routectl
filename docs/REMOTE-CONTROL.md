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
Anthropic inference with its own credential (`credential_source =
"own"`, the default) while re-routing Claude Code's other
`api.anthropic.com` traffic untouched; [Pure-proxy
mode](#pure-proxy-mode) further sets `credential_source = "forwarded"`
so the client's own claude.ai token authenticates the inference call
too, and routectl holds no Anthropic credential of its own.

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

Pure-proxy mode runs Claude Code through routectl with `[mitm]
credential_source = "forwarded"` and no `[providers]` configured: the
client's own claude.ai session token authenticates the Anthropic
inference call directly against `api.anthropic.com`, and routectl never
holds or resolves an Anthropic credential of its own for that call.
routectl still sits in the loop as a transparent HTTPS proxy -- it still
terminates TLS locally, still keeps its usage accounting, trim, and
translation machinery in the request path, and still logs the request
the same way it logs every other route. Only the egress credential
changes: it lives in Claude Code, not in routectl's config or
credentials store.

Pure-proxy mode is a separate setting from Remote Control's default
`credential_source = "own"`, but it rides the same `[mitm]` listener,
CA, and re-injection plumbing described above.

### How to enable

```toml
[mitm]
credential_source = "forwarded"
```

With no `[providers]` block anywhere in config, the zero-config
bootstrap (see [`docs/CONFIGURATION.md`](CONFIGURATION.md)
"`credential_source`") auto-injects a synthetic Anthropic egress and a
`default` catch-all alias at startup, so there is nothing else to
configure. `[mitm]` is read once at daemon startup like every other
field on this block -- restart `routectl serve` after adding or editing
`credential_source`. Launch Claude Code through the proxy the same way
as Remote Control:

```bash
HTTPS_PROXY=http://127.0.0.1:8443 NODE_EXTRA_CA_CERTS=/home/you/.config/routectl/mitm-certs/ca.pem claude
```

### Transparent identity

On the forwarded leg the Anthropic-API egress presents Claude Code's
OWN identity, not routectl's minted fingerprint. Claude Code's captured
`x-claude-code-*` headers (the whole set, forwarded transparently) and
`x-stainless-*` SDK-fingerprint headers, plus its own `anthropic-beta`
set, all reach Anthropic and override routectl's default cloak headers
and minted session id. This is deliberate: on this leg routectl is a
transparent forwarder and the client genuinely is Claude Code, so
Anthropic sees the real Claude Code request shape end to end.

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

Every forwarded (Anthropic-dialect) request is admitted or rejected at
ingress, before dispatch:

| Condition | Result |
|---|---|
| MITM-marked request with no inbound `Authorization` bearer | HTTP 401 -- sign Claude Code into claude.ai (`claude /login`) |
| Forwarded request that did not arrive via the MITM leg (no `x-routectl-mitm-proxied` seam header) | HTTP 400 |
| Missing `x-claude-code-session-id` identity header | HTTP 400 |
| Non-Anthropic dialect (`/v1/chat/completions`, `/v1/responses`) under forwarded mode | HTTP 400 |

Once a request is admitted, an upstream 401/403/429 on the forwarded
credential is surfaced to the client verbatim: routectl does not
refresh it (it never held the credential to refresh) and does not fall
back to a sibling provider (a request-scoped forwarded token has no
sibling seat to fall back to). Claude Code owns its own retry/backoff
for these statuses.

### Known limitations

1. **Zero-config model fidelity.** The zero-config bootstrap's
   synthetic egress carries exactly one upstream model, and the router
   rewrites every dispatched request's model to that one target. In
   zero-config pure-proxy, every request reaches Anthropic as that
   single model regardless of which model Claude Code actually
   requested. Operators who need more than one model reachable through
   pure-proxy must configure `[providers]` / `[models]` / `[aliases]`
   explicitly rather than relying on the zero-config bootstrap. This is
   a known limitation of the current release.
2. **Coexistence with operator-configured providers.**
   `credential_source = "forwarded"` together with one or more
   `[providers.X]` entries is not a supported combination in this
   release: a forwarded request that would resolve to any target other
   than an `api.anthropic.com` `anthropic-api` egress is refused
   outright rather than silently routed elsewhere. Pure-proxy mode is
   intended to run with no `[providers]` configured, relying on the
   zero-config bootstrap above.

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
