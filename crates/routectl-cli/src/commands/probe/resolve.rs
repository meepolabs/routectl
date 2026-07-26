//! Shared config resolution for the CLI probe surfaces.
//!
//! A probe is scoped to exactly one target, named either by an `--alias`
//! (a `[models]` nickname, which resolves both the provider and the upstream
//! model id) or by a bare `--provider` (which resolves the upstream id from
//! the single selectable model referencing it). Both the envelope-capture
//! harness and the capability probe scope this way, so the resolution lives
//! here once.
//!
//! Two views over the same resolution:
//!   - [`resolve_provider_and_model`] returns just `(provider, model_id)` --
//!     what a bare dispatch needs.
//!   - [`resolve_probe_target`] additionally carries the routing `state_key`
//!     (the nickname the learned-capability ledger keys on), which a
//!     capability probe must emit so its events land on the SAME lane a live
//!     request through that model would.

use routectl_router::Config;

/// A resolved probe target: the routing state key plus the provider and
/// upstream model id the dispatch needs.
///
/// `state_key` is the `[models]` nickname -- the exact key the router's
/// learned-capability registry and the usage ledger record events under for
/// a non-pooled model (see `into_one_dispatch_target`), so a probe emitting
/// on this key settles the same lane live traffic would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProbeTarget {
    /// Routing state key (the `[models]` nickname) -- the ledger lane key.
    pub state_key: String,
    /// Provider name (a key in the `[providers]` table).
    pub provider: String,
    /// Upstream model id forwarded to the provider verbatim.
    pub model_id: String,
}

/// Resolve a scoped target to its `(state_key, provider, model_id)`. An
/// alias names all three (its own key is the state key); a bare provider
/// resolves them from the single selectable model referencing it, and errors
/// when zero or more than one model qualifies.
pub fn resolve_probe_target(
    config: &Config,
    provider: Option<&str>,
    alias: Option<&str>,
) -> Result<ResolvedProbeTarget, String> {
    if let Some(alias) = alias {
        let model = config
            .models
            .get(alias)
            .ok_or_else(|| format!("no model named `{alias}` is configured"))?;
        return Ok(ResolvedProbeTarget {
            state_key: alias.to_string(),
            provider: model.provider.clone(),
            model_id: model.upstream.clone(),
        });
    }
    let provider = provider
        .ok_or_else(|| "an explicit --provider or --alias target is required".to_string())?;
    let mut matches = config
        .models
        .iter()
        .filter(|(_, m)| m.selectable && m.provider == provider);
    let first = matches.next().ok_or_else(|| {
        format!("provider `{provider}` has no selectable model; pass --alias with a model nickname")
    })?;
    if matches.next().is_some() {
        return Err(format!(
            "provider `{provider}` is referenced by multiple models; pass --alias to pick one"
        ));
    }
    let (nickname, model) = first;
    Ok(ResolvedProbeTarget {
        state_key: nickname.clone(),
        provider: provider.to_string(),
        model_id: model.upstream.clone(),
    })
}

/// Resolve `(provider_name, model_id)` from a scoped target. An alias names
/// both; a bare provider resolves its model id from the single selectable
/// model referencing it. A thin view over [`resolve_probe_target`] for
/// callers that do not need the routing state key.
pub fn resolve_provider_and_model(
    config: &Config,
    provider: Option<&str>,
    alias: Option<&str>,
) -> Result<(String, String), String> {
    let target = resolve_probe_target(config, provider, alias)?;
    Ok((target.provider, target.model_id))
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
