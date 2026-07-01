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
    #[serde(rename = "type")]
    pub kind: String,
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
    pub position: BreakpointPosition,
    pub control: &'a CacheControl,
}

/// Cache prefix order: tools -> system -> messages. Breakpoints in earlier
/// positions are seen first by the cache; longer TTLs (1h) must come before
/// shorter (5m).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BreakpointPosition {
    Tools,
    System,
    Messages,
    /// Top-level auto-cache marker. Counts toward the 4-breakpoint cap.
    TopLevel,
}

/// Allowed `cache_control.type` values per Anthropic spec. Today only
/// `ephemeral` is defined; the spec leaves room for future kinds, but
/// shipping a non-allowlisted kind to upstream just produces a vague
/// 400. Validating up front gives the operator a precise error.
const ALLOWED_KINDS: &[&str] = &["ephemeral"];

/// Allowed `cache_control.ttl` values. Anthropic accepts only "5m"
/// and "1h" today.
const ALLOWED_TTLS: &[&str] = &["5m", "1h"];

/// Validate a sequence of breakpoints against Anthropic's invariants:
/// (1) at most 4 breakpoints, (2) longer TTLs (1h) must appear before
/// shorter ones (5m) in cache-prefix order, (3) every breakpoint's
/// `type` must be in `ALLOWED_KINDS`, (4) every breakpoint's `ttl`
/// (when present) must be in `ALLOWED_TTLS`. All four emit
/// `Error::Validation` with a position-aware message.
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
        if !ALLOWED_KINDS.contains(&bp.control.kind.as_str()) {
            return Err(Error::Validation(format!(
                "cache_control: unknown type `{}` at {:?}; allowed: {ALLOWED_KINDS:?}",
                bp.control.kind, bp.position,
            )));
        }
        if let Some(ttl) = bp.control.ttl.as_deref()
            && !ALLOWED_TTLS.contains(&ttl)
        {
            return Err(Error::Validation(format!(
                "cache_control: unknown ttl `{}` at {:?}; allowed: {ALLOWED_TTLS:?}",
                ttl, bp.position,
            )));
        }
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
    pub position: BreakpointPosition,
    pub control: CacheControl,
}

impl OwnedBreakpoint {
    /// Construct an owned breakpoint at `position` carrying `control`.
    pub fn new(position: BreakpointPosition, control: CacheControl) -> Self {
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
/// ASSEMBLED wire body and counts a different set, because assembly is lossy:
/// `tool_choice="none"` suppresses tools, the billing-attribution strip drops
/// a block, a legacy `Role::System` lift flattens cache_control away, and
/// `Role::Tool` Parts collapse into a single unmarked block. Validating this
/// canonical pre-image where the wire post-image is required would change the
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
        for m in &self.messages {
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
    pub fn caller_breakpoint_count(&self) -> usize {
        self.positions.len()
    }

    /// The occupied positions, in cache-prefix order.
    pub fn positions(&self) -> &[BreakpointPosition] {
        &self.positions
    }

    /// Whether the caller supplied at least one breakpoint.
    pub fn has_caller_breakpoints(&self) -> bool {
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
    fn unknown_kind_rejected() {
        let cc = CacheControl {
            kind: "banana".into(),
            ttl: Some("5m".into()),
        };
        let bps = vec![Breakpoint {
            position: BreakpointPosition::Tools,
            control: &cc,
        }];
        let err = validate(&bps).unwrap_err();
        assert!(
            err.to_string().contains("unknown type `banana`"),
            "msg: {err}"
        );
    }

    #[test]
    fn unknown_ttl_rejected() {
        let cc = CacheControl {
            kind: "ephemeral".into(),
            ttl: Some("forever".into()),
        };
        let bps = vec![Breakpoint {
            position: BreakpointPosition::Tools,
            control: &cc,
        }];
        let err = validate(&bps).unwrap_err();
        assert!(
            err.to_string().contains("unknown ttl `forever`"),
            "msg: {err}"
        );
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
            messages: vec![user_text_msg("hi", None)],
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
            messages: vec![user_text_msg("hi", None)],
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
            messages: vec![user_text_msg("hi", Some(CacheControl::ephemeral_5m()))],
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
            }],
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
            messages: vec![user_text_msg("hi", Some(CacheControl::ephemeral_5m()))],
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
            ],
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
            messages: vec![user_text_msg("hi", None), user_text_msg("there", None)],
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
            messages: vec![user_text_msg("hi", None), user_text_msg("there", None)],
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
            messages: vec![user_text_msg("a", None), user_text_msg("b", Some(cc_5m()))],
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
            messages: vec![user_text_msg("a", Some(cc_5m()))],
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
            messages: vec![],
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
            messages: vec![user_text_msg("a", None), user_text_msg("b", None)],
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
            messages: vec![user_text_msg("a", Some(cc_5m())), user_text_msg("b", None)],
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let _ = mutable_suffix_start(&req);

        // Assert: byte-identical (ChatRequest has no PartialEq).
        let after = serde_json::to_value(&req).unwrap();
        assert_eq!(before, after);
    }
}
