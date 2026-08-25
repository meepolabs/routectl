//! Anthropic prompt-caching `cache_control` shared type and validator.
//!
//! Spec: <https://platform.claude.com/docs/en/build-with-claude/prompt-caching>
//!
//! - Up to 4 explicit `cache_control` breakpoints per request (5th is a 400).
//! - 1h TTL breakpoints must appear before 5m breakpoints in the cache prefix
//!   order: `tools` -> `system` -> `messages`.
//! - The hub stores cache_control as a typed value so both the Anthropic
//!   ingress and the Anthropic / Bedrock-Invoke egresses can treat it
//!   symmetrically. OpenAI-compat egress drops it with a warn.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Cache breakpoint marker. Currently only `type = "ephemeral"` is defined.
/// `ttl` defaults to "5m" when absent on the wire; we serialize it
/// explicitly when set so a parsed-and-re-emitted request is byte-stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheControl {
    /// Breakpoint kind, serialized as the wire `type` field. Only
    /// `ephemeral` is currently defined.
    #[serde(rename = "type")]
    pub kind: String,
    /// Cache lifetime (`"5m"` or `"1h"`). Absent means the `"5m"` wire
    /// default; see [`CacheControl::effective_ttl`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

impl CacheControl {
    /// Default 5-minute ephemeral breakpoint.
    pub fn ephemeral_5m() -> Self {
        Self {
            kind: "ephemeral".into(),
            ttl: Some("5m".into()),
        }
    }

    /// 1-hour ephemeral breakpoint (premium-priced).
    pub fn ephemeral_1h() -> Self {
        Self {
            kind: "ephemeral".into(),
            ttl: Some("1h".into()),
        }
    }

    /// Resolve the effective TTL with the wire default (`"5m"` when absent).
    pub fn effective_ttl(&self) -> &str {
        self.ttl.as_deref().unwrap_or("5m")
    }
}

/// Maximum explicit cache breakpoints per request. The top-level auto-cache
/// `cache_control` field counts as one of the four when present.
pub const MAX_BREAKPOINTS: usize = 4;

/// One observed cache_control marker, with where it sat in the cache prefix.
/// Used by `validate` to enforce TTL ordering across positions.
pub struct Breakpoint<'a> {
    /// Where this marker sits in the cache prefix.
    pub position: BreakpointPosition,
    /// The observed marker.
    pub control: &'a CacheControl,
}

/// Cache prefix order: tools -> system -> messages. Breakpoints in earlier
/// positions are seen first by the cache; longer TTLs (1h) must come before
/// shorter (5m).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BreakpointPosition {
    /// Tool definitions, first in the cache prefix.
    Tools,
    /// System blocks, after tools.
    System,
    /// Message content parts, after system.
    Messages,
    /// Top-level auto-cache marker. Counts toward the 4-breakpoint cap.
    TopLevel,
}

/// Validate the STRUCTURAL invariants routectl owns, because it injects its
/// own markers and must do the same arithmetic upstream does:
/// (1) at most `MAX_BREAKPOINTS` breakpoints, (2) longer TTLs (1h) must
/// appear before shorter ones (5m) in cache-prefix order. Both emit
/// `Error::Validation` with a position-aware message.
///
/// Marker VOCABULARY is upstream's to define: an unrecognized `type` or
/// `ttl` forwards verbatim rather than being rejected here. An unknown ttl
/// is not "5m", so it never opens the 1h-after-5m window in the ordering
/// walk.
pub fn validate(breakpoints: &[Breakpoint<'_>]) -> Result<()> {
    if breakpoints.len() > MAX_BREAKPOINTS {
        return Err(Error::Validation(format!(
            "cache_control: {} breakpoints exceeds maximum of {}",
            breakpoints.len(),
            MAX_BREAKPOINTS
        )));
    }

    let mut last_ttl_was_5m = false;
    for bp in breakpoints {
        let ttl = bp.control.effective_ttl();
        if last_ttl_was_5m && ttl == "1h" {
            return Err(Error::Validation(format!(
                "cache_control: 1h TTL breakpoint at {:?} appears after a 5m \
                 breakpoint; longer TTLs must come before shorter ones in \
                 cache prefix order",
                bp.position
            )));
        }
        last_ttl_was_5m = ttl == "5m";
    }

    Ok(())
}

/// One observed cache_control marker, with where it sat in the cache
/// prefix, carrying an OWNED control. Owned (rather than borrowed) so
/// sources whose marker is parsed on demand from an opaque payload --
/// e.g. `ToolDef::Other`, whose `cache_control()` returns an owned
/// value -- can yield it without lifetime gymnastics.
///
/// `#[non_exhaustive]`: external `CacheBreakpointSource` implementors
/// construct this, so a future field must not break them. Build via
/// `OwnedBreakpoint::new`.
#[non_exhaustive]
pub struct OwnedBreakpoint {
    /// Where this marker sits in the cache prefix.
    pub position: BreakpointPosition,
    /// The owned marker.
    pub control: CacheControl,
}

impl OwnedBreakpoint {
    /// Construct an owned breakpoint at `position` carrying `control`.
    pub const fn new(position: BreakpointPosition, control: CacheControl) -> Self {
        Self { position, control }
    }
}

/// A request shape that can enumerate its cache_control breakpoints in
/// cache-prefix order (tools -> system -> messages -> top-level).
///
/// This trait is the single source of truth for the per-position walk.
/// Every validator (`validate_source`) and the auto-cache breakpoint
/// counter (`compute_frozen_floor`) consume the same enumeration, so a
/// new consumer never adds a parallel traversal.
pub trait CacheBreakpointSource {
    /// Breakpoints in cache-prefix order. Each carries an owned control
    /// (see `OwnedBreakpoint`).
    fn cache_breakpoints(&self) -> Vec<OwnedBreakpoint>;
}

/// Canonical (PRE-assembly) breakpoint walk: enumerates the markers on the
/// request as RECEIVED. Backs `compute_frozen_floor` / `mutable_suffix_start`
/// and validates egresses whose wire shape mirrors canonical 1:1 (e.g.
/// Bedrock Converse, via `validate_source(req)`).
///
/// NOTE: this is deliberately NOT interchangeable with the anthropic-api
/// egress's `CacheBreakpointSource for AnthropicRequest` walk (in
/// routectl-providers anthropic_api/request.rs). That one runs on the
/// ASSEMBLED wire body and counts a different set, because assembly is lossy;
/// `validate_breakpoints` there enumerates the lossy points (single source of
/// truth for that list -- do not restate it here). Validating this canonical
/// pre-image where the wire post-image is required would change the
/// 4-breakpoint-cap / TTL-ordering outcome for those cases. Both walks are
/// load-bearing; do not "deduplicate" one into the other.
impl CacheBreakpointSource for crate::ChatRequest {
    fn cache_breakpoints(&self) -> Vec<OwnedBreakpoint> {
        let mut bps: Vec<OwnedBreakpoint> = Vec::new();

        // Tools come first in the cache prefix. `ToolDef::cache_control`
        // covers both the typed `Custom` variant and `Other` builtins
        // (e.g. `web_search_*`) whose marker is parsed on demand; a
        // malformed marker on `Other` is treated as no-breakpoint.
        if let Some(tools) = &self.tools {
            for t in tools {
                if let Some(cc) = t.cache_control() {
                    bps.push(OwnedBreakpoint {
                        position: BreakpointPosition::Tools,
                        control: cc,
                    });
                }
            }
        }

        // Then system blocks (per-block markers on `Blocks`).
        if let Some(crate::SystemContent::Blocks(blocks)) = self.system.as_ref() {
            for b in blocks {
                if let Some(cc) = b.cache_control.as_ref() {
                    bps.push(OwnedBreakpoint {
                        position: BreakpointPosition::System,
                        control: cc.clone(),
                    });
                }
            }
        }

        // Then messages: each typed `ContentPart` may carry a marker.
        for m in &*self.messages {
            if let crate::MessageContent::Parts(parts) = &m.content {
                for p in parts {
                    if let Some(cc) = p.cache_control() {
                        bps.push(OwnedBreakpoint {
                            position: BreakpointPosition::Messages,
                            control: cc.clone(),
                        });
                    }
                }
            }
        }

        // Top-level auto-cache marker.
        if let Some(cc) = self.cache_control.as_ref() {
            bps.push(OwnedBreakpoint {
                position: BreakpointPosition::TopLevel,
                control: cc.clone(),
            });
        }

        bps
    }
}

/// Validate any `CacheBreakpointSource` against Anthropic's invariants.
/// Collects the owned breakpoint sequence, builds the borrowed
/// `Breakpoint` slice referencing it, and delegates to `validate`.
pub fn validate_source<S: CacheBreakpointSource>(src: &S) -> Result<()> {
    let owned = src.cache_breakpoints();
    let bps: Vec<Breakpoint<'_>> = owned
        .iter()
        .map(|ob| Breakpoint {
            position: ob.position,
            control: &ob.control,
        })
        .collect();
    validate(&bps)
}

/// The breakpoint slots the caller already occupies, in cache-prefix
/// order. An auto-emitter consults this to avoid exceeding
/// `MAX_BREAKPOINTS` or duplicating the `TopLevel` slot.
///
/// Built only by `compute_frozen_floor`; fields are private and the
/// struct is `#[non_exhaustive]`, so it cannot be constructed via a
/// struct literal outside this crate.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct FrozenFloor {
    positions: Vec<BreakpointPosition>,
}

impl FrozenFloor {
    /// How many cache_control breakpoints the caller already supplied.
    pub const fn caller_breakpoint_count(&self) -> usize {
        self.positions.len()
    }

    /// The occupied positions, in cache-prefix order.
    pub fn positions(&self) -> &[BreakpointPosition] {
        &self.positions
    }

    /// Whether the caller supplied at least one breakpoint.
    pub const fn has_caller_breakpoints(&self) -> bool {
        !self.positions.is_empty()
    }
}

/// Build the `FrozenFloor` from a request, reusing the same
/// `cache_breakpoints` walk every validator uses. This is how a 4th
/// consumer (the auto-cache breakpoint counter) avoids adding a 4th
/// traversal.
pub fn compute_frozen_floor(req: &crate::ChatRequest) -> FrozenFloor {
    let positions = req
        .cache_breakpoints()
        .into_iter()
        .map(|ob| ob.position)
        .collect();
    FrozenFloor { positions }
}

/// The first MUTABLE message index: the boundary a cache-safe context
/// reduction transform may begin byte-changing without invalidating any
/// caller-supplied prompt-cache breakpoint.
///
/// A message is "frozen" when one of its content parts carries a caller
/// `cache_control` marker -- the same condition the `cache_breakpoints`
/// walk records as a `BreakpointPosition::Messages` breakpoint. The
/// transform may only mutate messages STRICTLY AFTER the last frozen
/// message.
///
/// `BreakpointPosition` does not carry the originating message index, so
/// the last frozen index is recovered by re-scanning `req.messages` with
/// the same per-part `cache_control().is_some()` check the walk uses; no
/// new content classification is introduced. Pure function of `req` alone
/// -- no `FrozenFloor` is threaded in, so a stale floor computed from a
/// different request can never desync the boundary from what it bounds.
///
/// A top-level `req.cache_control` marker is the exception: it selects
/// Anthropic AUTOMATIC caching, which freezes the entire prompt prefix
/// (tools + system + ALL messages up to the last block), so it leaves NO
/// mutable message tail. A tools- or system-level marker, by contrast,
/// sits BEFORE the messages and leaves them mutable.
///
/// Return semantics:
/// - `Some(i)`, `0 <= i < req.messages.len()`: `messages[i..]` are mutable
///   and `messages[..i]` are frozen, where `i` is `(index of the last
///   message carrying a caller marker) + 1`.
/// - `Some(0)`: NO message carries a caller marker AND there is no top-level
///   marker. This covers zero caller breakpoints anywhere AND markers only
///   on tools / system -- minifying messages never changes those bytes, so
///   the whole list is mutable.
/// - `None`: there is no mutable tail -- a top-level caller marker freezes
///   the whole prefix, OR the last message-level marker sits on the FINAL
///   message (nothing follows it), OR `req.messages` is empty.
pub fn mutable_suffix_start(req: &crate::ChatRequest) -> Option<usize> {
    if req.messages.is_empty() {
        return None;
    }

    // A top-level caller `cache_control` selects Anthropic automatic caching,
    // which freezes the ENTIRE prefix (tools + system + all messages up to the
    // last block). Unlike a tools/system marker -- which sits before the
    // messages, leaving them mutable -- it leaves no mutable message tail.
    if req.cache_control.is_some() {
        return None;
    }

    let last_frozen = req
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| message_has_caller_marker(m))
        .map(|(i, _)| i)
        .next_back();

    match last_frozen {
        // No message-level marker (markers only on tools/system): every
        // message is mutable; minifying them does not touch the frozen
        // tools/system bytes.
        None => Some(0),
        // Last marker on the final message: nothing follows it.
        Some(i) if i + 1 >= req.messages.len() => None,
        Some(i) => Some(i + 1),
    }
}

/// Where an auto-emitted FRONT cache breakpoint can be placed on a
/// request. Both variants sit strictly BEFORE the messages in cache-prefix
/// order, so a front marker never freezes any part of the message list.
///
/// Each variant carries the resolved INDEX of the element to mark. The
/// injector must mark that index rather than re-deriving "the last one":
/// the resolution rules here (wire-eligibility for system blocks, typed-only
/// for tools) are not reproducible from a naive `last()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontSlot {
    /// The last WIRE-ELIGIBLE block of a `SystemContent::Blocks` system
    /// prompt, by index into that block list. See
    /// [`system_block_is_wire_eligible`].
    LastSystemBlock {
        /// Index into the `SystemContent::Blocks` vector.
        block_index: usize,
    },
    /// The last typed custom tool definition, by index into `req.tools`.
    LastCustomTool {
        /// Index into the `req.tools` vector.
        tool_index: usize,
    },
}

impl FrontSlot {
    /// The cache-prefix position this slot occupies. Never
    /// [`BreakpointPosition::Messages`] -- see [`front_breakpoint_slot`].
    pub const fn position(self) -> BreakpointPosition {
        match self {
            Self::LastSystemBlock { .. } => BreakpointPosition::System,
            Self::LastCustomTool { .. } => BreakpointPosition::Tools,
        }
    }
}

/// Whether a system block survives to the wire and can therefore anchor a
/// cache breakpoint.
///
/// A blank-text block cannot: the Converse egress skips blank blocks
/// individually (`bedrock/converse/system.rs:66-68`, where an empty text
/// block cannot anchor a `cachePoint` because AWS rejects a marker with no
/// preceding content), and both egresses drop a wholly-blank system outright
/// via `SystemContent::is_blank` (`anthropic_api/request.rs:523`,
/// `bedrock/converse/system.rs:40`). Marking a blank block would emit a
/// marker that never reaches the wire while also suppressing fallback to an
/// available tools anchor.
///
/// The blank test mirrors `SystemContent::is_blank`'s per-block rule
/// (`system_content.rs:71`) so one definition of "blank" governs both.
pub fn system_block_is_wire_eligible(block: &crate::SystemBlock) -> bool {
    !block.text.trim().is_empty()
}

/// Index of the last wire-eligible block in a system prompt, or `None` when
/// the system offers no anchor (a flat `Text`, an empty block list, or a
/// block list whose every block is blank).
///
/// This is the SINGLE definition of the system anchor. The injector resolves
/// its target through this function (or through the index in
/// [`FrontSlot::LastSystemBlock`]) so placement can never diverge from
/// selection.
pub fn eligible_system_block_index(system: &crate::SystemContent) -> Option<usize> {
    match system {
        crate::SystemContent::Text(_) => None,
        crate::SystemContent::Blocks(blocks) => {
            blocks.iter().rposition(system_block_is_wire_eligible)
        }
    }
}

/// Resolve the slot an auto-emitted FRONT cache breakpoint would occupy,
/// or `None` when the request offers no anchor.
///
/// Pure read of `req`: no mutation, no wire re-encode, no shape lift.
///
/// System is preferred over tools whenever a wire-eligible system-block
/// anchor exists. A tools-slot marker can be dropped during anthropic-api
/// assembly -- `tool_choice = "none"` suppresses the whole tools array -- so
/// a marker placed there is not guaranteed to reach the wire, while a marker
/// on a wire-eligible system block is.
///
/// The system anchor is the last WIRE-ELIGIBLE block, not simply the last
/// block: see [`system_block_is_wire_eligible`]. When no eligible block
/// remains, resolution falls back to the tools anchor rather than returning
/// a slot the wire would discard.
///
/// A flat-string system (`SystemContent::Text`) has no per-block marker
/// field and offers no anchor. It is NOT lifted to `Blocks`: that is a
/// wire-shape change on a re-encoding-banned path and would break cache
/// affinity for every caller already sending a flat string. Such a
/// request with no custom tool yields `None` (no front marker emitted).
///
/// Only `ToolDef::Custom` anchors the tools slot. `ToolDef::Other`
/// (builtins, future wire shapes) is preserved verbatim through the
/// egresses, so injecting a marker into its opaque payload is not a read
/// this function can promise.
pub fn front_breakpoint_slot(req: &crate::ChatRequest) -> Option<FrontSlot> {
    if let Some(block_index) = req.system.as_ref().and_then(eligible_system_block_index) {
        return Some(FrontSlot::LastSystemBlock { block_index });
    }

    let tool_index = req
        .tools
        .as_ref()?
        .iter()
        .rposition(|t| matches!(t, crate::ToolDef::Custom(_)))?;

    Some(FrontSlot::LastCustomTool { tool_index })
}

/// Whether any content part of `m` carries a caller `cache_control`
/// marker, using the same check the `cache_breakpoints` walk applies.
fn message_has_caller_marker(m: &crate::Message) -> bool {
    if let crate::MessageContent::Parts(parts) = &m.content {
        parts.iter().any(|p| p.cache_control().is_some())
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc_5m() -> CacheControl {
        CacheControl::ephemeral_5m()
    }

    fn cc_1h() -> CacheControl {
        CacheControl::ephemeral_1h()
    }

    #[test]
    fn empty_is_valid() {
        validate(&[]).unwrap();
    }

    #[test]
    fn four_breakpoints_is_valid() {
        let cc = cc_5m();
        let bps = vec![
            Breakpoint {
                position: BreakpointPosition::Tools,
                control: &cc,
            },
            Breakpoint {
                position: BreakpointPosition::System,
                control: &cc,
            },
            Breakpoint {
                position: BreakpointPosition::Messages,
                control: &cc,
            },
            Breakpoint {
                position: BreakpointPosition::Messages,
                control: &cc,
            },
        ];
        validate(&bps).unwrap();
    }

    #[test]
    fn five_breakpoints_is_rejected() {
        let cc = cc_5m();
        let bps: Vec<Breakpoint<'_>> = (0..5)
            .map(|_| Breakpoint {
                position: BreakpointPosition::Messages,
                control: &cc,
            })
            .collect();
        let err = validate(&bps).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exceeds maximum"), "got: {msg}");
    }

    #[test]
    fn five_minute_then_one_hour_is_rejected() {
        let five = cc_5m();
        let one = cc_1h();
        let bps = vec![
            Breakpoint {
                position: BreakpointPosition::Tools,
                control: &five,
            },
            Breakpoint {
                position: BreakpointPosition::System,
                control: &one,
            },
        ];
        let err = validate(&bps).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("1h"), "got: {msg}");
        assert!(msg.contains("after a 5m"), "got: {msg}");
    }

    #[test]
    fn one_hour_then_five_minute_is_valid() {
        let five = cc_5m();
        let one = cc_1h();
        let bps = vec![
            Breakpoint {
                position: BreakpointPosition::Tools,
                control: &one,
            },
            Breakpoint {
                position: BreakpointPosition::System,
                control: &five,
            },
        ];
        validate(&bps).unwrap();
    }

    #[test]
    fn missing_ttl_treated_as_5m_default() {
        let no_ttl = CacheControl {
            kind: "ephemeral".into(),
            ttl: None,
        };
        let one = cc_1h();
        let bps = vec![
            Breakpoint {
                position: BreakpointPosition::Tools,
                control: &no_ttl,
            },
            Breakpoint {
                position: BreakpointPosition::System,
                control: &one,
            },
        ];
        let err = validate(&bps).unwrap_err();
        assert!(err.to_string().contains("after a 5m"));
    }

    #[test]
    fn cache_control_serializes_with_explicit_ttl() {
        let cc = cc_5m();
        let v = serde_json::to_value(&cc).unwrap();
        assert_eq!(v["type"], "ephemeral");
        assert_eq!(v["ttl"], "5m");
    }

    #[test]
    fn unknown_kind_forwards_verbatim() {
        // Arrange: a `type` routectl does not recognize. Vocabulary is
        // upstream's to define; routectl only checks structure.
        let cc = CacheControl {
            kind: "banana".into(),
            ttl: Some("5m".into()),
        };
        let bps = vec![Breakpoint {
            position: BreakpointPosition::Tools,
            control: &cc,
        }];

        // Act
        let result = validate(&bps);

        // Assert: accepted, and the kind reaches the wire unchanged.
        result.unwrap();
        let v = serde_json::to_value(&cc).unwrap();
        assert_eq!(v["type"], "banana");
        assert_eq!(v["ttl"], "5m");
    }

    #[test]
    fn unknown_ttl_forwards_verbatim() {
        // Arrange
        let cc = CacheControl {
            kind: "ephemeral".into(),
            ttl: Some("forever".into()),
        };
        let bps = vec![Breakpoint {
            position: BreakpointPosition::Tools,
            control: &cc,
        }];

        // Act
        let result = validate(&bps);

        // Assert
        result.unwrap();
        let v = serde_json::to_value(&cc).unwrap();
        assert_eq!(v["ttl"], "forever");
    }

    #[test]
    fn unknown_ttl_does_not_open_the_one_hour_after_five_minute_window() {
        // Arrange: an unrecognized ttl is not "5m", so a following 1h
        // breakpoint is not an ordering violation -- the unknown marker
        // degrades to the longest-TTL side of the walk. The 5m-first
        // counterpart (`five_minute_then_one_hour_is_rejected`) is the
        // positive control proving this walk can still fail.
        let unknown = CacheControl {
            kind: "ephemeral".into(),
            ttl: Some("forever".into()),
        };
        let one = cc_1h();
        let bps = vec![
            Breakpoint {
                position: BreakpointPosition::Tools,
                control: &unknown,
            },
            Breakpoint {
                position: BreakpointPosition::System,
                control: &one,
            },
        ];

        // Act
        let result = validate(&bps);

        // Assert
        result.unwrap();
    }

    #[test]
    fn cache_control_serializes_without_ttl_when_none() {
        let cc = CacheControl {
            kind: "ephemeral".into(),
            ttl: None,
        };
        let v = serde_json::to_value(&cc).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("type").unwrap(), "ephemeral");
        assert!(!obj.contains_key("ttl"));
    }

    // --- FrozenFloor / shared-walk tests ---

    use crate::{
        ChatRequest, ContentPart, CustomTool, KnownContentPart, Message, MessageContent, Role,
        SystemBlock, SystemContent, ToolDef,
    };
    use serde_json::json;

    fn user_text_msg(text: &str, cc: Option<CacheControl>) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: text.into(),
                citations: None,
                cache_control: cc,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn frozen_floor_no_markers_is_zero() {
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };
        let floor = compute_frozen_floor(&req);
        assert_eq!(floor.caller_breakpoint_count(), 0);
        assert!(!floor.has_caller_breakpoints());
        assert!(floor.positions().is_empty());
    }

    #[test]
    fn frozen_floor_counts_builtin_tool_and_top_level() {
        // A builtin tool (ToolDef::Other) carrying cache_control AND a
        // top-level cache_control marker must both be counted.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_text_msg("hi", None)].into(),
            tools: Some(vec![ToolDef::Other(json!({
                "type": "web_search_20250901",
                "name": "web_search",
                "cache_control": {"type": "ephemeral"}
            }))]),
            cache_control: Some(CacheControl::ephemeral_5m()),
            ..Default::default()
        };
        let floor = compute_frozen_floor(&req);
        assert_eq!(floor.caller_breakpoint_count(), 2);
        assert_eq!(
            floor.positions(),
            &[BreakpointPosition::Tools, BreakpointPosition::TopLevel]
        );
    }

    #[test]
    fn frozen_floor_four_positions_in_prefix_order() {
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "calc".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                cache_control: Some(CacheControl::ephemeral_5m()),
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            system: Some(SystemContent::Blocks(vec![SystemBlock {
                kind: "text".into(),
                text: "sys".into(),
                cache_control: Some(CacheControl::ephemeral_5m()),
                citations: None,
            }])),
            messages: vec![user_text_msg("hi", Some(CacheControl::ephemeral_5m()))].into(),
            cache_control: Some(CacheControl::ephemeral_5m()),
            ..Default::default()
        };
        let floor = compute_frozen_floor(&req);
        assert_eq!(floor.caller_breakpoint_count(), 4);
        assert_eq!(
            floor.positions(),
            &[
                BreakpointPosition::Tools,
                BreakpointPosition::System,
                BreakpointPosition::Messages,
                BreakpointPosition::TopLevel,
            ]
        );
    }

    #[test]
    fn validate_source_rejects_over_cap_chatrequest() {
        // Five message-level markers exceed MAX_BREAKPOINTS; the shared
        // walk + validate_source must surface the cap error.
        let parts: Vec<ContentPart> = (0..5)
            .map(|_| {
                ContentPart::Known(KnownContentPart::Text {
                    text: "x".into(),
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                })
            })
            .collect();
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(parts),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            ..Default::default()
        };
        let err = validate_source(&req).unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"), "got: {err}");
    }

    #[test]
    fn validate_source_accepts_well_ordered_chatrequest() {
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "calc".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                cache_control: Some(CacheControl::ephemeral_1h()),
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            messages: vec![user_text_msg("hi", Some(CacheControl::ephemeral_5m()))].into(),
            ..Default::default()
        };
        validate_source(&req).unwrap();
    }

    // --- mutable_suffix_start tests ---

    #[test]
    fn mutable_suffix_start_markers_on_0_and_2_of_5_returns_3() {
        // Arrange: messages 0 and 2 carry caller markers; last frozen = 2.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_text_msg("a", Some(cc_5m())),
                user_text_msg("b", None),
                user_text_msg("c", Some(cc_5m())),
                user_text_msg("d", None),
                user_text_msg("e", None),
            ]
            .into(),
            ..Default::default()
        };
        // Act
        let start = mutable_suffix_start(&req);

        // Assert
        assert_eq!(start, Some(3));
    }

    #[test]
    fn mutable_suffix_start_markers_only_on_tools_and_system_returns_0() {
        // Arrange: caller markers on tools + system (which sit BEFORE the
        // messages), none on any message, and NO top-level marker.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "calc".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                cache_control: Some(cc_5m()),
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            system: Some(SystemContent::Blocks(vec![SystemBlock {
                kind: "text".into(),
                text: "sys".into(),
                cache_control: Some(cc_5m()),
                citations: None,
            }])),
            messages: vec![user_text_msg("hi", None), user_text_msg("there", None)].into(),
            ..Default::default()
        };
        let floor = compute_frozen_floor(&req);

        // Act
        let start = mutable_suffix_start(&req);

        // Assert: tools/system bytes precede the messages, so all messages
        // remain mutable.
        assert!(floor.has_caller_breakpoints());
        assert_eq!(start, Some(0));
    }

    #[test]
    fn mutable_suffix_start_top_level_marker_freezes_whole_prefix_returns_none() {
        // Arrange: a top-level caller cache_control (Anthropic automatic
        // caching) freezes the ENTIRE prefix, including all messages.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_text_msg("hi", None), user_text_msg("there", None)].into(),
            cache_control: Some(cc_5m()),
            ..Default::default()
        };
        let floor = compute_frozen_floor(&req);

        // Act
        let start = mutable_suffix_start(&req);

        // Assert: no mutable message tail under a top-level breakpoint.
        assert!(floor.has_caller_breakpoints());
        assert_eq!(start, None);
    }

    #[test]
    fn mutable_suffix_start_marker_on_final_message_returns_none() {
        // Arrange: the only message marker is on the final message.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_text_msg("a", None), user_text_msg("b", Some(cc_5m()))].into(),
            ..Default::default()
        };

        // Act
        let start = mutable_suffix_start(&req);

        // Assert
        assert_eq!(start, None);
    }

    #[test]
    fn mutable_suffix_start_single_message_with_marker_returns_none() {
        // Arrange: exactly one message, carrying a marker -- the minimum
        // values for the `i + 1 >= len` guard (i = 0, len = 1).
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_text_msg("a", Some(cc_5m()))].into(),
            ..Default::default()
        };

        // Act
        let start = mutable_suffix_start(&req);

        // Assert: the marker sits on the final (only) message -- no tail.
        assert_eq!(start, None);
    }

    #[test]
    fn mutable_suffix_start_empty_messages_returns_none() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![].into(),
            ..Default::default()
        };

        // Act
        let start = mutable_suffix_start(&req);

        // Assert
        assert_eq!(start, None);
    }

    #[test]
    fn mutable_suffix_start_no_breakpoints_anywhere_returns_0() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_text_msg("a", None), user_text_msg("b", None)].into(),
            ..Default::default()
        };
        let floor = compute_frozen_floor(&req);

        // Act
        let start = mutable_suffix_start(&req);

        // Assert
        assert!(!floor.has_caller_breakpoints());
        assert_eq!(start, Some(0));
    }

    #[test]
    fn mutable_suffix_start_does_not_mutate_request() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_text_msg("a", Some(cc_5m())), user_text_msg("b", None)].into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let _ = mutable_suffix_start(&req);

        // Assert: byte-identical (ChatRequest has no PartialEq).
        let after = serde_json::to_value(&req).unwrap();
        assert_eq!(before, after);
    }

    // --- front_breakpoint_slot tests ---

    fn custom_tool(name: &str) -> ToolDef {
        ToolDef::Custom(CustomTool {
            name: name.into(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })
    }

    fn system_blocks(texts: &[&str]) -> SystemContent {
        SystemContent::Blocks(
            texts
                .iter()
                .map(|t| SystemBlock {
                    kind: "text".into(),
                    text: (*t).into(),
                    cache_control: None,
                    citations: None,
                })
                .collect(),
        )
    }

    #[test]
    fn front_slot_system_blocks_only_is_last_system_block() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(system_blocks(&["a", "b"])),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, Some(FrontSlot::LastSystemBlock { block_index: 1 }));
    }

    #[test]
    fn front_slot_prefers_system_over_tools_when_both_present() {
        // Arrange: a tools-slot marker can be dropped under
        // tool_choice = "none", so System must win.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(system_blocks(&["sys"])),
            tools: Some(vec![custom_tool("calc")]),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, Some(FrontSlot::LastSystemBlock { block_index: 0 }));
    }

    #[test]
    fn front_slot_tools_only_is_last_custom_tool() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            tools: Some(vec![custom_tool("calc"), custom_tool("grep")]),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, Some(FrontSlot::LastCustomTool { tool_index: 1 }));
    }

    #[test]
    fn front_slot_skips_trailing_blank_system_block() {
        // Arrange: the final block is whitespace-only. The Converse egress
        // skips blank blocks individually, so a marker there would never
        // reach the wire -- the anchor must be the last NONBLANK block.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(system_blocks(&["real instructions", "   \n\t "])),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert: index 0, the block the wire keeps -- not index 1.
        assert_eq!(slot, Some(FrontSlot::LastSystemBlock { block_index: 0 }));
    }

    #[test]
    fn front_slot_all_blank_system_blocks_falls_back_to_custom_tool() {
        // Arrange: every system block is blank, so both egresses drop the
        // whole system via is_blank. The tools anchor must not be blocked.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(system_blocks(&["", "  "])),
            tools: Some(vec![custom_tool("calc")]),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, Some(FrontSlot::LastCustomTool { tool_index: 0 }));
    }

    #[test]
    fn front_slot_all_blank_system_blocks_no_tools_is_none() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(system_blocks(&["", "   "])),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, None);
    }

    #[test]
    fn front_slot_tool_index_skips_trailing_builtin_tool() {
        // Arrange: the last tool is an opaque `Other`; the anchor is the
        // last TYPED custom tool.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            tools: Some(vec![
                custom_tool("calc"),
                ToolDef::Other(json!({"type": "web_search_20250901", "name": "web_search"})),
            ]),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, Some(FrontSlot::LastCustomTool { tool_index: 0 }));
    }

    #[test]
    fn eligible_system_block_index_agrees_with_resolved_slot() {
        // The injector resolves its target through the same rule the
        // selector used; the two must never diverge.
        let system = system_blocks(&["a", "b", "  "]);
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(system.clone()),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);
        let direct = eligible_system_block_index(&system);

        // Assert
        assert_eq!(direct, Some(1));
        assert_eq!(slot, Some(FrontSlot::LastSystemBlock { block_index: 1 }));
    }

    #[test]
    fn eligible_system_block_index_is_none_for_flat_text() {
        assert_eq!(
            eligible_system_block_index(&SystemContent::Text("flat".into())),
            None
        );
    }

    #[test]
    fn system_block_wire_eligibility_matches_is_blank_per_block_rule() {
        // is_blank calls a Blocks system blank when EVERY block's text is
        // blank; per-block eligibility must use the same blank predicate.
        let blank = SystemBlock {
            kind: "text".into(),
            text: "  \n ".into(),
            cache_control: None,
            citations: None,
        };
        let real = SystemBlock {
            kind: "text".into(),
            text: "hi".into(),
            cache_control: None,
            citations: None,
        };
        assert!(!system_block_is_wire_eligible(&blank));
        assert!(system_block_is_wire_eligible(&real));
        assert!(SystemContent::Blocks(vec![blank.clone()]).is_blank());
        assert!(!SystemContent::Blocks(vec![blank, real]).is_blank());
    }

    #[test]
    fn front_slot_flat_string_system_no_tools_is_none() {
        // Arrange: SystemContent::Text has no per-block marker field, and
        // lifting Text -> Blocks is a banned wire-shape change.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(SystemContent::Text("you are helpful".into())),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert: no anchor, and the flat string is still a flat string.
        assert_eq!(slot, None);
        let after = serde_json::to_value(&req).unwrap();
        assert_eq!(before["system"], json!("you are helpful"));
        assert_eq!(before, after);
    }

    #[test]
    fn front_slot_flat_string_system_with_custom_tool_is_last_custom_tool() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(SystemContent::Text("you are helpful".into())),
            tools: Some(vec![custom_tool("calc")]),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, Some(FrontSlot::LastCustomTool { tool_index: 0 }));
    }

    #[test]
    fn front_slot_empty_system_blocks_is_none() {
        // Arrange: an empty Blocks array has no block to anchor on.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(SystemContent::Blocks(vec![])),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, None);
    }

    #[test]
    fn front_slot_builtin_tool_only_is_none() {
        // Arrange: ToolDef::Other is an opaque payload preserved verbatim
        // through the egresses -- not an injection anchor.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            tools: Some(vec![ToolDef::Other(json!({
                "type": "web_search_20250901",
                "name": "web_search"
            }))]),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, None);
    }

    #[test]
    fn front_slot_no_system_no_tools_is_none() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_text_msg("hi", None)].into(),
            ..Default::default()
        };

        // Act
        let slot = front_breakpoint_slot(&req);

        // Assert
        assert_eq!(slot, None);
    }

    /// This test IS the D1 ordering contract: a front marker never lands on
    /// a message-part position, so `mutable_suffix_start`'s domain is
    /// byte-identical with and without front-marker injection.
    #[test]
    fn front_slot_is_never_a_messages_position() {
        // Arrange: every request shape that can yield a slot, including ones
        // whose messages carry their own caller markers.
        let shapes = vec![
            ChatRequest {
                model: "claude-sonnet-4".into(),
                system: Some(system_blocks(&["a", "b"])),
                messages: vec![user_text_msg("hi", Some(cc_5m()))].into(),
                ..Default::default()
            },
            ChatRequest {
                model: "claude-sonnet-4".into(),
                system: Some(system_blocks(&["sys"])),
                tools: Some(vec![custom_tool("calc")]),
                messages: vec![user_text_msg("hi", Some(cc_1h()))].into(),
                ..Default::default()
            },
            ChatRequest {
                model: "claude-sonnet-4".into(),
                tools: Some(vec![custom_tool("calc")]),
                messages: vec![user_text_msg("hi", Some(cc_5m()))].into(),
                ..Default::default()
            },
            ChatRequest {
                model: "claude-sonnet-4".into(),
                system: Some(SystemContent::Text("flat".into())),
                messages: vec![user_text_msg("hi", None)].into(),
                ..Default::default()
            },
            ChatRequest {
                model: "claude-sonnet-4".into(),
                system: Some(system_blocks(&["real", "  "])),
                messages: vec![user_text_msg("hi", Some(cc_5m()))].into(),
                ..Default::default()
            },
            ChatRequest {
                model: "claude-sonnet-4".into(),
                system: Some(system_blocks(&["", " "])),
                tools: Some(vec![custom_tool("calc")]),
                messages: vec![user_text_msg("hi", Some(cc_5m()))].into(),
                ..Default::default()
            },
        ];

        for req in &shapes {
            // Act
            let slot = front_breakpoint_slot(req);

            // Assert
            if let Some(slot) = slot {
                assert_ne!(
                    slot.position(),
                    BreakpointPosition::Messages,
                    "front slot {slot:?} resolved to a message-part position"
                );
                assert!(matches!(
                    slot.position(),
                    BreakpointPosition::System | BreakpointPosition::Tools
                ));
            }
        }
    }

    #[test]
    fn front_breakpoint_slot_does_not_mutate_request() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(system_blocks(&["a", "b"])),
            tools: Some(vec![custom_tool("calc")]),
            messages: vec![user_text_msg("hi", Some(cc_5m()))].into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let _ = front_breakpoint_slot(&req);

        // Assert: byte-identical (ChatRequest has no PartialEq).
        let after = serde_json::to_value(&req).unwrap();
        assert_eq!(before, after);
    }
}
