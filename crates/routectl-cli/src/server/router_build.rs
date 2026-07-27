//! Router construction from a parsed config + catalog overlay: the
//! validation gauntlet run on every build and the empty-overlay test
//! wrapper. Split out of `serve` so the reload coordinator depends on
//! this builder directly, keeping the serve <-> reload edge one-way.

use std::sync::Arc;

use routectl_auth::SecretStore;
use routectl_core::{Error, Result};
use routectl_router::{CatalogOverlay, Config, Router};

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
    build_router_from_config_with_overlay(config, &CatalogOverlay::default(), secrets).await
}

/// Build a `Router` from the parsed config + catalog overlay + a shared
/// `Arc<dyn SecretStore>`. The store is hoisted out of this function
/// (the caller passes it in) so a hot-reload config change that
/// triggers a Router rebuild reuses the SAME store handle, preserving
/// the OAuthStore in-memory token cache and the per-provider
/// single-flight refresh mutex across rebuilds.
pub async fn build_router_from_config_with_overlay(
    config: Arc<Config>,
    catalog_overlay: &CatalogOverlay,
    secrets: Arc<dyn SecretStore>,
) -> Result<Router> {
    let mut router = Router::new(config.clone());

    // Surface incoherent `[bedrock]` config (e.g. populated
    // `allowed_body_fields` missing routectl-mandatory keys) at
    // startup instead of at first-request 400. Empty lists are
    // pass-through and accepted; see `validate_bedrock_global_config`.
    routectl_router::validate_bedrock_global_config(&config)?;

    // Reject empty-string `thinking = ""` on any provider before
    // building, so the operator gets a clean error rather than
    // silently emitting `effort: ""` on every routed request.
    routectl_router::validate_reasoning_defaults(&config)?;

    // Reject `[aliases]` chains that reference unknown OR disabled
    // `[models.X]` nicknames. Without this, dispatching against a
    // typo'd alias chain returns `UnknownAlias` at request time with
    // no breadcrumb back to the misconfiguration; failing here gives
    // the operator the offending alias + nickname pair upfront.
    routectl_router::validate_alias_chain_targets(&config)?;

    // Reject malformed `[aliases]` glob keys (embedded/bare asterisks)
    // at startup. Without this, `Router::new` warn-and-drops the
    // malformed key and the request mis-routes while `config check`
    // still reports ok.
    routectl_router::validate_alias_patterns(&config)?;

    // Reject the reserved `[retry.classes.feature-unsupported]` key and
    // any `[providers.X.class_overrides]` remap targeting a class the
    // router retries or debits for health. Advisory findings on the
    // same surface (a health-status source remapped away from breaker
    // accounting, an empty `ClassPolicy` block) are logged rather than
    // rejected.
    routectl_router::validate_class_policy(&config)?;
    for warning in routectl_router::class_policy_warnings(&config) {
        tracing::warn!(warning = %warning, "class policy warning");
    }

    // Reject malformed `[registry]` glob keys at startup so query-time
    // cost resolution never silently skips a key it cannot parse.
    routectl_router::validate_registry_patterns(&config)?;

    // Reject an incoherent `[mitm]` block (bad upstream_origin, a
    // listen_port colliding with [server] port, an empty mitm_host) at
    // startup. A no-op (`Ok(())`) when `[mitm]` is absent -- gated here
    // on `mitm.is_some()` purely for readability at the call site, since
    // the validator itself already treats absence as trivially valid.
    if config.mitm.is_some() {
        routectl_router::validate_mitm_config(&config)?;
    }

    // Provider-level credential_source coherence (forwarded => host pin +
    // empty api_key_ref; own => key present). Also runs in the cheap
    // pre-parse gate (`validate_effective_config`); repeated here because
    // this builder is also reachable without that gate (tests, callers
    // constructing a Config directly), and containment point (1) of the
    // forwarded-credential invariant must hold on every build path.
    routectl_router::validate_provider_credential_sources(&config)?;

    // Reject a degenerate `[cache_pricing]` override (unparseable selector
    // key or a multiplier that makes the break-even math degenerate) at
    // startup. Without this, a bad override silently goes inert at lookup
    // time and the operator never learns their correction did nothing;
    // failing here names the offending selector upfront.
    routectl_router::validate_overrides(&config.cache_pricing).map_err(Error::Config)?;

    // Advisory: warn (never fail) if the WHOLE baked catalog table's
    // snapshot has gone stale (> 90 days). A redesign dropped the per-row
    // `verified_at`, so this is now a single table-wide check rather than
    // per-cell (see `routectl_router::catalog::warn_if_stale`'s doc).
    routectl_router::catalog::warn_if_stale();

    let opts = routectl_router::BuildOptions::new()
        .with_strict_translation(config.server.strict_translation)
        .with_normalize_tools(config.cache.normalize_tools)
        .with_bedrock_allowed_betas(config.bedrock.allowed_betas.clone())
        .with_bedrock_allowed_body_fields(config.bedrock.allowed_body_fields.clone());

    // v0.6.0: walk `[models]` once, building one provider per unique
    // non-Bedrock provider entry (cached) and one provider per Bedrock
    // model. Failures are collected and only fatal when an `[aliases]`
    // chain references a model whose provider failed to build.
    let (resolved_models, failed) =
        routectl_router::build_resolved_models(&config, secrets, opts).await?;
    // Stamp each resolved model's precomputed two-layer catalog merge
    // (baked table + this boot/reload's overlay) onto the table BEFORE
    // installing it, so `Router::record_would_trim` reads a resolved
    // `EffectiveRow` straight off the dispatch target instead of
    // re-resolving the merge per request.
    let resolved_models =
        routectl_router::apply_catalog_overlay(resolved_models, &config, catalog_overlay);
    router.install_resolved_models(resolved_models);
    // Stamp the overlay revision the resolved-model table was merged
    // against so a later hot-reload can detect an overlay change and
    // invalidate the learned-capability registry.
    router.note_overlay_revision(routectl_router::overlay_revision(catalog_overlay));

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
