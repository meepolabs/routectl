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
