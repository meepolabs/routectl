//! `routectl config show --effective` -- the provenance-annotated effective
//! view.
//!
//! Plain `config show` dumps `config.toml` verbatim (secrets redacted).
//! `--effective` additionally renders the surfaces where more than one layer
//! competes for a value, tagging each with the layer that won:
//!
//!   - model catalog cells (baked table vs operator overlay),
//!   - retry class policy (baked class default vs `[retry.classes.<class>]`),
//!   - resolved tuning (breaker, RPM and timeout cells, each tagged with the
//!     model, provider, pool, global or default tier that supplied it), and
//!   - capability cells (the config-derived override layer, tagged with the
//!     source that set it).
//!
//! The derivation lives router-side ([`routectl_router::derive_effective_view`])
//! and is pure over `(&Config, &CatalogOverlay)`; this module is render-only.

use routectl_core::Result;
use std::fmt::Display;

use routectl_router::{
    BreakerThreshold, CatalogOverlay, ClassPolicyCell, ClassPolicySource, Config, EffectiveRow,
    ModelCell, OverrideRow, OverrideVerdict, Resolved, Source, TuningRow, TuningTargetKind,
    derive_effective_view,
};

/// Print the plain redacted config dump, then the provenance-annotated
/// effective view (catalog cells + retry class policy + resolved tuning +
/// capability cells).
pub fn show_effective(config: &Config, overlay: &CatalogOverlay) -> Result<()> {
    super::config::show(config)?;

    let view = derive_effective_view(config, overlay);
    println!("effective view -- provenance-annotated (layered surfaces only)");
    render_model_cells(&view.models);
    render_class_cells(&view.classes);
    render_tuning_cells(&view.tuning);
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

/// Render one line per resolved-tuning row: each provider entry, then each
/// model that sets its own first-content timeout (one line per seat for a
/// pooled model).
fn render_tuning_cells(rows: &[TuningRow]) {
    println!("\nresolved tuning (model > provider > pool > global > default):");
    if rows.is_empty() {
        println!("  (no [providers] entries)");
        return;
    }
    for row in rows {
        println!("{}", tuning_line(row));
    }
}

/// Format one tuning row as `key=value(source)` cells. Pure so the line
/// contract is unit-testable without capturing stdout.
fn tuning_line(row: &TuningRow) -> String {
    let t = &row.tuning;
    let kind = match row.kind {
        TuningTargetKind::Provider => "provider",
        TuningTargetKind::Model => "model",
    };
    let failures = Resolved {
        value: match t.circuit_failures.value {
            BreakerThreshold::Disabled => "off".to_string(),
            BreakerThreshold::After(n) => n.to_string(),
        },
        source: t.circuit_failures.source,
    };
    format!(
        "  {kind:<8} {target:<24} {failures} {cooldown} {rpm} {request} {first_byte}",
        target = row.target,
        failures = cell("circuit_failures", &failures),
        cooldown = cell("circuit_cooldown_ms", &t.circuit_cooldown_ms),
        rpm = optional_cell("rpm_limit", &t.rpm_limit),
        request = optional_cell("request_timeout_ms", &t.request_timeout_ms),
        first_byte = optional_cell(
            "stream_first_byte_timeout_ms",
            &t.stream_first_byte_timeout_ms
        ),
    )
}

/// `key=value(source)` for one resolved cell.
fn cell<T: Display>(key: &str, resolved: &Resolved<T>) -> String {
    format!("{key}={}({})", resolved.value, resolved.source.as_str())
}

/// [`cell`] for an optional value; `None` renders `none`.
fn optional_cell<T: Display + Copy>(key: &str, resolved: &Resolved<Option<T>>) -> String {
    let value = resolved
        .value
        .map_or_else(|| "none".to_string(), |v| v.to_string());
    cell(
        key,
        &Resolved {
            value,
            source: resolved.source,
        },
    )
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
        source = CAPABILITY_SOURCE_TAG,
    )
}

/// The grep-friendly tag for a capability cell's source layer. Every
/// config-derived cell comes from `[capability.overrides]`, so this is the
/// SAME `override` token the routing filter emits on its skip log: an
/// operator reading the effective view and an operator reading a route-away
/// log see one vocabulary.
const CAPABILITY_SOURCE_TAG: &str = "override";

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

    /// One config in which every tier supplies at least one cell: a provider
    /// key on `tuned`, a pool key inherited by its untuned member `seat-b`,
    /// an explicit `[retry]` global, a model's own first-content override,
    /// and the shipped defaults for whatever nothing sets.
    fn every_tier_config() -> Config {
        toml::from_str(
            r#"
version = 2

[providers.tuned]
kind = "openai-compat"
base_url = "https://tuned.example/v1"
api_key_ref = "env://K"
circuit_failures = 0
rpm_limit = 30

[providers.seat-b]
kind = "openai-compat"
base_url = "https://seat.example/v1"
api_key_ref = "env://K"

[pools.team]
members = ["seat-b"]
circuit_cooldown_ms = 45000
rpm_limit = 12

[retry]
request_timeout_ms = 900000

[models.slow]
provider = "team"
upstream = "u"
stream_first_byte_timeout_ms = 70000

[models.plain]
provider = "tuned"
upstream = "u"
"#,
        )
        .expect("valid config")
    }

    fn rendered_lines() -> Vec<String> {
        let view = derive_effective_view(&every_tier_config(), &CatalogOverlay::default());
        view.tuning.iter().map(tuning_line).collect()
    }

    #[test]
    fn tuning_lines_are_one_per_provider_then_per_model_override() {
        let lines = rendered_lines();

        let heads: Vec<Vec<&str>> = lines
            .iter()
            .map(|l| l.split_whitespace().take(2).collect())
            .collect();
        assert_eq!(
            heads,
            [
                ["provider", "seat-b"],
                ["provider", "tuned"],
                ["model", "slow#seat-b"],
            ]
        );
    }

    #[test]
    fn tuning_lines_tag_each_cell_with_the_tier_that_supplied_it() {
        let lines = rendered_lines();
        let cells = |i: usize| -> Vec<&str> { lines[i].split_whitespace().skip(2).collect() };

        assert_eq!(
            cells(0),
            [
                "circuit_failures=5(default)",
                "circuit_cooldown_ms=45000(pool)",
                "rpm_limit=12(pool)",
                "request_timeout_ms=900000(global)",
                "stream_first_byte_timeout_ms=300000(default)",
            ],
            "pool member seat-b"
        );
        assert_eq!(
            cells(1),
            [
                "circuit_failures=off(provider)",
                "circuit_cooldown_ms=60000(default)",
                "rpm_limit=30(provider)",
                "request_timeout_ms=900000(global)",
                "stream_first_byte_timeout_ms=300000(default)",
            ],
            "standalone provider tuned"
        );
        assert_eq!(
            cells(2),
            [
                "circuit_failures=5(default)",
                "circuit_cooldown_ms=45000(pool)",
                "rpm_limit=12(pool)",
                "request_timeout_ms=900000(global)",
                "stream_first_byte_timeout_ms=70000(model)",
            ],
            "model slow on pool team, seat seat-b"
        );
    }

    #[test]
    fn an_unlimited_rpm_cell_renders_none() {
        let config: Config = toml::from_str(
            r#"
version = 2

[providers.free]
kind = "openai-compat"
base_url = "https://free.example/v1"
api_key_ref = "env://K"
"#,
        )
        .expect("valid config");
        let view = derive_effective_view(&config, &CatalogOverlay::default());

        let line = tuning_line(&view.tuning[0]);

        assert!(line.contains(" rpm_limit=none(default) "), "line: {line}");
    }
}
