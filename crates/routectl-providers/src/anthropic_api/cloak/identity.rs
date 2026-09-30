//! Relocates a non-CC client system prompt and mints the metadata user_id.

use routectl_core::cache_control::{
    self, BreakpointPosition, CacheBreakpointSource, CacheControl, OwnedBreakpoint,
};
use serde_json::{Value, json};

use super::ClaudeCodeIdentity;

/// Canonical Claude Code first-block identity strings. When the inbound
/// body's first system block already matches one of these verbatim, the
/// client is presenting a real Claude Code identity block and we leave it
/// untouched. The first entry is the interactive shape we inject for a
/// non-CC client.
const RECOGNIZED_IDENTITY_LINES: &[&str] = &[
    "You are Claude Code, Anthropic's official CLI for Claude.",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
];

/// The identity line injected for a non-CC client: the interactive
/// (first) recognized line.
pub(super) const INTERACTIVE_IDENTITY_LINE: &str = RECOGNIZED_IDENTITY_LINES[0];

/// Opening tag wrapping the relocated client system content in the first
/// user message. The client's real system prompt is moved here verbatim so
/// the subscription classifier sees only the Claude Code identity in
/// `system` while the client's behavior is preserved.
pub(super) const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";

/// Closing tag for the relocated client system content.
pub(super) const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

/// What one `relocate_client_system` pass LOST, reported upward as a value so
/// the orchestrator owns every counter call and this module stays free of
/// telemetry state. Each field is one operator-facing class, at most once per
/// request however many blocks or turns contributed to it.
///
/// `must_use`: a caller that computes this and drops it has silently stopped
/// counting a loss the request still performs, which is the exact failure the
/// instrumentation exists to refuse. Ignoring it must be spelled out.
#[must_use]
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct RelocationOutcome {
    /// More than one client cache breakpoint on blocks that fold into the
    /// reminder was reduced to the one the reminder carries.
    pub(super) cache_breakpoints_collapsed: bool,
    /// A captured system block whose role or adjacency requirements the user
    /// turn cannot honor (tool use, tool result, thinking, redacted thinking,
    /// or an unknown type) was left out of the relocation.
    pub(super) unrepresentable_block_dropped: bool,
}

/// Why a relocation was refused. Decided before the body is touched, so a
/// refused body is exactly the body the caller passed in. `detail` names the
/// shape only and never carries request content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelocationRefusal {
    pub detail: String,
}

/// Reduce a non-CC client's `system` to the interactive identity line only,
/// relocating the client's real system content into the conversation.
///
/// The subscription classifier runs a substance check on `system`; a
/// third-party agent's system prompt fails it wholesale. So the client's real
/// system content (already billing-stripped) is captured, the `system` field
/// is replaced with the identity line only, and -- unless `strict_mode` is set
/// -- the captured content is reattached at the front of the first user
/// message: text as one `<system-reminder>` block, then every image and
/// document block verbatim in capture order. With no user message but a
/// nonempty remaining history, one synthetic user message carrying the same
/// payload is prepended instead.
///
/// `role: "system"` entries in `messages[]` are a second carrier of the same
/// client directives (the anthropic-api egress forwards a mid-conversation
/// system turn in place), so they are captured and REMOVED from the array by
/// the same pass and folded into the same payload.
///
/// Transactional: the whole landing is planned against the unmodified body
/// and committed only once it is known to be legal. A refusal -- no message
/// array, a non-array `messages`, nothing left of the conversation once the
/// system turns leave it, or carried cache breakpoints the cap/ordering policy
/// cannot accept -- returns before any mutation.
///
/// Recognized identity lines in the captured content are excluded (we re-add
/// our own identity). The transform is egress-only: the response never echoes
/// `system`, so there is no reverse map.
pub(super) fn relocate_client_system(
    body: &mut Value,
    strict_mode: bool,
) -> Result<RelocationOutcome, RelocationRefusal> {
    if body.as_object().is_none() {
        return Ok(RelocationOutcome::default());
    }
    let plan = plan_relocation(body, strict_mode)?;
    commit_relocation(body, plan.landing);
    log_relocation_losses(plan.outcome);
    Ok(plan.outcome)
}

/// What the commit lands, decided against the unmodified body.
enum Landing {
    /// Nothing to relocate: only the system reduction and the system-turn
    /// removal run.
    Nowhere,
    /// Reminder first, then carried blocks, in capture order. Prepended to the
    /// first `role: "user"` message, or carried by one synthetic user message
    /// ahead of the history when there is none.
    Payload(Vec<Value>),
}

struct RelocationPlan {
    landing: Landing,
    outcome: RelocationOutcome,
}

/// Decide the full relocation without touching the body.
///
/// Under `strict_mode` the operator asked for the client system to be DROPPED
/// rather than relocated, so nothing lost from here on is a loss routectl took
/// on the operator's behalf: no class fires, nothing is logged, and no shape
/// is refused.
fn plan_relocation(body: &Value, strict_mode: bool) -> Result<RelocationPlan, RelocationRefusal> {
    let capture = capture_all(body);
    if strict_mode {
        return Ok(RelocationPlan {
            landing: Landing::Nowhere,
            outcome: RelocationOutcome::default(),
        });
    }
    let reminder = build_reminder_block(&capture.texts);
    let outcome = RelocationOutcome {
        // Only a REDUCTION counts, and only when a reminder carries the
        // surviving breakpoint.
        cache_breakpoints_collapsed: reminder.is_some() && capture.folded_breakpoints > 1,
        unrepresentable_block_dropped: capture.unrepresentable_block_dropped,
    };
    // Only a CARRIED block can add a breakpoint the request did not already
    // count: the reminder carries at most one of the folded markers. Gating the
    // check on carried markers keeps every text-only relocation's output
    // exactly as it has always been.
    let carries_breakpoint = capture
        .carried
        .iter()
        .any(|block| block.get("cache_control").is_some());
    let payload: Vec<Value> = reminder.into_iter().chain(capture.carried).collect();
    if payload.is_empty() {
        return Ok(RelocationPlan {
            landing: Landing::Nowhere,
            outcome,
        });
    }
    let has_first_user = conversation_has_user_turn(body.get("messages"))?;
    if carries_breakpoint {
        validate_candidate_breakpoints(body, &payload, has_first_user)?;
    }
    Ok(RelocationPlan {
        landing: Landing::Payload(payload),
        outcome,
    })
}

/// Whether the conversation left once system turns are removed has a user
/// turn to land on, refusing the shapes with nowhere legal to land.
fn conversation_has_user_turn(messages: Option<&Value>) -> Result<bool, RelocationRefusal> {
    let Some(messages) = messages else {
        return Err(refusal("the request carries no message array"));
    };
    let Some(messages) = messages.as_array() else {
        return Err(refusal("the request's messages field is not an array"));
    };
    let mut remaining = messages.iter().filter(|m| !is_role(m, "system")).peekable();
    if remaining.peek().is_none() {
        return Err(refusal(
            "the request carries no conversation once its system content is relocated",
        ));
    }
    Ok(remaining.any(|m| is_role(m, "user")))
}

fn refusal(detail: &str) -> RelocationRefusal {
    RelocationRefusal {
        detail: format!("client system content cannot be relocated: {detail}"),
    }
}

fn is_role(message: &Value, role: &str) -> bool {
    message.get("role").and_then(Value::as_str) == Some(role)
}

/// Run the existing breakpoint cap/ordering policy over the body the plan
/// would produce. The payload's markers sit where the commit will put them:
/// ahead of the first user message's own content, or in the synthetic message
/// ahead of all history.
fn validate_candidate_breakpoints(
    body: &Value,
    payload: &[Value],
    has_first_user: bool,
) -> Result<(), RelocationRefusal> {
    let candidate = CandidateBreakpoints {
        body,
        payload,
        has_first_user,
    };
    cache_control::validate_source(&candidate).map_err(|e| RelocationRefusal {
        detail: format!("relocated system blocks break the cache breakpoint policy: {e}"),
    })
}

struct CandidateBreakpoints<'a> {
    body: &'a Value,
    payload: &'a [Value],
    has_first_user: bool,
}

impl CacheBreakpointSource for CandidateBreakpoints<'_> {
    fn cache_breakpoints(&self) -> Vec<OwnedBreakpoint> {
        let mut out = Vec::new();
        let tools = self.body.get("tools").and_then(Value::as_array);
        push_markers(
            &mut out,
            BreakpointPosition::Tools,
            tools.into_iter().flatten(),
        );
        if !self.has_first_user {
            push_markers(&mut out, BreakpointPosition::Messages, self.payload.iter());
        }
        let mut payload_pending = self.has_first_user;
        let messages = self.body.get("messages").and_then(Value::as_array);
        for message in messages.into_iter().flatten() {
            if is_role(message, "system") {
                continue;
            }
            if payload_pending && is_role(message, "user") {
                payload_pending = false;
                push_markers(&mut out, BreakpointPosition::Messages, self.payload.iter());
            }
            let blocks = message.get("content").and_then(Value::as_array);
            push_markers(
                &mut out,
                BreakpointPosition::Messages,
                blocks.into_iter().flatten(),
            );
        }
        push_markers(
            &mut out,
            BreakpointPosition::TopLevel,
            std::iter::once(self.body),
        );
        out
    }
}

/// Append the parseable `cache_control` marker of each item. An unparseable
/// marker counts as no breakpoint, matching the assembled-body walk's
/// treatment of an opaque tool's marker.
fn push_markers<'a>(
    out: &mut Vec<OwnedBreakpoint>,
    position: BreakpointPosition,
    items: impl Iterator<Item = &'a Value>,
) {
    for item in items {
        if let Some(control) = item
            .get("cache_control")
            .and_then(|cc| serde_json::from_value::<CacheControl>(cc.clone()).ok())
        {
            out.push(OwnedBreakpoint::new(position, control));
        }
    }
}

/// Apply a planned relocation. Infallible by construction: every shape the
/// plan could not land was refused before this runs.
fn commit_relocation(body: &mut Value, landing: Landing) {
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        messages.retain(|m| !is_role(m, "system"));
    }
    set_identity_only_system(body);
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let Landing::Payload(payload) = landing else {
        return;
    };
    match messages.iter_mut().find(|m| is_role(m, "user")) {
        Some(user) => prepend_to_user_content(user, payload),
        None => messages.insert(0, synthetic_user(payload)),
    }
}

fn synthetic_user(payload: Vec<Value>) -> Value {
    json!({"role": "user", "content": payload})
}

/// Prepend `payload` to a user message's content, promoting string content to
/// a text block and creating the array when content is absent or null.
fn prepend_to_user_content(user: &mut Value, payload: Vec<Value>) {
    let Some(obj) = user.as_object_mut() else {
        return;
    };
    let content = match obj.remove("content") {
        Some(Value::Array(blocks)) => payload.into_iter().chain(blocks).collect(),
        Some(Value::String(text)) => payload
            .into_iter()
            .chain(std::iter::once(json!({"type": "text", "text": text})))
            .collect(),
        _ => payload,
    };
    obj.insert("content".into(), Value::Array(content));
}

/// Log the losses this pass took, ONCE PER REQUEST each, at the levels their
/// severity earns.
///
/// Aggregated here rather than logged where each loss is detected: the capture
/// runs a per-block loop over client-supplied content, so a log there emits one
/// line per block and a request carrying a large block array turns one policy
/// action into unbounded log volume.
fn log_relocation_losses(outcome: RelocationOutcome) {
    if outcome.unrepresentable_block_dropped {
        tracing::warn!(
            "cloak system relocation: dropping client system blocks a user turn cannot carry"
        );
    }
    if outcome.cache_breakpoints_collapsed {
        // DEBUG rather than WARN: the loss is cache economics, it can fire on a
        // large share of requests, and a WARN that common trains an operator to
        // ignore the level.
        tracing::debug!(
            "cloak system relocation: collapsing client system cache breakpoints to the last"
        );
    }
}

/// Capture the `system` field and every `role: "system"` turn, in that order.
fn capture_all(body: &Value) -> SystemCapture {
    let mut capture = capture_client_system(body.get("system"));
    let turns = body.get("messages").and_then(Value::as_array);
    for turn in turns.into_iter().flatten().filter(|m| is_role(m, "system")) {
        capture.absorb(capture_client_system(turn.get("content")));
    }
    capture
}

/// A captured client system text block: its text plus any `cache_control`
/// it carried (so a cache breakpoint can be preserved on relocation).
struct CapturedSystemBlock {
    text: String,
    cache_control: Option<Value>,
}

/// One capture pass's result. Paired rather than returned separately so a
/// caller folding several passes together cannot keep one and lose the others.
#[derive(Default)]
struct SystemCapture {
    /// Text folded into the reminder, in capture order.
    texts: Vec<CapturedSystemBlock>,
    /// Image and document blocks carried verbatim, in capture order.
    carried: Vec<Value>,
    unrepresentable_block_dropped: bool,
    /// Breakpoints on every block that does NOT ride as itself (text, identity
    /// line, unrepresentable): a carried block keeps its own marker, so only
    /// these can collapse into the one the reminder carries.
    folded_breakpoints: usize,
}

impl SystemCapture {
    /// Fold another pass's result into this one.
    fn absorb(&mut self, other: Self) {
        self.texts.extend(other.texts);
        self.carried.extend(other.carried);
        self.unrepresentable_block_dropped |= other.unrepresentable_block_dropped;
        self.folded_breakpoints += other.folded_breakpoints;
    }
}

/// Capture client system content, excluding any block whose trimmed text is a
/// recognized identity line (we re-add our own identity). Handles the string
/// form, the array-of-block form, and absence. Shared by the `system` field and
/// the `role: "system"` message turns, whose content shapes are the same.
fn capture_client_system(system: Option<&Value>) -> SystemCapture {
    match system {
        Some(Value::String(s)) => {
            if RECOGNIZED_IDENTITY_LINES.contains(&s.trim()) {
                return SystemCapture::default();
            }
            SystemCapture {
                texts: vec![CapturedSystemBlock {
                    text: s.clone(),
                    cache_control: None,
                }],
                ..SystemCapture::default()
            }
        }
        Some(Value::Array(blocks)) => {
            let mut capture = SystemCapture::default();
            for block in blocks {
                let classified = capture_one_system_block(block);
                if block.get("cache_control").is_some()
                    && !matches!(classified, CapturedBlock::Carried(_))
                {
                    capture.folded_breakpoints += 1;
                }
                match classified {
                    CapturedBlock::Text(captured) => capture.texts.push(captured),
                    CapturedBlock::Carried(raw) => capture.carried.push(raw),
                    CapturedBlock::Unrepresentable => capture.unrepresentable_block_dropped = true,
                    CapturedBlock::IdentityLine => {}
                }
            }
            capture
        }
        // Neither carrier can present a third shape: the canonical `system`
        // reaches here as a string or a block array, and a forwarded system
        // turn's content is translated to one of the same two. An unmodeled
        // shape captures nothing and is not counted as a loss -- there is no
        // evidence it carried client directives to lose.
        _ => SystemCapture::default(),
    }
}

/// What one system array element contributed to the capture.
enum CapturedBlock {
    /// Text folded into the reminder.
    Text(CapturedSystemBlock),
    /// An image or document block, legal user content with no adjacency
    /// requirement, carried as itself.
    Carried(Value),
    /// A block whose role or adjacency requirements a user turn cannot honor.
    Unrepresentable,
    /// A recognized identity line, excluded by design because we re-add our
    /// own. Not a loss: the same line goes back on the wire.
    IdentityLine,
}

/// Classify a single system array element.
///
/// A string `text` decides first, whatever the block's type, so every block the
/// reminder has always folded keeps folding byte-for-byte. Image and document
/// blocks are carried raw -- never stringified, never scanned for reminder
/// delimiters. Anything else (tool use, tool result, thinking, redacted
/// thinking, an unknown type) is left out and reported: POLICY ACTION rather
/// than a drop, because the block arrived in a field that accepted it and the
/// user turn it would have to move into cannot carry it legally.
fn capture_one_system_block(block: &Value) -> CapturedBlock {
    if let Some(text) = block.get("text").and_then(Value::as_str) {
        if RECOGNIZED_IDENTITY_LINES.contains(&text.trim()) {
            return CapturedBlock::IdentityLine;
        }
        return CapturedBlock::Text(CapturedSystemBlock {
            text: text.to_string(),
            cache_control: block.get("cache_control").cloned(),
        });
    }
    match block.get("type").and_then(Value::as_str) {
        Some("image" | "document") => CapturedBlock::Carried(block.clone()),
        _ => CapturedBlock::Unrepresentable,
    }
}

/// Replace `body["system"]` with the identity-only array (no
/// `cache_control`; matches `identity_block()`).
fn set_identity_only_system(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("system".into(), Value::Array(vec![identity_block()]));
    }
}

/// Build the `<system-reminder>` text block from the captured client system
/// text, or `None` when there is no text to relocate. Multiple captured blocks'
/// text is joined with a blank line. KNOWN LIMITATION: multiple client system
/// cache breakpoints on folded text collapse to one -- the last captured
/// `cache_control` (closest to the cache boundary) is carried, the rest are
/// dropped.
fn build_reminder_block(captured: &[CapturedSystemBlock]) -> Option<Value> {
    if captured.is_empty() {
        return None;
    }
    // Single-pass build with a blank-line separator between blocks; a literal
    // closing tag inside client content is neutralized so it cannot
    // prematurely close our wrapper framing.
    let mut joined = String::new();
    for (i, b) in captured.iter().enumerate() {
        if i > 0 {
            joined.push_str("\n\n");
        }
        joined.push_str(&neutralize_close_tag(&b.text));
    }
    let text = format!("{SYSTEM_REMINDER_OPEN}\n{joined}\n{SYSTEM_REMINDER_CLOSE}");
    let mut block = json!({"type": "text", "text": text});
    if let Some(cache_control) = captured.iter().rev().find_map(|b| b.cache_control.clone())
        && let Some(obj) = block.as_object_mut()
    {
        obj.insert("cache_control".into(), cache_control);
    }
    Some(block)
}

/// Strip any literal `</system-reminder>` from captured client content so the
/// relocated text cannot prematurely close the wrapper framing. The tag is
/// removed entirely (the least-surprising minimal transform); unrelated
/// content is untouched.
fn neutralize_close_tag(text: &str) -> String {
    if text.contains(SYSTEM_REMINDER_CLOSE) {
        text.replace(SYSTEM_REMINDER_CLOSE, "")
    } else {
        text.to_string()
    }
}

fn identity_block() -> Value {
    json!({"type": "text", "text": INTERACTIVE_IDENTITY_LINE})
}

/// Mint `body["metadata"]["user_id"]` to a corpus-shaped JSON-encoded
/// string when it is absent or empty. The encoded object keeps key order
/// device_id, account_uuid, session_id (corpus shape). A present non-empty
/// `user_id` is left untouched.
pub(super) fn mint_metadata_user_id(body: &mut Value, identity: &ClaudeCodeIdentity) {
    let already_set = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if already_set {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let metadata = obj
        .entry("metadata")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(metadata_obj) = metadata.as_object_mut() else {
        return;
    };
    metadata_obj.insert("user_id".into(), Value::String(encode_user_id(identity)));
}

/// Build the JSON-encoded `user_id` string with keys in the exact corpus
/// order: device_id, account_uuid, session_id. A hand-built string (not
/// `serde_json::to_string` of a map) so key order is guaranteed.
fn encode_user_id(identity: &ClaudeCodeIdentity) -> String {
    // All three interpolated fields are UUID-shaped (device_id is two
    // concatenated simple uuids; account_uuid is a dashed uuid; session_id
    // is a uuid or a corpus-shaped session id), so they contain no quote /
    // backslash / control bytes that would need JSON escaping. The hand-built
    // string (rather than `serde_json::to_string` of a map) is deliberate: it
    // guarantees the corpus key order device_id, account_uuid, session_id.
    format!(
        r#"{{"device_id":"{}","account_uuid":"{}","session_id":"{}"}}"#,
        identity.device_id, identity.account_uuid, identity.session_id
    )
}
