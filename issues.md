# routectl bug + fix log

Tracking open routectl issues observed against
`/Volumes/workplace/hollamad_workflows/routectl` (develop, current
tip `db7a260`).

---

## Open

### [INV-13] Bedrock-opus rejects built-in tool `type: "web_search_20250305"`

**First seen:** 2026-05-17 05:48 PDT (request `019e35fa-8b54-7292-ba8a-568c9946b2fc`)

**Symptom.** Claude Code includes a built-in tool entry
`{"name":"web_search","type":"web_search_20250305","max_uses":8}` in
the request `tools[]` array. Bedrock-opus returns 400:

```
{"message":"tool type 'web_search_20250305' is not supported for this model"}
```

**Hypothesis.** The `web-search-2025-03-05` beta IS in our 16-flag
Bedrock-accepted list (per the 2026-05-12 sweep in
`/scripts/routectl/issues.md` INV-6+7), so Bedrock accepts the
beta header... but does NOT accept the corresponding tool `type`
discriminator on opus-4-7. Two possibilities:

1. The beta is account-gated separately for the tool path. Header
   accepted but tool entry refused.
2. The beta only enables web_search on certain models (haiku/sonnet
   maybe?) and opus support hasn't been turned on yet.

**Action.**
- This is a Claude Code feature getting blocked by Bedrock. Not a
  routectl bug -- routectl correctly forwards both the beta header
  and the tool entry; Bedrock is the one rejecting.
- Worth a probe: try the same web_search tool via direct Bedrock
  on haiku-4-5 + sonnet-4-6 to confirm whether opus is the only
  model that rejects it. If yes, routectl could conditionally drop
  the tool when alias=opus to silently degrade rather than 400.
- Either way, filing for tracking.

**Apply at.** Probably nothing routectl-side until we have more
data. If the per-model rejection pattern holds, consider an
operator-facing knob to drop unsupported built-in tools by alias.

---

### [INV-12] Bedrock 5xx `upstream error (empty body)` correlates with `output_config.format` -- same root cause as INV-11

**First seen:** 2026-05-17 10:03:13 PDT (request `019e3563-cdae-7653...`)
**Pattern:** 11 lifetime occurrences against opus-4-7, 8 against
sonnet-4-6, 1 confirmed against haiku-4-5.

**Investigation 1 (2026-05-17 03:33 PDT) -- INVALIDATED.** Claimed
both 5xx requests carried `output_config.format`. Re-checked
2026-05-17 05:30 PDT: that finding was an artifact of a sloppy
`grep -c '"format"'` matching the word "format" inside other JSON
structures (likely tool input_schema), NOT output_config.format
specifically. The original opus 5xx
(`019e3563-cdae-...`) actually has `output_config={"effort":"max"}`
with NO format key. Confidence in Investigation 1's correlation:
zero -- it was a measurement bug, not a finding.

**Investigation 2 (2026-05-17 03:42 PDT) -- HYPOTHESIS REJECTED.**
Cross-checked all 11 requests in the run that shipped
`output_config.format` in the outgoing body, grouped by alias:

| alias                       | total | ok | 4xx | 5xx |
|-----------------------------|------:|---:|----:|----:|
| `claude-haiku-4-5-20251001` |    10 |  9 |   0 |   1 |
| `claude-opus-4-7`           |     1 |  0 |   1 |   0 |
| `claude-sonnet-4-6`         |     0 |  - |   - |   - |

Conclusions:
- Haiku ACCEPTS `output_config.format` in 9/10 cases -- so it is NOT
  deterministically rejected on haiku. The single 5xx is
  more likely AWS endpoint flakiness coinciding with an
  output_config.format request than caused by it.
- Opus rejected its only such request with the clean 400 (INV-11).
  Sample size 1 -- cannot generalize, but consistent with "opus
  doesn't accept format, haiku mostly does".
- Sonnet got zero structured-output requests in this run.

**Revised hypothesis.** Bedrock's `output_config.format` support is
inconsistent across models -- haiku-4-5 mostly accepts it, opus-4-7
rejects it, sonnet unknown. INV-11 (opus 400) and INV-12 (haiku
empty 500) are likely DIFFERENT root causes:
- INV-11: opus genuinely lacks `output_config.format` support.
- INV-12: AWS-side flakiness (returns to original "global profile"
  hypothesis), happening to land on a request that also had
  `output_config.format`. Single coincidence, not causation.

**Action.**
1. INV-11 fix still stands: nested-path egress filter for opus.
2. INV-12 reverts to "monitor". If 5xx rate climbs above a few
   percent, add regional-endpoint fallback. No content-correlation
   action needed at this sample size.
3. **Open question**: collect more samples per model. Need at least
   N=20 per (alias, format-present?) cell before drawing harder
   conclusions. Sonnet entirely missing from the sample is also a
   collection gap -- Claude Code may not route `output_config.format`
   requests to sonnet at all.

**Investigation 3 (2026-05-17 05:30 PDT) -- third opus 5xx,
hypothesis stays "AWS flakiness".**

New occurrence `019e35e9-126f-7333-...` (opus 5xx). Outgoing body
has NO `output_config.format`, just `output_config={"effort":"max"}`
plus standard fields (anthropic_beta, messages, context_management,
thinking, etc.). Definitively breaks any "format triggers 5xx"
hypothesis.

**Investigation 4 (2026-05-17 05:50 PDT) -- fourth opus 5xx, same
shape.** Request `019e35fd-2e03-7a81-...`, opus, plain
`output_config={"effort":"max"}`, regular tools (Bash, etc.), no
format, no web_search.

**Investigation 5 (2026-05-17 05:53 PDT) -- fifth opus 5xx.**
Request `019e35ff-f0f4-7ff2-...`, opus, NO `output_config` at all,
no format, no web_search.

Total: 5 opus + 1 haiku 5xx, with no content correlation.
Pattern remains AWS-side flakiness on the `global.*` profile.

Lessons:
1. Investigation 1's 2/2 correlation was a regex bug, not a real
   finding. Substring grep on `"format"` matches inside other
   nested keys. Use a structural matcher (jq/python) when checking
   nested key presence.
2. Always cross-check correlation findings with a third sample
   before locking in a root cause. With N=2 it's almost always
   coincidence.

---

### [INV-11] Bedrock rejects `output_config.format`: JSON-schema structured outputs not Bedrock-supported

**First seen:** 2026-05-17 10:25:12 PDT (request `019e3577-ed20-78b2...`)
**Recurring:** opus 400s on `output_config.format` are now persistent.
Confirmed occurrences:
- `019e3577-ed20-78b2-...` (10:25 PDT)
- `019e35c0-08b2-7290-...` (04:44 PDT)
- `019e35c8-0b62-7d93-...` (04:52 PDT)
- `019e35c8-a6ed-74f2-...` (04:53 PDT)
- `019e35cc-7ef2-73a2-...` (04:57 PDT)
- `019e35d6-731f-7b33-...` (05:08 PDT)
- `019e35d8-8cbd-75f1-...` (05:11 PDT)
- `019e35da-7bc5-71b2-...` (05:13 PDT)
- `019e35dc-261f-7b71-...` (05:14 PDT)
- `019e35dd-ce27-7b40-...` (05:16 PDT)
- `019e35e3-78ff-7641-...` (05:22 PDT)
- `019e35e5-8d01-72e2-...` (05:25 PDT)

Lifetime opus count: 12/12 -- every opus request that ships
`output_config.format` so far has been rejected. Confirms this is a
deterministic Bedrock-opus rejection (NOT flakiness, NOT model-mix).

**Symptom.** Claude Code sends a request with
`output_config.format = {schema: {...}, type: "json_schema"}` (the
structured-outputs feature gated by the `structured-outputs-2025-12-15`
beta). Bedrock rejects with status 400:

```
{"message":"output_config.format: Extra inputs are not permitted"}
```

The parent `output_config` is in our `allowed_body_fields` allowlist,
so routectl forwards it whole; the rejection is on the nested
`format` sub-key. Same outgoing body sent `output_config.effort=xhigh`
which Bedrock accepts.

**Hypothesis.** Bedrock's Anthropic-compat schema accepts
`output_config.effort` (the `effort-2025-11-24` knob) but does NOT
yet support `output_config.format` (the structured-outputs surface).
Anthropic-direct supports both; Bedrock implements a strict subset.
The `structured-outputs-2025-12-15` beta is in our list of
Bedrock-known-rejected betas (per `discovery.md` 2026-05-13 sweep
showed it is NOT in the 16-beta accepted set), so this 400 is
consistent with that earlier finding -- but Claude Code is sending
the body field even when the beta isn't in the allowlist, because
the field-level filter is independent of the beta filter.

**Action.** None routectl-side -- the egress filter is doing its job
on top-level keys. Two follow-ups:
- For our `allowed_body_fields` to cover this case, we'd need
  nested-path filtering (drop `output_config.format` while keeping
  `output_config.effort`). That's a real upstream feature ask --
  current allowlist is shallow.
- Alternatively, gate `output_config.format` behind the
  `structured-outputs-*` beta in routectl's outgoing rewriter so
  the nested key is dropped when the beta isn't in the egress list.

Recommend filing as upstream feature request: "nested-path
allowlist for [bedrock] allowed_body_fields, mirroring the depth-1
shape." Low-priority; the 400 surfaces clearly to Claude Code
which then retries without the field.

---

### [INV-10] Anthropic egress drops `usage.service_tier` from Bedrock response

**First seen:** 2026-05-17 03:08 PDT (nested-path harness backfill).
**Symptom.** Bedrock returns `usage.service_tier` in the response
(values seen: `standard` and possibly others). Routectl's Anthropic
egress wire-render strips it. Sibling `usage.cache_creation_input_tokens`,
`usage.cache_read_input_tokens` etc. are passed through correctly, so
the egress `Usage` struct has SOME but not ALL upstream fields.

**Hypothesis.** `routectl-providers/src/anthropic_api/types.rs`
`Usage` struct missing the `service_tier: Option<String>` field.
Anthropic-direct API also returns this on the public Messages
endpoint, so it's not Bedrock-specific drift -- it's a coverage gap
in the canonical `Usage` model. Lower-impact than INV-9 (Claude Code
doesn't currently use it for UX), but still a fidelity bug.

**Apply at.** Anthropic `Usage` type. Add the field, mirror in the
`From<bedrock::Usage>` impl if one exists.

---

### [INV-9] Anthropic egress drops `context_management` from Bedrock 200 response

**First seen:** 2026-05-17 ~02:55 PDT (request `019e355d-47fe-78f2...`)
**Recurring:** confirmed 03:01 (request `019e3561-ca8b-7741...`)
**Count this window:** 2 / 2 sampled requests where the upstream
returned the field.

**Symptom.** When the request enables `context-management-2025-06-27`,
Bedrock's response carries a top-level `context_management` object
(e.g. `{"applied_edits":[]}`) describing the auto-compaction edits it
performed. Routectl's Anthropic-egress wire-render strips it before
returning to the client. Direct upstream/egress diff via 4-direction
trace:

```
upstream success body keys: content, context_management, id, model,
  role, stop_details, stop_reason, stop_sequence, type, usage
egress response body keys:  content, id, model, role,
  stop_reason, stop_sequence, type, usage
```

The sibling drop of `stop_details` (Bedrock-only Anthropic-API
extension) is correct -- Anthropic Messages baseline doesn't have it.
But `context_management` IS in the Anthropic Messages spec when the
beta is enabled, and Claude Code uses it to surface what compaction
the server applied. Dropping it makes the feature half-broken: client
asks for compaction, server does it, but the client never learns
which messages were edited.

**Hypothesis.** The Anthropic-shape egress response struct in
`routectl-providers/src/anthropic_api/types.rs` (or
`routectl-cli/src/ingress/anthropic.rs` egress render) was built
against the pre-context-management baseline and doesn't have a
`#[serde(flatten)]` or explicit field for `context_management`.
Likely a small additive PR.

**Apply at.** Anthropic egress response type. Mirror handling for the
recently-stabilized `effort-2025-11-24` beta if it ships any
response-side fields too.

**Workaround.** None client-side. Operators can't add it; this is a
routectl-internal type.

---

### [INV-6+7] Bedrock egress: replace hardcoded beta allowlist with TOML-driven allowlists for both betas and body fields

**Problem.** `87ebd16` lifts the inbound `anthropic-beta` header into
the body and forward-sweeps unknown top-level keys to `provider_extras`.
Bedrock's strict schema validator 400s any unsupported value in either
surface (`"invalid beta flag"` or `"X: Extra inputs are not permitted"`),
so every claude-code request to Bedrock 4xxs.

INV-6 (`da48507`) fixed the beta side with a hardcoded
`BEDROCK_ACCEPTED_BETAS` const + optional `[bedrock] anthropic_beta`
TOML override. The body-field side (INV-7) has no fix yet.

**Empirically derived data (2026-05-12, direct boto3 to
`bedrock-runtime.us-west-2.amazonaws.com` against haiku-4-5 / sonnet-4-6
/ opus-4-7; identical results across all three).**

Bedrock-accepted betas (16):
```
context-1m-2025-08-07
claude-code-20250219
interleaved-thinking-2025-05-14
context-management-2025-06-27
effort-2025-11-24
fine-grained-tool-streaming-2025-05-14
computer-use-2025-01-24
computer-use-2024-10-22
mcp-client-2025-04-04
search-results-2025-06-09
tool-search-tool-2025-10-19
web-search-2025-03-05
structured-outputs-2025-12-15
task-budgets-2026-03-13
afk-mode-2026-01-31
token-counting-2024-11-01
```

Bedrock-accepted top-level body fields (per AWS docs +
empirical confirmation):
```
anthropic_version  anthropic_beta  max_tokens  messages  system
temperature  top_p  top_k  tools  tool_choice  stop_sequences
thinking  output_config  cache_control  metadata
context_management   # gated by context-management-2025-06-27
```

Bedrock-rejected body fields (forwarded by the ingress sweep, must be
stripped on the egress): `diagnostics`, `context_hint`, `speed`,
`mcp_servers`.

**Proposal: uniform TOML-driven allowlist for both surfaces. Drop the
hardcoded const entirely.**

```toml
[bedrock]
allowed_betas = ["context-1m-2025-08-07", "claude-code-20250219", ...]
allowed_body_fields = ["anthropic_version", "anthropic_beta", ..., "context_management"]
```

Filter: drop anything not on the list. Same shape on both surfaces.

**Why allowlist not denylist.**
- Bedrock is a strict subset of the Anthropic API.
- Default-deny means new Anthropic features don't silently 400 Bedrock
  on the day Anthropic ships them.
- Operator owns the list; no routectl release for AWS schema drift.
- Symmetric with the existing `[bedrock] anthropic_beta` semantics.

**Why no hardcoded default.**
- Bedrock allowlist drifts independently of routectl releases.
- Operators know their account's gating better than routectl can.
- A snapshot baked into the binary is wrong by definition for any
  operator that isn't the routectl maintainer's account.

**Replaces.**
- `bedrock::betas::BEDROCK_ACCEPTED_BETAS` const -> deleted
- `[bedrock] anthropic_beta` TOML field -> renamed `allowed_betas`
- Per-provider `[providers.X] anthropic_beta` floor -> kept as-is
  (operator-asserted always-send), but bypasses the allowlist filter
  same as today

**Apply at.** `bedrock/betas.rs` (already shared by Invoke + Converse)
plus a parallel `bedrock/body_fields.rs` with the same filter shape.
