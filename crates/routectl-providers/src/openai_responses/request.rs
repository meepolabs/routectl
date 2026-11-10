//! Canonical -> OpenAI Responses API request body translation.
//!
//! Orchestrator pattern (mirrors `bedrock::converse::request::translate`):
//! call into the per-concern sub-modules in a deterministic order and
//! assemble the final `ResponsesRequest`. Each sub-module owns one
//! field family: `system.rs` -> `instructions`, `messages.rs` ->
//! `input`, `tools.rs` -> `tools`/`tool_choice`, `extras.rs` ->
//! `reasoning` + the 8-key provider_extras allowlist.
//!
//! `store` defaults to `false` here; the `extras` module flips it for
//! non-ChatgptOauth auth_kinds when the operator opts in via
//! `provider_extras["store"]`. `parallel_tool_calls` forwards a Responses
//! client's own boolean and otherwise defaults to `true`, matching codex's
//! `ResponsesApiRequest` default.

use routectl_core::cache_control::{BreakpointPosition, CacheBreakpointSource};
use routectl_core::wire_check::{EditKind, EditSite, WireTap};
use routectl_core::{
    ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, RequestProvenance,
    ResponsesCacheBreakpoint, ResponsesPassthroughItem, Result, Role,
};
use serde_json::Value;

use super::messages::{
    UnknownParts, build_input, withholds_forwarded_file_url, withholds_user_file_url,
};
use super::system::{flush_fingerprint_tally, record_developer_turn_withhold, translate_system};
use super::tools::{translate_tool_choice, translate_tools};
use super::types::{ResponseInput, ResponseInputItem, ResponsesContentItem, ResponsesRequest};
use super::{AuthKind, OpenAiResponsesConfig, extras};
use crate::translation_drop_metrics::{
    ClientFingerprintStripTally, record_translation_drop, record_translation_lane_seen,
};

/// Build a fully-populated `ResponsesRequest` from a routectl
/// `ChatRequest`. The Provider's `complete()` toggles `stream` to
/// false / `stream()` toggles it true; this orchestrator builds with
/// `stream = false` and the caller flips as needed.
pub fn translate(cfg: &OpenAiResponsesConfig, req: &ChatRequest) -> Result<ResponsesRequest> {
    let _observation = crate::translation_drop_metrics::ObservationScope::enter(req);
    // The ONE per-lane denominator site for `openai-responses`. Placed at the
    // top of the orchestrator rather than inside a sub-module: this is the
    // only function every request on this lane passes through exactly once,
    // and it runs before the first `?` so a request that FAILS translation
    // still counts toward the lane's volume -- a drop rate whose denominator
    // omitted the failures would read low for exactly the requests that went
    // worst.
    record_translation_lane_seen(super::LANE);
    count_unmodeled_thinking_type(req);
    log_dropped_cache_control(req);
    count_withheld_sampling_params(req);
    // The canonical sampling knobs have no Responses-API home and are gated
    // out of the provider_extras merge as canonical keys; log once at DEBUG
    // (an expected transform).
    crate::sampling_drop_guard::log_dropped_sampling_fields(&cfg.id, req, &[]);

    let tap = crate::wire_declare::tap(req);
    let mut fingerprint = ClientFingerprintStripTally::default();
    let mut cache_withheld = extras::PromptCacheWithheldTally::default();
    let instructions = translate_system(req, &mut fingerprint);
    declare_instructions(tap, instructions.is_some(), &fingerprint);
    record_developer_turn_withhold(&req.messages, &mut fingerprint);
    let instructions = instructions.unwrap_or_default();
    let input = build_input_with_passthrough(&cfg.id, cfg.auth_kind, req, tap, &mut cache_withheld);
    // Flushed before the input build's `?`, so a request that withheld the
    // fingerprint and then failed translation still counts.
    flush_fingerprint_tally(&fingerprint);
    // The prompt-cache tally is shared with the top-level fields merged
    // below, so it flushes once: here only when the input build failed.
    let input = match input {
        Ok(input) => input,
        Err(e) => {
            cache_withheld.flush();
            return Err(e);
        }
    };
    let tools = translate_tools(req);
    let tool_choice = translate_tool_choice(req.tool_choice.as_ref());

    let mut request = ResponsesRequest {
        model: req.model.clone(),
        instructions,
        input,
        tools,
        tool_choice,
        parallel_tool_calls: resolve_parallel_tool_calls(req).value,
        reasoning: None,
        // Every lane starts at store=false; extras.rs honors a
        // provider_extras["store"] override only on lanes that allow it.
        store: false,
        stream: false,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        prompt_cache_options: None,
        prompt_cache_retention: None,
        text: None,
        client_metadata: None,
        // Codex's `ResponsesApiRequest` has no `max_output_tokens` member,
        // and chatgpt.com's codex backend rejects fields the client never
        // sends. Other auth_kinds (ApiKey, BedrockMantle) accept it as a
        // documented top-level field, so forward the caller's ceiling there.
        // Baked seed verdict: this provider kind sits outside field repair,
        // and its refusal can arrive as a status-0 `response.failed` after
        // the stream has started, too late to retry invisibly.
        // SEED-VERDICT: acceptance-seed -- the refusal can arrive mid-stream
        // outside field repair, so the ceiling is withheld before send.
        max_output_tokens: if cfg.auth_kind.is_codex_lane() {
            None
        } else {
            req.max_tokens
        },
    };

    if let Some(tap) = tap {
        declare_lane_gates(tap, cfg.auth_kind, req);
    }
    extras::apply_reasoning(&mut request, req, tap);
    extras::merge_provider_extras(&mut request, req, cfg.auth_kind, tap, &mut cache_withheld);
    cache_withheld.flush();
    // Runs after merge_provider_extras so a client or operator key wins.
    extras::apply_session_cache_key(&mut request, req, cfg.auth_kind, tap);
    // Runs after merge_provider_extras so the stamp lands on the merged
    // object; the resolved id wins over any request-carried value.
    extras::apply_installation_id(
        &mut request,
        cfg.auth_kind,
        cfg.installation_id.as_deref(),
        tap,
    );
    // Honor the canonical structured-output directive onto `text.format`.
    // Runs after merge_provider_extras so a `verbosity` sibling lifted into
    // provider_extras["text"] survives and the format merges alongside it.
    extras::apply_response_format(&mut request, req, tap);
    // Runs last: the encrypted-reasoning include depends on the final
    // `store` value (which merge_provider_extras may have flipped) and
    // on whether the operator pinned `include` explicitly.
    extras::finalize_reasoning_include(&mut request, req, tap);

    Ok(request)
}

/// Build the `input[]` array: the modeled items translated from the
/// canonical `messages[]`, with any Responses item kinds this hub does
/// not model spliced back into their original inbound positions from
/// `req.routectl_internal.responses_input_passthrough`.
///
/// The passthrough items are the Responses ingress's capture of
/// codex-only kinds (`local_shell_call`, `custom_tool_call(_output)`,
/// `tool_search_call`, `agent_message`, ...) that no canonical field
/// models; replaying them keeps a codex multi-turn conversation whole
/// instead of dropping them. Each carries a `modeled_prefix` (the count
/// of modeled INPUT items that preceded it inbound), so it is re-emitted
/// after that many modeled EGRESS items -- preserving original source
/// order rather than appending everything to the tail. Preserve-and-
/// passthrough only: the raw JSON is forwarded unchanged, no cross-
/// dialect translation. Empty for every request that did not enter
/// through the Responses ingress.
fn build_input_with_passthrough(
    id: &str,
    auth_kind: AuthKind,
    req: &ChatRequest,
    tap: Option<&WireTap>,
    cache_withheld: &mut extras::PromptCacheWithheldTally,
) -> Result<Vec<ResponseInput>> {
    let provenance = req.routectl_internal.provenance;
    let mut items = build_input(
        id,
        auth_kind,
        provenance,
        &req.messages,
        tap,
        cache_withheld,
    );
    if provenance == RequestProvenance::ResponsesIngress {
        place_cache_breakpoints(
            id,
            auth_kind,
            &req.routectl_internal.responses_cache_breakpoints,
            &req.messages,
            items.as_deref_mut().ok(),
            tap,
            cache_withheld,
        );
    }
    let items = items?;
    let modeled: Vec<ResponseInput> = items.into_iter().map(ResponseInput::Item).collect();
    let passthrough = &req.routectl_internal.responses_input_passthrough;
    let merged = merge_passthrough(modeled, passthrough);
    if let Some(tap) = tap {
        declare_passthrough_relocations(tap, &merged, passthrough);
    }
    Ok(merged)
}

/// Put each client `prompt_cache_breakpoint` back on the `input_text` block
/// it came from. Only the api-key surface takes one; the ChatGPT-OAuth and
/// Mantle surfaces have no recorded acceptance of the field.
///
/// A block is identified by its ordinal in [`super::breakpoint_blocks`],
/// the one rule the ingress and this egress share, and by the digest of its
/// text: both must match, so a breakpoint never lands on a block whose text
/// changed between ingress and egress.
/// Every breakpoint left unplaced (refused by the surface, lifted into
/// `instructions`, moved, rewritten, or `items` absent because the input
/// build failed) is recorded in `cache_withheld` and declared as a Remove
/// at the client's block: an auth-surface gate on a surface that takes
/// none, a dialect strip where the block it named is gone.
/// TRANSLATION-DROP: lane=openai-responses class=prompt_cache_field_withheld test=unplaced_breakpoints_count_once_and_declare
fn place_cache_breakpoints(
    id: &str,
    auth_kind: AuthKind,
    breakpoints: &[ResponsesCacheBreakpoint],
    messages: &[Message],
    items: Option<&mut [ResponseInputItem]>,
    tap: Option<&WireTap>,
    cache_withheld: &mut extras::PromptCacheWithheldTally,
) {
    if breakpoints.is_empty() {
        return;
    }
    let mut placed = vec![false; breakpoints.len()];
    let surface_accepts = extras::PromptCacheField::Breakpoint.forwards_on(auth_kind);
    if let (true, Some(items)) = (surface_accepts, items) {
        attach_matching_breakpoints(breakpoints, messages, items, &mut placed);
    }
    let unplaced: Vec<&ResponsesCacheBreakpoint> = breakpoints
        .iter()
        .zip(&placed)
        .filter(|(_, placed)| !**placed)
        .map(|(breakpoint, _)| breakpoint)
        .collect();
    if unplaced.is_empty() {
        return;
    }
    cache_withheld.record();
    tracing::debug!(
        provider = id,
        unplaced = unplaced.len(),
        "dropping prompt_cache_breakpoint values the Responses egress could not place"
    );
    if let Some(tap) = tap {
        let site = if surface_accepts {
            EditSite::DialectFieldStrip
        } else {
            EditSite::AuthSurfaceGate
        };
        for breakpoint in unplaced {
            crate::wire_declare::declare(
                tap,
                site,
                format!(
                    "{}/{}",
                    breakpoint.client_pointer,
                    extras::PromptCacheField::Breakpoint.key()
                ),
                EditKind::Remove,
            );
        }
    }
}

/// Set each breakpoint on the emitted `input_text` block at its ordinal in
/// [`super::breakpoint_blocks`], the rule the ingress recorded it against,
/// when that block's text digest matches, marking it in `placed`. When the
/// emitted stream disagrees with the rule nothing is placed: a breakpoint
/// never lands on a block the ingress did not mean.
fn attach_matching_breakpoints(
    breakpoints: &[ResponsesCacheBreakpoint],
    messages: &[Message],
    items: &mut [ResponseInputItem],
    placed: &mut [bool],
) {
    let expected = super::breakpoint_blocks(messages);
    let mut emitted: Vec<(&String, &mut Option<Value>)> = items
        .iter_mut()
        .filter_map(|item| match item {
            ResponseInputItem::Message { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ResponsesContentItem::InputText {
                text,
                prompt_cache_breakpoint,
            } => Some((&*text, prompt_cache_breakpoint)),
            _ => None,
        })
        .collect();
    let agrees = emitted.len() == expected.len()
        && emitted
            .iter()
            .zip(&expected)
            .all(|((text, _), block)| text.as_str() == block.text);
    if !agrees {
        return;
    }
    for (index, breakpoint) in breakpoints.iter().enumerate() {
        let Some(ordinal) = breakpoint.ordinal else {
            continue;
        };
        let Some((text, slot)) = emitted.get_mut(ordinal) else {
            continue;
        };
        if slot.is_none() && breakpoint.text_digest == extras::breakpoint_text_digest(text) {
            **slot = Some(breakpoint.value.clone());
            placed[index] = true;
        }
    }
}

/// Declare a Relocate for each spliced passthrough item that left at a
/// different `input[]` index than it arrived at. Its inbound index is its
/// modeled prefix plus the passthroughs before it; a modeled item that
/// expands to a different egress-item count shifts it.
fn declare_passthrough_relocations(
    tap: &WireTap,
    merged: &[ResponseInput],
    passthrough: &[ResponsesPassthroughItem],
) {
    let egress_positions = merged
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item, ResponseInput::Passthrough(_)))
        .map(|(position, _)| position);
    for (ordinal, (item, egress)) in passthrough.iter().zip(egress_positions).enumerate() {
        let inbound = item.modeled_prefix.saturating_add(ordinal);
        if inbound == egress {
            continue;
        }
        for position in [inbound, egress] {
            crate::wire_declare::declare(
                tap,
                EditSite::ResponsesPassthroughSplice,
                format!("/input/{position}"),
                EditKind::Relocate,
            );
        }
    }
}

/// Declare the `instructions` edits: the field is always sent, so a
/// request without a system prompt gains an empty one, and a withheld
/// fingerprint block rewrites it (to the empty string when nothing else
/// was there).
fn declare_instructions(
    tap: Option<&WireTap>,
    has_system: bool,
    fingerprint: &ClientFingerprintStripTally,
) {
    let Some(tap) = tap else {
        return;
    };
    if !has_system {
        crate::wire_declare::declare(
            tap,
            EditSite::RequiredFieldDefault,
            "/instructions",
            EditKind::Add,
        );
    }
    if fingerprint.stripped() {
        crate::wire_declare::declare(
            tap,
            EditSite::ClientFingerprintStrip,
            "/instructions",
            EditKind::Rewrite,
        );
    }
}

/// The `parallel_tool_calls` value this egress sends, and the edit that
/// value makes to what the request carried (`None` when it sends the
/// request's own value).
struct ParallelToolCalls {
    value: bool,
    edit: Option<EditKind>,
}

/// Resolve `parallel_tool_calls` for the body and its declaration alike. A
/// Responses-ingress boolean is the client's own field in this dialect and
/// is forwarded as sent. The key stays off the extras allowlist: from any
/// other ingress it spelled another dialect's toggle, so the field is
/// pinned `true` there and the difference declared.
fn resolve_parallel_tool_calls(req: &ChatRequest) -> ParallelToolCalls {
    let carried = req
        .provider_extras
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("parallel_tool_calls"));
    let is_responses = req.routectl_internal.provenance == RequestProvenance::ResponsesIngress;
    let (value, edit) = match carried {
        None => (true, Some(EditKind::Add)),
        Some(Value::Bool(sent)) if is_responses => (*sent, None),
        Some(Value::Bool(true)) => (true, None),
        Some(_) => (true, Some(EditKind::Rewrite)),
    };
    ParallelToolCalls { value, edit }
}

/// Declare the fields this egress writes or withholds by lane, whatever the
/// request carried: `store` is always sent with routectl's value (the
/// ingress never forwards the client's), `parallel_tool_calls` differs from
/// the request wherever `resolve_parallel_tool_calls` says it does, the
/// ChatGPT-OAuth lane never sends `max_output_tokens`, and no lane sends the
/// client's `temperature` or `top_p`.
fn declare_lane_gates(tap: &WireTap, auth_kind: AuthKind, req: &ChatRequest) {
    let site = EditSite::RequiredFieldDefault;
    crate::wire_declare::declare(tap, site, "/store", EditKind::Add);
    crate::wire_declare::declare(tap, site, "/store", EditKind::Rewrite);
    if let Some(kind) = resolve_parallel_tool_calls(req).edit {
        crate::wire_declare::declare(tap, site, "/parallel_tool_calls", kind);
    }
    for (path, carried) in sampling_params(req) {
        if carried {
            crate::wire_declare::declare(tap, EditSite::DialectFieldStrip, path, EditKind::Remove);
        }
    }
    if auth_kind.is_codex_lane() && req.max_tokens.is_some() {
        crate::wire_declare::declare(
            tap,
            EditSite::AuthSurfaceGate,
            "/max_output_tokens",
            EditKind::Remove,
        );
    }
    if auth_kind == AuthKind::BedrockMantle && carries_user_file_url(req) {
        crate::wire_declare::declare(
            tap,
            EditSite::AuthSurfaceGate,
            "/input/*/content/*/file_url",
            EditKind::Remove,
        );
    }
}

/// True when the Mantle lane leaves a client `file_url` off the wire: on a
/// user turn's modeled file part, or on any turn's unmodeled `input_file`
/// part rebuilt from the Responses dialect.
fn carries_user_file_url(req: &ChatRequest) -> bool {
    let rebuilds_unknown_files =
        UnknownParts::for_request(req.routectl_internal.provenance, AuthKind::BedrockMantle)
            == UnknownParts::RebuildFilesOnly;
    let withholds = |is_user: bool, part: &ContentPart| match part {
        ContentPart::Known(KnownContentPart::File { file, .. }) => {
            is_user && withholds_user_file_url(AuthKind::BedrockMantle, file)
        }
        ContentPart::Other {
            type_tag, extras, ..
        } => {
            rebuilds_unknown_files
                && withholds_forwarded_file_url(AuthKind::BedrockMantle, type_tag, extras)
        }
        ContentPart::Known(_) => false,
    };
    req.messages.iter().any(|m| {
        let is_user = matches!(m.role, Role::User);
        matches!(&m.content, MessageContent::Parts(parts) if parts.iter().any(|p| withholds(is_user, p)))
    })
}

/// Splice preserved passthrough items back into the modeled egress
/// `input[]` so each sits at its original inbound position instead of
/// being appended after every modeled item. Each passthrough carries
/// `modeled_prefix` = the count of modeled INPUT items that preceded it
/// inbound; it is emitted after that many modeled EGRESS items.
/// Passthroughs keep their mutual inbound order.
///
/// Residual limitation: the prefix counts INBOUND modeled items, while
/// the splice indexes EGRESS modeled items. These match 1:1 for plain
/// messages (the regression-tested case), but a single modeled inbound
/// item can expand to a different egress-item count -- an assistant turn
/// with tool_calls emits a message item PLUS one `function_call` item per
/// call, and a `function_call` input item attaches to a prior turn rather
/// than emitting its own message. When the counts diverge a passthrough
/// lands at the best stable position (prefix-count splice, clamped to the
/// tail) rather than its exact original slot -- still order-stable, never
/// dropped.
fn merge_passthrough(
    modeled: Vec<ResponseInput>,
    passthrough: &[ResponsesPassthroughItem],
) -> Vec<ResponseInput> {
    if passthrough.is_empty() {
        return modeled;
    }
    let mut out: Vec<ResponseInput> = Vec::with_capacity(modeled.len() + passthrough.len());
    let mut next = passthrough.iter().peekable();
    for (i, item) in modeled.into_iter().enumerate() {
        while let Some(p) = next.peek() {
            if p.modeled_prefix > i {
                break;
            }
            out.push(ResponseInput::Passthrough(
                next.next().expect("peeked").item.clone(),
            ));
        }
        out.push(item);
    }
    // Trailing passthroughs whose prefix exceeds the modeled egress count
    // (or that follow the last modeled item) land at the tail, in order.
    for p in next {
        out.push(ResponseInput::Passthrough(p.item.clone()));
    }
    out
}

/// Cache-prefix surfaces (other than `system`) that carry a caller
/// `cache_control` marker the Responses egress will drop. The Responses
/// API has no prompt-caching breakpoint surface, so dropping the markers
/// is correct -- this only names which surfaces carried one.
///
/// `system` is excluded on purpose: `system.rs` already logs that drop at
/// DEBUG, so re-reporting it here would double-log the same surface.
/// Pure function of `req` -- no logging, no mutation -- so the detection
/// can be unit-tested directly.
fn dropped_cache_surfaces(req: &ChatRequest) -> Vec<&'static str> {
    let mut surfaces: Vec<&'static str> = Vec::new();
    for bp in req.cache_breakpoints() {
        let name = match bp.position {
            BreakpointPosition::Tools => "tools",
            BreakpointPosition::Messages => "messages",
            BreakpointPosition::TopLevel => "top-level",
            // Already logged at DEBUG in system.rs; skip to avoid a double-log.
            // This `continue` skips only the REPORTING of a system-surface
            // marker, never the marker's translation: `system.rs` owns both
            // the drop and its record for that surface. Nothing is lost here
            // that is not accounted for there.
            // TRANSLATION-DROP: structural -- de-duplicates the system surface's diagnostic; the system surface owns that drop and its record
            BreakpointPosition::System => continue,
        };
        if !surfaces.contains(&name) {
            surfaces.push(name);
        }
    }
    surfaces
}

/// Count the client thinking choice this wire loses when an Anthropic client
/// sent a `thinking.type` canonical reasoning cannot express: `reasoning` is
/// built from the canonical config alone, which holds the default for it.
/// TRANSLATION-DROP: lane=openai-responses class=thinking_type_unrepresentable test=an_unmodeled_thinking_type_counts_one_openai_responses_drop
fn count_unmodeled_thinking_type(req: &ChatRequest) {
    if crate::translation_drop_metrics::carries_unmodeled_thinking_type(req) {
        record_translation_drop(super::LANE, "thinking_type_unrepresentable");
    }
}

/// The client sampling parameters this egress withholds, by wire path, with
/// whether the request carries each.
const fn sampling_params(req: &ChatRequest) -> [(&'static str, bool); 2] {
    [
        ("/temperature", req.temperature.is_some()),
        ("/top_p", req.top_p.is_some()),
    ]
}

/// Count, once per request, the client `temperature` / `top_p` this egress
/// leaves off the wire. `ResponsesRequest` carries neither field on any auth
/// surface: the ChatGPT-OAuth backend refuses fields codex never sends, and
/// nothing records that the API-key or Mantle surfaces accept them.
/// TRANSLATION-DROP: lane=openai-responses class=sampling_param_unsupported test=sampling_params_withheld_count_once_and_declare_each_path
fn count_withheld_sampling_params(req: &ChatRequest) {
    if sampling_params(req).iter().any(|(_, carried)| *carried) {
        record_translation_drop(super::LANE, "sampling_param_unsupported");
    }
}

/// Emit one DEBUG naming every cache-prefix surface carrying a caller
/// `cache_control` marker that the Responses egress drops, and count the
/// drop once for the request. Matches the openai-compat egress convention
/// (`check_dropped_anthropic_fields`), which logs every dropped
/// cache_control carrier at DEBUG: the lane has no breakpoint surface, so
/// the drop is inherent to routing there and the counter carries the rate. Logs only the surface
/// name(s) + a count: no message content, no bodies, no secrets.
///
/// The DEBUG and the COUNTER have deliberately different surface sets. The
/// DEBUG excludes `system` because `system.rs` already logs that surface at
/// DEBUG and a second record would double-report it. The counter includes
/// it, because a request whose ONLY marker sat on a system block did have a
/// marker dropped, and a counter that missed it would understate the lane's
/// drop rate. Both fire at most once per request either way.
///
/// Cross-dialect only. The Responses wire has no prompt-cache breakpoint
/// field, so its ingress mints no `cache_control` anywhere -- every content
/// part and system block it builds sets the marker to `None`. A marker
/// reaching this egress therefore came from an Anthropic-shape or
/// OpenAI-shape client. Seed per foundations sec 14, deletion-blocked
/// pending per-lane wire evidence.
/// TRANSLATION-DROP: lane=openai-responses class=cache_control_unsupported test=cache_control_marker_drops_from_the_wire_and_counts
fn log_dropped_cache_control(req: &ChatRequest) {
    if !req.cache_breakpoints().is_empty() {
        record_translation_drop(super::LANE, "cache_control_unsupported");
    }
    let surfaces = dropped_cache_surfaces(req);
    if surfaces.is_empty() {
        return;
    }
    tracing::debug!(
        dropped_surfaces = ?surfaces,
        dropped_count = surfaces.len(),
        "openai-responses egress: cache_control dropped (Responses API has no \
         prompt-cache breakpoint surface)"
    );
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;

// response_format round-trip: the Responses ingress parses inbound
// text.format into req.response_format; the egress must re-emit it onto
// text.format (same-protocol regression).
#[cfg(test)]
mod response_format_tests {
    use super::translate;
    use crate::openai_responses::{AuthKind, OpenAiResponsesConfig};
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

    fn cfg() -> OpenAiResponsesConfig {
        let mut c = OpenAiResponsesConfig::new("openai-responses:test", "literal:test");
        c.auth_kind = AuthKind::ChatgptOauth;
        c
    }

    fn req_with(response_format: Option<serde_json::Value>) -> ChatRequest {
        ChatRequest {
            model: "gpt-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            response_format,
            ..Default::default()
        }
    }

    #[test]
    fn json_schema_response_format_round_trips_to_text_format() {
        let req = req_with(Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "widget",
                "schema": {"type": "object", "required": ["x"]},
                "strict": true
            }
        })));
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        let fmt = &body["text"]["format"];
        assert_eq!(fmt["type"], "json_schema", "got: {body}");
        assert_eq!(fmt["name"], "widget", "got: {body}");
        assert_eq!(fmt["schema"]["required"][0], "x", "got: {body}");
        assert_eq!(fmt["strict"], true, "got: {body}");
    }

    #[test]
    fn json_object_response_format_round_trips_to_text_format() {
        let req = req_with(Some(json!({"type": "json_object"})));
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["text"]["format"]["type"], "json_object", "got: {body}");
    }

    #[test]
    fn response_format_merges_with_verbosity_from_provider_extras() {
        // The ingress lifts the non-format remainder of `text` (e.g.
        // verbosity) into provider_extras["text"]; the egress must keep it
        // and merge the format alongside.
        let mut req = req_with(Some(json!({"type": "json_object"})));
        req.provider_extras = Some(json!({"text": {"verbosity": "low"}}));
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["text"]["verbosity"], "low", "got: {body}");
        assert_eq!(body["text"]["format"]["type"], "json_object", "got: {body}");
    }

    #[test]
    fn caller_text_format_wins_over_response_format() {
        // An operator-supplied text.format is left untouched.
        let mut req = req_with(Some(json!({"type": "json_object"})));
        req.provider_extras = Some(json!({
            "text": {"format": {"type": "json_schema", "name": "op", "schema": {}}}
        }));
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(
            body["text"]["format"]["type"], "json_schema",
            "caller text.format must win: {body}"
        );
    }

    #[test]
    fn no_response_format_leaves_text_absent() {
        let req = req_with(None);
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert!(body.get("text").is_none(), "got: {body}");
    }
}
