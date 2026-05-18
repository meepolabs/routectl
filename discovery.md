# routectl Bedrock allowlist discovery log

Empirical record of what Claude Code's TS SDK sends through routectl
to Bedrock, and what Bedrock accepts. Cron loop watches the trace log
every 1 min; appends below when a new beta or top-level body field
appears. Each entry includes a direct-to-Bedrock probe so the
operator allowlist in `config.toml` can be updated by hand.

Source of truth for new flags / fields is the binary inspection +
trace observations -- AWS docs lag reality.

---

## Current allowlists (config.toml `[bedrock]`)

### `allowed_betas` (5 entries, last verified 2026-05-13)
- `context-1m-2025-08-07`
- `claude-code-20250219`
- `interleaved-thinking-2025-05-14`
- `context-management-2025-06-27`
- `effort-2025-11-24`

### `allowed_body_fields` (16 entries)
- `anthropic_version`, `anthropic_beta`, `max_tokens`, `messages`,
  `system`, `temperature`, `top_p`, `top_k`, `stop_sequences`,
  `tools`, `tool_choice`, `thinking`, `output_config`,
  `cache_control`, `metadata`, `context_management`

---

## Discovery log (newest first)

### 2026-05-17 — 4-direction trace observations (in progress)

**03:08 update.** Added nested-path fingerprinting to discover.py
(curated key paths walked via JSON when body fits, regex fallback
when truncated -- real Claude Code requests are 200KB+ so the regex
path carries most signal).

**Newly visible nested fields**

Request side (ingress + outgoing, identical sets so routectl is not
dropping any of these on the egress translation):
- `output_config.effort` = `max`, `xhigh`
- `output_config.format` (JSON-schema structured outputs surface --
  Bedrock REJECTS this nested key with 400, see INV-11)
- `thinking.type=adaptive`
- `context_management.edits[].type=clear_thinking_20251015`
- `messages[].role` = `user`, `assistant` (both turns observed)
- `messages[].content[].type=text`
- `system[].type=text`
- `content[].type` = `text`, `tool_use`, `thinking`
- `tools[].name` = `Read`, `Glob`, `Grep` (more will accumulate)
- `tools[].description`, `tools[].input_schema` (structural)
- `tools[].type` = `integer`, `object`, `string` (JSON Schema types
  inside tool input_schema)

Response side (upstream vs egress diff):
- `usage.input_tokens`, `usage.output_tokens` -- passthrough
- `usage.cache_creation_input_tokens`, `usage.cache_read_input_tokens`
  -- passthrough
- `usage.cache_creation` -- passthrough
- `usage.service_tier` -- DROPPED on egress (filed as INV-10)
- `context_management.applied_edits` -- DROPPED on egress (INV-9)
- `content[].type=text` -- passthrough



First day with the new 4-direction trace harness (ingress / outgoing /
upstream success / egress) running under `ROUTECTL_LOG_REDACT_PROMPTS=1`.
Findings accumulate here as the day progresses.

**Ingress top-level body fields seen** (Claude Code -> routectl):
- `model`, `messages`, `max_tokens`, `metadata`, `context_management`,
  `system`, `tools`, `output_config`, `stream`, `temperature`,
  `thinking`

**Outgoing top-level body fields seen** (routectl -> Bedrock):
- `anthropic_version`, `anthropic_beta`, `messages`, `max_tokens`,
  `metadata`, `context_management`, `system`, `tools`, `output_config`,
  `temperature`, `thinking`

**Upstream success body keys** (Bedrock -> routectl):
- `id`, `type`, `role`, `model`, `content`, `stop_reason`,
  `stop_sequence`, `stop_details`, `usage`, `context_management`

**Egress body keys** (routectl -> Claude Code):
- `id`, `type`, `role`, `model`, `content`, `stop_reason`,
  `stop_sequence`, `usage`

**Outgoing betas** (subset of allowlist, 5/5):
- `context-1m-2025-08-07`, `claude-code-20250219`,
  `interleaved-thinking-2025-05-14`,
  `context-management-2025-06-27`, `effort-2025-11-24`

**Routectl drop fingerprints**

| Phase | Dropped | Verdict |
|---|---|---|
| ingress -> outgoing | `model` | EXPECTED. Bedrock InvokeModel uses URL path for model selection; routectl correctly strips it on egress. |
| ingress -> outgoing | `stream` | EXPECTED. Bedrock InvokeModelWithResponseStream is a separate operation, not a body field; routectl translates streaming via the URL/method, not by forwarding `stream:true`. |
| upstream -> egress | `stop_details` | EXPECTED. Bedrock-only Anthropic-API extension (sibling of `stop_reason`); not in the Anthropic Messages baseline. |
| upstream -> egress | `context_management` | LIKELY BUG. Filed as INV-9 in issues.md. Field is in the Anthropic spec when the beta is enabled; Claude Code uses it for compaction UX. |

Action: filed INV-9 (egress drop). No allowlist changes needed; current
config still matches Bedrock policy.

---

### 2026-05-13 — initial baseline

Captured from a single Claude Code Opus 4.7 streaming request after
restart with `--trace`.

**Outgoing betas** (10 total, unfiltered pass-through):
- ✓ `context-1m-2025-08-07` — Bedrock OK
- ✓ `claude-code-20250219` — Bedrock OK
- ✗ `oauth-2025-04-20` — Bedrock REJ (Anthropic-direct OAuth flow)
- ✓ `interleaved-thinking-2025-05-14` — Bedrock OK
- ✗ `redact-thinking-2026-02-12` — Bedrock REJ
- ✓ `context-management-2025-06-27` — Bedrock OK
- ✗ `prompt-caching-scope-2026-01-05` — Bedrock REJ
- ✗ `advisor-tool-2026-03-01` — Bedrock REJ
- ✓ `effort-2025-11-24` — Bedrock OK
- ✗ `extended-cache-ttl-2025-04-11` — Bedrock REJ

**Outgoing top-level body fields** observed in trace (deduped across 3
real requests, redacted prompts):
- `anthropic_beta`, `anthropic_version`, `max_tokens`, `messages`,
  `context_management`

Action: configured `allowed_betas` + `allowed_body_fields` per above.
