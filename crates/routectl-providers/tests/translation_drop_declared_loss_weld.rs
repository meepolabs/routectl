//! The DECLARED-LOSS contract of the translation-drop census, welded at
//! SYMBOL granularity.
//!
//! Side A is the LOG: a `warn!` / `debug!` whose message matches a loss
//! vocabulary. Side B is the markers, each attributed to its NEAREST
//! ENCLOSING `fn`, DERIVED from position. Every symbol carrying a
//! loss-declaring log is accounted for on side B; every symbol carrying a
//! loss-declaring marker declares that loss through a log, a counter call, or
//! the `silent` tag.
//!
//! The marker parse is `translation_drop_census/marker.rs` and the counter
//! harvest is `translation_drop_census/counter.rs`, shared with the counted
//! weld and the scope weld -- including the LEXER (`code_only`,
//! `without_comments`), so this file adds no second scan over Rust source.
//!
//! # Why side A is the LOG and not the code's syntax
//!
//! Recorded so a future reader does not "improve" this back into a syntax
//! grep. Three waves of sweeps looked for syntactic drop shapes, and the
//! measurement that settled the question ran the pattern set in the PRECISION
//! direction (does each arm the pattern finds carry a marker?) rather than the
//! recall direction everyone had measured before:
//!
//! | pattern set                          | false positives |
//! |--------------------------------------|-----------------|
//! | the three originally specified       | 6               |
//! | plus `let ... else`                  | 38              |
//! | widened to all four surface dirs     | 64              |
//!
//! Adding `let ... else` buys ~7 recall wins for ~32 precision failures, and
//! the curve diverges monotonically from there: `?`-on-`Option` is ~177 sites
//! and "a function returning `None`" is every `-> Option<T>` in the tree.
//! Verified non-drops among those false positives include a whitespace
//! collapse and two pure control-flow `let ... else` uses. "Code that silently
//! loses data" is not syntactically characterizable in Rust -- the same token
//! is a drop in one file and control flow in another, so the predicate is
//! semantic. That is a CEILING, not a gap in the pattern list. A check that
//! red-fails on correct code gets loosened the first time it fires, which is
//! the one outcome this census exists to prevent.
//!
//! The log avoids all of it. It is the author's own semantic claim that
//! something is lost, written in the same commit as the marker, and it
//! SURVIVES the refactor syntax cannot: rewriting `match` -> `if let` ->
//! `let ... else` does not change `"dropping Anthropic-builtin tool on
//! Converse egress"`. It also fails in the SAFE direction -- a reworded
//! message drops out of side A and the weld stops asserting on that site,
//! rather than red-failing on correct code.
//!
//! It earns its place empirically rather than by argument: this side A found
//! six real uninstrumented drops across five files that three waves of
//! syntax-pattern sweeps missed. All six are instrumented in the tree this
//! weld runs against.
//!
//! # Why the anchor is DERIVED and must stay that way
//!
//! No `anchor=<symbol>` field is added to the marker grammar. The grammar
//! already refuses file paths and line numbers because they rot on move; an
//! authored symbol name rots on RENAME, silently, with no compiler signal.
//! The asymmetry that settles it: if the anchoring rule turns out wrong, a
//! derived anchor is one edit in this file, while an authored one is a
//! re-touch of every marker in the tree.
//!
//! # Why side A must be multiline
//!
//! `rustfmt` wraps a `warn!` body across lines and splits long messages with
//! a trailing `\` continuation. A LINE-scoped scan for the loss vocabulary
//! found 1 of 13 real sites on three files -- so the harvest reads each
//! macro's whole delimited argument list, and the weld would otherwise go
//! green by having almost nothing to check.
//!
//! # This weld is what holds the `structural` markers honest
//!
//! Fifty `structural` markers carry no counter literal, so the counted weld
//! cannot see them at all. What they do have is a symbol, and a `structural`
//! marker in a symbol that declares a loss is the hybrid lie the grammar
//! forbids for `class=` -- and unlike the `class=` case it is
//! machine-detectable. [`STRUCTURAL_BESIDE_A_LOSS_LOG`] is the register that
//! keeps each such pairing a reviewed judgement rather than an accident.
//!
//! # Every register is CONTENT-pinned, never SIZE-pinned
//!
//! A size pin lets one entry swap for another with no signal, which is the
//! difference between an exemption and a hole. Each register below names its
//! entries and each entry carries the reason it is there.
//!
//! THE CEILING, restated because this weld is the one most likely to be
//! over-read: a fully silent drop has no log to harvest, so no source-derived
//! side A can see it. The full statement lives in the module doc of
//! `translation_drop_census.rs`, which owns the `silent` human register.

use std::collections::{BTreeMap, BTreeSet};

#[path = "translation_drop_census/counter.rs"]
mod counter;
#[path = "translation_drop_census/marker.rs"]
mod marker;

use counter::{Counter, code_only};
use marker::{
    MARKER_TOKEN, Marker, Verdict, expect, holds_task_id, parse_file, production_files, read_source,
};

// ---------------------------------------------------------------------------
// Side A: the loss vocabulary.
// ---------------------------------------------------------------------------

/// The loss vocabulary, matched case-insensitively against a log macro's whole
/// argument list. Each token is an author SAYING content is going away.
///
/// Deliberately NOT widened past this set. Adding `skipped` / `stripped` /
/// `omits` grows side A from 87 to 92 sites and adds one symbol to the
/// register below without finding a single uninstrumented drop -- the extra
/// tokens land on aggregate tally logs whose own arms are already marked. The
/// tuning direction that matters is the one this vocabulary already covers.
const LOSS_VOCABULARY: &[&str] = &[
    "dropping",
    "dropped",
    "skipping",
    "omitting",
    "discarding",
    "stripping",
    "unrepresentable",
    "no equivalent",
];

/// The two log macros a loss declaration is written with. `error!` is absent
/// on purpose: a translation loss is a degradation the request survives, and
/// no arm in these surfaces declares one at `error!`.
const LOG_MACROS: &[&str] = &["warn", "debug"];

// ---------------------------------------------------------------------------
// The registers.
// ---------------------------------------------------------------------------

/// A shared logging helper, whose loss declaration is attributed to its
/// CALLERS rather than to itself. The verdict belongs to the arm whose
/// decision produces the loss; a helper handed a pre-computed decision takes
/// none, it only formats the log.
///
/// The rule that admits an entry here is DERIVED, not asserted: every
/// production call site of the symbol must itself carry a marker
/// ([`callers_all_marked`] computes it, and
/// [`a_shared_helper_with_one_unmarked_caller_is_not_resolved`] pins that an
/// unmarked caller breaks the resolution). The entry exists so the set is a
/// review moment: a helper joining it means a new log moved out from under
/// its arm's own verdict.
///
/// Marking a helper here instead would need a `class=` it cannot name -- one
/// helper serves several classes -- or a `structural` verdict on a function
/// that declares a real loss, which is the hybrid lie the grammar refuses in
/// the other direction.
const CALLER_ATTRIBUTED_HELPERS: &[(&str, &str, &str)] = &[
    (
        "openai_compat/wire_lift/mod.rs",
        "reject_or_drop_unrepresentable",
        "formats the strict-reject-or-warn-and-drop log for four lift arms; each caller carries \
         its own marker naming the class and the pinning test, and no single class fits a helper \
         serving all four",
    ),
    (
        "openai_responses/messages.rs",
        "translate_user_message",
        "logs the empty-after-translation skip for the role dispatcher above it; the marker sits \
         on the arm that decides the message carries nothing",
    ),
    (
        "openai_responses/messages.rs",
        "translate_other_message",
        "same empty-after-translation skip on the forward-compat role path, decided by the same \
         marked dispatcher",
    ),
    (
        "openai_responses/messages.rs",
        "walk_assistant_part",
        "per-part walker for one marked assistant turn; the unsupported-part logs belong to the \
         turn translation that routes each part",
    ),
];

/// Loss-declaring logs that neither sit in a marked symbol nor resolve
/// through the derivation, each with the reason it is legitimate.
/// CONTENT-pinned in both directions and keyed on a phrase from the LOG'S OWN
/// MESSAGE: the symbol name is not a unique key here, because three
/// per-request tallies in one file spell their emitter `flush`, and pinning on
/// the message is what makes a reworded declaration a review moment.
///
/// Every entry is one of two shapes, and both are stated rather than folded
/// into the resolution rule:
///
/// - a RESPONSE-side or transport log, in a file the scope weld classifies out
///   of the census: the loss costs model output or best-effort transport
///   state, never caller content, so no request-translation marker belongs on
///   it;
/// - a request-side log whose loss is already declared at a marked arm
///   elsewhere, where none of the three derivable resolutions applies -- the
///   symbol emits no counter beside its log, and a caller of it is itself
///   unmarked.
const EXPECTED_UNMARKED_LOSS_LOGS: &[(&str, &str, &str, &str)] = &[
    (
        "bedrock/converse/eventstream.rs",
        "handle_converse_frame",
        "skipping eventstream frame with no",
        "response-side: skips an untyped upstream stream frame, so the loss costs model output \
         rather than caller content",
    ),
    (
        "bedrock/converse/eventstream.rs",
        "handle_converse_frame",
        "skipping unknown eventstream frame",
        "response-side forward compat: tolerates an upstream frame kind this decoder does not \
         model, which costs model output rather than caller content",
    ),
    (
        "bedrock/converse/eventstream.rs",
        "handle_block_delta",
        "skipping text delta on a tool_use block",
        "response-side: refuses a delta that contradicts its own block's type, which would \
         corrupt the accumulator rather than lose caller content",
    ),
    (
        "bedrock/converse/eventstream.rs",
        "handle_block_delta",
        "skipping reasoning delta on a tool_use block",
        "response-side: the reasoning half of the same type-contradiction refusal",
    ),
    (
        "bedrock/converse/eventstream.rs",
        "handle_block_delta",
        "unknown content-block delta type",
        "response-side forward compat: tolerates a delta kind this decoder does not model, so the \
         loss costs model output rather than caller content",
    ),
    (
        "bedrock/converse/extras.rs",
        "build_additional_fields",
        "allowed_body_fields omits routectl-managed field",
        "warns that the operator's own allowed_body_fields list will drop a routectl-managed \
         key; the loss is the operator's configuration choice, not a wire translation, and the \
         bag filter downstream owns the drop",
    ),
    (
        "bedrock/converse/extras.rs",
        "insert_thinking",
        "dropping thinking.display",
        "the display strip goes because acceptance on this lane is unverified; the arms of this \
         same request path that carry verdicts own the extras policy actions, and the strip \
         itself has no deciding arm of its own to mark",
    ),
    (
        "bedrock/converse/messages.rs",
        "record",
        "dropping unrecognized document citations value",
        "per-document DEBUG of the citations tally, whose arm is marked at the document \
         translation that feeds it",
    ),
    (
        "bedrock/converse/messages.rs",
        "flush",
        "dropping unrecognized document citations value",
        "the citations tally's aggregate WARN, which records no counter of its own, so the \
         emitter derivation does not cover it; its arm is marked at the document translation",
    ),
    (
        "gemini/request.rs",
        "build_tool_config",
        "tool_choice forced a tool with no surviving declaration",
        "drops a tool_choice forcing whose target has no surviving declaration; deliberately not \
         counted again, because the declaration drop that emptied the list is already counted at \
         its own marked arm",
    ),
    (
        "gemini/sse.rs",
        "part_chunks",
        "functioncall beyond cap",
        "response-side streaming: truncates upstream output past a bounded-growth cap, so the \
         loss costs model output rather than caller content",
    ),
    (
        "openai_responses/client.rs",
        "drop",
        "skipping cookie jar persist",
        "transport teardown: skips a best-effort cookie-jar persist when no runtime remains, \
         which loses no request content at all",
    ),
    (
        "openai_responses/messages.rs",
        "build_user_content",
        "dropping unsupported user content part",
        "logs the unsupported user part drop for the marked role dispatch above it; one of its \
         two callers is itself caller-attributed rather than marked, so the chain does not close",
    ),
    (
        "openai_responses/messages.rs",
        "build_user_content",
        "dropping forward-compat user content part",
        "the forward-compat half of the same user part drop, on the same unclosed chain",
    ),
    (
        "openai_responses/messages.rs",
        "build_tool_output_body",
        "dropping unsupported tool result part",
        "logs the unsupported tool-result part drop on the all-text fast path; the tool-message \
         translation that decides it carries no marker of its own yet, which is what keeps this \
         entry a review moment",
    ),
    (
        "openai_responses/messages.rs",
        "build_tool_output_body",
        "dropping unsupported tool result part",
        "the mixed typed-items path of the same drop, which repeats the message verbatim -- two \
         occurrences, so the register counts rather than collapsing them to one",
    ),
    (
        "openai_responses/mod.rs",
        "complete",
        "output_item.done beyond cap",
        "response-side: truncates accumulated upstream output items past the same cap the stream \
         path applies",
    ),
    (
        "openai_responses/response.rs",
        "walk_output",
        "unknown message content block dropped",
        "response-side: drops an unmodeled upstream content block, so the loss costs model output \
         rather than caller content",
    ),
    (
        "openai_responses/response.rs",
        "walk_output",
        "unknown reasoning content block dropped",
        "response-side: the reasoning half of the same unmodeled-block tolerance",
    ),
    (
        "openai_responses/sse.rs",
        "parse_event",
        "skipping unknown stream event",
        "response-side streaming: skips an unknown upstream event kind at DEBUG, a forward-compat \
         tolerance on the response path",
    ),
    (
        "openai_responses/sse.rs",
        "handle_item_added",
        "output_item.added beyond cap",
        "response-side streaming: the bounded-growth cap on distinct output indices, which \
         protects the accumulator rather than losing caller content",
    ),
];

/// Loss-declaring log COUNT per symbol that direction one resolves. Pinned
/// because resolution is per-symbol while a declaration is per-line: without
/// this, a brand-new uninstrumented drop added inside an already-resolved
/// symbol passes every weld in this repository. Content-pinned by
/// `(file, symbol)`, so a count that moves either way is a review moment.
const EXPECTED_RESOLVED_LOG_COUNTS: &[(&str, &str, usize)] = &[
    ("bedrock/converse/extras.rs", "insert_operator_extras", 1),
    ("bedrock/converse/extras.rs", "insert_provider_extras", 1),
    ("bedrock/converse/messages.rs", "flush", 1),
    ("bedrock/converse/messages.rs", "translate_document", 2),
    ("bedrock/converse/messages.rs", "translate_image_source", 2),
    ("bedrock/converse/messages.rs", "translate_image_url", 1),
    ("bedrock/converse/messages.rs", "translate_known_part", 1),
    ("bedrock/converse/messages.rs", "translate_messages", 2),
    ("bedrock/converse/system.rs", "build_system", 2),
    (
        "bedrock/converse/tools.rs",
        "append_tool_with_cache_point",
        1,
    ),
    (
        "bedrock/converse/tools.rs",
        "passthrough_converse_tool_choice",
        1,
    ),
    (
        "bedrock/converse/tools.rs",
        "translate_tool_choice_string",
        1,
    ),
    (
        "bedrock/converse/tools.rs",
        "translate_typed_tool_choice",
        3,
    ),
    ("gemini/request.rs", "build_response_format", 1),
    ("gemini/request.rs", "build_tools_and_config", 2),
    ("gemini/request.rs", "content_part_to_part", 7),
    ("gemini/request.rs", "drop_redacted_thinking", 1),
    ("gemini/request.rs", "flush", 2),
    ("gemini/request.rs", "merge_payload_extras", 1),
    ("gemini/request.rs", "reasoning_details_to_thought_parts", 1),
    ("gemini/request.rs", "tool_call_to_function_call_part", 1),
    ("gemini/request.rs", "warn_dropped_cache_control", 1),
    (
        "openai_compat/wire_lift/mod.rs",
        "reject_or_drop_unrepresentable",
        1,
    ),
    (
        "openai_compat/wire_lift/response_format.rs",
        "translate_format",
        2,
    ),
    (
        "openai_compat/wire_lift/tool_choice.rs",
        "map_tool_choice",
        4,
    ),
    ("openai_responses/extras.rs", "responses_text_format", 5),
    ("openai_responses/messages.rs", "lift_reasoning_details", 1),
    ("openai_responses/messages.rs", "translate_image_source", 1),
    ("openai_responses/messages.rs", "translate_other_message", 1),
    (
        "openai_responses/messages.rs",
        "translate_tool_image_source",
        1,
    ),
    ("openai_responses/messages.rs", "translate_user_message", 1),
    ("openai_responses/messages.rs", "walk_assistant_part", 4),
    (
        "openai_responses/request.rs",
        "warn_dropped_cache_control",
        1,
    ),
    ("openai_responses/system.rs", "translate_system", 1),
    (
        "openai_responses/system.rs",
        "warn_on_cache_control_loss",
        1,
    ),
    (
        "openai_responses/tools.rs",
        "translate_tool_choice_object",
        2,
    ),
    (
        "openai_responses/tools.rs",
        "translate_tool_choice_string",
        1,
    ),
];

/// Loss-declaring markers whose symbol declares that loss NOWHERE the
/// derivation can see it -- no log, no counter call, and no callee that has
/// either. Each entry names where the declaration really lives.
///
/// The shape every entry shares: a pure decision function whose flag is
/// tallied and emitted by a separate per-request flush, which is the shape
/// this crate deliberately uses so one request emits one aggregated log
/// instead of one per dropped block. The counter and the log are at the
/// flush; the verdict is at the arm, which is where the grammar puts it.
const EXPECTED_UNDECLARED_LOSS_MARKERS: &[(&str, &str, &str, &str)] = &[
    (
        "bedrock/converse/messages.rs",
        "emit_reasoning_blocks_converse",
        "reasoning_signature_missing",
        "sets a flag on the reasoning skip tally; the WARN and the counter fire once per request \
         from that tally's own emitter",
    ),
    (
        "bedrock/converse/messages.rs",
        "emit_reasoning_blocks_converse",
        "reasoning_summary_unsupported",
        "same reasoning skip tally, second flag on the same per-request emitter",
    ),
    (
        "bedrock/converse/messages.rs",
        "emit_reasoning_blocks_converse",
        "reasoning_foreign_format_unsupported",
        "same reasoning skip tally, third flag on the same per-request emitter",
    ),
    (
        "bedrock/converse/messages.rs",
        "emit_reasoning_blocks_converse",
        "reasoning_foreign_format_unsupported",
        "same reasoning skip tally, third flag on the same per-request emitter",
    ),
    (
        "bedrock/converse/messages.rs",
        "drop_nested_tool_result_cache_control",
        "tool_result_cache_control",
        "a one-line arm that only records on the cache-control tally; the tally's emitter owns \
         both the WARN and the counter",
    ),
    (
        "gemini/request.rs",
        "build_thinking_config",
        "reasoning_effort_unrecognized",
        "sets a flag on this egress's per-request drop tally, whose flush emits the WARN and the \
         counter for every class in one place",
    ),
    (
        "gemini/schema.rs",
        "clean_object",
        "schema_keyword_unsupported",
        "the schema cleaner is a pure transformer that returns whether a constraint was lost; \
         keeping the log and the counter out of it is what stops a metrics call from inverting \
         that module's dependencies, and the egress tally owns both",
    ),
    (
        "gemini/schema.rs",
        "clean_object",
        "schema_keyword_unsupported",
        "the schema cleaner is a pure transformer that returns whether a constraint was lost; \
         keeping the log and the counter out of it is what stops a metrics call from inverting \
         that module's dependencies, and the egress tally owns both",
    ),
    (
        "gemini/schema.rs",
        "clean_object",
        "schema_keyword_unsupported",
        "the schema cleaner is a pure transformer that returns whether a constraint was lost; \
         keeping the log and the counter out of it is what stops a metrics call from inverting \
         that module's dependencies, and the egress tally owns both",
    ),
];

/// `structural` markers sitting in a symbol that also declares a loss. Each
/// is the machine-detectable form of the hybrid lie, so each is adjudicated
/// here rather than tolerated: a `structural` verdict may share a symbol with
/// a loss log only when the loss belongs to a DIFFERENT arm of that symbol.
///
/// Keyed by `(file, symbol, phrase from the reason)` so the entry is pinned to
/// the marker's own words. An occurrence count, not a set: three of these
/// share one file, one symbol and one reason, and a set would collapse them
/// so deleting two would stay green.
const STRUCTURAL_BESIDE_A_LOSS_LOG: &[(&str, &str, &str, &str)] = &[
    (
        "bedrock/converse/messages.rs",
        "translate_messages",
        "an empty translated block vec carries no content",
        "the symbol's loss logs are the empty-message skips this marker describes; every part \
         that did carry content was counted at its own arm",
    ),
    (
        "bedrock/converse/messages.rs",
        "translate_messages",
        "an empty translated block vec carries no content",
        "the assistant-role arm of the same empty-message skip",
    ),
    (
        "bedrock/converse/messages.rs",
        "translate_messages",
        "an empty translated block vec carries no content",
        "the forward-compat-role arm of the same empty-message skip",
    ),
    (
        "bedrock/converse/system.rs",
        "build_system",
        "a whitespace-only system block carries no instruction text",
        "the symbol's loss logs are the billing-attribution strips, a different arm; this marker \
         covers only the empty-block skip, which removes no instruction text",
    ),
    (
        "openai_compat/wire_lift/response_format.rs",
        "translate_format",
        "is not a format specification this egress can be said to have dropped",
        "the symbol's loss logs belong to the two counted arms below this one, each carrying its \
         own lane marker",
    ),
    (
        "openai_responses/tools.rs",
        "translate_tool_choice_object",
        "falls through to named-function extraction; no branch is terminal",
        "the symbol's loss logs belong to the two counted arms below; this marker sits on a \
         non-terminal fallthrough that discards nothing",
    ),
];

// ---------------------------------------------------------------------------
// A symbol: a `fn` and the span of its body.
// ---------------------------------------------------------------------------

/// One `fn` of one file, with the byte range of its body. The KEY is the
/// body's opening offset rather than the name: several tallies in these files
/// spell their emitter `flush`, and a name-keyed map would merge them into one
/// symbol whose logs and markers came from different arms.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Symbol {
    file: String,
    name: String,
    /// Offset of the body's `{`. Never pinned: it moves on every edit above.
    body_open: usize,
    body_end: usize,
}

/// Every `fn` of one source, with its body span, read over CODE ONLY so a
/// commented-out or stringified `fn` is not a symbol.
///
/// An empty result is legal per FILE: these surfaces hold serialize-only wire
/// type modules that declare no function at all. Emptiness is refused
/// POPULATION-wide in [`derive`] instead -- a scan that recovered no symbol
/// anywhere would attribute every log and every marker to nothing, which is
/// green by having nothing to weld.
fn symbols(file: &str, source: &str) -> Vec<Symbol> {
    let scan = code_only(source);
    let bytes = scan.as_bytes();
    let mut out = Vec::new();
    for (at, _) in scan.match_indices("fn ") {
        // `fn` must be its own token: `.rfn(` or `a_fn ` is not a definition.
        if at > 0 && {
            let prev = bytes[at - 1];
            prev.is_ascii_alphanumeric() || prev == b'_'
        } {
            continue;
        }
        let rest = &scan[at + "fn ".len()..];
        let name_len = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        if name_len == 0 {
            continue;
        }
        let name = &rest[..name_len];
        let Some((body_open, body_end)) = body_span(&scan, at + "fn ".len() + name_len) else {
            // A `fn` with no body is a trait signature or a fn-pointer type,
            // not a symbol markers can anchor to.
            continue;
        };
        out.push(Symbol {
            file: file.to_string(),
            name: name.to_string(),
            body_open,
            body_end,
        });
    }
    if out.is_empty() {
        // Legal: `types.rs` and its siblings declare wire structs only.
        let _ = file;
    }
    out
}

/// The `{ .. }` body following a signature that starts at `from`, as
/// `(open, end)`. `None` when the signature ends in `;` (no body) at
/// parameter depth zero.
fn body_span(scan: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = scan.as_bytes();
    // Walk the signature to its body brace. Parameter and generic depth must
    // be tracked: a default value or a closure argument in the signature
    // carries braces of its own that are not the body.
    let mut depth = 0i32;
    let mut cursor = from;
    let open = loop {
        match bytes.get(cursor)? {
            b'(' | b'[' | b'<' => depth += 1,
            b')' | b']' | b'>' => depth -= 1,
            b'{' if depth <= 0 => break cursor,
            b';' if depth <= 0 => return None,
            _ => {}
        }
        cursor += 1;
    };
    let mut braces = 0usize;
    let mut cursor = open;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'{' => braces += 1,
            b'}' => {
                braces -= 1;
                if braces == 0 {
                    return Some((open, cursor + 1));
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

/// The symbol whose body most tightly encloses `offset`, or `None` for an
/// offset at module level.
fn enclosing(symbols: &[Symbol], offset: usize) -> Option<&Symbol> {
    symbols
        .iter()
        .filter(|s| s.body_open <= offset && offset < s.body_end)
        .max_by_key(|s| s.body_open)
}

/// The symbol a marker at `offset` belongs to, DERIVED from position by two
/// rules in order:
///
/// 1. The marker's comment block introduces an item -- walk past the comment,
///    attribute and blank lines below it, and if a `fn` signature starts
///    there, that is the symbol. This is the doc-comment shape, where the
///    marker sits ABOVE the body it describes and so encloses nothing.
/// 2. Otherwise the marker sits INSIDE a body, so it belongs to the symbol
///    that encloses it.
///
/// Without rule 1 a marker in a function's own doc comment resolves to
/// whatever body happens to precede it, which is a wrong answer wearing the
/// shape of a right one.
fn marker_symbol<'a>(symbols: &'a [Symbol], source: &str, offset: usize) -> Option<&'a Symbol> {
    if let Some(introduced) = introduced_fn(symbols, source, offset) {
        return Some(introduced);
    }
    enclosing(symbols, offset)
}

/// The `fn` whose signature begins on the first non-comment, non-attribute,
/// non-blank line below `offset`, if any.
fn introduced_fn<'a>(symbols: &'a [Symbol], source: &str, offset: usize) -> Option<&'a Symbol> {
    let mut cursor = source[offset..].find('\n').map(|at| offset + at + 1)?;
    loop {
        let line_end = source[cursor..]
            .find('\n')
            .map_or(source.len(), |at| cursor + at);
        let line = source[cursor..line_end].trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with("#[") {
            if line_end >= source.len() {
                return None;
            }
            cursor = line_end + 1;
            continue;
        }
        // The first real line. A `fn` whose `fn` token sits on it -- after
        // any of `pub`, `pub(super)`, `const`, `async`, `unsafe`, `extern` --
        // is the item this marker introduces.
        return symbols
            .iter()
            .find(|s| s.body_open > cursor && signature_starts_on(source, cursor, line_end, s));
    }
}

/// Whether `symbol`'s signature starts within `[line_start, line_end)`. The
/// signature's `fn` token is what is located, so the modifiers before it do
/// not need enumerating.
fn signature_starts_on(source: &str, line_start: usize, line_end: usize, symbol: &Symbol) -> bool {
    let needle = format!("fn {}", symbol.name);
    source[line_start..line_end.min(source.len())]
        .find(&needle)
        .is_some_and(|at| {
            // The located `fn <name>` must be THIS symbol's, not a same-named
            // one elsewhere: its body has to be the first one after it.
            let token_at = line_start + at;
            symbol.body_open > token_at && symbol.body_open - token_at < line_end + 512 - line_start
        })
}

// ---------------------------------------------------------------------------
// Side A: harvesting the loss-declaring logs.
// ---------------------------------------------------------------------------

/// One loss-declaring log call.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LossLog {
    file: String,
    /// 1-based line of the macro token, for error attribution only.
    line: usize,
    /// The vocabulary tokens the message matched, so a reader of a failure
    /// can see WHY the site is on side A.
    matched: Vec<String>,
    /// The whole argument list, whitespace-normalized and lowercased. What the
    /// registers pin against: a symbol NAME is not a unique key here (three
    /// per-request tallies in one file spell their emitter `flush`), and the
    /// message is the author's own words, which is what a reviewer re-reads.
    message: String,
}

/// Every loss-declaring log in one source.
///
/// MULTILINE by construction: the macro's whole delimited argument list is
/// read, never the rest of its line. A line-scoped scan of this vocabulary
/// found 1 of 13 real sites on three files, because rustfmt wraps the body
/// and splits long messages with a trailing `\`.
///
/// Read over the source WITH its string literals intact -- the message IS a
/// literal -- but with the macro token located in CODE ONLY, so a
/// commented-out or stringified `warn!` is not a call.
fn loss_logs(file: &str, source: &str) -> Result<Vec<(LossLog, usize)>, String> {
    let scan = code_only(source);
    let mut out = Vec::new();
    for macro_name in LOG_MACROS {
        let token = format!("{macro_name}!");
        for (at, _) in scan.match_indices(&token) {
            // A longer identifier ending in this token is not this macro.
            if at > 0 && {
                let prev = scan.as_bytes()[at - 1];
                prev.is_ascii_alphanumeric() || prev == b'_'
            } {
                continue;
            }
            let after = at + token.len();
            let Some(open_at) = scan[after..]
                .find('(')
                .map(|off| after + off)
                .filter(|open| scan[after..*open].trim().is_empty())
            else {
                // A `warn!` with no argument list on the same statement is
                // not something to skip past: the harvest reads the message
                // from that `(` onward, so this shape means it would read
                // none. Fail loudly rather than lose a side-A site, since
                // fewer sites on side A is green by having less to check.
                return Err(format!(
                    "{file} names {token} in code with no argument list, so the harvest would \
                     read no message from it"
                ));
            };
            let args = delimited(source, open_at)?;
            let lowered = args.to_ascii_lowercase();
            let matched: Vec<String> = LOSS_VOCABULARY
                .iter()
                .filter(|term| lowered.contains(**term))
                .map(|term| (*term).to_string())
                .collect();
            if matched.is_empty() {
                continue;
            }
            out.push((
                LossLog {
                    file: file.to_string(),
                    line: source[..at].matches('\n').count() + 1,
                    matched,
                    message: lowered.split_whitespace().collect::<Vec<&str>>().join(" "),
                },
                at,
            ));
        }
    }
    Ok(out)
}

/// The macro's whole argument list, read with the SHARED lexer rather than a
/// local scan. The local one tracked `"` strings only: a char literal holding
/// `)` truncated the read (the site silently left side A -- green by having less
/// to check), and one holding `"` desynced the scan into a hard error on legal
/// code. Reusing the shared function is also what the module doc claims.
fn delimited(source: &str, open_at: usize) -> Result<&str, String> {
    counter::delimited(source, open_at, '(', ')')
}

// ---------------------------------------------------------------------------
// The population: both sides over the same files, keyed by symbol.
// ---------------------------------------------------------------------------

/// Both sides of the weld plus the call graph, over one `(file, source)`
/// population. Supplied rather than read from the tree, so the controls drive
/// the real derivation over a planted population.
#[derive(Debug, Default)]
struct Sides {
    /// Loss-declaring logs, by the symbol enclosing each.
    logs: BTreeMap<Symbol, Vec<LossLog>>,
    /// Markers, by the symbol each is attributed to.
    markers: BTreeMap<Symbol, Vec<Marker>>,
    /// Symbols holding a `record_translation_*` call.
    counting: BTreeSet<Symbol>,
    /// Callers of each symbol, resolved only for a name unique in the
    /// population -- an overloaded name resolves to no caller rather than to
    /// a guessed one.
    callers: BTreeMap<Symbol, BTreeSet<Symbol>>,
    /// Callees of each symbol, same uniqueness rule.
    callees: BTreeMap<Symbol, BTreeSet<Symbol>>,
}

fn derive(population: &[(String, String)]) -> Result<Sides, String> {
    let mut all: Vec<Symbol> = Vec::new();
    let mut per_file: BTreeMap<&str, Vec<Symbol>> = BTreeMap::new();
    for (file, source) in population {
        let found = symbols(file, source);
        all.extend(found.iter().cloned());
        per_file.insert(file.as_str(), found);
    }
    // Emptiness is refused here rather than per file: a serialize-only wire
    // type module legitimately declares no `fn`, but a scan that recovered
    // none ANYWHERE would attribute every log and marker to nothing, and both
    // sides emptying at once is what a broken scan looks like.
    if all.is_empty() {
        return Err(
            "the symbol scan recovered no `fn` from the whole population; an empty scan is a \
             failed scan, not a tree with nothing to attribute"
                .to_string(),
        );
    }

    // A name defined once in the population resolves a call; a name defined
    // twice resolves nothing. Guessing between two same-named `fn`s would
    // attribute a log to a symbol that never emits it, and a wrong
    // attribution looks resolved.
    let mut by_name: BTreeMap<&str, Vec<&Symbol>> = BTreeMap::new();
    for symbol in &all {
        by_name
            .entry(symbol.name.as_str())
            .or_default()
            .push(symbol);
    }
    let unique: BTreeMap<&str, &Symbol> = by_name
        .iter()
        .filter_map(|(name, found)| match found.as_slice() {
            [only] => Some((*name, *only)),
            _ => None,
        })
        .collect();

    let mut sides = Sides::default();
    for (file, source) in population {
        let found = &per_file[file.as_str()];
        let scan = code_only(source);

        for (log, at) in loss_logs(file, source)? {
            // A log at module level cannot exist -- a macro call is a
            // statement -- so an unenclosed one means the symbol scan lost a
            // body, which must fail rather than silently drop a side-A site.
            let symbol = enclosing(found, at).ok_or_else(|| {
                format!(
                    "{file} carries a loss-declaring log on line {} that no `fn` body encloses; \
                     the symbol scan lost a body",
                    log.line
                )
            })?;
            sides
                .logs
                .entry(symbol.clone())
                .or_default()
                .push(log.clone());
        }

        for marker in parse_file(file, source)? {
            let at = offset_of_line(source, marker.line);
            let symbol = marker_symbol(found, source, at).ok_or_else(|| {
                format!(
                    "the marker in {file} on line {} is attributed to no `fn`. Markers at module \
                     level, or on a `struct` / `impl` rather than a `fn`, need a content-pinned \
                     register entry rather than a loosened resolution rule.",
                    marker.line
                )
            })?;
            sides
                .markers
                .entry(symbol.clone())
                .or_default()
                .push(marker);
        }

        for counter in [Counter::Drop, Counter::PolicyAction] {
            for (at, _) in scan.match_indices(counter.token()) {
                if let Some(symbol) = enclosing(found, at) {
                    sides.counting.insert(symbol.clone());
                }
            }
        }

        for (at, callee) in call_sites(&scan, &unique) {
            let Some(caller) = enclosing(found, at) else {
                continue;
            };
            if caller == callee {
                continue;
            }
            sides
                .callers
                .entry(callee.clone())
                .or_default()
                .insert(caller.clone());
            sides
                .callees
                .entry(caller.clone())
                .or_default()
                .insert(callee.clone());
        }
    }
    Ok(sides)
}

/// Byte offset of the start of 1-based `line`.
fn offset_of_line(source: &str, line: usize) -> usize {
    source
        .split_inclusive('\n')
        .take(line - 1)
        .map(str::len)
        .sum()
}

/// Every `<name>(` call in already-blanked code whose name resolves to a
/// unique symbol, as `(offset, symbol)`. A method call (`x.flush()`) is
/// excluded by the preceding-`.` test only when the name is not unique
/// anyway; a uniquely named method is still the symbol it names.
fn call_sites<'a>(scan: &str, unique: &BTreeMap<&str, &'a Symbol>) -> Vec<(usize, &'a Symbol)> {
    let bytes = scan.as_bytes();
    let mut out = Vec::new();
    for (name, symbol) in unique {
        let needle = format!("{name}(");
        for (at, _) in scan.match_indices(&needle) {
            if at > 0 && {
                let prev = bytes[at - 1];
                prev.is_ascii_alphanumeric() || prev == b'_'
            } {
                continue;
            }
            // The definition is not a call site.
            if scan[..at].trim_end().ends_with("fn") {
                continue;
            }
            out.push((at, *symbol));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The two predicates, extracted so every control runs the SAME code the real
// assertions run. A control that only asserts its fixture proves the plant
// rather than the check: three predicates in this census shipped with both
// the assertion and its control still green after the predicate was neutered
// to a no-op.
// ---------------------------------------------------------------------------

/// Whether a symbol's loss declaration is attributable to its CALLERS: it has
/// at least one, and every one of them carries a marker. This is the derived
/// half of [`CALLER_ATTRIBUTED_HELPERS`] -- the register names which symbols
/// are expected to satisfy it, this decides whether they do.
fn callers_all_marked(sides: &Sides, symbol: &Symbol) -> bool {
    let Some(callers) = sides.callers.get(symbol) else {
        return false;
    };
    !callers.is_empty()
        && callers
            .iter()
            .all(|caller| sides.markers.contains_key(caller))
}

/// Whether a symbol is a per-request TALLY EMITTER: it declares the loss in a
/// log AND records the counter for it, in one place.
///
/// That pairing is this crate's standard shape for aggregating a loss -- an
/// arm sets a flag and one emitter logs and counts once per request, so a turn
/// dropping several blocks still emits one WARN. The emitter is not the
/// deciding site, so no verdict belongs on it: the arms that set its flags
/// carry the markers, and the counted weld already holds the emitter's class
/// literals against them.
///
/// DERIVED, not registered, because it is checkable from the code: a log
/// beside a counter call in one symbol is the emitter shape, and a log with no
/// counter beside it is not.
fn is_tally_emitter(sides: &Sides, symbol: &Symbol) -> bool {
    sides.logs.contains_key(symbol) && sides.counting.contains(symbol)
}

/// Direction one's predicate: whether a symbol's loss declaration is
/// accounted for. Extracted so every control runs THIS rather than a copy --
/// neutering it must break both the real assertion and its controls.
///
/// The three derivable ways, in order of how much they claim: the symbol
/// carries its own verdict; it is the tally emitter that logs and counts for
/// arms marked elsewhere; or every one of its callers carries a verdict.
fn log_is_accounted_for(sides: &Sides, symbol: &Symbol) -> bool {
    sides.markers.contains_key(symbol)
        || is_tally_emitter(sides, symbol)
        || callers_all_marked(sides, symbol)
}

/// Whether a symbol declares a loss where the derivation can see it: it logs
/// one, it counts one, or it delegates to a callee that does. The callee leg
/// is one hop by design -- an arm whose flag is tallied and flushed
/// elsewhere is the shape this crate uses everywhere, and a transitive walk
/// buys one extra resolution while making "declares a loss" reach most of the
/// call graph.
fn declares_a_loss(sides: &Sides, symbol: &Symbol) -> bool {
    if sides.logs.contains_key(symbol) || sides.counting.contains(symbol) {
        return true;
    }
    sides.callees.get(symbol).is_some_and(|callees| {
        callees
            .iter()
            .any(|callee| sides.logs.contains_key(callee) || sides.counting.contains(callee))
    })
}

/// Whether a marker claims a loss occurred. The prose verdicts that claim
/// nothing is lost (`structural`) are excluded, and `unresolved` is too: an
/// arm nobody could classify makes no claim about a loss either way.
const fn claims_a_loss(marker: &Marker) -> bool {
    matches!(
        marker.verdict,
        Verdict::Lane(_) | Verdict::PolicyAction | Verdict::FidelityRisk
    )
}

// ---------------------------------------------------------------------------
// The real population.
// ---------------------------------------------------------------------------

fn tree_population() -> Result<Vec<(String, String)>, String> {
    let mut population = Vec::new();
    for file in production_files()? {
        population.push((file.clone(), read_source(&file)?));
    }
    if population.is_empty() {
        return Err("the four surfaces hold no production source".to_string());
    }
    Ok(population)
}

fn tree_sides() -> Sides {
    expect(derive(&expect(tree_population())))
}

// ---------------------------------------------------------------------------
// THE WELD, in both directions.
// ---------------------------------------------------------------------------

#[test]
fn every_symbol_with_a_loss_declaring_log_is_accounted_for() {
    // Direction one. An author who wrote "dropping X" and no marker has
    // declared a loss the census does not know about -- which is exactly the
    // six real uninstrumented drops this side A found across five files that
    // three waves of syntax sweeps missed.
    //
    // The register comparison runs over OCCURRENCE COUNTS keyed on the log's
    // own message, not over a set of symbol names: `flush` names three
    // per-request tallies in one file, so a name-keyed set would let one
    // exemption cover a second, unreviewed declaration in a same-named symbol.
    let sides = tree_sides();
    assert!(
        !sides.logs.is_empty(),
        "no loss-declaring log recovered; side A is empty and the loop below asserts nothing"
    );

    // RESOLVED SYMBOLS ARE COUNT-PINNED, not skipped. Resolution is per-SYMBOL
    // (a marker, a counter beside the log, or every caller marked) while a log
    // declaration is per-LINE, so `continue`ing past a resolved symbol let a
    // BRAND NEW uninstrumented drop added inside it pass every weld -- one
    // marker immunized a whole function. Reproduced on six sites. The register
    // comparison below was already per-declaration; this makes resolution match
    // it, so a new declaration in a resolved symbol is a review moment.
    let mut resolved_counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut found: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for (symbol, logs) in &sides.logs {
        if log_is_accounted_for(&sides, symbol) {
            resolved_counts.insert((symbol.file.clone(), symbol.name.clone()), logs.len());
            continue;
        }
        for log in logs {
            let claimed = EXPECTED_UNMARKED_LOSS_LOGS
                .iter()
                .find(|(file, name, phrase, _)| {
                    symbol.file == *file && symbol.name == *name && log.message.contains(phrase)
                });
            let Some((file, name, phrase, _)) = claimed else {
                panic!(
                    "{}::{} declares a loss on line {} ({:?}) and nothing accounts for it: it \
                     carries no marker, records no counter beside the log, and does not resolve \
                     through its callers. Mark the arm that decides the loss, or register the log \
                     in EXPECTED_UNMARKED_LOSS_LOGS with the reason. Do NOT reword the message or \
                     narrow the vocabulary to make this pass. Message: {}",
                    symbol.file, symbol.name, log.line, log.matched, log.message
                );
            };
            *found
                .entry((
                    (*file).to_string(),
                    (*name).to_string(),
                    (*phrase).to_string(),
                ))
                .or_default() += 1;
        }
    }
    let mut pinned: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for (file, name, phrase, _) in EXPECTED_UNMARKED_LOSS_LOGS {
        *pinned
            .entry((
                (*file).to_string(),
                (*name).to_string(),
                (*phrase).to_string(),
            ))
            .or_default() += 1;
    }
    assert_eq!(
        found, pinned,
        "the unmarked-loss-log register drifted. An entry the census no longer finds is a stale \
         exemption claiming coverage of a site nobody looks at; a count that rose means a second \
         declaration slipped under a reviewed one."
    );

    // The other half of the same guarantee: a resolved symbol's log COUNT is
    // pinned, so a new declaration added under an existing verdict is red rather
    // than absorbed. Without this the resolution legs are blanket exemptions.
    let mut pinned_resolved: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (file, name, count) in EXPECTED_RESOLVED_LOG_COUNTS {
        pinned_resolved.insert(((*file).to_string(), (*name).to_string()), *count);
    }
    assert_eq!(
        resolved_counts, pinned_resolved,
        "the resolved-symbol log counts drifted. A count that ROSE means a new loss-declaring log \
         was added inside a symbol an existing verdict already covered, which no other assertion \
         here can see; a count that FELL means a declaration left and its pin is stale. Update the \
         pin only after confirming the new declaration is covered by the verdict that resolves it."
    );
}

#[test]
fn every_symbol_with_a_loss_claiming_marker_declares_that_loss() {
    // Direction two. A marker claiming a counted loss whose symbol declares
    // it nowhere is either an overclaim or an arm whose declaration moved --
    // and the counted weld cannot see the difference, because the class
    // literal still resolves from wherever the counter actually lives.
    let sides = tree_sides();
    assert!(
        !sides.markers.is_empty(),
        "no marker attributed to a symbol; side B is empty and the loop below asserts nothing"
    );

    // OCCURRENCE-COUNTED, not set-keyed. Three of these keys already cover
    // several real markers (one `clean_object` class covers three arms), so a set
    // let a fourth unreviewed marker hide under an existing key -- the exact
    // collapse the sibling registers avoid by counting.
    let mut registered: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
    for (file, symbol, class, _) in EXPECTED_UNDECLARED_LOSS_MARKERS {
        *registered.entry((*file, *symbol, *class)).or_default() += 1;
    }

    let mut undeclared: Vec<String> = Vec::new();
    let mut found_undeclared: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
    for (symbol, markers) in &sides.markers {
        if declares_a_loss(&sides, symbol) {
            continue;
        }
        for marker in markers.iter().filter(|m| claims_a_loss(m)) {
            // The `silent` tag IS a declaration of its own: the arm drops
            // with no log and no counter, on the record, in the census's own
            // human register.
            if marker.silent {
                continue;
            }
            let class = marker.class.as_deref().unwrap_or(marker.verdict.label());
            let key = (symbol.file.as_str(), symbol.name.as_str(), class);
            *found_undeclared.entry(key).or_default() += 1;
            if registered.contains_key(&key) {
                continue;
            }
            undeclared.push(format!(
                "{}::{} claims {class} on line {}",
                symbol.file, symbol.name, marker.line
            ));
        }
    }
    assert!(
        undeclared.is_empty(),
        "these markers claim a loss their symbol declares nowhere -- no log, no counter call, and \
         no callee with either: {undeclared:?}. Add the log or the counter, tag the arm `silent`, \
         or register it in EXPECTED_UNDECLARED_LOSS_MARKERS naming where the declaration lives."
    );

    // The register's counts must match what was actually found, so a SECOND
    // marker sharing a registered key cannot hide under the first.
    let found_registered: BTreeMap<(&str, &str, &str), usize> = found_undeclared
        .into_iter()
        .filter(|(key, _)| registered.contains_key(key))
        .collect();
    assert_eq!(
        found_registered, registered,
        "the undeclared-loss-marker register drifted. A count that ROSE means a second marker \
         slipped under a registered key; one that FELL means the entry is stale."
    );
}

#[test]
fn every_structural_marker_beside_a_loss_log_is_adjudicated() {
    // THE check that holds the `structural` markers honest. They carry no
    // counter literal, so the counted weld cannot see them at all; what they
    // do have is a symbol, and `structural` in a symbol that declares a loss
    // is the hybrid lie the grammar forbids for `class=` -- machine-detectable
    // here, unlike the `class=` case.
    //
    // COUNTED, not set-collapsed: three of the register's entries share one
    // file, one symbol and one reason phrase, so a set comparison would
    // absorb two deletions.
    let sides = tree_sides();
    let mut found: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for (symbol, markers) in &sides.markers {
        if !sides.logs.contains_key(symbol) {
            continue;
        }
        for marker in markers.iter().filter(|m| m.verdict == Verdict::Structural) {
            let claimed = STRUCTURAL_BESIDE_A_LOSS_LOG
                .iter()
                .find(|(file, name, phrase, _)| {
                    symbol.file == *file && symbol.name == *name && marker.reason.contains(phrase)
                });
            let key = match claimed {
                Some((file, name, phrase, _)) => (
                    (*file).to_string(),
                    (*name).to_string(),
                    (*phrase).to_string(),
                ),
                None => {
                    panic!(
                        "{}::{} carries a `structural` marker on line {} in a symbol that declares \
                         a loss: {:?}. A verdict claiming nothing is lost cannot share a symbol \
                         with a loss declaration unless the loss belongs to a DIFFERENT arm -- \
                         say so in STRUCTURAL_BESIDE_A_LOSS_LOG, or reclassify the arm.",
                        symbol.file, symbol.name, marker.line, marker.reason
                    );
                }
            };
            *found.entry(key).or_default() += 1;
        }
    }
    let mut pinned: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for (file, name, phrase, _) in STRUCTURAL_BESIDE_A_LOSS_LOG {
        *pinned
            .entry((
                (*file).to_string(),
                (*name).to_string(),
                (*phrase).to_string(),
            ))
            .or_default() += 1;
    }
    assert_eq!(
        found, pinned,
        "the structural-beside-a-loss-log register drifted. Each entry is an adjudication that \
         the symbol's loss belongs to a different arm than the `structural` marker; re-take that \
         judgement before updating the register."
    );
}

// ---------------------------------------------------------------------------
// The shared-helper resolution, and its own controls.
// ---------------------------------------------------------------------------

#[test]
fn every_caller_attributed_helper_really_has_only_marked_callers() {
    // The register names which symbols claim caller attribution; this is what
    // makes the claim checkable. An entry whose callers are not all marked
    // would be an exemption wearing the shape of a derivation.
    let sides = tree_sides();
    for (file, name, reason) in CALLER_ATTRIBUTED_HELPERS {
        let matches: Vec<&Symbol> = sides
            .logs
            .keys()
            .filter(|s| s.file == *file && s.name == *name)
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "CALLER_ATTRIBUTED_HELPERS names {file}::{name}, and the tree holds {} symbols of \
             that name carrying a loss-declaring log. Either the helper stopped logging a loss or \
             the entry is stale.",
            matches.len()
        );
        let symbol = matches[0];
        assert!(
            !sides.markers.contains_key(symbol),
            "{file}::{name} is registered as caller-attributed but now carries a marker of its \
             own; drop the register entry"
        );
        let callers = sides.callers.get(symbol).cloned().unwrap_or_default();
        assert!(
            !callers.is_empty(),
            "{file}::{name} has no production caller the derivation can see, so attributing its \
             declaration to callers attributes it to nothing"
        );
        let unmarked: Vec<String> = callers
            .iter()
            .filter(|caller| !sides.markers.contains_key(*caller))
            .map(|caller| format!("{}::{}", caller.file, caller.name))
            .collect();
        assert!(
            unmarked.is_empty(),
            "{file}::{name} is registered as caller-attributed, but these callers carry no \
             marker: {unmarked:?}. The verdict belongs at the arm that decides; mark them, or \
             move the helper to EXPECTED_UNMARKED_LOSS_LOGS."
        );
        assert!(
            reason.split_whitespace().count() >= 8,
            "{file}::{name}'s reason is too short to state why the declaration belongs to its \
             callers: {reason:?}"
        );
    }
    // THE MISSING DIRECTION. Everything above validates the entries that ARE
    // listed; nothing asserted the register names every symbol that actually
    // resolves this way -- so emptying it entirely left the whole binary green,
    // making it a claim of coverage with no mechanism. This closes it: the
    // register must equal the set the derivation resolves through callers.
    let resolved_via_callers: BTreeSet<(String, String)> = sides
        .logs
        .keys()
        .filter(|symbol| {
            !sides.markers.contains_key(*symbol)
                && !is_tally_emitter(&sides, symbol)
                && callers_all_marked(&sides, symbol)
        })
        .map(|symbol| (symbol.file.clone(), symbol.name.clone()))
        .collect();
    let listed: BTreeSet<(String, String)> = CALLER_ATTRIBUTED_HELPERS
        .iter()
        .map(|(file, name, _)| ((*file).to_string(), (*name).to_string()))
        .collect();
    assert_eq!(
        resolved_via_callers, listed,
        "CALLER_ATTRIBUTED_HELPERS must name exactly the symbols whose loss log resolves through \
         their callers. A symbol resolving that way without an entry is an unreviewed delegation; \
         an entry resolving some other way is stale."
    );
}

#[test]
fn a_shared_helper_with_one_unmarked_caller_is_not_resolved() {
    // Paired control on [`callers_all_marked`], run through the SAME
    // predicate the weld runs. Asserting only that the fixture has two
    // callers would prove the plant, not the check: the predicate could be
    // neutered to `true` and stay green.
    let both_marked = derive(&[(
        "gemini/planted.rs".to_string(),
        format!(
            "fn helper() {{\n    tracing::warn!(\"dropping a planted shape\");\n}}\n\
             // {MARKER_TOKEN} lane=gemini class=planted_first test=planted_first_drops\n\
             fn first_arm() {{\n    helper();\n}}\n\
             // {MARKER_TOKEN} lane=gemini class=planted_second test=planted_second_drops\n\
             fn second_arm() {{\n    helper();\n}}\n"
        ),
    )])
    .expect("the planted population derives");
    let helper = both_marked
        .logs
        .keys()
        .find(|s| s.name == "helper")
        .expect("the plant's helper carries the loss log");
    assert!(
        callers_all_marked(&both_marked, helper),
        "two marked callers must resolve the helper's declaration"
    );

    // One caller loses its marker: the helper is no longer attributable, so
    // the weld must fall back to demanding a register entry.
    let one_unmarked = derive(&[(
        "gemini/planted.rs".to_string(),
        format!(
            "fn helper() {{\n    tracing::warn!(\"dropping a planted shape\");\n}}\n\
             // {MARKER_TOKEN} lane=gemini class=planted_first test=planted_first_drops\n\
             fn first_arm() {{\n    helper();\n}}\n\
             fn second_arm() {{\n    helper();\n}}\n"
        ),
    )])
    .expect("the planted population derives");
    let helper = one_unmarked
        .logs
        .keys()
        .find(|s| s.name == "helper")
        .expect("the plant's helper carries the loss log");
    assert!(
        !callers_all_marked(&one_unmarked, helper),
        "one unmarked caller must break the attribution; otherwise a helper serving an unmarked \
         arm reads as covered"
    );

    // And with no caller at all, which is the shape a helper reached only
    // from outside the population takes.
    let no_callers = derive(&[(
        "gemini/planted.rs".to_string(),
        "fn helper() {\n    tracing::warn!(\"dropping a planted shape\");\n}\n".to_string(),
    )])
    .expect("the planted population derives");
    let helper = no_callers
        .logs
        .keys()
        .find(|s| s.name == "helper")
        .expect("the plant's helper carries the loss log");
    assert!(
        !callers_all_marked(&no_callers, helper),
        "a helper with no caller is attributable to nothing"
    );
}

// ---------------------------------------------------------------------------
// Mutation controls, one per direction of the weld. Each runs the real
// predicate over a planted population, so a predicate that stopped deciding
// is visible.
// ---------------------------------------------------------------------------

#[test]
fn a_marker_deleted_from_a_log_bearing_symbol_fails_the_weld() {
    // Direction one's mutation: the arm still declares its loss in the log,
    // the verdict is gone. This is what deleting one of the redundant markers
    // looks like -- invisible to the counted weld, because the class literal
    // still resolves from another arm that shares it.
    let marked = derive(&[(
        "gemini/planted.rs".to_string(),
        format!(
            "// {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops\n\
             fn arm() {{\n    tracing::warn!(\"gemini: dropping the planted shape\");\n}}\n"
        ),
    )])
    .expect("the planted population derives");
    let symbol = marked
        .logs
        .keys()
        .next()
        .expect("the plant's arm carries a loss log");
    assert!(
        marked.markers.contains_key(symbol),
        "the marked plant must attribute its marker to the same symbol as its log"
    );

    let unmarked = derive(&[(
        "gemini/planted.rs".to_string(),
        "fn arm() {\n    tracing::warn!(\"gemini: dropping the planted shape\");\n}\n".to_string(),
    )])
    .expect("the planted population derives");
    let symbol = unmarked
        .logs
        .keys()
        .next()
        .expect("the plant's arm carries a loss log");
    assert!(
        !unmarked.markers.contains_key(symbol),
        "a symbol whose marker was deleted must have no marker on side B"
    );
    assert!(
        !callers_all_marked(&unmarked, symbol),
        "and it must not resolve through callers either, or the deletion would pass"
    );
}

#[test]
fn a_loss_declaring_log_added_to_an_unmarked_symbol_fails_the_weld() {
    // Direction two's mutation, and the shape the six found drops took: a new
    // arm whose author wrote the WARN and no verdict.
    let quiet = derive(&[(
        "gemini/planted.rs".to_string(),
        "fn arm() {\n    tracing::warn!(\"gemini: forwarding the planted shape\");\n}\n"
            .to_string(),
    )])
    .expect("the planted population derives");
    assert!(
        quiet.logs.is_empty(),
        "a log with no loss vocabulary is not a side-A site: {:?}",
        quiet.logs
    );

    let loud = derive(&[(
        "gemini/planted.rs".to_string(),
        "fn arm() {\n    tracing::warn!(\"gemini: dropping the planted shape\");\n}\n".to_string(),
    )])
    .expect("the planted population derives");
    let symbol = loud
        .logs
        .keys()
        .next()
        .expect("the added loss log must land on side A");
    assert!(
        !loud.markers.contains_key(symbol) && !callers_all_marked(&loud, symbol),
        "an unmarked symbol that gained a loss-declaring log must be unaccounted for"
    );
}

#[test]
fn a_counted_marker_whose_declaration_was_removed_fails_the_weld() {
    // Direction two's other mutation, run through [`declares_a_loss`]: the
    // verdict stays, the log and the counter both go.
    let declaring = derive(&[(
        "gemini/planted.rs".to_string(),
        format!(
            "// {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops\n\
             fn arm() {{\n    tracing::warn!(\"gemini: dropping the planted shape\");\n}}\n"
        ),
    )])
    .expect("the planted population derives");
    let symbol = declaring
        .markers
        .keys()
        .next()
        .expect("the plant's marker is attributed");
    assert!(
        declares_a_loss(&declaring, symbol),
        "a symbol with a loss log declares its loss"
    );

    let silent_arm = derive(&[(
        "gemini/planted.rs".to_string(),
        format!(
            "// {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops\n\
             fn arm() {{\n    let _ = ();\n}}\n"
        ),
    )])
    .expect("the planted population derives");
    let symbol = silent_arm
        .markers
        .keys()
        .next()
        .expect("the plant's marker is attributed");
    assert!(
        !declares_a_loss(&silent_arm, symbol),
        "a symbol with neither a log, a counter, nor a declaring callee must not read as \
         declaring a loss"
    );

    // The counter alone is a declaration, and so is a callee that has one.
    let counted = derive(&[(
        "gemini/planted.rs".to_string(),
        format!(
            "// {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops\n\
             fn arm() {{\n    record_translation_drop(\"gemini\", \"planted_drop\");\n}}\n"
        ),
    )])
    .expect("the planted population derives");
    assert!(declares_a_loss(
        &counted,
        counted
            .markers
            .keys()
            .next()
            .expect("the plant's marker is attributed")
    ));

    let via_callee = derive(&[(
        "gemini/planted.rs".to_string(),
        format!(
            "fn emit() {{\n    record_translation_drop(\"gemini\", \"planted_drop\");\n}}\n\
             // {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops\n\
             fn arm() {{\n    emit();\n}}\n"
        ),
    )])
    .expect("the planted population derives");
    let arm = via_callee
        .markers
        .keys()
        .find(|s| s.name == "arm")
        .expect("the plant's marker is attributed to the arm");
    assert!(
        declares_a_loss(&via_callee, arm),
        "the tally-and-flush shape this crate uses everywhere must resolve through the callee"
    );
}

#[test]
fn a_silent_tagged_marker_needs_no_log_and_no_counter() {
    // The third way a marker may declare its loss. The tag's register is
    // empty in the tree, so without this the `silent` leg of the weld would
    // never run -- and an empty register plus an unexercised leg reads as
    // coverage that does not exist.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        format!(
            "// {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops silent\n\
             fn arm() {{\n    let _ = ();\n}}\n"
        ),
    )];
    let sides = derive(&population).expect("the planted population derives");
    let (symbol, markers) = sides
        .markers
        .iter()
        .next()
        .expect("the plant's marker is attributed");
    assert!(
        !declares_a_loss(&sides, symbol),
        "the plant's arm must declare nothing, so the `silent` leg is what carries it"
    );
    assert!(
        markers.iter().all(|m| m.silent && claims_a_loss(m)),
        "the plant must produce a loss-claiming marker carrying the tag: {markers:?}"
    );
}

// ---------------------------------------------------------------------------
// Controls on the anchoring rule itself.
// ---------------------------------------------------------------------------

#[test]
fn a_marker_in_a_doc_comment_anchors_to_the_function_below_it() {
    // The doc-comment shape, which is most of the population: the marker sits
    // ABOVE the body, so it encloses nothing and the enclosing-body rule alone
    // would attribute it to whatever body happens to precede it.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        format!(
            "fn earlier() {{\n    let _ = ();\n}}\n\n\
             /// Doc prose above the verdict.\n\
             /// {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops\n\
             #[allow(dead_code)]\n\
             pub fn later() {{\n    let _ = ();\n}}\n"
        ),
    )];
    let sides = derive(&population).expect("the planted population derives");
    let (symbol, _) = sides
        .markers
        .iter()
        .next()
        .expect("the plant's marker is attributed");
    assert_eq!(
        symbol.name, "later",
        "a marker in a doc comment belongs to the item it introduces, not to the body above it"
    );
}

#[test]
fn a_marker_inside_a_body_anchors_to_that_body() {
    // The other half of the rule. A marker on a match arm or a `let` inside a
    // function belongs to that function, and must NOT be pulled onto a nested
    // `fn` that happens to follow it.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        format!(
            "fn outer() {{\n    \
             // {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops\n    \
             let _ = ();\n\
             }}\n"
        ),
    )];
    let sides = derive(&population).expect("the planted population derives");
    let (symbol, _) = sides
        .markers
        .iter()
        .next()
        .expect("the plant's marker is attributed");
    assert_eq!(symbol.name, "outer");
}

#[test]
fn a_marker_introducing_a_nested_item_anchors_to_that_item() {
    // A marker inside one body that introduces a nested `fn` belongs to the
    // NESTED one. The doc-comment rule runs first for exactly this reason: the
    // enclosing-body rule would give the outer symbol, which is the more
    // permissive answer and the wrong one.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        format!(
            "fn outer() {{\n    \
             // {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops\n    \
             fn inner() {{\n        let _ = ();\n    }}\n    inner();\n\
             }}\n"
        ),
    )];
    let sides = derive(&population).expect("the planted population derives");
    let (symbol, _) = sides
        .markers
        .iter()
        .next()
        .expect("the plant's marker is attributed");
    assert_eq!(symbol.name, "inner");
}

#[test]
fn two_same_named_functions_are_two_symbols() {
    // The reason the symbol key is the body offset and not the name. Several
    // per-request tallies in these files spell their emitter `flush`; a
    // name-keyed map merges them, so one tally's marker would satisfy the
    // weld for another tally's log.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        "struct A;\nstruct B;\n\
         impl A {\n    fn flush(&self) {\n        tracing::warn!(\"dropping alpha\");\n    }\n}\n\
         impl B {\n    fn flush(&self) {\n        tracing::warn!(\"dropping beta\");\n    }\n}\n"
            .to_string(),
    )];
    let sides = derive(&population).expect("the planted population derives");
    assert_eq!(
        sides.logs.len(),
        2,
        "two same-named emitters must be two symbols, not one: {:?}",
        sides.logs
    );
    // And neither resolves a caller, since the name is ambiguous in the
    // population. Guessing between them would attribute a log to a symbol
    // that never emits it.
    for symbol in sides.logs.keys() {
        assert!(
            !sides.callers.contains_key(symbol),
            "an overloaded name must resolve no caller: {symbol:?}"
        );
    }
}

#[test]
fn a_marker_attributed_to_no_function_is_an_error() {
    // Markers at module level, or on a `struct` / `impl` rather than a `fn`,
    // are handled by a register entry rather than by loosening the rule -- so
    // the derivation has to FAIL on one instead of dropping it, which would
    // take the marker off side B and be green by having less to check.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        format!(
            "// {MARKER_TOKEN} lane=gemini class=planted_drop test=planted_arm_drops\n\
             struct NotAFunction;\n\
             fn unrelated() {{\n    let _ = ();\n}}\n"
        ),
    )];
    let why = derive(&population).expect_err("a marker on a struct is attributed to no fn");
    assert!(
        why.contains("attributed to no `fn`"),
        "unexpected reason: {why}"
    );
}

// ---------------------------------------------------------------------------
// Controls on side A's harvest. Each is aimed at a shape that demonstrably
// breaks a naive scan.
// ---------------------------------------------------------------------------

#[test]
fn the_harvest_reads_a_message_rustfmt_split_across_lines() {
    // THE reason side A is multiline. A line-scoped scan of this vocabulary
    // found 1 of 13 real sites on three files, because rustfmt wraps the
    // `warn!` body and splits a long message with a trailing `\`. Aimed at
    // the real tree, so it also fails if that formatting stops being the
    // shape the harvest is written against.
    let population = expect(tree_population());
    let mut wrapped = 0usize;
    let mut split_message = 0usize;
    for (file, source) in &population {
        let lines: Vec<&str> = source.lines().collect();
        for (log, _) in expect(loss_logs(file, source)) {
            let macro_line = lines[log.line - 1];
            if macro_line.trim_end().ends_with('(') {
                wrapped += 1;
            }
            // A message split with a `\` continuation: no single line of the
            // call holds the whole matched term set.
            let single_line_hit = LOSS_VOCABULARY.iter().any(|term| {
                lines
                    .iter()
                    .skip(log.line - 1)
                    .take(1)
                    .any(|line| line.to_ascii_lowercase().contains(term))
            });
            if !single_line_hit {
                split_message += 1;
            }
        }
    }
    assert!(
        wrapped > 0,
        "no loss-declaring log in the tree has its arguments wrapped onto later lines, so this \
         control is aimed at nothing. Re-aim it before trusting side A."
    );
    assert!(
        split_message > 0,
        "every loss-declaring log's vocabulary now sits on the macro's own line, so a line-scoped \
         scan would find them all and this control proves nothing about the multiline read"
    );

    // The check itself: reading only the macro's own line loses most of side
    // A. Run over the same population, so the comparison is real.
    let multiline: usize = population
        .iter()
        .map(|(file, source)| expect(loss_logs(file, source)).len())
        .sum();
    let line_scoped: usize = population
        .iter()
        .map(|(_, source)| {
            source
                .lines()
                .filter(|line| {
                    let lowered = line.to_ascii_lowercase();
                    LOG_MACROS
                        .iter()
                        .any(|m| lowered.contains(&format!("{m}!(")))
                        && LOSS_VOCABULARY.iter().any(|term| lowered.contains(term))
                })
                .count()
        })
        .sum();
    assert!(
        line_scoped * 4 < multiline,
        "a line-scoped scan found {line_scoped} of the {multiline} sites the multiline harvest \
         reads. If the gap has closed, the tree's formatting changed -- re-derive before relaxing \
         the harvest."
    );
}

#[test]
fn a_log_inside_a_comment_or_a_string_is_not_a_side_a_site() {
    // The census's lexer is shared for exactly this: a commented-out or
    // stringified `warn!` read as live invents a side-A site nothing can
    // account for, which red-fails correct code and gets a check loosened.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        "fn arm() {\n    let _note = \"tracing::warn!(\\\"dropping ghost\\\")\";\n    \
         // tracing::warn!(\"dropping commented\");\n    /* tracing::warn!(\"dropping blocked\"); \
         */\n    let _ = ();\n}\n"
            .to_string(),
    )];
    let sides = derive(&population).expect("the planted population derives");
    assert!(
        sides.logs.is_empty(),
        "a log inside a comment or a string is not a call: {:?}",
        sides.logs
    );
}

#[test]
fn a_log_beside_a_string_containing_a_comment_marker_is_still_harvested() {
    // The inverse, which cost the counter harvest a real call: a URL literal
    // on the log's own line made a naive scan read the line as commented out
    // and drop the site -- green by having less to check.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        "fn arm() {\n    let _u = \"https://example.com//v1\";\n    \
         tracing::warn!(\"gemini: dropping the planted shape\");\n}\n"
            .to_string(),
    )];
    let sides = derive(&population).expect("the planted population derives");
    assert_eq!(
        sides.logs.len(),
        1,
        "the log beside a URL literal must be seen: {:?}",
        sides.logs
    );
}

#[test]
fn the_vocabulary_match_is_case_insensitive_and_bounded_to_the_call() {
    // Case, because a message may open a sentence with the term; bounded to
    // the call, because a `warn!` whose neighbour mentions "dropping" in
    // prose is not itself a declaration.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        "fn arm() {\n    tracing::warn!(\"Dropping the planted shape\");\n}\n\
         fn neighbour() {\n    tracing::warn!(\"forwarding the planted shape\");\n    \
         let _ = \"dropping is mentioned here\";\n}\n"
            .to_string(),
    )];
    let sides = derive(&population).expect("the planted population derives");
    let named: BTreeSet<&str> = sides.logs.keys().map(|s| s.name.as_str()).collect();
    assert_eq!(
        named,
        BTreeSet::from(["arm"]),
        "only the call whose own message declares a loss is a side-A site: {:?}",
        sides.logs
    );
}

#[test]
fn a_log_macro_with_no_argument_list_is_an_error_rather_than_a_skipped_site() {
    // Skipping it would take a site off side A, and fewer sites there means
    // fewer things to account for -- green by having less to check.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        "fn arm() {\n    let alias = tracing::warn!;\n    let _ = alias;\n}\n".to_string(),
    )];
    let why = derive(&population).expect_err("a macro with no argument list cannot be read");
    assert!(why.contains("no argument list"), "unexpected reason: {why}");
}

// ---------------------------------------------------------------------------
// Non-vacuity and register hygiene.
// ---------------------------------------------------------------------------

#[test]
fn both_sides_of_the_weld_are_populated_across_all_four_surfaces() {
    // A side that emptied on one surface satisfies every assertion above for
    // that surface, which is the false green this census exists to refuse.
    let sides = tree_sides();
    for surface in marker::SURFACES {
        assert!(
            sides.logs.keys().any(|s| s.file.starts_with(surface)),
            "no loss-declaring log recovered from {surface}, which demonstrably carries them"
        );
        assert!(
            sides.markers.keys().any(|s| s.file.starts_with(surface)),
            "no marker attributed to a symbol in {surface}, which demonstrably carries them"
        );
    }
    // The floor is a real one, not `> 0`: side A runs to 87 sites over 53
    // symbols and side B to 115 markers over 66 symbols in the reviewed tree,
    // so a collapse to a handful must be red even though every surface is
    // still represented. Bounded rather than exact -- an added arm is ordinary
    // and the per-file marker pin in the census binary is what makes side B's
    // population exact.
    let log_sites: usize = sides.logs.values().map(Vec::len).sum();
    assert!(
        log_sites >= 60 && sides.logs.len() >= 40,
        "side A collapsed to {log_sites} sites over {} symbols; the reviewed tree carries far \
         more, so the harvest stopped reading something",
        sides.logs.len()
    );
    let markers: usize = sides.markers.values().map(Vec::len).sum();
    assert!(
        markers >= 90 && sides.markers.len() >= 50,
        "side B collapsed to {markers} markers over {} symbols; the parse or the attribution \
         stopped recovering them",
        sides.markers.len()
    );
}

#[test]
fn every_register_entry_carries_a_reason_a_reader_can_check() {
    // The reason IS the value of an escape entry: the weld only proves the
    // symbol was classified, so the reason is what a reviewer re-takes when
    // the symbol changes. A blank or placeholder reason is an unexplained
    // hole wearing the shape of a decision.
    let reasons = CALLER_ATTRIBUTED_HELPERS
        .iter()
        .map(|(file, name, reason)| (*file, *name, *reason))
        .chain(
            EXPECTED_UNMARKED_LOSS_LOGS
                .iter()
                .map(|(file, name, _, reason)| (*file, *name, *reason)),
        )
        .chain(
            EXPECTED_UNDECLARED_LOSS_MARKERS
                .iter()
                .map(|(file, name, _, reason)| (*file, *name, *reason)),
        )
        .chain(
            STRUCTURAL_BESIDE_A_LOSS_LOG
                .iter()
                .map(|(file, name, _, reason)| (*file, *name, *reason)),
        );
    let mut checked = 0usize;
    for (file, name, reason) in reasons {
        checked += 1;
        assert!(
            reason.split_whitespace().count() >= 8,
            "{file}::{name} is registered on {reason:?}, which is too short to state a reason a \
             reader can check"
        );
        assert!(
            !reason.contains('\n'),
            "{file}::{name}'s reason spans lines; one line keeps the registers readable"
        );
        assert!(
            !holds_task_id(reason),
            "{file}::{name}'s reason carries a planning id; state a reason a reader of this repo \
             can check instead of pointing at a board"
        );
    }
    assert!(
        checked >= 25,
        "only {checked} register entries were checked; the registers emptied rather than the \
         defects being fixed"
    );
}

#[test]
fn no_register_names_a_symbol_the_tree_does_not_carry() {
    // A stale entry claims an exemption for a symbol that is gone, which
    // reads as coverage of a site nobody is looking at any more.
    let sides = tree_sides();
    let present: BTreeSet<(&str, &str)> = sides
        .logs
        .keys()
        .chain(sides.markers.keys())
        .map(|s| (s.file.as_str(), s.name.as_str()))
        .collect();
    for (file, name) in CALLER_ATTRIBUTED_HELPERS
        .iter()
        .map(|(f, n, _)| (*f, *n))
        .chain(
            EXPECTED_UNMARKED_LOSS_LOGS
                .iter()
                .map(|(f, n, _, _)| (*f, *n)),
        )
        .chain(
            EXPECTED_UNDECLARED_LOSS_MARKERS
                .iter()
                .map(|(f, n, _, _)| (*f, *n)),
        )
        .chain(
            STRUCTURAL_BESIDE_A_LOSS_LOG
                .iter()
                .map(|(f, n, _, _)| (*f, *n)),
        )
    {
        assert!(
            present.contains(&(file, name)),
            "a register names {file}::{name}, which carries neither a loss log nor a marker in \
             the tree. Drop the entry."
        );
    }
}

#[test]
fn no_unmarked_loss_log_entry_covers_an_already_accounted_symbol() {
    // The direction the counted comparison cannot see. That comparison skips
    // an ACCOUNTED symbol before consulting the register, so an entry whose
    // symbol has since gained a marker (or become the tally emitter that logs
    // and counts) simply stops being reached -- and a stale exemption left in
    // place hides the next real declaration behind a name that already reads
    // as reviewed.
    let sides = tree_sides();
    for (file, name, phrase, _) in EXPECTED_UNMARKED_LOSS_LOGS {
        let matching: Vec<&Symbol> = sides
            .logs
            .iter()
            .filter(|(symbol, logs)| {
                symbol.file == *file
                    && symbol.name == *name
                    && logs.iter().any(|log| log.message.contains(phrase))
            })
            .map(|(symbol, _)| symbol)
            .collect();
        assert!(
            !matching.is_empty(),
            "EXPECTED_UNMARKED_LOSS_LOGS names {file}::{name} declaring {phrase:?}, which the \
             census does not find there; the message was reworded or the entry is stale"
        );
        for symbol in matching {
            assert!(
                !log_is_accounted_for(&sides, symbol),
                "{file}::{name} is registered as unaccounted for, but the derivation now covers \
                 it (a marker of its own, the log-and-count emitter shape, or every caller \
                 marked). Drop the entry so the next unaccounted log in this symbol is red."
            );
        }
    }
}

#[test]
fn every_undeclared_loss_marker_entry_still_declares_its_loss_nowhere() {
    // Same both-directions pin on the other register. An arm that gained its
    // own log or counter no longer needs an exemption.
    let sides = tree_sides();
    for (file, name, class, _) in EXPECTED_UNDECLARED_LOSS_MARKERS {
        let matches: Vec<&Symbol> = sides
            .markers
            .iter()
            .filter(|(symbol, markers)| {
                symbol.file == *file
                    && symbol.name == *name
                    && markers.iter().any(|m| m.class.as_deref() == Some(class))
            })
            .map(|(symbol, _)| symbol)
            .collect();
        assert!(
            !matches.is_empty(),
            "EXPECTED_UNDECLARED_LOSS_MARKERS names {file}::{name} carrying {class}, which the \
             census does not find there; the arm moved or the class was renamed"
        );
        for symbol in matches {
            assert!(
                !declares_a_loss(&sides, symbol),
                "{file}::{name} now declares its loss where the derivation can see it, so the \
                 {class} entry is stale; drop it"
            );
        }
    }
}

#[test]
fn the_symbol_scan_reads_the_functions_below_a_test_only_helper_attribute() {
    // The test-code exclusion is a FILE list, not a `#[cfg(test)]` cut -- one
    // surface file declares a test-only HELPER far above its test module, and
    // production markers live BELOW that attribute. A scan cutting at the
    // first `#[cfg(test)]` returns a shorter, still-plausible symbol set, and
    // fewer symbols on either side is green by having less to weld.
    const FILE: &str = "gemini/schema.rs";
    let source = expect(read_source(FILE));
    let found = symbols(FILE, &source);
    let first_cfg_test = source
        .lines()
        .position(|line| line.trim().starts_with("#[cfg(test)]"))
        .expect("the control needs the attribute it exists to describe");
    let cut_at = offset_of_line(&source, first_cfg_test + 1);
    let below: Vec<&Symbol> = found.iter().filter(|s| s.body_open > cut_at).collect();
    assert!(
        below.len() >= 2,
        "the control is no longer aimed at anything: {FILE} now holds {} symbols below its first \
         `#[cfg(test)]`, so a naive cut would lose little and this proves nothing. Re-aim it.",
        below.len()
    );
    assert!(
        below.iter().any(|s| s.name == "clean_object"),
        "the symbol scan lost a production function the file demonstrably carries below the \
         test-only helper: {below:?}"
    );
}
