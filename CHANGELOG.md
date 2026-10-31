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

- **Circuit-breaker state changes log at INFO.** A target's breaker now logs one INFO line when a failure debit opens it (`event = breaker_opened`), when an upstream reset hint parks it (`event = breaker_parked`, with `cooldown_ms`), and when a success closes an open or half-open circuit (`event = breaker_closed`), on every surface including a committed stream's tail. Each line carries only the event and the target's `state_key`; failures that leave the circuit unchanged stay at DEBUG. See [docs/LOGGING.md](docs/LOGGING.md#circuit-breaker-transition-info-lines).

- **`[pools.X]` accepts the five runtime tuning keys.** `circuit_failures`, `circuit_cooldown_ms`, `rpm_limit`, `request_timeout_ms` and `stream_first_byte_timeout_ms` may now be set on a pool block, with the same names and meanings as on `[providers.X]`. Each applies to every member of the pool and to a pooled model's own runtime slot; a member's own value wins key by key, and a provider that is not a member never reads it. Before this, a pooled model's own breaker and RPM slot always ran on the defaults. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#credential-pools-multiple-accounts-per-provider).

- **A request dropped before it finished is recorded as `cancelled`; an observed client hangup stays `client_disconnect` (`/status/query` schema 2).** A usage row is written `client_disconnect` only where routectl sees the client leave: a stream whose client channel closes or whose send fails, or a complete body dropped before it is handed off. A request whose handler is dropped before any of those exits (a shutdown, a task abort, or a client that hangs up before the response is built, which axum reports the same way) now writes `cancelled` instead of `client_disconnect`, so some client cancels land as `cancelled`. The `cancelled` row names the last target the request reached and carries `missing_terminals` 0; complete, stream and token-count requests all follow this rule. Both outcomes stay out of `errors` on every reader. Because `errors` on `/status/query` now excludes rows that can actually occur, its `schema_version` moves from 1 to 2; the dashboard expects 2. See [docs/LOGGING.md](docs/LOGGING.md#usage-accounting-log-shapes).

- **Token-count requests are recorded in the usage ledger.** Each `POST /v1/messages/count_tokens` request whose body parses now writes one `requests` row marked `operation = count_tokens`, carrying the response's `x-request-id`, the identity of the target that served the count (or the last one tried on failure), and its outcome (`ok`, `upstream_error` or `gate_blocked`). The token columns and `session_id` stay NULL: unlike an inference row, a count row never records the client's session id. Inference readers (usage aggregates, cost, `/status/usage` totals and groups) skip these rows, and the per-window observation totals include them. A body that fails to parse still writes no row. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#usage-accounting-usage).

- **Streams cut before their terminal event are counted.** An anthropic-api, openai-responses, Bedrock Invoke, Bedrock Converse or openai-compat stream that closes before its terminal event now counts once on the request's `missing_terminals` usage column and on a new process-wide `rc_missing_terminal_total` on the router metrics snapshot. The error the client receives is unchanged. An openai-compat stream cut mid tool-call keeps its own error and is not counted; a stream the client stops reading is not counted.

- **`/status/usage` and `/status/doctor` report each window's observation totals, ledger health and whether the window is complete (usage schema 4, doctor schema 23).** The usage panel gains `cancelled_total` on its totals and on every group, so `requests == ok + errors + client_disconnect_total + cancelled_total` holds at both levels, and an `observation` block for the requested window: the summed `wire_losses`, `translation_drops`, `local_refusals`, `field_repairs` and `missing_terminals`, the `ledger_gaps` rows recorded inside the window (`gap_rows`, `gap_dropped`), and `complete`. A window reads incomplete when a gap row falls inside it or when this process has dropped a row on a closed writer channel or failed a write, since neither loss leaves a row behind to mark a window. The doctor panel gains the same block for today and `ledger_health`: the live `dropped_full`, `dropped_closed` and `write_errors` counters, reported apart. Its `observation` is `null` when the usage database cannot be read; the rest of the panel stands. Rows refused on a closed channel by a process that has since exited are not recorded anywhere, so they cannot mark a window incomplete. `/status/query` metric sets carry the same `cancelled_total` and the same narrower `errors`. The doctor panel's version is now defined from the doctor report's own constant rather than a second copy. Every new field is additive. See [docs/LOGGING.md](docs/LOGGING.md#usage-accounting-log-shapes).

- **Usage database schema v18 adds per-request observation columns and a ledger-gap table.** Every `requests` row gains `operation` (`inference` or `count_tokens`), `probe` (0 or 1), `harness` (`claude_code`, `codex_cli` or `other`; the user-agent itself is never stored) and five per-request counts: `wire_losses`, `translation_drops`, `local_refusals`, `field_repairs` and `missing_terminals`. A new `ledger_gaps(ts_ms, cause, dropped)` table records request rows lost because the writer's queue was full: the writer appends one `queue_full` row carrying the count since its previous gap row, so a window that lost rows stays marked incomplete after a restart. Rows refused because the writer had already stopped are counted separately (a new `dropped_closed` usage counter beside `dropped_full`) and are not written to the table. The migration runs on first start in one transaction: it only appends nullable columns and creates the table, so no row is rewritten and a failure leaves the file at v17. Rows written before v18 hold NULL in the new columns, read as `inference`, not a probe, `other` and 0. `routectl usage downgrade --to 17` hands the file back to a v17 binary: it checks the `requests` columns and `ledger_gaps` are exactly the v17 shape plus the v18 additions, then re-stamps the version and keeps every column, row and table. `--to 16` runs that step and then the v17 step in the same transaction, so a refusal at either step leaves the file at v18.

- **`routectl doctor`, `/status/health` and the dashboard show the value a learned verdict applies (doctor schema 22, health schema 8).** Every capability matrix cell and every `learned_negatives[]` row gains `value`: `shape:<v>` for a learned rewrite (an effort level or thinking shape), `ceiling:<n>` for a learned output limit, `null` for a verdict that writes none (a dropped field, a catalog capability) and for a matrix cell no learned entry backs. The value is the one recorded for the exact verdict the row shows, never one recorded for a later verdict on the same key. The doctor text grid ends each learned cell in ` value=<token>` (`value=-` when there is none), and the dashboard's Health chips and Doctor matrix cells name it in their tooltips. The key shown is the one to copy into `force_supported`, which targets the lane's provider entry (the part before `#`). Every new field is additive. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#capabilityoverrides-is-the-durable-decision-surface).

- **The fidelity snapshot and `/status/doctor` count how chained repairs end and how many repairs each request spends (doctor schema 19).** The INFO fidelity snapshot gains `rc_field_chain_steps_committed_total` (chained field-repair steps proven by a success or by a refusal of a different field), `rc_field_chain_steps_dropped_total` (steps still pending when the attempt ended unproven: the same field refused again, a fallback to another target, an unrelated error), `rc_field_repair_budget_exhausted_total` with a per-lane `rc_field_repair_budget_exhausted_by_lane` map (rejections a repair was proposed for that the per-request allowance could not pay; a refusal no repair answered is not counted), and `rc_field_repairs_per_request`, a six-bucket array of requests by repairs drawn, recorded once per request on every walk. A committed step is not a learned verdict: a proven repair that teaches nothing counts as committed and persists nothing, and no counter here claims the verdict was durably written. The `/status/doctor` panel gains `budget_exhaustions_by_lane` and `repairs_per_request` beside `lossy_repairs_by_lane`; the doctor report and panel schema move to 19, and every new field is additive. See [docs/LOGGING.md](docs/LOGGING.md).

- **A refused `temperature` is dropped and learned per lane.** When Bedrock Converse answers `This model doesn't support the temperature field. Remove temperature and try again.`, or an openai-compat upstream answers `Unsupported parameter: 'temperature' is not supported with this model.` (or names it in `error.param`), routectl retries once on the same target without `temperature`. On success the lane learns the drop and later requests to it go out without the field, with no 400 first. A retry the upstream refuses again learns nothing. See [docs/PROVIDER-QUIRKS.md](docs/PROVIDER-QUIRKS.md#bedrock-any-region).

- **openai-compat refusals of `top_p`, `presence_penalty` and `frequency_penalty` are dropped and learned per lane** the same way, one retry per field. On the `openai` and `deepseek` dialects, ids the chat-lane rule matches (the OpenAI families, or any id containing `reasoner`) drop the whole sampling set before send (see the chat-lane rule under Changed); for an id the rule does not match, OpenAI names one parameter per 400, so a request carrying several refused parameters spends one retry on each. `logprobs` and `top_logprobs` are not learned. See [docs/PROVIDER-QUIRKS.md](docs/PROVIDER-QUIRKS.md#openai--o-series--gpt-5).

- **A `max_tokens` above the model's limit is clamped to the limit the refusal names, and the limit is learned per lane.** On Bedrock Converse (`The maximum tokens you requested exceeds the model limit of N. ...`) and on anthropic-api and Bedrock Invoke (`max_tokens: SENT > N, which is the maximum allowed number of output tokens for <model>`), routectl retries once on the same target with `max_tokens` lowered to N; N is used only when it is a canonical nonzero number below the value sent, so a limit at or above it makes no retry. When the retry succeeds the lane learns N as its output ceiling (persisted as `ceiling:N` and restored on restart), and pre-flight lowers any later limit above N before the request is sent (`field_preflight_ceiling`). A learned ceiling never raises a limit: a request at or below it ships unchanged (`within_ceiling`). It recovers by decay only, never by canary: it lapses after `[capability] decay_hours` and the next refusal learns the limit again, and a refusal naming a different limit starts a new verdict. Lowering a client value is recorded as `field_ceiling_repair`, `repaired_lossy`, and counted in `rc_field_repair_lossy_total`. Lowering the `max_tokens` routectl filled in for a client that sent none (anthropic-api and Invoke only) is lossless and recorded as `field_ceiling_fill_repair`; a refused fill used to stop at its declaration and now reaches repair. A failed retry learns nothing, a fallback target still receives the original value, and a refusal after a stream has delivered content is not repaired. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#output-ceiling-max_tokens-fill).

- **A refused reasoning effort level is repaired and learned per lane.** When an anthropic-api, Bedrock Invoke or Bedrock Converse upstream refuses the request's `output_config.effort` with a 400 that lists the levels it takes (`This model does not support effort level 'xhigh'. Supported levels: high, low, medium.`, or a field-validation list), routectl retries once on the same target at the nearest listed level below the one refused, or the nearest one above it when none is lower. Only the effort value changes: the `thinking` object ships byte for byte as sent, and no thinking or effort is created for a request that sent none. On success the lane learns the refused level and the level that worked (for example `field:output_config.effort.xhigh` as `shape:high`), and later requests at that level go out at the learned one with no refusal first. On an Anthropic-family lane the level is learned only when the refused request sent `adaptive` or `enabled` thinking: beside `disabled` or `between_tools` thinking, or with the thinking object left out, the refusal may depend on that thinking shape, so the request is repaired and nothing is learned. On openai-responses (chatgpt-oauth and api-key lanes, not Bedrock Mantle) a 400 with `error.param` `reasoning.effort` and `Supported values are: ...` (for example codex `Unsupported value: 'minimal' is not supported with the '<model>' model.`, code `unsupported_value`) is repaired the same way at `reasoning.effort`, streamed or not, as long as it arrives before any content; that lane's body has no thinking object for the refusal to depend on, so the level is learned (`field:output_config.effort.minimal` as `shape:low`). The same text carried by a failed stream event is not repaired. Field repair on openai-responses covers the effort level alone: a refusal of any other field there is not repaired or learned. A refusal of the retried level is repaired again the same way, never at a level the request already sent, within the request's repair budget of 5; on success every refused level is learned as the level that went out (`max` then `xhigh` refused, `high` served, learns both `field:output_config.effort.max` and `field:output_config.effort.xhigh` as `shape:high`), each only where its own refusal may be learned. A retried level whose learning another request in flight already holds, or on which a learned level already acts, is still retried; that step is recorded as repaired and learns nothing (the first refused level of a request is not, as before). A level routectl wrote before sending from a learned verdict (for example `xhigh` sent as `high`) that the upstream refuses with a list is repaired the same way within the request and learned under its own level (`field:output_config.effort.high` as `shape:medium`); the verdict that wrote it is left as it is, and later requests at the first level follow both learned levels before sending (`xhigh` goes out at `medium` with one attempt). A level the upstream accepts on a completion (served as sent, served after a repair, or read past to refuse another field) clears that level's own learned verdict on the lane in the same settlement, so a lane that flapped stops keeping `xhigh` as `high` once a request is served at `xhigh`; a token count clears no level's verdict. Pre-flight follows each learned level while its verdict acts, never writing a level the request already sent, and records one pre-flight action per level it passed through. A refusal of the effort field itself (`Extra inputs are not permitted`), one that lists no level routectl knows, one whose list names the level sent, and one listing only levels already sent are not repaired; `none` is never repaired and never chosen. An effort routectl supplies beside adaptive thinking, whether from a learned thinking repair applied before sending or while repairing a refused thinking type in the same request, is repaired the same way. Each such repair is recorded as `repaired_lossy` and counted lossy on its lane, under `field_rewrite_repair` when the level served is below the one refused and under `field_rewrite_higher_repair` when it is above it (the served request may cost more than the one the client sent); its `envelope_field_repaired` WARN also names `refused_level`, `chosen_level` and `direction` (`lower` or `higher`), all effort rank tokens. No other repair's WARN carries these fields. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#removed-model-keys).

- **The Bedrock Mantle Responses lane repairs a refused `max` effort level.** When a Mantle Responses model refuses `reasoning.effort` `max` with `Unsupported value: 'max' is not supported with the '<model>' model. Supported values are: ...`, routectl retries once at the nearest listed level below it (`xhigh` on `openai.gpt-5.4`) and learns that level for the lane, as it already did for a refused `minimal`. See [docs/PROVIDER-QUIRKS.md](docs/PROVIDER-QUIRKS.md#bedrock-any-region).

- **A refused adaptive thinking type is repaired and learned on Bedrock Converse.** When a non-Anthropic Converse model (GLM, Qwen, Magistral) refuses a client's `thinking: {type: adaptive}` with `Invalid 'thinking': Invalid 'type': unknown variant `adaptive`, expected `disabled` or `enabled``, routectl retries once with the legacy `enabled` shape (`display` kept, `budget_tokens` composed from the request's effort and `max_tokens`) and learns it for that lane, so later requests go out `enabled` with no rejection. A request whose `max_tokens` leaves no budget window is not rewritten. Models that accept adaptive are untouched, and the repair is recorded as `repaired_lossy`. A thinking type the upstream accepts on a completion clears that type's own learned verdict on the lane, so a lane that learned `enabled` as `adaptive` and later `adaptive` as `enabled` keeps only the later lesson instead of rewriting every `enabled` request into a refusal. See [docs/PROVIDER-QUIRKS.md](docs/PROVIDER-QUIRKS.md#anthropic-opus-47-and-any-future-adaptive-thinking-models).

- **A refused Anthropic thinking type is repaired and learned per lane.** When an anthropic-api or Bedrock upstream answers `thinking.type.enabled` or `thinking.type.disabled` with a 400 that names the shape it takes instead, routectl rewrites the request's `thinking` to that shape, retries once on the same target, and on success learns the shape for that lane, so later requests go out repaired with no rejection. A refused `enabled` becomes `adaptive` (`display` kept, `budget_tokens` removed). A refused `disabled` becomes whichever shape the rejection names: `adaptive` (Opus 5.5) or a bare `between_tools` (Bedrock Sonnet 5.5, whose differently worded rejection the parser now reads). An `adaptive` retry carries the client's effort, or `"low"` when the request has no positive effort; that effort is never learned. A retry the upstream refuses again learns nothing, a fallback target still receives the client's original `thinking`, and every such repair is recorded as `repaired_lossy`, since it serves a shape the client did not send. No `[models.X]` key is needed for adaptive-only models. See [docs/PROVIDER-QUIRKS.md](docs/PROVIDER-QUIRKS.md#anthropic-opus-47-and-any-future-adaptive-thinking-models).

- **`scripts/wire_watch/run.py` runs the wire-watch capture matrix under bounds.** The matrix comes from the committed `scripts/wire_watch/cells.tsv` (one row per driver, lane and connection mode, expanded over every case and, on Bedrock lanes, over the primary model and a small candidate sweep), so a skipped cell is recorded as `NOT_RUN(reason)` before anything launches and never reads as clean. Each cell runs in its own capture container with bounded concurrency (`--concurrency`), a per-cell timeout, a run deadline, and per-cell and aggregate size caps; a stop is SIGTERM, a grace period, SIGKILL, `docker rm -f`, then a bounded reap. A lock on `<report-dir>/run.lock` allows one run per report directory, and a directory that already holds a run is refused, so a paid cell is never re-run. `run.json` is written atomically before the first launch and records each cell's state, its token counts, and a run-level `run_stop` (`run-size`, `leftover-containers`, `sweep-failed`) that marks the whole run INCOMPLETE. On every exit path (normal end, a terminating signal, or an exception) one sweep removes any container of the run that is still present and lists again to confirm. A cell's key is read at launch and passed only through that child's environment, never on a command line. Files a container wrote are read by descriptor without following symlinks, with capped sizes and a strict JSON decoder; malformed evidence reads as incomplete. See `scripts/wire_watch/README.md`.

- **`scripts/wire_watch/report.py` turns a run into a dated report.** It writes `report-<UTC stamp>/` holding `report.md` and `findings.jsonl`. Every cell gets exactly one outcome decided from its evidence, never from the runner's exit code: a cell is `PASS` only when the daemon counted at least one clean wire check and its loss counters agree with the findings shown, and a run with a `run_stop` or any incomplete, timed-out, capped or interrupted cell publishes as INCOMPLETE with every reason listed. `report.md` carries a harness x ingress coverage table (a pairing is covered only by a `PASS` or `FINDING` cell) and per-cell tokens and leg coverage; `findings.jsonl` has one ASCII JSON object per lane, path class and leg with capped example paths. A cell whose loss counters run past the findings the daemon kept (its finding ring holds the newest 128) is a `FINDING`, with one row per short lane and path class at leg `unknown`, no example path, and the missing count, so the totals still equal the daemon counters. Upstream refusals are classified by `refusal_rules.tsv`, each row citing its source: on Bedrock a 401 is `FAILED(credential)` and a 403, a 404, or the recorded invalid-model 400 is `NOT_RUN(model-access)`, and with `--key-dir` a short-term Bedrock key that had expired when the cell ended is `FAILED(credential)`. Container-written text reaches `report.md` only through one Markdown escaper, and the report is published only after `scripts/scrub-fixture.sh --check` passes over it.

- **Driver runs save per-cell evidence.** Every `scripts/capture_driver.sh` run whose daemon became healthy leaves `<out>/.cell-evidence/` on every later exit path: the daemon's `/status/health` and `/status/usage` snapshots, the fidelity lines, the traced ingress dialects, upstream error rows with a capped excerpt (replaced by a fixed marker when it looks like or contains a credential the run holds), and `client_result.json` with the driver's and rig's exit codes. Teardown is bounded on every path; a signal exits 129, 130 or 143, and a daemon still alive after SIGKILL fails the run with exit 9. The fixture scrubber also detects Bedrock long-term keys and newer admin and service-account key shapes. Layout: docs/DEVELOPMENT.md "Per-cell evidence".

- **pi and hermes drivers for all three ingress dialects.** `scripts/drivers/` gains `pi-anthropic.sh`, `pi-chat.sh`, `pi-responses.sh`, `hermes-anthropic.sh`, `hermes-chat.sh` and `hermes-responses.sh`. Each points its client at the daemon through a config written inside the run workspace that names the local key's variable rather than its value, clears the inherited settings the client would otherwise route by, and refuses a second turn unless the first left a session to continue. The capture image now pins four clients (claude, codex, pi, hermes); its tag is derived from all four pins; pi is pinned by the committed lock in `scripts/container/pi/`, claude and codex by `build.sh --claude-version` / `--codex-version`, and hermes by `--hermes-version` with `--hermes-commit`; and `build.sh --dry-run` prints the resolved pins and tags without docker.

- **Bedrock and Ollama capture lanes.** New driver configs for `bedrock-invoke`, `bedrock-converse` (us-west-2) and `bedrock-mantle` (us-east-1), each with one model entry per candidate, plus an `openai-compat.ollama.toml` variant against Ollama Cloud. Every committed lane config now sets `[wire_check] sample_one_in = 1`, so a driven run checks every attempt. `scripts/wire_watch/models.tsv` lists the Bedrock candidates with tier, thinking flag and source doc, and `scripts/drivers/config/lane_configs.test.sh`, run in CI, keeps the two in step and pins each Bedrock lane to its key's region.

- **Streamed responses now have their documents body-traced.** While the body trace is on, a stream whose upstream and client documents both assemble emits them on the same `upstream success body` / `egress response body` TRACE lines a non-stream response uses: on a fidelity lane when the wire check sampled the attempt, and on a translation lane whether sampled or not (there the upstream stream is folded for the trace alone and nothing is compared). A capture fixture of such a stream therefore carries both response body files, each the assembled JSON document rather than the SSE frames, so `meta.stream` is now the only record of whether a run streamed. Limitation: streams from Bedrock Converse and Gemini egress have no stream assembler and still trace no upstream or client response body; neither does any stream cut short on either side.

- **`routectl config check` warns when an override cannot withhold a floor beta flag.** An `unsupported = ["beta:<flag>"]` override on an `anthropic-api` provider that names a flag the egress re-adds after the override is applied now draws one advisory per flag naming every reason it still ships: an `anthropic-beta` pin in the provider's `header_extras`, or in the `header_extras` of any model the override cell reaches (a provider-wide override checks every model on that provider, including through a pool, unless the model's own override cell names the flag); the structured-outputs beta whenever the request carries `output_config.format` (not on a forwarded-credential provider); and, on an `oauth-bearer` provider on `api.anthropic.com` using its own credential only, the OAuth gate, the effort beta whenever the request carries `output_config.effort`, and the Claude Code floor (not under `cloak.mode = "never"`). The context-management beta is not reported on a provider with `context_management = true`, which strips it, and the redaction beta's warning names the `thinking.display` exception. Flags compare case-insensitively; `force_supported` entries are not checked. Advisory only: the exit code is unchanged, and `routectl doctor` shows it as a config WARN finding.

- **A Bedrock rejection of a client tool type or of structured outputs is learned per lane and routed away.** When a Bedrock lane answers `tool type '<type>' is not supported for this model` for any tool type, in any dated version (`web_search_20250305` is learned as `web_search`), or `output_config.format: Extra inputs are not permitted`, on InvokeModel or inside the Converse wrapper, routectl records the capability for that lane and routes later requests that carry it to a target that supports it. The two other wordings Bedrock uses to refuse a tool type are learned the same way: `tools.<n>: Input tag '<type>' found using 'type' does not match any of the expected tags: ...` and `'<model>' does not support tool types: <type>. Did you mean one of ...`. The same input-tag rejection on a message content block (a refused tool change, not a tool) and a refusal naming two types at once teach nothing. Rejections of other request fields still teach nothing.

- **A request for a capability every lane has refused outright now fails fast.** On a complete or streamed request, when every target in the chain has an acting, self-identifying learned rejection of a requested capability that routectl cannot strip there (or that `[capability] essential` lists), routectl answers 400 `invalid_request_error` with the message ``no provider configured for this route supports the requested capability `<key>` `` and contacts no upstream, instead of walking the chain into the same rejection on every lane. The refusal logs one `event=hard_exclusion` WARN naming the refusing lanes, counts on `rc_capability_hard_excluded_total`, and records the usage row with `error_class = capability_unsupported`; the client body names only the capability. Token counting keeps dispatching to the soft tail, and an inferred suspicion never refuses a request. A lane the operator pins with a beta floor never counts, and a lapsed negative whose re-probe slot is free sends that probe instead of failing. To lift a refusal, set `[capability.overrides.<spec>] force_supported` for the capability on one lane, or remove a wrong negative with `routectl capability purge`. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md) and the `hard_exclusion` entry in [docs/LOGGING.md](docs/LOGGING.md).

- **Streamed replies now provide positive capability evidence.** A streamed reply is folded into the few facts the capability detectors read, and once the whole stream, end-of-stream frames included, has reached the client, the detectors read it through the configuration live at that moment. Only verified observations are admitted from a stream, so a gap in the fold can never record a suspected absence. A disconnect, a render failure, an upstream error, or a cancelled task reads nothing; so does a reply with more than one choice or one past the fold's 1 MiB cap, and a reload mid-stream that turns the capability subsystem off. The new `rc_stream_evidence_total{result,exit}` counter rides a DEBUG `stream evidence snapshot` line: `settled` counts a folded reply the detectors read, `abandoned` one that was not, each by how the stream ended. See [docs/LOGGING.md](docs/LOGGING.md).

- **`routectl doctor` shows what dispatch does for every capability cell (schema 18).** Each matrix cell resolves through the same precedence dispatch applies, so `layer` and `source` gain `pin` (an operator wire floor pins the flag, verdict `forced_supported`) and `representability` (a lane that cannot carry the flag, verdict `unrepresentable`, action `withhold`), and every cell and `nickname_actions` entry gains `overrode_layer`, the next tier below the deciding one that had an opinion. A pinned beta flag with a learned negative now shows the send dispatch takes rather than `withhold`, and a Bedrock Converse lane that refuses the beta bag key shows a client flag as withheld. The served `/status/doctor` panel gains `lossy_repairs_by_lane`. The doctor report and `/status/doctor` panel schema move to 18; every new field is additive.

- **`/status/health` learned rows say what routing does with them (schema 7).** Each `learned_negatives[]` row gains `action`, `layer` and `overrode_layer`, resolved the same way as the doctor matrix so the two always agree, plus `age_ms` since first seen and `expires_at_ms`, the moment the negative lapses into a re-probe under the runtime rule (including the re-check backstop), `null` for a verified positive. The health panel schema moves to 7; every new field is additive.

- **An anthropic-api refusal of a tool type is learned per lane.** When the Messages API answers `'<model>' does not support tool types: <type>. Did you mean one of ...?` (for example a `computer_20250124` tool sent to a model without computer use), routectl records the capability (`computer_use`) for that lane and routes later requests that carry it elsewhere, exactly as it does for the same Bedrock wording. A refusal naming two types at once, or a tool type the request did not carry, teaches nothing.

- **The usage database can be handed back to the previous release.** Schema v17 appends two nullable columns to the capability ledger (the provider kind and the vocabulary version a row was written under), and `routectl usage downgrade --to 16` re-stamps a v17 file as v16 so a v16 binary starts against it (a v18 file first steps through v17). The command is offline only: it refuses while the daemon or any other reader has the database open, and refuses any file that is not exactly the additive v17 shape. Warm rebuild now maps each ledger row to the current token vocabulary before decoding it; a row in a vocabulary this build does not know is skipped whole and counted in the new `skipped_vocab` field of the rebuild log line.

- **A rejection that names client beta flags is repaired and learned per lane, on anthropic-api and Bedrock alike.** When the upstream answers a 400 naming the `anthropic-beta` flags it rejects (Bedrock Invoke and Converse send it as a `ValidationException`, anthropic-api lanes, Mantle's Anthropic lane included, as an `invalid_request_error`), the router's learning engine re-sends the request once on the same target without exactly those flags, for streaming, non-streaming, and token-count calls. The repair draws on the same per-request budget of five repairs as field repair. Once an attempt served without those flags succeeds, the lane learns them as `beta:<flag>` rejections (see Changed), so later requests withhold them without another 400; a token-count repair learns nothing. A flag is repaired only when the client sent it: operator-pinned flags, flags the request's own body requires (the structured-outputs or `thinking.display` `"updates"` beta), and flags on a forwarded credential are never withheld or remembered. The live wire check reports a beta rejection the repair answered as `repaired_lossy` when the repaired attempt succeeds, and as unrepaired otherwise. The Bedrock provider no longer retries on its own, so its `bedrock beta retry failed; nothing reported` WARN is gone.

- **The envelope-field pre-flight surface is now readable at INFO** -- a
  status or doctor poll emits one structured line, when its content has
  changed since the last one or an hour has passed, carrying what an
  operator needs to explain, audit, or distrust every resident wire-shape
  verdict: the capability key and target it applies to, its transform
  class and whether that class rewrites a cache prefix, where the evidence
  came from, how many confirmations back it against how many its class
  requires, what is currently blocking it, and where its re-verification
  cadence stands. Two new process counters ride the same line: requests a
  pre-flight rewrite modified before dispatch, and upstream rejections
  whose envelope no field path could be read from -- the second is the
  am-I-flying-blind figure, and what carries its signal is its ratio
  against the first.

  The line also carries the probe scheduler's own state -- lanes activated,
  queue depth, work in flight or backing off, the most recent settled
  outcome, and how long until the next retry -- because "why is this
  verdict not moving" is usually answered there rather than by the verdict
  itself.

  Rows are reported for every resident verdict rather than only the acting
  ones, because the question an operator actually asks is why a repair is
  NOT firing, and a surface listing only what acts answers that with
  silence. Every row is drawn from one coherent read, so two counts on the
  line can differ only by a documented filter and never by timing. Rows are
  sanitized and capped at a fixed ceiling, with the real total and the
  omitted count both reported -- a truncated line says how much it is not
  showing rather than presenting a subset as complete.

  Nothing on the line is a request value, a response body, a credential, or
  upstream text. Reading it mutates nothing: no lane activates, no cadence
  advances, no re-verification slot is claimed, so polling a dashboard
  cannot re-verify verdicts on its refresh interval or consume the slots
  real traffic needs. At most one line per request, so the combined
  `/status` endpoint does not double-report, and an unchanged snapshot
  stays silent between hourly repeats so a polling dashboard does not
  grow the log.

- **A newly confirmed wire-shape repair now takes effect without a
  restart** -- a learned envelope-field verdict becomes eligible to rewrite
  requests ahead of a rejection only once the ledger row describing its
  confirmation is durably written, and that acknowledgment now happens in
  the running process. Previously the only thing that could raise a
  verdict's acknowledged confirmation count was a boot replaying the
  ledger, so a repair confirmed at noon started saving round trips at the
  next restart. Now the next request on that lane is already rewritten.

  The advancement is the daemon's own work rather than the request's, which
  matters in two ways a caller can feel. A client that hangs up mid-write no
  longer loses it -- previously the wait lived in the request, so cancelling
  the request cancelled the advancement while the row still landed, leaving a
  verdict dormant until a restart for exactly the requests whose clients went
  away. And no response waits on the database for it: the request hands the
  work off and returns. At shutdown the daemon waits briefly for any in-flight
  advancement and then lets it go, which is safe because the row is already
  written and the next start reads it back.

  The condition is unchanged, which is the point: eligibility advances on
  the acknowledgment and on nothing else. A write that failed, was refused
  by a saturated writer, was dropped because capture is disabled, or
  described a verdict version an operator purge has since superseded
  advances nothing at all -- the verdict stays resident and the lane keeps
  being served by repair-after-rejection, which costs one extra round trip
  and is always correct. What that rules out is a verdict rewriting traffic
  today on evidence no later boot can find.

- **Field rejections are now parsed and repaired on anthropic-api, Bedrock, and openai-compat lanes.** When one of these upstreams answers a 400 or 422 by naming a request field -- the Anthropic `<path>: ...` location prefix and `"a.b" is not supported ... Use "x.y"` forms, the same two inside Bedrock's flat message (Converse wrapper stripped), or OpenAI `error.param` / `Unsupported parameter: '<x>' is not supported with this model.` -- routectl resolves the path against its closed table of repairable fields, repairs the request on the same target, and retries once. A repair drops the field, rewrites it to the single accepted value the rejection names, or lowers a token limit to the ceiling the rejection names (see the Added entries on sampling drops and output ceilings). On a rewrite row, a rejection naming no accepted value, or several, repairs nothing; on a drop row, a rejection refusing the value (as the shipped `thinking.display` row's captured rejection does) still drops the field for that attempt, lossily, and learns nothing. Only a refused field, a rewrite to the upstream's own named value, or a lowered limit the upstream named is learned; a refused value on a drop row is repaired for that request only, recorded as `field_lossy_drop_repair` and counted in the new `rc_field_repair_lossy_total`, because a learned drop would strip every valid value from later requests. The `envelope_field_repaired` WARN is now emitted once per repair rather than once per request (at most five per request). Paths outside the table, Gemini, and openai-responses repair nothing. The table holds the Anthropic `thinking.display` drop, whose captured rejection refuses a value -- so that repair serves the request and learns nothing -- the two `thinking.type` rewrite rows (see the Added entry on refused thinking types), four sampling drop rows and the `max_tokens` ceiling row; further fields plug into the same loop. Each grounded wording is pinned to a captured envelope under `crates/routectl-router/tests/fixtures/rejections/`, and `scripts/capture-rejection.sh` turns a logged rejection into such a fixture (see docs/DEVELOPMENT.md "Capturing an upstream rejection fixture").

- **A learned rewrite verdict now survives restart** (no shipped table row rewrites yet; this is the persistence a rewrite row relies on). Warm rebuild restores a rewrite verdict's value from the capability ledger, decoded strictly against the field's closed accepted set; a malformed value replays as a plain negative and is counted in the new `degraded_value` field of the rebuild log line. Pre-flight then rewrites the field to the restored value before dispatch, logged as `action = field_preflight_rewrite`, and two new fail-open reasons explain a rewrite that did not happen: `no_stored_value` and `value_already_sent`. A field verdict confirms only when its value matches; an observation with a changed value starts a new verdict, so a stale value never acts.

- **Probe re-checks clear a learned field negative only once the clear is durable.** A re-check that finds a field accepted now removes the learned negative only after its `cleared` row is durably written to the ledger. A refused or failed write, or no answer within 10 seconds, leaves the negative acting and the probe retryable; a negative replaced while the probe was out is not cleared. The field-verdict snapshot line gains `rc_probe_clear_persistence_failures_total` (accepted re-checks whose clear could not be made durable) and `rc_probe_oldest_due_ms` (how long the longest-waiting probe job has been due, `-1` when none is). A successful clear logs `probe_clear` at INFO. A clear also supersedes a same-version negative still in flight to the ledger, so a restart cannot bring it back.

- **Pre-flight rewrites stand down while the capability ledger cannot be
  trusted** -- every way a wrong wire-shape verdict is taken back out of
  service is itself a ledger write: the clear a failed re-verification
  performs, an operator purge, a later confirmation. So when durable
  capability writes cannot be guaranteed, rewriting requests ahead of a
  rejection is a bet that cannot be called off -- the in-memory retraction
  lasts until the process restarts, and the next boot restores exactly the
  verdict that was proven wrong. The router now suspends those rewrites
  instead, and says so with its own status reason rather than reporting a
  shortfall of evidence an operator would go looking for in vain.

  Repair-after-rejection is untouched and keeps serving every request: it
  acts on a rejection it has in hand, so it needs no durable record to be
  correct. The cost is one extra round trip on an affected lane, against an
  unbounded one avoided. The suspension is deliberately sticky for the life
  of the process -- a daemon that dropped one capability write serves the
  rest of its life on repair-after-rejection, and recovery is a restart.

  A failure counts no matter when it happened, boot included. The check is
  an absolute one rather than a comparison against whatever had already
  failed by the time the daemon finished starting, because the replay
  boundary the daemon writes at startup is itself one of these writes: a
  boundary that never landed leaves the ledger disagreeing with the
  registry about which history is current, which is the strongest reason of
  all to keep verdicts off the rewrite path. Every way a capability write
  can be lost now counts toward this -- a storage failure, a write the
  writer was too far behind to accept, and a write submitted after the
  writer was gone -- and the last two are reported apart, since a saturated
  daemon and a shutting-down one need different responses. Ordinary
  accounting rows falling behind are deliberately not counted: that says
  nothing about whether a capability write would land.

- **Every pre-flight decision now reports the evidence that authorized
  it** -- the diagnostic for a request routectl rewrote before dispatch
  carries the detection phase and evidence source of the verdict it acted
  on, how many acknowledged confirmations back it, and where that verdict's
  re-verification stood at the moment the rewrite was authorized. The
  values are taken from the exact snapshot that permitted the action rather
  than re-read afterwards, so a line can never describe a state the rewrite
  was not actually authorized under.

  A decision that rewrote nothing reports no evidence, explicitly, because
  it had none -- the one exception is a re-verification, which did hold
  authorization and spent it on testing the verdict rather than acting on
  it. Both the per-decision detail and the one-per-request summary carry
  the same fields, so reading a summary and then scanning the detail for
  the target it named lands on matching values. Nothing added is a request
  value, a response body, or upstream text.

- **`catalog set --create` publishes a narrow cell for an uncovered
  upstream** -- correcting a catalog fact (a context window, a rate, a
  capability prior) for a model the baked table and the overlay both miss
  previously left two options: a provider catch-all selector, which applies
  the value to every sibling model on that provider kind, or hand-editing
  `catalog_overlay.json`. The flag writes ONE cell for ONE model instead.
  It inverts `set`'s admission rule rather than relaxing it: a selector
  that IS already known is refused (plain `set` edits that one), so a typo
  cannot fork an existing cell into a competing second one, and the created
  selector must name a provider kind a `[providers.*]` block can declare
  plus a single model -- an
  exact id or one trailing-asterisk prefix, never a `*` catch-all.
  Everything past admission is the code plain `set` already runs: same
  field vocabulary, same value validation, same `source: user` stamping.

- **Subscription usage carries an API-equivalent value** -- rows served by a
  managed-OAuth seat have no per-token dollar cost, so they used to report a
  bare `n/a (subscription)`. They now also report what that usage would have
  cost at API rates: `n/a (sub ~$12.34)` in the `routectl usage --detail`
  cost cell, `$5.00 (+sub ~$12.34)` in a group that mixes both, and a new
  `equivalent_cost_usd` field beside `cost_usd` on every `/status/query`
  metric set (groups, totals, and each series bucket; `schema_version` stays
  1, the addition is purely additive). The rates are the same ones a priced
  row resolves -- a `[registry]` row wins whole, the baked catalog fills in
  behind it -- and the figure comes out of the same cost function, so an
  equivalent and a real price can never disagree on reasoning structure or
  cache basis.

  The two numbers are never added together and never share a field: the
  dollar figure stays real spend under the unchanged four-token
  `cost_status` vocabulary, and the `~$` figure is current replacement
  value -- what the usage would cost today, not spend and not realized
  savings. Recomputing a past window after rates change may legitimately
  move it, the same query-time doctrine `cost_usd` already follows.

  **Complete or absent.** The equivalent appears only when every dimension
  the row actually used resolved a positive rate; otherwise the cell reads
  exactly as it did before rather than showing a partial figure, since a
  base-tokens-only number understates a heavily cached workload by most of
  its value. The baked catalog deliberately supplies no cache rates, so
  cache-active subscription traffic reads as absent until a `[registry]`
  row supplies complete rates -- cache read plus both cache-write buckets,
  alongside input and output. A `[registry] pricing` row on an `oauth://`
  provider was previously inert config (subscription detection
  short-circuited ahead of every rate lookup) and is now live as the
  equivalence basis -- an operator's declared API price is what their
  subscription usage is valued at.

  `routectl doctor`'s **Cost pricing** section answers the question that
  rule creates. A subscription row still leads with "billed by
  subscription", and now adds what the equivalent is based on: complete via
  your `[registry]` table, or absent with the rateless dimensions NAMED
  (`cache read, cache write 5m, cache write 1h` for a model running on
  catalog base rates), or absent because neither layer prices the upstream
  at all. Doctor report schema 10 -> 11.

- **Output ceilings fill themselves from the catalog** -- a model now takes
  the ceiling its resolved catalog row confirms, so a common Claude upstream
  gets its real ceiling instead of the Anthropic-shape egresses' built-in
  64000 baseline. Precedence is catalog > baseline, and a selector the
  catalog cannot pin to one figure (a glob spanning models with different ceilings) fills
  nothing rather than guessing. The catalog overlay corrects a ceiling the
  baked table has wrong, the same channel that corrects rates and context
  windows. One startup line names the models whose ceiling came from the
  catalog (count exact, list bounded), and `routectl doctor` grows a **Model
  knobs** section naming every configured model's ceiling source -- the
  catalog or the baseline (doctor report schema 9 -> 10); if
  the catalog overlay cannot be loaded that section reports unavailable
  rather than a ceiling the overlay may supersede.

  **Spend note.** Reasoning tokens bill as output, and three legacy-path
  (`enabled`) thinking budgets routectl translates track the resolved
  ceiling because they pass through Anthropic's `budget < max_tokens` window
  clamp: `reasoning.effort = "max"` (ceiling minus one), `reasoning.enabled = true` with no
  effort or budget (half the ceiling), and a caller-supplied
  `reasoning.max_tokens` ABOVE the previous ceiling (it was being capped at
  `ceiling - 1` and the higher ceiling admits more of the ask). An operator
  on Opus 4.6-4.8 will therefore see
  those budgets rise after upgrading (64000 -> 128000 ceiling). Every
  standard effort level (`minimal` through `xhigh`) resolves through the
  exact effort-to-budget table and is unaffected, as is any caller whose own
  `reasoning.max_tokens` already fit under 64000. Set the catalog figure
  (`routectl catalog set <selector> max_output_tokens=64000`) to keep the
  previous figures.

- **Operators can veto a capability strip** -- `[capability]` grows an
  `essential` list of capability keys that must never be stripped in
  place. When a target only fails on a listed key, routectl routes the
  request to a target that supports it instead of removing the capability
  and proceeding, so an operator who considers a feature load-bearing no
  longer has to accept routectl's baked strip-vs-route judgment for it.
  The list is global (droppability is operator intent that holds for every
  target, unlike `[capability.overrides]`, where support is a per-target
  fact) and hot-reloadable. It only ever TIGHTENS: it can turn a strip
  into a route-away, never the reverse -- making a capability droppable
  needs a strip recipe in code, and a strip without one would dispatch a
  request guaranteed to fail. It is not a support override either: it
  changes only the response to a learned negative, never masking it
  (`force_supported`) or hard-dropping the target (`unsupported`), and
  flipping the list preserves the learned negatives it affects rather than
  re-probing them. Accepts any known capability key except
  `reasoning_replay`, whose strip is managed by the target lane's replay
  path rather than this key-only consult; an unknown key or that carve-out
  fails config load naming the key, on both `routectl config check` and
  the runtime load. An empty or absent list leaves dispatch behavior
  byte-identical. See [CONFIGURATION.md](docs/CONFIGURATION.md).

- **Cost accounting works out of the box** -- when `[registry]` carries no
  pricing for an upstream, routectl now prices its usage from the baked
  catalog's own rates instead of reporting it unpriced. An operator
  `[registry]` row still always wins, taken WHOLE and verbatim so a
  deliberately omitted dimension is never backfilled. The canonical
  operator-facing price unit is USD per million tokens; the catalog's
  internal per-token rates convert at exactly one boundary, and the
  per-token fields are no longer part of the router crate's public surface,
  so no consumer can join the two unit systems raw. The fill covers the
  input and output dimensions only -- cache rates stay `[registry]`-only,
  since deriving them from a row's cache multipliers would fabricate
  figures for cells whose multipliers are placeholders. A row the catalog
  cannot price unambiguously stays unpriced rather than reading as free.
  History is keyed by the provider kind PERSISTED with each row, so
  re-kinding a provider cannot reprice the past, and a row with no recorded
  kind fails closed to unpriced. Subscription providers are unaffected:
  the subscription check still runs before any rate lookup, and the
  `priced` / `unpriced` / `subscription` / `partial` cost vocabulary is
  unchanged. `routectl doctor` grows a **Cost pricing** section naming each
  configured model's rate source and resolved per-million rates (doctor
  report schema 8 -> 9); if the catalog overlay cannot be loaded the section
  reports unavailable rather than a rate the overlay may supersede.
  `routectl probe --capabilities` estimates off the same resolution, so a
  lane the operator never priced now shows a dollar estimate instead of
  `unpriced` -- and a managed-subscription lane still shows none.
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


- **Anthropic `thinking.display` is honored end to end.** A caller sending `thinking: {display: "omitted"}` on the Anthropic Messages ingress now gets an upstream request that carries it: the ingress translates `display` onto the canonical `reasoning.exclude`, and the `anthropic-api` egress stamps it back onto the wire `thinking` object as `display: "omitted"` (`exclude: true`) or `display: "summarized"` (`exclude: false`), on both the legacy `enabled` and the Opus 4.7+ `adaptive` shape. Under `"omitted"` Anthropic returns a thinking block with empty text and a full signature -- routectl keeps that block through response normalization, streaming, and multi-turn replay, so an omitted-display turn stays replayable and billing is unchanged. A non-string `display` is a local 400. An ABSENT `display` stays absent on the wire: the upstream default is model-dependent, so routectl never materializes a value the caller did not send. The `bedrock-converse` egress forwards `display` verbatim as well (see Changed); the Gemini and OpenRouter egresses continue to consume `reasoning.exclude` as before.

- **Live wire check: routectl now counts what it loses between the client and the upstream.** On sampled attempts it compares the request body the client sent with the body it sent upstream, per lane (one ingress dialect feeding one egress kind, e.g. `anthropic->anthropic-api`), and reports every difference no routectl transform declared. On a same-dialect (fidelity) lane a sampled attempt is checked in the response direction too: the upstream's answer against what the client received, streamed or not, with a cut or abandoned stream recorded as `incomplete` rather than clean. The check only reads; it never changes what is sent or fails a request.
  - **Counters.** `rc_wire_loss_total{lane,path_class}`, `rc_wire_check_total{lane,outcome}` (`clean`, `loss`, `budget`, `incomplete`; a check that ran out of budget or had nothing complete to compare is never counted clean) and `rc_request_refused_local_total{lane,reason}` for requests routectl refused locally without asking the upstream, all on the router's metrics snapshot line.
  - **Status and logs.** The `/status/health` panel gains a `wire_check` section: loss and check counts per lane, recent losses named by path and leg (`request` or `response`), unrepaired upstream rejections, and local refusals, filtered to the configured window. A loss or an unrepaired rejection logs one WARN per lane, path and kind. Paths are sanitized JSON pointers: object keys the client chooses (under `metadata`, tool schemas and arguments, header maps) render as `*`, and no body value is ever logged or shown.
  - **Rejections.** An upstream rejection is always checked, whatever the sampler decided, and the field it names is classified as unchanged from the client, changed by routectl, added by routectl, or not localized; it is then joined with field repair's outcome, so a rejected field under a client-keyed container (e.g. `metadata.user_id`) reports `repaired` when its repair succeeds.
  - **Intended rewrites are not loss.** Each routectl transform that changes a body on purpose declares the edit where it makes it -- among them system-message lifts, structured-output schema `additionalProperties` injection, the `max_tokens` default, the Bedrock `anthropic_version`, renderer envelope stamps and Responses status rebuilds -- so only undeclared differences count.
  - **Config `[wire_check]`.** `first_n` (default `8`: successful attempts checked per lane and client version after boot), `sample_one_in` (default `100`: one in this many checked afterwards) and `window_secs` (default `86400`: how long a counter stays current on the status surface). There is no on/off key; the block is optional and hot-reloadable. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#live-wire-check-wire_check).
  - **Benchmark.** The hot-path bench gains `wire_check__*` cases pricing an admitted attempt's extra work, alongside `wire_unadmitted__*` for a declined one.

- **A rejection of a routectl edit is answered by undoing the edit.** When an upstream 400/422 on a same-dialect (fidelity) lane names a path that one of routectl's own declared edits put in the body -- an operator `payload_extras` value added inside an object the client sent, or a prompt-cache marker routectl placed -- routectl undoes that one edit (restoring the client's value, or removing what it added) and re-sends the request once, on completions and streaming, drawing from the same per-request repair allowance as other repairs. It applies only when the value sent is the edit's and undoing it leaves exactly what the client sent; two conflicting edits or any other edit site leave the rejection unrepaired, and a target reverts at most once. Nothing is learned or persisted: no field verdict and no capability negative comes from such a rejection. Every rejection laid to routectl logs one deduped `routectl_edit_rejected` WARN with the lane, the masked path and the edit site (`conflict` for several edits, `undeclared` for a path routectl added that no declaration explains, which is then handled as before). Translation lanes neither revert nor warn. The wire check records the rejection `repaired` when the re-sent request succeeds and `unrepaired` otherwise. A `/v1/messages/count_tokens` request rejected because of an operator payload extra is retried once without it and teaches nothing; a token count leaves the wire check's counters unchanged.

### Changed

- **The stream first-content timeout defaults to 5 minutes, down from 10.** A stream that has not delivered its first content within 5 minutes of an attempt now fails that attempt and moves to the next target when no model, provider, pool or `[retry]` sets `stream_first_byte_timeout_ms`. A config that sets the key keeps its value. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#per-provider-runtime-gates).

- **Completions and token counts time out after 30 minutes per attempt by default.** When no provider, pool or `[retry]` sets `request_timeout_ms`, each complete and count-tokens attempt now ends after 30 minutes with `request timed out after 1800000ms` and the request moves to the next target; before, such an attempt had no deadline. A stream that has committed content is not cut by this timeout. A config that sets the key keeps its value. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#per-provider-runtime-gates).

- **A hot reload applies breaker and RPM changes to running gates.** Editing `circuit_failures`, `circuit_cooldown_ms` or `rpm_limit` on a provider or pool now takes effect on reload for every model and seat that keeps its runtime state; before, a carried gate kept the values it was built with until a restart. The consecutive-failure count and an open or half-open breaker carry across the reload, so the next failure is judged against the new threshold. An open window follows a changed cooldown unless an upstream reset opened it. A limited bucket keeps its level (capped at the new limit), a newly limited one starts full, and a removed limit is unlimited at once. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#per-provider-runtime-gates).

- **Committed-stream errors debit only health classes.** An error frame that arrives after a stream's first content now settles a provider's circuit breaker by the same class rule as a failure before content, after the provider's `class_overrides` remap: a rate-limit, server error, timeout, network error or overload counts toward `circuit_failures`, while an auth, bad-request, content-policy, context-window or unsupported-feature frame records nothing. A non-probe rate-limit frame whose `Retry-After` exceeds the in-loop cap now parks the provider for that reset clamped to `max_honored_retry_after_ms`, as it does before content. Probe streams are unchanged: a probe's 429/529 still leaves the breaker untouched.

- **A complete (non-streaming) response records `latency_ms` when its body has been handed off for sending to the client, not when the upstream answer is ready.** The usage row of a complete response is now written once its body is handed off, so its latency on `/status/usage`, in the usage ledger and on the `request finished` line includes the time spent sending the body. Time to first byte still marks the response being ready. Streamed responses are unchanged: their row is written at the end of the stream, as before.

- **One INFO `request finished` line per request; per-attempt detail moves to DEBUG.** Every inference and token-count request now writes exactly one content-free INFO line when it finishes, carrying `request_id`, `operation`, `outcome`, `http_status`, `harness`, `probe`, the served `alias` / `model` / `upstream` / `provider` / `seat`, attempt and fallback counts, latency, time to first byte, input / output / cache-read tokens and the five per-request counts. It is written even when the usage writer's queue refuses the row. Routing names and the request id are escaped, capped at 256 characters and written as quoted values, since on a forwarded-credential or default-alias route they carry the client's requested model id; a model id spelling `key=value` pairs cannot add a key to the line. The per-request `cache=READ/PROMPT (PCT%)` breadcrumb, the `route_away` tail demotion that leaves a supported alternative, the `feature_unsupported` event, the token-count seat-terminal event, the anthropic-api cloak decision, the Gemini content-filter finish lines and the opaque SSE block stop summary now log at DEBUG. WARN and ERROR lines and the request access line are unchanged. See [docs/LOGGING.md](docs/LOGGING.md#request-finished-info).

- **The fidelity status row reports `unsupported_lane` for a verdict whose field the lane never rewrites before sending.** A learned field verdict on a lane whose row view omits that field (an effort level on a direct openai-compat lane, any field but an effort level on a direct openai-responses lane, any field outside a Bedrock Mantle sub-lane's own list) used to report an evidence token such as `not_eligible` or `below_quorum`, suggesting more confirmations would let pre-flight act on it. Pre-flight never acts on such a row, and `blocked_reason` now says so. Verdicts on anthropic-api and direct Bedrock lanes, and verdicts inside a lane's view, report as before. See [docs/LOGGING.md](docs/LOGGING.md).

- **The usage ledger's `harness` column now names the client.** Each request is classified once from its `User-Agent` when it arrives: a Claude Code CLI value (`claude-cli/<version>`) records `claude_code`, a Codex CLI value (`codex_cli_rs/<version>`) records `codex_cli`, and a missing, non-UTF-8 or any other value records `other`. The user-agent itself is still never stored or logged.

- **The usage database no longer writes the `would_trim_raw_marks` column.** New `requests` rows store NULL there; the column stays in the schema so older binaries keep writing to the file, and values already recorded remain readable. Reclaiming the space older rows use is a separate, optional step. Library callers lose `UsageRecord::would_trim_raw_marks` and `DispatchMeta::would_trim_raw_marks`.

- **The learned token-estimate correction keys on the upstream model id, not the nickname.** A `[calibration]` lane is now `(provider kind, upstream)`: two nicknames sending one upstream on one provider kind share their evidence, a pool's seats share their model's lane, and renaming a nickname keeps it. A request served on a forwarded credential no longer contributes evidence, live or to the usage ledger; a forwarded target reads the lane of the model the client requested, which an own-credential target serving that upstream feeds. The startup warm's INFO line field `rejected_unknown_nickname` is renamed `rejected_unknown_lane`, and `CalibrationRebuildSummary` / `CalibrationLedgerRow` rename the same fields. Existing evidence is rebuilt from the usage ledger under the new key. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#learned-token-estimate-correction-calibration).

- **A credential park now covers the whole credential account, not one model.** When an upstream rejects a credential with a typed reason, every model on that account is refused (`Error::Credential` with the reason and deadline) and never dialed, while models on other accounts keep serving. The account is the OAuth seat (`provider#label`) for an `oauth://` credential and the `[providers]` entry for any other; two entries reading one secret are two accounts, and a forwarded credential is never parked. After the hour lapses each model re-tests through its own half-open slot, and the first success lifts the park for every model on the account. A model added by a reload onto a parked account starts parked. `/status`, `/status/doctor` and sticky seat placement report the park on every target of the account, one row each, and the park WARN now names `account` instead of `state_key`. See [docs/LOGGING.md](docs/LOGGING.md).

- **A reload lifts only the credential parks it may have fixed.** A credentials reload (the file watcher or SIGHUP) lifts the park of each OAuth seat whose stored record the reload added, removed or changed, and leaves every other park in place, including every non-OAuth account. An accepted config reload lifts every non-OAuth account (one `[providers]` entry each), whose credential the rebuild re-resolves, and no OAuth seat; a model the reload adds on a parked OAuth seat starts refused. A rejected reload of either kind lifts nothing. A rejection admitted before a reload re-parks its account only when that reload did not lift it. See [docs/LOGGING.md](docs/LOGGING.md).

- **Streams and token counts log the same dispatch lines as completions.** Completions, streams and `/v1/messages/count_tokens` now walk the fallback chain through one shared path, so a log filter keyed on the old per-surface wording needs updating: `stream fallback to next` is now `fallback to next`; `stream chain exhausted; no fallback target available; request will fail` is now `chain exhausted; no fallback target available; request will fail`; `stream gate blocked` is now `gate blocked` (`count_tokens gate blocked` is unchanged); and the DEBUG auth-refresh lines `stream 401 pre-content; refreshing auth and retrying once` and `count_tokens 401; refreshing auth and retrying once` are both now `upstream 401; refreshing auth and retrying once`. The stream `fallback to next` line gains `state_key`, streams and token counts now also log the DEBUG `retry decision` line, and a token count whose attempt timed out logs `fallback to next` or `chain exhausted ...`. The walk's lines log on target `routectl_router::router::dispatch_loop` (the gate refusal on `routectl_router::router::credential_park`, `reasoning_replay_degraded` on `routectl_router::router::replay_repair`), so a filter naming `routectl_router::router::dispatch` or `routectl_router::router::count_tokens` for them needs the new target. See [docs/LOGGING.md](docs/LOGGING.md).

- **A token count honours `request_timeout_ms` and moves to the next target on expiry.** A `/v1/messages/count_tokens` attempt that does not answer within the composed `request_timeout_ms` ends with the same `request timed out after <ms>ms` error a completion gives, debits the seat's breaker like a 5xx, and the count moves to the next count-capable target. Every other upstream health error on a count still ends the request. When no target counts, the last timeout is returned ahead of a parked credential and the 501.

- **A client that cancels a stream after content no longer counts as a success on the provider's breaker.** Dropping a stream before it ends used to record a success, resetting the provider's failure count; it now records nothing. A stream that runs to its end still records a success, and an error mid-stream still settles as before.

- **A system or developer message holding a non-text part stays in place.** On the Chat Completions and Responses ingresses, a system or developer message with an image, a document or an unknown part is no longer lifted into the request's system prompt as text only; the whole message stays in its position. openai-compat and openai-responses forward it; anthropic-api (lift policy), Bedrock Invoke, Bedrock Converse and Gemini carry its text and count the parts they cannot represent once per request as the `system_part_unrepresentable` translation drop. See [docs/LOGGING.md](docs/LOGGING.md).

- **The default log filter is `info,aws_config=warn`.** With `ROUTECTL_LOG` unset, the AWS credential-provider crate now logs at WARN and above only. A `ROUTECTL_LOG` you set replaces the default whole; see [docs/LOGGING.md](docs/LOGGING.md) for the targets to cap in a custom filter.

- **Behavior change: one rule decides which ids get the modern OpenAI chat shape, and every `gpt-` id now matches.** On an `openai-compat` model with the `openai` (default) or `deepseek` reasoning dialect, an id that, lowercased with at most one leading `openai.` removed, starts with `gpt-`, `o1`, `o3` or `o4`, or whose lowercased id contains `reasoner`, has `temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `logprobs` and `top_logprobs` removed before send; on the `openai` dialect its `max_tokens` also goes out as `max_completion_tokens`. This replaces the per-model table, which kept sampling params for `gpt-4o`, `gpt-5-chat`, `gpt-5.4`, `gpt-5.5` and `gpt-oss-*`: those ids now lose them on chat lanes. The `vllm`, `raw-think-tag`, `openrouter` and `passthrough` dialects are unchanged, so a self-hosted `gpt-oss-*` that should keep its sampling params sets `reasoning_dialect = "vllm"`. For gpt-5.6 and later, prefer a Responses lane. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#openai-chat-lane-rule-reasoning_dialect).

- **CI scans secrets and dependencies with the shared organization security floor.** A new `security.yml` workflow runs the `meepolabs/meepolabs-conventions` security-floor action (a gitleaks secret scan, and osv-scanner over every lockfile), pinned by commit to v0.1.1. It replaces the `gitleaks.yml` workflow and the `osv-scan` job in `ci.yml`, so every repository in the organization runs one scan with one ruleset. The CI scan ignores the repo `.gitleaks.toml`, which now serves only the local pre-commit hook: gitleaks' default rules plus a rule for `GOCSPX-` Google OAuth client secrets (the default rules miss them) and one exact-value allowlist entry for the public Antigravity installed-app client secret. Its path allowlist (`Cargo.lock`, `target/`, the captured-fixture directory) is gone, so the hook now scans `Cargo.lock` and `target/`; only gitleaks' built-in default path skips (images, fonts, binary documents, the Go, JS, and Python lockfiles, `node_modules/` and vendored trees) still apply locally, and the CI scan removes those too. A false positive is fixed by rewriting the value. `cargo audit` and `cargo deny` still run in `ci.yml`.

- **Breaking on macOS and Windows: routectl keeps its files in one folder on every OS.** The folder is `$XDG_CONFIG_HOME/routectl` when `XDG_CONFIG_HOME` is an absolute path, else `.config/routectl` under the home folder: `HOME` when that is an absolute path, else on Windows `USERPROFILE` (`%USERPROFILE%\.config\routectl`). An empty or relative value counts as unset. The CLI used to read `config.toml` from `~/Library/Application Support/routectl` on macOS and `%APPDATA%\routectl` on Windows; it now reads it from that one folder, and routectl moves nothing. The old folders are chosen by variable, not by OS: `%APPDATA%\routectl` when `APPDATA` is an absolute path, then `~/Library/Application Support/routectl` when `HOME` is, so a shell that exports both gets both. Unless `--config` or `ROUTECTL_CONFIG` names the config, and as long as at least one old folder is a candidate, every command first checks for the new `config.toml`; when it is missing, each old folder is checked in that order, and the first that holds a `config.toml` or cannot be inspected stops the command before it runs, naming both paths and saying routectl moved nothing. Move the old folder's files to the new folder, or pass `--config` (or set `ROUTECTL_CONFIG`). When neither `XDG_CONFIG_HOME` nor `HOME` (or, on Windows, `USERPROFILE`) is an absolute path, the CLI's default config path falls back to `.config/routectl` under the working directory instead of the account's home from the system user database. `routectl doctor` (and the dashboard's doctor panel) warns once for each old folder that still holds files, naming up to eight of them, since routectl no longer reads anything there, and warns on its own when an old folder cannot be inspected; a folder that holds the config in use is not listed (doctor schema 20).

- **The ChatGPT cookie jar honors `XDG_CONFIG_HOME`, and an empty `HOME` no longer resolves credentials or secrets to a relative folder.** The openai-responses Cloudflare cookie jar now defaults to `$XDG_CONFIG_HOME/routectl/cookies/chatgpt.json` when `XDG_CONFIG_HOME` is set, like routectl's other files; it used `~/.config/routectl/cookies/chatgpt.json` even then. `ROUTECTL_COOKIE_FILE` still overrides it, and without `XDG_CONFIG_HOME` the path is unchanged. With `XDG_CONFIG_HOME` unset and `HOME` set but empty (and, on Windows, no absolute `USERPROFILE`), `credentials.json` and the secrets folder used to resolve to `.config/routectl` under the working directory; they now fail to resolve, as with `HOME` unset, so `serve` runs without the oauth store instead of reading credentials from the working directory. For credentials, secrets, the cookie jar, the router's own files and the CLI's default config path, an empty or relative `XDG_CONFIG_HOME`, `HOME` or `USERPROFILE` counts as unset, and a non-UTF-8 `XDG_CONFIG_HOME` is honored.

- **`config migrate` checks the migrated text with the same raw-config checks as load and edit.** A file carrying the removed `[mitm] credential_source` key is refused with the same actionable message as load and edit (it was a redacted unknown-field error from the migrated config), and the file is left untouched. The checks run on the text the migration ladder produced, so a file with several problems may now report a ladder or pricing error before the removed-key refusal.

- **A request refused for several fields in turn is repaired in turn on the same target.** When a repaired retry is refused for a DIFFERENT field, routectl now repairs that field too and re-sends again, instead of returning the second refusal. The new refusal proves the earlier repairs, so each is committed then (learned per lane with its own value) even if the new field cannot be repaired: one with no repair row, an exhausted allowance, or a repair a concurrent request holds. A request that refuses `thinking.type: disabled` and then `max_tokens` above the model limit converges in one request with both learned; so does an openai-compat request carrying several refused sampling parameters, which used to need one request per parameter. A refusal of the value a repair just sent (the same field) stops there: nothing is learned and that refusal reaches the client. A refusal answered by undoing a routectl edit, one naming a field the sent body did not carry or that routectl cannot confirm it carried, one attributed only by an inferred template, a transport failure followed by a fallback to another target, and a non-settling token count commit nothing; a same-target retry after a transient keeps the pending repairs, and its success commits them. A field is repaired at most once per target, and the repairs draw from the per-request allowance of five. Each repair still logs its own `envelope_field_repaired` WARN. See [docs/LOGGING.md](docs/LOGGING.md#envelope-field-repair-warn).

- **A request may draw up to five repairs, up from two.** The per-request repair allowance, shared by field, beta, reasoning-replay and edit-revert repairs across every target in a fallback chain, is now five, so a chain whose targets each reject a repairable request is repaired on more of them before the rejection reaches the client. Each repair is one more upstream call. Auth retries and fallback hops are bounded separately, as before.

- **A reasoning-replay repair logs one WARN per repair.** A request that repaired reasoning-replay artifacts on two targets reported only the last repair. Each repair now emits its own `reasoning_replay_degraded` WARN describing its target, and every line carries `repair_count`, the request's total, so a request that repaired once emits one line with `repair_count=1`. See [docs/LOGGING.md](docs/LOGGING.md).

- **A rewrite repair counts as lossy, and lossy repairs are counted per lane.** `rc_field_repair_lossy_total` now counts a field rewritten to the accepted value the rejection named, alongside a dropped refused value and a lowered client token limit, so the counter agrees with the `repaired_lossy` outcome the wire check already reported. The same count is kept per learned lane (`provider_entry#upstream`, at most 256 lanes plus an `other` bucket): the INFO fidelity line carries it as `rc_field_repair_lossy_by_lane` (capped at 32 rows, with `rc_field_repair_lossy_lanes_omitted` saying how many were left off) and the served doctor panel as `lossy_repairs_by_lane`. The field-repair WARN now names its `lane`. See [docs/LOGGING.md](docs/LOGGING.md).

- **A client tool type in the beta namespace is no longer treated as a feature key.** A tool whose `type` (or any `.`-separated segment of it) reads as a `beta:` key is no longer derived as a capability the feature filter routes on, so the feature and beta-header namespaces stay apart; a beta capability is minted only from the `anthropic-beta` header.

- **Bedrock Invoke refuses the same operator keys as anthropic-api, and counts the refusal.** A key in a Bedrock Invoke provider's `additional_model_request_fields` is now withheld when it names any field the anthropic-api egress manages, plus `anthropic_version`, which the Invoke body must carry as `bedrock-2023-05-31`. Newly withheld: `max_completion_tokens`, `stop`, `n`, `seed`, `logprobs`, `top_logprobs`, `logit_bias`, `presence_penalty`, `frequency_penalty`, `user`, `response_format`, `reasoning`, `reasoning_details`, `provider_extras`, `chat_template_kwargs` and `parallel_tool_calls`. Each still logs the WARN `additional_model_request_fields attempted to override routectl-managed key; dropped` naming the key, and a request with any refused key now counts once as the `operator_extra_managed_key_conflict` policy action on the `bedrock-invoke` lane, as Bedrock Converse and openai-compat already did. Behavior change: those keys, none of which is an Anthropic Messages field, used to be merged onto the top level of the Invoke body as configured.

- **openai-compat streaming always asks for usage unless the operator says otherwise.** routectl sets `stream_options.include_usage: true` on every streaming openai-compat request after merging extras, so the stream summary and usage accounting get the terminal usage chunk. A client value other than `true` (such as `false`, or a `stream_options` that is not an object) is now overridden, recorded as a declared edit, and counted once per request as the `client_stream_include_usage_overridden` policy action; a client `stream_options` without the flag still gains it. Only `payload_extras` in operator config can set the flag to something else.

- **Client extras forward only to an upstream that speaks the client's dialect.** The top-level fields an ingress does not model (its `provider_extras`) now reach anthropic-api, Bedrock Invoke and the Bedrock Converse bag only from an Anthropic Messages client, and reach openai-compat only from an OpenAI Chat Completions client; a library caller's own `provider_extras` still reach every one of them. Any other client extra is dropped and counted once per request as the `foreign_dialect_extra` drop class on that lane, naming keys (never values) at `debug`. Operator `payload_extras` are not affected and always ship with the operator's value. This replaces the hand-kept openai-compat key blocklist: a Chat client's fields now reach a Chat upstream as sent, so a Chat client's `metadata` object forwards to openai-compat, where it used to be dropped as `metadata_unsupported` (a `metadata` object from a client of another dialect is still withheld there, as `client_fingerprint_stripped`). The `metadata_unsupported` and `forward_compat_extra_unrepresentable` drop classes are gone. Behavior change: a Chat client's Anthropic-only field, such as `top_k`, no longer reaches Claude on anthropic-api or Bedrock; send it from an Anthropic Messages client or set it in operator `payload_extras`. A strict Chat upstream that refuses a field a Chat client sent now answers with its own 400. The OpenAI Responses egress keeps its fixed allowlist, and Gemini keeps forwarding only operator extras to ingress traffic.

- **Bedrock Converse sends no Anthropic bag key a non-Claude model refuses.** On `api_shape = "converse"`, a model whose upstream id is provably not Anthropic (GPT, Nova, Llama and the like, with or without a region prefix, or a `foundation-model/` ARN naming one) refused the `anthropic_beta` and `output_config` bag keys with a validation error, so a request carrying client betas, a reasoning effort or a structured-output format failed on it. The router now withholds every client beta from such a model (the operator `anthropic_beta` floor still ships), and the Converse egress omits `output_config` from its bag, counting the loss once per request as the `output_config_unrepresentable` drop class; the structured-outputs beta that format would imply is not added either. An inference-profile ARN proves no vendor and is handled as before, and Claude models are unchanged.

- **The `name` and `strict` a `response_format` loses on the way to Claude are counted.** Converting an OpenAI `json_schema` `response_format` into Anthropic's `output_config.format` drops its `name` and `strict`, which the target cannot carry. The loss is now counted once per request as the `response_format_keys_unrepresentable` drop class on anthropic-api, Bedrock Invoke and Bedrock Converse, besides the existing log line. A conversion that loses to the client's own `output_config.format` counts nothing.

- **Active thinking no longer adds a `temperature` the client did not send.** With `enabled` or `adaptive` thinking, anthropic-api, Bedrock Invoke and Bedrock Converse used to send `temperature: 1.0` even when the client sent none. A client temperature is still rewritten to 1.0 and `top_p` is still dropped while thinking is active, but an absent temperature now stays absent (1.0 is the upstream default). The late pass that recomputed `temperature` and `top_p` after body assembly is removed; it never changed a body, since assembly already reads the thinking type that ships.

- **A learned output ceiling now caps the `max_tokens` routectl fills.** On anthropic-api and Bedrock Invoke, a request whose client sent no `max_tokens` still gets the filled value (the catalog ceiling, else 64000); when the lane has learned a lower output ceiling, that fill is lowered to it before the request is sent, and it is never raised. Bedrock Converse sends no `maxTokens` the client did not send, so there a learned ceiling caps only a client value, and with none nothing is capped. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#output-ceiling-max_tokens-fill).

- **The client's Anthropic `thinking` object and `output_config.effort` reach the upstream as sent.** anthropic-api, Bedrock Invoke and Bedrock Converse used to rebuild `thinking` from routectl's own reasoning settings; they now forward the client's object, so `adaptive`, `between_tools`, and an `enabled` budget below 1024 ship unchanged (the upstream rules on them), and `count_tokens` carries the same object. The client's effort ships beside whatever thinking it sent, and the client's type is forwarded (no config key rewrites it; see the removed model keys below). The temperature clamp follows the type actually sent, so `disabled` and `between_tools` keep the caller's sampling. Behavior change: with the knob unset, a client's `adaptive` thinking used to reach the upstream as `enabled` with a budget, and its effort beside `disabled` or `enabled` thinking was dropped; an effort of `"none"` used to be turned into `thinking: disabled`; a Messages request with an effort and no `thinking` object used to get legacy `enabled` thinking from its effort, and now gets none; a client budget below 1024 used to be raised to 1024. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#removed-model-keys).

- **A forcing `tool_choice` strips only active thinking.** Anthropic refuses thinking while `tool_choice` forces tool use, so routectl removes `thinking` for that request; it now does so only for `enabled` or `adaptive` thinking, on anthropic-api and Converse alike, and declares the removal once. Behavior change: a client's `disabled`, `between_tools` or unknown thinking type used to be stripped too and now ships beside the forced choice; the client's own `output_config.effort` used to be dropped with the thinking and now ships as sent (only an effort routectl composed is dropped).

- **A reasoning budget of exactly 8192 now maps to effort `medium` on openai-compat dialects.** The DeepSeek and vLLM dialects used their own budget threshold and mapped 8192 to `high`; they now use the shared budget table, as every other egress already did. Budgets of 8193 and above still map to `high`.

- **An unrecognized Anthropic `thinking.type` now reaches Anthropic-shape upstreams, and its loss elsewhere is counted.** The client's `thinking` object, `display` included, is sent as written to anthropic-api, Bedrock Invoke and Bedrock Converse whatever its `type`. Canonical reasoning still cannot represent the type, so openai-compat, openai-responses and gemini build their reasoning settings from the default and count the loss once per request as the `thinking_type_unrepresentable` translation drop (a `thinking` object with no `type` counts too). The ingress WARN `unrecognized thinking.type` is reworded to say this and still fires once per request.


- **Streams and token counts park and debit a seat's breaker by the same rule as completions.** A rate-limit or overload carrying an upstream reset longer than 5 seconds parks the seat until the reset (clamped to `max_honored_retry_after_ms`), and a shorter reset takes the ordinary threshold-gated failure debit, on completions, streams and `count_tokens` alike; a probe request (`max_tokens` at or below `probe_max_tokens`) never parks, and a probe's 429 or 529 never debits on any surface, token counts included, nor when it arrives in a stream after content has been delivered. Behavior change: a stream that hit a reset of 5 seconds or less used to park the seat for that reset and now debits it; a `count_tokens` request used to park on any reset, probe or not, and now debits on a short reset; and a `count_tokens` probe that draws a 429 or 529 used to debit or park the seat and now only frees its half-open slot, as completions and streams already did; a probe stream that delivered content and then failed with a 429 or 529 used to debit the seat and now leaves it untouched; and a probe stream's error after content now follows the provider's `class_overrides` remap as it does before content, so a 503 remapped to `bad-request` no longer debits, and a probe's 400 after content no longer debits either. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#honoring-upstream-resets-max_honored_retry_after_ms).

- **The prompt-cache breakpoint rules are checked by the target, not the Anthropic ingress.** A `/v1/messages` request with more than 4 `cache_control` breakpoints, or a 1h breakpoint after a 5m one, used to be refused before any target was chosen, so it failed even when routed to an openai-compat, openai-responses or gemini target, none of which carries breakpoints on the wire. Those targets now serve it. An anthropic-api or Bedrock target still refuses it locally (`cache-breakpoint-cap` / `cache-ttl-order`, counted once on that target's lane) without contacting the upstream, and the request walks the fallback chain.

- **A local refusal names its reason, and the reason decides what happens next.** Every request routectl refuses without asking the upstream now carries one reason from a closed vocabulary: `cache-breakpoint-cap`, `cache-ttl-order`, `tool-call-id-missing`, `field-type`, `previous-response-id`, `beta-header-cap`, `schema-depth`, `schema-size`, `translation-accounting`, `tool-pairing`, `unrepresentable`, `strict-translation`, `forwarded-bearer-missing`, `cloak-relocation`, `gemini-seed` and `unparseable`. A refusal raised while the ingress parses the request (`field-type`, `previous-response-id`, `beta-header-cap`, `unparseable`) ends the request. Any other refusal is classed as a bad request: the refused target is never retried, its breaker is not debited, and the request walks the fallback chain, since the next target may hold the shape the first could not. On complete and stream a `strict-translation` refusal walks the chain like the rest (on `count_tokens` it ends the request); the one exception is a missing forwarded bearer (`forwarded-bearer-missing`), which still ends the request. Behavior change: a refusal raised by the Converse, Responses or openai-compat translation used to end the request and now falls back, as the identity cloak's relocation refusal already did, and is recorded as a bad request in the usage ledger.

- **Local refusals are counted by lane and reason.** `rc_request_refused_local_total` gains a `reason` label (`{lane,reason}`), and the `wire_check.local_refusals` rows on `/status/health` gain a `reason` column, one row per lane and reason. A refusal raised before any target is chosen counts on `<ingress>->local` (for example `responses->local`). Each refusal is counted exactly once, on complete, stream and `count_tokens` alike, including one on the first target of a chain that a later target then serves. Refusals that used to leave uncounted are counted now: a missing forwarded bearer, a body that is not JSON, a strict-translation refusal raised by a provider, a `count_tokens` refusal, and a capability strip that has to be rolled back.

- **A mispaired tool transcript is forwarded instead of refused.** A transcript whose tool calls and tool results carry ids but do not pair up now reaches the target unchanged on every inference route, `count_tokens` included, instead of getting a local 400 for every target. Of the egresses only Gemini, whose translation needs each result's call, refuses such a pairing defect locally (`tool-pairing`); the chain then moves to the next target. A tool result with a missing or empty tool-call id is still refused locally by the anthropic-api egress (`tool-call-id-missing`). An upstream 400 or 422 on a mispaired transcript reaches the client as is: it is not retried, does not fall back, is not field-repaired, and teaches no learned capability. It is counted once as an unlocalized rejection.

- **Converse, Responses and openai-compat forward what their wire types can hold.** These translations now refuse locally (`unrepresentable`) only a value no wire slot can carry: on Converse a source that is not an object, has no type, is an image url, or carries a non-string media type or payload; on Responses a non-object `image_url`, a non-string image slot, or a file part whose only carriers are non-strings; on openai-compat a `tool_use` block that is not an object. Values the slot can hold go to the upstream, and its answer reaches the learning loop. Converse forwards an empty or absent media type, payload or reasoning signature, an empty data-URI payload, and a tool result without a tool id (as `toolUseId: ""`). Responses forwards an empty or absent tool call id (as `call_id: ""`), an empty image url or base64 payload, and a file part with no carrier (as a bare `input_file`); the openai-compat lift carries `tool_use` / `tool_result` ids and names verbatim, absent ones included. A user file whose only carrier is `file_url` is still refused on Bedrock Mantle (`unrepresentable`): that lane forwards no client-supplied file url. The anthropic-api egress still refuses a tool result with no id (`tool-call-id-missing`).

- **Capture containers take a checked name and upstream credentials are stripped from the driven client's environment.** `scripts/container/run_capture.sh --name <name>` names the container so an orchestrator can kill or remove exactly that cell; the name must be `routectl-cell-` followed by 1-63 of `[a-z0-9_.-]` (first `[a-z0-9]`), so it can never resolve as another container's id prefix, and anything else exits 2. Bedrock and Ollama keys are forwarded by name like the other providers. The daemon still reads the real credentials, but the driven client's environment is stripped of them and of every other credential-shaped variable (`*_API_KEY`, `*_TOKEN`, `*_SECRET`, `*CREDENTIALS*`, `AWS_PROFILE`, ...), keeping only the local placeholders, so variables added by future client releases are covered too. This keeps a client from picking a real key up natively; it is not an isolation boundary inside the cell: the runner and the daemon run under the same uid as the client, so a shell tool in the driven agent can still read the forwarded credentials from their process environments (docs/DEVELOPMENT.md "Residual credential exposure in a cell").

- **A field rejection naming a path routectl did not send no longer triggers a field repair.** When an upstream 400/422 names a request path the outgoing body did not carry, routectl no longer drops or rewrites a field for it, re-sends a re-verification canary, or records a field verdict; the wire check records the rejection as `unrepaired`. A path counts as absent only when the upstream refused a field or value it read there (an extra input, an unsupported parameter, a value outside a listed set) and every segment before the last resolved in the sent body. A missing-field report, a bare OpenAI `error.param`, a segment that is an implied union tag or a client key containing a dot, and a segment that is both an object key and that object's `type` tag are treated as unknown and proceed as before. On Bedrock Converse a path is also looked for inside `additionalModelRequestFields`. Capability learning from such a rejection is unchanged.

- **`[capability.overrides]` beta flags match case-insensitively.** A `beta:<flag>` entry in `unsupported` or `force_supported` now matches a client beta flag in any letter case, as upstreams treat them; the forwarded flag keeps the client's spelling. An `unsupported` / `force_supported` pair on the same target that differs only in case is now rejected at load as contradictory.

- **A lane at its learned beta-flag cap evicts its most recently learned entry.** When a lane already holds 32 learned beta entries, a new one now displaces the youngest (latest first seen) rather than the least recently seen, so a burst of new flags churns among itself and established rejections survive it.

- **`routectl doctor` shows every capability column.** The capability matrix no longer caps observed capability keys at 10 columns; the `(+N more capability columns)` note and the panel's `other_overflow` field are gone. An override naming a beta flag in another letter case renders in that flag's existing column. A cell decided by a catalog prior is marked stale when any nickname on the lane has a stale prior, not only the first. The doctor report and `/status/doctor` panel schema move to 16.

- **Status and doctor no longer list learned-capability rows with no provider kind.** A learned row recorded under an empty provider kind -- what an unconfigured lane resolves to -- is no longer shown as a lane's current verdict.

- **BREAKING: config schema version 5 retires the capability lists.** The per-provider and per-model `unsupported_features` lists, the `anthropic-api` provider's `allowed_betas`, and the whole `[bedrock]` table (`allowed_betas`, `allowed_body_fields`) are gone; `[capability.overrides]` is the one place a capability decision is written. A version 4 file is refused at startup naming `routectl config migrate`, and a version 5 file still carrying a retired key is refused at load naming the key's path. `routectl config migrate` carries a version 1-4 file to 5: it folds each `unsupported_features` list into `[capability.overrides.<spec>].unsupported` (the provider name, or `"<provider>:<nickname>"` for a model list), drops every EMPTY `allowed_betas` / `allowed_body_fields` list, and removes `[bedrock]` once nothing is left in it. It refuses and writes nothing when an allowlist is NON-empty -- the message names each list's path and entry count, never its values. For a beta that must never be sent, the successor is `unsupported = ["beta:<flag>"]` under `[capability.overrides.<provider>]`; a body-field list has no successor. It also refuses when a folded capability is already in `force_supported` on the same cell. Upgrade per daemon: snapshot the config, install the new binary, run `config migrate --dry-run`, review, `config migrate`, `config check`, then restart -- the running daemon must restart onto the version 5 binary, and a rollback restores the version 4 file together with the old binary. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md) "Version 5 retires the capability lists".

- **Anthropic API client betas pass through.** Every client `anthropic-beta` flag reaches the upstream, so a flag Anthropic adds needs no config change, except the flags the router withholds from the lane: an `unsupported` override on `beta:<flag>` under `[capability.overrides.<provider>]`, and seeded or learned `beta:` verdicts where the lane has them. The floor is never withheld: provider and model `header_extras` pins, the own-OAuth gate and Claude Code floor, and the body-implied structured-outputs and effort betas always ship. A forwarded-credential request withholds nothing, and each withheld flag logs at `debug`.

- **Client beta flags are normalized at ingress, and more than 64 is a 400.** On the Anthropic Messages and OpenAI Chat ingresses, every client beta flag, whether from the body's `anthropic_beta` array or (on Anthropic Messages) the `anthropic-beta` header, is split on commas, trimmed, and deduplicated in source order before any routing or withholding decision sees it, so `" a, b "` and `"a,b"` are the same two flags. A request carrying more than 64 distinct flags after that is rejected with 400 in the ingress's own error envelope; repeats do not count toward the cap.

- **Bedrock body fields pass through.** Every body field reaches Bedrock on Invoke and in the Converse `additionalModelRequestFields` bag, except `mcp_servers` and a `tool_choice` left with no non-empty `tools`, which are always dropped (a client field from a non-Anthropic ingress is also withheld, as `foreign_dialect_extra`). The Bedrock beta allowlist is gone with it: client betas reach Invoke and Converse verbatim apart from the flags the router withholds from the lane (its shipped seed plus learned rejections), and the operator floor (provider `anthropic_beta` and `header_extras`-pinned betas) is never withheld; `[capability.overrides]` on `beta:<flag>` pins either direction.

- **The legacy capability-list startup WARN and doctor nudge are removed.** No version 5 config can carry the lists they pointed at, so the `legacy_deprecation` startup event and the doctor `capability` section's `config migrate` nudge are gone. The doctor JSON schema does not change for this removal.

- **Internal:** the routectl-router library surface gains the overlay soft-defect and matrix-warm types. Added: `OverlaySoftDefect`, `overlay_soft_defects`, `log_overlay_soft_defects`, `MatrixSource`, `MatrixWarm`, and `Router::learned_seed_clear_snapshot`. `CapabilityMatrixPanel` gains the public fields `source` and `warm`, which breaks struct-literal construction of it. `load_catalog_overlay` no longer logs a WARN for soft defects; a caller that wants them logged calls `overlay_soft_defects` and `log_overlay_soft_defects`. No change for `routectl` binary users.

- **Internal:** the library surface follows the retired keys. The routectl-router API drops `OverrideProvenance`, `BedrockGlobalConfig` and `Config.bedrock`, `ModelEntry::with_unsupported_features`, `normalize_capability_overrides`, `validate_bedrock_global_config` and `BuildOptions::with_bedrock_allowed_betas` / `with_bedrock_allowed_body_fields`; `OverrideRegistry::resolve` now returns `Option<OverrideVerdict>`. The routectl-providers API drops `BedrockConfig.allowed_betas` / `allowed_body_fields` and `AnthropicApiConfig.allowed_betas`. `migrate_v4_to_v5`, `PresentAllowlist`, `preflight_raw_config` / `RawConfigPreflightError` (non-exhaustive), `RetiredCapabilityKeysError`, `Refusal::CapabilityConflict` and `Refusal::OverrideShape` are added, and `Refusal::EgressAllowlist` now carries `allowlists`. A `/status/config` capability cell's `provenance` is always `override`. No change for `routectl` binary users.

- **Learned Bedrock beta rejections persist across restarts and show in doctor and purge.** A client beta flag a Bedrock lane rejected is now recorded as a learned `beta:<flag>` capability on that `<provider>#<upstream>` lane instead of in a per-provider in-memory list. It is written to the capability ledger and survives a restart, decays on the `[capability] decay_hours` window (48 hours by default) into one re-probe that sends the flag again, and an accepted re-probe records a positive that keeps sending it. `routectl doctor` shows each one as a `beta:<flag>` cell with action `withhold`, and `routectl capability purge <provider#upstream> beta:<flag>` drops it. A `force_supported` override on `beta:<flag>` sends the flag and learns nothing; an `unsupported` override withholds it. A lane holds at most 32 learned beta entries, and `[capability] enabled = false` stops learning them.

- **The built-in Bedrock-rejected betas are a seed the operator can lift.** `advanced-tool-use-2025-11-20`, `advisor-tool-2026-03-01` and `prompt-caching-scope-2026-01-05` are still withheld on every `bedrock` lane by default, even with `[capability] enabled = false`, but the router now decides it per target: the operator floor, a capability override on `beta:<flag>`, or a learned verdict outranks the seed. `routectl capability purge <provider#upstream> beta:<flag>` lifts a seeded flag on a lane a configured model routes to; the lift survives restarts and lasts until the next catalog or overlay revision change. Code that calls the Bedrock provider directly, without the router, no longer has the seed applied and must drop those flags itself.

- **The doctor capability matrix shows the shipped Bedrock beta seed.** On every `bedrock` lane, `routectl doctor`, `doctor --json`, and the dashboard's Doctor tab now show a `beta:<flag>` column for each client beta flag routectl withholds by default, even before anything is learned. The cell reads `broken` from layer and source `seed` with the new routing action `withhold`, regardless of `[capability] enabled`; once a successful re-probe or `routectl capability purge` lifts the seed for that lane it reads `cleared` with action `allow`. A learned verdict or an override on the flag still outranks the seed, and a learned `beta:` negative now shows action `withhold` instead of `strip`, since the flag is withheld rather than the target routed away. The doctor report and `/status/doctor` panel schema move to 14.

- **Learned capability history now follows the provider entry that owns it.** When a `[providers]` entry is removed, or keeps its name but changes `kind`, the capabilities learned through it are dropped at hot reload and skipped at the next warm rebuild, so they never act on whatever that name points at next. Renaming a model nickname, or changing a provider's `base_url` without changing its `kind`, keeps the history. The warm rebuild log line gains a `skipped_owner` count, and a reload that drops entries logs one `owner_sweep` line with `dropped_owner`. A request already in flight through the pre-reload configuration cannot bring a dropped entry back: what it learns is recorded under the old `kind` and never acts for the new one.

- **The doctor capability matrix is keyed by learned lane.** `routectl doctor`, `doctor --json`, and the dashboard's Doctor tab now show one row per `<provider>#<upstream>` lane -- the same string `routectl capability purge` accepts and `routectl probe` prints -- with the provider kind and the model nicknames that map to it, instead of one row per nickname; lane-keyed learned facts previously never matched a nickname row. Each cell adds the layer that decided it, the routing action the dispatch filter takes (`drop`, `route_away`, `strip`, `reprobe`, `allow`, or `none`), and the learned entry's first-seen, last-seen, and expiry times. The panel reports how many ledger rows the replay read, replayed, and skipped by reason. A failure reading the ledger after its replay boundary now renders the matrix as unavailable (`open_failed` / `query_failed`) rather than as an empty registry. `/status/doctor` also carries the daemon's three live capability-write failure counters (`capability_events_dropped_full`, `write_errors`, `capability_writer_unavailable`), which the Doctor tab shows; any nonzero value means a learned verdict may not survive a restart. The doctor report and `/status/doctor` panel schema move to 13.

- **`routectl capability purge` takes a learned lane, not a nickname.** Learned capability facts, envelope-field verdicts included, are keyed on the provider entry that egresses a request and the upstream model id it sends, so the command's first argument is now that lane exactly as doctor and `routectl probe` print it: `routectl capability purge anthropic#claude-sonnet-4-5 web_search`. One purge clears the fact for every nickname that shares the lane. A value that is not of the `<provider>#<upstream>` form, such as a bare nickname, is refused locally with an error and a non-zero exit instead of being reported as "nothing to purge"; the daemon's control route refuses it the same way. A field verdict learned through one nickname now applies to every nickname on the same lane. Field rows in the usage ledger keyed any other way are skipped on warm rebuild and counted in the new `skipped_lane` field of the rebuild log line.

- **`/status/health` keys learned rows by lane, and each target names its lane.** `learned_negatives[].state_key` is now the learned lane `<provider>#<upstream>` the row is recorded under, not a model nickname, and every entry in `targets[]` gains a `learned_lane` field carrying the same lane as dispatch mints it for that target (`null` only for a provider entry the config would refuse). Join learned rows to targets on `learned_lane`; a target's own `state_key` stays its runtime breaker key and never matches a lane. The dashboard's Health tab joins this way, so a learned negative shows on the card of every nickname that maps to its lane. The health panel schema moves to 6.

- **`routectl probe --capabilities` prints and records the learned lane.** The report header and the `--json` `lane` field are now `<provider>#<upstream>`, the lane live traffic on the probed model learns under, rather than the model nickname. Doctor, the probe, and the status health panel now all read their lanes from one router-side projection of the configuration.

- **A doctor matrix cell whose nicknames disagree reads `mixed`.** When two nicknames on one lane resolve a capability to different verdicts or routing actions -- one carries a model-scoped override or a pinned beta the other does not -- the disagreeing field reads `mixed`, and the new `nickname_actions` list carries each nickname's own `verdict`, `layer` and `action`, in `doctor --json` and the Doctor tab's tooltip. A mixed verdict carries no lane-wide `supported`, `source` or `layer`, so a partial override is never reported as the lane's verdict. The human grid renders `mixed(alpha=allow,beta=drop)`, adding each nickname's verdict in brackets when the verdicts differ. Previously the cell showed the first nickname's override and folded the beta-pin check across all of them. When the nicknames agree the list is empty and the cell is unchanged.

- **A late request through the pre-reload router no longer deletes learned facts.** After a reload that changes a provider entry's `kind`, a request still in flight on the previous router could look up or write a lane and remove what the new kind had learned since. Learned entries are now keyed by the kind that wrote them: each router reads and writes only its own kind's entries, and an entry the current config does not own is removed only by the next reload's owner sweep or skipped at restart, never on a lookup. The `owner_lookup_drop` DEBUG line is gone.

- **Bedrock Converse now forwards `thinking.display` verbatim instead of stripping it.** The caller's value rides the `additionalModelRequestFields` `thinking` object unchanged on both the legacy `enabled` and the `adaptive` shape, and an absent `display` stays absent; Converse was measured accepting and honoring `"summarized"` and `"omitted"` on both shapes. `"updates"` additionally gains its `thinking-display-updates-2026-08-18` beta on the bag's `anthropic_beta`, even when `[bedrock] allowed_betas` omits it, never duplicated, and not when a forcing `tool_choice` has stripped thinking. Any other value forwards unchanged with no beta. The `dropping thinking.display` WARN is gone.

- **The OAuth cloak surface now identifies as Claude Code 2.1.294** (Stainless SDK 0.128.0, runtime v26.3.0, request timeout header 600, the stock client default); operators pinning `user_agent` are unaffected.

- **A malformed tool call / tool result transcript is now rejected locally with a 400.** Before any target is selected, every chat completions, messages, responses, and `count_tokens` request is checked so that each tool call is answered by exactly one tool result, matched on its id across every carrier (OpenAI `tool_calls` / `role: "tool"`, Anthropic `tool_use` / `tool_result` blocks, Responses `function_call` / `function_call_output`). A result with no matching call, a result before its call, a result for a call from an earlier closed turn, a duplicate call id or result, and a call left unanswered by the next non-result turn or by the end of the request are all refused. Previously these reached the upstream unrepaired, and most providers rejected them with an error the caller could not trace. The response is the ingress dialect's `invalid_request_error` / `validation_error` envelope, naming the defect class and the offending message index but never a tool id or tool content. The request makes no upstream call, is not retried or failed over, and does not count against provider health. Nothing is synthesized or dropped: a correctly paired transcript is forwarded exactly as before. Parallel calls may be answered in any order; an empty tool id keeps its existing normalization and is not matched.

- **The minted Claude Code identity is scoped to the exact Anthropic API host.** The compiled Stainless header pack, the default Claude Code `User-Agent`, and the session and client request ids are emitted only when an `oauth-bearer` anthropic-api provider's configured base URL is exactly `api.anthropic.com`; the auth kind alone no longer authorizes them, so loopback, lookalike, and third-party bases receive none of it. Explicit `header_extras` and `user_agent` values still reach every host, and the forwarded-client leg is unchanged.

- **Tests can no longer touch the real XDG-resolved config or the usage ledger.** `cargo test` and `cargo run` pin `XDG_CONFIG_HOME` to `target/test-xdg` through `.cargo/config.toml`, so config, credentials, secrets and the usage db resolve there; the OpenAI-responses cookie jar still resolves from `HOME` (set `ROUTECTL_COOKIE_FILE` to move it); CLI tests restore the prior `XDG_CONFIG_HOME` instead of unsetting it; the live Responses-ingress test and every leg of the live-gate isolation check run against scratch config and usage paths; and server tests drain the usage writer before their temporary directory is removed.

- **The `gen-catalog` tests now run in CI.** The two catalog-codegen tests --
  the selectors/snapshot `output_ambiguous` flag weld and the
  `catalog_baked.rs` byte-for-byte drift guard -- are
  `#[cfg(feature = "gen-catalog")]`, and no gate enabled that feature for
  tests, so a stale baked catalog or a flag that no longer matched the vendored
  snapshots could reach a release unnoticed. CI's all-features workspace run
  (`bash scripts/test-gate.sh workspace-all-features`) enables the feature and
  so runs both; there is no separate leg for them. No local hook runs them:
  the commit stage carries no test legs and the pre-push suite builds default
  features. Run `cargo test -p routectl-router --features gen-catalog --lib`
  locally after touching the catalog or the vendored snapshots.

- **Contributor gates are faster and CI reports one check.** Tests build with
  a new `test-release` profile, which keeps the shipped release profile's
  correctness semantics (opt-level, debug assertions and overflow checks off)
  but drops thin LTO, the single codegen unit, symbol stripping and
  `panic = "abort"`; `scripts/test-gate.sh` is the one place the gate
  commands are defined. The pre-push hook runs the default-features
  workspace suite on that profile and is correspondingly faster. The
  public-API baseline check is no longer a commit hook: it runs in CI and on
  demand (`bash scripts/public-api.sh --check all`). CI runs the workspace
  suite once (all features, inside the network-isolation check) instead of
  several overlapping times; a new job builds the shipped release binary and
  smoke-tests its startup, `/health`, and SIGTERM exit; and a `debug lanes`
  job lints and tests every workspace crate on the plain debug profile --
  default features for every crate, plus reduced feature sets for the
  crates that have them -- so `debug_assert!` and overflow checks still run in CI. A
  single `required` job succeeds only when every other job did, so branch
  protection can require that one check instead of one per job and leg.

- **The public-API baseline check is informational.** CI's `public-api` job runs it through `scripts/public-api-report.sh`, which reports the result as clean, drift (naming the crates) or could-not-run in one annotation and one job-summary line; the job never fails and is not part of the `required` check. The pre-push hook that ran it is removed. Baselines are no longer regenerated with each change (see [public-api/POLICY.md](public-api/POLICY.md)); `bash scripts/public-api.sh --check all` and `bash scripts/test-gate.sh public-api` still run it on demand.

- **The proactive context-window gate (`[window_gate]`) is now off by default.** A config without the block, or with an empty block, no longer reorders or skips fallback chain targets. Set `[window_gate] enabled = true` to keep the previous behavior. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#proactive-context-window-gate-window_gate).

- **Usage database: a one-time data migration on first start deletes legacy capability observations that precede the newest replay boundary.** Rows with no vocabulary version written before the latest tombstone are removed; tombstones, rows after the boundary, and versioned rows are kept. The schema version is unchanged and `routectl usage downgrade` still works. The deleted rows are recoverable only from a backup taken before the upgrade.

- **`routectl init`, the capability probe's cost confirmation, and `provider add`'s post-add probe offer now DECLINE on a non-interactive stdin instead of waiting for an answer.** The same class as the egress-defining confirmation above, on the remaining prompt surfaces: the `init` wizard's questions (scaffold-vs-wizard, offer selection, model id, default route, the write ack, and the credential-capture choice), the `proceed with the probe?` confirmation `routectl probe --capabilities` asks after printing its cost estimate, and the `run a capability probe against this provider now?` offer `routectl provider add` asks after a successful add. All three read stdin unconditionally, so a caller whose stdin was an open-but-silent pipe blocked at the prompt indefinitely. With no terminal on stdin each prompt now declines immediately without reading, printing the question (or, for the two probe offers, the cost estimate) so a scripted caller sees exactly what was declined, and naming the non-interactive flag: `routectl init --yes`, `routectl probe --capabilities --yes`, and `routectl provider add --probe`/`--no-probe`. A declined `init` writes nothing and still prints its actionable next steps; a declined probe offer dispatches no calls. All exit 0. A run with a closed stdin already declined and is unaffected, as is any interactive or explicit-flag run.
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
  logs one structured `DEBUG` line per affected request naming only the
  dropped keys. `stop_sequences` is unaffected. The gate is the LANE, not the
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
- **Internal:** the unused write path into the legacy
  `capability_learn_events` table is removed from `routectl-usage`
  (`CapabilityLearnEvent`, `UsageHandle::try_send_learn_event`, and the
  three learn-event counters). The table and its migrations stay, so a
  database keeps whatever rows the migration ladder already left in it
  (the historical v9 -> v10 step empties it on databases that old); the
  schema version is unchanged -- no behavior change.

- **Free capability probes now only re-check an acting field negative.** A free `count_tokens` probe is queued only when the lane already holds an acting negative on a field that a token count validates, on a lane with a free token count; the field value is captured before pre-flight removes it. A request that merely carries a known field no longer starts discovery: real traffic is the learning source, and probes exist to un-learn.

- **Negative capability learning is gated per target, not per request.** On a fallback chain that mixes a forwarded-credential target with an own-credential one, a rejection from the own-credential target is now learned; previously any forwarded bearer on the request suppressed learning for every target in the chain. A forwarded-credential target still never teaches.

- **An upstream error body over 64 KiB is no longer parsed for client-facing detail.** Each upstream error body is now parsed once, within the existing `MAX_ERROR_BODY_BYTES` cap, and every consumer (the client error envelope and its `param`, the stream terminal error, the failure classifier, the capability matcher) reads that one parse. An over-cap JSON body now yields a status-only client message and no `param`, the same as a non-JSON body.

- **`/status/doctor` reports the daemon's own state instead of re-reading disk.** A poll no longer parses the config file, loads the catalog overlay, or replays the usage ledger: it renders the config and overlay generation the daemon accepted and its resident learned-capability registry. The capability matrix carries `source` (`resident`, or `ledger_replay` for the offline `routectl doctor`, which keeps its replay) and, on a resident panel, `warm` (`outcome`, `class`, `summary`) describing the boot warm; an empty registry after a failed boot warm shows the warm's failure code rather than an empty matrix. The version finding names the config the daemon accepted. Two new `config` findings: `reload`, the last rejected config or overlay reload (a path-free failure class and its age, cleared by the next successful reload), and one `catalog overlay` finding per overlay cell whose write multiplier is below the sentinel. Doctor report schema 14 -> 15.

- **Routine operation no longer fills the log with WARN lines.** Log levels were reviewed across every crate; [docs/LOGGING.md](docs/LOGGING.md) now states the level policy, catalogs every remaining WARN and ERROR with the operator action it calls for, and lists what moved to DEBUG or to counters. The catalog overlay below-sentinel warning logs once at boot and once per reload that changes the overlay revision (it was every status poll). Capability rebuild skips log at DEBUG under one aggregated INFO tally. Access lines for `/health`, `/status` and `/status/*` log at DEBUG. Expected egress transforms log at DEBUG on every lane: sampling and thinking strips, fingerprint and billing withholds, `cache_control` drops on lanes with no breakpoint surface, reasoning skips, the beta retry, and the placeholder Bedrock `toolSpec`. Also at DEBUG: the would-trim shadow misfire, quota-placement fallbacks, feature-naming template misses, and the boot row-cap notes of the K-estimator and calibration warms. Relayed upstream rejections log at WARN instead of ERROR, and the Responses ingress `store=true ignored` notice logs once per process. A real-daemon test pins that polling `/status` and `/status/doctor` adds no WARN, ERROR, field-verdict, rebuild-skip or overlay-defect lines.

### Deprecated

- **`[retry] retry_on_429`, `retry_on_5xx` and `retry_on_network`.** Each still parses and still sets the retry cap of its classes, below any `[retry.classes.<class>] retry` leaf, so behavior is unchanged; each one set now logs one advisory config warning at load and in `routectl config check` naming its replacement: `[retry.classes.rate-limited] retry` for `retry_on_429`, `server-error` and `overloaded` for `retry_on_5xx`, `timeout` and `network-error` for `retry_on_network`. The class table is now the documented retry surface, with `max_attempts` as the cap of the five retrying classes. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#per-class-retry-and-fallback-policy-retryclasses).

### Removed

- **`DispatchMeta::selection_decision` (routectl-router).** The seat-selection token it carried was no longer written to the usage ledger, and nothing read it. Library callers that read it have no replacement. The sticky birth-pick and overflow-repin DEBUG lines are unchanged.
- **BREAKING: `routectl config migrate`, and loading any config below schema version 5.** A config with an older `version` is refused naming it (`config version 4 is no longer supported; v5 required.`), and a config with no `version` key (every config written for v0.9.0) is refused with `config version missing; v5 required.` Both end in one remediation, `Move the old file aside, run \`routectl init\` to write a new one, and see docs/CONFIGURATION.md for the current schema. Nothing was written.`, and both apply at `serve`, `config check`, `doctor`, `init` and every config edit; a hot reload of such a file is rejected with a WARN and the running config is kept. `routectl config migrate` is gone: invoking it is a usage error (exit 2), and nothing rewrites an older file, so write the new one with `routectl init` and carry settings over by hand ([docs/CONFIGURATION.md](docs/CONFIGURATION.md#config-schema-version-version) lists what each version retired). To roll back, restore the old file together with the previous binary. Library callers lose the migration API the command used: the routectl-router re-exports `BareOauthRef`, `MigrateError`, `MigrationError`, `MigrationPlan`, `OverlayWrite`, `PresentAllowlist`, `Refusal`, `RefusalSource`, `SeatPoolAccount`, `SeatPoolMove`, `StepOutcome`, `WriteKind`, `apply_config_transforms`, `apply_seat_pool_move`, `bare_oauth_pool_candidates`, `migrate_v2_to_v3`, `migrate_v3_to_v4`, `migrate_v4_to_v5`, `models_routed_at`, `plan_migration` and `upsert_pool_members`; `seat_naming::{EntryMaterialization, family_default_rename, plan_entry_materialization}`; and routectl-core's `failure_class::{StatusClassGuidance, class_guidance_for_status}`.
- **Breaking: the `[models.X]` keys `supports_adaptive_thinking`, `effort_levels`, `max_thinking_budget` and `max_output_tokens`.** Learned per-lane repairs, the catalog and the client's own request now decide what they set. A config still carrying any of them is refused at load, hot reload (the running router stays live), `config migrate` and every config edit, before the typed parse, with ONE message listing every `models.<nick>.<key>` path, sorted, what replaces it, and "delete the key"; values are never echoed. No `config migrate` rung removes them and the config version is unchanged: delete the lines by hand. Behavior changes that come with the removal: a request carrying an effort and no `thinking` object no longer gets adaptive thinking synthesized for it; effort levels reach the upstream unclamped until a refusal teaches the lane the level to use; a client `enabled` thinking budget is forwarded at any size, including above 65536; and an omitted `max_tokens` is filled from the learned ceiling, else the catalog prior (correct it with `routectl catalog set <selector> max_output_tokens=<n>`), else 64000. The `max_thinking_budget` advisory warning goes with its key. There is no longer a per-model thinking-budget ceiling, so an operator who used `max_thinking_budget` as a spend limit needs to set that limit elsewhere. Library callers lose the fields that relayed these keys: `RoutectlInternal::{supports_adaptive_thinking, effort_levels, max_thinking_budget}`, the matching `ResolvedModel` fields and `with_*` builders, `BedrockConfig::adaptive_thinking`, the `EditSite` variants `AdaptiveThinkingOverride` and `ThinkingBudgetCap`, and `anthropic_api::{MAX_THINKING_BUDGET_READER_KINDS, egress_reads_max_thinking_budget}`; `anthropic_api::emitted_thinking_type` takes only the request. `routectl doctor`'s Model knobs section loses its config state and its catalog row names `routectl catalog set` (doctor and `/status/doctor` schema 21). See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#removed-model-keys).
- **`OpenAiCompatConfig::disable_stream_include_usage`.** No configuration key ever set it. Library callers that want the upstream not to send usage set `stream_options.include_usage` in `OpenAiCompatConfig::payload_extras`.
- **The hidden `routectl pricing` alias for `routectl catalog`.** Use `routectl catalog ...`.
- **Paid capability probes, the `routectl probe --capabilities` command, and the `[fidelity] paid_probe_daily_caps` key** (all added on develop after v0.9.0 and never released). There is no top-level `routectl probe` command any more: invoking it is an unrecognized-subcommand usage error (exit 2). The free, read-only `routectl provider probe` reachability check is unchanged, and capability verdicts an earlier probe recorded keep acting until they lapse normally. routectl no longer makes paid probe calls, keeps no paid-probe reservation ledger, and no longer reports paid-probe budget rows: `rc_paid_probe_budgets`, `rc_paid_probe_budgets_total`, `rc_paid_probe_budgets_omitted`, and `rc_paid_probe_consumed_unauthorized_total` are gone from the status line, as is the `paid-probe unit committed as shutdown began` WARN. A config written against a development build that still sets `[fidelity] paid_probe_daily_caps` is refused at load, by `config check`, by the edit commands, and by `config migrate` at any version, with an error naming the key to delete. Reservation rows an earlier development build left in the usage database are inert and are not removed. `[fidelity] prefix_impact_opt_in` is unchanged.
- **The post-setup capability-probe offers, and the `--probe` / `--no-probe` flags on `routectl init` and `routectl provider add`** (added on develop after v0.9.0 and never released). Neither command offers a capability probe after it writes config any more, so neither asks the `run a capability probe against this provider now?` question. Both flags were removed outright rather than kept as no-ops, and passing either is now an unknown-argument error; drop the flag from any script that passes it.
- **The hidden `routectl provider capture-envelope` command** and its `ROUTECTL_BEDROCK_ENVELOPE_CAPTURE` environment variable.
- **Unwired development scripts.** `scripts/coverage.sh`, `scripts/test-inventory.sh`, `scripts/stray-build-dirs.sh` and `scripts/bootstrap.sh` are gone; their commands are now recipes in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) "Maintenance recipes" (coverage snapshot, test inventory, stray build dirs) and "Commit gate (one-time setup)" (`pre-commit install --install-hooks`). The driver credential-resolution probe (`scripts/drivers/config/credential_probe.sh`, its self-test and `scripts/drivers/lib/resolve_bin.sh`), `scripts/container/build.test.sh`, and `scripts/drivers/profiles/README.md` are removed; the client-profile rules now live in docs/DEVELOPMENT.md "Client profiles".

### Fixed

- **A provider's timeouts now win over the `[retry]` table.** `request_timeout_ms` on `[providers.X]` used to apply only when `[retry] request_timeout_ms` was unset, and `stream_first_byte_timeout_ms` on `[providers.X]` never applied at all, because `[retry]` always carried the 10-minute first-content default. Both timeouts now resolve model (first-content only) > provider > pool > `[retry]` > default, each key independently. A config that sets a timeout on both a provider and `[retry]` now gets the provider's value for that provider. `routectl config show` no longer prints `[retry] stream_first_byte_timeout_ms` when the config omits it; it still defaults to 5 minutes at runtime. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#per-provider-runtime-gates).

- **Translation-drop counters count a client request once, not once per attempt.** A request that is retried on the same target, re-sent after a learned beta repair, or falls back to another lane is normalized again on each attempt, and each pass used to bump the lane's request count (`lane_seen`) and every `(lane, drop_class)` drop count again, so retries and repairs inflated both. The lane count and each drop class now advance only the first time a client request reaches that lane or loses that class, and the request's `translation_drops` usage column counts the distinct `(lane, drop_class)` pairs, equal to what the request added to the global drop counts. Policy-action counts are unchanged and still count every attempt, so a policy-action rate can now exceed 1.0 on a lane where requests are retried. A library caller that sets no per-request observation slot keeps the previous per-call counting.

- **An invalid Bedrock Mantle key is no longer sent a second request before parking.** A 401 the upstream names as an invalid credential (`Invalid bearer token`) now parks the seat after its first request instead of spending the auth refresh-and-retry on a key no refresh can revive. An expired key keeps its one retry, and an unclassified 401 on an OAuth provider still refreshes and retries once.

- **A routine OAuth token refresh no longer lifts every credential park.** routectl persists a refreshed token to the credentials file, and the reload that write triggered used to lift every park, so a rejected credential was re-dialed after each refresh. That reload now finds no seat changed and lifts nothing.

- **A Bedrock token count rejected for a field routectl added is now laid to that edit.** `/v1/messages/count_tokens` on a Bedrock seat (Invoke or Converse) now records the request body its CountTokens envelope carries, decoded from the base64 wrapping on Invoke, so a 400 naming a path routectl declared adding logs the `routectl_edit_rejected` WARN naming the edit site, as on an anthropic-api count; before, a Bedrock count recorded nothing and such a rejection was never attributed.

- **A request routectl cannot build or serialize fails closed instead of counting as a network failure.** When an egress cannot assemble the outgoing request (a header value the HTTP stack cannot carry, such as a client beta flag containing a control character, or a body that fails to serialize), the attempt now fails with `500 internal_error` and the request stops there. Before, it was classed as a network error: it was retried on the same target, fell back to the next one and was debited against the target's circuit breaker, though nothing had reached the upstream. Applies to every provider; transport failures after the request is sent are unchanged. The logged detail names the provider and the failing step, never the offending value.

- **Bedrock streams end with an error on a corrupt eventstream frame.** A frame that fails to decode (bad prelude or CRC) on an InvokeModel or ConverseStream response now ends the stream with a stream error instead of skipping ahead, so later bytes are never parsed out of a misaligned buffer. The WARN carries the prelude only; the frame hex (up to 256 bytes) stays at TRACE. See [docs/LOGGING.md](docs/LOGGING.md).

- **Invoke, Converse and openai-compat streams error when they end before their terminal event.** A Bedrock InvokeModel stream that closes without `message_stop`, a ConverseStream without `messageStop`, or an openai-compat stream with neither `data: [DONE]` nor a `finish_reason` on every choice now ends with one stream error after the content already sent, instead of finishing as if complete; usage evidence for such a stream is not settled. An openai-compat host that never sends either terminal signal now errors on every stream. An openai-compat stream also ends at its first error event, emitting any held-back `<think>`-tag bytes first.

- **Chat Completions streams return the upstream's cached-token count and `system_fingerprint`.** A streamed response's usage chunk now carries `usage.prompt_tokens_details.cached_tokens`, as the non-streamed response already did, so an OpenAI-shape cost tracker sees the cache hit either way. Every stream chunk now carries the `system_fingerprint` the upstream sent, where it was always `null` before; it stays `null` when the upstream sent none.

- **Bedrock Mantle's `openai.`-prefixed reasoning models get the OpenAI reasoning-model shaping.** The model profile table matched bare ids only, so on the Mantle Chat Completions lane `openai.gpt-5.6-terra` kept its sampling parameters and was sent `max_tokens`, which it refuses before any other field; a client sending an output limit got a 400 on every request. A leading `openai.` is now ignored when looking up a profile, so these ids drop sampling parameters and send `max_completion_tokens` exactly as their bare ids do. See [docs/PROVIDER-QUIRKS.md](docs/PROVIDER-QUIRKS.md#openai--o-series--gpt-5).

- **GPT-6 models get the OpenAI reasoning-model shaping.** The model profile table had no `gpt-6` row, so `gpt-6-sol`, `gpt-6.1-sol`, `gpt-6-astra` and `gpt-6-luna` (bare or `openai.`-prefixed) were sent `max_tokens`, which each refuses, and sampling parameters they refuse (`top_p` on all four; `temperature` and both penalties on `gpt-6-sol` too). They now drop sampling parameters and send `max_completion_tokens`, as the GPT-5 reasoning models do.

- **`config migrate --help` names the whole migration ladder.** The help described only the v1->v2->v3 rungs. It now names every rung through v4->v5 with what each one folds, and says that a file already at version 5 still carrying `allowed_betas`, `unsupported_features` or a top-level `[bedrock]` table is refused with each key named.

- **Bedrock Converse forwards a client's structured-output format.** Converse treated the client's whole `output_config` as a key routectl manages and dropped it, so an Anthropic client's `output_config.format` never reached the model on that lane while the same request on anthropic-api kept it. The client's `format` now merges into the `output_config` routectl composes, beside any effort, and wins over a `response_format` conversion as it does on anthropic-api. It is read only from an Anthropic Messages client or a library caller; another dialect's `output_config` counts as `foreign_dialect_extra`.

- **Bedrock Invoke refuses a top-level cache marker it cannot place instead of dropping it.** When moving a caller's top-level `cache_control` onto a content block would break the breakpoint rules (more than four, or a 5m breakpoint ahead of a 1h one), the Invoke target now refuses the request locally as `cache-breakpoint-cap` or `cache-ttl-order`, counts it once on `rc_request_refused_local_total` for its lane, and the chain moves to its next target; before, the marker was silently removed and the request was sent without it.

- **A failure to serialize a request routectl built itself is a 500, not a 400.** When routectl cannot serialize a body it assembled, or a body it just serialized is not a JSON object or array, the client now gets `500 internal_error` with a generic body instead of `400 bad_request`, and the failure is not counted as a local refusal. It is a routectl bug, not something the client can fix. Routing is unchanged.

- **An empty mid-conversation system turn no longer fails the request on the anthropic-api lane.** After an idle compaction Claude Code can send a `role: "system"` turn whose content is empty (`[]`, `""`, or only whitespace text) alongside a top-level `system`; routectl refused it with a 400 (`messages[N] is a Role::System turn with no content`), ending the session. Such a turn is now omitted from the upstream body and counted on the existing forwarded-system DEBUG line (`system_turns_dropped_empty`); a system turn carrying any non-blank block, including a non-text block, still forwards unchanged. When omitting the turn leaves a user (or assistant) turn on each side of it, the two are merged into one message with both bodies in order, and the same applies to a turn dropped by the billing/attribution screen; the merge is skipped, leaving the two messages adjacent as Anthropic accepts them, when it would need a block reorder (`tool_result` blocks after non-`tool_result` content, or thinking blocks after assistant content), counted as `system_turn_merges_skipped`. A `cache_control` breakpoint on the omitted turn moves onto the last block of the preceding message, so the request keeps the breakpoint that already suppressed routectl's own cache placement (counted as `system_turn_breakpoints_moved`, or `system_turn_breakpoints_dropped` when no preceding block can bear one).

- **A new learned beta flag can no longer push a lane past its cap or evict an unrelated fact.** When all 32 learned beta entries on a lane are held by in-progress purges, a new beta-flag observation is now refused (logged as `event=evict reason=beta_lane_cap_leased`) instead of exceeding the per-lane bound, so it can no longer evict an unrelated learned fact through the global cap; it is learned on a later observation once the purge settles.

- **The computer tool is learned and overridden as `computer_use`.** A request carrying the `computer_20250124` tool now derives the `computer_use` capability key, as do the learn-side tool-type and openai-param rejections, so `[capability.overrides.<spec>] unsupported = ["computer_use"]`, a `computer_use` entry in `[capability] essential`, and a `computer_use` catalog prior now apply to it; previously it keyed as `computer` and none of them matched. `computer_toolset_*` tools keep their own `computer_toolset` key. A learned `computer` negative recorded before the upgrade stays under its old key until it decays or is purged with `routectl capability purge`.

- **A learned field negative that a free token-count re-check validates no longer decays into an ordinary re-probe or a re-verification canary**, so an ordinary successful request can no longer clear it before (or without) the durably acknowledged re-check.

- **A learned field negative owned by a token-count re-check no longer stays stuck when the re-check never answers.** Such a negative still keeps acting past its decay deadline while its re-check is pending, but once a fixed 48 hours beyond that deadline have passed it falls back to the ordinary re-probe, so a re-check job that was exhausted or abandoned can no longer leave the lane withholding the field until a restart. The `routectl doctor` and `/status/doctor` capability matrix applies the same rule, so a cell shows `reprobe` exactly when a request would re-probe, rather than for a negative the router is still holding for its re-check.

- **A learned field rewrite keeps its value across a catalog or overlay revision change.** The replay boundary written on a revision-changing reload or boot restated each surviving verdict without its value, so a rewrite verdict came back unable to act until it was relearned. The restatement now carries the value.

- **A tool the egress withheld or could not translate no longer teaches the capability store.** When an attempt's egress kept a hosted tool off the wire, that attempt's success or rejection is no evidence about the tool, so it is neither learned nor cleared from it.

- **`config migrate` no longer half-applies or silently drops a capability list.** The file is left untouched when a later step refuses after an earlier one succeeded, and a retired `unsupported_features` list whose `[capability.overrides]` destination has the wrong shape is refused (kind `override_shape`) instead of being dropped; a version 1 file that still carries retired capability keys now migrates.

- **`config migrate` on a version 5 file that still carries a retired capability key names the key to remove.** Such a file has no migration rung left to fold the key, and previously failed with a generic unknown-field error that did not say which key was at fault; it is now refused up front with each retired key's path named, and the file is left untouched.

- **Bedrock Invoke sends each beta flag once.** A flag in both the provider `anthropic_beta` floor and the client's betas shipped twice in the body's `anthropic_beta` array. The floor and client flags are now merged with the floor first and each flag kept at its first occurrence; withholding never removes a floor flag.

- **A Converse-wrapped Bedrock template rejection is now learned.** Converse puts `The model returned the following errors: ` in front of a model validation message that InvokeModel returns bare, and only the exact-phrase rejections were unwrapped, so a wrapped template rejection such as `tool type 'advisor' is not supported for this model` was never learned on the Converse carrier. The wrapper is now removed once before both the template and the phrase match, so the rejection is learned on either carrier; a doubled wrapper still fails closed.

- **`[capability] essential = ["forced_tool_choice"]` now loads.** The key was known only to the router, so an essential list naming it was refused at config load as an unknown capability. It is now a well-known capability key: the config loads, and the key renders as a leading doctor matrix column.

- **A boot after a catalog or overlay bump keeps learned envelope-field verdicts.** A revision change at startup wrote a fresh replay boundary and replayed nothing, so wire-shape verdicts that are meant to survive a revision change were lost until relearned, while a hot reload already restated them. Boot now restates them through the same boundary batch reload uses, and installs them only once that batch has committed.

- **Positive capability observations no longer fill the rebuild window.** Every successful request wrote a `verified` ledger row, so routine traffic filled the bounded warm-rebuild window within days and pushed older learned negatives out of restart replay. A positive is now written only on a verdict transition -- a first observation, or a positive replacing a cleared negative; a repeat of the same verdict refreshes the resident entry without a row.

- **A SIGHUP during daemon startup no longer terminates the daemon.** The
  SIGHUP reload handler was installed by a background task after startup had
  begun, so a SIGHUP that arrived before that task ran -- for example from a
  supervisor reloading as the daemon came up -- took the default signal
  action and killed the process. The handler is now installed before the
  daemon starts serving.

- **A long thinking stream no longer falls over before its first content.** A thinking block can stream many empty reasoning deltas before its first reasoning detail, and each one counted toward the 8-chunk pre-content limit, so a healthy upstream was abandoned for a fallback. A single-choice chunk carrying only an empty reasoning string no longer counts toward the limit, and a consecutive run of them is held and replayed as a single chunk. Every other content-free chunk still counts, the commit boundary is unchanged, and `stream_first_byte_timeout_ms` still bounds a stream that never sends content.
- **A stream that gives up before first content now says what it received.** When an upstream sends more than 8 content-free chunks before any content, or closes having sent only content-free chunks, the fallback error now ends with an ordered, run-length summary of the kinds it buffered, e.g. `(buffered: role, empty_reasoning, usage x2, finish)`. The kinds are `role`, `empty_text`, `empty_reasoning`, `finish`, `usage`, `upstream_meta`, `empty_choices` and `other`; no text, ids or token counts are included. The limit, the commit boundary and fallback behavior are unchanged.
- **Bedrock Invoke no longer ships a `tool_choice` without `tools`.** An `[bedrock] allowed_body_fields` list that keeps `tool_choice` but omits `tools` left the choice on the body with nothing to select, a pairing Anthropic rejects, so every tool-using request failed. The orphaned `tool_choice` is now removed after the allowlist filter, with a DEBUG line naming only the field; a list that keeps both is unaffected.
- **The cloak `sensitive_words` scan is now linear in request size.** Each request's system and message text is scanned in one leftmost-longest pass over its case-folded form; only where a match would end or start inside a character whose lowercase is two characters (U+0130) does the scan re-read up to one word length. On a 32 MiB body, release build: a list at the 32 x 32 bounds of words sharing a first letter takes about 0.4 s (previously about 225 s, holding an async worker for the duration), 31 nested words matching everywhere about 0.6 s, and the re-read worst case (15 words each ending inside U+0130, against text made only of U+0130) about 2.6 s. Matching and the marked output are unchanged. The text is processed in fixed-size windows, so the scan's working memory no longer grows with the body.

- **A zero-argument streamed tool call is no longer lost when routectl consumes another routectl's Anthropic stream.** A `tool_use` block can close with no `input_json_delta` at all, which is how routectl's own Anthropic re-emission sends a call with no arguments, so a routectl-to-routectl hop dropped the call while `stop_reason` still said `tool_use`. The call (id, name, empty arguments) is now emitted when the block opens, and any later argument fragments append to it.
- **`serve` no longer aborts at startup on a config with a default-chain Bedrock provider.** Building the AWS default credential chain panicked for lack of an async sleep implementation, taking the whole process down before it bound a port. `aws-config` now builds with its `rt-tokio` feature, so the chain resolves.
- **`mcp_servers` is never forwarded to Bedrock, on Invoke or Converse, regardless of `[bedrock] allowed_body_fields`.** Bedrock rejects the field on both carriers, yet it shipped whenever the list was empty or named it. It is now removed as the last body mutation on each carrier, and the drop is logged by field name only.
- **The three Bedrock-rejected Claude Code betas are withheld on both carriers, even with an empty `[bedrock] allowed_betas`.** `advanced-tool-use-2025-11-20`, `advisor-tool-2026-03-01` and `prompt-caching-scope-2026-01-05` each 400 every Bedrock request, and Claude Code sends all three, so a lane with no allowlist failed every Claude Code request. They are now dropped from client betas in both allowlist modes; the operator floor (provider `anthropic_beta` and `header_extras`-pinned betas) still sends them.
- **A plain-text document with citations enabled no longer 400s on Bedrock Converse.** A `text`-source document sent with `citations: {enabled: true}` -- in a user message or a tool result -- was base64-encoded into `source.bytes`, which Converse rejects for a cited text document. It now ships its body verbatim as `source.text`. Without citations it still ships as `source.bytes`, and a `base64` source is unchanged whether or not citations are on.
- **A `cache_control` marker on a tool result now caches on Bedrock Converse.** A marker on a part of a tool-role turn was dropped with a warning while still counting toward the 4-breakpoint cap, so the breakpoint an agentic client places on its latest tool result never cached. It now ships as one `cachePoint` right after that `toolResult` block, the position Converse caches from (a `cachePoint` inside `toolResult.content` is accepted but ignored). Several marked parts on one tool result share that single `cachePoint`, which takes the longest TTL among them; one cache write covers them all, and a request never carries more `cachePoint` blocks than the markers it was charged for, so no request that fit under the cap before now exceeds it. Markers nested inside a raw Anthropic-shape `tool_result` content array are still not carried or counted. The `tool_result_cache_control` drop class is gone.
- **Bedrock Converse structured outputs no longer 400 on a schema that omits `additionalProperties`.** The `additionalModelRequestFields.output_config.format.schema` now gets the same repair as the `anthropic-api` and Bedrock Invoke egresses: `additionalProperties: false` is supplied on every object node that omits it, a present `true` is forwarded verbatim with one WARN, an `output_config` removed by `[bedrock] allowed_body_fields` is left alone, and a schema past the repair walk's depth or node limit is rejected locally before any upstream call.

- **A request that forces a tool call now routes away from models that reject a forced `tool_choice`.** Some models refuse `tool_choice` `any` or a named tool with a 400 (`tool_choice: type "tool" and "any" are not supported for this model.`), and a chain kept sending forced requests to them. A `tool_choice` that forces a tool -- Anthropic `{"type":"any"}` or `{"type":"tool","name":...}`, OpenAI `"required"` or `{"type":"function",...}`, Converse `{"any":{}}` or `{"tool":{"name":...}}` -- now derives the `forced_tool_choice` capability key. That rejection is learned for it: on Bedrock (InvokeModel bare, or inside the Converse `The model returned the following errors: ` wrapper, gated on `ValidationException`) from one observation, and on the first-party Anthropic API from one observation too, because the whole message must equal the captured rejection exactly (case-sensitive). A learned lane is moved to the tail for forced requests only; `auto`, `none`, and absent `tool_choice` are unaffected. The directive is never removed from the request -- a forced call the caller asked for is not silently relaxed -- and when every lane has learned the negative the request now fails before dispatch with routectl's own 400 naming `forced_tool_choice` (see the fail-fast entry under Added). Operators can pin it with `[capability.overrides."<target>"] unsupported = ["forced_tool_choice"]`.
- **A file returned by a tool now reaches the OpenAI Responses upstream.** A tool result carrying a file part (an OpenAI-shape `file`, or a native Responses `input_file`) was dropped with only a warning, so the model answered without the file. It now ships as an `input_file` item inside `function_call_output.output`, in its original position among the tool's other parts; the Responses API accepts `input_file` there, and the ChatGPT Codex backend was verified to read the file (api.openai.com was not probed). A file part naming no `file_data`, `file_id` or `file_url` fails the request, as it already did on a user turn. The Bedrock Mantle lane has no evidence it accepts files in tool output, so it still drops them. A user-turn file part's `file_url` now forwards too, on the api-key and chatgpt-oauth lanes; on Bedrock Mantle it is still left off the wire, and a user-turn file carrying only a `file_url` fails there as it did before. Tool-result parts with no tool-output slot (documents, thinking, nested tool calls or results, unknown parts) still drop, now counted once per request as `tool_result_part_unsupported`.
- **A number, boolean, or array `tool_choice` is now counted as a lost tool_choice on the Bedrock Converse egress.** The Converse `toolChoice` union has only object members, so these values were already dropped, but silently: no warning and no `tool_choice_shape_unrepresentable` count, while an unknown string or object got both. They now warn (naming the JSON type) and count once per request on that class. A `null` `tool_choice` still means the caller set none and records nothing.
- **Anthropic thinking tokens now reach the usage ledger as reasoning tokens.** Anthropic reports them as `usage.output_tokens_details.thinking_tokens` (on the closing `message_delta` when streaming), but the anthropic-api egress read a flat `usage.reasoning_tokens` that the wire never sends, so every thinking request recorded no reasoning tokens. The nested field is now read on buffered and streamed responses, on both the anthropic-api and Bedrock Invoke lanes. A response without thinking still records none (absent, not zero). Thinking tokens are already inside `output_tokens`, so recorded cost is unchanged; the reasoning column in usage reports and the thinking-capability signal are what change.
- **A summary-only reasoning item echoed back by a Responses client now keeps its summary on the OpenAI Responses egress.** An item carrying a summary but no `encrypted_content` -- what the server returns when encrypted reasoning is not included -- was dropped whole, summary and all, because re-sending it by its id fails upstream. It now ships as a `reasoning` item carrying the summary with no `id` and no `encrypted_content`, the shape verified against the ChatGPT Codex backend (api.openai.com was not probed). Reasoning text on such an item still has no verified carrier and drops, counted as `reasoning_text_unsigned`. Items with a signature are byte-for-byte unchanged.
- **`config check` now refuses an openai-responses entry with a misplaced `account_id_ref`.** A `chatgpt-oauth` entry with a static bearer and no `account_id_ref`, or an `api-key` entry carrying one, passed `config check` clean and failed only once `serve` or a hot reload built the provider. The rule now runs in the shared config validation, so `config check` refuses it with the same error; no secret is read to decide it. `oauth://` bearers may still omit `account_id_ref`.
- **Provider names are capped at 128 bytes.** A provider name keys runtime and learned state, so an unbounded name could create an equally unbounded key. `config check`, `serve` startup, and hot reload now refuse a longer name; the error reports the length and withholds the name. Names within the limit, including ones carrying separator characters, are stored exactly as before.
- **The OAuth identity cloak no longer discards a non-Claude-Code client's system prompt, or its image and document system blocks.** On the own-OAuth Anthropic lane the cloak moves the client's system content into the first user message; with no user message it used to drop the whole prompt silently, and any system block without text was dropped too.
  - A history with no user turn but earlier assistant turns now gets one synthetic leading user message carrying the `<system-reminder>`.
  - A request with nothing left once its system content moves (no `messages`, a non-array `messages`, a `messages` entry that is not an object with a string `role`, or only system turns) now fails before anything is sent. The error is a 400 `invalid_request_error` saying routectl refused the request. It never retries the seat, never counts against its circuit breaker, and never teaches the capability registry anything, whatever `class_overrides` or `[retry.classes]` say about status 400, and a configured fallback gets the caller's original request.
  - Image and document blocks in forwarded system turns are carried as their own user-content blocks right after the reminder, in order. Their `cache_control` markers are kept and checked against the 4-breakpoint cap and TTL ordering, as is the reminder's own marker at its new position after any earlier history markers; a body that would break either fails locally the same way.
  - Tool-use, tool-result, thinking, redacted-thinking, and unknown blocks still cannot move into a user turn. They are dropped and now counted as `cloak_unrepresentable_system_block_dropped`.
  - The policy-action classes `cloak_client_system_prompt_discarded` and `cloak_non_text_system_block_dropped` are gone, because those losses no longer happen. Text-only relocation output is byte-for-byte unchanged, and `strict_mode` is unchanged.

- **An unrecognized `text.format` type from a native Responses client is now forwarded to an OpenAI Responses upstream unchanged.** The Responses egress matched the tag against a closed list and dropped anything else -- counting it as a `response_format_type_unrepresentable` translation drop -- so a newer structured-output format member the upstream accepts was silently replaced by free-form prose. The whole format object now rides through verbatim and the upstream judges it; that counter class is retired on the openai-responses lane. Cross-dialect egresses still drop an unknown tag with a WARN, since their wires have no member it could become: Gemini also counts it as a translation drop, while Anthropic and Bedrock drop it without a counter.
- **DeepSeek `history_reasoning = "preserve"` no longer leaves `reasoning_content` absent on a tool-call turn.** An assistant turn carrying `tool_calls` with no reasoning to echo now ships `reasoning_content: ""`, the shape DeepSeek's thinking mode accepts; with the key absent it rejected the request. Other assistant turns, and the `auto` and `strip` modes, are unchanged. The vLLM dialect shares the preserve path and sends the same empty key.
- **Anthropic `thinking.display` is no longer overridden by the redact-thinking beta.** When the assembled body carries `thinking.display`, the `anthropic-api` egress removes `redact-thinking-2026-02-12` from `anthropic-beta` whichever source added it (client, operator config, or the OAuth beta floor) and logs one DEBUG line; with both present Anthropic returned redacted thinking and the requested display mode was lost. Requests without `thinking.display` keep the flag, and the forwarded-credential leg still passes the client's betas through verbatim.
- **The Gemini schema cleaner no longer drops `allOf` contents or leaves dangling `required` entries, and refuses oversize schemas.** `allOf` branches are now merged into the parent schema (a conflicting merge falls back to the previous drop, still counted), and `required` is filtered to the properties the cleaned schema still declares. A schema deeper than 64 levels is refused, and so is a request whose tool and response schemas together exceed 10,000 nodes or 8 MiB of emitted bytes (a small document whose `$ref` targets fan out, or many tools each individually modest): the ceilings apply per request, not per schema, with a 400 and one WARN instead of cleaning without bound and forwarding. A `$ref` pointer longer than 1 KiB is treated as unresolvable, and an `enum` is charged at its coerced string size.
- **A client dispatch no longer releases another request's half-open circuit-breaker probe slot.** Client complete, stream, and `count_tokens` paths armed their cancellation guard by re-reading the shared half-open flag, so a request that was never admitted as the probe could free the slot the real probe held and let a second probe through. Each path now arms its guard from the admission that actually took the slot.
- **Two different routing identities can no longer share one circuit breaker and rate-limit bucket.** A direct model keys its runtime state by its nickname, a pool seat by `nickname#member`, and a provider entry by its own name, all in one map, so a name spelled to match another identity's key silently shared that identity's breaker, RPM bucket, probe slot, and learned state. `#` is now reserved in every `[models]` nickname and every `[providers]` name, and `config check`, serve, and hot reload refuse a name carrying it -- including unselectable and unreferenced entries, which the build previously skipped. A model nickname equal to the name of a provider it does NOT dispatch through is refused for the same reason; a model named after its own provider, the shape `routectl init` writes, is unchanged. A library caller installing a resolved-model table directly gets the same protection: an ambiguous model is dropped with a WARN rather than installed onto another identity's slot. A slot stays bound to the identity that first created it for as long as the slot exists, so removing a model and re-adding the same name under a different provider or pool is still refused. Every valid key keeps its exact bytes, so breaker and rate-limit state still carry across a hot reload and status and logs read the same names. A hot reload carries a key's state only when the same identity owns it on both sides: a model moved to another provider, or switched between direct and pooled, starts with a fresh breaker and rate-limit bucket instead of inheriting the old identity's.

  **Before upgrading:** a config that uses `#` in any `[models]` or `[providers]` key, or names a model after a provider it does not dispatch through, will no longer load. Rename those entries before you upgrade. Runtime state recorded under the old names (breaker, rate-limit, probe, and learned-capability entries) is not migrated: a renamed entry starts fresh.

- **System content is no longer dropped or forwarded unfiltered on the Gemini, OpenAI Responses, and OpenAI-compatible egresses.** The Gemini egress skipped the Claude Code billing/attribution filter entirely, the OpenAI Responses egress discarded every `role: "system"` message in `messages[]` (only the top-level `system` reached `instructions`), and the OpenAI-compatible egress filtered only when a top-level `system` was present, so a system message arriving on its own, for example through the Anthropic Messages ingress, reached the upstream unfiltered. All three now withhold the billing/attribution block from both the top-level `system` and every system message, counting once per request under `client_fingerprint_stripped` even when both carry it. Legitimate system content is kept: Responses joins the top-level `system` first and then system-message text in message order, and OpenAI-compatible keeps every system message in place, in caller order.

- **Tool definitions in three shapes real clients send are now accepted.** A legacy function element with no `type` (`{"function": {...}}`) no longer fails the whole request, a camelCase `inputSchema` now carries the same schema as `input_schema` instead of an empty one, and function declarations in an OpenAI Responses `additional_tools` input item now join the request's tools. Those inline functions now count for capability routing and cost estimation, so a tool-bearing Responses request that was previously routed and costed as tool-less may pick a different model. A later inline function with an existing name replaces the earlier definition, so every lane sees the definition a Responses replay sends. Inline declarations with no portable form (hosted, MCP, namespace, or unknown tools) still reach only a Responses upstream, verbatim.

- **A streamed response that was cut short, or that reported an upstream error inside a successful HTTP response, no longer reaches the client as a finished turn.** Once a stream has answered HTTP 200 the transport reports a dropped connection, a truncating gateway, or an upstream that closed early as an ordinary end of stream, so a half-written answer could be delivered with nothing to tell it apart from a complete one -- and a coding agent acting on it would act on half a response. On the egress lanes whose wire closes every turn with an explicit terminal event, the stream is now checked against that event when it ends: on the Anthropic Messages lane only `message_stop` closes a turn (a stream that ends after `message_delta` but before `message_stop` was still cut), and on the OpenAI Responses lane any of `response.completed`, `response.incomplete`, `response.failed`, or `response.cancelled` does. A stream that ends without its terminal event now ends with an upstream error that names the cause and classifies with the other transport failures, on both the streaming and the buffered path. Whatever content arrived before the cut is still delivered first. In-band error reporting inside a 200 stream is normalized the same way: an error the upstream reports mid-stream surfaces as an upstream error whose client-facing status agrees with the one routectl records, rather than as an empty success or a mismatched status.

  The check fails toward reporting completion. A complete stream is unchanged on every touched lane, including gateway variants that append a `[DONE]` sentinel after the terminal event and gateways that attach `error: null` or `error: {}` to every normal chunk. A `[DONE]` that arrives BEFORE the terminal event does not count as one. A stream that ends with its own terminal failure (`response.failed`, `response.cancelled`, or an in-band `error` event) reports that failure once, never a second "cut" error on top of it. A lane whose wire has no terminal event -- where end of stream IS the terminator -- is deliberately not checked this way, since the check would flag every healthy stream there.

- **routectl no longer rejects request vocabulary the upstream owns -- which restores Remote Control (`[mitm]`) against a current Claude Code client.** Several ingress validators held local allowlists over enum-ish fields whose value sets upstream keeps extending, so a client sending a value newer than routectl's copy of the vocabulary got a local HTTP 400 before the upstream was ever consulted -- a failure the operator cannot fix in config and cannot distinguish from a real rejection. The live instance was `thinking.display`: a current Claude Code session sends `display: "updates"` (added by the `thinking-display-updates-2026-08-18` beta), which the previous allowlist refused, failing every inference request the MITM proxy re-injected. Four changes, all on the same principle -- forward the caller's bytes and let the upstream own vocabulary; keep only the checks routectl structurally needs:
  - **`thinking.display` forwards verbatim on the Anthropic Messages ingress.** The captured literal rides an internal carrier to the `anthropic-api` egress and is stamped onto the wire `thinking` object unchanged, on both the legacy `enabled` and the Opus 4.7+ `adaptive` shape, and the beta flag that gates it survives the default (empty) `[providers.X] allowed_betas` pass-through. Known values still derive the canonical `reasoning.exclude` boolean (`"omitted"` and `"updates"` both suppress the thinking text, `"summarized"` keeps it), so the cross-dialect egresses that read only the boolean are unaffected: Gemini still excludes thoughts, the openai-compat reasoning object still carries no `display` key, and Bedrock Converse forwards the literal verbatim (see Changed). An unmodeled value logs one DEBUG line. Only a NON-string `display` keeps a local 400, since the carrier is a string and there is no shape to forward. Absent still stays absent (the upstream default is model-dependent). Pinned end to end on the non-streaming, streaming, and `count_tokens` paths.
  - **The OpenAI Responses ingress forwards `reasoning.summary`, `reasoning.context`, and `reasoning.mode` unvalidated.** The two closed-enum allowlists and the `mode` string-type check blocked values a newer upstream accepts; the existing remainder mechanism already carries those sub-keys verbatim onto the Responses wire, so the validators only ever converted an upstream verdict into an earlier local one. Structural checks on that lane are unchanged -- a `previous_response_id` against this stateless proxy is still a 400.
  - **`cache_control` marker vocabulary forwards verbatim.** The `type` and `ttl` allowlists (`ephemeral`; `5m` / `1h`) are gone. The validator keeps exactly what routectl needs to inject its own breakpoints correctly: the 4-breakpoint cap and the longest-TTL-first ordering rule, both still local errors. An unrecognized `ttl` is not `5m`, so it never opens the 1h-after-5m ordering window.
  - **An unrecognized `thinking.type` now says so.** The catch-all arm silently discarded the whole `thinking` object; it now emits one sanitized WARN naming the value. The object itself now reaches Anthropic-shape upstreams; see the Changed entry on unrecognized `thinking.type`.

  Upstream rejections on these fields are unchanged in every other respect: a value the upstream really does refuse now earns the upstream's own message, surfaced without rewording, instead of routectl's guess at it.

- **Mid-conversation `role: "system"` messages are no longer silently discarded on the Anthropic egress.** `role: "system"` inside `messages[]` is a supported Anthropic Messages API shape (position-gated: such a turn must precede an `assistant` turn or end the array; also model-gated), and a client can legitimately send one alongside a populated top-level `system`. The egress computed the wire `system` field as "canonical `system`, else lift the `Role::System` messages" -- an exclusive choice -- while the per-role message walk dropped those same turns on the premise the lift had consumed them. With both present, neither branch owned them: every mid-conversation system turn was deleted, with no log line. Observed on live traffic as 43 of 43 turns removed from one request, taking with them operator interjections, hook context, editor diagnostics, and subagent notifications. Those turns now ride the wire in place at their original index, preserving per-block `cache_control` and forward-compat block types, whenever a canonical top-level `system` is shipping. When no canonical `system` is present the legacy lift still runs and still consumes them, unchanged -- that path is what keeps a direct library caller without an ingress working, and its leading system message would be in an illegal wire position if forwarded. Nothing is capability-gated, probed, or configurable: a model that rejects the shape produces a 400 that surfaces unmodified, because the silent deletion WAS the 400-avoidance mechanism and avoiding the error is what destroyed the content.

  The Claude Code billing/attribution strip now covers the forwarded path too, per block and across every carrier a block can hold text in (typed text, an unrecognized block type carrying a `text` field, a document source), using the same predicate the canonical `system` and the legacy lift already run -- forwarding verbatim would otherwise have opened a third way for the client fingerprint to reach a non-Anthropic host. A turn left with nothing after the strip is skipped, a turn left with only blank text is a local error naming its index, and the stripped-block count rides one contents-free DEBUG line per request. On the OAuth non-Claude-Code lane the identity cloak also relocates forwarded system turns into its `<system-reminder>` block (and drops them under `strict_mode`), the same treatment it already gave the `system` field, so client system directives do not reach the upstream by the new carrier either.

  Three adjacent gaps closed in the same pass, each of which would have made the fix silently incomplete: a system turn now ENDS a run of tool-result turns rather than being transparent inside one (folding across it would delete or reorder it); the unsigned-thinking whole-turn drop now refuses to remove an `assistant` turn that a FORWARDED system turn legally precedes (lanes that ship no system turn keep their previous drop behavior), since routectl minting a position 400 is the same class of defect; and the message walk carries an accounted-identity ledger -- every position it consumes is either emitted or charged to one named lossy term (the tool-run fold, the lift, the billing strip), and a shortfall fails the request instead of deleting content. One DEBUG line per request reports how many turns were forwarded, and a 4xx whose body carried them earns a dedicated WARN naming the count and the resolved mid-conversation-system beta decision on every auth lane.

  Bedrock InvokeModel is explicitly excluded (support for the shape is not established on that lane) and Bedrock Converse is untouched. A forwarded turn whose content is empty is a local error naming the message index rather than an opaque upstream rejection.

  **Cache-cost note for the first request after upgrade:** a forwarded system turn changes the bytes of the cached prefix from its position onward, so the first request of each in-flight long session re-writes rather than reads that prefix -- a one-time cost of roughly the 1-hour cache-write rate over the affected prefix instead of the cache-read rate. Steady state returns immediately after: forwarded turns are stable bytes and new ones append at the tail. A cache-write spike right after deploy has this explanation.

- **The baked catalog's context-window and base-rate fields no longer trust a hand-maintained ambiguity flag that could drift from the vendored snapshots.** `context_ambiguous` and `price_ambiguous` gated a selector's window and price fields the same way `max_output_tokens`'s `output_ambiguous` already did, but only the ceiling flag was ever mechanically checked against the snapshots: the window and price derivations read a single representative litellm entry, so a curated flag could go stale without a test catching it. Both fields now derive from EVERY litellm entry a selector's glob spans agreeing (unanimity or nothing baked, the same rule the ceiling already followed, matched case-sensitively -- an entry whose model id differs from the glob only in case is not among those spanned), and a weld test fails generation when a flag disagrees with that mechanical verdict. Several selectors' baked figures moved as a result: `openai-responses:*`, `openai-compat:deepseek-*`, `deepseek-v4-pro*`, `gemini-*`, `kimi-*`, and `moonshot-*` lose their baked context window (the glob's gateway re-listings genuinely disagree on it), `deepseek-v4-pro*` also loses its baked base rates (an azure/fireworks re-listing prices roughly 4x the direct listing), and two Anthropic Claude 4.5 selectors already known to disagree on their output ceiling across gov-region re-listings (`claude-sonnet-4-5*` on both `anthropic-api` and `bedrock`, plus `bedrock:anthropic.claude-haiku-4-5*`) now also lose their baked base rates for the same reason. An operator relying on any of these figures should confirm the actual value via `[models.X]` or the catalog overlay; CATALOG_VERSION moves 3 -> 4.

- **`GET /v1/models` no longer advertises a context window for a model whose OAuth credential is absent.** Resolving an `oauth://` credential ref never touches the credential store at build time (deliberate -- it is what lets a live token refresh apply without a daemon restart), so a model whose managed credential was missing built cleanly, landed in the installed table, and reported `context_length` exactly like a healthy route -- then 503'd on every dispatch. Discovery now cross-checks the first chain target's oauth id against the same activation inventory the auto-activation feature already computes, and omits `context_length` (never a new field) when that id is not currently activated -- byte-identical to the existing build-failure omission shape. A `routectl login` for the missing credential clears the suppression on the next credentials-change reload, no restart required. Like the existing `context_length` read this reports the first configured chain target only, so a model whose first target is broken but whose fallback is healthy still has its window suppressed even though it would serve.

- **A live antigravity/gemini seat no longer reports as an unroutable false alarm.** `compute_activation` gated the OAuth id -> config kind mapping on baked-catalog membership, so an `antigravity` token (which maps to the `gemini` config kind) could never reach `Activated` even when present: `gemini` is fully configurable and served by the overlay lookup regardless of baked rows, but carries zero baked rows itself, so the gate always won and doctor's inventory panel raised a WARN about an unusable route -- on a lane that actually serves traffic -- whenever a configured alias referenced it. The gate now checks the config-kind vocabulary instead of baked-catalog membership, matching what the overlay lookup itself asks; a present token now activates like any other config-nameable kind, and the entry stays `Unresolved(NotCataloged)` only when no config in this build can name the id's kind at all (today: the `gemini` cargo feature off). `provider add`'s conventional-env-var offer for `--kind gemini` gets the same predicate fix and now offers `GEMINI_API_KEY` when it resolves, instead of always prompting.

- **A `[capability] essential` entry rejected for an invisible defect is now diagnosable.** `essential = ["advisor "]` (trailing space) or `["Advisor"]` are correctly rejected as unknown capability keys, but the error rendered the key in plain prose -- so the operator read a message naming what looked exactly like the key they had written, with no way to see the difference. The rejection now quotes the raw key inside backticks so surrounding whitespace shows its bounds, and adds a `did you mean `advisor`?` hint naming which difference it is (surrounding whitespace, letter case, or both) when the entry is an exact whitespace-or-case variant of an accepted key. The suggestion is exact-after-normalization rather than fuzzy on purpose: it fires only for defects the operator cannot see, so a plain misspelling -- already visible in the quoted key -- gets no guess instead of a possibly-wrong one. The candidate set is the validator's own accept set, so `reasoning_replay`, which that list deliberately does not govern, can never be offered as a fix. Which configs load is unchanged.

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

- **openai-compat never sends `mcp_servers`.** Its entries can carry an authorization token issued for a remote MCP server, and with client extras of the Chat dialect now forwarding, a Chat client's `mcp_servers` could reach an openai-compat or Mantle OpenAI-compatible host. The field is now removed from every openai-compat body whoever wrote it, operator `payload_extras` included, as Bedrock already does, and counted once per request as the `credential_bearing_field_withheld` policy action; the `debug` line names the field, never its value.

- **OpenAI Responses hosted-MCP tools no longer cross to an Anthropic-shaped upstream.** A top-level `{"type":"mcp", ...}` tool, which can carry the caller's remote-server `authorization` and `headers`, was forwarded verbatim as an Anthropic builtin when a Responses request reached an anthropic-api provider (including an Anthropic-compatible proxy) or Bedrock Invoke. Those egresses now withhold it, turn a `tool_choice` naming it (`{"type":"mcp", ...}`) into `{"type":"auto"}` (or omit it when no tool remains), and log one WARN per request naming only the tool type and counts, never the label, tool name, URL, or credentials; on anthropic-api each such request bumps the `(anthropic, hosted_mcp_tool_withheld)` policy-action counter once. The remaining tools still ship, Anthropic's own server tools (and its `mcp_toolset`) still forward verbatim, Bedrock Converse already dropped the tool and its `tool_choice`, and an OpenAI Responses upstream still receives it unchanged.
- **Body traces and upstream error excerpts redact credential-valued keys.** The TRACE body lines (ingress, outgoing, upstream success, egress), the DEBUG upstream error body, each provider's WARN `body_excerpt`, and the bounded `body_excerpt` an upstream error carries into retry / fallback WARN lines printed credentials verbatim -- an Anthropic `mcp_servers[].authorization_token`, a Responses MCP tool's `authorization` or `headers` map, or an `api_key` a validation error echoed back -- because prompt redaction covers prompt content, not credential fields, and with `ROUTECTL_LOG_REDACT_PROMPTS` off nothing covered them. The value of any body key whose lowercased name contains `authorization`, `authentication`, `api_key`, `api-key`, `apikey`, `private_key`, `private-key`, `privatekey`, `secret`, `password`, or `bearer`, or ends in `token` but not `tokens`, now renders as `<redacted len=N>` (a structured value as `{"redacted":true}`) at any depth and in both knob states; inside a `headers` object the header-trace rule also applies, so `Cookie` and `*-Key` headers are redacted as well. Token-count fields (`max_tokens`, `budget_tokens`, `max_output_tokens`, usage `input_tokens` / `output_tokens`, Gemini `promptTokenCount`) stay visible. A JSON upstream error body of at most 64 KiB is redacted before any excerpt is cut (prompt content too when the knob is on); on the DEBUG line and in both WARN excerpts, a JSON-looking body (a leading byte-order mark is ignored) that is larger or no longer parses renders as `(json body, N bytes, not excerpted)`. Only logged and error-message copies change; the bytes sent upstream are unchanged. Not detected: a credential inside a URL query string (for example in `server_url`), a credential inside the value of a non-credential key, and non-JSON upstream error bodies. See [docs/LOGGING.md](docs/LOGGING.md) "Redaction".
- **Cloak `sensitive_words` is bounded to 32 entries of at most 32 case-folded characters.** The per-request scan's worst case grows with how many configured words share a first letter times their length, against request text the client controls, so an unbounded list could hold an async worker for minutes on one large body. A list over either bound now fails `config check`, `serve` startup, and hot reload with an error naming the provider and the bound only (never the word); a provider built directly as a library refuses each cloaked request with a config error before egress. Lists within the bounds behave exactly as before. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#cloak-sensitive_words-anthropic-api-provider-flag) for the derivation and the reproducible benchmark.
- **TLS library updated to rustls 0.23.45 ([RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285.html)).** rustls 0.23.13 through 0.23.44 accepted TLS 1.3 handshake messages sent at the wrong encryption level when they followed a key-changing message in the same record, instead of terminating the connection as RFC 8446 section 5.1 requires. The handshake transcript stays authenticated, so a handshake cannot be altered or completed this way. Every outbound provider connection and the MITM listener now link the fixed release; no configuration change is needed.
- **CI and release builds always compile the vendored AWS-LC.** Both workflows set `AWS_LC_SYS_USE_SYSTEM=0`, so the `aws-lc-sys` build script no longer probes `OPENSSL_DIR` or pkg-config for a system AWS-LC; the crypto backend a release binary links no longer depends on what the runner image has installed. Every job that checks out the repository now also fails, right after checkout, when the runner environment carries any `AWS_LC_SYS_*` build override other than the two pins -- including target-suffixed names, which aws-lc-sys reads ahead of the workflow pin.
- **Gemini ingress forwarding now respects request provenance.** HTTP ingress traffic forwards only configured operator payload extras, while trusted library callers retain explicit extras. Client metadata and omitted request fields are filtered and counted without logging values.
- **AWS regions that derive an endpoint host must be canonical region identifiers.** Native Bedrock and every Bedrock mantle lane built the endpoint host by interpolating the configured `region` unchecked, so a value carrying a dot or URL structure (`@`, `/`, `:`, `#`, `?`) could change the parsed host and send the signed request and its body somewhere other than the AWS endpoint. One shared parser now accepts only lowercase region identifiers of at most 32 bytes in the commercial, GovCloud or China partition; it runs at config load (`region` and `bedrock_mantle.region`, on every validation path) and again inside the endpoint builders, the credential resolver and the request signer, so a provider constructed directly as a library bypassing config validation is still refused before any credential is resolved, any request is signed, or any connection is opened. China-partition regions now resolve to their own DNS suffixes (`amazonaws.com.cn` for `bedrock-runtime`, `api.amazonwebservices.com.cn` for `bedrock-mantle`). BREAKING for library callers: the `bedrock::endpoint` and `mantle` URL builders now return `Result<String>`, and `BedrockProvider::new` returns `Result<BedrockProvider>`, refusing a non-canonical region at construction. The native Bedrock stream decoders also stop hardcoding the vendor-origin usage flag: it is true only when the endpoint that answered is exactly the `bedrock-runtime` host the validated region derives (`https`, no userinfo or explicit port), and false otherwise.
- **MITM forwarding now applies the configured transport policy to each request path.** Redirect and concurrency limits are unchanged.
- **Loopback provider traffic no longer follows an environment proxy.** Provider clients inherited reqwest's system-proxy discovery for every host, so with `HTTP_PROXY` or `ALL_PROXY` exported and no matching `NO_PROXY`, a request to a loopback cleartext base URL -- API key and prompt content included -- was sent to the configured proxy in the clear. Each provider client (every egress lane, the mantle lanes, and the `doctor` reachability probe) now fixes its proxy policy at construction from its base URL's parsed host: a loopback host (`localhost`, `127.0.0.0/8`, `::1`, or IPv4-mapped loopback) is always dialed directly over `http://` and `https://`, and every other host keeps honoring `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` exactly as before. The loopback classification matches the cleartext `http://` admission check below, so DNS lookalikes and the IPv4-compatible form are not loopback.
- **Cleartext `http://` base URLs are exempt only for true loopback hosts.** The exemption matched a `127.` text prefix, so a DNS name such as `127.example.com` or a wildcard-DNS form like `127.0.0.1.nip.io` -- which can resolve to any address -- was allowed to carry API keys and prompt content in the clear. The check now classifies the parsed host: IPv4 loopback literals, native `::1`, IPv4-mapped loopback (`::ffff:127.0.0.1`), and the exact name `localhost` are accepted; every other DNS name (including `localhost.` and `*.localhost`) and the deprecated IPv4-compatible form (`::127.0.0.1`) now require `https://`.
- **`config migrate` no longer merges distinct OAuth egresses into one pool.** Phase 2 keyed the materialized pool purely on the provider family, so two or more `[providers.X]` entries carrying a bare `oauth://<family>` ref -- including entries with deliberately different `base_url` values -- collapsed into ONE `[pools.<family>]` block, and every model naming either entry was repointed at it: each account's OAuth bearer would then dispatch to every egress host in the merged set. The same path could also add members to a hand-authored pinned pool (`accepts_new_logins = false`). Both are now typed fail-closed refusals that name the offending entries and leave `config.toml` byte-identical: pin all but one entry with `oauth://<family>#<label>`, or write the pool blocks by hand. The pure v3 -> v4 rung refuses on the matching shape too (two entries deriving one pool name, where the second's relocated `seat_selection` silently overwrote the first's).
- **The managed Anthropic subscription token is contained to the Anthropic API host.** An `oauth://anthropic` or `oauth://anthropic#<label>` `api_key_ref` was accepted on any provider kind and any `base_url`, so a mistyped or hostile endpoint (a gateway, a lookalike host, a loopback tunnel) could receive the subscription bearer. Such a ref is now accepted only on an `anthropic-api` provider with `credential_source = "own"`, no `bedrock_mantle`, and a `base_url` that is omitted or `https://` on exactly `api.anthropic.com`; every other kind and host is rejected, loopback included. The check runs in the shared config validation (`config check`, `serve` startup, hot reload) and again in the provider factory before any secret is resolved, so library callers that bypass validation are refused too. Symptom: startup and `config check` fail with an error naming the provider (the `base_url` and ref are withheld from the text); a hot reload logs `config reload failed; keeping previous config` and keeps the previous config. Fix: give a gateway or passthrough provider its own static `env://` or `file://` credential -- never copy the subscription token -- or point the provider at the default Anthropic URL. There is no override flag; static credentials on any host are unchanged.
- **The managed Codex and Antigravity subscription tokens are contained to their first-party hosts.** An `oauth://codex[#<label>]` or `oauth://antigravity[#<label>]` reference was accepted on any provider kind and any `base_url`, so a gateway, a lookalike host, or a loopback tunnel could receive the refreshing subscription bearer. `oauth://codex` is now accepted only on an `openai-responses` provider with `auth_kind = "chatgpt-oauth"`, no `bedrock_mantle`, and a `base_url` that is omitted or `https://` on exactly `chatgpt.com`; `oauth://antigravity` only on a `gemini` provider with `auth_mode = "cloud-code"` and a `base_url` that is omitted or `https://` on exactly `cloudcode-pa.googleapis.com` or `daily-cloudcode-pa.googleapis.com`. A `user:pass@` prefix, an explicit non-default port, a trailing dot, `http://`, and loopback are rejected for both. The check runs in the shared config validation and again in the provider factory before any secret is resolved. Symptom: startup and `config check` fail with an error naming the provider and the rule (the `base_url`, ref, and label are withheld); a hot reload keeps the previous config. Fix: point the entry at its default host, or give a staging shard, mirror, or mock its own static `env://` or `file://` credential. A cloud-code entry pinned to an enterprise mirror is no longer reachable, since that mode takes no static credential. There is no override flag.
- **Router error-path and quota-header log fields sanitized.** Five `%`-rendered tracing fields still carried unfiltered caller or upstream bytes: the ingress `NotImplemented` arm's provider and detail, the router's `count_tokens` and capability-filter WARNs (requested alias plus the derived feature list), and the five Anthropic subscription-quota header values (`to_str` rejects CR/LF/ESC but passes tab and caps nothing). All now route through `sanitize_for_log`, and the Bedrock Converse billing-block WARN no longer files the caller's requested model under a `provider` field.
- **Operator-config-derived pool and seat surfaces sanitized.** The pool-omission WARN, the zero-usable-pool boot / reload refusal (and the reload rejection that re-logs it), the doctor config-validation findings, and the doctor auth and orphan-seat findings all rendered operator-written table keys and seat labels with no control-char filtering, so a key or `login --label` value bearing a newline plus an ANSI sequence could forge a whole log record or a fabricated doctor finding line. All now route through `sanitize_for_log`; the config-validation render point covers every validator at once. The seat-report sanitizer additionally neutralizes `=` and the backtick, the two remaining characters the pool sentence's own grammar uses.
- **Operator key names are control-character filtered in `config migrate` output.** The migrate refusal messages, the retired-key error, and the `--dry-run` summary of removed keys rendered operator-written table keys verbatim, so a quoted key carrying a newline or an ANSI sequence could forge a line of that output. Those names now go through the same filter as the other config-derived surfaces.
- **Client beta flags holding a byte outside visible ASCII are dropped at ingress.** A flag containing a control byte, DEL, a space, or any non-ASCII character is discarded before routing, so a body-sourced value can no longer reach an egress that ships it on the upstream `anthropic-beta` header, where the header builder refuses it and the request fails as a transport error against the seat instead of a client error. One WARN per request reports how many flags were dropped and the longest dropped length, never the values.
- **Every production log record stays on one line, whatever a field or message carries.** The daemon's log sink now escapes every control, format, line/paragraph separator, and default-ignorable character (zero-width, bidi, soft hyphen, BOM, tag characters) into visible ASCII (`\n`, `\r`, `\x1b`, `\u{202e}`, ...) in event fields, span fields, message text, and later span updates, so a caller- or upstream-supplied `\n` can no longer forge a record and an ESC or bidi override can no longer rewrite what an operator sees -- regardless of which macro form or data path produced the value. Clean printable records are byte-identical to before, and a field whose own rendering fails writes its escaped partial text plus a fixed marker instead of falling back to raw stderr. The call-site scanner that tried to prove each `%` field was sanitized is replaced by a commit-gate inventory that fails unless the escaping sink is the only production subscriber; call-site sanitizers remain for length caps and redaction.
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
