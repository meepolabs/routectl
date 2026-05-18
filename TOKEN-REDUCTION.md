# Token Reduction (working note)

Branch: `feat/token-reduction-pipeline`. Working note, not committed.
Consolidated retrospective covering proposal / architecture / plan
phases now that Waves 1+2 have shipped.

## What shipped (Waves 1+2)

- **Item 4: cache_control auto-injection** -- OpenAI-in -> Anthropic-out
  paths get a `cache_control` breakpoint stamped at end-of-system (and
  optionally end-of-tools). Per-provider TOML; default off. Lives in
  `crates/routectl-router/src/cache_inject.rs`.
- **Item 3: tool_result compression** -- when a request's history
  carries a `tool_result` block over `min_prefix_tokens`, the local LM
  summarizes that block before forwarding upstream. Cache-safe
  (mutates message tail, not prefix). Lives in
  `crates/routectl-router/src/compress.rs` and the
  `routectl-local-lm/` crate (default-off feature `local-lm`).
- **Pipeline-stage trait surface** -- `ContentTransform`,
  `CacheInjectorPolicy`, `MessageSummarizer`, `PreflightPolicy` in
  `crates/routectl-core/src/pipeline.rs`. Stages run per-attempt:
  `compress -> cache_inject -> preflight -> dispatch`. Pre-pipeline
  `gate_peek` short-circuits on closed circuit / RPM-empty so a
  closed-gate provider does not pay full local-LM timeout cost.

## Cache-safety rule (read this before touching the pipeline)

Every transform must operate at a position the cache prefix does not
cover:

- **compress** mutates ONLY `Role::User` + `MessageContent::Parts` +
  `KnownContentPart::ToolResult.content`. Never touches `req.system`,
  `req.tools`, or assistant turns. Keeps the prefix byte-stable.
- **cache_inject** stamps breakpoints at end-of-system / end-of-tools
  AFTER compress runs. Order matters: stamp-then-mutate would
  invalidate the cache; mutate-then-stamp keeps it stable.
- Both stages run BEFORE the provider's per-attempt timeout envelope.
  `LocalLmClient` carries its own outer `tokio::time::timeout`
  (default 3s) so a hung local LM cannot stall the request.
- Compress is deterministic (sha256 over original content + summarizer
  model id + prompt template version). Re-running on the same input
  yields the same byte output, so the cache hash on the next request
  matches.

## Configuration

```toml
# Global compress config. Master switch + threshold + LRU.
[compress]
enabled = false               # opt-in
min_prefix_tokens = 1024      # bytes threshold = tokens * 4
lru_capacity = 2048

# Local LM endpoint. Required for compress to actually summarize;
# without it, NullSummarizer flows content unchanged.
[local_lm]
endpoint = "http://127.0.0.1:8080/v1"
model = "qwen3-30b-a3b-q6"
temperature = 0.0
max_tokens = 800
timeout_ms = 3000

# Per-provider cache_control auto-injection. Anthropic-shape
# egresses only (anthropic-api, bedrock-invoke, bedrock-converse).
# OpenAI-compat / openai-responses silently skip.
[providers.<name>.cache_inject]
enabled = true
min_prefix_tokens = 1024
positions = ["system"]        # also "tools"; both stamp last block
ttl = "5m"                    # only "5m" wired; "1h" WARNs and falls back
```

Per-provider compress overrides (`[providers.X.compress]`) parse but
are not yet wired -- `Router::new` emits a startup WARN if a
per-provider value differs from the global. Per-provider wiring lands
in a follow-up alongside per-alias compress overrides.

## Deferred -- Item 1: Speculative cascade

Local Qwen3-30B drafts a response; preflight policy decides return
draft / defer to cloud / rewrite request. Out of scope this branch.
Seam exists (`PreflightPolicy` trait + `DeferAlways` no-op impl); add
a real impl when there's a quality-vs-savings measurement ready.

## Out of scope (per original proposal)

- Local-classifier routing decisions (overlaps cost-aware routing,
  separate workstream).
- Repo-map / project-aware retrieval (covered by operator's existing
  Semble MCP + LSP servers exposed to claude-code).
- Reasoning-trace pruning on OpenAI Responses `encrypted_content`
  (niche; defer until measurable).
- Semantic response cache for agent traffic (false-positive rate
  dominates token savings on agent workloads per the survey).

## File map

- `crates/routectl-core/src/pipeline.rs` -- 4 trait surfaces + types
  (`EgressKind`, `PipelineCtx`, `SummaryRole`, `SummaryInput`,
  `PreflightDecision`, `DeferAlways`).
- `crates/routectl-router/src/compress.rs` -- `DefaultCompressor`,
  single-flight cache, `infer_role_hint` heuristic,
  `NullSummarizer`.
- `crates/routectl-router/src/cache_inject.rs` -- breakpoint stamping
  on `system` / `tools`; per-provider policy.
- `crates/routectl-router/src/pipeline_runner.rs` -- per-attempt
  ordering (`compress -> cache_inject -> preflight`).
- `crates/routectl-router/src/router.rs` -- `gate_peek` non-charging
  pre-pipeline check.
- `crates/routectl-local-lm/` -- thin OpenAI-compat HTTP client +
  `OpenAiCompatSummarizer` impl. Behind workspace feature `local-lm`
  (default-off).
- `crates/routectl-cli/tests/compress_integration.rs` +
  `cache_inject_integration.rs` -- wiremock end-to-end.

## Status as of branch tip

Branch is 8 commits ahead of develop's old tip (`7f37ed1`). develop
has since moved to `db7a260` (clippy/fmt cleanup). Rebase before PR
when ready to merge.

Hands-on validation against real claude-code traffic is operator-
gated; not run during implementation.
