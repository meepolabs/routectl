# Security

## Reporting a vulnerability

Report vulnerabilities to **developers@meepolabs.com**. We aim to
acknowledge within 72 hours. Please do not open public issues for
security reports.

## Security posture

routectl is a local-first proxy that handles provider credentials, so
the defaults are conservative:

- **Loopback by default.** The server binds `127.0.0.1` and refuses a
  non-loopback bind unless started with the explicit `--unsafe-public`
  flag. The optional MITM front-proxy refuses non-loopback binds
  unconditionally.
- **Refs-only secrets.** The config file stores secret *references*
  (`env://VAR`, `file:///abs/path`, `oauth://<provider>`), never
  plaintext values. Inline `literal:` refs are rejected at parse and
  resolve. `file://` refs are refused on Unix unless the file is
  owner-only (0600/0400).
- **Credential storage.** OAuth tokens persist to
  `~/.config/routectl/credentials.json` via atomic 0600 writes in a
  0700 directory. There is no OS-keychain integration and no secret
  auto-discovery: routectl reads only refs named in the config.
- **Redaction.** Bearer tokens and API keys are redacted in trace and
  header logs; config error rendering routes through a fail-safe
  redactor so parse errors never echo secret values; the status
  dashboard and `/status` JSON expose no secret values or paths.
- **Read-only observability.** The dashboard (`GET /`) and the
  `/status` panels are structurally read-only: no mutation routes
  exist, and test-enforced scans keep write handles out of that code.
- **Supply chain.** CI runs `cargo-deny` (advisories + licenses) and
  gitleaks secret scanning; release checksums are signed with
  sigstore cosign (keyless) and verifiable with `cosign verify-blob`.

## Scope notes

routectl is a single-operator local tool, not a multi-tenant gateway:
there is no user management, SSO, or audit-log compliance surface,
and it is not designed to be exposed to untrusted networks. See the
README's responsible-use section for the operating envelope.
