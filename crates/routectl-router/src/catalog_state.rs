//! Cross-version catalog drift observability: `catalog_state.json`.
//!
//! A SEPARATE, purely rebuildable state file (never behavioral, unlike
//! [`crate::catalog_overlay`]): it records the baked catalog row this
//! build last observed for every IN-USE selector (a
//! `(provider_kind, model)` pair a configured, selectable model
//! actually references), keyed against the `CATALOG_VERSION` that
//! produced it. On the boot where `CATALOG_VERSION` changes,
//! [`check_drift_and_persist_state`] diffs the prior snapshot against
//! today's baked rows and emits one structured `tracing::warn!` per
//! selector whose row actually changed -- making a silent codegen
//! refresh visible to the operator instead of a mystery pricing
//! shift months later.
//!
//! NEVER blocks serve: every failure mode (missing file, corrupt JSON,
//! a `schema_version` too new to understand, a write that fails) is
//! caught inside this module and turned into a `tracing::warn!` plus a
//! rebuild-from-scratch, never a propagated `Err`. Losing this file
//! costs exactly one skipped diff on the next `CATALOG_VERSION` bump --
//! there is nothing here worth failing startup over.
//!
//! Writer discipline extends the OAuth credentials-file standard
//! (`routectl-auth/src/oauth/file_io.rs`) with the post-rename
//! parent-directory `fsync` `crate::catalog_overlay` already carries (not
//! yet backported to routectl-auth): temp file in the same directory,
//! `0o600` set before the write, `fsync` the temp file, atomic `rename`,
//! `0o600` re-set after, then `fsync` the PARENT DIRECTORY so the
//! rename itself survives a crash (mirrors
//! `crate::config_migrate::write_config_atomic`). Unlike the overlay's
//! `save`, there is deliberately NO revision check here: this file is
//! rebuildable observability data, so a last-write-wins race between
//! two boots is harmless (the loser's boot simply gets re-diffed on the
//! next `CATALOG_VERSION` change from whichever snapshot won).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use routectl_auth::atomic_write::{FsyncPolicy, write_0600_atomic_with_policy};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::catalog::{CatalogRow, lookup, lookup_overlay_cell};
use crate::catalog_overlay::CatalogOverlay;

/// Schema version this build understands for `catalog_state.json`. A
/// file whose `schema_version` exceeds this is treated exactly like a
/// corrupt file by [`check_drift_and_persist_state`]: warn once, skip
/// this boot's diff, rebuild. Unlike the overlay, there is no
/// fail-closed posture to preserve here -- this file carries no
/// behavior, so "rebuild from scratch" is always the safe answer.
pub const CATALOG_STATE_SCHEMA_VERSION: u32 = 1;

/// On-disk cross-version drift state: the `CATALOG_VERSION` this
/// build's serve process last observed, and the baked row it saw at
/// that time for every in-use selector.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogState {
    pub schema_version: u32,
    pub last_seen_catalog_version: u32,
    /// Selector (see [`selector_key`]) -> the baked row
    /// [`crate::catalog::lookup`] returned for it as of
    /// `last_seen_catalog_version`.
    pub in_use_snapshot: BTreeMap<String, CatalogRow>,
}

impl Default for CatalogState {
    fn default() -> Self {
        Self {
            schema_version: CATALOG_STATE_SCHEMA_VERSION,
            last_seen_catalog_version: 0,
            in_use_snapshot: BTreeMap::new(),
        }
    }
}

/// Hand-written `Deserialize`: `CatalogRow` carries `tier: Option<&'static
/// str>`, and deriving `Deserialize` on a type that nests `CatalogRow`
/// inside another `#[derive(Deserialize)]` type requires proving `'de:
/// 'static` for an arbitrary caller-chosen `'de` -- unsatisfiable, so the
/// derive does not compile here (unlike `CatalogRow`'s OWN derive, which
/// never has to prove that for an arbitrary caller). This impl
/// deserializes into a fully-owned shadow ([`StoredRow`], `tier: Option<
/// String>`) and converts to the real `&'static str` tier via
/// [`StoredRow::into_catalog_row`].
impl<'de> Deserialize<'de> for CatalogState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Raw {
            schema_version: u32,
            last_seen_catalog_version: u32,
            in_use_snapshot: BTreeMap<String, StoredRow>,
        }

        let raw = Raw::deserialize(deserializer)?;
        let mut in_use_snapshot = BTreeMap::new();
        for (selector, stored) in raw.in_use_snapshot {
            let row = stored
                .into_catalog_row()
                .map_err(serde::de::Error::custom)?;
            in_use_snapshot.insert(selector, row);
        }
        Ok(Self {
            schema_version: raw.schema_version,
            last_seen_catalog_version: raw.last_seen_catalog_version,
            in_use_snapshot,
        })
    }
}

/// Fully-owned shadow of [`CatalogRow`] used ONLY as the `Deserialize`
/// target for `in_use_snapshot` values -- see [`CatalogState`]'s manual
/// `Deserialize` impl for why `CatalogRow` cannot derive it directly in
/// this nested position. Every field matches `CatalogRow`'s except
/// `tier`, which stays a plain owned `String` until
/// [`into_catalog_row`](StoredRow::into_catalog_row) maps it onto the
/// small fixed set of real `&'static str` tier tokens.
///
/// Deliberately NOT `#[serde(default)]` as a struct -- mirrors
/// [`crate::catalog_overlay::OverlayCell`]'s same deliberate choice: our
/// own writer always emits every field (including `null` for an unset
/// `Option`, since `CatalogRow`'s `Serialize` has no
/// `skip_serializing_if`), so a row object missing a field is truncated
/// or hand-edited input, not a legitimate forward-compat gap. Silently
/// defaulting a missing `wm`/`rm`/etc. to `0.0` would fabricate a false
/// drift signal instead of routing the file through the `Corrupt` path
/// [`check_drift_and_persist_state`] advertises.
///
/// `max_output_tokens` is the ONE exception, and per-field rather than
/// struct-wide: a snapshot written by a build before the column existed
/// legitimately has no such key, and the whole POINT of that snapshot is
/// the drift diff on the very boot that introduces the column. Rejecting
/// it as corrupt would skip the one comparison the operator needs. The
/// default (`None`, "no ceiling observed") is also what the older build
/// actually saw, so the diff it feeds is accurate rather than fabricated.
#[derive(Debug, Deserialize)]
struct StoredRow {
    wm: f32,
    rm: f32,
    ttl_seconds: u32,
    min_prefix_tokens: u32,
    has_storage_rent: bool,
    storage_rent: f32,
    auto_cacher: bool,
    tier: Option<String>,
    max_context_tokens: Option<u32>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    input_cost_per_token: Option<f32>,
    output_cost_per_token: Option<f32>,
    capabilities: BTreeMap<String, bool>,
}

impl StoredRow {
    /// Convert to a real [`CatalogRow`], mapping `tier` onto its
    /// `&'static str` token. `Err` on any tier string other than the two
    /// this build knows about -- a state file is rebuildable
    /// observability data, so an unrecognized tier (a newer routectl's
    /// tier vocabulary) is treated as corrupt input by the caller rather
    /// than silently coerced to `None`.
    fn into_catalog_row(self) -> Result<CatalogRow, String> {
        let tier = match self.tier.as_deref() {
            None => None,
            Some("5m") => Some("5m"),
            Some("1h") => Some("1h"),
            Some(other) => {
                return Err(format!("catalog state: unknown tier `{other}`"));
            }
        };
        Ok(CatalogRow {
            wm: self.wm,
            rm: self.rm,
            ttl_seconds: self.ttl_seconds,
            min_prefix_tokens: self.min_prefix_tokens,
            has_storage_rent: self.has_storage_rent,
            storage_rent: self.storage_rent,
            auto_cacher: self.auto_cacher,
            tier,
            max_context_tokens: self.max_context_tokens,
            max_output_tokens: self.max_output_tokens,
            input_cost_per_token: self.input_cost_per_token,
            output_cost_per_token: self.output_cost_per_token,
            capabilities: self.capabilities,
        })
    }
}

/// Errors from loading `catalog_state.json`. Every variant is folded
/// into the SAME "warn once, skip the diff, rebuild" posture by
/// [`check_drift_and_persist_state`] -- see the module doc.
#[derive(Debug, thiserror::Error)]
pub enum CatalogStateError {
    #[error("catalog state {path}: corrupt or invalid: {reason}")]
    Corrupt { path: String, reason: String },

    #[error(
        "catalog state {path}: schema_version {found} is newer than the {current} this build supports"
    )]
    VersionTooNew {
        path: String,
        found: u32,
        current: u32,
    },

    #[error("catalog state {path}: {reason}")]
    Io { path: String, reason: String },
}

/// Resolve the state file's default path: `catalog_state.json` inside
/// `routectl_config_dir()`, sibling to `catalog_overlay.json`.
#[must_use]
pub fn default_path() -> PathBuf {
    crate::config::routectl_config_dir().join("catalog_state.json")
}

/// Build the `"provider_kind:model"` key this module uses to name an
/// in-use catalog selector. Distinct from
/// [`crate::catalog::CachePricingSelector`]'s glob-keyed selectors:
/// this key names one CONCRETE resolved model string, never a pattern.
#[must_use]
pub fn selector_key(provider_kind: &str, model: &str) -> String {
    format!("{provider_kind}:{model}")
}

/// Load `catalog_state.json` at `path`.
///
/// - missing file -> `Ok(None)` (first run; not an error).
/// - corrupt / invalid JSON -> `Err(CatalogStateError::Corrupt)`.
/// - `schema_version` newer than this build understands ->
///   `Err(CatalogStateError::VersionTooNew)`.
/// - any other I/O failure -> `Err(CatalogStateError::Io)`.
fn load(path: &Path) -> Result<Option<CatalogState>, CatalogStateError> {
    let display = path.display().to_string();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CatalogStateError::Io {
                path: display,
                reason: e.to_string(),
            });
        }
    };

    let state: CatalogState =
        serde_json::from_slice(&bytes).map_err(|e| CatalogStateError::Corrupt {
            path: display.clone(),
            reason: e.to_string(),
        })?;

    if state.schema_version > CATALOG_STATE_SCHEMA_VERSION {
        return Err(CatalogStateError::VersionTooNew {
            path: display,
            found: state.schema_version,
            current: CATALOG_STATE_SCHEMA_VERSION,
        });
    }

    Ok(Some(state))
}

/// Persist `state` atomically as an owner-only (`0o600`) file via the
/// shared secret-file writer. Parent-directory fsync stays BEST-EFFORT:
/// this state file is a rebuildable cache, so a parent-fsync error must
/// not fail the save. No revision check -- see the module doc.
fn save(path: &Path, state: &CatalogState) -> Result<(), CatalogStateError> {
    let display = path.display().to_string();
    let json = serde_json::to_vec_pretty(state).map_err(|e| CatalogStateError::Io {
        path: display.clone(),
        reason: format!("serialize: {e}"),
    })?;
    write_0600_atomic_with_policy(path, &json, FsyncPolicy::BestEffort).map_err(|reason| {
        CatalogStateError::Io {
            path: display,
            reason,
        }
    })
}

/// Compute the current baked row for every in-use `(provider_kind,
/// model)` pair, keyed by [`selector_key`]. Pure baked lookup
/// ([`crate::catalog::lookup`] -- no overlay, no legacy
/// `[cache_pricing]` overrides): this file tracks the COMPILED-IN
/// catalog table across `CATALOG_VERSION` bumps, independent of any
/// operator override layer.
fn current_snapshot(in_use: &[(String, String)]) -> BTreeMap<String, CatalogRow> {
    in_use
        .iter()
        .map(|(provider_kind, model)| {
            (
                selector_key(provider_kind, model),
                lookup(provider_kind, model, None),
            )
        })
        .collect()
}

/// Check `catalog_state.json` for a `CATALOG_VERSION` change since the
/// last boot, emit a structured per-cell drift log for every in-use
/// selector whose baked row actually changed, and persist the
/// refreshed state. NEVER fails: every I/O or corruption error is
/// caught, warned once, and treated as "rebuild from an empty
/// baseline" -- this is observability, not behavior, and must
/// never block `serve`.
///
/// `in_use` is the caller's list of `(provider_kind, model)` pairs
/// currently referenced by configured, selectable `[models]` entries
/// -- i.e. what the built router actually resolved this boot (the same
/// derivation `crate::factory::apply_catalog_overlay` uses per resolved
/// model). `overlay` is this boot's loaded catalog overlay, used only
/// to compute the `overlay_masked` flag on a changed cell -- this
/// function never reads or writes the overlay file itself.
pub fn check_drift_and_persist_state(
    in_use: &[(String, String)],
    overlay: &CatalogOverlay,
    path: &Path,
) {
    let catalog_version = crate::catalog_baked::CATALOG_VERSION;
    let current = current_snapshot(in_use);

    match load(path) {
        Ok(None) => persist(path, catalog_version, current),
        Ok(Some(state)) if state.last_seen_catalog_version == catalog_version => {
            // No-op: this exact CATALOG_VERSION was already diffed (or
            // baselined) on a prior boot. Deliberately does not
            // rewrite the file -- an in-use set that grew or shrank
            // since that boot is baselined on the NEXT CATALOG_VERSION
            // change instead, keeping "exactly once per version" a
            // hard property rather than a best-effort one.
        }
        Ok(Some(state)) => {
            log_drift(in_use, &state.in_use_snapshot, &current, overlay);
            persist(path, catalog_version, current);
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                reason = %e,
                "catalog_state.json is corrupt, unreadable, or from a newer routectl \
                 build; skipping this boot's cross-version catalog drift diff and \
                 rebuilding the state file",
            );
            persist(path, catalog_version, current);
        }
    }
}

/// Serialize and atomically persist a fresh [`CatalogState`], warning
/// (never propagating) on write failure.
fn persist(path: &Path, catalog_version: u32, in_use_snapshot: BTreeMap<String, CatalogRow>) {
    let state = CatalogState {
        schema_version: CATALOG_STATE_SCHEMA_VERSION,
        last_seen_catalog_version: catalog_version,
        in_use_snapshot,
    };
    if let Err(e) = save(path, &state) {
        tracing::warn!(
            path = %path.display(),
            reason = %e,
            "failed to persist catalog_state.json; cross-version catalog drift \
             observability is degraded for the next CATALOG_VERSION change, but serve \
             continues normally",
        );
    }
}

/// Emit one structured `tracing::warn!` per in-use selector whose baked
/// row differs between `prior` (the last-persisted snapshot) and
/// `current` (this boot's baked lookup). A selector absent from
/// `prior` (newly in use since the last boot) has nothing to diff
/// against and is silently skipped -- it is simply added to the
/// persisted snapshot going forward.
fn log_drift(
    in_use: &[(String, String)],
    prior: &BTreeMap<String, CatalogRow>,
    current: &BTreeMap<String, CatalogRow>,
    overlay: &CatalogOverlay,
) {
    for (provider_kind, model) in in_use {
        let selector = selector_key(provider_kind, model);
        let Some(old_row) = prior.get(&selector) else {
            continue;
        };
        let Some(new_row) = current.get(&selector) else {
            continue;
        };
        let Some((impact, old_diff, new_diff)) = diff_row(old_row, new_row) else {
            continue;
        };
        let overlay_masked = lookup_overlay_cell(provider_kind, model, overlay).is_some();
        tracing::warn!(
            selector = selector.as_str(),
            impact_class = impact.label(),
            overlay_masked,
            old = old_diff.as_str(),
            new = new_diff.as_str(),
            "catalog baked row changed across a CATALOG_VERSION update",
        );
    }
}

/// Serialize `v` to a `serde_json::Value`, falling back to `Value::Null`
/// on the (practically unreachable, since every `CatalogRow` field is a
/// plain number/bool/string/map) serialize failure -- this is a
/// best-effort diagnostic log, never worth a panic.
fn jv<T: Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Escalation class for one changed catalog field, shared by the
/// baked-row drift log (`diff_row`) and the overlay import diff
/// (`crate::catalog_import::diff_overlay`). `Ord`-derived in ascending
/// severity (`DisplayOnly < CostAffecting < RoutingAffecting`) so
/// [`escalate`] -- folding several changed fields on the same row --
/// always keeps the highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImpactClass {
    /// Provenance-only: the value carried no pricing or routing
    /// consequence (`verified_at`, `source`).
    DisplayOnly,
    /// Changes the break-even $ math (`wm`, `rm`, `auto_cacher`, and the
    /// baked-only reserved economics fields `has_storage_rent` /
    /// `storage_rent` / `tier`, which shift write economics the same
    /// way `wm` does).
    CostAffecting,
    /// Changes WHETHER or HOW a request gets cached / routed
    /// (`ttl_seconds`, `min_prefix_tokens`, `max_context_tokens`,
    /// `max_output_tokens`, a capability flip, or a row's enable/disable
    /// state).
    RoutingAffecting,
}

impl ImpactClass {
    /// The stable label rendered in a diff row or a log line. A public
    /// contract once the import diff renders it to an operator: never
    /// rename, only add a class.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DisplayOnly => "display-only",
            Self::CostAffecting => "cost-affecting",
            Self::RoutingAffecting => "routing-affecting",
        }
    }
}

/// Fold a second changed field's class into an already-accumulated
/// [`ImpactClass`], keeping the higher of the two. Mixed changes on the
/// same row escalate to their highest-severity field.
#[must_use]
pub const fn escalate(a: ImpactClass, b: ImpactClass) -> ImpactClass {
    if matches!(
        (a, b),
        (ImpactClass::RoutingAffecting, _) | (_, ImpactClass::RoutingAffecting)
    ) {
        ImpactClass::RoutingAffecting
    } else if matches!(
        (a, b),
        (ImpactClass::CostAffecting, _) | (_, ImpactClass::CostAffecting)
    ) {
        ImpactClass::CostAffecting
    } else {
        ImpactClass::DisplayOnly
    }
}

/// One classifiable catalog field, spanning both the baked
/// [`CatalogRow`] (drift log) domain and the overlay `OverlayCell`
/// (import diff) domain -- see [`classify_field`]. `HasStorageRent` /
/// `StorageRent` / `Tier` only ever appear on the baked side (no
/// `OverlayCell` field carries them, by design -- see
/// `crate::catalog_import`'s module doc); `VerifiedAt` / `Source` only
/// ever appear on the overlay side (no `CatalogRow` field carries them);
/// `Enablement` is synthetic, standing in for a whole row flipping
/// between enabled and disabled rather than a single field's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpactField {
    /// The write multiplier.
    Wm,
    /// The read multiplier.
    Rm,
    /// The cache time-to-live.
    TtlSeconds,
    /// The minimum cacheable prefix size.
    MinPrefixTokens,
    /// The context window.
    MaxContextTokens,
    /// The output-token ceiling.
    MaxOutputTokens,
    /// The base input price, in dollars per token.
    InputCostPerToken,
    /// The base output price, in dollars per token.
    OutputCostPerToken,
    /// The capability priors.
    Capabilities,
    /// The auto-cacher flag.
    AutoCacher,
    /// The storage-rent-charging flag (baked-only).
    HasStorageRent,
    /// The storage-rent multiplier (baked-only).
    StorageRent,
    /// The TTL tier (baked-only).
    Tier,
    /// The verification stamp (overlay-only).
    VerifiedAt,
    /// The provenance (overlay-only).
    Source,
    /// Synthetic: a whole row flipping between enabled and disabled.
    Enablement,
}

/// The shared impact taxonomy (see the module-level classes on
/// [`ImpactClass`]): display-only (`verified_at`, `source`),
/// cost-affecting (`wm`, `rm`, `auto_cacher`, the base per-token rates,
/// plus the baked-only reserved economics fields), routing-affecting
/// (`ttl_seconds`, `min_prefix_tokens`, `max_context_tokens`,
/// `max_output_tokens`, a capability
/// flip, or a row's enable/disable state). Reused verbatim by
/// `crate::catalog_import::diff_overlay` for its own row labels.
#[must_use]
pub const fn classify_field(field: ImpactField) -> ImpactClass {
    match field {
        ImpactField::VerifiedAt | ImpactField::Source => ImpactClass::DisplayOnly,
        ImpactField::Wm
        | ImpactField::Rm
        | ImpactField::AutoCacher
        | ImpactField::HasStorageRent
        | ImpactField::StorageRent
        | ImpactField::InputCostPerToken
        | ImpactField::OutputCostPerToken
        | ImpactField::Tier => ImpactClass::CostAffecting,
        ImpactField::TtlSeconds
        | ImpactField::MinPrefixTokens
        | ImpactField::MaxContextTokens
        | ImpactField::MaxOutputTokens
        | ImpactField::Capabilities
        | ImpactField::Enablement => ImpactClass::RoutingAffecting,
    }
}

/// Diff two baked rows for the SAME selector across a `CATALOG_VERSION`
/// change. Returns `None` when every tracked field is identical (no
/// drift to log). Otherwise returns the changed fields' escalated
/// [`ImpactClass`] (the same classifier
/// `crate::catalog_import::diff_overlay` uses for its own row labels --
/// "one classifier drives both") plus compact-JSON `old`/`new` subsets
/// covering ONLY the fields that actually differ.
///
/// This function's field list must track [`CatalogRow`]'s exactly (see
/// `diff_row_covers_every_catalog_row_field` in the test module, an
/// exhaustive-destructure guard that fails to COMPILE if a field is
/// ever added to or removed from the row without a matching update
/// here) -- an omitted field would silently drop out of drift
/// detection.
fn diff_row(old: &CatalogRow, new: &CatalogRow) -> Option<(ImpactClass, String, String)> {
    let mut changed: Vec<(&'static str, Value, Value)> = Vec::new();
    let mut impact = ImpactClass::DisplayOnly;

    let mut note = |field: ImpactField, name: &'static str, old_v: Value, new_v: Value| {
        changed.push((name, old_v, new_v));
        impact = escalate(impact, classify_field(field));
    };

    if old.wm != new.wm {
        note(ImpactField::Wm, "wm", jv(old.wm), jv(new.wm));
    }
    if old.rm != new.rm {
        note(ImpactField::Rm, "rm", jv(old.rm), jv(new.rm));
    }
    if old.ttl_seconds != new.ttl_seconds {
        note(
            ImpactField::TtlSeconds,
            "ttl_seconds",
            jv(old.ttl_seconds),
            jv(new.ttl_seconds),
        );
    }
    if old.min_prefix_tokens != new.min_prefix_tokens {
        note(
            ImpactField::MinPrefixTokens,
            "min_prefix_tokens",
            jv(old.min_prefix_tokens),
            jv(new.min_prefix_tokens),
        );
    }
    if old.has_storage_rent != new.has_storage_rent {
        note(
            ImpactField::HasStorageRent,
            "has_storage_rent",
            jv(old.has_storage_rent),
            jv(new.has_storage_rent),
        );
    }
    if old.storage_rent != new.storage_rent {
        note(
            ImpactField::StorageRent,
            "storage_rent",
            jv(old.storage_rent),
            jv(new.storage_rent),
        );
    }
    if old.auto_cacher != new.auto_cacher {
        note(
            ImpactField::AutoCacher,
            "auto_cacher",
            jv(old.auto_cacher),
            jv(new.auto_cacher),
        );
    }
    if old.tier != new.tier {
        // A tier reclassification (e.g. a selector moving from the 5m
        // to the 1h breakpoint row in a codegen refresh) changes which
        // write-multiplier economics apply even when `wm`/`rm`
        // themselves happen to coincide -- always economics-impacting.
        note(ImpactField::Tier, "tier", jv(old.tier), jv(new.tier));
    }
    if old.max_context_tokens != new.max_context_tokens {
        note(
            ImpactField::MaxContextTokens,
            "max_context_tokens",
            jv(old.max_context_tokens),
            jv(new.max_context_tokens),
        );
    }
    if old.max_output_tokens != new.max_output_tokens {
        note(
            ImpactField::MaxOutputTokens,
            "max_output_tokens",
            jv(old.max_output_tokens),
            jv(new.max_output_tokens),
        );
    }
    if old.input_cost_per_token != new.input_cost_per_token {
        note(
            ImpactField::InputCostPerToken,
            "input_cost_per_token",
            jv(old.input_cost_per_token),
            jv(new.input_cost_per_token),
        );
    }
    if old.output_cost_per_token != new.output_cost_per_token {
        note(
            ImpactField::OutputCostPerToken,
            "output_cost_per_token",
            jv(old.output_cost_per_token),
            jv(new.output_cost_per_token),
        );
    }
    if old.capabilities != new.capabilities {
        note(
            ImpactField::Capabilities,
            "capabilities",
            jv(&old.capabilities),
            jv(&new.capabilities),
        );
    }

    if changed.is_empty() {
        return None;
    }

    let mut old_obj = Map::new();
    let mut new_obj = Map::new();
    for (field, old_value, new_value) in changed {
        old_obj.insert(field.to_string(), old_value);
        new_obj.insert(field.to_string(), new_value);
    }
    Some((
        impact,
        Value::Object(old_obj).to_string(),
        Value::Object(new_obj).to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_overlay::{OverlayCell, OverlaySource};

    /// Pin: fails to COMPILE if `CatalogRow` ever gains or loses a
    /// field without a matching update to `diff_row` (and `StoredRow`)
    /// above. See `diff_row`'s doc comment for why this guard exists.
    #[test]
    fn diff_row_covers_every_catalog_row_field() {
        let CatalogRow {
            wm: _,
            rm: _,
            ttl_seconds: _,
            min_prefix_tokens: _,
            has_storage_rent: _,
            storage_rent: _,
            auto_cacher: _,
            tier: _,
            max_context_tokens: _,
            max_output_tokens: _,
            input_cost_per_token: _,
            output_cost_per_token: _,
            capabilities: _,
        } = CatalogRow::sentinel();
    }

    /// A real, stable in-use pair the baked table matches with a
    /// tiered (5m) row -- mirrors the fixture other `catalog.rs` tests
    /// already use for `lookup("anthropic-api", "claude-opus-4-8", ..)`.
    const SELECTOR: (&str, &str) = ("anthropic-api", "claude-opus-4-8");

    fn in_use() -> Vec<(String, String)> {
        vec![(SELECTOR.0.to_string(), SELECTOR.1.to_string())]
    }

    fn current_row() -> CatalogRow {
        lookup(SELECTOR.0, SELECTOR.1, None)
    }

    // -----------------------------------------------------------------------
    // Shape + atomic write: 0600, no leftover tempfiles, round-trips.
    // -----------------------------------------------------------------------

    #[test]
    fn default_path_uses_catalog_state_json_basename() {
        let path = default_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("catalog_state.json")
        );
    }

    #[test]
    fn check_drift_first_run_persists_baseline_without_any_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path)
        });

        assert!(
            events.is_empty(),
            "first run must not emit any event: {events:?}"
        );

        let loaded = load(&path)
            .unwrap()
            .expect("state file must exist after first run");
        assert_eq!(loaded.schema_version, CATALOG_STATE_SCHEMA_VERSION);
        assert_eq!(
            loaded.last_seen_catalog_version,
            crate::catalog_baked::CATALOG_VERSION
        );
        assert_eq!(
            loaded
                .in_use_snapshot
                .get(&selector_key(SELECTOR.0, SELECTOR.1)),
            Some(&current_row())
        );
    }

    #[cfg(unix)]
    #[test]
    fn persisted_state_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "catalog_state.json must be 0600");
    }

    #[test]
    fn check_drift_leaves_no_leftover_tempfiles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);

        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "left tempfiles: {leftover:?}");
    }

    // -----------------------------------------------------------------------
    // No-op when the catalog version is unchanged: exactly-once per version.
    // -----------------------------------------------------------------------

    #[test]
    fn no_op_when_last_seen_catalog_version_matches_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");

        let state = CatalogState {
            schema_version: CATALOG_STATE_SCHEMA_VERSION,
            last_seen_catalog_version: crate::catalog_baked::CATALOG_VERSION,
            in_use_snapshot: BTreeMap::new(),
        };
        save(&path, &state).unwrap();
        let before = std::fs::read(&path).unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert!(
            events.is_empty(),
            "unchanged version must emit nothing: {events:?}"
        );
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "unchanged version must not rewrite the file");
    }

    #[test]
    fn second_start_on_the_same_version_after_a_diff_emits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");

        let mut drifted = current_row();
        drifted.wm += 1.0;
        let mut snapshot = BTreeMap::new();
        snapshot.insert(selector_key(SELECTOR.0, SELECTOR.1), drifted);
        let stale_version = crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1);
        save(
            &path,
            &CatalogState {
                schema_version: CATALOG_STATE_SCHEMA_VERSION,
                last_seen_catalog_version: stale_version,
                in_use_snapshot: snapshot,
            },
        )
        .unwrap();

        let first_boot = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });
        assert_eq!(
            first_boot.len(),
            1,
            "the version change must log exactly one drifted cell"
        );

        let second_boot = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });
        assert!(
            second_boot.is_empty(),
            "a second start on the same (now-current) version must emit nothing: {second_boot:?}"
        );
    }

    // -----------------------------------------------------------------------
    // CATALOG_VERSION change: per-cell structured drift log, in-use only,
    // impact_class + overlay_masked correctness.
    // -----------------------------------------------------------------------

    fn prior_state_with(row: CatalogRow, last_seen: u32) -> CatalogState {
        let mut snapshot = BTreeMap::new();
        snapshot.insert(selector_key(SELECTOR.0, SELECTOR.1), row);
        CatalogState {
            schema_version: CATALOG_STATE_SCHEMA_VERSION,
            last_seen_catalog_version: last_seen,
            in_use_snapshot: snapshot,
        }
    }

    #[test]
    fn version_change_logs_economics_drift_for_changed_wm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let mut drifted = current_row();
        drifted.wm += 1.0;
        save(
            &path,
            &prior_state_with(
                drifted,
                crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(
            event.field("selector"),
            Some(selector_key(SELECTOR.0, SELECTOR.1).as_str())
        );
        assert_eq!(event.field("impact_class"), Some("cost-affecting"));
        assert_eq!(event.field("overlay_masked"), Some("false"));
        assert!(event.field("old").unwrap().contains("\"wm\""));
        assert!(event.field("new").unwrap().contains("\"wm\""));
    }

    // -----------------------------------------------------------------------
    // Shared classifier: the SAME `classify_field` call this log's
    // `impact_class` field used above is exactly what
    // `crate::catalog_import::diff_overlay` calls for its own row
    // labels (see `catalog_import`'s
    // `diff_overlay_applies_a_cost_affecting_wm_change_over_baked` test)
    // -- a `wm` change is `cost-affecting` in both consumers.
    // -----------------------------------------------------------------------

    #[test]
    fn classify_field_wm_is_cost_affecting_matching_the_import_diffs_own_label() {
        assert_eq!(classify_field(ImpactField::Wm), ImpactClass::CostAffecting);
        assert_eq!(classify_field(ImpactField::Wm).label(), "cost-affecting");
    }

    #[test]
    fn escalate_keeps_the_highest_of_two_classes() {
        assert_eq!(
            escalate(ImpactClass::DisplayOnly, ImpactClass::CostAffecting),
            ImpactClass::CostAffecting
        );
        assert_eq!(
            escalate(ImpactClass::CostAffecting, ImpactClass::RoutingAffecting),
            ImpactClass::RoutingAffecting
        );
        assert_eq!(
            escalate(ImpactClass::RoutingAffecting, ImpactClass::DisplayOnly),
            ImpactClass::RoutingAffecting
        );
    }

    #[test]
    fn version_change_logs_window_drift_when_only_context_window_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let mut drifted = current_row();
        drifted.max_context_tokens = Some(drifted.max_context_tokens.unwrap_or(0) + 1);
        save(
            &path,
            &prior_state_with(
                drifted,
                crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("impact_class"), Some("routing-affecting"));
    }

    #[test]
    fn version_change_logs_economics_drift_for_changed_tier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let mut drifted = current_row();
        drifted.tier = Some("1h");
        save(
            &path,
            &prior_state_with(
                drifted,
                crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("impact_class"), Some("cost-affecting"));
        assert!(events[0].field("old").unwrap().contains("\"tier\""));
    }

    #[test]
    fn max_context_tokens_and_capabilities_changes_both_land_in_the_diff_subset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let mut drifted = current_row();
        drifted.max_context_tokens = Some(drifted.max_context_tokens.unwrap_or(0) + 1);
        drifted.capabilities.insert("web_search".to_string(), true);
        save(
            &path,
            &prior_state_with(
                drifted,
                crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("impact_class"), Some("routing-affecting"));
        // Both changed fields land in the subset, even though they share
        // one escalated class.
        assert!(
            events[0]
                .field("old")
                .unwrap()
                .contains("\"max_context_tokens\"")
        );
        assert!(events[0].field("old").unwrap().contains("\"capabilities\""));
    }

    #[test]
    fn version_change_logs_capabilities_only_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let mut drifted = current_row();
        drifted.capabilities.insert("web_search".to_string(), true);
        save(
            &path,
            &prior_state_with(
                drifted,
                crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("impact_class"), Some("routing-affecting"));
    }

    #[test]
    fn version_change_with_no_actual_row_change_emits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        save(
            &path,
            &prior_state_with(
                current_row(),
                crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert!(events.is_empty(), "identical rows must not log: {events:?}");
    }

    #[test]
    fn version_change_skips_selectors_absent_from_the_prior_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        // Prior snapshot has NO entry for our selector -- e.g. a model
        // added to `[models]` since the last boot.
        save(
            &path,
            &CatalogState {
                schema_version: CATALOG_STATE_SCHEMA_VERSION,
                last_seen_catalog_version: crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
                in_use_snapshot: BTreeMap::new(),
            },
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert!(
            events.is_empty(),
            "a newly-in-use selector has nothing to diff: {events:?}"
        );
    }

    #[test]
    fn version_change_ignores_selectors_no_longer_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let mut snapshot = BTreeMap::new();
        // A selector that was in use last boot but is NOT in `in_use()`
        // this boot, with a drifted row.
        let mut drifted = current_row();
        drifted.wm += 1.0;
        snapshot.insert(selector_key("openai-compat", "retired-model"), drifted);
        save(
            &path,
            &CatalogState {
                schema_version: CATALOG_STATE_SCHEMA_VERSION,
                last_seen_catalog_version: crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
                in_use_snapshot: snapshot,
            },
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert!(
            events.is_empty(),
            "a selector no longer in use must not be diffed: {events:?}"
        );
    }

    #[test]
    fn overlay_masked_true_when_an_overlay_cell_wins_over_the_changed_selector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let mut drifted = current_row();
        drifted.wm += 1.0;
        save(
            &path,
            &prior_state_with(
                drifted,
                crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
            ),
        )
        .unwrap();

        let mut cells = BTreeMap::new();
        cells.insert(
            format!("{}:{}", SELECTOR.0, SELECTOR.1),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-07-01".to_string(),
                wm: Some(9.99),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        let overlay = CatalogOverlay {
            schema_version: 1,
            revision: 1,
            cells,
        };

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &overlay, &path);
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("overlay_masked"), Some("true"));
    }

    #[test]
    fn overlay_masked_false_when_no_overlay_cell_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let mut drifted = current_row();
        drifted.wm += 1.0;
        save(
            &path,
            &prior_state_with(
                drifted,
                crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("overlay_masked"), Some("false"));
    }

    // -----------------------------------------------------------------------
    // Corrupt / unreadable / newer schema_version: warn once, never blocks,
    // rebuilds after.
    // -----------------------------------------------------------------------

    #[test]
    fn corrupt_state_file_warns_once_skips_diff_and_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        std::fs::write(&path, b"not json {{{").unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(
            events.len(),
            1,
            "corrupt file must warn exactly once: {events:?}"
        );
        assert!(events[0].message.contains("corrupt"));

        // Never blocks: the function returned normally (no panic, no
        // Result to propagate), and the state is rebuilt for next boot.
        let loaded = load(&path)
            .unwrap()
            .expect("state must be rebuilt after a corrupt boot");
        assert_eq!(
            loaded.last_seen_catalog_version,
            crate::catalog_baked::CATALOG_VERSION
        );
    }

    #[test]
    fn unknown_tier_string_is_treated_as_corrupt_warns_once_and_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let selector = selector_key(SELECTOR.0, SELECTOR.1);
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":1,"last_seen_catalog_version":0,"in_use_snapshot":{{"{selector}":{{
                    "wm":1.25,"rm":0.1,"ttl_seconds":300,"min_prefix_tokens":4096,
                    "has_storage_rent":false,"storage_rent":0.0,"auto_cacher":false,
                    "tier":"3h","max_context_tokens":null,"capabilities":{{}}
                }}}}}}"#
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(
            events.len(),
            1,
            "an unrecognized tier string must warn exactly once, same as corrupt JSON: {events:?}"
        );
        assert!(events[0].message.contains("corrupt"));

        let loaded = load(&path)
            .unwrap()
            .expect("state must still be rebuilt after an unknown-tier boot");
        assert_eq!(
            loaded.last_seen_catalog_version,
            crate::catalog_baked::CATALOG_VERSION
        );
    }

    #[test]
    fn row_object_missing_a_field_is_treated_as_corrupt_not_silently_defaulted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let selector = selector_key(SELECTOR.0, SELECTOR.1);
        // Omits "wm" entirely -- our own writer always emits every
        // field, so a missing key means truncated/hand-edited input.
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":1,"last_seen_catalog_version":0,"in_use_snapshot":{{"{selector}":{{
                    "rm":0.1,"ttl_seconds":300,"min_prefix_tokens":4096,
                    "has_storage_rent":false,"storage_rent":0.0,"auto_cacher":false,
                    "tier":null,"max_context_tokens":null,"capabilities":{{}}
                }}}}}}"#
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(
            events.len(),
            1,
            "a row missing a required field must warn as corrupt, not silently default: {events:?}"
        );
        assert!(events[0].message.contains("corrupt"));
    }

    #[test]
    fn a_snapshot_written_before_the_output_ceiling_column_still_diffs_rather_than_reading_corrupt()
    {
        // Arrange: a state file in the shape a build BEFORE the
        // `max_output_tokens` column wrote it -- every other field present,
        // that one key simply absent. Rejecting it as corrupt would skip the
        // one drift diff the column's introduction exists to surface.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        let selector = selector_key(SELECTOR.0, SELECTOR.1);
        let current = current_row();
        assert!(
            current.max_output_tokens.is_some(),
            "this test needs an in-use selector whose CURRENT row carries a ceiling, so the \
             absent prior key is a real diff"
        );
        let mut prior_row = serde_json::to_value(&current).expect("serialize row");
        prior_row
            .as_object_mut()
            .expect("row serializes as an object")
            .remove("max_output_tokens");
        let prior = serde_json::json!({
            "schema_version": CATALOG_STATE_SCHEMA_VERSION,
            "last_seen_catalog_version": crate::catalog_baked::CATALOG_VERSION.wrapping_sub(1),
            "in_use_snapshot": {selector.clone(): prior_row},
        });
        std::fs::write(&path, serde_json::to_vec(&prior).unwrap()).unwrap();

        // Act
        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        // Assert
        assert_eq!(
            events.len(),
            1,
            "the older-shaped snapshot must produce exactly one drift log, not a corrupt \
             warning: {events:?}"
        );
        assert!(
            !events[0].message.contains("corrupt"),
            "the absent column must not read as corruption: {events:?}"
        );
        assert_eq!(events[0].field("selector"), Some(selector.as_str()));
        assert_eq!(events[0].field("impact_class"), Some("routing-affecting"));
        assert!(
            events[0]
                .field("new")
                .unwrap()
                .contains("\"max_output_tokens\""),
            "the diff must name the newly-baked ceiling: {events:?}"
        );
    }

    #[test]
    fn newer_schema_version_warns_once_skips_diff_and_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":{},"last_seen_catalog_version":1,"in_use_snapshot":{{}}}}"#,
                CATALOG_STATE_SCHEMA_VERSION + 1
            ),
        )
        .unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        assert_eq!(
            events.len(),
            1,
            "too-new schema must warn exactly once: {events:?}"
        );

        let loaded = load(&path).unwrap().expect("state must be rebuilt");
        assert_eq!(loaded.schema_version, CATALOG_STATE_SCHEMA_VERSION);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_state_file_warns_once_skips_diff_and_never_blocks() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_state.json");
        save(&path, &CatalogState::default()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let events = routectl_testkit::capture_events(|| {
            check_drift_and_persist_state(&in_use(), &CatalogOverlay::default(), &path);
        });

        // Restore perms before any assertion can panic and leave a
        // locked-down tempdir behind for the OS to clean up awkwardly.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        // Running as root (some CI/dev containers) bypasses the
        // permission bit entirely, in which case this degrades to the
        // "readable" path -- skip rather than false-fail.
        if events.is_empty() {
            return;
        }
        assert_eq!(
            events.len(),
            1,
            "unreadable file must warn exactly once: {events:?}"
        );
    }

    // -----------------------------------------------------------------------
    // selector_key
    // -----------------------------------------------------------------------

    #[test]
    fn selector_key_joins_provider_kind_and_model_with_a_colon() {
        assert_eq!(
            selector_key("anthropic-api", "claude-opus-4-8"),
            "anthropic-api:claude-opus-4-8"
        );
    }
}
