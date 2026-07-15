//! `routectl config show --effective` -- the provenance-annotated effective
//! view.
//!
//! Plain `config show` dumps `config.toml` verbatim (secrets redacted).
//! `--effective` additionally renders the surfaces where more than one layer
//! competes for a value, tagging each with the layer that won:
//!
//!   - model catalog cells (baked table vs operator overlay),
//!   - retry class policy (baked class default vs `[retry.classes.<class>]`), and
//!   - capability cells (the config-derived override layer, tagged with the
//!     source that set it).
//!
//! The derivation lives router-side ([`routectl_router::derive_effective_view`])
//! and is pure over `(&Config, &CatalogOverlay)`; this module is render-only.

use routectl_core::Result;
use routectl_router::{
    CatalogOverlay, ClassPolicyCell, ClassPolicySource, Config, EffectiveRow, ModelCell,
    OverrideProvenance, OverrideRow, OverrideVerdict, Source, derive_effective_view,
};

/// Print the plain redacted config dump, then the provenance-annotated
/// effective view (catalog cells + retry class policy + capability cells).
pub fn show_effective(config: &Config, overlay: &CatalogOverlay) -> Result<()> {
    super::config::show(config)?;

    let view = derive_effective_view(config, overlay);
    println!("effective view -- provenance-annotated (layered surfaces only)");
    render_model_cells(&view.models);
    render_class_cells(&view.classes);
    render_capability_cells(&view.capabilities);
    Ok(())
}

/// Render one line per `[models.X]` entry: its `(provider_kind, upstream)`
/// selector, the winning layer, and the merged economics.
fn render_model_cells(cells: &[ModelCell]) {
    println!("\nmodel catalog cells (baked table + overlay merge):");
    if cells.is_empty() {
        println!("  (no [models] entries)");
        return;
    }
    for cell in cells {
        let selector = format!("{}/{}", cell.provider_kind, cell.upstream);
        match &cell.row {
            EffectiveRow::Present {
                row,
                source,
                verified_at,
            } => {
                let ctx = row
                    .max_context_tokens
                    .map_or_else(|| "?".to_string(), |c| c.to_string());
                println!(
                    "  {nickname:<20} {selector:<44} source={source:<13} \
                     verified={verified_at:<11} wm={wm} rm={rm} ctx={ctx}",
                    nickname = cell.nickname,
                    source = source_tag(*source),
                    wm = row.wm,
                    rm = row.rm,
                );
            }
            EffectiveRow::Disabled => {
                println!(
                    "  {nickname:<20} {selector:<44} source=disabled",
                    nickname = cell.nickname,
                );
            }
            EffectiveRow::Missing => {
                println!(
                    "  {nickname:<20} {selector:<44} source=missing (no catalog row)",
                    nickname = cell.nickname,
                );
            }
        }
    }
}

/// Render one line per failure class: the resolved retry/fallback pair and the
/// layer that supplied it.
fn render_class_cells(cells: &[ClassPolicyCell]) {
    println!("\nretry class policy (baked defaults + [retry.classes] overrides):");
    for cell in cells {
        println!(
            "  {class:<19} retry={retry:<3} fallback={fallback:<5} source={source}",
            class = class_name(cell.class),
            retry = cell.retry_cap,
            fallback = cell.fallback,
            source = class_source_tag(cell.source),
        );
    }
}

/// Render one line per config-derived capability-override cell: its target
/// spec, capability key, verdict, and the source layer that set it. Learned
/// negatives are runtime state and never appear in this config-derived view,
/// so an empty layer renders empty rather than erroring.
fn render_capability_cells(cells: &[OverrideRow]) {
    println!("\ncapability cells (config-derived override layer):");
    if cells.is_empty() {
        println!("  (no capability overrides)");
        return;
    }
    for cell in cells {
        println!("{}", capability_line(cell));
    }
}

/// Format one capability cell's render line. Pure so the line contract --
/// the target/capability columns and the source tag -- is unit-testable
/// without capturing stdout.
fn capability_line(cell: &OverrideRow) -> String {
    format!(
        "  {target:<24} {capability:<24} verdict={verdict:<15} source={source}",
        target = cell.target_spec,
        capability = cell.capability_key,
        verdict = verdict_tag(cell.verdict),
        source = capability_source_tag(cell.provenance),
    )
}

/// The grep-friendly tag for a capability cell's winning source layer. Uses
/// the SAME tokens the routing filter emits on its skip log (`provider` /
/// `model` / `override`), so an operator reading the effective view and an
/// operator reading a route-away log see one vocabulary.
const fn capability_source_tag(provenance: OverrideProvenance) -> &'static str {
    match provenance {
        OverrideProvenance::ProviderStatic => "provider",
        OverrideProvenance::ModelStatic => "model",
        OverrideProvenance::Override => "override",
    }
}

/// The grep-friendly tag for a capability cell's verdict.
const fn verdict_tag(verdict: OverrideVerdict) -> &'static str {
    match verdict {
        OverrideVerdict::RouteAway => "route-away",
        OverrideVerdict::ForceSupported => "force-supported",
    }
}

/// The grep-friendly tag for a catalog cell's winning layer.
const fn source_tag(source: Source) -> &'static str {
    match source {
        Source::Baked => "baked",
        Source::Import => "import",
        Source::User => "user",
    }
}

/// The grep-friendly tag for a class policy's winning layer.
const fn class_source_tag(source: ClassPolicySource) -> &'static str {
    match source {
        ClassPolicySource::Config => "config",
        ClassPolicySource::BakedDefault => "baked-default",
    }
}

/// The kebab-case config token for a failure class (matches the
/// `[retry.classes.<class>]` key an operator writes).
const fn class_name(class: routectl_router::class_policy::ConfigFailureClass) -> &'static str {
    use routectl_router::class_policy::ConfigFailureClass as C;
    match class {
        C::RateLimited => "rate-limited",
        C::Auth => "auth",
        C::BadRequest => "bad-request",
        C::ContentPolicy => "content-policy",
        C::ContextWindow => "context-window",
        C::ServerError => "server-error",
        C::Timeout => "timeout",
        C::NetworkError => "network-error",
        C::Overloaded => "overloaded",
        C::FeatureUnsupported => "feature-unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_line_carries_source_tag_for_seeded_override_cell() {
        // Arrange: a seeded config-derived override cell (route-away via a
        // `[capability.overrides.<spec>]` entry).
        let cell = OverrideRow {
            target_spec: "anthropic".to_string(),
            capability_key: "web_search".to_string(),
            verdict: OverrideVerdict::RouteAway,
            provenance: OverrideProvenance::Override,
        };

        // Act
        let line = capability_line(&cell);

        // Assert: the line names the cell and tags the winning source with
        // the routing filter's `override` token.
        assert!(line.contains("anthropic"), "line: {line}");
        assert!(line.contains("web_search"), "line: {line}");
        assert!(line.contains("verdict=route-away"), "line: {line}");
        assert!(line.contains("source=override"), "line: {line}");
    }

    #[test]
    fn capability_source_tags_match_the_routing_filter_contract() {
        // The effective-view source tokens are the SAME strings the routing
        // filter emits on its skip log.
        assert_eq!(
            capability_source_tag(OverrideProvenance::ProviderStatic),
            "provider"
        );
        assert_eq!(
            capability_source_tag(OverrideProvenance::ModelStatic),
            "model"
        );
        assert_eq!(
            capability_source_tag(OverrideProvenance::Override),
            "override"
        );
    }
}
