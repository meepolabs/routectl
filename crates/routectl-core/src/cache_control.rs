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
        if let Some(ttl) = bp.control.ttl.as_deref() {
            if !ALLOWED_TTLS.contains(&ttl) {
                return Err(Error::Validation(format!(
                    "cache_control: unknown ttl `{}` at {:?}; allowed: {ALLOWED_TTLS:?}",
                    ttl, bp.position,
                )));
            }
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
}
