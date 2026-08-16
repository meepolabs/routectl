//! Provider-agnostic normalized subscription-quota vocabulary.
//!
//! Every upstream family reports its remaining subscription budget in its own
//! shape and its own units: Anthropic sends a per-window header triad with
//! fractions, Codex sends an integer percent and its own window-minutes
//! declaration. Those numbers are NOT comparable across families, which is
//! why a routing decision cannot read them directly. This module owns the one
//! shape they all reduce to, and nothing more -- the per-family extraction,
//! the curated role table, the store and the placement decision are each
//! their own concern.
//!
//! Three properties are load-bearing:
//!
//! - **Unknown is UNREPRESENTABLE as a number.** `QuotaWindow` is an
//!   algebraic `Unknown | Known { .. }`, never an `Option<f64>` and never a
//!   sentinel. The pressure to represent "we do not know" as a number always
//!   pushes toward the most permissive value, and here the most permissive
//!   value would make the LEAST-trustworthy seat the MOST attractive
//!   placement target -- the exact inversion of the workspace's fail-closed
//!   rule that unknown facts never enable more aggressive behavior. The RPM
//!   gate's `rpm_available.unwrap_or(f64::INFINITY)` convention is correct
//!   for RPM, where an absent limit genuinely means unlimited; it is wrong
//!   here, where an absent reading means no evidence. That convention must
//!   not cross into this module in any form.
//! - **`0.0` is a VALID observation, structurally distinct from unknown.** A
//!   seat that reports a genuinely empty window is the best placement target
//!   there is. Collapsing it into unknown would discard the strongest signal
//!   the feature exists to use, so the two can never compare equal.
//! - **The derive lists are deliberately minimal.** `Debug, Clone,
//!   PartialEq` and nothing else on the quota value types. No `Default`, so
//!   no call site can construct "known 0%" by accident -- a defaulted zero
//!   reads as maximal headroom and is indistinguishable from a real reading.
//!   No `Serialize`/`Deserialize`, so persisting this shape is not a
//!   one-liner and the field names never become a de-facto wire contract for
//!   a shape that is expected to be reshaped. No `Ord`/`PartialOrd`, because
//!   ranking happens over SEATS, not over windows, and an ordering on a
//!   window would invite exactly the unknown-vs-known comparison the first
//!   property forbids. No `Display`, so no diagnostic can render a quota
//!   value without deciding for itself how to word unknown.
//!
//! The whole tree is crate-private. The shape is expected to be wrong on the
//! first cut, and a `pub` item in a baselined crate is a one-way door: every
//! reshape here is an in-crate refactor instead of a workspace-wide public-API
//! break. There is deliberately no facade re-export here either -- a consumer
//! names the submodule it depends on, so the module a type lives in stays
//! visible at every call site while the shape is still moving.

pub mod curation;
pub mod feed;
pub mod freshness;
pub mod key;
pub mod reduce;
pub mod store;
pub mod window;
