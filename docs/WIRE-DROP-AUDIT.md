# Egress drop-arm audit: methodology and findings

This document records how to re-derive an inventory of "drop candidate"
match arms in routectl's egress translation code, and the findings from one
pass of that derivation. It is a snapshot of a derivation, not a frozen
inventory: re-run the commands below against current code before trusting
any row here. Nothing in this document claims the swept surfaces are fully
enumerated -- that property (an exhaustive, self-checking census) is a
separate, not-yet-built piece of work; this document only reports what one
targeted read-through found.

## Scope

Four egress surfaces, request-translation code only (not response
handling, not streaming state machines):

- `crates/routectl-providers/src/openai_compat/wire_lift/` (8 files)
- `crates/routectl-providers/src/bedrock/converse/`
- `crates/routectl-providers/src/gemini/`
- `crates/routectl-providers/src/openai_responses/` (egress-side; not the
  CLI-side ingress parser of the same name)

Within each surface, only functions that translate a canonical request
into a provider wire shape (`translate_*`, `build_*`, `lift*`, and the
match arms they contain) are candidates. Response deserialization,
streaming delta accumulation, and pure structural filters (skipping a
message because it belongs to a different match arm entirely, not because
content inside it was lost) are out of scope.

## Two lane classes

Every (ingress dialect, egress dialect) pairing in this codebase falls
into one of two classes:

- **Same-dialect pairing**: the client's wire dialect and the egress's
  wire dialect are the same (e.g. an OpenAI Chat Completions client
  routed to an OpenAI-compatible host; an OpenAI Responses client routed
  to an OpenAI Responses host). A drop on this pairing is a materially
  worse defect than a drop on a cross-dialect pairing: nothing needs
  translating, so nothing should be lost, and a silent loss here means
  routectl broke a request that would have worked unmodified.
- **Cross-dialect pairing**: the client's dialect differs from the
  egress's dialect (e.g. an Anthropic client routed to Bedrock Converse).
  Some content genuinely has no wire equivalent on the far side; a drop
  is acceptable only if it is deliberate (named, with a stated reason),
  logged (a structured `tracing::warn!` or `tracing::debug!` at
  construction time), and tested (a pinned pair: one test proving the
  drop fires, a sibling test proving a similar-but-representable shape
  does NOT drop, both asserting against real log capture rather than a
  hand-typed expectation of what a log "should" say).

A drop candidate on a same-dialect pairing must never be documented in
the findings table below as an acceptable, deliberate translation drop --
that misclassifies a worse bug as a survivable one. Every candidate found
here was checked against this before being scored.

Two of the four surfaces (Bedrock Converse, Gemini) can never be a
same-dialect pairing -- no ingress in this codebase speaks native
Converse or native Gemini wire shape, so every request reaching those
egresses is cross-dialect by construction. The other two
(`openai_compat/wire_lift/`, `openai_responses/`) share code between
same-dialect and cross-dialect callers; for those, "reachability" below
records whether the drop-triggering shape can actually be produced by
the matching same-dialect ingress, which is what determines whether a
given arm is a real same-dialect risk despite living in shared,
dialect-agnostic code.

## Derivation commands

Run from the repo root. These are the patterns that produced the table
below; re-running them is the only way to check the table for drift.

```sh
# Ok(None) returns inside translate_*/build_*/lift* functions
rg -n 'Ok\(None\)' \
  crates/routectl-providers/src/openai_compat/wire_lift/ \
  crates/routectl-providers/src/bedrock/converse/ \
  crates/routectl-providers/src/gemini/ \
  crates/routectl-providers/src/openai_responses/

# bare or comment-only match arms
rg -n '=>\s*\{\s*\}' \
  crates/routectl-providers/src/openai_compat/wire_lift/ \
  crates/routectl-providers/src/bedrock/converse/ \
  crates/routectl-providers/src/gemini/ \
  crates/routectl-providers/src/openai_responses/

# continue inside a per-part/per-detail loop
rg -n 'continue' \
  crates/routectl-providers/src/openai_compat/wire_lift/ \
  crates/routectl-providers/src/bedrock/converse/ \
  crates/routectl-providers/src/gemini/ \
  crates/routectl-providers/src/openai_responses/

# the shared reject-or-drop helper's call sites (openai_compat only)
rg -n 'reject_or_drop_unrepresentable\(' \
  crates/routectl-providers/src/openai_compat/wire_lift/

# whether a drop site's WARN is verified via real tracing capture
# anywhere in the same file (the presence of this harness is what
# distinguishes a properly pinned test from an absence-only assertion)
rg -n 'tracing_test::traced_test|logs_contain\(' <file>
```

Each `Ok(None)` / bare-arm / `continue` hit still needs a human read: the
patterns overmatch enormously (ordinary parsing logic, test assertions,
and structural role/message filters all produce the same greppable
shapes as a genuine content drop). A fourth pattern considered during
this pass, `=> \{` spanning multiple lines to catch comment-only arms
with a trailing statement, was tried and rejected: on these four
surfaces it matches hundreds of ordinary match arms and cannot be
narrowed further with plain regex. Treat every hit from the three
patterns above as a candidate to read, not a confirmed finding.

## How to score a candidate

For each: is it a genuine content-loss site (not a structural filter);
is it same-dialect-reachable or cross-dialect-only (see above); then
score three bars:

- **Deliberate**: the site carries a comment stating what is being
  dropped and why forwarding is not possible. A bare arm with no such
  comment fails this bar even if the drop is otherwise correct.
- **Logged**: a `tracing::warn!` or `tracing::debug!` fires at the drop
  site itself (not a downstream side effect). Zero log statement fails
  this bar regardless of how good the comment is.
- **Tested**: a test exists that captures the actual log line via a real
  tracing-capture harness (this codebase uses the `tracing_test` crate's
  `#[traced_test]` + `logs_contain(...)`), paired with a sibling test
  proving a similar shape that should NOT drop does not. A test that
  only asserts the output is absent -- with no log assertion and no
  paired positive control -- fails this bar even though it does exercise
  the drop.

## Findings

Legend: **Dial.** = same-dialect reachable (Y/N/? = undetermined in this
pass); D/L/T = deliberate/logged/tested.

### `openai_compat/wire_lift/`

| Module::function or arm | Dial. | D | L | T | Note |
|---|---|---|---|---|---|
| `content.rs::rewrite_parts` -- image block, unsupported source shape | N | partial | Y | N | Message states what's dropped but has no rationale comment; drop-triggering shapes (Anthropic `image`/`document` types) are not producible by the OpenAI Chat Completions ingress, so this is cross-dialect-only in practice despite the code being dialect-agnostic. No dedicated test exercises this branch. |
| `content.rs::rewrite_parts` -- document content block | N | Y | Y | N* | Has a rationale comment and a warn. Existing tests (`document_block_warn_drops_in_default_mode`, `document_block_strict_returns_err`) assert output shape only -- neither uses real tracing capture. See systemic note below. |
| `tool_result.rs::lift_inner_block` -- document / image-url-missing-url / image-unsupported-source-shape (three call sites in one function) | N | Y | Y | N* | Each has a rationale comment and a warn via the shared helper. Existing tests (e.g. `inner_document_block_dropped_lenient`) assert output shape only. |
| `tool_choice.rs::map_tool_choice` -- `{type:"tool"}` missing/invalid `name` | N | partial | Y | N | Warn message states the problem; no separate rationale comment. No dedicated test. |
| `tool_choice.rs::map_tool_choice` -- unrecognized shape | N | partial | Y | N | Same pattern; `unknown_shape_is_dropped` test asserts absence only. |
| `tool_choice.rs::lift` -- forcing tool_choice with no tools to force | N | Y | Y | N* | Rationale comment present; `forcing_tool_choice_without_tools_dropped_lenient` asserts absence only. |
| `tools.rs::lift` -- Anthropic builtin / non-custom tool | N | Y | Y | N* | Rationale comment present (in the surrounding match); `other_with_anthropic_builtin_warns_and_drops_non_strict` asserts absence only. |
| `response_format.rs::translate_format` -- unrecognized `format.type` (`_ => None`) | N | N | N | N | NEW finding, not previously flagged. No rationale comment, no log statement at all -- the arm silently returns `None` and the caller treats absence as "nothing to lift." Existing test `unknown_format_type_is_no_op` asserts absence only; there is no warn to capture. Reachable only via Anthropic-shape `provider_extras.output_config`, which the OpenAI ingress does not populate -- cross-dialect-only in practice. |
| `response_format.rs::translate_format` -- `json_schema` missing `schema` key (`obj.get("schema").cloned()?`) | N | N | N | N | Same function, same defect: the `?` on the `Option` silently exits with no log. No test found exercising a `json_schema` entry with no `schema` key. |

`N*` = the drop mechanics are otherwise solid (deliberate + logged), but
fail the tested bar strictly because the codebase's tracing-capture
harness (`tracing_test`) is available and used elsewhere in this
workspace but is not used anywhere in `wire_lift/`. Every existing test
in this directory that touches a drop site asserts the resulting shape
(field absent, key removed) rather than the log line, and none pairs
that with a captured-log assertion. This is a **systemic gap across the
whole directory**, not an isolated per-site issue -- fixing it once
(adopting the same `#[traced_test]` + `logs_contain(...)` pattern already
used in `bedrock/converse/` and `openai_responses/`, see below) would
close the tested bar for every row marked `N*` here.

### `bedrock/converse/`

| Module::function or arm | Dial. | D | L | T | Note |
|---|---|---|---|---|---|
| `messages.rs::translate_messages` -- `Role::System => {}` | N | N | N | N | Already flagged and being fixed by other in-flight work at the time of this pass; not re-reported as new. No log statement; the comment claims the case is already handled elsewhere, which is only true when a separate top-level system field is absent. |
| `messages.rs::emit_reasoning_blocks_converse` -- `ReasoningDetailKind::Summary \| ReasoningDetailKind::Other(_) => {}` | N | partial | N | N | Already flagged and being fixed by other in-flight work at the time of this pass; not re-reported as new. Sits ~30 lines from a sibling arm in the same match that IS tallied and warned. |
| `messages.rs::emit_reasoning_blocks_converse` -- `if detail.format != <the reasoning-detail format tag this egress replays> { continue; }`, present in both the `Text` and `Encrypted` arms of the same match | N | N | N | N | NEW finding. Zero tally, zero log, in both arms -- unlike the signature-empty case a few lines below in the same `Text` arm, which IS tallied and eventually warned when the tally is flushed. A non-native-format reasoning detail riding through on replay is silently dropped with no trace. |
| `tools.rs::append_tool_with_cache_point` -- Anthropic builtin tool | N | Y | Y | N | Rationale comment ("no equivalent shape available") and a warn. No test references this drop at all (searched for "builtin" in the file's test module; zero hits). |
| `extras.rs::insert_provider_extras` -- managed-key override attempt (debug) | N | Y | Y | N | Rationale comment and a debug log. No test exercises this specific branch. |
| `extras.rs::insert_provider_extras` -- client metadata fingerprint skip (debug) | N | Y | Y | partial | Rationale comment and a debug log. `client_metadata_fingerprint_skipped_from_converse_bag` asserts the bag does not contain the fingerprint, but does not use `#[traced_test]` / `logs_contain` -- absence-only, same systemic pattern as `wire_lift/`. |

For contrast: `extras.rs::insert_top_level_cache_control`'s warn-and-forward
site and `tools.rs::build_tool_config`'s dummy-toolConfig-injection warn
ARE tested with `#[traced_test]` + `logs_contain(...)` paired against a
"does not warn" sibling test. Those are not drops (nothing is lost, the
value is forwarded inert or a placeholder is injected), so they are not
findings, but they are the reference pattern the rows above should be
brought up to.

### `gemini/`

Gemini has no native ingress in this codebase, so every request reaching
this egress is cross-dialect by construction -- no same-dialect risk is
possible here regardless of arm.

| Module::function or arm | D | L | T | Note |
|---|---|---|---|---|
| `request.rs::content_part_to_part` -- `Image`, non-base64 source | Y | Y | Y | Well-covered: rationale comment, warn, and a `#[traced_test]`-backed assertion (`dropping non-base64 image source`). |
| `request.rs::content_part_to_part` -- `Image`, empty base64 data | Y | Y | Y | Same pattern, covered by a test asserting `"empty data"` under real capture. |
| `request.rs::content_part_to_part` -- `ImageUrl`, unparseable `data:` URI | Y | Y | Y | Covered (`dropping data: image_url`, under real capture). |
| `request.rs::content_part_to_part` -- `Document`, non-base64 / empty-data source | Y | Y | Y | Covered under real capture. |
| `request.rs::drop_redacted_thinking` (`RedactedThinking` arm) | Y | Y | Y | Already flagged in prior work and fixed before this pass began (now warns; a sibling arm ~40 lines away warns for the equivalent case). Function-level comment states this is a seed decision pending further evidence, matching the deliberate bar closely. Covered under real capture (`dropping redacted-thinking part`). |
| `request.rs::content_part_to_part` -- `File`, no inline base64 `file_data` | Y | N | N | NEW finding. Rationale comment and a warn exist, but no test references this message at all (`no inline base64 file_data`). Everything else in this function that drops has a matching `logs_contain` test; this one is the exception. |
| `request.rs::content_part_to_part` -- `ContentPart::Other` unknown block type (debug) | partial | Y | N | Comment is a one-line description rather than a stated rationale; debug log present; no test found for this message. Lower-severity: this is the same "unrecognized shape, forward-compat" family as the wire_lift passthrough arms, but here it drops rather than passing through verbatim. |

### `openai_responses/` (egress)

This surface is the one place among the four where a genuine same-dialect
pairing exists (an OpenAI Responses client routed to an OpenAI Responses
host), and it is also the only surface with runtime lane-awareness already
built in (`lift_reasoning_details` computes a lane from the auth kind and
checks per-format replayability before deciding whether a reasoning detail
rides the wire). This pass did not do a full line-by-line read of this
directory (it is the largest of the four, with roughly a hundred
warn/drop-shaped hits across its request-side files); the rows below are
what a targeted read of the message- and reasoning-translation functions
turned up, not a complete inventory of the directory.

| Module::function or arm | Dial. | D | L | T | Note |
|---|---|---|---|---|---|
| `messages.rs::translate_image_source` -- unrecognized source `type` | N | Y | Y | ? | Rationale comment, warn. Not yet checked against existing tests in this pass. |
| `messages.rs::translate_tool_image_source` -- unrecognized source `type` (tool result variant) | N | Y | Y | ? | Same pattern as above; not yet checked against tests. |
| `messages.rs::build_tool_output_body` -- unsupported tool result part type | N | Y | Y | ? | Warn present with rationale in the surrounding doc comment; not yet checked against tests. |
| `messages.rs::lift_reasoning_details` -- `ReasoningDetailKind::Other(_) => {}` | **?** | Y | N | partial | See the filed backlog item below -- this is the one candidate in this pass that could not be confidently classified as cross-dialect-only. It sits beside two sibling gates in the same function that ARE tallied and logged; this arm has neither. It has a paired test (`lift_skips_unrecognized_kind_detail` alongside a recognized-kind positive control), but the test only asserts item absence because there is no log to capture. Whether this arm is reachable from the same-dialect (Responses-to-Responses) pairing was not resolved in this pass -- it requires reading the Responses ingress's reasoning-detail parser, which is out of scope for an egress-only audit. **Not documented here as an accepted translation drop; filed to backlog pending that verification**, per the rule that a same-dialect drop candidate is a different and worse defect than a cross-dialect one. |

No other rows for this directory are reported as findings in this pass;
absence of a row is not a claim of a clean surface -- see Coverage below.

## Coverage: what this pass does and does not cover

Covered at file-level read depth: all 8 files in `openai_compat/wire_lift/`;
`bedrock/converse/messages.rs`, `tools.rs`, `system.rs`, `extras.rs` (deep
read); `bedrock/converse/response.rs`, `response_types.rs`, `eventstream.rs`
(scanned for candidate patterns; hits there were response-deserialization
and streaming-state-machine code, judged out of scope, not deep-read
line-by-line); `gemini/request.rs` (deep read on all content-part and
reasoning-detail translation arms; not deep-read on sampling-parameter and
tool-declaration arms); `gemini/schema.rs`, `gemini/cloudcode.rs`,
`gemini/mod.rs` (scanned only; `schema.rs` in particular looks like JSON
Schema shape-cleaning rather than message-content translation and was not
pursued further).

NOT covered at read depth in this pass: most of `openai_responses/`
(scanned for candidate patterns only; `messages.rs`'s tool-call and
tool-output translation paths beyond what's listed above, `extras.rs`,
`tools.rs`, `system.rs`, `request.rs` were not individually read for
deliberate/logged/tested scoring, nor checked for same-dialect
reachability). Any sweep of this directory should treat every row above
as a starting point, not a ceiling, and should re-run the derivation
commands rather than trusting this table's absence of a row as evidence
of a clean site.

This document does not claim exhaustiveness for any of the four surfaces.
A separate piece of work (a manifest-plus-test census that greps every
swept file for every candidate-arm pattern and asserts the found set
equals a maintained manifest exactly) is what would make completeness a
checkable property; until that lands, treat every table above as a
sample, not a census.
