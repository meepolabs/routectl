# Logging

This document covers routectl's logging surface: env filter, default
level, recipes, request correlation, triage modes for full body
inspection, and the prompt-redaction knob. For TOML configuration of
providers, models, and the runtime, see [CONFIGURATION.md](CONFIGURATION.md).

The first half is for operators (filtering, triage, redaction); the
"Event catalog" second half is the per-event field reference.

- [Env filter and default level](#env-filter-and-default-level)
- [Recipes](#recipes)
- [Request correlation](#request-correlation)
- [Triage recipes (full bodies on demand)](#triage-recipes-full-bodies-on-demand)
- [Redaction](#redaction)
- [What's never logged](#whats-never-logged)
- [Trace-level surfaces](#trace-level-surfaces)
- Event catalog:
  [SSE forward-compat](#anthropic-sse-forward-compat-observability),
  [auth failures](#auth-failure-log-shapes),
  [usage accounting](#usage-accounting-log-shapes),
  [config-edit audit](#config-edit-audit-shape),
  [prompt-cache auto-emission](#prompt-cache-auto-emission-log-shapes),
  [activation inventory](#auto-activation-inventory-audit-events),
  [context reduction](#context-reduction-log-shapes),
  [config-reload transitions](#config-reload-transition-fields),
  [stream first-activity](#stream-first-activity-mark),
  [capability intelligence](#capability-intelligence-events)

## Env filter and default level

routectl uses `tracing` with the env filter `ROUTECTL_LOG` (NOT the
default `RUST_LOG`, since we don't want stray `RUST_LOG=debug` exports
turning routectl into a firehose).

Default level is `info`. Every log line carries the module path
(`routectl_router::router`, `routectl_providers::bedrock`, etc.) and,
inside an HTTP request, the `request_id` field for correlation across
fallback hops.

## Recipes

```bash
# Full debug across all routectl crates.
ROUTECTL_LOG=routectl=debug,routectl_providers=debug,routectl_router=debug \
  ./routectl serve

# Bedrock-only deep dive (SigV4 inputs + eventstream frames).
ROUTECTL_LOG=routectl=info,routectl_providers::bedrock=trace ./routectl serve

# Auth tracing only (secret resolution + credential failures + listener
# rejections + upstream 401/403).
ROUTECTL_LOG=routectl_auth=warn,routectl_providers::bedrock::auth=warn,\
routectl_providers::bedrock::signing=warn,\
routectl_cli::server::auth=warn ./routectl serve

# Quiet -- only warnings and errors.
ROUTECTL_LOG=warn ./routectl serve
```

## Request correlation

Every request gets a `request_id`. Either supply your own via the
`x-request-id` header (echoed back on the response so your client logs
match) or routectl mints a `Uuid::now_v7()` (sortable by time). All
log lines emitted while processing the request inherit `request_id` as
a span field, so:

```bash
ROUTECTL_LOG=info ./routectl serve 2>&1 | grep request_id=probe-1
```

shows every event for one specific request: ingress parse, alias
resolution, fallback hops, retry attempts, upstream calls, response
shape, errors.

## Triage recipes (full bodies on demand)

When `body_excerpt=...` in a WARN line isn't enough -- typically when an
upstream returns a generic `400 "request not valid"` and you need to
see WHICH wire field tripped it -- flip the log level. The output
includes ingress body, outgoing egress body, and the full upstream
error body, all carrying the same `request_id` so a single grep
correlates them:

```bash
# Full upstream error bodies (4 KB cap, debug):
ROUTECTL_LOG=routectl=debug ./routectl serve

# Also outgoing + ingress bodies (16 KB cap, trace):
ROUTECTL_LOG=routectl=trace ./routectl serve

# Trace one specific request end-to-end:
ROUTECTL_LOG=routectl=trace ./routectl serve 2>&1 | grep request_id=<id>

# Which Bedrock-Invoke beta flags are getting filtered (operator
# suspects AWS allowlist drift):
ROUTECTL_LOG=routectl_providers::bedrock=debug ./routectl serve 2>&1 \
  | grep "dropping beta flag"
```

What you get at debug:
- Existing `body_excerpt=...` WARN on every 4xx/5xx (256-char
  truncated; greppable from any tracing subscriber that records
  WARN-level events)
- New `body=...` DEBUG with the full upstream error body (4 KB cap,
  HTML-collapsed)

What you get at trace, additionally (full 4-direction visibility):
- `body=...` TRACE `"ingress request body"` -- the body the client
  sent on `/v1/chat/completions` or `/v1/messages` (16 KB cap, fields
  `ingress=openai|anthropic`).
- `body=...` TRACE `"outgoing request body"` -- the JSON body routectl
  sent to the upstream (16 KB cap, fields
  `provider_kind=openai-compat|anthropic|bedrock-invoke|bedrock-converse|openai-responses`,
  `provider=<id>`).
- `body=...` TRACE `"upstream success body"` -- the deserialized 2xx
  body the upstream returned, traced BEFORE routectl's normalization
  rewrites it (16 KB cap; same `provider_kind` / `provider` fields).
  4xx/5xx error bodies stay on the existing DEBUG path.
- `body=...` TRACE `"egress response body"` -- what the client
  actually receives, traced AFTER canonical -> wire serialization
  (16 KB cap, field `ingress=openai|anthropic`). Single call site in
  `routectl-cli/src/handlers/ingress_handle.rs` covers both ingresses.
- TRACE `"stream summary"` lines on streaming completion: one per
  direction (`direction=upstream` from the provider-side wrapper,
  `direction=egress` from the ingress-side render loop). Carries
  `chunks=<N>`, `finish_reason=<...>`, `prompt_tokens`,
  `completion_tokens`, `total_tokens`. Streams DO NOT emit per-chunk
  body traces -- the per-chunk firehose floods the log without adding
  signal beyond the summary.

The 16 KB trace body cap is the DEFAULT. Override it with
`ROUTECTL_TRACE_BODY_BYTES=<N>` (env, read once at first trace) or
`[log] trace_body_bytes = <N>` (config, applied at startup). The
resolved value is announced at boot. The 4 KB debug excerpt cap is fixed.

Sensitivity caveat: bodies contain user prompts AND assistant outputs
at TRACE. Leave `ROUTECTL_LOG` at the default `info` level in
production. Only flip to debug/trace during active triage and prefer
redirecting the output to a file (`./routectl serve 2>/tmp/triage.log`)
rather than tailing live.

## Redaction

For sensitive environments where TRACE is needed but raw prompts are
not OK to disk, set `ROUTECTL_LOG_REDACT_PROMPTS=1` BEFORE launching
routectl. The redactor walks every traced body and replaces known
prompt-bearing fields (text blocks, system, instructions, tool_use
input, function_call arguments, refusal blocks, image source data,
image_url data URIs, reasoning-replay carry blobs
(`encrypted_content` and the `redacted_thinking` `data` blob), Bedrock
Converse `toolUse.input` and
`toolResult.content[*].json`) with `<redacted len=N>` placeholders
while preserving structural fields (model, tools, sampling params,
finish_reason, usage). Best-effort: unknown wire shapes (a new
Anthropic content-block type, a new OpenAI Responses output kind)
can still leak. The env var is read once on first traced body --
flipping it after the first trace fires has no effect; the server
emits a one-shot `info` line at boot reporting the resolved value
(`redact_prompts=true|false`) so operators can confirm the setting
took effect.

The knob also governs one field OUTSIDE the traced bodies. The
foreign-format reasoning WARN's `skipped_formats` names the `format` tag
of each reasoning detail the Anthropic translator could not echo. That
tag is caller-supplied, so the knob applies to it. The split:

- Tag in routectl's recognized vocabulary (`anthropic-claude-v1` and the
  Responses-family tags `openai-responses-v1`, `codex-oauth`,
  `openai-apikey`, `bedrock-mantle`): echoes literally in BOTH knob
  states. It is protocol vocabulary routectl itself defines, carries no
  caller bytes, and naming which dialect arrived is the field's whole
  diagnostic value.
- Tag outside that vocabulary: echoes literally with the knob OFF
  (sanitized, 256-char capped, 8 distinct values sampled), and renders as
  the literal `<unrecognized>` with the knob ON. Every unknown tag in a
  request collapses to that one placeholder, so the field's size is
  bounded regardless of how many distinct unknown tags arrive.
- A detail with no `format` at all renders `<none>` in both states.

To read the literal of a tag routectl does not recognize yet (the
forward-compat discovery case), run with the knob off.

Two known residual leaks even with the knob ON:
- `<redacted len=N>` reveals the char count of the original content.
  Short fixed-vocabulary prompts (e.g. "yes" / "no" tool confirms)
  are disambiguable by length alone. Treat redacted traces as a
  length-leaking side channel.
- Non-JSON 4xx/5xx upstream error bodies (`debug_upstream_error_body` at
  DEBUG level) are NOT redacted -- a non-JSON error body has no
  structure for `redact_prompts_in` to walk, so it goes through the
  HTML-collapse + control-char-strip path only and may echo back
  portions of the request as raw text. A PARSEABLE JSON error body IS
  routed through `redact_prompts_in` (the same entry point the trace
  helpers use), so it collapses under the knob the same way a success
  body does. Operators flipping DEBUG (not TRACE) for triage on a
  sensitive environment should be aware of the non-JSON gap.

```bash
# Redacted triage. All four trace directions still fire; user content
# replaced with `<redacted len=N>` markers; model/tools/usage/
# finish_reason intact for diagnosis.
ROUTECTL_LOG=routectl=trace ROUTECTL_LOG_REDACT_PROMPTS=1 \
  ./routectl serve 2>/tmp/triage.log
```

### Reasoning-replay degradation WARN

When a request carries reasoning-replay artifacts onto a lane that
rejects them, the router strips them and re-dispatches the same target
once (the fixed strip-repair branch). Each such request emits EXACTLY
ONE aggregated WARN at resolution -- `"reasoning_replay_degraded"` -- and
none when nothing degraded. The line carries a CLOSED SET of tokens
only, never the artifact bytes, a reasoning item id, any hash/digest,
the session key, or the upstream body:

| Field | Meaning |
|---|---|
| `action` | What the router did (`strip_repair`) |
| `target_lane` | The lane stripped against (`codex` / `mantle` / `gray`) |
| `state_key` | Sanitized `[providers]` state key of the repaired target |
| `source_schemes` | Distinct source schemes of the stripped artifacts, comma-joined |
| `reason` | Why the strip fired (`upstream_replay_rejection`) |
| `artifact_count` | Count of non-portable artifacts stripped |
| `repair_attempted` | The strip-repair branch fired |
| `repair_succeeded` | The stripped re-dispatch reached success / first chunk |
| `learned` | The confirmed negative was persisted to the learned registry |

Correlate across the retry/fallback hops with the request span's
`request_id`. A classified replay rejection is converted to a body-free
structured error before it reaches the generic retry/fallback logs, so
the rejection envelope never renders into an `error = ?e` line.

Two verdict-lifecycle lines accompany it at INFO, both carrying
sanitized keys only: `"replay_learn_commit"` when a stripped retry
confirms the negative, and `"replay_learn_clear"` when a later carry
succeeds and lifts a resident one. `state_key` is sanitized at both
sites; `capability_key` is the closed lane-scheme token, never upstream
text.

A commit or a clear can instead be REFUSED by the generation barrier
guarding the learned-capability registry, in which case no verdict
event is produced and the line is a content-free DEBUG rather than the
INFO above: `"replay_learn_stale"` / `"replay_clear_stale"` (the carry
predates the live capability generation), `"replay_learn_reserved"` /
`"replay_clear_reserved"` (an operator purge holds the pair's lease),
and `"replay_learn_exhausted"` / `"replay_clear_exhausted"` (the
incarnation sequence is exhausted). All six carry only the same
sanitized `state_key` and `capability_key` fields as the settling lines
-- no additional detail, since a refusal is definitionally a no-op on
the registry.

### Envelope-field repair WARN

When an upstream rejects a request by naming a WIRE FIELD PATH the router
has a grounded repair for, the router DROPS that field and re-dispatches
the same target once (the reactive L0 field repair). Each such request
emits EXACTLY ONE aggregated WARN at resolution --
`"envelope_field_repaired"` -- and none when no repair fired. It fires on
all three dispatch walks (`complete`, `stream` pre-content, and
`count_tokens`), once per request rather than once per walk.

The line is content-free by construction. `field_path` is the repair
table's OWN path literal, not text read out of the rejection, so no
upstream bytes can reach the line; the rejected field's VALUE, the
upstream body, and the session key are never carried:

| Field | Meaning |
|---|---|
| `action` | What the router did (`field_drop_repair`) |
| `state_key` | Sanitized `[providers]` state key of the repaired target |
| `field_path` | The closed-table qualified dotted path that was repaired |
| `reason` | Why the repair fired (`upstream_field_rejection`) |
| `repair_attempted` | The repair arm fired |
| `repair_succeeded` | The repaired re-dispatch reached success / first chunk / a count |
| `learned` | The confirmed verdict was persisted to the learned registry |

Two verdict-lifecycle lines accompany it at INFO, both carrying
normalized keys only: `"field_verdict_commit"` when a repaired retry
confirms a verdict, and `"field_verdict_clear"` when an accepted request
drops a resident one. `state_key` is sanitized at both sites;
`capability_key` is the closed repair-table path literal, never upstream
text, so it is logged as-is for consistency with the WARN line above.

A commit or a clear can instead be REFUSED by the generation barrier
guarding the learned-capability registry, in which case no verdict
event is produced and the line is a content-free DEBUG rather than the
INFO above: `"field_verdict_commit_stale"` /
`"field_verdict_clear_stale"` (the admission predates the live
capability generation), `"field_verdict_commit_reserved"` /
`"field_verdict_clear_reserved"` (an operator purge holds the key's
lease), and `"field_verdict_commit_exhausted"` /
`"field_verdict_clear_exhausted"` (the incarnation sequence is
exhausted). All six carry only the same sanitized `state_key` and
`capability_key` fields as the settling lines -- no additional detail,
since a refusal is definitionally a no-op on the registry.

Three counters ride the router metrics snapshot (DEBUG, target
`routectl_router::router::metrics`) so the arm is answerable without a
log level: `rc_field_repair_attempted_total`,
`rc_field_repair_succeeded_total`, and `rc_field_verdicts_learned_total`.
All three, plus the pre-flight and parser-blindness counters, also ride
the INFO snapshot below -- the DEBUG snapshot is not readable at a live
log level, so an observability floor satisfied only there would be a
floor no operator can read.
Read them as a pair -- a rising attempted count with a flat succeeded
count means the dropped field was not what the upstream objected to.

All three are ZERO on a current build, and that is expected rather than a
fault: no rejection parser is grounded yet, so nothing on real traffic
resolves a field path and the repair arm never fires. A nonzero count is
itself the signal that the arm has become reachable.

One SETTLEMENT limitation to read alongside the counters, because it
makes two token-count call paths behave differently on purpose. The
verdict lifecycle needs the caller to drain the dispatch metadata's
learned/cleared rows to the capability-event ledger: a verdict persisted
in memory whose row never reached the ledger would be invisible to the
next warm rebuild, and a verdict CLEARED without a row would be
resurrected by it. So the result-only `count_tokens` entry point --
which hands its caller no metadata -- neither learns nor clears. It
still repairs, so the count it returns describes a body the upstream
would accept; it simply records no verdict either way, and
`rc_field_verdicts_learned_total` never moves for such a call. The
`/v1/messages/count_tokens` endpoint uses the metadata-carrying variant
and drains it, so operator-visible traffic does settle. The two messages
walks always settle.

Two target shapes are excluded from the repair ENTIRELY -- not merely
from minting -- so neither appears in any of the counters and neither
has its request body touched: a target whose `base_url` names a local
destination (the rejection is not attributable to the wire format that
hop was configured with), and a target authenticating with a FORWARDED
client credential (routectl owns no credential there, and one client's
rejection would otherwise mint a verdict that steers every other client
through that entry). Both refusals apply on every walk, including the
non-settling result-only token count: a target this stage cannot
attribute a rejection to is one it must not act on, not just one it must
not learn from.

### Envelope-field pre-flight decision DEBUG + WARN

The PROACTIVE counterpart of the repair WARN above. When a resident learned
verdict is settled and confirmed, the router rewrites the request BEFORE
dispatch rather than waiting for the upstream to reject it again, and every
such decision is reported. Two tiers, because they carry different
information:

- **DEBUG, one line per DECISION**, acting or not, event
  `"envelope_field_preflight"`. A fail-open is the routine case -- most
  requests on most targets have no eligible verdict -- so a WARN per decision
  would make an ordinary request look faulty and bury the reportable one.
- **WARN, exactly ONE per REQUEST**, and only when at least one decision
  actually acted. A request that rewrote a client's request before dispatch is
  the reportable event; one that changed nothing is not.

A decision is per CLOSED-TABLE ROW per target, not per target: one request can
carry rows of two transform classes at once and each clears its own gates, so a
per-target record would report only one of two facts. A fallback chain
therefore emits one DEBUG per row per target it planned.

| Field | Meaning |
|---|---|
| `event` | `envelope_field_preflight` on both tiers |
| `action` | `field_preflight_drop`, attached ONLY to a record that acted -- labelling a fail-open with it would name an action the walk did not take |
| `acted` | DEBUG only: whether this decision rewrote the request |
| `state_key` | Sanitized `[providers]` state key of the target the decision was planned for |
| `field_path` | The closed-table qualified dotted path the decision considered, or `none` when the request carried no grounded field |
| `transform_class` | `envelope` or `prefix_impacting` -- what decided which gates the decision had to clear. `none` only for a decision that considered no row |
| `reason` | Closed-set token naming why the decision acted or fell open (below) |
| `provenance_phase` | Detection phase of the acting verdict the decision rested on (`f1` / `f2` / `f3`), or `none` for a decision no authorization permitted |
| `provenance_source` | Evidence source of that verdict (`live` / `probe`), or `none` |
| `confirmations` | Acknowledged confirmation cycles backing the verdict at authorization time. `0` for a decision no authorization permitted -- honest rather than a sentinel, since zero confirmations is exactly what authorized it |
| `canary` | Where that identity's re-verification stood when the decision was authorized (`counting` / `due` / `in_flight`), or `none` |
| `canary_last_outcome` | The last settled canary outcome for that incarnation (`confirmed` / `regressed` / `inconclusive`), or `none` when none has settled |
| `decisions_acted` | WARN only: how many decisions rewrote something |
| `decisions_planned` | WARN only: how many decisions were recorded for the request |

The five PROVENANCE fields answer "on what evidence?", and they are populated
from the exact authorization snapshot that permitted the action -- never re-read
afterwards, because the identity's state can move between the gates clearing and
the line being emitted, and a re-read would report a provenance the rewrite was
never authorized under.

They appear on BOTH tiers, and the pairing is what makes the two correlate: the
DEBUG line carries the provenance of the decision it describes, and the WARN
carries the provenance of the HEADLINE decision it names. So scanning the DEBUG
lines for the `state_key` a WARN named lands on a line with the same phase,
source, confirmation count, and canary posture.

They are `none` / `0` on every REFUSAL, and on a refusal only: a refusal held no
authorization, so reporting one would attribute permission to a decision that had
none. A `canary_restored` decision DOES carry them even though it did not act --
the authorization permitted an action, and the planner spent it on a
re-verification rather than a rewrite.

The WARN NAMES one acting decision, chosen by two rules in order:

1. **Highest impact wins** -- `prefix_impacting` over `envelope`. One line
   stands for the whole request, and what an operator needs from it is the worst
   thing that happened: a request that rewrote a cache prefix AND an envelope
   field is a prefix-impacting event, whatever else it did.
2. **Among equal impact, the FIRST planned wins** -- which matches the DEBUG
   order above. So after reading a WARN, scanning the DEBUG lines for the
   `state_key` it named lands on the decision it meant, rather than on an
   earlier equal-impact one the WARN skipped. Equal-impact ties are ordinary: a
   two-seat fallback chain whose seats both act on the same class produces one
   on every such request.

Naming one decision never narrows the counts. `decisions_acted` and
`decisions_planned` always describe every decision recorded for the request, so
the headline choice hides nothing.

The `reason` vocabulary is closed. Each token is a different operator
situation, and they are deliberately not collapsed -- which gate is holding is
the whole information content of a fail-open:

| `reason` | Meaning |
|---|---|
| `field_preflight_drop` | ACTED: the mapped field was dropped from the per-target request before dispatch |
| `no_grounded_field` | The request carries no closed-table field at all |
| `unsupported_lane` | The capability kill switch is off, or the target is not on the one lane this stage acts on |
| `unattributable_target` | A rejection from this target could not be attributed to a routectl-owned seat: it authenticates with a forwarded client credential, carries no attributable Anthropic API base URL (a Bedrock Mantle entry reports none), or names a local hop |
| `masked_by_override` | An operator `force_supported` override masks this field's capability cell for this target. The operator has said to send the field; the learned verdict is NOT deleted |
| `no_identity` | No identity could be minted for the field on this target |
| `not_eligible` | No settled, acknowledged, acting verdict -- covers absent, lapsed, canary-suspended, zero acknowledged confirmations, and a verdict whose state moved under the eligibility read |
| `below_quorum` | The verdict IS eligible, but its acknowledged confirmation count is below the quorum this transform class requires. A prefix-impacting rewrite needs two confirmed cycles; one is not enough |
| `no_target_opt_in` | A prefix-impacting transform whose quorum is satisfied but whose target is not named in `[fidelity] prefix_impact_opt_in`. A content rewrite stays dormant until the operator opts the target in |
| `ambiguous_mutation` | The transform was authorized but removed nothing, or reported success while its field was still present. Fails open to a byte-equivalent original |
| `canary_restored` | This request is the re-verification canary: the tested field was RESTORED rather than dropped, and the upstream's answer re-verifies or disproves the verdict |
| `capability_writer_unhealthy` | Durable capability-event persistence cannot currently be guaranteed, so learned pre-flight is suspended for EVERY verdict. Not a verdict gate: it says nothing about whether the verdict is right, only that a wrong one could not be durably retracted (the clear, the purge, and the confirmation are all capability-event writes). Held while ANY of three monotonic failure counters is nonzero -- a store fault, a capability write the channel refused as full, or one it refused as closed -- so a SINGLE failure holds it for the life of the process, including one that happened at boot before the gate was wired. Ordinary usage-record drops are excluded: telemetry falling behind says nothing about capability persistence. Reactive forward-and-repair is unaffected and still serves every request -- see the status token of the same name |

Content-free by construction, at BOTH tiers. Every field is a closed-set
token, a code-authored path literal, a `sanitize_for_log`-sanitized state key,
or a boolean/count. No request values, no response body, no credential, no
session key, and no upstream text at any verbosity. That matters most for the
`prefix_impacting` class, whose dropped value is a system prompt rather than an
enum token: the line names the path and the class, never the content.

### Field-verdict snapshot INFO (status / doctor)

One aggregated INFO line, `"envelope field verdict snapshot"`, carries the whole
observability floor -- so the repair counters above, every resident field verdict,
the probe scheduler's state, and the paid-cap accounting stay visible without
polling metrics or reading the response body.

ONE LINE PER REQUEST, not per panel build. A standalone `/status/health` or
`/status/doctor` request emits exactly one; the `/status` aggregate builds BOTH
panels and still emits one, because two identical snapshots per poll would make a
reader counting lines read double the poll rate while the two lines carried
different timestamps for one moment.

The arbitration is a request-scoped CLAIM rather than a pre-assigned emitter, and
the difference is operationally visible: each panel is built through a guard that
degrades a failing data source to an unavailable panel, and an unavailable build
never reaches its logger. With a pre-assigned emitter, a failing health panel took
the whole line down for that request. With a claim, whichever builder reaches the
logger first wins it -- so health failing still lets doctor emit. Suppression is
about the log only; both panels' data is built in full.

A daemon serving with NO on-disk config path emits the line from the doctor branch
too, with unavailable budget data. There is no report to build there, but the
observability floor is about the router, not the report: the counters, the verdict
rows, the probe state, and the writer health all come from state that branch has in
full, and only the per-provider caps (which come from config) are missing. Staying
silent would mean a config-less daemon satisfied the floor only through
`/status/health`.

Reading it is PURE: no lane activates, no cadence advances, no re-verification slot
is claimed, and no work is scheduled. A status poll runs every few seconds, so a
read that mutated would re-verify verdicts on the dashboard's refresh interval and
consume the slots real traffic needs.

| Field | Meaning |
|---|---|
| `rc_field_repair_attempted_total` | Same counter as the repair WARN section above |
| `rc_field_repair_succeeded_total` | Same counter as the repair WARN section above |
| `rc_field_verdicts_learned_total` | Same counter as the repair WARN section above |
| `rc_field_outstanding_unconfirmed_total` | CURRENT: requests riding on a verdict no canary has re-confirmed yet, summed over every resident identity. Rises and falls -- exposure that MIGHT later be disproved |
| `rc_field_disproved_requests_total` | LIFETIME: requests that applied a repair a canary later DISPROVED. Monotonic, never falls (including across a clear or reload), and counts requests AFFECTED rather than canary attempts -- one disproof of a verdict that repaired forty requests charges forty |
| `rc_field_preflight_actions_total` | ADOPTED ROW REWRITES: one per closed-table row whose transform was applied before dispatch, NOT one per request. A request carrying rows of two classes rewrites two surfaces and exposes two identities, so it counts two. Read alongside the per-REQUEST WARN below, whose unit is deliberately the other one -- this counter measures exposure, that line reports an event. The count that answers "is pre-flight acting at all", which no log level can. Rising here while `rc_field_repair_attempted_total` stays flat is the feature working: the upstream no longer sees the field, so the reactive arm has nothing to repair |
| `rc_parser_unlocalized_total` | Upstream rejections eligible to name a field whose envelope the parser localized NO field path from -- the am-I-flying-blind metric, modeled on `rc_bedrock_validation_unmatched_total`. Expected NON-ZERO on real traffic even when everything is healthy, since most caller-shaped 4xxs are not field rejections; the signal is its RATIO against the learned and repaired counts, which is why it is reported beside them. Deduped once per request per target |
| `rc_acting_field_verdicts_total` | Count of learned rows currently steering dispatch (field-scoped, not cleared, not catalog-scoped) |
| `rc_acting_field_verdicts` | One entry per acting row: `state_key`, `feature_key`, `phase`, `source` |
| `rc_field_verdict_rows_total` | Count of resident field verdicts the surface HAS, before the render ceiling. Rows are produced for every resident verdict, ACTING OR NOT: an operator debugging why pre-flight is not firing needs the row whose `blocked_reason` explains it |
| `rc_field_verdict_rows_omitted` | Rows the render ceiling dropped. Zero in the ordinary case; non-zero says the rows field is a SUBSET, so a truncated line is self-describing rather than silently partial |
| `rc_field_verdict_rows` | The rows that fit -- the per-verdict floor, detailed below. Capped at a fixed code constant (32), because the count grows with what a deployment has learned while this line is emitted every poll |
| `rc_acting_field_verdicts_omitted` | Same ceiling, same reading, for the acting list |
| `rc_paid_probe_budgets_total` | Count of providers named in `[fidelity] paid_probe_daily_caps`. Zero on a deployment that configured no cap, which is the default |
| `rc_paid_probe_budgets_omitted` | Same ceiling, same reading, for the budget rows |
| `rc_paid_probe_budgets` | The budget rows that fit -- the paid-cap accounting, detailed below |
| `rc_usage_writer_degraded` | PROCESS-GLOBAL: whether the usage writer can persist rows at all. The OTHER reason every provider's paid probes are refused, independent of any cap. Emitted ONCE and never copied onto a provider row -- the writer is one actor, and a per-row copy would make a summed or attributed column wrong in both readings. A LATCHING reading: it over-reports a resolved problem rather than under-reporting a live one |
| `rc_paid_probe_consumed_unauthorized_total` | PROCESS-GLOBAL: paid-probe units that COMMITTED but whose caller was never authorized, because shutdown began between the transaction and the answer -- budget consumed for no call. **The only place an operator can see that divergence.** A provider's `committed_today` cannot show it (the unit IS spent, there is no refund, and its ledger row looks like any other) and `rc_usage_writer_degraded` cannot either (the write landed, so it is neither a storage fault nor a healthy write). Emitted once because the counter has NO provider dimension -- the reservation records only that a unit committed unauthorized. **RESETS ON RESTART:** nothing persists it, so reconciling a day's budget across a restart means reading this from the logs of the process that emitted it |
| `rc_probe_activations_total` | Lifetime probe lanes activated. A lane activates on its first admitted real request, never at startup, install, config parse, or reload |
| `rc_probe_queued` / `rc_probe_in_flight` / `rc_probe_backing_off` | The bounded scheduler's own queue state, read from the scheduler rather than re-derived -- so these are the numbers it enforces its bounds against |
| `rc_probe_last_settlement` | The most recently settled free-probe outcome (`resolved` / `retryable` / `deferred` / `spent_free_step` / `timed_out` / `abandoned`), or `none` before any has settled. AGGREGATE, not per-lane: on a multi-lane deployment this names whichever lane settled last, so it is an existence-and-kind signal rather than an attribution -- the per-kind lifetime counters carry the distribution. A per-lane history would be a store with its own bound, eviction rule, and reload carry |
| `rc_probe_next_retry_ms` | Milliseconds until the EARLIEST backing-off job may be leased again, or `-1` when nothing is backing off. A negative sentinel rather than an absent field because ZERO is a real answer here (a backoff that already elapsed, leasable next tick). Derived from the deadlines jobs already carry -- no new state, no timer -- and bounded by the same ceiling the backoff itself is |

The two alarm halves are reported TOGETHER deliberately: one is exposure
that might yet be disproved, the other is exposure that was, and either
number read alone reads as the other.

#### Per-verdict rows (`rc_field_verdict_rows`)

One entry per resident field verdict. Every value is a closed-set token, a
count, a boolean, or a sanitized identifier.

| Field | Meaning |
|---|---|
| `state_key` | Sanitized `[providers]` state key of the target the verdict applies to |
| `capability_key` | The normalized `field:`-namespaced capability key, read off the resident row |
| `transform_class` | `envelope` / `prefix_impacting`, or `unknown` for a resident verdict whose path THIS build's closed table does not carry (what a verdict persisted by a build with a wider table looks like) |
| `prefix_impacting` | Whether this class's transform rewrites content the upstream hashes into its cache prefix. `false` for an unknown class -- it claims no prefix cost it cannot substantiate |
| `source` | `live` / `probe` |
| `phase` | `f1` / `f2` / `f3` |
| `confirmations` | Acknowledged confirmation cycles backing the verdict |
| `required_quorum` | Cycles this verdict's CLASS requires before a pre-flight rewrite may run, or `unknown` with no class. Reported beside the actual count deliberately: the required value is a per-class code constant, so a bare count of one is either sufficient or half-sufficient depending on a fact the row would otherwise not carry |
| `blocked_reason` | `capability_disabled` / `unsupported_lane` / `masked_by_override` / `capability_writer_unhealthy` / `canary_suspended` / `not_eligible` / `below_quorum` / `no_target_opt_in`, or `none` when nothing is blocking. Reported in the planner's own precedence, so the token names the FIRST gate that holds: the two CONFIG-LEVEL gates come first because they refuse the LANE rather than the verdict -- a verdict on a refused lane is real but can never fire, so no per-key reason about it would be actionable -- and the operator mask follows, since it is about a specific capability on a lane the stage would otherwise act on. These are STATUS-SURFACE tokens with their own stable table, deliberately not read from the planner's internal constants: those are a debug vocabulary its own module may retune, and a rename there would silently change what every operator dashboard and alert matches on. What keeps the two from meaning different things is that each value is produced by consulting the planner's own predicates (the kill switch, the lane predicates, the override registry, eligibility, the class quorum, the target-spec membership) rather than by paraphrasing its conditions. Four need reading carefully: **`capability_disabled`** is the global kill switch (`[capability] enabled = false`), so it reports for EVERY verdict at once and its remedy is flipping one setting rather than anything per-target. **`unsupported_lane`** means the target's lane is not one this stage acts on at all -- not an Anthropic-API provider, or no attributable base URL (a forwarded credential, a local hop, or a Bedrock Mantle entry) -- so the verdict is real and permanently inert; note that a status read carries no per-request forwarded-credential fact, so a lane whose REQUESTS are forwarded may instead read `not_eligible`, the correct conservative answer rather than a false claim about the lane. **`masked_by_override`** is the only reason whose remedy is a CONFIG edit rather than more evidence -- an operator `[capability.overrides]` cell forces the capability supported, so no learned verdict acts on it however well confirmed, and reading `not_eligible` there would send someone hunting confirmations that could never help. **`not_eligible`** additionally covers a verdict whose transform class THIS build's closed table does not carry (a ledger row from a wider build): it is unactionable rather than short of evidence, and the absent `transform_class` beside it is what says which case it is. **`capability_writer_unhealthy`** means durable capability-event persistence cannot currently be guaranteed, so learned pre-flight is suspended for EVERY verdict at once -- and it is NOT a verdict gate: every escape hatch a pre-flight rewrite depends on (the durable clear a disproving canary performs, an operator purge, a later confirmation) is itself a capability-event write, so while those cannot be guaranteed a wrong verdict could not be taken back out of service durably. Reported ahead of every evidence gate for exactly that reason -- an operator reading `not_eligible` or `below_quorum` here would go hunting confirmations when what is needed is a healthy writer -- and BEHIND the operator mask, because a masked row's fate does not change when the writer recovers. Held while ANY monotonic capability-persistence failure counter is nonzero (a store fault, a full-channel refusal, or a closed-channel refusal), compared against ZERO rather than a value sampled when the gate was wired -- the boot warm commits its replay-boundary tombstone before that point, so a baseline would have forgiven a tombstone that never landed and reported durable until a second failure arrived. Ordinary usage-record drops are deliberately excluded. Because the counters are monotonic this is STICKY for the process: recovery is a restart, which a warm rebuild makes cheap. Reactive forward-and-repair is unaffected, so a lane reporting this is still fully served: it forwards and repairs on rejection rather than rewriting ahead of one. `canary_suspended` is the one reason this surface distinguishes that the planner does not: the planner folds it into not-eligible because its decision is the same either way, but a suspension is a verdict a canary proved WRONG whose durable clear may be failing |
| `canary` | `counting` / `due` / `in_flight` |
| `canary_remaining_requests` | Eligible non-streaming completion requests before the next canary is due. A REQUEST count, never a timestamp: the cadence is driven by traffic, so any instant derived from it is a projection an operator would read as a schedule |
| `canary_last_outcome` | `confirmed` / `regressed` / `inconclusive`, or `none` when none has settled for this incarnation. The `none` case is distinct from `inconclusive` on purpose -- a re-verification that never ran must not read as one that ran and proved nothing |
| `requests_in_flight` | Requests currently applying this verdict's repair |
| `unconfirmed_requests` | Requests modified since this verdict's last confirmation -- the exposure a later disproof would charge to the lifetime alarm |

#### Paid-cap accounting health (`rc_paid_probe_budgets`)

One entry per provider named in `[fidelity] paid_probe_daily_caps`.

| Field | Meaning |
|---|---|
| `provider` | The configured provider name the cap is keyed on, SANITIZED. Config load validates that the key names a configured provider, not its shape or length, so the value is operator-supplied and unbounded -- and a control byte in a log field is how a forged second line is injected |
| `daily_cap` | The operator's configured UTC-day cap. Zero -- the default -- means no paid call is ever made for this provider, which is a different state from an exhausted budget |
| `committed_today` | Units the LEDGER records as committed for the current UTC day, or `unknown` when the accounting could not be read as a count. Read from durable state rather than a process counter because the budget survives restart: a process-local tally reports zero on a daemon that came up after spending its day's cap. `unknown` rather than zero on a failed read, because zero is a claim that nothing was spent |
| `accounting` | `healthy` / `malformed` / `unreadable`. `malformed` is corrupt accounting to repair -- the reservation REFUSES on unparseable state, so every paid call for the provider is already failing closed. `unreadable` is a query that did not land, whose stored state may be fine |

A budget row carries ONLY these four per-provider facts. The writer's health and
the unauthorized-spend total are process-global and are emitted once at the top
level (above) rather than copied onto every row -- see those two fields for why
the scope is expressed in the shape.

The line never touches a panel's response body -- it is log-only
provenance for an operator watching INFO, not a wire contract. Like the
counters it accompanies, `rc_acting_field_verdicts_total` reads zero on
a build with no grounded rejection parsers; a nonzero count is itself
the signal that a field verdict is live.

## What's never logged

- Resolved secret values (env contents, file contents, OAuth tokens,
  bearer keys, AWS access/secret keys).
- The supplied `x-api-key` / `Authorization: Bearer` value on a
  rejected listener auth (we log only header presence).
- Full upstream request/response bodies. Bodies are only excerpted to
  256 chars on 4xx/5xx upstream paths, intentionally. Full body
  inspection is available at trace level -- see the Triage recipes
  section above.
- The inbound per-conversation session key, which is client-supplied and
  may be user-identifying. The header-vs-body conflict warning carries
  only the boolean fact of a mismatch, and the shadow-misfire warning
  carries a hash rather than the value.
  ACKNOWLEDGED EXCEPTION: the inbound session headers themselves
  (`x-session-id`, `session_id`, `session-id`, `agent-session-id`,
  `x-task-id`) do appear verbatim in the direction-1 ingress header
  trace, which is double-opt-in (`ROUTECTL_TRACE_HEADERS` plus trace
  level) and deliberately shows session headers so captured fixtures
  reproduce a real client's request. That set is the `OPENAI_SESSION_HEADERS`
  allowlist in `ingress::session_key`, which is authoritative -- the list
  here is a reader's convenience and follows it.
  BODY SIDE IS NOT PART OF THIS EXCEPTION: the whole `metadata` object
  (Anthropic ingress) and the whole `client_metadata` object (OpenAI
  Responses egress) collapse to the opaque `{"redacted": true}` marker
  under `ROUTECTL_LOG_REDACT_PROMPTS=1` -- not just the named
  `metadata.session_id` / `metadata.user_id` leaves, since either
  object is a caller-supplied free-form key/value bag and an unlisted
  key (e.g. `metadata.user_email`) cannot be enumerated by name. The
  top-level `prompt_cache_key` and canonical `user` fields (never
  nested under `metadata`) still collapse individually to the
  `<redacted len=N>` marker, same as any other prompt-bearing leaf.
  With the knob off, all of the above ride verbatim in the trace body
  (the fixture-capture posture), same as every other unredacted leaf.

## Trace-level surfaces

Operator grep cheat sheet:

| Surface | Direction | Filter |
|---|---|---|
| `"ingress request body"`           | 1 client -> routectl     | `ingress=<openai\|anthropic>` |
| `"outgoing request body"`          | 2 routectl -> upstream   | `provider_kind=<kind>` |
| `"upstream success body"`          | 3 upstream -> routectl   | `provider_kind=<kind>` |
| `"egress response body"`           | 4 routectl -> client     | `ingress=<openai\|anthropic>` |
| `"structural summary"`             | 1 + 2 (request-side only) | `direction=ingress\|outgoing` |
| `"stream summary"` `direction=upstream` | provider-side stream end | `chunks=`, `finish_reason=` |
| `"stream summary"` `direction=egress`   | ingress-side stream end  | `chunks=`, `finish_reason=` |

The `"structural summary"` line fires on every REQUEST-side body
(directions 1 and 2 only -- response bodies are not summarized). It
carries a stable set of prompt-content-free fields for grepping
wire-shape invariants (`model=`,
`max_tokens=`, `thinking_shape=`, `output_config_effort=`,
`tool_choice_shape=`, `cache_control_count=`, `messages_len=`,
`tools_len=`, `anthropic_beta=`, `provider_extras_keys=`, `stream=`)
without fighting the 16 KB body cap that truncates fields appearing
after a large messages array. Existing field names in this line are
stable; pin scripts on them freely.

### Header trace redaction policy

When `ROUTECTL_TRACE_HEADERS=1` is set, the four `trace_*_headers`
emitters apply DIFFERENT redaction per direction by design:

| Direction | Headers emitted | Redaction |
|-----------|-----------------|-----------|
| 1 -- ingress request (client -> routectl) | Redacted | `authorization` Bearer values are replaced with `Bearer [REDACTED]` (scheme kept); `x-api-key`, non-Bearer `authorization`, and `proxy-authorization` collapse to `[REDACTED]` so a live client session token never lands in log archives |
| 2 -- outgoing request (routectl -> upstream) | Redacted | `authorization` Bearer values are replaced with `Bearer [REDACTED]` (scheme kept); `x-api-key`, non-Bearer `authorization`, and `proxy-authorization` collapse to `[REDACTED]` so live tokens never land in log archives |
| 3 -- upstream response (upstream -> routectl) | Redacted | `set-cookie` session credentials, an `authorization` echo, and the `x-amz-security-token` STS credential collapse to `[REDACTED]` (or `Bearer [REDACTED]`); rate-limit metadata, `x-amz-date`, and other non-secret headers round-trip verbatim |
| 4 -- egress response (routectl -> client) | RAW, no redaction | None -- egress headers carry no secrets |

This is intentional. Directions 1, 2, and 3 are the only directions
that carry auth or session material (a client session token inbound,
Bearer JWTs / api keys outbound, and an occasional session-cookie /
STS echo on the upstream response). Direction 4 is raw so
fixture-capture and triage workflows see the exact wire values
without workarounds; the fixture-capture rig parses the same TRACE
lines these emitters produce, so a direction-1 fixture's
`ingress_request.headers.json` carries the redacted value too --
consistent with the already-redacted `outgoing_request.headers.json`
and `upstream_response.headers.json` on directions 2 and 3.

**Treat TRACE logs as sensitive.** Even with direction-1, -2, and -3
redaction, direction 4 emits header values verbatim. Beta flags that
double as capability indicators still appear in the trace log on
every direction. Restrict log-archive
access accordingly and avoid leaving `ROUTECTL_TRACE_HEADERS=1` on in
long-running production processes.

## Anthropic SSE forward-compat observability

routectl's Anthropic-API egress sink-drains unknown SSE block / delta
/ event types and (when in budget) preserves their wire bytes through
the canonical pipeline for the matching Anthropic ingress to re-emit
verbatim. The capture is bounded per block at 256 KB total bytes
and 10000 deltas; once either cap trips, the block degrades to
sink-drain for the rest of its life and the canonical stream keeps
flowing. The five log emission sites give operators visibility into
what's being passed through, dropped, or capped. A typed delta that
arrives inside an `Unknown` block is captured opaquely (it surfaces
through `record_delta`'s "captured opaque delta" DEBUG line, like any
other opaque delta) rather than sink-drained, so it has no separate
log site.

| Site | Level | Fields |
|---|---|---|
| Unknown block opened (`sse_unknown::open_unknown_block`) | WARN | `provider`, `upstream_index`, `block_type`, `mode="v2_capture"` |
| Index mismatch (`sse_unknown::index_matches`) | WARN | `provider`, `expected_index`, `got_index`, `event_kind`, `open_block_type` |
| Per-delta capture (`sse_opaque::record_delta`) | DEBUG | `provider`, `upstream_index`, `delta_bytes` |
| Block stop summary (`sse_opaque::record_stop`) | INFO | `provider`, `upstream_index`, `block_type`, `captured_bytes`, `delta_count` |
| Cap exceeded / degrade (`sse_opaque::degrade`) | WARN | `provider`, `upstream_index`, `block_type`, `reason`, `captured_bytes`, `delta_count` |

Example lines (formatted for readability; real output is one event
per line and inherits the `request_id` span field):

```
WARN routectl_providers::anthropic_api::sse_unknown
  provider=anthropic-prod upstream_index=1 block_type=server_tool_use
  mode=v2_capture
  "anthropic SSE: opening forward-compat opaque content block"

WARN routectl_providers::anthropic_api::sse_unknown
  provider=anthropic-prod expected_index=0 got_index=1
  event_kind=delta open_block_type=text
  "anthropic SSE: content-block index mismatch; dropping misattributed event"

DEBUG routectl_providers::anthropic_api::sse_opaque
  provider=anthropic-prod upstream_index=1 delta_bytes=312
  "anthropic SSE: captured opaque delta"

INFO routectl_providers::anthropic_api::sse_opaque
  provider=anthropic-prod upstream_index=1 block_type=web_search_tool_result
  captured_bytes=2048 delta_count=4
  "anthropic SSE: opaque block closed"

WARN routectl_providers::anthropic_api::sse_opaque
  provider=anthropic-prod upstream_index=1 block_type=web_search_tool_result
  reason=byte_overflow captured_bytes=261888 delta_count=287
  "anthropic SSE: opaque-capture cap exceeded; degrading block to sink-drain"
```

`reason` on the degrade WARN is one of `byte_overflow` or
`delta_overflow` -- pin on this field in alerts, not on the message
string.

DEBUG-level logs are off by default; enable with
`ROUTECTL_LOG=routectl=debug` to see per-delta opaque-capture
detail. The INFO block-stop summary fires at the
default level, so operators routinely see one summary per
unknown block in production.

## Auth-failure log shapes

No secret values, ever:

| Surface | Log line |
|---|---|
| Listener auth (wrong `x-api-key` / `Bearer`) | `WARN routectl_cli::server::auth has_x_api_key=<bool> has_bearer=<bool> route=<path> "listener auth rejected"` |
| Bad secret ref (`env://NONEXISTENT`) | `WARN routectl_auth::memory_store scheme=env:// var=<NAME> reason="not set" "secret resolution failed"` |
| Bad secret ref (file perm too open) | `WARN routectl_auth::memory_store scheme=file:// path=<P> mode=<oct> reason="group/other readable; chmod 600 or 400" "secret resolution failed"` |
| Bedrock SigV4 / cred chain failed (Profile) | `WARN routectl_providers::bedrock::auth auth_kind=Profile profile=<name> region=<r> error=... "bedrock credential resolution failed"` |
| Bedrock SigV4 / cred chain failed (DefaultChain) | `WARN routectl_providers::bedrock::auth auth_kind=DefaultChain region=<r> error=... "bedrock credential resolution failed"` |
| Bedrock upstream 401 | `WARN routectl_providers::bedrock provider=<id> status=401 body_excerpt=... "bedrock upstream auth rejected"` |
| Bedrock SigV4 sign failure | `ERROR routectl_providers::bedrock::signing failure_kind=<kind> ... "bedrock auth failed"` -- where `<kind>` is one of `bearer_header_invalid`, `creds_unavailable`, `body_unbuffered`, `signing_params_build`, `non_ascii_header`, `signable_request_build`, `sigv4_sign`, `signed_header_name_invalid`, `signed_header_value_invalid`, `unexpected_query_params` |
| Bedrock 403 (IAM denied) | `WARN routectl_providers::bedrock provider=<id> status=403 action=<bedrock-runtime:InvokeModel...> principal_present=<bool> "bedrock IAM access denied"` -- `action` extracted from the AWS error body so you immediately see WHICH IAM action your role lacks |
| Bedrock in-stream auth event | `WARN routectl_providers::bedrock::eventstream provider=<id> event_type=accessDeniedException\|unauthorizedException\|authentication_error\|permission_error message=... "bedrock in-stream auth/permission exception"` |
| Anthropic upstream 401/403 | `WARN routectl_providers::anthropic_api provider=<id> status=<401\|403> auth_kind=<ApiKey\|OauthBearer> context=anthropic body_excerpt=... "upstream auth failed"` |
| OpenAI-compat upstream 401/403 | `WARN routectl_providers::openai_compat provider=<id> status=<401\|403> context=openai-compat body_excerpt=... "upstream auth failed"` |

Both rows share the message string `"upstream auth failed"`; the `context` field distinguishes the call site.

## Rejected authority claims (anti-DNS-rebinding)

Two surfaces validate the authority a request CLAIMS before serving it: the
read-only `/status*` subtree and the mutating control route
(`POST /control/capability/purge`). Both refuse with a fixed 403
`forbidden_host` envelope and both report the refusal the same way, so a
rejection can be correlated across them without learning two vocabularies.

| Surface | Level | Target | Message |
|---|---|---|---|
| `/status*` | WARN (SAMPLED) | `routectl::status::gate` | `status surface rejected a request with a disallowed authority claim` |
| control route | WARN | `routectl_cli::handlers::control` | `capability purge refused a request with a disallowed authority claim` |

| Field | Type | Meaning |
|---|---|---|
| `claim_site` | string | Which claim failed, from a CLOSED set: `host_header` (a `Host` header value was disallowed, or was not valid UTF-8 and so could not be evaluated) or `uri_authority` (the request URI's authority -- HTTP/2's `:authority` -- was disallowed). |
| `host_403_total` | integer | Running process-wide total of status-surface authority rejections. Present on the `/status*` line only, and it is what the sampling is keyed on. |

```
WARN routectl::status::gate host_403_total=1 claim_site="host_header"
  "status surface rejected a request with a disallowed authority claim"
```

**The claimed value is never logged.** It is attacker-controlled, and the SITE
is the only part an operator needs in order to know where to look. Requests
carrying no authority at all (origin-form HTTP/1) make no claim and are not
refused, so they produce no line.

**The status line is SAMPLED** (first, then every Nth) off `host_403_total`, so
a rejection burst does not emit one line per request -- the count is the
complete record, the lines are a readable sample. Do not read the line count as
an event count. The control-route line is not sampled: an operator-initiated
control call is not a traffic source.

## Usage accounting log shapes

The `routectl-usage` writer subsystem emits the following lines.
All have `target: "routectl_usage::writer"` or `"routectl_usage::handle"`
and inherit no `request_id` span (the writer runs on a dedicated OS thread).

| Level | Module target | Key fields | Message |
|---|---|---|---|
| ERROR | `routectl_usage::writer` | `error=...` | `"usage writer degraded -- dropping rows it cannot persist"` (healthy->degraded edge; fired once on transition) |
| ERROR | `routectl_usage::writer` | `write_errors=<N>` | `"usage writer still degraded"` (rate-limited: every 1024 errors after the first) |
| INFO  | `routectl_usage::writer` | (none) | `"usage writer recovered -- persisting rows again"` (degraded->healthy recovery edge) |
| ERROR | `routectl_usage::writer` | `error=...` | `"usage db open failed -- running degraded (records will be dropped)"` |
| WARN  | `routectl_usage::handle` | `dropped_total=<N>` | `"usage channel full -- dropping record (capture lags writer)"` (rate-limited: first drop + every 1024 thereafter) |
| WARN  | `routectl_usage::writer` | `error=...` | `"usage retention prune failed -- continuing"` |

The usage ledger's `http_status` column records the transport status the
client received: 200 for a delivered non-streaming body and once the SSE
head commits, while a mid-stream provider failure keeps 200 and is carried
by `outcome` / `error_class` / `stream_stage` instead (streaming rows
written before this rule was in force are NULL and are not back-migrated).

An Anthropic-ingress streaming row also carries its context meter opening in
the same `extra` JSON (no schema column): `opening_present` (`false`, and no
other opening key, when no opening event was enqueued for the client -- a
pre-opening HTTP error, a stream that errors or fails to render before its
first event, or a client gone before one), then
`opening_source` (`upstream_wire`, `upstream_wire_unverified`, `anchor`,
`calibrated`, `raw`), `opening_reason`, `opening_input` (the cache-inclusive
count the client frame showed: `input_tokens` plus the write and read cache
fields, the per-TTL breakdown not added), `opening_provisional`,
`opening_lane_switched`, `opening_first_event_ms` (the serving upstream's own
first event; first content is `ttfb_ms`), `opening_first_enqueue_ms` (when
the first body event was handed to the response's SSE channel -- a
server-side time, not when the client received it), `terminal_source`
(`explicit_final`, `vendor_opening`, `proxy_opening`, `interim_carry`,
`partial_final`, `unmarked` for a count whose parser stated no provenance,
`unrecognized` for a provenance this build does not name, `missing`),
`terminal_input` and `terminal_vendor_verified`. Counts, milliseconds and
flags are JSON numbers and booleans. The terminal fields are refreshed each
time the renderer accepts a new terminal report, so a stream that later
errors or loses its client keeps the last one. `terminal_vendor_verified`
comes from the endpoint the parser read the report from (the first-party API
host, or an AWS Bedrock stream), never from numbers agreeing; `false` means
not established: an explicit final report relayed by an Anthropic-compatible
endpoint (a routectl back hop included) is only what that endpoint reported.
Other dialects' rows carry none of these keys. A naturally completed turn
also logs the source, reason, provisional and terminal labels on the
`context meter opening settled` DEBUG line. The vocabulary lives in
`crates/routectl-cli/src/handlers/opening_diagnostics.rs`.
`routectl usage --opening-accuracy` reads these keys back read-only
(CONFIGURATION.md, "Context meter opening accuracy"); it treats a label
outside these sets as a data defect, so adding a label means extending the
closed sets in `opening_diagnostics.rs`.

## Config-edit audit shape

`routectl config set` emits exactly one audit event on a successful
write (a no-op set or a rejected edit emits nothing):

| Level | Fields | Message |
|---|---|---|
| INFO | `surface="cli"`, `verb="set"`, `path=<dotted path>`, `restart_required=[<field>, ...]`, `high_consequence=<bool>` | `"config edit committed"` |

The event records WHICH key changed and whether the change was
egress-defining or restart-only -- never the value written. The value
may be a `literal:` secret, so it is deliberately absent from the audit
trail (as it is from every other log surface).

## Prompt-cache auto-emission log shapes

The dispatch-path prompt-cache auto-emitter (see CONFIGURATION.md,
"Prompt-cache auto-emission") emits two lines per request: a per-dispatch
`cache_auto_decision` at DEBUG, and a `cache_auto_outcome` at DEBUG on the
healthy path or WARN when a cache thrash is detected (see each section
below for the exact level). Both carry counts and stable tokens only --
never bodies, prompt content, or secrets.

### `cache_auto_decision` (DEBUG, per dispatch)

Emitted once per dispatch target with the decision the auto-emitter
made for that target. routectl places up to TWO markers per request -- a
FRONT per-block marker (on the last wire-eligible system block, else the
last custom tool) and a TERMINAL top-level marker -- so the line carries
one decision per marker plus the legacy aggregate.

| Field                | Meaning                                                   |
|----------------------|-----------------------------------------------------------|
| `provider`           | The provider name the request dispatched to.              |
| `model`              | The resolved model id.                                    |
| `strategy`           | The TERMINAL marker's decision token (vocabulary below); kept under this name for continuity with existing log consumers. |
| `front_decision`     | The FRONT marker's decision token, same vocabulary.       |
| `terminal_decision`  | The TERMINAL marker's decision token, same vocabulary.    |

The two markers are gated independently, so the tokens routinely differ:
a Bedrock Converse target records `auto_emitted` for the front marker and
`auto_skipped:no_capability` for the terminal one, and an opted-in
openai-compat target records `auto_skipped:no_capability` for the front
marker (its egress cannot carry one). Request-level facts
(`caller_supplied`, `auto_skipped:global_disabled`) apply to both.

The tokens are one vocabulary shared by this log line and the usage
ledger's `cache_front_decision` / `cache_terminal_decision` columns. The
legacy `requests.strategy` column stays write-stopped (retained in the
schema, NULL for every row written by this version onward):

| Token                                  | Meaning                                                                 |
|----------------------------------------|-------------------------------------------------------------------------|
| `auto_emitted`                         | routectl injected an ephemeral_5m breakpoint in this marker's slot.     |
| `caller_supplied`                      | The caller already supplied a breakpoint; routectl deferred entirely.   |
| `volatile_vetoed`                      | The stable prefix carried high-confidence volatile tokens; vetoed.      |
| `auto_skipped:global_disabled`         | `[cache] auto_emit_top_level_breakpoint = false` (the master kill for both markers). |
| `auto_skipped:provider_disabled`       | The provider's `auto_emit_top_level_breakpoint = false` (terminal) or `auto_emit_per_block_breakpoints = false` (front). |
| `auto_skipped:no_capability`           | The target's egress cannot carry this marker (or its capability is unknown -- fail closed): no top-level support for the terminal marker, no per-block surface for the front marker. |
| `auto_skipped:breakpoint_cap`          | Injecting would exceed the 4-breakpoint maximum.                        |
| `auto_skipped:no_placement_region`     | The request offers no slot this marker could occupy -- a front marker on a flat-string system with no typed custom tool. |
| `auto_skipped:validation_rolled_back`  | Injection was attempted but the combined breakpoint sequence failed validation; the whole candidate was discarded. |

### `cache_auto_outcome` (DEBUG healthy / WARN on thrash)

Emitted only when routectl auto-emitted a breakpoint AND the upstream
reported cache creation this request. Compares what the auto-emitted
breakpoint cost against what it returned.

| Field            | Meaning                                                  |
|------------------|----------------------------------------------------------|
| `provider`       | The provider name.                                       |
| `model`          | The served upstream model id.                            |
| `strategy`       | Always `auto_emitted` for this line.                     |
| `cache_creation` | Aggregate cache-write tokens (5m + 1h) the upstream reported. |
| `cache_read`     | Cache-read tokens the upstream reported.                 |

- **DEBUG** (healthy): the auto-emitted breakpoint created a cache entry
  AND got a read -- the cache is paying off.
- **WARN** (thrash): the auto-emitted breakpoint created a cache entry
  but got NO read this request (`cache_creation > 0` and `cache_read ==
  0`). The stable prefix is being cached on every request without ever
  being re-read, so the premium cache-write tokens are spent for no
  payoff. **Remedy:** disable auto-emit for that provider with the
  per-provider `auto_emit_top_level_breakpoint = false`, or set its
  `cache_capability` `supports_top_level_cache_control = false` (see
  CONFIGURATION.md). A caller-supplied or skipped strategy is never
  flagged as thrash -- routectl only warns on decisions it made itself.

### per-request cache summary (`cache=READ/PROMPT (PCT%)`)

Emitted once per request from the usage-capture finalize path, alongside
the thrash signal above. The message reads `cache=READ/PROMPT (PCT%)`:

- **READ** -- the cache-read token count the upstream reported
  (`cache_read`).
- **PROMPT** -- the cache-INCLUSIVE prompt total. The usage DB stores a
  cache-EXCLUSIVE `input_tokens`, so this line reconstructs the inclusive
  prompt as `input_tokens + cache_read + cache_write_5m + cache_write_1h`.
- **PCT%** -- integer cache-hit percentage, `READ * 100 / PROMPT` (guards
  `PROMPT == 0` -> `0%`).

| Field          | Meaning                                                   |
|----------------|-----------------------------------------------------------|
| `request_id`   | The request correlation id.                               |
| `provider`     | The served provider name.                                 |
| `model`        | The served upstream model id.                             |
| `strategy`     | The stable cache-decision token (vocabulary above).       |
| `cache_read`   | Cache-read tokens the upstream reported.                  |
| `prompt`       | Cache-inclusive prompt total (reconstructed, see above).  |
| `cache_hit_pct`| Integer cache-hit percentage.                             |

Level gating, to avoid flooding INFO with `cache=0/0` on every uncached
request:

- **INFO** when there was cache activity -- a read (`cache_read > 0`), a
  write (`cache_write_5m + cache_write_1h > 0`), or an auto-emitted
  decision (`strategy == auto_emitted`). Cached / auto-emitted requests
  get an INFO breadcrumb.
- **DEBUG** otherwise (no cache activity).

Counts, ids, and stable tokens only -- never bodies, prompt content, or
secrets.

### `cache_prefix_rewritten_in_epoch` (WARN)

Emitted when the prefix-rewrite detector observes that the CLIENT rewrote
history inside its own conversation prefix. The detector runs once per client
request, before any dispatch target is chosen, over the raw canonical prefix
(system + tools + every message except the newest turn -- the newest turn grows
every turn by construction, so including it would report a rewrite every time).
A rewritten prefix invalidates the upstream cache from the rewrite point
onward, so every later turn pays full input price on bytes that were already
cached.

| Field                 | Meaning                                                                |
|-----------------------|------------------------------------------------------------------------|
| `session_key_hash`    | Per-process-salted hash of the inbound session key. Stable within one run so an operator can correlate lines, unpredictable across runs. The raw key is never logged. |
| `previous_prefix_len` | Message count the stored baseline prefix covered.                      |
| `prefix_len`          | Message count the prefix covers on this turn.                          |
| `epoch`               | Rewrites observed for this session within its tracked lifetime.        |

Dedup is per-process edge-triggered: the FIRST in-epoch rewrite any session
shows emits this WARN, and later rewrites stay silent (same trade as
`cache_volatile_in_caller_prefix`). The unsuppressed volume rides the usage
DB's `prefix_epoch_event` column instead -- 0 stable, 1 rewritten, 2 reseeded.

Never a false positive, by construction:

- A prefix that SHORTENED is recognized by length alone as the compaction shape
  (a summary replacing history) -- recorded as a reseed, never warned. The
  accepted residual is a bounded false NEGATIVE: a rewrite that also shortens
  the prefix is unobservable.
- A first-seen session (including the first turn after a process restart, and a
  session evicted from the bounded store) is recorded as a baseline with no
  classification and no WARN.
- A request carrying no session key produces no state and no WARN -- there is
  nothing to compare a later turn against.

**Remedy:** the rewrite happens client-side, so routectl cannot fix it. Look
for a client that edits, re-orders, or re-renders earlier turns (a re-generated
system preamble, a re-serialized tool history, an injected per-turn timestamp)
rather than only appending to them.

## Startup cache-policy banner

At server startup, immediately after the `routectl listening on ...` line,
routectl emits one INFO banner summarizing the two cache-policy switches:

| Field                 | Meaning                                                  |
|-----------------------|----------------------------------------------------------|
| `auto_emit_top_level` | `[cache] auto_emit_top_level_breakpoint` (bool).         |
| `reduction`           | `[reduction] enabled` (bool).                            |

The human message reads `cache policy: auto-emit top-level breakpoint
<enabled|disabled>, context reduction <enabled|disabled>`. It lets an
operator confirm at a glance which cache behaviors are live for this
process without grepping the config.

## Auto-activation inventory audit events

routectl tracks which of its own OAuth providers (anthropic, codex, xai,
antigravity) currently carry a usable LOCAL credential -- computed at
server boot and recomputed on every config or credentials reload. Each
transition into or out of the activated set emits one audit event. All
share the stable message `activation inventory` (grep this to isolate the
trail). The probe is local-only: it reads the in-memory OAuth token cache
and never touches the network.

| Level | Trigger condition | Key fields | Message |
|---|---|---|---|
| INFO | A provider became activated | `provider`, `kind`, `trigger`, `transition=activated`, `referenced_by_aliases` | `activation inventory` |
| INFO | A provider became unresolved (lost its credential) | `provider`, `kind`, `trigger`, `transition=deactivated`, `reason`, `referenced_by_aliases` | `activation inventory` |
| WARN | No OAuth credential store to probe (no HOME/XDG) | `trigger` | `activation inventory: no OAuth credential store available to probe` |

Nothing is emitted when a recompute changes nothing (a routine token
refresh that keeps every provider activated is silent). Field vocabulary:

| Field | Meaning |
|---|---|
| `provider` | OAuth provider id (`anthropic`, `codex`, `xai`, `antigravity`). |
| `kind` | The provider's own-credential config kind (`anthropic-api`, `openai-responses`, `openai-compat`, `gemini`). |
| `trigger` | What caused the recompute: `startup`, `config_change`, or `credentials_change`. |
| `transition` | `activated` or `deactivated`. |
| `reason` | Deactivation reason code (deactivated only): `oauth_missing`, `oauth_expired`, `oauth_store_unavailable`, `not_cataloged`, or `unknown`. |
| `referenced_by_aliases` | `true` when a configured provider consumes this credential AND is reachable via the alias table; `false` for a bare login with no matching config. |

These fields carry only display-safe discriminants -- never a token, a
filesystem path, or an env value. The initially-activated set at boot
surfaces as `transition=activated` events with `trigger=startup`.

```bash
# Watch activation transitions (login / logout / expiry) live.
ROUTECTL_LOG=info ./routectl serve 2>&1 | grep "activation inventory"

# Only the deactivation reason codes.
ROUTECTL_LOG=info ./routectl serve 2>&1 \
  | grep "activation inventory" | grep transition=deactivated
```

## Context-reduction log shapes

The dispatch-path context reducer (see CONFIGURATION.md, "Context
reduction") emits one line per request, and only when reduction actually
stripped bytes. The line carries counts and stable tokens only -- never
message bodies, tool content, prompt text, or secrets.

### `context_reduction` (DEBUG, only when applied)

Emitted once per dispatch when the whitespace-only minify pass changed at
least one JSON-valued string in the mutable tail. A request where
reduction is disabled, has no mutable tail, or finds nothing to strip
logs nothing here -- and its decision is NOT persisted either: the usage
DB's `reduction_strategy` column is write-stopped (see below).

| Field             | Meaning                                                       |
|-------------------|---------------------------------------------------------------|
| `provider`        | The provider name the request dispatched to.                  |
| `model`           | The resolved model id.                                        |
| `strategy`        | The stable decision token (always `applied` for this line).   |
| `strings_minified`| How many JSON-valued strings were minified this request.      |
| `bytes_saved`     | Total bytes removed across those strings.                     |
| `est_tokens_saved`| Estimated tokens saved (a byte-derived approximation).        |

The `strategy` token is a stable contract, but a LOG-ONLY one: the usage
DB's `reduction_strategy` column is write-stopped (retained in the schema,
NULL for every row written by this version onward). Observability is
PARTIAL -- only the `applied` token ever reaches a log line, because
`context_reduction` is emitted only when reduction actually stripped
bytes. The `skipped:*` tokens below are the vocabulary of the reducer's
decision, not of anything observable: they are neither logged nor
persisted:

| Token                       | Meaning                                                                  |
|-----------------------------|--------------------------------------------------------------------------|
| `applied`                   | Reduction ran and stripped whitespace from at least one JSON string.     |
| `skipped:disabled`          | Reduction not effective (global off, or provider `reduction_enabled = false`); the minify pass never ran. |
| `skipped:no-tail`           | No mutable tail (every message is frozen behind a caller breakpoint); nothing to safely touch. |
| `skipped:nothing-to-strip`  | The pass ran but no JSON-valued string in the tail had insignificant whitespace to remove. |
| `skipped:unknown`           | Reduction ran but produced an outcome this build does not map (forward-compat catch-all). |

Only `applied` emits a `context_reduction` log line; the `skipped:*`
tokens produce no log line and, since the ledger column is write-stopped,
leave no record at all.

## Config-reload transition fields

Every applied hot reload logs one INFO line on target
`routectl_cli::server::reload`:

```
config reloaded; router rebuilt and swapped path=... trigger=config change
```

`[reduction] enabled` is the operator's live kill switch for the
dispatch-path reducer (see CONFIGURATION.md, "Context reduction"), and
`[cache] k_gated_emission` is the live kill switch for break-even-gated
cache-marker suppression (see CONFIGURATION.md, "Break-even emission
gate"), so a reload that CHANGED either adds that switch's before/after
pair to the same line:

| Field                      | Meaning                                                              |
|----------------------------|----------------------------------------------------------------------|
| `reduction_enabled_before` | The `[reduction] enabled` value the outgoing config carried.          |
| `reduction_enabled_after`  | The value the freshly-loaded config carries (now live).               |
| `k_gated_emission_before`  | The `[cache] k_gated_emission` value the outgoing config carried.     |
| `k_gated_emission_after`   | The value the freshly-loaded config carries (now live).               |

Each pair is emitted TOGETHER and ONLY when that switch's value changed.
The two pairs are independent: a reload that flips one leaves the other's
fields absent, and a reload that flips both stamps all four. A reload
that left both alone logs the line without any of them, so a pair's
presence is itself the signal that that flip landed:

```
config reloaded; router rebuilt and swapped path=~/.config/routectl/config.toml \
  trigger=config change reduction_enabled_before=true reduction_enabled_after=false
```

The line is the operator's confirmation that the intended transition took
effect. Absence of a pair after a config write that was meant to flip
that switch means the reload applied a config whose value was already what
the file now says -- or that the reload never fired at all (which logs no
success line). A reload that FAILED parse or validation also logs no
success line, so a declined candidate can never stamp a transition it did
not make. CONFIGURATION.md, "Context reduction", carries the recovery
runbook that uses this line.

## Stream first-activity mark

`try_stream_with_first_content` (routectl-router) emits one DEBUG line the
instant a streaming upstream's response headers arrive -- before the
first content chunk is awaited. This is the first sign of upstream
life, distinct from the existing first-CONTENT `ttfb_ms` mark
(`mark_first_byte`), which additionally waits out any upstream
`message_start`/`ping` events the SSE parser swallows. There is no
automated regression test for this line (capturing a `tracing` event
through a thread-local subscriber proved flaky under the parallel test
harness); observe it manually instead:

```bash
ROUTECTL_LOG=routectl_router=debug ./routectl serve
```

Then issue a streaming request. Look for:

```
stream first-activity: upstream response headers received provider=... upstream=... elapsed_ms=...
```

`elapsed_ms` is measured from the per-attempt clock at dispatch. The
gap between this mark and the request's existing first-content mark
(`mark_first_byte`, recorded as `ttfb_ms` in the usage DB -- see
`routectl usage`) is the first-activity-to-first-content delta --
effectively the upstream prefill time.

## Capability intelligence events

routectl's learned-capability subsystem (see CONFIGURATION.md,
"Capability intelligence") emits a fixed vocabulary of structured events
as it learns, re-probes, clears, and strips per-target capability
negatives, plus two config-layer events for operator override hygiene.
Every event carries a stable `event` discriminator so alerts pin on that
field, never on the human message string. The events share one unified
field vocabulary: `event` names the kind, `state_key` names the dispatch
target's session/target key, and `capability_key` carries the normalized
capability token. All fields are display-safe discriminants -- capability
TOKENS, session/target keys, and counts only. Never a request body,
prompt content, or secret.

**Revision:** field vocabulary last changed in 0.9.0 (the capability
event vocabulary was unified on `event` / `state_key` / `capability_key`;
the tail-demotion event was renamed to `route_away` with INFO/WARN
levels; the `learn` event gained the `provider_kind` / `upstream_status`
/ `upstream_code` / `upstream_param` enrichment fields; `clear`,
`expire_probe`, `count_tokens`, and `purge` were added). The field names
and `outcome` / `signal_tier` / `event` tokens below are a stable
contract; new fields may be added between releases.

**Not the stable API.** `routectl doctor --json` surfaces the capability
panel (catalog priors and operator overrides read by a fresh process;
the learned registry is runtime-only and NOT visible to doctor) for
human triage, but its shape is NOT a stability guarantee and may
change between releases. Build tooling against the event contract
documented here (these `event` tokens and field names), not against
`doctor --json`.

Summary (grep the `event` field to isolate a kind):

| `event` | Level | Module target | Message |
|---|---|---|---|
| `learn` | WARN | `routectl_router::router` | `learned-capability negative observed` |
| `clear` | INFO | `routectl_router::learned_capability` | `learned-capability negative cleared by successful re-probe` |
| `purge` | INFO | `routectl_router::router::capability_purge` | `operator purged a learned-capability entry` |
| `expire_probe` | INFO | `routectl_router::learned_capability` | `lapsed learned negative admitted for its single re-probe` |
| `evict` | WARN | `routectl_router::learned_capability` | `learned-capability registry at capacity; evicted oldest entry` |
| `route_away` | INFO / WARN | `routectl_router::router` | `learned-capability negative de-prioritized this target to the tail` (INFO) / `... routed this target away; request survives only via the de-prioritized learned tail` (WARN) |
| `count_tokens` | INFO | `routectl_router::router` | `count_tokens seat terminal; resilience class policy applied` |
| `invalidation` | WARN | `routectl_router::router` | `catalog/overlay changed across reload; clearing catalog-scoped learned capabilities` |
| `strip` | WARN | `routectl_router::router` | `capability_strip_decision` |
| `suppression` | WARN | `routectl_router::router` | `force_supported override contradicted: masked capability still rejected upstream` |
| `dead_override_key` | WARN | `routectl_router::override_registry` | `capability override key is rewritten by normalization; ...` |
| `legacy_deprecation` | WARN | `routectl_cli::server` | `deprecated capability-list keys are set; ...` |

## Bounded probe-scheduler diagnostics

The background probe scheduler emits THREE edge-triggered warnings and carries
the rest of its state as counters. Each condition repeats -- per request, or per
settlement -- and would otherwise flood exactly when the daemon is busiest, so
every line is latched and its counter carries the suppressed volume. The line is
the existence proof; the counter is the rate.

The latch SCOPES differ, because the conditions do:

- `probe_activation_refused` and `probe_payload_retention_refused` latch once
  per ROUTER INCARNATION. Both latches are fields on `Router`, so a reload
  publishes a new one with the latch clear: saturation or a refused beta shape
  under a NEW configuration is new information.
- `probe_tombstone_capacity_saturated` latches once per SATURATION EPISODE. It
  stays suppressed until retirement clears the marker -- which can outlast a
  single incarnation -- because the condition is a property of the marker set
  rather than of the router that observed it.

| Message | Level | Module target | Meaning |
|---|---|---|---|
| `probe_activation_refused` | WARN | `routectl_router::router` | The probe queue is at its depth bound, so a lane could not be activated. Carries `queued` / `in_flight` / `backing_off` / `queue_full_total`. |
| `probe_payload_retention_refused` | WARN | `routectl_router::router` | A lane's beta context breached a payload retention bound or validity rule, so no probe was activated for it. Carries `payload_refusals_total` only -- deliberately no token, value, or identity, since the refusal is precisely that the shape was unacceptable. |
| `probe_tombstone_capacity_saturated` | WARN | `routectl_router::probe_scheduler` | Terminal-marker capacity is exhausted for this incarnation, so the scheduler fails closed and refuses further activations until a republication. |

### Probe counters (`ProbeSchedulerSnapshot`)

These are TELEMETRY on the router's snapshot accessor, not an
operator-rendered surface: nothing in `status` or `doctor` prints them today,
and a consumer reads them through `Router::probe_scheduler_snapshot()`. Two are
worth naming because they are the only signal their condition produces:

- `payload_refusals_total` -- lanes not probed because their beta context was
  refused. A lane counted here produces no job, no settlement, and no
  tombstone, so without this counter it is indistinguishable from a lane that
  was never admitted.
- `paid_candidate_capacity_refusals_total` -- exhausted lanes whose paid-probe
  CANDIDATE could not be recorded because the candidate list was full. Costs no
  upstream call (a candidate is not permission to spend), but a non-zero value
  means the candidate list is truncated rather than complete.

### `learn` (WARN)

Emitted once per request per `(state_key, capability_key)` when an
upstream rejection teaches routectl a new (or reconfirming) capability
negative. `capability_key` is the CANONICAL capability the shared
resolver attributed the fault to (e.g. `web_search`, `structured_output`)
-- not the raw upstream `error.code` token, which is carried separately as
`upstream_code` for observability.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `learn`. |
| `state_key` | string | The dispatch target's session/target key. |
| `capability_key` | string | The normalized canonical capability token learned unsupported. |
| `provider_kind` | string | The target provider's egress kind (`anthropic-api`, `openai-compat`, ...). |
| `upstream_status` | integer | The upstream HTTP status that carried the rejection (`400` or `422`). |
| `upstream_code` | string | The upstream `error.code` token, or empty when the upstream sent none. |
| `upstream_param` | string | The upstream `error.param` value -- PRESENT ONLY when the sanitizer deemed it safe to log verbatim (bounded, single-token, no whitespace/control bytes); the field is OMITTED entirely otherwise, so an adversarial or oversized `error.param` never reaches the log. |
| `signal_tier` | string | `self-identifying` or `inferred` -- how the negative was classified. |
| `observations` | integer | How many times this negative has been observed (>= 1). |
| `acting` | bool | `true` once the entry is acting (routes away / strips); `false` while still pending. |

```
WARN routectl_router::router event=learn state_key=m1
  capability_key=structured_output provider_kind=openai-compat
  upstream_status=400 upstream_code=unsupported_parameter
  upstream_param=response_format signal_tier=self-identifying
  observations=1 acting=true "learned-capability negative observed"
```

### `clear` (INFO)

Emitted when a successful re-probe clears a resident learned negative.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `clear`. |
| `state_key` | string | The target's session/target key. |
| `capability_key` | string | The normalized capability token cleared. |
| `signal_tier` | string | `self-identifying` or `inferred` (from the cleared entry). |

```
INFO routectl_router::learned_capability event=clear state_key=nick
  capability_key=web_search signal_tier=self-identifying
  "learned-capability negative cleared by successful re-probe"
```

### `purge` (INFO)

Emitted once per operator purge request the daemon accepts -- one line
per call to the loopback-only capability-purge control route (see
CONFIGURATION.md, "Dropping ONE learned observation"), whether or not the
key held anything. `removed` is what tells the two apart, so an operator
reading the log can distinguish "I removed a verdict" from "there was
nothing there".

Content-free by construction: the four fields below are the whole record.
The state key and the normalized capability key are the same
display-safe discriminants every sibling event carries, and both are run
through the shared log sanitizer since the caller supplies them. No
request body, prompt, upstream text, or caller address ever appears.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `purge`. |
| `state_key` | string | The target's session/target key, as requested. |
| `capability_key` | string | The NORMALIZED capability token the purge was keyed on. |
| `removed` | bool | `true` when a resident entry was removed; `false` for a clean no-op on a key that held nothing. |

```
INFO routectl_router::router::capability_purge event=purge state_key=sonnet
  capability_key=web_search removed=true
  "operator purged a learned-capability entry"
```

A `removed=true` line is emitted only AFTER the `cleared` row has durably
committed to the capability-event ledger -- that row is what stops the next
boot's warm rebuild from replaying the negative, and the ordering is what
makes the log trustworthy: a `removed=true` line can never describe a
removal the ledger did not receive. A `removed=false` line writes no row.

### `purge_abandoned` (WARN)

Emitted when a purge could NOT persist its clear. Nothing was removed: the
entry is unchanged and still acting, and the operator received a non-2xx
answer telling them to retry.

Carries the same two display-safe key fields and nothing else. The wire
answer collapses every cause to one code, but this line names the cause
for the operator reading their own daemon's log, via a companion WARN
carrying an `outcome` token (`writer_unavailable`, `writer_channel_full`,
or `write_failed`).

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `purge_abandoned`. |
| `state_key` | string | The target's session/target key, as requested. |
| `capability_key` | string | The NORMALIZED capability token the purge was keyed on. |

A `purge_abandoned` line is never accompanied by a `cleared` row, which is
the property that makes the pair readable: exactly one of
`purge`/`removed=true` and `purge_abandoned` is emitted per attempted
removal.

Both are emitted by a DAEMON-OWNED task rather than by the request, so a
client that disconnects mid-purge still produces exactly one of them. Two
further lines belong to that ownership:

- `purge_incarnation_mismatch` (ERROR) -- the clear committed, but the
  resident entry was no longer the version the operator approved
  removing, so nothing was deleted. Nothing in normal operation produces
  this; it means a mutation reached a leased key.
- an ERROR naming an unaccounted-for settlement, which also stops the
  daemon. After a batch is admitted, a settlement that neither finalizes
  nor abandons leaves the daemon unable to say whether its registry
  agrees with its ledger for that key, so it stops serving rather than
  routing on state it cannot verify. A restart reads the ledger and is
  authoritative again.

### `incarnation_exhausted` (ERROR)

Emitted when the per-key ordering sequence a purge floor compares against
has no values left. The mutation that hit it is REFUSED, having changed
nothing: the value is reserved before any entry is touched, so exhaustion
cannot leave a mutated entry that no event describes.

Carries no fields. Nothing an operator does causes it -- the sequence is
64-bit and advances once per learned-capability mutation, so at one
mutation per nanosecond it outlives the process by centuries. A line here
means something is driving the registry far outside real traffic, and the
daemon is refusing to wrap rather than let a superseded event compare as
newer than the purge that superseded it. A restart resets the sequence.

The refusal is also counted, so it is visible without a log level.

One writer-side counter accompanies the per-key purge floor:
`capability_events_superseded` counts events dropped because a purge of
the same key had already superseded them. Zero on a daemon nobody purges.
A large value is a throughput signal (events queueing long enough to
straddle purges), not a correctness one -- the drop is the protection
working.

### `expire_probe` (INFO)

Emitted when a lapsed learned negative is admitted for its single
re-probe.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `expire_probe`. |
| `state_key` | string | The target's session/target key. |
| `capability_key` | string | The normalized capability token being re-probed. |
| `signal_tier` | string | `self-identifying` or `inferred`. |

```
INFO routectl_router::learned_capability event=expire_probe state_key=nick
  capability_key=web_search signal_tier=self-identifying
  "lapsed learned negative admitted for its single re-probe"
```

### `evict` (WARN)

Emitted when the registry is at capacity and evicts the oldest entry. A
safety valve, not a routine cache policy.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `evict`. |
| `state_key` | string | The evicted entry's session/target key. |
| `capability_key` | string | The evicted entry's capability token. |
| `max_entries` | integer | The registry capacity that triggered eviction. |

```
WARN routectl_router::learned_capability event=evict state_key=n
  capability_key=cap_a max_entries=2
  "learned-capability registry at capacity; evicted oldest entry"
```

### `route_away` (INFO / WARN)

Emitted once per learned-tail demotion when an acting capability negative
de-prioritizes a target. The LEVEL distinguishes the two outcomes:

- **INFO** when a supported alternative still fronts the chain -- the
  learned negative simply moved this target to the tail.
- **WARN** when the chain survives ONLY via the de-prioritized learned
  tail (every other target was filtered out), so the request rides the
  route-away floor. This is the level to alert on: it is the moment a
  learned negative -- possibly a mislearn -- actually changes which target
  serves traffic.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `route_away`. |
| `state_key` | string | The demoted target's session/target key. |
| `capability_key` | string | The normalized capability token that routed the target away. |

```
INFO routectl_router::router event=route_away state_key=front
  capability_key=web_search
  "learned-capability negative de-prioritized this target to the tail"

WARN routectl_router::router event=route_away state_key=only
  capability_key=web_search
  "learned-capability negative routed this target away; request survives
   only via the de-prioritized learned tail"
```

### `count_tokens` (INFO)

Emitted once when a `count_tokens` seat reaches its class/remap/debit
settle point -- an upstream health error (rate-limit / server / timeout /
network / overload) that the token-count path surfaces as terminal (it
never falls back on health). The messages path emits a class-decision
event at every error arm; the token-count path was otherwise silent, so
this event makes a `count_tokens` breaker debit or park triageable. A
clean count (the happy path) and a capability walk (a wire-501 that
advances to the next capable seat) do NOT emit it. Safe dimensions only
-- never a body or prompt.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `count_tokens`. |
| `state_key` | string | The seat's session/target key. |
| `provider` | string | The provider name the seat dispatched to. |
| `status` | integer | The upstream HTTP status, or `0` for a transport/non-upstream error. |
| `upstream_type` | string | The upstream error `type` token, or empty. |
| `upstream_code` | string | The upstream error `code` token, or empty. |
| `effective_class` | string | The failure class after any operator remap (`rate_limited`, `server_error`, `timeout`, `network_error`, `overloaded`, `bad_request`, `unknown`, ...). |
| `matched_by` | string | How the native class was decided (`variant`, `status`, `upstream_type`). |
| `remapped` | bool | `true` when an operator per-provider status remap replaced the native class. |
| `debit` | bool | `true` when the class debited the seat's circuit breaker (or parked it on a rate-limit reset hint); `false` when the slot was released without a health debit. |

```
INFO routectl_router::router event=count_tokens state_key=haiku
  provider=prov status=500 upstream_type= upstream_code=
  effective_class=server_error matched_by=status remapped=false debit=true
  "count_tokens seat terminal; resilience class policy applied"
```

### Capability replay boundary (INFO / ERROR)

Emitted when a hot reload changes the catalog version or overlay revision.
Moving the replay boundary makes every earlier ledger row invisible to
later boots, so the reload re-appends the catalog-independent verdicts
past the new boundary in one atomic batch before publishing the
replacement router.

| Level | Message | Meaning |
|---|---|---|
| INFO | `capability replay boundary committed; catalog-independent verdicts restated` | The tombstone and every restatement are durable. `restated_survivors` counts the carried verdicts, `generation` is the registry generation now active, and `pruned_catalog_scoped` counts the entries the transition evicted. |
| ERROR | `capability replay boundary NOT admitted; keeping the previous router` | The batch was never queued (`reason`: unavailable writer or full channel). Nothing was written, no generation advanced, nothing pruned. |
| ERROR | `capability replay boundary NOT committed; keeping the previous router` | The batch was admitted and the transaction failed. `reason` names it; `pending_survivors` counts what would have been restated. The generation does not advance and no entry is pruned -- the boundary was never recorded, so evicting anything would discard live state. |
| WARN | `shutdown during a capability boundary write; publishing nothing` | Shutdown won the race against the admitted wait. The rows may or may not commit; nothing is published on the strength of them and the process does not resume serving. |

Every non-committed outcome rejects the reload and leaves the previous router
live. There is deliberately no timed-out outcome: answering while the queued
transaction can still commit would let the ledger's boundary move behind a
router that never adopted it.

Capability events are stamped with the registry generation that produced them
(the pending generation while a boundary is admitted), so an observation made
during a boundary sorts after it rather than being discarded. A `debug` line on
target `routectl_usage::writer` --
`dropped a capability event older than the committed replay boundary` --
records a straggler event the writer refused because a boundary has since
committed at a newer generation (`event_generation` / `boundary_generation`).
Persisting it would restore, on the next boot, state the boundary evicted.

At boot, the same acknowledged path writes the fail-closed boundary when the
ledger's tombstone is missing or stamped a different revision:

| Level | Message | Meaning |
|---|---|---|
| INFO | `committed fresh capability tombstone at boot (fail-closed replay boundary)` | This boot's boundary is durable. Verdicts learned this session sort after it and replay on the next boot. |
| ERROR | `capability boot tombstone NOT committed; verdicts learned this session may not survive a restart` | The boundary write failed. Boot continues (it never fails on the usage subsystem), but the consequence is named: this session's verdicts may sit before a stale boundary and be invisible to later boots. |

A reload that did NOT change either revision writes no boundary and emits
neither line.

### `invalidation` (WARN)

Emitted when a catalog or overlay change across a hot reload discards the
catalog-scoped learned capabilities (fresher config truth wins over
learned negatives). Entries whose truth does not depend on the catalog --
the envelope-field verdicts, keyed `field:<dotted.path>` -- are carried
across instead, and `carried_catalog_independent` counts them.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `invalidation`. |
| `catalog_changed` | bool | Whether the baked catalog version changed. |
| `overlay_changed` | bool | Whether the operator overlay revision changed. |
| `previous_catalog_version` | integer | The outgoing Router's catalog version. |
| `catalog_version` | integer | The incoming Router's catalog version. |
| `previous_overlay_revision` | integer | The outgoing Router's overlay revision. |
| `overlay_revision` | integer | The incoming Router's overlay revision. |
| `carried_catalog_independent` | integer | Entries carried across the change because their key is catalog-independent. |

```
WARN routectl_router::router event=invalidation catalog_changed=true
  overlay_changed=false previous_catalog_version=7 catalog_version=8
  previous_overlay_revision=0 overlay_revision=0
  carried_catalog_independent=1
  "catalog/overlay changed across reload; clearing catalog-scoped learned capabilities"
```

### Capability warm rebuild (INFO)

Emitted once at serve bootstrap after the learned-capability registry is
warmed from the usage ledger: `warmed learned-capability registry from
usage ledger`, on target `routectl_cli::server::capability_rebuild`. The
per-verdict fields (`replayed_verified`, `replayed_negative`,
`replayed_cleared`, `cleared_noop`, `replayed_probe`) tally what replayed;
`loaded_rows` and `row_cap` report the read.

Two skip tallies are deliberately separate, because they answer different
questions and only their combination distinguishes an empty verdict
history from a fully evicted one:

| Field | Type | Meaning |
|---|---|---|
| `skipped_unknown` | integer | Rows skipped because a persisted TOKEN (verdict / phase / source / tier / evidence class) is not one this build recognizes. |
| `skipped_revision` | integer | Catalog-scoped rows skipped because their stamped catalog / overlay revision is not the replay boundary's -- an eviction. Envelope-field rows are never counted here: their truth is catalog-independent, so they replay under a superseded revision. |

### `strip` (WARN)

Emitted once per capability-strip decision. `capability_key` names the
verdict's keys (already sorted + normalized; comma-joined at the
per-decision site, a single token at the probe-bypass site). `outcome`
is the stable decision token.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `strip`. |
| `state_key` | string | The target's session/target key. |
| `capability_key` | string | The normalized strip verdict key(s), comma-joined. |
| `outcome` | string | The stable decision token (vocabulary below). |

The `outcome` token is a stable contract -- pin alerts on it, not on the
message:

| Token | Meaning |
|---|---|
| `applied` | The strip ran and removed the capability surface from the attempt request. |
| `noop` | The verdict named a capability the request did not carry; nothing to strip. |
| `strict_rejected` | `strict_translation` is on; the strip would mutate, so the request is rejected before any change. |
| `validation_rolled_back` | The strip created a post-strip hazard; the request was restored byte-for-byte and the attempt routes away. |
| `probe_bypassed` | A strip-eligible feature was admitted for re-probe, so it was intentionally NOT stripped (emitted at the verdict site). |

A `disabled` kill switch (empty strip verdict) emits NO `strip` event --
the verdict is skipped entirely, so there is no per-decision context to
name.

```
WARN routectl_router::router event=strip state_key=nick
  capability_key=advisor outcome=applied "capability_strip_decision"
```

### `suppression` (WARN)

Emitted once per request (deduped) when an operator `force_supported`
override is contradicted: the masked capability was still rejected
upstream.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `suppression`. |
| `state_key` | string | The target's session/target key. |
| `capability_key` | string | The normalized capability token the operator forced on. |

```
WARN routectl_router::router event=suppression state_key=m1
  capability_key=unsupported_parameter
  "force_supported override contradicted: masked capability still rejected upstream"
```

### `dead_override_key` (WARN)

A config-layer event, emitted once per operator override key that
normalization rewrites: such a key can never match a normalized registry
key, so the override is dead.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `dead_override_key`. |
| `target_spec` | string | The override target (`provider` or `provider:nickname`). |
| `raw_key` | string | The operator's key as written in config. |
| `normalized_key` | string | What normalization rewrites it to (use this form instead). |

```
WARN routectl_router::override_registry event=dead_override_key
  target_spec=br raw_key=additionalModelRequestFields.anthropic_beta
  normalized_key=anthropic_beta
  "capability override key is rewritten by normalization; it can never
  match and is dead -- use the normalized form"
```

### `legacy_deprecation` (WARN)

A config-layer event, emitted exactly once on a serve cold-start or hot
reload when the loaded config carries any legacy capability-list key. It
names key NAMES only -- never config values (secrets can live near these
tables). `config check` never emits it.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `legacy_deprecation`. |
| `legacy_keys` | string | Debug-rendered list of the present legacy key names. |
| `successor` | string | Always `[capability.overrides]`. |
| `migrate_command` | string | Always `config migrate`. |

```
WARN routectl_cli::server event=legacy_deprecation
  legacy_keys=["unsupported_features", "allowed_betas"]
  successor=[capability.overrides] migrate_command="config migrate"
  "deprecated capability-list keys are set; they are tolerated for one
  release cycle and rejected at the next config schema version. Move them
  under [capability.overrides] with `config migrate`."
```
