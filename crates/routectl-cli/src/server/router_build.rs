//! Router construction from a parsed config + catalog overlay: the
//! validation gauntlet run on every build and the empty-overlay test
//! wrapper. Split out of `serve` so the reload coordinator depends on
//! this builder directly, keeping the serve <-> reload edge one-way.

use std::sync::Arc;

use routectl_auth::SecretStore;
use routectl_core::{Error, Result, sanitize_for_log_with_cap};
use routectl_router::{CatalogOverlay, Config, Router};

use crate::commands::config::MAX_REPORTED_LINE_CHARS;

/// Log-safe rendering of an advisory validator warning.
///
/// Every advisory validator interpolates operator-written table keys into
/// its message, and a `%`-rendered tracing field reaches the log line
/// verbatim -- so a `[models.X]` nickname bearing a newline plus an ANSI
/// sequence would forge a startup log record. Same filter and same ceiling
/// the `config check` and doctor renders of these very messages apply, so
/// all three surfaces bound the line identically.
fn sanitize_warning_for_log(warning: &str) -> String {
    sanitize_for_log_with_cap(warning, MAX_REPORTED_LINE_CHARS)
}

/// Build a `Router` from the parsed config + a shared
/// `Arc<dyn SecretStore>`, with an EMPTY catalog overlay. Thin wrapper kept
/// for the many call sites (this crate's own unit tests plus
/// `handlers::ingress_handle_tests`) that build a Router without caring
/// about the overlay -- an empty overlay is behaviorally identical to
/// "every target falls through to the baked catalog", which is what those
/// callers already exercised before the overlay existed. Callers that DO
/// care (the server's boot + reload paths) use
/// [`build_router_from_config_with_overlay`] instead.
#[cfg(test)]
pub async fn build_router_from_config(
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
) -> Result<Router> {
    build_router_from_config_with_overlay(config, &Arc::default(), secrets).await
}

/// Build a `Router` from the parsed config + catalog overlay + a shared
/// `Arc<dyn SecretStore>`. The store is hoisted out of this function
/// (the caller passes it in) so a hot-reload config change that
/// triggers a Router rebuild reuses the SAME store handle, preserving
/// the OAuthStore in-memory token cache and the per-provider
/// single-flight refresh mutex across rebuilds.
///
/// The overlay arrives as an `Arc` because the built Router RETAINS it (see
/// `Router::install_catalog_overlay`): every caller already holds one, so
/// retention costs a refcount rather than a clone of the overlay map.
pub async fn build_router_from_config_with_overlay(
    config: Arc<Config>,
    catalog_overlay: &Arc<CatalogOverlay>,
    secrets: Arc<dyn SecretStore>,
) -> Result<Router> {
    // The whole shared validation suite, in its one deterministic order,
    // BEFORE any construction. Running the suite rather than a hand-picked
    // subset is what keeps this builder's accept set identical to the one
    // `config check` and the `serve` pre-parse gate report: a programmatic
    // caller that reaches this function directly (tests, a library embedder)
    // otherwise builds a Router whose invalid config sits inert.
    if let Some(first) = routectl_router::collect_config_validation(&config)
        .errors
        .into_iter()
        .next()
    {
        return Err(Error::Config(first));
    }

    // The advisory findings, emitted per category so each startup log line
    // names the surface an operator has to go look at.
    for warning in routectl_router::class_policy_warnings(&config) {
        tracing::warn!(warning = %sanitize_warning_for_log(&warning), "class policy warning");
    }
    for warning in routectl_router::codex_identity_warnings(&config) {
        tracing::warn!(warning = %sanitize_warning_for_log(&warning), "codex identity warning");
    }
    // `auto_emit_per_block_breakpoints` is inert on Bedrock Invoke (the
    // knob gates the Converse cachePoint surface).
    for warning in routectl_router::per_block_breakpoint_warnings(&config) {
        tracing::warn!(warning = %sanitize_warning_for_log(&warning), "per-block breakpoint warning");
    }
    // A cloud-code Gemini entry pinned to the production Cloud Code host
    // keeps that pin, but the lane default is the daily host.
    for warning in routectl_router::cloudcode_host_warnings(&config) {
        tracing::warn!(warning = %sanitize_warning_for_log(&warning), "cloud-code host warning");
    }
    // A cloud-code Gemini model entry pinning an upstream id Google has
    // deprecated server-side still serves, but not what the id names.
    for warning in routectl_router::cloudcode_model_warnings(&config) {
        tracing::warn!(warning = %sanitize_warning_for_log(&warning), "cloud-code model warning");
    }

    // Reject an incoherent `[mitm]` block (bad upstream_origin, a
    // listen_port colliding with [server] port, an empty mitm_host) at
    // startup. NOT part of the shared suite -- it is specific to this
    // router-build path. A no-op (`Ok(())`) when `[mitm]` is absent --
    // gated here on `mitm.is_some()` purely for readability at the call
    // site, since the validator itself already treats absence as
    // trivially valid.
    if config.mitm.is_some() {
        routectl_router::validate_mitm_config(&config)?;
    }

    // Advisory: warn (never fail) if the WHOLE baked catalog table's
    // snapshot has gone stale (> 90 days). A redesign dropped the per-row
    // `verified_at`, so this is now a single table-wide check rather than
    // per-cell (see `routectl_router::catalog::warn_if_stale`'s doc).
    routectl_router::catalog::warn_if_stale();

    let mut router = Router::new(config.clone());

    let opts = routectl_router::BuildOptions::new()
        .with_strict_translation(config.server.strict_translation)
        .with_normalize_tools(config.cache.normalize_tools)
        .with_bedrock_allowed_betas(config.bedrock.allowed_betas.clone())
        .with_bedrock_allowed_body_fields(config.bedrock.allowed_body_fields.clone());

    // v0.6.0: walk `[models]` once, building one provider per unique
    // non-Bedrock provider entry (cached) and one provider per Bedrock
    // model. Failures are collected and only fatal when an `[aliases]`
    // chain references a model whose provider failed to build.
    let built = routectl_router::build_resolved_models_reported(&config, secrets, opts).await?;
    let (resolved_models, failed) = (built.models, built.failed);
    // Retain what the build observed about each pool BEFORE the resolved table
    // is installed: a degraded pool is invisible to config alone, so the read
    // side has to be handed the build's own observation.
    router.install_pool_reports(built.pool_reports);
    // Stamp each resolved model's precomputed two-layer catalog merge
    // (baked table + this boot/reload's overlay) onto the table BEFORE
    // installing it, so `Router::record_would_trim` reads a resolved
    // `EffectiveRow` straight off the dispatch target instead of
    // re-resolving the merge per request.
    let resolved_models =
        routectl_router::apply_catalog_overlay(resolved_models, &config, catalog_overlay);
    router.install_resolved_models(resolved_models);
    // Retain the overlay the resolved-model table was merged against, which
    // also stamps its revision so a later hot-reload can detect an overlay
    // change and invalidate the learned-capability registry. Retaining it
    // (rather than leaving each reader to re-read the file) is what lets the
    // status read side report the ACCEPTED generation.
    router.install_catalog_overlay(catalog_overlay.clone());

    // Provider build failures are normally non-fatal (an operator
    // may have an unused-but-declared model whose provider creds
    // aren't set in the current environment). But a failed model
    // that an `[aliases.*]` entry actually references is a real
    // misconfiguration -- without this guard, the server starts
    // "healthy" and the first real request hits `Error::UnknownAlias`
    // at dispatch time, with no configuration-error breadcrumb to
    // follow. Fail loudly here so operators see the issue at startup,
    // not at first traffic.
    if !failed.is_empty() {
        let failed_models: std::collections::HashSet<&str> =
            failed.iter().map(|(n, _)| n.as_str()).collect();
        let mut blocking: Vec<String> = Vec::new();
        for (alias, entry) in &config.aliases {
            for nick in entry.nicknames() {
                if failed_models.contains(nick) {
                    blocking.push(format!("alias `{alias}` -> model `{nick}`"));
                }
            }
        }
        if !blocking.is_empty() {
            let detail = failed
                .iter()
                .map(|(n, e)| format!("  - {n}: {e}"))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(Error::Config(format!(
                "{} model(s) failed to build AND are referenced by routes:\n{}\n\
                 affected routes:\n  {}",
                failed.len(),
                detail,
                blocking.join("\n  "),
            )));
        }
    }

    Ok(router)
}

#[cfg(test)]
#[path = "router_build_tests.rs"]
mod router_build_tests;
