//! `routectl config show --effective` -- the provenance-annotated effective
//! view.
//!
//! Plain `config show` dumps `config.toml` verbatim (secrets redacted).
//! `--effective` additionally renders the two surfaces where more than one
//! layer competes for a value, tagging each with the layer that won:
//!
//!   - model catalog cells (baked table vs operator overlay), and
//!   - retry class policy (baked class default vs `[retry.classes.<class>]`).
//!
//! The derivation lives router-side ([`routectl_router::derive_effective_view`])
//! and is pure over `(&Config, &CatalogOverlay)`; this module is render-only.

use routectl_core::Result;
use routectl_router::{
    CatalogOverlay, ClassPolicyCell, ClassPolicySource, Config, EffectiveRow, ModelCell, Source,
    derive_effective_view,
};

/// Print the plain redacted config dump, then the provenance-annotated
/// effective view (catalog cells + retry class policy).
pub fn show_effective(config: &Config, overlay: &CatalogOverlay) -> Result<()> {
    super::config::show(config)?;

    let view = derive_effective_view(config, overlay);
    println!("effective view -- provenance-annotated (layered surfaces only)");
    render_model_cells(&view.models);
    render_class_cells(&view.classes);
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
