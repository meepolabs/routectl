# Egress drop-arm audit: methodology

This document records how to re-derive an inventory of "drop candidate"
match arms in routectl's egress translation code, by hand, from scratch.
It carries no inventory of its own: the authoritative one is the
`TRANSLATION-DROP:` verdict marker at each arm, held honest by the census
tests described below. Use this file when you want to look for arms the
markers do not yet cover.

## The census is the inventory

Each arm's verdict lives in a `TRANSLATION-DROP:` marker beside the code it
describes. `crates/routectl-providers/tests/translation_drop_census.rs`
parses every marker out of the four surfaces and pins the population; three
welds hold the markers against the rest of the tree. Two have landed: the
COUNTED weld, comparing marker classes against the literals reachable from
the drop counters, and the SCOPE weld, asserting the four swept directories
hold exactly the files classified in scope plus the files explicitly
exempted with a reason. The third, the DECLARED-LOSS weld -- comparing
loss-declaring log statements against markers at symbol granularity -- is
NOT YET IN THE TREE. Stated as pending rather than omitted: a reader who
greps for it needs to find out it is absent, not conclude they misread.

THE CEILING, stated here because an unstated one gets read as a coverage
guarantee: no source-derived census can see a fully silent drop, because a
silent drop is defined by the ABSENCE of evidence -- there is no log to
harvest, no counter literal to resolve, and no marker unless a human wrote
one. The welds make divergence between authored verdicts and code loud.
They do not, and cannot, enumerate the arms nobody has looked at yet. That
is what the derivation below is for.

## Scope

Four egress surfaces, request-translation code only (not response
handling, not streaming state machines):

- `crates/routectl-providers/src/openai_compat/wire_lift/`
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

These directories hold both kinds of file, so which file is on which side
is not something to re-judge by eye: the scope weld pins the in-scope list
and the exemption list, each exemption with its reason, and refuses a file
on neither.

RENORMALIZE vs LOSE is the scope test, applied PER ARM. "This file looks
like normalization, not translation" is not a scope test and has already
been wrong once here at the cost of seven constraint-destroying arms in one
module: a module can renormalize almost everywhere and still destroy a
caller-written constraint in one arm, and a file-level judgement loses every
instance inside it.

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

A drop candidate on a same-dialect pairing must never be recorded as an
acceptable, deliberate translation drop -- that misclassifies a worse bug as
a survivable one. The marker grammar spells this out as its own verdict
(`fidelity-risk`), so such a candidate is filed as a defect rather than
wearing the verdict of an accepted drop.

Two of the four surfaces (Bedrock Converse, Gemini) can never be a
same-dialect pairing -- no ingress in this codebase speaks native
Converse or native Gemini wire shape, so every request reaching those
egresses is cross-dialect by construction. The other two
(`openai_compat/wire_lift/`, `openai_responses/`) share code between
same-dialect and cross-dialect callers, so for those the reachability of an
arm has to be derived rather than assumed.

Derive it from what the ingress SWEEPS, not from what it explicitly parses.
A forward-compat passthrough sweep of unknown top-level keys into
`provider_extras` makes every NON-CANONICAL key reachable by construction --
so "the matching ingress does not populate this field" is not available as a
reason on any shared, dialect-agnostic code path unless the field is a
canonical request key. Check the key against `is_canonical_request_key` and
read the ingress's sweep before concluding an arm is cross-dialect-only.

## Derivation commands

Run from the repo root. These are the candidate-shape patterns a by-hand
sweep starts from.

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
shapes as a genuine content drop). Treat every hit as a candidate to
read, not a confirmed finding.

Do NOT try to grow this pattern set into a gate. That was measured and it
does not converge: "code that silently loses data" is not syntactically
characterizable in Rust -- the same `let ... else` is a real drop in one
function on these surfaces and pure control flow in another, so the
predicate is semantic. Widening the set buys a few recall wins at several
times the cost in arms it red-fails on correctly, and a check that
red-fails on correct code gets loosened the first time it fires. That is
why the welds compare authored verdicts against counters and logs instead
of against syntax.

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

The verdict then goes in a `TRANSLATION-DROP:` marker at the arm, which is
where the census picks it up. It does not come back here.

## Lifecycle: this file is scaffolding, and it has reached its end state

This file is expected to SHRINK, not grow. The findings tables it once
carried were working notes for the per-surface sweeps; those sweeps have run
and the census is the inventory now, so the tables are deleted. Two
inventories of one thing is how the second one rots.

The rules that remain in force:

- The methodology stays. Its value is that anyone can re-derive from
  scratch, and that does not expire.
- No inventory returns here -- no table of arms, no per-file coverage claim,
  no list of what was and was not read. A coverage claim in prose is the one
  thing this document got most wrong: it was written as a note about reading
  depth and was read as a verdict on the code, which is how one module's
  losing arms stayed uninstrumented through three sweeps.
- A verdict on an arm goes in that arm's marker, never in a row here. A
  hand-patched row is indistinguishable from a stale one.
