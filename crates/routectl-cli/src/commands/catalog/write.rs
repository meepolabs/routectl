//! Catalog overlay write ops.

use std::collections::BTreeMap;
use std::path::Path;

use routectl_router::{
    AliasPattern, CONFIG_PROVIDER_KINDS, CachePricingOverride, CachePricingSelector,
    CatalogOverlay, OverlayCell, OverlayError, OverlaySource, baked_table_rows,
    catalog_state_selector_key, is_config_provider_kind, load_catalog_overlay,
    overlay_default_path, with_overlay_write_lock,
};

use super::today_verified_at;

/// `routectl catalog export` -- serialize the on-disk overlay to pretty
/// JSON, printed to stdout or written to `--out <path>`. Resolves the
/// default overlay path; see `export_at` for the testable core.
///
/// The exported JSON is exactly `catalog_overlay.json` -- catalog cells
/// only. It does NOT back up credentials: provider keys, OAuth tokens, and
/// every other secret live in separate files this command never reads, so
/// a leaked export can never disclose one. There is no separate
/// overlay-import format to pair with this: restoring an export is placing
/// the JSON back at the overlay path (`catalog_overlay.json`), where the
/// next load picks it up. `catalog import` consumes VENDOR economics
/// snapshots, not this dump.
pub fn export(out: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let json = export_at(&overlay_default_path())?;
    match out {
        Some(path) => {
            std::fs::write(path, format!("{json}\n"))
                .map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
            println!("catalog overlay exported to {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}

/// Core of [`export`], taking the overlay path explicitly so tests can
/// point it at a temp directory instead of the real `catalog_overlay.json`.
///
/// READ-ONLY: loads the overlay and serializes it, never opening the file
/// for writing -- the on-disk overlay is byte-identical afterward. A
/// missing overlay file loads as the empty default (first run), so export
/// still succeeds and emits an empty-cells overlay rather than erroring.
/// The output round-trips: `serde_json::from_str` of it yields an equal
/// [`CatalogOverlay`].
pub(crate) fn export_at(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let overlay = load_catalog_overlay(path)?;
    Ok(serde_json::to_string_pretty(&overlay)?)
}

/// `routectl catalog verify <selector>` -- stamp an existing overlay cell
/// verified today. Resolves the default overlay path; see `verify_at` for
/// the testable core.
pub fn verify(selector_raw: &str) -> Result<(), Box<dyn std::error::Error>> {
    verify_at(selector_raw, &overlay_default_path())
}

/// Core of [`verify`], taking the overlay path explicitly so tests can point
/// it at a temp directory instead of the real `catalog_overlay.json`.
///
/// Verifying is a USER act: an existing cell -- whichever layer wrote it --
/// is rewritten with `source: user` and `verified_at` bumped to today; every
/// other field on the cell is carried through unchanged. A selector with no
/// overlay cell (baked-only, or entirely unknown to both layers) has nothing
/// to stamp: creating a NEW overlay cell is a `set` concern, so this
/// returns a clear error instead of silently pinning the current effective
/// values. A selector whose overlay cell is explicitly `null` (disabled) is
/// likewise nothing to stamp -- verify never resurrects a disabled row.
///
/// The load-modify-save runs through [`with_overlay_write_lock`], so a
/// concurrent verify/import/set against the same overlay file serializes
/// instead of racing; the "nothing to stamp" checks below abort the closure
/// (returning `Err` before it produces a mutated overlay), so a rejected
/// verify never reaches the write.
pub(crate) fn verify_at(selector_raw: &str, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    CachePricingSelector::parse(selector_raw).map_err(|e| format!("invalid selector: {e}"))?;

    let today = today_verified_at();
    let selector = selector_raw.to_string();
    with_overlay_write_lock::<Box<dyn std::error::Error>, _>(path, |overlay| {
        let existing = match overlay.cells.get(&selector) {
            None => {
                return Err(format!(
                    "no overlay cell for selector `{selector}`; nothing to stamp (baked-only or \
                     unknown to the catalog) -- creating a new overlay cell is a `set` concern"
                )
                .into());
            }
            Some(None) => {
                return Err(format!(
                    "selector `{selector}` is disabled in the overlay (null); nothing to stamp"
                )
                .into());
            }
            Some(Some(cell)) => cell.clone(),
        };

        let stamped = OverlayCell {
            source: OverlaySource::User,
            verified_at: today.clone(),
            wm: existing.wm,
            rm: existing.rm,
            ttl_seconds: existing.ttl_seconds,
            min_prefix_tokens: existing.min_prefix_tokens,
            max_context_tokens: existing.max_context_tokens,
            max_output_tokens: existing.max_output_tokens,
            input_cost_per_token: existing.input_cost_per_token,
            output_cost_per_token: existing.output_cost_per_token,
            capabilities: existing.capabilities,
        };
        let mut next = overlay;
        next.cells.insert(selector.clone(), Some(stamped));
        Ok(next)
    })?;

    println!(
        "verified: selector={selector_raw}  date={today}  source=user  written to {}",
        path.display()
    );
    Ok(())
}

/// Errors from `catalog set` / `catalog disable`. Distinguishes an
/// intentional admission/validation abort from a raw [`OverlayError`] so an
/// unknown selector, or a field the overlay cannot carry, never reads as a
/// storage failure.
#[derive(Debug, thiserror::Error)]
pub enum CatalogWriteError {
    #[error("invalid selector `{selector}`: {reason}")]
    InvalidSelector { selector: String, reason: String },

    #[error(
        "selector `{0}` is unknown to the catalog (no baked row, no existing overlay cell); \
         creating a brand-new selector is not supported by `set` / `disable`\n\
         hint: to publish a narrow cell for an upstream the catalog does not cover, run \
         `catalog set --create <provider_kind>:<model> <field>=<value>...`"
    )]
    UnknownSelector(String),

    #[error(
        "selector `{0}` is already known to the catalog (a baked row or an existing overlay \
         cell); `--create` writes only brand-new selectors -- use `set` without `--create` to \
         edit this one"
    )]
    SelectorAlreadyKnown(String),

    #[error("cannot create selector `{selector}`: {reason}")]
    UncreatableSelector { selector: String, reason: String },

    #[error("field `{field}` is not supported by `catalog set`: {reason}")]
    UnsupportedField { field: String, reason: String },

    #[error("invalid field `{raw}`: {reason}")]
    InvalidField { raw: String, reason: String },

    #[error("{0}")]
    Validation(String),

    #[error(transparent)]
    Overlay(#[from] OverlayError),
}

fn parse_selector(selector_raw: &str) -> Result<CachePricingSelector, CatalogWriteError> {
    CachePricingSelector::parse(selector_raw).map_err(|reason| CatalogWriteError::InvalidSelector {
        selector: selector_raw.to_string(),
        reason,
    })
}

/// Admission shape for a selector `--create` is about to introduce. Two
/// independent bounds, both narrower than [`CachePricingSelector::parse`]:
///
/// - The provider kind must be one a `[providers.X]` block can declare
///   ([`is_config_provider_kind`]) -- the vocabulary dispatch resolves
///   against, so a cell under it is reachable. Deliberately NOT the baked
///   catalog's kind set: an overlay lookup keys on the selector's own kind
///   string, so a configurable kind with zero baked rows (today `gemini`)
///   serves its overlay cells normally. The `"*"` cross-kind catch-all is
///   excluded separately -- it is not a narrow cell.
/// - The model segment must be a shape [`routectl_router::lookup_overlay_cell`]
///   can actually serve, minus the bare `"*"` provider catch-all: an exact
///   model, or a trailing-asterisk prefix over a non-empty stem. The
///   catch-all is excluded on purpose -- it applies the value to every
///   sibling model on the kind, which is the failure mode an explicit
///   create path exists to avoid.
fn validate_creatable_selector(
    selector_raw: &str,
    selector: &CachePricingSelector,
) -> Result<(), CatalogWriteError> {
    let uncreatable = |reason: String| CatalogWriteError::UncreatableSelector {
        selector: selector_raw.to_string(),
        reason,
    };

    if selector.provider_kind == "*" {
        return Err(uncreatable(
            "provider_kind `*` spans every provider kind; name one configurable kind (e.g. \
             `openai-compat`)"
                .to_string(),
        ));
    }
    if !is_config_provider_kind(&selector.provider_kind) {
        return Err(uncreatable(format!(
            "provider_kind `{}` is not a kind a `[providers.*]` block can declare, so no \
             configured provider would ever look this cell up; expected one of: {}",
            selector.provider_kind,
            CONFIG_PROVIDER_KINDS.join(", ")
        )));
    }
    if selector.model_glob == "*" {
        return Err(uncreatable(format!(
            "model `*` is the provider catch-all -- it would apply to every model on \
             `{}`; name the model, or a trailing-asterisk prefix of it",
            selector.provider_kind
        )));
    }
    AliasPattern::parse(&selector.model_glob).map_err(|_| {
        uncreatable(format!(
            "model `{}` is not a shape the overlay can match; expected an exact model or a \
             single trailing-asterisk prefix (e.g. `grok-4.5` or `grok-*`)",
            selector.model_glob
        ))
    })?;
    Ok(())
}

/// One parsed `field=value` pair from `catalog set`'s variadic field list.
#[derive(Debug)]
enum FieldUpdate {
    Wm(f32),
    Rm(f32),
    TtlSeconds(u32),
    MinPrefixTokens(u32),
    MaxContextTokens(u32),
    MaxOutputTokens(u32),
    InputCostPerToken(f32),
    OutputCostPerToken(f32),
    /// A `cap:<name>=true|false` capability flag.
    Capability(String, bool),
}

/// Field names `set` deliberately does NOT accept as a plain
/// `field=value`, each with the reason the operator sees. Two kinds live
/// here:
///
/// - Baked-only [`routectl_router::CatalogRow`] fields [`OverlayCell`]
///   structurally cannot carry (settled: the import pipeline cannot produce
///   a differing `auto_cacher`, and the storage-rent fields are
///   reserved-unused on every baked row).
/// - [`OverlayCell`] fields that exist but are not operator-assignable
///   through this verb (provenance is stamped, capabilities have their own
///   `cap:<name>` syntax).
///
/// Naming a field here is what makes its absence from `parse_field`'s match
/// DELIBERATE rather than forgotten -- `every_overlay_cell_field_is_settable_or_deliberately_excluded`
/// fails if an `OverlayCell` field is in neither place, so adding the next
/// overlay field is a one-touch change.
const UNSUPPORTED_FIELDS: &[(&str, &str)] = &[
    (
        "auto_cacher",
        "this field lives only on the baked catalog table; the overlay has no field to carry it",
    ),
    (
        "has_storage_rent",
        "this field lives only on the baked catalog table; the overlay has no field to carry it",
    ),
    (
        "storage_rent",
        "this field lives only on the baked catalog table; the overlay has no field to carry it",
    ),
    (
        "verified_at",
        "verified_at is stamped automatically to today; it cannot be set directly",
    ),
    (
        "source",
        "source is stamped automatically (`set` always writes a user cell); it cannot be set \
         directly",
    ),
    (
        "capabilities",
        "capability priors are set one at a time via `cap:<name>=true|false`, never as a whole map",
    ),
];

/// Parse one `field=value` argument. Capability flags use the
/// `cap:<name>=true|false` syntax (documented in `--help`); every other
/// supported field is a bare name. A field in [`UNSUPPORTED_FIELDS`] is
/// rejected with that entry's own reason so the operator knows WHY rather
/// than getting a generic "unknown field".
fn parse_field(raw: &str) -> Result<FieldUpdate, CatalogWriteError> {
    let (field, value) = raw
        .split_once('=')
        .ok_or_else(|| CatalogWriteError::InvalidField {
            raw: raw.to_string(),
            reason: "expected `field=value`".to_string(),
        })?;

    if let Some(name) = field.strip_prefix("cap:") {
        if name.is_empty() {
            return Err(CatalogWriteError::InvalidField {
                raw: raw.to_string(),
                reason: "capability name must not be empty".to_string(),
            });
        }
        let flag = parse_bool_value(value).ok_or_else(|| CatalogWriteError::InvalidField {
            raw: raw.to_string(),
            reason: "capability value must be `true` or `false`".to_string(),
        })?;
        return Ok(FieldUpdate::Capability(name.to_string(), flag));
    }

    if let Some((_, reason)) = UNSUPPORTED_FIELDS.iter().find(|(name, _)| *name == field) {
        return Err(CatalogWriteError::UnsupportedField {
            field: field.to_string(),
            reason: (*reason).to_string(),
        });
    }

    match field {
        "wm" => parse_num(raw, value).map(FieldUpdate::Wm),
        "rm" => parse_num(raw, value).map(FieldUpdate::Rm),
        "ttl_seconds" => parse_num(raw, value).map(FieldUpdate::TtlSeconds),
        "min_prefix_tokens" => parse_num(raw, value).map(FieldUpdate::MinPrefixTokens),
        "max_context_tokens" => parse_num(raw, value).map(FieldUpdate::MaxContextTokens),
        "max_output_tokens" => parse_num(raw, value).map(FieldUpdate::MaxOutputTokens),
        "input_cost_per_token" => parse_num(raw, value).map(FieldUpdate::InputCostPerToken),
        "output_cost_per_token" => parse_num(raw, value).map(FieldUpdate::OutputCostPerToken),
        other => Err(CatalogWriteError::InvalidField {
            raw: raw.to_string(),
            reason: format!(
                "unknown field `{other}`; supported fields are wm, rm, ttl_seconds, \
                 min_prefix_tokens, max_context_tokens, max_output_tokens, \
                 input_cost_per_token, output_cost_per_token, cap:<name>"
            ),
        }),
    }
}

fn parse_bool_value(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_num<T: std::str::FromStr>(raw: &str, value: &str) -> Result<T, CatalogWriteError> {
    value.parse().map_err(|_| CatalogWriteError::InvalidField {
        raw: raw.to_string(),
        reason: format!("`{value}` is not a valid number"),
    })
}

fn apply_field_update(cell: &mut OverlayCell, update: FieldUpdate) {
    match update {
        FieldUpdate::Wm(v) => cell.wm = Some(v),
        FieldUpdate::Rm(v) => cell.rm = Some(v),
        FieldUpdate::TtlSeconds(v) => cell.ttl_seconds = Some(v),
        FieldUpdate::MinPrefixTokens(v) => cell.min_prefix_tokens = Some(v),
        FieldUpdate::MaxContextTokens(v) => cell.max_context_tokens = Some(v),
        FieldUpdate::MaxOutputTokens(v) => cell.max_output_tokens = Some(v),
        FieldUpdate::InputCostPerToken(v) => cell.input_cost_per_token = Some(v),
        FieldUpdate::OutputCostPerToken(v) => cell.output_cost_per_token = Some(v),
        FieldUpdate::Capability(name, flag) => {
            cell.capabilities
                .get_or_insert_with(BTreeMap::new)
                .insert(name, flag);
        }
    }
}

/// True when `selector` is known to the catalog: either an EXACT
/// baked-table key (`"provider_kind:model_glob"`) or an existing overlay
/// cell (present or disabled). This is the synthetic-row poisoning guard:
/// `merge`'s own `Some(cell)` + no baked row still yields `Present` (over
/// the sentinel), so an unbounded selector would otherwise be silently
/// admitted the moment `set`/`disable` write it.
fn selector_known(selector: &str, overlay: &CatalogOverlay) -> bool {
    if overlay.cells.contains_key(selector) {
        return true;
    }
    baked_table_rows()
        .into_iter()
        .any(|row| catalog_state_selector_key(row.provider_kind, row.model_glob) == selector)
}

/// Reuse [`CachePricingOverride::validate`]'s degeneracy contract (`rm >
/// 0`, `max_context_tokens` / `max_output_tokens` != 0, below-sentinel `wm`
/// needs the ack flag)
/// against ONLY the fields THIS call is setting -- never against a field
/// inherited unchanged from a prior cell. A baked/import cell can
/// legitimately carry a `wm` below the sentinel already (auto-cachers
/// commonly do); re-validating the whole merged cell on every edit would
/// force an unrelated `set rm=...` to also re-acknowledge a `wm` it never
/// touched.
fn validate_updates(
    updates: &[FieldUpdate],
    acknowledge_cost_risk: bool,
) -> Result<(), CatalogWriteError> {
    let mut ov = CachePricingOverride {
        override_acknowledges_cost_risk: acknowledge_cost_risk,
        ..Default::default()
    };
    for update in updates {
        match update {
            FieldUpdate::Wm(v) => ov.wm = Some(*v),
            FieldUpdate::Rm(v) => ov.rm = Some(*v),
            FieldUpdate::MaxContextTokens(v) => ov.max_context_tokens = Some(*v),
            FieldUpdate::MaxOutputTokens(v) => ov.max_output_tokens = Some(*v),
            FieldUpdate::InputCostPerToken(v) => ov.input_cost_per_token = Some(*v),
            FieldUpdate::OutputCostPerToken(v) => ov.output_cost_per_token = Some(*v),
            FieldUpdate::TtlSeconds(_)
            | FieldUpdate::MinPrefixTokens(_)
            | FieldUpdate::Capability(..) => {}
        }
    }
    ov.validate().map_err(CatalogWriteError::Validation)
}

/// The same serve-pickup note `catalog import` prints after a write: a
/// running `routectl serve` re-reads the overlay via its file watch.
/// `pub(crate)` so `commands::catalog_import`'s own summary can print the
/// identical line without a second copy of the string.
pub(crate) fn print_pickup_note() {
    println!(
        "note: a running `routectl serve` picks up this change automatically via the overlay \
         watch."
    );
}

/// `routectl catalog set <selector> <field>=<value>...` -- resolves the
/// default overlay path; see `set_at` for the testable core.
pub fn set(
    selector_raw: &str,
    fields: &[String],
    acknowledge_cost_risk: bool,
    create: bool,
) -> Result<(), CatalogWriteError> {
    set_at(
        selector_raw,
        fields,
        acknowledge_cost_risk,
        create,
        &overlay_default_path(),
    )
}

/// Core of [`set`], taking the overlay path explicitly so tests can point
/// it at a temp directory instead of the real `catalog_overlay.json`.
///
/// Selector SYNTAX and field PARSING are checked up front -- pure, no I/O.
/// ADMISSION -- is the selector actually known to the catalog (see
/// [`selector_known`]) -- and value VALIDATION (see [`validate_updates`])
/// both run INSIDE the write lock, admission first: a selector typo'd
/// alongside a bad value reads as "unknown selector", not a value error,
/// and neither check needs the loaded overlay for validation, but keeping
/// both under the same lock hold keeps the ordering simple.
///
/// `create` INVERTS the admission rule instead of relaxing it: with the
/// flag, a selector already known to either layer is REJECTED
/// ([`CatalogWriteError::SelectorAlreadyKnown`]) and a brand-new one is
/// admitted, subject to [`validate_creatable_selector`]'s narrower shape
/// bound. The inversion is what makes a typo'd variant of an existing
/// selector a hard error rather than a silent second cell competing with
/// the real one. Everything downstream of admission -- field parsing,
/// value validation, provenance stamping -- is the same code either way.
///
/// `set` on a selector that already carries a present overlay cell (either
/// provenance) starts from that cell's own fields, so an unset field in
/// `fields` still inherits whatever the prior cell -- import or user --
/// already had; naming a field overwrites it. `set` on a baked-only
/// selector, on a currently-DISABLED one, or on a `--create` selector,
/// starts from an all-`None` sparse cell instead: a disabled cell carries
/// no fields to inherit, and re-enabling via `set` is exactly that -- a
/// fresh cell. Every call always stamps `source: user` and `verified_at`
/// to today (UTC) -- editing a cell is itself a ratification, even of one
/// an import last wrote.
pub(crate) fn set_at(
    selector_raw: &str,
    fields: &[String],
    acknowledge_cost_risk: bool,
    create: bool,
    path: &Path,
) -> Result<(), CatalogWriteError> {
    let parsed = parse_selector(selector_raw)?;
    if create {
        validate_creatable_selector(selector_raw, &parsed)?;
    }
    let updates: Vec<FieldUpdate> = fields
        .iter()
        .map(|f| parse_field(f))
        .collect::<Result<_, _>>()?;

    let today = today_verified_at();
    let selector = selector_raw.to_string();

    with_overlay_write_lock::<CatalogWriteError, _>(path, |overlay| {
        match (create, selector_known(&selector, &overlay)) {
            (false, false) => return Err(CatalogWriteError::UnknownSelector(selector.clone())),
            (true, true) => {
                return Err(CatalogWriteError::SelectorAlreadyKnown(selector.clone()));
            }
            _ => {}
        }
        validate_updates(&updates, acknowledge_cost_risk)?;

        let mut cell = match overlay.cells.get(&selector) {
            Some(Some(existing)) => existing.clone(),
            _ => OverlayCell {
                source: OverlaySource::User,
                verified_at: today.clone(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            },
        };
        cell.source = OverlaySource::User;
        cell.verified_at = today.clone();
        for update in updates {
            apply_field_update(&mut cell, update);
        }

        let mut next = overlay;
        next.cells.insert(selector.clone(), Some(cell));
        Ok(next)
    })?;

    let verb = if create { "created" } else { "set" };
    println!(
        "{verb}: selector={selector_raw}  source=user  verified_at={today}  written to {}",
        path.display()
    );
    print_pickup_note();
    Ok(())
}

/// `routectl catalog disable <selector>` -- resolves the default overlay
/// path; see `disable_at` for the testable core.
pub fn disable(selector_raw: &str) -> Result<(), CatalogWriteError> {
    disable_at(selector_raw, &overlay_default_path())
}

/// Core of [`disable`]. Same admission rule as [`set_at`] (a brand-new
/// selector is rejected). A disable always writes JSON `null` regardless
/// of what the selector previously carried -- there is nothing to
/// preserve; disabling discards the cell's own field values by design, and
/// re-enabling is a fresh `set`.
pub(crate) fn disable_at(selector_raw: &str, path: &Path) -> Result<(), CatalogWriteError> {
    parse_selector(selector_raw)?;
    let selector = selector_raw.to_string();

    with_overlay_write_lock::<CatalogWriteError, _>(path, |overlay| {
        if !selector_known(&selector, &overlay) {
            return Err(CatalogWriteError::UnknownSelector(selector.clone()));
        }
        let mut next = overlay;
        next.cells.insert(selector.clone(), None);
        Ok(next)
    })?;

    println!(
        "disabled: selector={selector_raw}  written to {}",
        path.display()
    );
    print_pickup_note();
    Ok(())
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
