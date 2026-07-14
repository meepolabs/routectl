//! Config-diff classification shared by the `serve` hot-reload logging
//! path and `config set`/`config unset` (`commands::config_edit`).
//!
//! Two runtime diff functions plus a drift tripwire. The functions encode
//! `serve` RUNTIME semantics -- what a live config swap actually applies vs
//! what waits for a daemon restart, and which egress-defining edits warrant
//! an operator confirmation -- so they live next to the reload code that
//! defines them, not in the schema-owning router crate.

use std::collections::BTreeSet;

use routectl_router::{Config, ProviderEntry};

/// Top-level `[Config]` sections whose change requires a daemon restart to
/// take effect. Coarse per-SECTION bucketing for the drift tripwire (see
/// [`every_top_level_field_is_classified`]); the runtime
/// [`collect_restart_required_changes`] diffs the individual knobs inside.
#[cfg(test)]
pub(crate) const RESTART_REQUIRED_SECTIONS: &[&str] = &["server", "log", "usage", "mitm"];

/// Top-level sections carrying egress-defining knobs (where requests go,
/// which credential authenticates them) that warrant an operator prompt on
/// `config set`. The `[mitm]` section is egress-relevant too, but its whole
/// block is restart-required, so it is bucketed there; the runtime
/// [`collect_high_consequence_changes`] still flags its egress fields.
#[cfg(test)]
pub(crate) const HIGH_CONSEQUENCE_SECTIONS: &[&str] = &["providers"];

/// Top-level sections that hot-reload cleanly on the next config swap with
/// no restart and no confirmation prompt.
#[cfg(test)]
pub(crate) const HOT_RELOADABLE_SECTIONS: &[&str] = &[
    "version",
    "aliases",
    "retry",
    "bedrock",
    "models",
    "registry",
    "cache",
    "reduction",
    "trim",
    "cache_pricing",
    "capability",
];

/// Diff the previous config against the new one and return the names of
/// fields whose change requires a daemon restart to take effect. Per the
/// architect-validated classification: bind, listener auth, the
/// `DefaultBodyLimit` axum layer, and the three `[log]` knobs (deliberately
/// frozen behind `OnceLock` in `routectl-core/src/log_safe.rs`) all stay
/// restart-required. `usage.db_path` (the writer holds a handle opened at
/// boot) and `usage.retention_days` (pruning runs only at startup, so a
/// changed value takes effect only on the next daemon start) are
/// restart-required too; `usage.enabled` alone flips live. A `[mitm]` edit
/// is restart-required because the front-proxy listener is spawned once at
/// boot.
pub(crate) fn collect_restart_required_changes(prev: &Config, next: &Config) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();

    if prev.server.host != next.server.host {
        out.push("server.host");
    }
    if prev.server.port != next.server.port {
        out.push("server.port");
    }
    let prev_tokens: &[String] = prev
        .server
        .auth
        .as_ref()
        .map_or(&[], |a| a.tokens.as_slice());
    let next_tokens: &[String] = next
        .server
        .auth
        .as_ref()
        .map_or(&[], |a| a.tokens.as_slice());
    if prev_tokens != next_tokens {
        out.push("server.auth.tokens");
    }
    if prev.server.max_body_bytes != next.server.max_body_bytes {
        out.push("server.max_body_bytes");
    }

    if prev.log.trace_headers != next.log.trace_headers {
        out.push("log.trace_headers");
    }
    if prev.log.trace_body_bytes != next.log.trace_body_bytes {
        out.push("log.trace_body_bytes");
    }
    if prev.log.redact_prompts != next.log.redact_prompts {
        out.push("log.redact_prompts");
    }

    if prev.usage.db_path != next.usage.db_path {
        out.push("usage.db_path");
    }
    if prev.usage.retention_days != next.usage.retention_days {
        out.push("usage.retention_days");
    }

    if prev.mitm != next.mitm {
        out.push("mitm");
    }

    out
}

/// Diff the previous config against the new one and return the names of
/// egress-defining fields whose change is high-consequence: the operator is
/// prompted before such an edit lands (`config set` reuses the confirmation
/// pattern; `--yes` bypasses). Covers every provider entry's `base_url` and
/// `credential_source` and the `[mitm]` block's upstream origin + SNI/Host.
/// Local knobs (`[mitm] listen_port`, `cert_dir`) are not egress and stay
/// out.
pub(crate) fn collect_high_consequence_changes(prev: &Config, next: &Config) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    keys.extend(prev.providers.keys().map(String::as_str));
    keys.extend(next.providers.keys().map(String::as_str));

    let mut base_url_changed = false;
    let mut credential_source_changed = false;
    for key in keys {
        let (prev_url, prev_src) = provider_egress(prev.providers.get(key));
        let (next_url, next_src) = provider_egress(next.providers.get(key));
        base_url_changed |= prev_url != next_url;
        credential_source_changed |= prev_src != next_src;
    }
    if base_url_changed {
        out.push("providers.base_url");
    }
    if credential_source_changed {
        out.push("providers.credential_source");
    }

    let prev_origin = prev.mitm.as_ref().map(|m| m.upstream_origin.as_str());
    let next_origin = next.mitm.as_ref().map(|m| m.upstream_origin.as_str());
    if prev_origin != next_origin {
        out.push("mitm.upstream_origin");
    }
    let prev_host = prev.mitm.as_ref().map(|m| m.mitm_host.as_str());
    let next_host = next.mitm.as_ref().map(|m| m.mitm_host.as_str());
    if prev_host != next_host {
        out.push("mitm.mitm_host");
    }

    out
}

/// Project a provider entry down to its two egress-defining knobs.
/// `ProviderEntry` carries neither `PartialEq` nor a public reader for
/// `base_url` / `credential_source`, so read them off the serialized value
/// -- the only crate-boundary-respecting way to diff just these fields
/// across the tagged variants. Variants without a field (e.g. Bedrock has
/// no `base_url`) yield `None`.
fn provider_egress(entry: Option<&ProviderEntry>) -> (Option<String>, Option<String>) {
    let Some(entry) = entry else {
        return (None, None);
    };
    let value = serde_json::to_value(entry).unwrap_or(serde_json::Value::Null);
    let read = |field: &str| {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    (read("base_url"), read("credential_source"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_restart_required_changes_flags_bind_and_log() {
        use routectl_router::{Config, ServerAuth, ServerConfig};

        // `next` is cloned from `prev` (not a second `Config::default()`) so
        // both share an identical baseline -- including `usage.db_path`, whose
        // default reads `XDG_CONFIG_HOME`/`HOME` and so can differ between two
        // independent `Config::default()` calls if a concurrent test mutates
        // those env vars. Cloning keeps this test hermetic against that.
        let mut prev = Config::default();
        let mut next = prev.clone();

        // Baseline: identical configs -> empty list.
        assert!(collect_restart_required_changes(&prev, &next).is_empty());

        // Host change -> server.host.
        next.server = ServerConfig {
            host: "0.0.0.0".into(),
            ..ServerConfig::default()
        };
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"server.host"), "got {changes:?}");

        // Token change -> server.auth.tokens.
        prev.server = ServerConfig::default();
        next.server = ServerConfig {
            auth: Some(ServerAuth {
                tokens: vec!["literal:tok-1".into()],
            }),
            ..ServerConfig::default()
        };
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"server.auth.tokens"), "got {changes:?}");

        // Log knob change -> log.redact_prompts.
        prev = Config::default();
        next = prev.clone();
        next.log.redact_prompts = Some(true);
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"log.redact_prompts"), "got {changes:?}");

        // usage.db_path change -> restart-required.
        prev = Config::default();
        next = prev.clone();
        next.usage.db_path = std::path::PathBuf::from("/tmp/other-usage.db");
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"usage.db_path"), "got {changes:?}");

        // usage.enabled change -> hot-reload, NOT restart-required.
        prev = Config::default();
        next = prev.clone();
        next.usage.enabled = !prev.usage.enabled;
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(
            !changes.contains(&"usage.enabled") && changes.is_empty(),
            "enabled must hot-reload; got {changes:?}"
        );

        // usage.retention_days change -> restart-required (pruning is
        // startup-only, so a changed value takes effect only at the next
        // daemon start; the reload printout must not silently drop it).
        prev = Config::default();
        next = prev.clone();
        next.usage.retention_days = prev.usage.retention_days + 1;
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(
            changes.contains(&"usage.retention_days"),
            "retention_days must be restart-required; got {changes:?}"
        );

        // [mitm] edit -> restart-required (the MITM listener is
        // startup-only; a hot-reloaded edit has no live listener to
        // apply to).
        use routectl_router::MitmConfig;
        prev = Config::default();
        next = prev.clone();
        next.mitm = Some(MitmConfig::default());
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"mitm"), "got {changes:?}");

        prev = Config::default();
        prev.mitm = Some(MitmConfig::default());
        next = prev.clone();
        next.mitm = Some(MitmConfig {
            listen_port: prev.mitm.as_ref().unwrap().listen_port + 1,
            ..MitmConfig::default()
        });
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"mitm"), "got {changes:?}");
    }

    #[test]
    fn high_consequence_flags_provider_base_url() {
        use routectl_router::ProviderEntry;

        let mut prev = Config::default();
        prev.providers.insert(
            "anthropic".into(),
            ProviderEntry::anthropic_api("env://KEY"),
        );
        let mut next = prev.clone();

        // No-op: identical configs -> empty.
        assert!(collect_high_consequence_changes(&prev, &next).is_empty());

        // base_url change -> providers.base_url.
        next.providers.insert(
            "anthropic".into(),
            ProviderEntry::anthropic_api("env://KEY")
                .with_base_url("https://elsewhere.example.com"),
        );
        let changes = collect_high_consequence_changes(&prev, &next);
        assert!(changes.contains(&"providers.base_url"), "got {changes:?}");
    }

    #[test]
    fn high_consequence_flags_provider_credential_source() {
        use routectl_router::ProviderEntry;
        use routectl_router::config::CredentialSource;

        let mut prev = Config::default();
        prev.providers.insert(
            "anthropic".into(),
            ProviderEntry::anthropic_api("env://KEY"),
        );

        // No-op on an unrelated add (same egress knobs) must not fire on
        // credential_source.
        let mut next = prev.clone();
        next.providers.insert(
            "anthropic".into(),
            ProviderEntry::anthropic_api("")
                .with_base_url("https://api.anthropic.com")
                .with_credential_source(CredentialSource::Forwarded),
        );
        let changes = collect_high_consequence_changes(&prev, &next);
        assert!(
            changes.contains(&"providers.credential_source"),
            "got {changes:?}"
        );
    }

    #[test]
    fn high_consequence_flags_mitm_egress_absent_on_no_op() {
        use routectl_router::MitmConfig;

        // Enabling [mitm] surfaces both egress fields.
        let prev = Config::default();
        let mut next = prev.clone();
        next.mitm = Some(MitmConfig::default());
        let changes = collect_high_consequence_changes(&prev, &next);
        assert!(
            changes.contains(&"mitm.upstream_origin") && changes.contains(&"mitm.mitm_host"),
            "got {changes:?}"
        );

        // upstream_origin change alone -> only that field.
        let prev = Config {
            mitm: Some(MitmConfig::default()),
            ..Config::default()
        };
        let mut next = prev.clone();
        next.mitm = Some(MitmConfig {
            upstream_origin: "https://api.anthropic.com/".into(),
            ..MitmConfig::default()
        });
        let changes = collect_high_consequence_changes(&prev, &next);
        assert!(
            changes.contains(&"mitm.upstream_origin") && !changes.contains(&"mitm.mitm_host"),
            "got {changes:?}"
        );

        // A local-only [mitm] edit (listen_port) is not egress -> no
        // high-consequence flag.
        let prev = Config {
            mitm: Some(MitmConfig::default()),
            ..Config::default()
        };
        let mut next = prev.clone();
        next.mitm = Some(MitmConfig {
            listen_port: prev.mitm.as_ref().unwrap().listen_port + 1,
            ..MitmConfig::default()
        });
        assert!(collect_high_consequence_changes(&prev, &next).is_empty());
    }

    /// Drift tripwire: every top-level `Config` field the schema exposes must
    /// be classified into EXACTLY ONE of the three sets. A new unclassified
    /// `Config` field fails this test, forcing a deliberate reload-semantics
    /// decision instead of a silent omission from the diff classifiers.
    #[test]
    fn every_top_level_field_is_classified() {
        let schema: serde_json::Value =
            serde_json::from_str(&routectl_router::schema_gen::render_schema_json())
                .expect("rendered config schema parses");
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("config schema root carries top-level properties");

        let classified: BTreeSet<&str> = RESTART_REQUIRED_SECTIONS
            .iter()
            .chain(HIGH_CONSEQUENCE_SECTIONS)
            .chain(HOT_RELOADABLE_SECTIONS)
            .copied()
            .collect();

        // Disjoint: the three sets must not overlap (exactly-one membership).
        let total = RESTART_REQUIRED_SECTIONS.len()
            + HIGH_CONSEQUENCE_SECTIONS.len()
            + HOT_RELOADABLE_SECTIONS.len();
        assert_eq!(
            total,
            classified.len(),
            "classification sets overlap; each field must appear in exactly one"
        );

        let schema_fields: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(
            schema_fields,
            classified,
            "top-level Config fields and classification sets diverged: \
             unclassified={:?}, stale={:?}",
            schema_fields.difference(&classified).collect::<Vec<_>>(),
            classified.difference(&schema_fields).collect::<Vec<_>>(),
        );
    }
}
