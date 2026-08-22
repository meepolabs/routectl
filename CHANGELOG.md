# Changelog

All notable changes to routectl. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Versions 0.3.0 and 0.5.0 were development iterations that never got
standalone release tags; their changes are folded into the 0.4.0 and
0.6.0 entries respectively.

## [Unreleased]

A large body of work has landed since v0.9.0 and awaits the next
release cut. [ROADMAP.md](ROADMAP.md) "On develop" carries the same
list with more narrative.

### Added

- **`context_length` on `GET /v1/models`** -- each local discovery entry
  now carries the resolved target's context window in tokens, read from
  the same overlay-corrected catalog row the proactive context-window
  gate acts on, so a client and the router never disagree about a
  model's window. A chain alias reports its FIRST configured target's
  window. The field is omitted when routectl has no confirmed window
  (never `null`, never `0`); a `max_context_tokens` overlay cell supplies
  one for an upstream the baked catalog does not cover. Additive: no
  existing field changes.
- **OpenAI Responses ingress** -- a third ingress dialect at
  `POST /v1/responses`, feeding the same canonical request shape as
  the OpenAI Chat Completions and Anthropic Messages routes.
- **Native Gemini egress** (`kind = "gemini"`) -- Google
  `generateContent` / `streamGenerateContent` with API-key and Cloud
  Code OAuth (`oauth://antigravity`) auth modes.
- **Operator-supplied Cloud Code project id** -- `cloud_project_id` on a
  `gemini` provider entry names the seat's project outright (the BARE id,
  never the `projects/`-prefixed resource name). It is consulted before
  the credential store's cached id and before any onboarding call, so a
  cold request goes straight to inference, and it writes through to the
  seat's persisted id so other entries on the same seat pick it up (two
  entries naming one seat: last writer wins). If the upstream rejects the
  configured id as not applying to the seat, that request fails with one
  warning naming the entry and the rejected id, and the entry falls back
  to ordinary discovery for the rest of the process; quota, auth, and 5xx
  failures never trigger the fallback, and a restart re-trusts the
  configured value.
- **Deprecated cloud-code alias warning** -- a config load emits an
  advisory when a model entry on a cloud-code gemini provider routes
  `gemini-2.5-flash` or `gemini-2.5-pro` (still-serving server-side
  aliases the docs no longer recommend), naming a live-verified
  replacement. The same ids on the API-key lane stay silent; routing
  never changes. The cloud-code model documentation is rewritten as a
  dated verification snapshot against the live catalog.
- **Learned capability system** -- a per-target capability registry
  learned from live rejections and response evidence, persisted in
  the usage ledger and rebuilt at boot; consented active probes
  (`routectl probe --capabilities`); operator overrides
  (`[capability.overrides]`); a doctor truth-matrix panel with
  freshness/staleness hints.
- **Per-failure-class retry policy** -- a stable failure-class
  taxonomy (`rate-limited`, `auth`, `bad-request`, ...) with
  `[retry.classes.<class>]` per-class retry/fallback overrides and
  `[providers.X.class_overrides]` status remaps.
- **Model catalog** -- a baked cache-economics catalog plus a user
  overlay with provenance and verification stamps; `routectl catalog
  list/verify/import/set/disable/export`.
- **Config schema v4 + `config migrate`** -- versioned config with a
  deterministic migration ladder; `config set/unset/show/check` with
  dotted-key paths; a committed JSON Schema (`routectl.schema.json`)
  for editor completion.
- **Onboarding** -- the `routectl init` wizard, `provider add` with
  secret capture, `doctor` diagnostics with a stable exit-code
  contract, and reachability probes (`provider probe`).
- **Read-only status dashboard** -- a single-file dashboard at
  `GET /` plus `/status/{usage,health,config,doctor}` JSON panels;
  structurally read-only (no mutation routes exist).
- **Grouped usage query endpoint** -- `QUERY /status/query`, a
  windowed aggregate over the local usage ledger grouped by model,
  provider, or alias, with per-group cost resolution and a bounded
  per-request query budget. The request and response vocabularies are
  closed and versioned (`schema_version = 1`), and UNSTABLE pre-1.0.
- **First-party passthrough** -- an optional `[mitm]` front-proxy so
  Claude Code can route inference through routectl while Remote
  Control keeps working against `api.anthropic.com`; per-provider
  forwarded-credential mode (`credential_source = "forwarded"`).
- **Bedrock surface expansion** -- bearer-auth "mantle" lanes on the
  Anthropic and OpenAI provider classes; Converse request-side gap
  closure.
- **Codex identity currency** -- a config-overridable codex client
  version (`codex_version`) reaching every fingerprint surface from
  one derivation, plus a persistent installation id.
- **Cache stability guardrails** -- an advisory warning when a
  caller-cached prefix carries volatile content; optional tool-array
  normalization (`[cache] normalize_tools`).
- **Advisory context reduction** -- lossless dedup/supersession
  analysis with per-request would-save accounting and a
  confidence-bounded cache-hit estimator (observation-only; no
  request mutation). `routectl prompt-size` reports the same analysis
  offline.
- **Automatic prompt-cache breakpoint emission on the dispatch path** (default on). When a caller supplies no `cache_control` of its own, routectl adds a single top-level ephemeral 5-minute breakpoint over the stable cacheable prefix (system prompt + tool name/description strings) for capable providers, turning an otherwise-uncached prefix into a cache hit with no client change. The injection is lossless: applied to a per-attempt clone, re-validated before dispatch and rolled back on any doubt, and skipped entirely whenever the caller already supplied a breakpoint. Not applied to `count_tokens`.
  - **Kill-switches.** A global `[cache] auto_emit_top_level_breakpoint` (default true) plus a per-provider `auto_emit_top_level_breakpoint` override; the effective decision is "global on AND provider not explicitly off".
  - **Conservative per-provider capability.** A per-kind `cache_capability` default decides whether a provider honors a top-level breakpoint at all (anthropic-api / bedrock yes; openai-compat / unknown kinds no), overridable per entry. A `kind = "anthropic-api"` entry pointed at a non-default base URL fails closed until the operator opts in with an explicit `cache_capability`.
  - **Structural volatile-prefix veto.** A pure, non-mutating detector vetoes auto-caching a prefix that carries high-confidence per-request-volatile tokens (UUIDs, RFC3339 timestamps, JWTs, long hex blobs), so a churning prefix is never cached without payoff.
- **`[pools.<name>]` config blocks** -- group same-kind provider entries (accounts) under one seat-selection strategy. Each block takes `members` (a list of `[providers.X]` names), an optional `seat_selection`, and an optional `accepts_new_logins` growth marker (default false: a plain member list is pinned). `seat_selection` MOVED off `[providers.X]` onto the pool block, where it belongs -- it is a property of a set of accounts, not of one transport; its three strategies and the `fill-first` default are unchanged. Validation rejects an unknown member, a mixed-kind pool, a member claimed by two pools, a non-`oauth://` member (OAuth accounts only this release), a pool name held by a provider entry or a model nickname, an empty member list, and more than 32 members -- at `config check`, at the serve gate, and on hot reload (a rejected reload keeps the running router live).
- **Config schema version 4, with a `config migrate` rung that carries today's implicit multi-seat refs forward.** Version 4 makes `[pools.<name>]` the ONLY multi-seat shape: a bare `oauth://<provider>` ref now means the DEFAULT SEAT alone, and `#<label>` stays the single-seat pin. `config migrate` therefore does two things in ONE change with ONE confirmation. It relocates a provider-level `seat_selection` onto a `[pools.<name>]` block named after the provider family (offline, format-preserving, comments and key order intact). Then, reading the credential store READ-ONLY, for each provider entry whose bare ref covered more than one stored seat it materializes one suffixed account entry per labelled seat (`<family>-<label>`) and a pool listing them alongside the original entry -- byte-for-byte the names the login writer generates, so re-running is a no-op. Every `[models.X]` that routed at such an entry is repointed at the POOL, which is what preserves dispatch breadth: the entry's bare ref means one seat at v4, so a model left on it would silently drop from N seats to 1 through the very migration meant to preserve behavior. A single-seat bare ref needs no structural rewrite by construction: the default seat IS the one seat. Fail-closed throughout, leaving `config.toml` byte-identical: an unreadable credential store refuses the whole migration rather than silently narrow the ref (the store is what tells a single-seat family from a multi-seat one); a generated name held by an unrelated entry, two labels generating one name, an unusable label, or a pool name held by a provider entry or model nickname each refuse with the naming rule's own wording; a seat inventory that changed between the shown diff and the commit refuses; declining the confirmation, or any failure at any point, writes nothing. `--dry-run` prints the full combined candidate and writes nothing. A version-3 file on a version-4 binary exits nonzero at the version preflight with the `config migrate` pointer -- on `serve`, on `config check`, and on hot reload (where the running router stays live) -- never as an unknown-field parse error.
- **Ingress provenance** -- the canonical request now records which ingress dialect produced it (`Library` / `AnthropicIngress` / `OpenaiIngress`).
- **`routectl login` OFFERS the config change the new seat implies.** A successful login stored a working credential and wrote nothing to `config.toml`, leaving no seat using it and no sign a second step was needed. The login now plans that change, PRINTS it in full, and writes it on confirmation (`--yes` skips the prompt, never the print): the `[providers.<name>]` entry with the right `kind`, the auth selector that kind needs (`auth_kind` / `auth_mode`) and the `api_key_ref` -- carrying the `#<label>` fragment exactly when `--label` was passed -- plus a `[pools.<family>]` block, created with `accepts_new_logins = true` on a first login for the family or joined when a growth pool already exists. Reconciliation is by credential REF, not by name, so a hand-named entry already consuming the seat is recognized rather than duplicated; a re-login that changes nothing proposes nothing. Fail-safe throughout and never a nonzero exit before the confirmation: no config file, an unreadable or unparseable or too-old one, an ambiguous match (two entries carrying one ref), a pool pinned with `accepts_new_logins = false`, an entry whose required auth fields drift, a candidate the shared validation gate rejects, and a decline all print the block to paste by hand and exit 0 -- the credential is stored either way. Only a failure AFTER acceptance exits nonzero, saying the credential remains stored and was never rolled back. Login never CREATES `config.toml` (`config init` owns that), never flips an operator's existing growth marker, and never touches routing; after the write it names whatever routing is still missing (a `[models.X]` naming the pool, or an `[aliases]` entry reaching that model) without ever guessing an upstream model id. `provider add`, which drives the same login between its own snapshot and commit, opts out of the auto-surface and writes its own entry as before.
- **The shipped `examples/config.toml` passes `routectl config check` as shipped.** It previously failed the repo's own validator with three errors: two `file://<local-path>` placeholders and a `literal:` inline account id, which is rejected outright. The Anthropic and Codex OAuth entries now carry real `oauth://` references under the naming convention (`anthropic-default`, `anthropic-work`, `codex-default`), grouped by `[pools.anthropic]` and `[pools.codex]` blocks that teach the growth-pool shape including `accepts_new_logins = true`, and `[models.opus-direct]` points at the POOL. The api-key Anthropic entry is renamed `anthropic-api-key`, since a provider entry holding the plain family name makes the pool of that name unwritable. The Codex entry drops `account_id_ref` (derived from the session under an OAuth bearer). The example still needs credentials to serve -- unresolved refs are warnings, not errors -- and both the file and the docs now say so instead of calling it "working end-to-end". A hermetic unit test pins the zero-error contract.
- **Per-request cache strategy in `routectl usage`** -- the auto-cache decision token (`auto_emitted`, `caller_supplied`, `volatile_vetoed`, `auto_skipped:<reason>`) got a dedicated nullable `strategy` column in the usage DB (schema v2; migrate-on-open). The column exists in the schema but is write-stopped (see `Changed` below), so rows carry NULL and the decision is visible in the `cache_auto_decision` log instead. A `cache_auto_outcome` log warns on cache thrash (an auto-emitted breakpoint that created a cache entry but got no read).

- **Anthropic `thinking.display` is honored end to end.** A caller sending `thinking: {display: "omitted"}` on the Anthropic Messages ingress now gets an upstream request that carries it: the ingress translates `display` onto the canonical `reasoning.exclude`, and the `anthropic-api` egress stamps it back onto the wire `thinking` object as `display: "omitted"` (`exclude: true`) or `display: "summarized"` (`exclude: false`), on both the legacy `enabled` and the Opus 4.7+ `adaptive` shape. Under `"omitted"` Anthropic returns a thinking block with empty text and a full signature -- routectl keeps that block through response normalization, streaming, and multi-turn replay, so an omitted-display turn stays replayable and billing is unchanged. `display` is a closed two-value enum upstream, so an unrecognized value (or a non-string) is now a local 400 naming both accepted values instead of a slower upstream rejection. An ABSENT `display` stays absent on the wire: the upstream default is model-dependent, so routectl never materializes a value the caller did not send. The `bedrock-converse` egress STRIPS `display` with a WARN, because acceptance of the field on `additionalModelRequestFields` is unverified and a bad guess would 400 every thinking request; the Gemini and OpenRouter egresses continue to consume `reasoning.exclude` as before.

### Changed

- **`routectl init` and the capability probe's cost confirmation now DECLINE on a non-interactive stdin instead of waiting for an answer.** The same class as the egress-defining confirmation above, on the two remaining prompt surfaces: the `init` wizard's questions (scaffold-vs-wizard, offer selection, model id, default route, the write ack, and the credential-capture choice) and the `proceed with the probe?` confirmation `routectl probe --capabilities` asks after printing its cost estimate. Both read stdin unconditionally, so a caller whose stdin was an open-but-silent pipe blocked at the prompt indefinitely. With no terminal on stdin each prompt now declines immediately without reading, printing the question (or, for the probe, the cost estimate) so a scripted caller sees exactly what was declined, and naming the non-interactive flag: `routectl init --yes` and `routectl probe --capabilities --yes`. A declined `init` writes nothing and still prints its actionable next steps; a declined probe dispatches no calls. Both exit 0. A run with a closed stdin already declined and is unaffected, as is any interactive or `--yes` run.
- **`routectl doctor` now reports the config validator suite's ADVISORY findings, not just its errors.** The config section consumed the error half only, so a warning `routectl config check` printed -- an `oauth://` reference missing its `auth_kind` selector, a `class_overrides` remap that masks an outage, an empty `[retry.classes.<c>]` block, a per-block-breakpoint or codex-identity advisory -- was invisible in a doctor run, which reported "config passes the static validator suite" on a config the checker had flagged. Each advisory is now its own WARN finding, control-char-filtered through the same render seam as the errors, and the single PASS finding requires both halves empty. Exit codes are unchanged: only FAIL findings move the exit code, so a warnings-only config still exits 0. The doctor report `schema_version` (and the `/status/doctor` panel version that mirrors it) is 8.
- **The egress-defining confirmation now DECLINES on a non-interactive stdin instead of waiting for an answer.** `config set`, `config unset`, `provider add` and the `routectl login` config offer all prompt before a high-consequence (egress-defining) edit. That prompt read stdin unconditionally, so a caller whose stdin was an open-but-silent pipe -- a wrapper script, a CI step, a supervisor feeding a process that writes nothing -- blocked at the prompt indefinitely. With no terminal on stdin the confirmation is now declined immediately without reading, matching what `config migrate` and `catalog import` already did. The behavior change is visible to automation that relied on the block: such a run now exits 0 having written nothing, printing the egress-defining field list (so a scripted caller sees exactly what was declined) and naming `--yes` as the way to apply it non-interactively. A run with a closed stdin already declined and is unaffected, as is any interactive or `--yes` run.
- **An unlabelled `routectl login` now prints and writes `<family>-default`, not the bare family name.** The printed provider-entry name is taken from the shared naming convention, so the name a login SHOWS and the name a config write PICKS are one string; a bare `anthropic` entry would hold the name `[pools.anthropic]` needs and leave a later second seat with nowhere to go. User-visible in the login output: `[providers.anthropic-default]` where earlier builds printed `[providers.anthropic]`. Existing configs are untouched -- reconciliation matches by credential ref, so a hand-named entry already consuming the seat keeps its name.
- **Production hardening pass** -- auth-store lock ordering + atomic
  fsync-policy writes, network-exposure fail-closed middleware,
  request-body caps and disconnect-cancel semantics, retry-boundary
  fixes (no retry after the first content chunk), usage-ledger
  accuracy (`http_status`, error classes), OAuth refresh cooldown,
  and log-redaction tightening.
- **Streaming reliability** -- early-response commit with a
  flush-first grace window; interim usage semantics.
- **Performance** -- a criterion bench harness plus hot-path
  allocation reductions (zero-alloc token estimates, copy-on-write
  message sharing, byte-oriented ingress).
- **BREAKING: `[mitm] credential_source` removed.** A forwarded credential is now a per-provider choice, not a `[mitm]`-level one -- `[mitm]` reverts to transport-only (bind port, cert dir, upstream pin). A config still carrying the old key fails to load with an actionable error naming the exact replacement. Migrate by deleting the key and adding a provider block:
  ```toml
  [providers.anthropic-forwarded]
  kind              = "anthropic-api"
  base_url          = "https://api.anthropic.com"
  credential_source = "forwarded"
  ```
  No `api_key_ref` line -- a forwarded provider has no configured credential of its own.
  `GET /v1/models` also proxies through to Anthropic's live model list on the MITM reinject leg when a forwarded provider is configured and the request carries a captured client bearer; it falls back to the local alias list on every other case, including a proxy-side failure.
- **Usage ledger: three decision columns are write-stopped** -- the
  `requests` table's `strategy`, `reduction_strategy`, and
  `selection_decision` columns are no longer written. An audit confirmed
  no reader ever queried them back. The physical columns and the
  migrate-on-open ladder are unchanged, so existing databases open as
  before and historical values stay readable; rows written from this
  version onward store NULL. The three decision tokens are no longer
  persisted anywhere; they are visible only through the existing partial
  trace logging -- the auto-cache token in the `cache_auto_decision` /
  `cache_auto_outcome` lines (per dispatch), the reduction token only via
  a `context_reduction` line, which is emitted only when reduction
  actually stripped bytes (the `skipped:*` outcomes log nothing), and the
  seat-selection outcome only via the sticky birth-pick / overflow-repin
  DEBUG lines (`sticky_stay`, `defer_no_healthy` and the keyless
  fall-through log nothing). `UsageRecord` loses the three matching
  fields.
- **BEHAVIOR CHANGE: `temperature` and `top_p` are dropped on the
  own-OAuth Anthropic lane.** Anthropic's OAuth seat 400s a
  `/v1/messages` body carrying either param, so an `auth_kind =
  "oauth-bearer"` provider talking to `api.anthropic.com` (excluding the
  forwarded / pure-proxy leg) now strips both from the outbound body and
  logs one structured `WARN` per affected request naming only the dropped
  keys. `stop_sequences` is unaffected. The gate is the LANE, not the
  cloak setting -- **`cloak.mode = "never"` on such a provider still drops
  these params**, which is intended: the rejection is a property of the
  credential, not of the disguise, so honouring the knob would mean
  failing the request instead. Route to an API-key provider or a
  non-Anthropic host if you need them honoured. The `count_tokens` path
  is unaffected (its body allowlist already excluded sampling).
- **`effort-2025-11-24` is now unioned on demand.** A request whose
  assembled body carries `output_config.effort` gains the beta
  automatically on the own-OAuth `api.anthropic.com` lane, bypassing
  `allowed_betas` -- the same capability carve-out
  `structured-outputs-2025-12-15` already had. One-way: the body's field
  adds the flag, and a caller-supplied flag with no matching field is
  passed through untouched. Forwarded and API-key lanes are unchanged.
- **BEHAVIOR CHANGE: `[reduction] enabled` now defaults to `true`.**
  Dispatch-path context reduction (whitespace-only minify of
  JSON-valued string tool content in the mutable message tail) was
  opt-in; it is now on unless an operator turns it off. A config with
  no `[reduction]` block, an empty block, or the key omitted gets
  reduction; `enabled = false` still opts out globally, and a
  per-provider `reduction_enabled = false` still opts out that
  provider. The config schema version is unchanged and no migration
  runs -- the flip comes from the parse-time default, so fresh and
  existing installs pick it up identically. Expect one transient on
  upgrade: reduction runs before auto-cache, so any prefix previously
  cached in its unreduced form re-writes once against the new bytes,
  after which cache reads stabilize.

### Fixed

- **The Cloud Code (`auth_mode = "cloud-code"`) gemini lane no longer splits its traffic across two hosts.** Inference went to the production Cloud Code host while onboarding went to the daily one, so a consumer seat -- which is served on the daily host -- onboarded successfully and then had every inference request rejected on permission or quota, with nothing naming the endpoint as the cause. There is now exactly ONE `base_url` for the lane, defaulting to the daily host, and it moves `generateContent`, `loadCodeAssist` and `onboardUser` together: a production or enterprise seat sets that single value and the whole lane follows, and an explicitly configured value is forwarded verbatim, never rewritten. `config check` and startup emit a warning when a cloud-code entry pins the production host, `config show --effective` reports the host such an entry actually uses (it previously reported the api-key public base, a host this lane never talks to), and a permission / not-found / quota rejection now carries a suffix naming the host the request egressed to plus `base_url` as the recovery path -- hedged, since the same verdict can be earned honestly, and never acted on: no request is ever reissued against the other host. The client `User-Agent` is also composed from one pinned version and the real host platform rather than a stored literal, so the fingerprint no longer claims a platform routectl is not running on.
- **`config migrate` no longer stalls on the config shape earlier quickstarts taught.** A version-3 entry named after its own provider family (`[providers.anthropic]` carrying a bare `oauth://anthropic`) holds the exact name the materialized pool must take, and providers, pools and model nicknames share one namespace -- so the migration correctly refused, leaving the operator to hand-rename the entry and every `[models.X]` naming it before rerunning. That is the most likely real migration input, so the rename is now part of the migration: the entry moves to `<family>-default` (the name its own default-seat ref would generate, derived by the same naming module the login writer uses, so a later login still proposes nothing), the models that named it follow the rename before the pool repoint decides they should name the pool, and the whole thing rides in the SAME all-or-nothing combined diff with the same single confirmation. Every fail-closed guarantee is unchanged: the rename shows in the combined candidate, the change summary and the dry-run; a `<family>-default` name already held by an entry with a DIFFERENT credential still refuses with `config.toml` byte-identical (the migration never displaces one credential's entry with another's); and re-running over the migration's own output is still a no-op. Both the store-aware phase and the pure `seat_selection`-relocation rung rename, so a family-named entry migrates whether or not it carried the retired knob.
- **`config check` now flags an `oauth://` credential ref whose entry never selects the OAuth surface.** A provider entry pointing at a managed OAuth seat (`api_key_ref = "oauth://anthropic"`) but leaving its auth selector at the default (`auth_kind` unset, or `auth_mode` on a `gemini` entry) passed every validator, then dispatched the subscription bearer on the API-key header and 401'd on the first request -- with nothing in the config surfaces to say why. `config check` now emits a WARNING naming the entry, the selector key, and the value to set. WARNING rather than an error deliberately: the shape predates auth-selector enforcement, so hard-failing would refuse to load an otherwise-working config for a per-entry defect that only bites on dispatch. The required pairs come from the one auth-shape table the login output and the login auto-surface's drift check already read, so the shape `login` prints, the shape it validates, and the shape `config check` demands cannot diverge.

- **A Bedrock `count_tokens` 404 no longer sends the fallback chain past a working region.** The Bedrock CountTokens lane lifted any HTTP 404 to a capability error, on the premise that a region or model not offering the operation answers 404. Live bedrock-runtime captures refuted it: a 404 means the model resource was not found -- observed only as an end-of-life model id, returned by a region that served other models in the same session -- so a stale model id in the config made the chain silently step past a fully capable seat and replaced AWS's actionable message (`This model version has reached the end of its life.`) with a generic capability 501. The lane now propagates every upstream status unchanged, so the operator sees the real cause. HTTP 400 is still deliberately not a capability signal: it is overloaded across invalid model id, model-cannot-count, and malformed-body causes, and the code now records all three.

- **Anthropic structured-outputs requests now carry the required beta flag on api-key auth.** routectl emits `output_config.format` whenever a request carries a JSON-schema `response_format`, but the `structured-outputs-2025-12-15` beta flag was injected only on the Claude-Code OAuth identity path -- an ordinary `x-api-key` provider sending a schema request emitted the beta-gated body field with no beta header. The flag is now unioned onto the request whenever the assembled body carries `output_config.format`, on every auth kind, driving off the assembled body rather than the raw `response_format` (so a format arriving via `provider_extras` triggers it too). It is applied to both the api.anthropic.com header and the Bedrock-Invoke body -- on Bedrock the union runs after the `[bedrock] allowed_betas` filter, so a restrictive allowlist that omits the flag can no longer strip it back off and ship the gated field ungated. A Claude-Code request's beta list is byte-identical to before because the union is idempotent.

- **Bedrock Converse no longer drops citations from a raw tool-result document.** A `tool_result` content array carrying an Anthropic-shape `document` element with citations enabled reached the Converse wire without its citations config, so the caller asked for citations and the model returned uncited text with no warning. That path was hand-assembling the document wire block and never read the sibling `citations` key; it now lifts citations through the same shared helper the two canonical document paths use, so enabled / disabled / absent / malformed citations behave identically regardless of how the document reaches the egress.

- **An unusable tool-result correlation id now fails the same way on both openai-responses ingress shapes.** A tool output whose correlating id was empty was rejected with a normalize error when it arrived as a canonical tool-role message, but logged at WARN and dropped when it arrived as an Anthropic-shape `tool_result` content block -- so the same malformed history either failed loudly or silently lost a tool result depending on which ingress produced it. The Responses API binds an output to its call by `call_id` alone, so an empty id cannot produce a usable request; both shapes now reject it up front rather than sending a request the model answers without the tool result.

- **Bedrock Converse streaming no longer drops text and reasoning deltas.** The ConverseStream decoder required a prior `contentBlockStart` frame at a given `contentBlockIndex` before it would accept a `contentBlockDelta` there, so any block without one had every delta silently skipped. AWS emits `contentBlockStart` for tool use only (`ContentBlockStart` is a union of `image` / `toolResult` / `toolUse`), which means text and reasoning blocks never get one and a streamed response could arrive as an empty assistant turn with a valid `finish_reason` and correct usage. Text and reasoning blocks now open lazily on their first delta; tool-use blocks keep their start-driven path, and a text or reasoning delta landing on a tool-use index is still skipped.

- **`count_tokens` no longer trips the shared circuit breaker on a capability error.** When the first count_tokens-capable seat is capable by kind (`anthropic-api`) but its upstream does not implement `count_tokens` (e.g. an `anthropic-api` base URL that forwards to a Bedrock Invoke egress), it returns a wire 501. That 501 was recorded as a health failure on the per-model breaker shared with completions, so a steady stream of count_tokens probes could flap the breaker open and force completions onto their fallback. `count_tokens` now treats a capability error (local `NotImplemented` or a wire 501) as capability, not health: it releases the probe slot without a breaker debit and walks to the next capable seat, returning a real count. Completion-path 501s are unchanged and still trip the breaker.
  - The per-seat capability 501 now logs at `debug` (it is the steady-state path when a passthrough seat cannot count); other `count_tokens` upstream errors and all completion-path 501s stay at WARN.

### Security

- **`config migrate` no longer merges distinct OAuth egresses into one pool.** Phase 2 keyed the materialized pool purely on the provider family, so two or more `[providers.X]` entries carrying a bare `oauth://<family>` ref -- including entries with deliberately different `base_url` values -- collapsed into ONE `[pools.<family>]` block, and every model naming either entry was repointed at it: each account's OAuth bearer would then dispatch to every egress host in the merged set. The same path could also add members to a hand-authored pinned pool (`accepts_new_logins = false`). Both are now typed fail-closed refusals that name the offending entries and leave `config.toml` byte-identical: pin all but one entry with `oauth://<family>#<label>`, or write the pool blocks by hand. The pure v3 -> v4 rung refuses on the matching shape too (two entries deriving one pool name, where the second's relocated `seat_selection` silently overwrote the first's).
- **Router error-path and quota-header log fields sanitized.** Five `%`-rendered tracing fields still carried unfiltered caller or upstream bytes: the ingress `NotImplemented` arm's provider and detail, the router's `count_tokens` and capability-filter WARNs (requested alias plus the derived feature list), and the five Anthropic subscription-quota header values (`to_str` rejects CR/LF/ESC but passes tab and caps nothing). All now route through `sanitize_for_log`, and the Bedrock Converse billing-block WARN no longer files the caller's requested model under a `provider` field.
- **Operator-config-derived pool and seat surfaces sanitized.** The pool-omission WARN, the zero-usable-pool boot / reload refusal (and the reload rejection that re-logs it), the doctor config-validation findings, and the doctor auth and orphan-seat findings all rendered operator-written table keys and seat labels with no control-char filtering, so a key or `login --label` value bearing a newline plus an ANSI sequence could forge a whole log record or a fabricated doctor finding line. All now route through `sanitize_for_log`; the config-validation render point covers every validator at once. The seat-report sanitizer additionally neutralizes `=` and the backtick, the two remaining characters the pool sentence's own grammar uses.
- **The log-display gate is fail-closed and no longer bypassable.** Its sanitizer check was line-scoped, so a trailing comment mentioning `sanitize_for_log` -- or one sanitized field sharing a line with a raw one -- passed; the sanitizer must now sit on the flagged field's own value. Two previously unscanned shapes are covered: a field name split from its `= %` across lines, and tracing's positional `%field` shorthand. A missing ripgrep or a renamed search path is now a gate failure rather than a PASS over zero scanned files, and CI runs the gate and its self-test.
- **`ROUTECTL_LOG_REDACT_PROMPTS=1` now also covers unrecognized reasoning format tags.** BREAKING for anyone parsing the field: the foreign-format reasoning WARN's `skipped_formats` echoed a caller-chosen `format` tag verbatim regardless of the redaction knob. Tags in routectl's recognized vocabulary (`anthropic-claude-v1` plus the Responses-family tags) still echo in both knob states; a tag outside it now renders as the literal `<unrecognized>` under the knob, with every unknown tag in a request sharing that one placeholder slot. Run with the knob off to read the literal of an unrecognized tag. See [docs/LOGGING.md](docs/LOGGING.md) "Redaction".

## [0.9.0] - 2026-06-18

### Added

- **Per-request usage accounting** (`[usage]` block + `routectl usage` CLI). The new `routectl-usage` crate writes one row per request to a local WAL-mode SQLite ledger (mode 0600) off the hot path; a usage-subsystem failure never affects serving.
  - `routectl usage` reports per-model-per-window aggregates (today / week / month / all-time, or `--since/--until`), humanized (K/M/B); `--detail` adds time-to-first-token p50/p95, throughput tok/s, and the cache-write 5m/1h split.
  - Query-time cost from a `[registry."<glob>".pricing]` table; subscription rows show `n/a (subscription)`. Config: `enabled`, `db_path` (restart-only), `retention_days`.
- **OAuth credential seat pools.** `routectl login --label <name>` registers named seats; a bare `oauth://<provider>` with more than one seat expands into one dispatch target per seat (each with its own breaker state), ordered by `seat_selection` (`fill-first` / `round-robin`). Re-expands on credentials hot-reload. Single-seat and non-oauth refs are unchanged.
- **Managed Claude Code identity on the Anthropic OAuth egress.** For requests that lack one, injects the canonical identity system block, mints a stable per-seat `x-claude-code-session-id`, synthesizes `metadata.user_id`, and auto-injects the Claude Code `anthropic-beta` floor. The billing-header checksum is re-signed over the final bytes; genuine Claude Code requests pass through unchanged.
- **Codex `session-id` header** on the ChatgptOauth egress (closes a backend correlation gap; never logged).
- **Anthropic unified-quota / overage observation** on the OAuth path: tolerant parse of `anthropic-ratelimit-unified-*`, a WARN on entering overage and INFO on recovery, surfaced in the `routectl usage` quota line. A missing or malformed header never fails a request.
- **Per-model `reported_model`** -- pin the `model` echoed to clients to a fixed label, independent of alias or fallback target (#53, thanks @juukas).
- **Per-model `visible_routectl_provider`** (default `true`) -- set `false` to drop `routectl_provider` from a model's responses.
- **OpenAI file / PDF parts** are translated to native document blocks on Anthropic / Bedrock-Converse (and `InputFile` on openai-responses); untranslatable shapes are re-emitted verbatim so the upstream errors cleanly.
- **Graceful shutdown** -- SIGTERM/SIGINT drains in-flight streaming responses (20s deadline) instead of severing them.

### Changed

- **BREAKING: the response `model` field now echoes the client-requested alias**, not the upstream's internal id (e.g. no longer `deepseek-chat`). Pin it with `[models.X] reported_model`; usage and cost accounting still key off the real upstream.
- **Upstream-failure WARN messages** normalized to stable literals (`upstream auth failed` / `upstream error`) with structured fields, so log filters keyed on the message string are reliable across egresses.
- **Dependency + CI maintenance** -- cargo deps and pinned GitHub Actions bumped; cargo-deny allows BSL-1.0 (xxhash-rust).

### Fixed

- **Claude 4.x `temperature` + `top_p`**: the Anthropic-shape egresses emit `top_p` only when no `temperature` is set, fixing a hard 400.
- **OpenAI top-level `reasoning_effort`** is promoted into canonical `reasoning.effort` at ingress, so it triggers thinking on Claude / Bedrock instead of leaking into the body.
- **`content_filter` -> `refusal`**, and empty tool arguments render as `{}` (not `null`) on the Anthropic-ingress render.
- **OpenAI schema gaps**: bare-string `stop`, `refusal`, and `logprobs` preserved; cache-read tokens surfaced as `prompt_tokens_details.cached_tokens`.
- **Upstream error `type` / `code` / stop-reason** threaded through to the client; 503 / 529 map to `overloaded_error` on both dialects.
- **Foreign-shaped thinking signatures** are dropped on the Anthropic egress instead of forwarded as an invalid signature.
- **Exact effort-to-budget table** replaces the proportional estimate; openai-responses converts a budget-only request to the nearest effort level.
- **Tool-call ids** sanitized to the Anthropic charset at every emit and correlation site, injectively -- two distinct source ids never collapse onto one wire id.
- **Assistant `tool_calls`** re-emitted as native tool-use on Converse and openai-responses, so multi-turn tool loops no longer break.
- **openai-compat wire-lift** shapes corrected for strict hosts (json_schema `name`, stray `cache_control`, thinking-only turns, unrepresentable blocks, dropped `is_error`).
- **openai-compat egress fidelity**: text-only `tool_result` collapse, Anthropic `metadata` blocked, `max_completion_tokens` restored for o-series / gpt-5.
- **gpt-5 chat models** keep sampling params; the HTTP client gains a connect timeout.
- **openai-responses egress** hardened: reasoning replay under `store=false`, a redacted-blob leak, atomic cookie-jar write, billing-drop WARN ordering, and the non-streaming output cap.
- **openai-compat RawThinkTag stream** recovers terminal usage and mid-stream errors via a shared error mapper.
- **Anthropic egress**: orphan-effort invariant, billing re-sign scope, and an SSE undeserializable-payload fallback.
- **Anthropic ingress**: unified error mapping, a `message_start` before an empty `message_stop`, object-form tool args, and upstream-attributed mid-stream failures.
- **Router hardening**: strict `[server]` / `[auth]` keys, alias-glob validation, zero-cap-as-unset, IPv4-in-IPv6 SSRF canonicalization, breaker closes on the first chunk, and the original upstream error preserved on a retry refusal.
- **Audit batch**: adaptive-thinking with a forced `tool_choice` no longer 400s, `response.incomplete` is terminal, n>1 stop-sequence false positives fixed, think-tag flush at stream end, and a 300s idle-read timeout.
- **Low-severity batch**: terminal `output_tokens` default, opaque-block close, cache eviction-on-lookup, Bedrock decode recovery, and an empty `toolResult` on Converse.

### Security

- **Claude Code billing-attribution block stripped on every egress, including anthropic-api** (all auth kinds, hosts, and entry points -- the canonical `system`, the legacy `Role::System` lift, and the OAuth identity path), so its client fingerprint never leaks to any upstream.
- **Bedrock** no longer forwards client metadata; 403s return only the IAM action (not the principal ARN or account id); other error bodies are capped and sanitized.
- **TRACE / DEBUG redaction widened** to file payloads, document citations, response headers, AWS signing-token / cookie headers, and Bedrock inline `source.bytes`; the sweep is hardened against absent keys.
- **Listener auth**: empty / whitespace tokens rejected at startup; the OAuth HTTP client is built identity-neutral; the stdin login read is bounded.
- **Internal-reference guard**: a checked-in ID scanner + pre-commit hook + CI gate block planning identifiers from committed code and messages.

### Documentation

- Reference-drift sweep across `README`, `ROADMAP`, `CLAUDE.md`, and `docs/*`: added the `routectl-usage` crate to the maps, documented the seat-pool / `--label` workflow, the `routectl usage` CLI, `reported_model`, and `visible_routectl_provider`, and corrected log-shape, config-key, and fix-site references.

## [0.8.0] - 2026-06-07

### Added

- **Config-overridable identity defaults (codex + anthropic egresses).** Compiled identity-header defaults fire on zero-config; operator `header_extras` overrides any key.
  - Merge order in `build_headers`: auth headers (never overridden) -> compiled defaults -> `header_extras` -> per-request UUIDs (always win).
  - `chatgpt-oauth`: emits `originator`, `x-openai-internal-codex-residency`, `version` from pinned constants.
  - `oauth-bearer` anthropic-api: emits `x-stainless-*`, `x-app`, `anthropic-dangerous-direct-browser-access` (dynamic OS/arch); `user_agent` defaults to `claude-cli/<version>` when unset.
  - Superseded `OpenAiResponsesConfig` fields (`session_id`, `installation_id`, `originator`) are now auto-generated or carried via `header_extras`.

- **codex CLI client-header parity (chatgpt.com surface).** The `openai-responses` codex egress and its OAuth refresh client present every required codex CLI client header, fixing sessions the upstream rejected within minutes and required re-authentication to recover.
  - New `routectl-core::codex_fingerprint` module: single source of truth for the codex `User-Agent`, the `originator`/residency defaults, and the process-global `x-codex-window-id`.
  - Per-request headers stamped: `version`, `session-id`, `x-codex-installation-id`, `x-codex-window-id`, `thread-id`, `x-client-request-id`.
  - `session_id` is per-credential: persisted on the `credentials.json` token record, minted on `routectl login codex`, lazy-backfilled into older records, preserved across refresh.
  - Static identity headers ride the config-overridable defaults above (overridable via `header_extras`); `thread-id` / `x-client-request-id` are a fresh UUIDv4 per turn.
  - Pinned codex version and the client-header contract are documented in `docs/PROVIDER-QUIRKS.md`.

- **`bedrock-mantle` bearer-token auth on the `openai-responses` provider.** New `auth_kind = "bedrock-mantle"` replaces the prior `NotImplemented` stub.
  - Unlocks AWS Bedrock GPT-5.5 (and future Mantle-hosted Responses models) with no new provider kind.
  - Wire shape is verbatim OpenAI Responses, so request assembly, the SSE state machine, decoding, and the URL builder are reused unchanged.
  - Only delta is the bearer source (typically `env://AWS_BEARER_TOKEN_BEDROCK`); the `/openai/v1` prefix lives in `base_url`.

- **Hot-reload of `credentials.json` and `config.toml`.** A single watcher on the parent directories (`IN_CLOSE_WRITE`/`IN_MOVED_TO`, 250ms debounce) plus a `SIGHUP` escape hatch feed one reload coordinator.
  - Parse-validate-or-keep-old: an invalid write logs a WARN with the parse error and keeps the current config; only successful parses swap in.
  - Hot-reloadable: `[providers]`, `[models]`, `[aliases]`, `[retry]`, `[bedrock]`, and `credentials.json`.
  - Restart-required (emits a diff-WARN on change): `server.host`, `server.port`, `server.auth`, `server.max_body_bytes`, `[log]`.
  - Circuit-breaker counters, RPM buckets, the OAuth token cache, and per-provider refresh mutexes survive the reload.

- **`[server] max_body_bytes`, per-model `max_output_tokens`, and raised internal caps.**
  - `max_body_bytes`: request-body cap, default 32 MiB (was a hardcoded 4 MiB); returns 413 above the cap.
  - `max_output_tokens`: caps the injected `max_tokens` on the Anthropic-shape egresses (`anthropic-api`, `bedrock-invoke`) when the caller omits it. Resolution: `request.max_tokens` -> `[models.X] max_output_tokens` -> `64000` baseline. Other egresses forward omission cleanly.
  - Caps raised for parallel-tool-fanout / long conversations: anthropic ingress tool-call index 64 -> 4096, openai-responses output-block count 512 -> 4096, anthropic-api thinking-cache entries 1000 -> 10000.

- **Per-provider `max_thinking_entry_bytes` knob** (`[providers.X]` of `kind = "anthropic-api"`). Tunes the per-entry byte cap on the `context_management` thinking-cache.
  - Range 1 KiB to 4 MiB, default 1 MiB; zero falls back to the default with a WARN; over-ceiling clamps with a WARN.

- **Startup WARN when `context_management = true` without `history_reasoning = "preserve"`.** The two are complementary on non-Anthropic anthropic-api endpoints (DeepSeek `/anthropic`, vLLM, LM Studio); without `preserve`, the next turn 400s on missing thinking. The WARN fires once at startup instead of per dispatch.

- **`max_completion_tokens` -> `max_tokens` translation (OpenAI Chat ingress).** o-series / gpt-5+ clients send `max_completion_tokens`, which the canonical request lacked, so it was silently dropped. It is now renamed before deserialization (`max_tokens` wins if both are present).

- **`role: "developer"` support (OpenAI Chat ingress).** The system-voice successor `developer` role is rewritten to `system` before deserialization, so it flows through the system-message lift instead of failing with a 400.

### Changed

- **`Error::Internal` added to the error taxonomy** for unexpected runtime failures (serialization bugs, socket / serve-loop IO, impossible states).
  - Six sites that misused `Error::Config` for non-config failures are reclassified.
  - The HTTP mapping returns a generic `internal error` to clients while logging detail at ERROR; `Error::Config` is now documented as configuration-validation-only.

- **Default retry jitter is now 50ms** (`RetryPolicy::default().jitter_ms`, was 0), giving retry spread out of the box.

- **`context_management` thinking-cache per-entry cap raised 256 KB -> 1 MiB.** The 256 KB cap rejected writes on full-budget Opus 4.6/4.7/4.8 turns (~328 KB at 65k thinking tokens); 1 MiB gives ~3x headroom. Tune down via `max_thinking_entry_bytes`.

- **`context_management` thinking-cache TTL is now sliding.** Each hit refreshes `expires_at` to `ttl-from-now` (matching Anthropic / DeepSeek prompt-cache semantics); idle entries still die after the 60-minute window.

- **Internal:** structural decomposition and shared-helper extraction across the bedrock and anthropic-api modules (shared eventstream framing driver, hoisted HTTP/header helpers, request-builder split, consolidated identity module, unified provider seam naming, test-builder dedup into `routectl-core`) -- no behavior change.

### Fixed

- **Bedrock Converse `stop_sequence` round-trip.** The request always declares `["/stop_sequence"]` in `additionalModelResponseFieldPaths`, and routectl reads the matched literal back (gated on `stop_reason == "stop_sequence"`) on the non-streaming and streaming paths. A schema-drift DEBUG fires when the stop reason indicates a match but the field is absent.

- **anthropic-api effort caps no longer bypassed via `provider_extras`.** `merge_provider_extras` overwrote the clamped `output_config.effort` with the raw caller value, so a client could ship an unclamped effort (e.g. `max`) despite a declared `effort_levels`. The post-merge seam now re-clamps `output_config.effort` (sibling sub-keys untouched). Affects `anthropic-api` and `bedrock-invoke`.

- **anthropic-api: probe `thinking` 400, `signature: null`, and openai-compat envelope leak.**
  - Claude Code probes (`max_tokens` 48-128) 400'd because legacy thinking derived a budget with no floor; `thinking` is now dropped with a WARN when `max_tokens` cannot fit a >=1024 budget plus content, instead of mutating the caller's `max_tokens`.
  - A reasoning detail with no signature rendered `signature: null` (rejected on a mid-conversation provider switch); the field is now omitted when absent.
  - openai-compat envelope fields (`object`, `system_fingerprint`, `cost`, top-level `role`) and usage sub-bags are filtered at the openai-compat parse seam so they stop leaking onto Anthropic-shape responses.

- **`count_tokens` body allowlist drops `metadata`.** `/v1/messages/count_tokens` 400'd on `metadata` ("Extra inputs are not permitted"); it is removed from the allowlist (`output_config` is documented as accepted but token-count-irrelevant).

- **anthropic-api: opaque-block stop sentinel and dropped-reasoning WARN.** A degraded opaque (unknown-type) block that overflowed the capture cap after its `content_block_start` left an unclosed block; `content_block_stop` is now emitted unconditionally. A structured WARN fires when `emit_reasoning_blocks` drops non-`anthropic-claude-v1` `reasoning_details`.

- **anthropic-api `allowed_betas` enforced on the `anthropic-beta` header.** The allowlist applied only to the (stripped-before-send) body field, so the header the upstream inspects carried the unfiltered list. Header and body now share one predicate.

- **OpenAI ingress: tool_use double-render, system `cache_control` loss, stop-sequence strip.**
  - tool_use parts are stripped at the OpenAI render seam when `tool_calls` is present, so Chat clients stop receiving duplicate tool_use blocks (Anthropic paths untouched).
  - Parts-form system messages now emit `SystemContent::Blocks` when a block carries `cache_control`/citations, instead of flattening to plain text.
  - The internal `matched_stop_sequence` field is stripped from every choice on both render paths.

- **anthropic ingress: streaming usage and finalization.** The closing `message_delta` now defaults a missing `prompt_tokens` to 0, attaches usage only when populated, early-returns from the error-eos path once finished, drops a JSON-null `output_format`, and builds the non-streaming usage object incrementally (no more null cache-usage fields).

- **openai-compat: mid-stream errors and hardened lifts.** A mid-200 error envelope now reports as an upstream error (not a chunk-deserialize error); a JSON-null usage sub-bag no longer blocks the usage lift; a `tool_result` without `tool_use_id` hard-fails (matching anthropic-api); the reasoning-strip WARN catches the `Thinking` part shape; and streamed `reasoning_details` increment `detail_index` per block.

- **openai-responses: signature passthrough, cancel semantics, reasoning logging.** The Anthropic-shape signature is forwarded into `encrypted_content` only when the source format matches; `response.cancelled` and a payload-less `response.completed` surface as upstream errors; dropped non-`openai-responses-v1` reasoning is summarized in one DEBUG.

- **bedrock-converse: forward text documents and envelope `reasoning_details`.** Text-source documents are normalized to base64 instead of dropped; unmapped/missing media types log on drop; the egress emits `ReasoningContent` for `anthropic-claude-v1` details (skipping unsigned blocks Bedrock rejects) and no longer emits an orphan `cachePoint` for an empty system block.

- **bedrock: reserve `x-amz-*` headers and sign the User-Agent.** An operator `header_extras` `x-amz-*` entry could desync the SigV4 signature, so the prefix is now reserved. The UA is inserted pre-sign (SigV4-signed and trace-visible); a chunk frame missing its `bytes` field is skipped with a WARN instead of killing the stream.

- **router: half-open breaker probe slot released on all exits.** Several dispatch exits (probe fast-fail, auth-refresh failure/success, retry-without-fallback, non-fallbackable client errors) returned without recording an outcome, leaking the single probe slot and pinning the breaker open until restart. A no-debit slot release now runs on every such exit, and the gate runs inside the retry loop in `stream`/`count_tokens` to match `complete()`.

- **router: 429 non-retryable when excluded from fallback; operator beta floor and `header_extras` preserved.** The 429 retry arm is now gated on fallbackability like the 5xx arm; a model-level `anthropic-beta` rides a separate operator-floor field (re-added after the `allowed_betas` filter instead of being dropped); and `apply_layered_overlays` preserves the composed `header_extras` map across the `routectl_internal` rebuild.

- **factory: dedup bedrock credential probe on the failure path; WARN on the bedrock-mantle region pin.** Sibling Bedrock models no longer re-run the SSO probe after one fails (the failure path now consults `provider_failed`); the `us-east-1` bedrock-mantle endpoint fallback now WARNs so other-region operators are not silently misdirected.

- **auth: close the OAuth reload/refresh race; bound the login read.** A generation counter lets a stale refresh detect a concurrent `reload_from_disk` and discard its result; the unbounded login-line read is replaced with a bounded `read_line` loop.

- **file-watch: suppress the misleading Remove WARN on atomic rewrites.** Atomic-rewrite editors issue Remove + Create in one batch; the handler now matches the sibling Create/Modify by basename and emits a DEBUG breadcrumb instead of "watched file was removed". Lone Remove events keep the WARN. (#38)

- **build: the `openai-responses` feature now declares `dep:chrono`** (previously it built only when a sibling feature pulled `chrono` in).

### Documentation

- **Reference-drift sweep** across `CONFIGURATION.md`, `CODEMAP.md`, `ARCHITECTURE.md`, `LOGGING.md`, `PROVIDER-QUIRKS.md`, and `WIRE-GOTCHAS.md`: the bedrock framing driver, the two-tier retry resolution and per-error-class caps, the bedrock `api_shape` selector, the 256-char WARN body-excerpt cap, 6 reasoning dialects, the 300s OAuth refresh lead, per-provider runtime gates, config-show secret redaction, the non-loopback-bind auth requirement, and the per-direction header-trace policy. Each verified against the cited code.

- **New `[server]` knob docs** in `CONFIGURATION.md`: `max_body_bytes` and `allow_disable_fallbacks` (plus its `x-routectl-disable-fallbacks` header), and the per-model `max_output_tokens` resolution chain.

- **Replay harness recast** around a local-only fixture corpus: `REPLAY-FIXTURES.md` is now a format reference, the "Adding a replay fixture" flow collapses to 5 steps, and the capture script stamps the workspace version into fixture metadata.

### Security

- **Eventstream payload logging gated behind TRACE on both Bedrock decoders.** The Converse decode-error path and `contentBlockStart` handler logged decoded, upstream-controlled output (possible prompt-derived secrets/PII) at WARN/DEBUG; both now log only the 12-byte prelude (or top-level key list) and gate the full payload behind TRACE, matching the Invoke decoder.

- **Bearer JWT redacted from the outgoing-headers TRACE.** `routectl-core::log_safe` emitted `Authorization: Bearer <jwt>` verbatim, exposing live access tokens. The fix lives at the lowest trace layer (all four providers covered); `Authorization`, `x-api-key`, and `proxy-authorization` values are masked while names are preserved.

- **Cloudflare cookie jar on the chatgpt.com client.** The `openai-responses` provider attaches a persistent jar (default `~/.config/routectl/cookies/chatgpt.json`, mode 0600, `ROUTECTL_COOKIE_FILE`-overridable), allowlist-filtered to Cloudflare service-cookie names so account/session cookies never hit disk.

- **OAuth refresh tracing with `sha256[0:8]` hashes.** Pre-POST, success, and failure events emit grant type, status, and hashed-token correlation; token values are never logged, failure events omit body excerpts (some endpoints echo `refresh_token`), and a canary test pins that no echoed token leaks.

- **Upstream error bodies sanitized against log-line forgery.** Upstream-controlled 4xx/5xx bodies and the forward-compat SSE `content_block.type` were logged without control-char filtering (CR/LF/ANSI could forge log lines); openai-compat, openai-responses, bedrock, the shared DEBUG full-body helper, and the SSE block-type capture now filter them.

- **Secret-ref values redacted from error messages.** `SecretRef::parse` and the listener-token resolver embedded the raw reference in error strings; they now report only a validated scheme prefix (or, for listener tokens, the entry position).

- **Operator `header_extras` cannot override codex client headers** on the ChatGPT-OAuth path, keeping the client-header contract intact.

- **Per-entry size bound on the anthropic-api thinking cache.** Oversized writes are rejected with a WARN (the next turn recovers as it would on a TTL eviction), preventing unbounded LRU growth. Truncation was rejected -- it would corrupt the opaque continuity signature on Anthropic thinking blocks.

## [0.7.0] - 2026-05-30

The v0.7 release: routectl-managed OAuth login (Anthropic + Codex)
with runtime refresh and one-shot 401 recovery, claude-code as a
first-class gateway client, forward-compat for unknown Anthropic SSE
block types, server-side emulation of the context-management beta
for non-Anthropic anthropic-api endpoints, and a BREAKING refactor
moving model reasoning capabilities from per-provider floors to
declarative per-model fields.

**Responsible use.** Anthropic publishes a gateway pattern at
<https://code.claude.com/docs/en/llm-gateway> for first-party
deployments. routectl's `oauth://anthropic` ref and gateway support
are for personal-use proxying with the operator's own subscription
token; per the Anthropic Agent SDK overview, claude.ai OAuth tokens
may not be embedded in third-party products. routectl does not
support or condone gateway usage beyond what the upstream provider
permits and does not vouch for whether a particular credential is
permitted to be used a particular way -- read the upstream
provider's terms before pointing routectl at production traffic.
See [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) "claude-code as
a gateway client" for the operator setup, and the README
"Responsible use" section.

### Added

- **routectl-managed OAuth client** (Anthropic + Codex).
  - `routectl login <provider>` runs PKCE through a loopback callback, persists tokens atomically to `~/.config/routectl/credentials.json` (chmod 0600, tempfile + fsync + rename).
  - New `oauth://<provider>` SecretRef resolves at request time through a `CompositeStore`.
  - `--print-url` headless flow on Anthropic; rejected for Codex (no headless paste-back; SSH operators port-forward 1455).
  - `routectl whoami` reports stored token expiry.
  - `SecretToken` newtype zeroizes on drop with a redacted `Debug`.

- **OAuth runtime refresh + 401 recovery.**
  - Lazy refresh at egress: `oauth://` resolution checks near-expiry (300s lead -- matches the codex CLI 5-minute refresh window), acquires a per-provider mutex, double-checks under the lock so concurrent gets collapse to one refresh per window, persists atomically.
  - New `Provider::on_auth_failure` hook lets a 401 force-rotate the token and retry the same provider once.
  - New `routectl logout <provider>` and `routectl refresh <provider>` ops subcommands.
  - Migration: replace `env://ROUTECTL_ANTHROPIC` with `oauth://anthropic` after running `routectl login anthropic` once.

- **claude-code as a first-class gateway client.** Implements the Anthropic-published gateway contract for Anthropic Messages, so claude-code with `ANTHROPIC_BASE_URL=http://routectl` works without silent capability downgrades.
  - New per-provider `forward_client_headers: Vec<String>` opt-in for `x-claude-code-*` attribution headers (defaults to drop).
  - New `POST /v1/messages/count_tokens` proxy with an explicit 8-field body allowlist (first-target-only dispatch -- tokenizer correctness; no fallback walk).
  - `GET /v1/models` skips alias keys with `*` so the picker does not show unroutable globs.
  - New per-dialect `ErrorEnvelopeShape::{Anthropic, OpenAi}` so each ingress emits its native error shape.
  - New `Error::NotImplemented(provider, op)` maps to 501 (covers `count_tokens` on Bedrock today).

- **Server-side emulation of `context-management-2025-06-27`.** New per-provider `context_management: bool` on `[providers.X]` of kind `anthropic-api` (default false).
  - When true, routectl caches thinking blocks observed in upstream responses (bounded LRU at 1000 entries + 60-minute TTL, keyed by `(provider_id, tool_use_id)`), re-injects them on next-turn requests per the `clear_thinking_20251015` keep policy, and strips the beta header + body field on egress.
  - Unblocks Anthropic-API-shaped upstreams (e.g. DeepSeek `/anthropic`) that demand thinking echoback but do not implement the beta natively.
  - Composes with `history_reasoning`: strip on canonical messages first, inject on wire last.

- **Forward-compat for unknown Anthropic SSE block types.**
  - `Other(Value)` catchalls on three strict-tagged enums (`SseEvent`, `SseContentBlockStart`, `SseDelta`) plus `OpenBlockKind::Unknown` in the SSE state machine, so an unrecognized `content_block.type` no longer crashes the stream and walks the fallback chain.
  - A `#[serde(skip)] opaque_events` carrier on `ChatChunk` captures the upstream event bytes (value-preserving / semantically lossless for valid JSON); the matching Anthropic ingress re-emits them so strict clients (citation links, search-status UI) see the full upstream wire.
  - Bounded caps (256 KB / 10000 deltas per block) downgrade overflow silently with a WARN.
  - Bedrock-Invoke inherits the fix free; Bedrock-Converse streaming forward-compat is a separate task.

- **Per-provider `unsupported_features` filter + cross-alias resolution.** Operator-declared list on `ProviderRuntimePolicy`.
  - The router strips date suffixes from request `tools` to derive feature keys and removes providers whose declared list intersects request features BEFORE dispatch.
  - Eliminates wasted Bedrock round-trips for `web_search_*`-bearing requests; when the filter eliminates every provider the request fails 501 instead of cascading 400s.
  - Cross-alias resolution lets a chain entry reference another alias key (not just a model nickname); cycle detection runs at startup with a runtime depth cap of 8.

- **`probe_max_tokens` fast-fail on 429 / 529.** New `[retry] probe_max_tokens` knob (default 1, 0 disables).
  - claude-code's `max_tokens=1` availability probes used to walk the full fallback chain on rate-limit; a request with `max_tokens <= probe_max_tokens` now skips retry AND fallback on 429/529 and returns the status immediately.
  - Other errors keep the normal retry / fallback path; real requests above the threshold are unaffected.

- **`[log]` config block** for runtime knob fallbacks.
  - `trace_headers`, `trace_body_bytes`, `redact_prompts` gain a config-side default.
  - Resolution per knob: env wins when set, then `[log]` if set, then the hardcoded default.
  - `ROUTECTL_LOG` stays env-only because it must reach the tracing subscriber before config loads.

- **Opt-in 4-direction HTTP header tracing** (`ROUTECTL_TRACE_HEADERS=1`).
  - Emit headers on all four hops -- ingress in/out, egress in/out -- routed through the existing `log_safe` redaction so bearer JWTs and API keys are masked.
  - `scripts/capture_fixtures.sh` consumes the new format.
  - `docs/DEVELOPMENT.md` documents the toggle.

- **Stream-error terminator emission.** When an egress stream errored mid-stream, the ingress used to drop the SSE channel without a terminator; multi-turn SDKs interpret the silent disconnect as truncation and retry up to 5 times.
  - Both ingress dialects now emit a dialect-appropriate terminal event before closing (Anthropic `event: error`; OpenAI `data: {"error":{...}}` then `data: [DONE]`).
  - Errors forwarded to clients are sanitized via `sanitize_stream_error_for_client`: only `upstream stream error (HTTP <status>)` reaches the wire so per-tenant existence hints stay out of the client-visible payload.

### Changed

- **anthropic-api egress: STRIP unsigned `thinking` blocks instead of REJECTing.** The 400-on-missing-signature check broke cross-provider fallback (a turn handled by deepseek with its own signature format, then a turn that walks to Anthropic) and SDKs that drop `signature` on serialization.
  - `validate_replay_invariants` -> `normalize_replay_invariants`, returning `Cow<'a, [Message]>` so unmodified requests pay zero clone cost.
  - `history_reasoning = "preserve"` opts a model out of the strip (required for upstreams like DeepSeek `/anthropic` that demand unsigned thinking echoback).
  - One structured WARN fires per request when stripping occurs; block content is never logged.

- **Effort clamping is now uniform across all egresses** when `effort_levels` is non-empty.
  - Anthropic-API and Bedrock now consult the model's declared `effort_levels` and clamp the caller's effort to the nearest supported value (rounding toward the most capable when above the declared maximum, the least capable when below the minimum).
  - Empty list keeps the pass-through default for OpenRouter-style providers that perform their own effort translation.
  - Shared `clamp_effort_to_supported` helper in a new `routectl-providers::effort` module.

### Fixed

- **`thinking` stripped when `tool_choice` forces tool use.** Anthropic Messages and Bedrock Converse reject `thinking` paired with `tool_choice = {type:"any"|"tool"}`.
  - Strip `thinking` (not `tool_choice`) so caller intent to force a named tool is preserved; `auto`, `none`, absent are unaffected.

- **Stale `extra_headers` doc references renamed to `header_extras`.** Field was renamed in v0.6.0; a few snippets and the example config still referenced the legacy spelling.

- **`cache_control` system-block drop demoted from WARN to DEBUG.** OpenAI Responses has no equivalent surface; the strip is correct, the WARN level just trained operators to ignore real WARNs.

- **CI clippy gate tightened to `--all-features`.** The clippy steps in `ci.yml` and the local pre-commit hook were labeled "all features" but omitted the flag, so feature-gated test files never type-checked under the strict gate; tightening exposed pre-existing breakages in `live_matrix.rs`.

- **Doc-vs-code currency sweep.**
  - `[providers.X]` / `[models.X]` field placements, `adaptive_thinking` -> `supports_adaptive_thinking` rename references, the `stream_first_byte_timeout_ms` resolution table (now three tiers: model > provider > global), DeepSeek `context_management` example base URL with `/anthropic` suffix, the `header_extras["anthropic-beta"]` mechanism, the corrected `effort_levels` clamping description, the `anthropic.rs` -> `anthropic/{mod,parse,render,stream}` directory split, the corrected `MAX_LOG_BODY_EXCERPT` size in `LOGGING.md`.
  - Cross-doc links converted from bare backtick text to Markdown link syntax.

### Removed (BREAKING)

- **`thinking` and `effort` on `[models.X]`** -- replaced by three declarative capability fields:
  - `supports_adaptive_thinking` (bool, selects the adaptive vs legacy thinking wire shape), `effort_levels` (array, default `["low","medium","high"]`; drives clamping; empty = pass-through), `max_thinking_budget` (u32 tokens, default 0 = no cap).
  - Migration: declare capabilities explicitly on each `[models.X]` block per the vendor docs.
  - The `EffortLevel` enum and the `merge_reasoning_defaults_into` helper are deleted.

- **`adaptive_thinking` on `[providers.X]` of kind `anthropic-api`** -- the egress now reads `supports_adaptive_thinking` from `RoutectlInternal` per request.
  - `Bedrock-Invoke` and `Bedrock-Converse` keep the static `adaptive_thinking` field on `BedrockConfig` because Bedrock model IDs do not carry the same Anthropic-vs-Bedrock split.

- **`fallback_on_status` on `[retry]`** -- replaced by the two-field `retry_allowlist` / `retry_denylist` schema (mutually exclusive at config-load).
  - With both unset (the new default) every 4xx / 5xx falls back, which is strictly more permissive than the previous 15-code default and still covers Cloudflare extended 5xx codes (520-527, 530); operators wanting the narrow behavior set `retry_allowlist` explicitly.
  - No back-compat shim: configs using `fallback_on_status` need a one-line rename.

### Security

- **OAuth callback rate limiting** (two-window guard).
  - Per-source-port (30 hits / 10s) AND listener-wide (60 hits / 10s) on the loopback callback, so a co-resident process spraying ephemeral ports cannot drown a legitimate browser callback during the 120s login window.
  - Memory bounded (256-entry LRU + capped VecDeque). State-valid browser callbacks bypass the tracker entirely.

- **OAuth refresh hygiene.**
  - The OAuth HTTP client disables redirect-following so a 307/308 from the IdP cannot replay the refresh-token POST to a different host.
  - Refresh-flow errors and JSON parse errors omit upstream body excerpts (some IdPs reflect request fields in error envelopes; refresh bodies carry the long-lived refresh token).

- **Anthropic upstream-error body excerpts sanitized in WARN logs.** The 4xx / 5xx WARN logs in `complete()` and `stream()` used to emit `body_excerpt = %msg` directly from the upstream message; an upstream returning CRLF in `error.message` could forge log lines on text-format tracing subscribers.

- **`capture_fixtures.sh --out` rejects symlink components.** A dangling symlink under `captured/` could let fixture writes (which carry raw upstream headers) land outside the gitignored tree.
  - A per-component `[ -L ]` walk now runs before physical resolution. `--allow-unsafe-out` still bypasses the check for legitimate symlink-traversal use cases.

## [0.6.0] - 2026-05-20

The big v0.6 release: layered provider + model config, dispatch
hygiene fixes, openai-compat normalization, and a wave of dogfood
fixes from daily live use.

### Added

- **Layered provider + model config**.
  - `[providers.X]` carries transport-wide knobs (auth, base URL, runtime gates) and `[models.X]` carries per-model behavior (reasoning, dialect, quirks).
  - Two fields live on BOTH layers and merge at dispatch time -- `header_extras` and `payload_extras` -- with model winning on key collision and `anthropic-beta` comma-unioning across all sources.
  - The router's `apply_layered_overlays` helper runs the merge before calling `provider.complete()` / `provider.stream()` so the `Provider` trait surface stays stable across all four concrete providers (openai-compat, anthropic-api, bedrock, openai-responses).

- **`[models.X]` first-class TOML table**.
  - Required: `provider` (key in `[providers.X]`), `upstream` (wire model id).
  - Optional: `thinking` (bool or `"adaptive"`), `effort` (enum), `reasoning_dialect`, `history_reasoning`, `additional_request_fields`, `anthropic_beta`, `stream_first_byte_timeout_ms`, `header_extras`, `payload_extras`, `selectable`.

- **Suffix-glob alias keys** in the unified `[aliases]` table:
  - `"claude-opus-*" = "heavy"` matches any wire model starting with `claude-opus-`.
  - Lookup precedence: exact match > longest matching prefix > `default`.
  - Alias values are `String | Vec<String>` -- single string is a one-entry chain, list is a fallback chain.

- **`Router::new` precomputes** a `BTreeMap<String, Arc<ResolvedModel>>` from `[models]`, so dispatch is one O(1) lookup per hop. Unknown nicknames in alias chains fail at startup, not at first request.

- **Tracing dispatch events** carry `model = <nickname>` alongside `provider = <provider_name>` for per-model triage.

- **`anthropic-beta` HTTP header lifted into canonical `req.anthropic_beta`**.
  - The Anthropic TypeScript SDK translates the `betas: [...]` SDK option into an `anthropic-beta: a,b,c` HTTP header (not a body field); claude-code uses this surface for first-party betas (context-management, prompt-cache-1h, adaptive-thinking, ...).
  - routectl now lifts the header so the egress emits it in the upstream body (Anthropic accepts either surface).

- **`POST /v1/messages` openai-responses provider** (default-on `openai-responses` Cargo feature).
  - ChatGPT Codex endpoint via `chatgpt-oauth` bearer JWT.
  - Stream-only (`complete()` forces `stream:true` and drains SSE to `response.completed`).
  - Flat Responses-shape tools and `tool_choice`.
  - `instructions` field always serialized (the server 400s if absent).

- **Operator-owned Bedrock allowlists** -- `[bedrock] allowed_betas` and `[bedrock] allowed_body_fields` in TOML.
  - Filters the body's `anthropic_beta` array (Invoke and Converse) and any forward-compat body fields the Anthropic ingress sweeps in (`mcp_servers`, `diagnostics`, `context_hint`, `speed`, ...).
  - routectl ships no built-in default; AWS schema drift is operator-tracked.
  - Empty list (or omitted `[bedrock]` section) is pass-through for discovery -- bring up routectl, observe sent flags via `ROUTECTL_LOG=routectl_providers::bedrock=trace`, populate the lists.
  - `examples/bedrock.toml` ships the empirical 2026-05-12 baseline (16 betas + 16 body fields).

- **`history_reasoning` per-provider knob** on `[providers.X]` of type `openai-compat`.
  - Three values: `auto` (default; defer to the dialect's strip-vs-preserve default), `strip` (required for DeepSeek v3 and vLLM <= 0.6), `preserve` (required for DeepSeek v4+ and vLLM 0.7+ hosts that 400 on missing echo-back).
  - Per-dialect preserve impls: DeepSeek and vLLM render `reasoning_content` scalars; OpenRouter renders typed `reasoning_details[]`; OpenAI and Passthrough are no-ops.

- **Per-provider and per-model timeout overrides**: `request_timeout_ms` and `stream_first_byte_timeout_ms`.
  - Resolution priority: per-model > per-provider > global `[retry]`.
  - Eliminates alias-level repetition (e.g. NIM cold-start, Opus 4.7 high-effort).

- **CF extended 5xx range in default `fallback_on_status`**: `[408, 429, 500, 502, 503, 504, 520-527, 530]`.
  - Cloudflare-fronted upstreams (opencode.ai, openrouter.ai) surface upstream-origin failures via 520-527; without these in the default list, a single 520 would kill a request even when a sibling provider in the chain could have served it.

- **`ROUTECTL_TRACE_BODY_BYTES` env var** to override the 16 KB TRACE body cap at process start.
  - Set to 1 MB (`1048576`) for live-traffic fixture capture; real claude-code requests routinely exceed 16 KB.
  - Resolved cap is announced once at server boot.

- **`scripts/capture_fixtures.sh`** -- operator script that drains the TRACE log into per-request fixture directories under `crates/routectl-cli/tests/fixtures/captured/` (gitignored). Atomic writes via `.tmp.<id>.XXXXXX` rename pattern.

- **`docs/PROVIDER-QUIRKS.md`** -- operator-facing config guide.
  - Per-model rows for Anthropic Opus 4.7+ (adaptive thinking), DeepSeek v4 (echo-back), vLLM 0.7+, NIM (reasoning_effort gate + cold-start cushion), Anthropic / Bedrock / OpenRouter / OpenAI.
  - Cross-cutting timing notes, multi-host fallback chain examples, troubleshooting matrix.

- **`SECURITY.md`** -- vulnerability disclosure policy.

### Fixed

- **Per-model circuit breaker isolation**.
  - Two `[models.X]` rows pointing at the same `[providers.X]` now have independent breaker counters and RPM buckets.
  - State is keyed by `[models.X]` nickname, not by provider name -- a single flaky model no longer trips the breaker for every healthy sibling on the same transport.

- **Bedrock SSO probe deduplication** across models on one `[providers.X]`. The factory now caches resolved AWS credentials per provider name; building 5 Bedrock models on one provider hits the credential chain once instead of 5x.

- **Alias chain validation at startup**. `serve` and `routectl test` reject `[aliases]` chains pointing at unknown OR `selectable = false` `[models.X]` nicknames before the server binds, instead of silently returning `UnknownAlias` at first request time. Validator accumulates every offending alias/nickname pair into one consolidated error.

- **Per-model `header_extras` reaches the wire**. The merged value (provider + model + ingress) lands on `req.anthropic_beta` and the Anthropic egress emits one comma-unioned `anthropic-beta` HTTP header.

- **Anthropic legacy thinking budget clamp**.
  - Drop legacy `thinking: Enabled` when `req.max_tokens <= 1024` (Anthropic requires `max > budget`, floor `budget >= 1024`); enforce both the 1024 floor and the `max - 1` ceiling on every Enabled emission path.
  - Caught live on probe-sized requests (e.g. title-generation, topic-summary, "continue?" prompts with `max_tokens=64`) when the operator's per-model config carried `thinking = true effort = high`.
  - Bedrock Invoke + Converse share the helper transitively.

- **openai-compat: strip vendor envelope + lift usage sub-bags**.
  - Anthropic-shape ingress + openai-compat upstream was bleeding envelope fields (`object`, `system_fingerprint`, `cost`) and four DeepSeek/OpenAI usage sub-bags (`prompt_cache_hit_tokens`, `prompt_cache_miss_tokens`, `prompt_tokens_details`, `completion_tokens_details`) back to the Anthropic-shape response.
  - Now lifted to canonical `Usage.reasoning_tokens` and `Usage.cache_read_input_tokens` and stripped from the extras catchall.
  - Mirrored on the SSE path before serde (`UsageDelta` has no extras flatten).

- **`tool_choice` shape mismatch: OpenAI bare-string -> Anthropic tagged-enum**.
  - Anthropic's Messages API and Bedrock-Invoke reject `tool_choice: "auto"` with a 400.
  - The Anthropic-API egress now translates `"auto" | "none" | "required"` and the OpenAI `{"type":"function","function":{"name":"X"}}` object map to Anthropic-shape `{"type":...}`.
  - Anthropic-shape inputs pass through unchanged.

- **Top-level `system` leaks onto openai-compat wire**.
  - The OpenAI ingress lifts wire `role: "system"` into canonical `req.system` (Anthropic-shape top-level field).
  - The openai-compat egress now performs the inverse lower: prepends a synthetic `role: "system"` message and strips the top-level `system` key.
  - Strict hosts (NVIDIA NIM) used to 400 with `Validation: Unsupported parameter(s): system`.

- **OpenAI ingress: `reasoning_content` keys coalesced before schema deserialization**.
  - DeepSeek-shape `reasoning_content` was arriving unmerged on `messages[].reasoning_content`, missing the canonical `reasoning` lift on multi-turn echo-back.
  - Added pre-deserialization coalescer mirroring the response-side `merge_reasoning_keys`.

- **`prompt_tokens` translation: cache_creation/cache_read summed into Anthropic streaming usage**. The Anthropic SSE response now captures `message_start` input usage, sums `input_tokens + cache_creation_input_tokens + cache_read_input_tokens` into the closing `message_delta` `UsageDelta`, and exposes per-TTL cache breakdown via field-level merge.

- **Stop-sequence end-to-end**.
  - Preserve the matched stop sequence through canonical so the Anthropic ingress can emit `stop_reason: "stop_sequence"` + `stop_sequence: "<value>"` instead of collapsing to `end_turn`.
  - Previously broke claude-code structured-output flows (stop_sequence fences the output) by flagging `is_error: true` on the result envelope.
  - Bedrock Converse is a known follow-up -- AWS surfaces the matched sequence via `additionalModelResponseFields` only when the request opts in.

- **Router log clarity: chain-exhausted vs fallback hop**. Both `complete()` and `stream()` previously WARNed "fallback to next" on every fallbackable terminal error including the LAST chain entry. Now emits "chain exhausted; no fallback target available; request will fail" when no next target exists.

- **WARN at egress when canonical reasoning is silently stripped**. Operator visibility for the `history_reasoning = "auto"` + strip dialect case where the request actually carried reasoning.

- **Alias glob double-parse**. `Router::new` parsed each `*`-bearing alias key twice; pattern is now reused via let-binding.

- **`routectl test` help text and module docstring** referenced the removed `provider:model` direct-target form. Now references the alias key / model nickname inputs the v0.6.0 router accepts.

### Security

- **Log redaction at TRACE level**.
  - `ROUTECTL_LOG_REDACT_PROMPTS=1` walks every traced body and replaces known prompt-bearing fields (text blocks, system, instructions, tool_use input, function_call arguments, refusal blocks, image source data, image_url data URIs, Bedrock Converse `toolUse.input`) with `<redacted len=N>` placeholders while preserving structural fields (model, tools, sampling params, finish_reason, usage).
  - Read once on first traced body; one-shot startup log line reports the resolved value.

- **gitleaks workflow + `.gitleaks.toml`** -- secret scan on every PR + push + weekly full-history sweep. Inherits the default rule set; allowlists Cargo.lock, target/, and the captured/ fixture directory.

- **CI hygiene**: pinned every third-party action to a commit SHA with a version comment (floating tags can be retroactively moved by an attacker), added `permissions: contents: read` at the workflow level.

### Removed (BREAKING)

- **`enabled` on `[models.X]`** -- renamed to `selectable` to free the TOML key for the flattened `ReasoningDefaults::enabled` (reasoning on/off). Operators wanting per-model reasoning-off semantics now write `enabled = false`; `selectable = false` is the routing-disable knob.
- **`[aliases.X.retry]` per-alias retry overrides** -- removed when `[aliases]` collapsed into a flat wire-string -> nickname-or-chain map. Use the global `[retry]` table; per-error-class caps (`retry_on_429`, `retry_on_5xx`, `retry_on_network`) cover the knobs operators previously set per alias.
- **`type` field on `[providers.X]`** -- renamed to `kind` to disambiguate from the `type` Rust keyword.
- **`model_id` on `[providers.bedrock-X]`** -- moves to `[models.X].upstream`. Bedrock providers are no longer 1:1 with a model.
- **`thinking`, `enabled`, `adaptive_thinking` on `[providers.X]`** -- move to `[models.X]`. Per-provider was the wrong granularity; two models on one provider can now carry different reasoning floors.
- **`additional_model_request_fields` on `[providers.bedrock-X]`** -- renamed to `additional_request_fields` and moved to `[models.X]`.
- **`default_extras` on `[providers.X]`** -- moves to `[models.X]`.
- **`[ingress.X.aliases]` per-ingress alias maps** -- collapsed into the unified top-level `[aliases]` table.
- **`[aliases.X] chain = [...]` sub-tables** -- chains live as list values directly in `[aliases]`: `heavy = ["opus", "sonnet"]`.
- **top-level `default_model = "..."`** -- replaced by `default = "..."` inside `[aliases]`.
- **`[bedrock] anthropic_beta`** -- renamed to `[bedrock] allowed_betas`.

### Deferred

- **Per-model `default_extras` and `chat_template_kwargs`** deferred until the egress wiring lands; they will return as `[models.X]` fields in a future release. The provider-side fields (`OpenAiCompatConfig::default_extras`, `chat_template_kwargs` on the wire) are unaffected -- callers continue to send them per-request via `provider_extras`.
- **Bedrock Converse stop_sequence round-trip** -- AWS surfaces the matched sequence only when the request opts into `additionalModelResponseFieldPaths`. Tracked as a follow-up.
- **OAuth token hot-rotation.** routectl reads `ROUTECTL_ANTHROPIC` once at startup; a credentials.json rotation by claude-code requires a routectl restart. Manual snapshot + restart workflow today; inotify-based file-watch is staged for a future release.

### Migration

No automated migration tool; old configs hit raw serde errors at
startup. Hand-edit your TOML against the new shape -- see
`examples/config.toml` for a complete reference.

## [0.4.0] - 2026-05-10

### Added

- **Native AWS Bedrock provider** (`type = "bedrock"`). Speaks SigV4 directly to `bedrock-runtime.<region>.amazonaws.com`, with both `InvokeModel` (per-vendor body shape, default) and `Converse` (vendor-neutral envelope) request paths selectable via `api_shape = "invoke" | "converse"`.
  - Streaming responses are decoded from the AWS eventstream binary frame format and re-emitted as routectl `ChatChunk`s; in-stream Anthropic `error` events (`overloaded_error`, `rate_limit_error`, etc.) surface as `Error::Upstream` with mapped HTTP status codes rather than silently truncating.
  - Credentials resolve via four mutually exclusive `creds.kind` shapes:
    - `bearer-key` -- short-term Bedrock API key from the AWS console. Skips SigV4 entirely and sends `Authorization: Bearer <key>`.
    - `static` -- raw `access_key_ref` / `secret_key_ref` / optional `session_token_ref`, each via routectl `SecretRef` URIs.
    - `profile` -- a named profile in `~/.aws/credentials`, with SSO auto-refresh via `aws-config`.
    - `default-chain` -- standard AWS provider chain (env -> profile -> SSO -> web identity / IRSA -> EC2/ECS metadata).
  - Gated behind a `bedrock` Cargo feature (default on for the binary; library consumers can opt out with `--no-default-features` to skip the `aws-config` / `aws-sigv4` / `aws-smithy-eventstream` dep tree).
  - Per-provider `user_agent` override is supported and recommended for IAM policies that gate access via the `aws:UserAgent` condition key.
  - Per-provider `anthropic_beta` flags route into the request body's top-level `anthropic_beta` array (Invoke) or `additionalModelRequestFields.anthropic_beta` (Converse). `additional_model_request_fields` is a free-form merge point for vendor-specific knobs.
  - Note: For Anthropic models, both `Invoke` and `Converse` adapters are wired and live-tested. Converse for non-Anthropic Bedrock vendors (Mistral, Llama, Cohere) is staged for a later cut.

- **`POST /v1/messages` Anthropic ingress**, full tool-call round-trip, thinking blocks + signature preservation, typed SSE events (`message_start` / `content_block_*` / `message_delta` / `message_stop`), server-side model-id -> alias mapping (`[ingress.anthropic.aliases]`), and `x-routectl-alias` header override.
  - Two ingress dialects (OpenAI + Anthropic) feeding one canonical request shape; any client speaking either wire format routes through any backend.

- **Canonical internal shape** absorbs Anthropic features losslessly:
  - typed `ContentPart` (Text / Image / ImageUrl / Document / ToolUse / ToolResult / Thinking / RedactedThinking / Other), typed `SystemContent` (Text or Blocks), typed `ToolDef` (Custom / Other), top-level `cache_control` and `anthropic_beta`, and `Usage` cache stats.
  - Forward-compat catchalls (`ContentPart::Other`, `ToolDef::Other`, `ContentBlock::Other`) pass unknown Anthropic block types through verbatim on the all-Anthropic path.
  - `cache_control::validate` enforces the 4-breakpoint cap and 1h-before-5m TTL ordering at ingress.

- **Listener-side auth** via static config tokens (`[server.auth] tokens = [...]`) accepts both `x-api-key` and `Authorization: Bearer`. Inbound auth is fully decoupled from upstream credentials (no bridging, no token storage).

- **`strict_translation`** server flag.
  - Default `false` emits `tracing::warn!` on lossy seams (cache_control dropped on openai-compat egress, ContentPart::Other forward-compat blocks on egresses that don't carry them, Anthropic builtin tools dropped).
  - `[server] strict_translation = true` upgrades all of these to a 400 Bad Request, rejecting the request before it hits upstream.

- **Adaptive thinking** for Anthropic Opus 4.7+. Per-provider `adaptive_thinking = true` on `[providers.X]` of type `anthropic-api` or `bedrock` rewrites the request to the new `thinking: {type: "adaptive"}` + `output_config: {effort: "..."}` shape; budget is no longer caller-provided.

- **`extra_headers` and `user_agent` on `AnthropicApiConfig`** and `[providers.X]` of type `anthropic-api`. Mirrors the existing fields on `OpenAiCompatConfig`.
  - Use `extra_headers` to declare any `anthropic-beta` flags (e.g. `context-1m-2025-08-07`, `prompt-caching-2024-07-31`).
  - Use `user_agent` to override the outbound UA, useful for IAM-gated upstreams whose policy condition matches on `aws:UserAgent`.

- **Universal 4xx/5xx self-diagnosing logging**.
  - Outgoing request body at `tracing::trace!`, ingress body at `tracing::trace!`, full upstream error body at `tracing::debug!` (cap 4 KB) on every 4xx/5xx from any provider.
  - Request-id correlation across the chain so `grep request_id=<id>` shows ingress -> egress -> upstream response in one shot.

### Security

- **`extra_headers` cannot override auth-bearing headers**. TOML-supplied `extra_headers` entries that case-insensitively match `authorization`, `x-api-key`, or `host` are now ignored with a `tracing::warn!` instead of silently overwriting the provider's auth header. This applies to both `anthropic-api` and `bedrock` providers.
- **`BedrockCreds` redacts secret material in `Debug` output**. `secret_access_key`, `session_token`, and bearer keys never appear in `tracing` events, panic messages, or test failures. `access_key_id` is shown as a 4-character prefix so operators can identify the active key. `BedrockConfig` is safe by transitivity.
- **Eventstream parser caps single-frame size at 8 MB**. Defends against a malicious or compromised upstream advertising a giant `total_length` to drive the inbound buffer toward OOM. Real Bedrock chunks are KB-scale.

### Changed

- **BREAKING (config-level)**: `auth_kind = "oauth-bearer"` no longer auto-injects `anthropic-beta: oauth-2025-04-20`.
  - Beta flags are now declared explicitly in `extra_headers`, decoupling auth method from capability gates.
  - This unblocks API-key-auth users from setting `context-1m-*`, `prompt-caching-*`, and `extended-thinking-*` gates via the same channel.

  Migration -- if you used `auth_kind = "oauth-bearer"`, add to your
  TOML:
  ```toml
  [providers.<your-anthropic-provider>.extra_headers]
  "anthropic-beta" = "oauth-2025-04-20"
  ```
  Or, if you want the OAuth gate alongside other beta flags, comma-join:
  ```toml
  "anthropic-beta" = "oauth-2025-04-20,context-1m-2025-08-07"
  ```

### Fixed

- **Bedrock eventstream prelude drain on `Incomplete`**. Multi-chunk HTTP body responses (any long Opus stream) hit `InvalidUtf8String` mid-stream because the decoder consumed the 12-byte prelude into state but the caller didn't drain the cursor on `Incomplete`, so the next iteration reread the prelude bytes as headers. Fixed by draining `cursor.position()` from the buffer before breaking.

## [0.2.0] - 2026-05-06

### Added

- **Tier-1 retry/timeout policy**: per-error-class retry caps (`retry_on_429`, `retry_on_5xx`, `retry_on_network`), `request_timeout_ms` per attempt, `stream_first_byte_timeout_ms`, jitter on backoff.
- **Tier-2 routing gates**: per-provider `rpm_limit` (token-bucket), passive circuit breaker (`circuit_failures` + `circuit_cooldown_ms`), per-request `x-routectl-disable-fallbacks` header.
- **Anthropic OAuth bearer auth**: `auth_kind = "oauth-bearer"` on `[providers.X]` of type `anthropic-api`. Sends `Authorization: Bearer ...` plus `anthropic-beta: oauth-2025-04-20` against the same `/v1/messages` endpoint, for callers that prefer that wire format over an `x-api-key` header.
- **Opinionated alias groups** in `examples/config.toml`: `heavy`, `med`, `cheap`, `local`, `reasoning`.
- **`ModelProfile` registry** for per-model quirks (`drops_sampling_params`, `requires_reasoning_effort`, `supports_adaptive_thinking`, `uses_chat_template_kwargs`).
- **`Dialect` trait** and `openai_compat/dialects/` per-dialect modules routing request/response/SSE through static dispatch.
- **`file://` SecretRef variant**: TOCTOU-safe (open-once + fd-based `fstat`), refuses non-regular files, refuses world/group-readable files, requires absolute paths.

### Changed

- **Public config types are `#[non_exhaustive]`**: `ProviderEntry` (enum + every variant), `ProviderRuntimePolicy`, `AliasEntry`, `RetryPolicy`, `RouterOptions`.
  - External callers construct via the per-variant `ProviderEntry::*` factories and the chainable `with_runtime` / `with_extra_headers` / `with_default_extras` / `with_reasoning_dialect` / `with_base_url` / `with_anthropic_version` / `with_organization_id` / `with_auth_kind` setters.
  - Variant-specific setters panic on wrong-variant misuse rather than silently dropping values.
- **Stream cancellation** now distinguishes half-open probe drop (records failure, releases the in-flight slot, re-trips circuit) from steady-state drop (records success, healthy provider doesn't flap on client cancel).
- **Half-open circuit breaker is single-probe under concurrent load**. An explicit `half_open_in_flight` flag gates concurrent requests so exactly one probe runs at a time after cooldown.
- **Per-attempt gate accounting**: RPM and breaker now debit on every upstream call (was per-request), so retries can't bypass the per-provider rate limit.
- **`ProviderEntry::redact_secrets()` and `secret_uris()`** are exhaustive methods on the type. The CLI delegates rather than matching on the variants itself, so any future variant fails to compile until redaction is wired up (closes the silent-secret-leak footgun).

### Removed

- **OS keychain support** is gone. `SecretRef` is now `env://`, `file://`, or `literal:` only. Rationale: routectl is not a credential-discovery tool and we don't want the keychain-permission prompt giving the wrong impression.

### Fixed

- **`file://` TOCTOU window closed**. `file://` reads no longer have a TOCTOU window between permission check and read; the open file descriptor is `fstat`-ed and read from in one go.
- **`file://` rejects non-absolute paths**. `file://` URIs reject non-absolute paths instead of resolving them cwd-dependently.
- **Stream-mid-failure charges the circuit breaker**. Previously a provider that consistently emitted one byte and died would never be quarantined.
- **Drop-time mutex poisoning recovered in the stream breaker**. Recovered via `into_inner()` instead of panicking, so cancellation cleanup stays non-aborting.

## [0.1.0] - 2026-05-06

Initial release.

- **Single binary**, OpenAI-compatible HTTP server bound to `127.0.0.1` by default.
- **Two provider classes**: `openai-compat` (6 reasoning dialects: `openai`, `deepseek`, `vllm`, `raw-think-tag`, `openrouter`, `passthrough`) and `anthropic-api` (api-key auth; `thinking` blocks with `signature` preserved across multi-turn tool use).
- **Reasoning normalization** to OpenRouter-shape `reasoning_details[]` with provider-tagged `format`.
- **Streaming SSE both directions**, including stateful `<think>` tag handling for tags split across chunk boundaries.
- **Fallback chain** on 408/429/5xx/timeout (no fallback once first chunk has streamed).
- **Per-provider retry** with exponential backoff.
- **TOML config** in `~/.config/routectl/config.toml`.

[Unreleased]: https://github.com/meepolabs/routectl/compare/v0.9.0...HEAD
[0.9.0]: https://github.com/meepolabs/routectl/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/meepolabs/routectl/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/meepolabs/routectl/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/meepolabs/routectl/compare/v0.4.0...v0.6.0
[0.4.0]: https://github.com/meepolabs/routectl/compare/v0.2.0...v0.4.0
[0.2.0]: https://github.com/meepolabs/routectl/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/meepolabs/routectl/releases/tag/v0.1.0
