//! Conventional per-provider-kind credential env-var table.
//!
//! Maps each config-nameable provider kind (one a `[providers.X]` block can
//! declare in this build) to the single conventional environment variable
//! an onboarding flow may OFFER as an `env://VAR` credential ref. This
//! table exists only to SUGGEST a resolvable var to the operator -- it
//! never auto-routes a credential and never reads the environment itself.
//!
//! Two classifications cover every config-nameable provider kind:
//!
//! - IN-TABLE (`ENV_VAR_TABLE`): kinds with a single well-known env var.
//! - EXCLUDED ([`EXCLUDED_KINDS`]): kinds that intentionally have NO single
//!   conventional var (e.g. multi-var cloud credentials, or a credential
//!   that is oauth-only in practice).
//!
//! A config-nameable kind absent from BOTH lists trips the completeness
//! guard in the tests below, forcing a deliberate decision when a new
//! provider kind joins. Lookups gate on
//! [`routectl_router::is_config_provider_kind`], the same vocabulary
//! `[providers.X].kind` draws from -- not baked-catalog membership, which a
//! config-nameable kind can have zero rows in (today `gemini`) while still
//! being fully configurable.

/// Config-nameable provider kinds paired with their single conventional
/// credential env var. Keys are the stable `kind_str()` discriminants
/// (`anthropic-api`, `openai-compat`, ...). Both openai-family kinds share
/// `OPENAI_API_KEY`, the vendor's conventional variable. `gemini`'s
/// `api-key` auth mode uses `GEMINI_API_KEY` (its `cloud-code` auth mode is
/// oauth-only and has no env var to offer, but this table names one
/// convention per kind, not per auth mode).
static ENV_VAR_TABLE: &[(&str, &str)] = &[
    ("anthropic-api", "ANTHROPIC_API_KEY"),
    ("openai-compat", "OPENAI_API_KEY"),
    ("openai-responses", "OPENAI_API_KEY"),
    ("gemini", "GEMINI_API_KEY"),
];

/// Config-nameable provider kinds deliberately omitted from
/// `ENV_VAR_TABLE`: kinds with no single conventional credential var to
/// offer.
pub static EXCLUDED_KINDS: &[&str] = &[
    // Bedrock authenticates with multi-var AWS credentials
    // (AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY, plus an optional
    // AWS_SESSION_TOKEN and region), so there is no single env var to
    // offer as one `env://VAR` credential ref.
    "bedrock",
];

/// The conventional credential env-var name for a config-nameable provider
/// `kind`, or `None` when the kind is not config-nameable in this build or
/// carries no single conventional var (see [`EXCLUDED_KINDS`]).
///
/// Gated on [`routectl_router::is_config_provider_kind`]: a kind no
/// `[providers.X]` block can declare in this build never resolves, even if
/// a stray table entry named it.
#[must_use]
pub fn env_var_for_kind(kind: &str) -> Option<&'static str> {
    if !routectl_router::is_config_provider_kind(kind) {
        return None;
    }
    ENV_VAR_TABLE
        .iter()
        .find(|(table_kind, _)| *table_kind == kind)
        .map(|(_, var)| *var)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn env_var_for_kind_returns_conventional_var_for_each_in_table_kind() {
        assert_eq!(env_var_for_kind("anthropic-api"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(env_var_for_kind("openai-compat"), Some("OPENAI_API_KEY"));
        assert_eq!(env_var_for_kind("openai-responses"), Some("OPENAI_API_KEY"));
        assert_eq!(env_var_for_kind("gemini"), Some("GEMINI_API_KEY"));
    }

    #[test]
    fn env_var_for_kind_returns_none_for_excluded_config_kind() {
        // Bedrock is config-nameable but deliberately excluded (multi-var
        // creds).
        assert!(routectl_router::is_config_provider_kind("bedrock"));
        assert_eq!(env_var_for_kind("bedrock"), None);
    }

    #[test]
    fn env_var_for_kind_returns_none_for_kind_no_config_can_name() {
        assert_eq!(env_var_for_kind("not-a-real-kind"), None);
    }

    #[test]
    fn every_table_kind_is_config_nameable() {
        // Drift guard: the table may only name kinds this build's config
        // schema knows.
        for (kind, _) in ENV_VAR_TABLE {
            assert!(
                routectl_router::is_config_provider_kind(kind),
                "table kind {kind:?} is not config-nameable",
            );
        }
    }

    #[test]
    fn every_excluded_kind_is_config_nameable_and_has_no_table_entry() {
        for kind in EXCLUDED_KINDS {
            assert!(
                routectl_router::is_config_provider_kind(kind),
                "excluded kind {kind:?} is not config-nameable",
            );
            assert!(
                !ENV_VAR_TABLE
                    .iter()
                    .any(|(table_kind, _)| table_kind == kind),
                "excluded kind {kind:?} also appears in the table",
            );
        }
    }

    #[test]
    fn every_config_kind_is_classified_in_table_or_excluded() {
        // Forces a decision when a new provider kind joins the config
        // schema: each config-nameable kind must be either in the table or
        // on the exclusion list, never silently unclassified.
        let config_kinds: BTreeSet<&str> = routectl_router::CONFIG_PROVIDER_KINDS
            .iter()
            .copied()
            .collect();

        for kind in config_kinds {
            let in_table = ENV_VAR_TABLE
                .iter()
                .any(|(table_kind, _)| *table_kind == kind);
            let excluded = EXCLUDED_KINDS.contains(&kind);
            assert!(
                in_table ^ excluded,
                "config kind {kind:?} must be classified in exactly one of \
                 the table or the exclusion list (in_table={in_table}, \
                 excluded={excluded})",
            );
        }
    }
}
