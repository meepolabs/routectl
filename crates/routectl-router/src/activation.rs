//! Auto-activation inventory: which routectl-owned OAuth providers carry
//! a usable local credential, computed purely from local probes plus the
//! operator config. A leaf module -- it never imports the dispatch path
//! (`router` / `factory`), so activation state is "never traffic" by
//! construction. The server layer maps [`diff`] output to tracing events;
//! this module emits none itself and stays testable without a subscriber.
//!
//! Redaction contract (hard rule): no type in this module ever carries a
//! token, a filesystem path, or an env-var value. Every field is a
//! display-safe discriminant -- an OAuth provider id, a stable config
//! kind token, a machine-readable reason code, or a bool.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use routectl_auth::LocalProbe;

use crate::catalog::is_cataloged_provider_kind;
use crate::config::{AliasValue, Config};

/// Local depth cap for the alias-reachability walk. Kept independent of
/// the router's `ALIAS_MAX_RECURSION_DEPTH` on purpose: this module does
/// not import the dispatch path. The static config validator already
/// rejects alias cycles; this is a defensive bound so a cycle a glob
/// value could reintroduce cannot spin the walk forever.
const MAX_ALIAS_DEPTH: usize = 8;

/// Machine-readable reason an OAuth provider is not activated. Closed set
/// of display-safe discriminants (snake_case reason codes chosen to match
/// the config kind vocabulary and the audit-event field contract). No
/// variant ever carries a secret, path, or env value.
///
/// `#[non_exhaustive]`: a future managed-file credential source may add
/// reason codes without breaking callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnresolvedReason {
    /// No OAuth record exists for the provider.
    OauthMissing,
    /// A record exists but the access token is expired and no refresh
    /// token can revive it.
    OauthExpired,
    /// No OAuth store exists to probe (HOME/XDG absent).
    OauthStoreUnavailable,
    /// The provider's own-credential config kind carries no baked catalog
    /// rows, so it cannot be meaningfully routed yet.
    NotCataloged,
    /// A local probe reported an outcome this module does not yet map --
    /// treated conservatively as unresolved rather than assumed usable.
    Unknown,
}

impl UnresolvedReason {
    /// Stable snake_case reason code, safe to surface in logs and audit
    /// events. Discriminant only -- never a secret, path, or env value.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::OauthMissing => "oauth_missing",
            Self::OauthExpired => "oauth_expired",
            Self::OauthStoreUnavailable => "oauth_store_unavailable",
            Self::NotCataloged => "not_cataloged",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for UnresolvedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Activation status of a single OAuth provider.
///
/// `#[non_exhaustive]`: additive states may appear as new credential
/// sources land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActivationStatus {
    /// A usable local credential is present.
    Activated,
    /// Not usable; the `reason` says why.
    Unresolved { reason: UnresolvedReason },
}

impl ActivationStatus {
    /// True iff this is [`ActivationStatus::Activated`].
    #[must_use]
    pub const fn is_activated(&self) -> bool {
        matches!(self, Self::Activated)
    }
}

/// One provider's activation record.
///
/// `#[non_exhaustive]`: model-granularity fields are expected to be added
/// additively by a later milestone.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActivationEntry {
    /// The provider's own-credential config kind token (`kind_str()`),
    /// e.g. `anthropic-api`. Always the value from the hardcoded id->kind
    /// map; empty only for an unmapped id (a drift-guard failure, never
    /// expected at runtime).
    pub provider_kind: &'static str,
    /// Whether the credential is usable, and if not, why.
    pub status: ActivationStatus,
    /// True iff a configured provider references this OAuth credential
    /// (`oauth://<id>` api_key_ref, bare or seat-labeled) AND that
    /// provider is reachable through the alias table.
    pub referenced_by_aliases: bool,
}

/// The full activation inventory, keyed by OAuth provider id. Ordered
/// (`BTreeMap`) so diffs and audit output are deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivationState {
    entries: BTreeMap<String, ActivationEntry>,
}

impl ActivationState {
    /// Iterate entries in provider-id order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ActivationEntry)> {
        self.entries.iter().map(|(id, entry)| (id.as_str(), entry))
    }

    /// Look up one provider's record.
    #[must_use]
    pub fn get(&self, provider_id: &str) -> Option<&ActivationEntry> {
        self.entries.get(provider_id)
    }

    /// Number of recorded providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no providers are recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A provider that transitioned into the activated set.
///
/// `#[non_exhaustive]`: fields may be added additively.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActivatedChange {
    pub provider_id: String,
    pub provider_kind: &'static str,
    pub referenced_by_aliases: bool,
}

/// A provider that transitioned out of the activated set, with the reason
/// it is now unresolved.
///
/// `#[non_exhaustive]`: fields may be added additively.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeactivatedChange {
    pub provider_id: String,
    pub provider_kind: &'static str,
    pub reason: UnresolvedReason,
    pub referenced_by_aliases: bool,
}

/// The transitions between two activation states. Carries only newly
/// activated and newly deactivated providers; unchanged providers (and
/// reason-only changes among unresolved providers) produce no entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivationDelta {
    pub newly_activated: Vec<ActivatedChange>,
    pub newly_deactivated: Vec<DeactivatedChange>,
}

impl ActivationDelta {
    /// True when nothing changed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.newly_activated.is_empty() && self.newly_deactivated.is_empty()
    }
}

/// The own-credential config kind for each routectl-owned OAuth provider
/// id. Every id in `routectl_auth::oauth::known_provider_ids()` must have
/// an entry here (pinned by the drift-guard test). Each kind is the
/// stable `kind_str()` token an operator would configure to consume the
/// matching `oauth://<id>` credential:
///   - `anthropic`    -> `anthropic-api`   (claude.ai subscription)
///   - `codex`        -> `openai-responses`(ChatGPT/Codex backend)
///   - `xai`          -> `openai-compat`   (Grok, api.x.ai/v1)
///   - `antigravity`  -> `gemini`          (Gemini Cloud Code egress)
///
/// `gemini` carries no baked catalog rows today, so `antigravity` gates
/// through to `Unresolved(NotCataloged)` until a gemini catalog lands --
/// intentional, not a bug (see `is_cataloged_provider_kind`).
fn provider_kind_for_id(oauth_id: &str) -> Option<&'static str> {
    match oauth_id {
        "anthropic" => Some("anthropic-api"),
        "codex" => Some("openai-responses"),
        "xai" => Some("openai-compat"),
        "antigravity" => Some("gemini"),
        _ => None,
    }
}

/// Map a local probe to an activation status. The catch-all arm handles
/// future `LocalProbe` variants (the enum is `#[non_exhaustive]`)
/// conservatively: an unrecognized outcome is treated as unresolved, not
/// assumed usable.
const fn status_from_probe(probe: LocalProbe) -> ActivationStatus {
    match probe {
        LocalProbe::Present => ActivationStatus::Activated,
        LocalProbe::Missing => ActivationStatus::Unresolved {
            reason: UnresolvedReason::OauthMissing,
        },
        LocalProbe::Expired => ActivationStatus::Unresolved {
            reason: UnresolvedReason::OauthExpired,
        },
        LocalProbe::StoreUnavailable => ActivationStatus::Unresolved {
            reason: UnresolvedReason::OauthStoreUnavailable,
        },
        _ => ActivationStatus::Unresolved {
            reason: UnresolvedReason::Unknown,
        },
    }
}

/// Compute the activation inventory from local credential probes and the
/// operator config. PURE and INFALLIBLE: a broken or absent store surfaces
/// as `Unresolved` entries with typed reasons, never a panic or an error.
///
/// `probes` is the candidate universe -- the caller pairs each
/// `routectl_auth::oauth::known_provider_ids()` entry with its local
/// probe. Each id maps through the hardcoded id->kind table and is gated
/// by [`is_cataloged_provider_kind`]; only a cataloged kind with a
/// `Present` probe yields `Activated`.
#[must_use]
pub fn compute_activation(probes: &[(&str, LocalProbe)], config: &Config) -> ActivationState {
    let mut entries = BTreeMap::new();
    for (oauth_id, probe) in probes {
        let kind = provider_kind_for_id(oauth_id);
        let status = if kind.is_some_and(is_cataloged_provider_kind) {
            status_from_probe(*probe)
        } else {
            ActivationStatus::Unresolved {
                reason: UnresolvedReason::NotCataloged,
            }
        };
        entries.insert(
            (*oauth_id).to_string(),
            ActivationEntry {
                provider_kind: kind.unwrap_or(""),
                status,
                referenced_by_aliases: provider_alias_referenced(config, oauth_id),
            },
        );
    }
    ActivationState { entries }
}

/// Diff two activation states into the set of transitions. Pure. A
/// provider is "newly activated" when it is `Activated` in `next` but was
/// not activated in `prev` (absent or unresolved); "newly deactivated"
/// when it is unresolved in `next` but was activated in `prev`. Nothing
/// is emitted for an unchanged provider or a reason-only change among
/// unresolved providers.
///
/// The candidate universe is stable across a run (the same
/// `known_provider_ids()` set every recompute), so diffing over `next`'s
/// keys is complete: a provider never silently disappears.
#[must_use]
pub fn diff(prev: &ActivationState, next: &ActivationState) -> ActivationDelta {
    let mut newly_activated = Vec::new();
    let mut newly_deactivated = Vec::new();
    for (id, entry) in &next.entries {
        let was_activated = prev
            .entries
            .get(id)
            .is_some_and(|e| e.status.is_activated());
        match entry.status {
            ActivationStatus::Activated if !was_activated => {
                newly_activated.push(ActivatedChange {
                    provider_id: id.clone(),
                    provider_kind: entry.provider_kind,
                    referenced_by_aliases: entry.referenced_by_aliases,
                });
            }
            ActivationStatus::Unresolved { reason } if was_activated => {
                newly_deactivated.push(DeactivatedChange {
                    provider_id: id.clone(),
                    provider_kind: entry.provider_kind,
                    reason,
                    referenced_by_aliases: entry.referenced_by_aliases,
                });
            }
            _ => {}
        }
    }
    ActivationDelta {
        newly_activated,
        newly_deactivated,
    }
}

/// True iff some configured provider references `oauth://<oauth_id>` (bare
/// or seat-labeled) AND that provider is reachable via the alias table
/// (alias -> nickname -> model -> provider). Config-only; no secret
/// resolution, no network.
fn provider_alias_referenced(config: &Config, oauth_id: &str) -> bool {
    let matching: BTreeSet<&str> = config
        .providers
        .iter()
        .filter(|(_, entry)| api_key_ref_names_oauth(entry.api_key_ref(), oauth_id))
        .map(|(key, _)| key.as_str())
        .collect();
    if matching.is_empty() {
        return false;
    }
    reachable_model_nicknames(config)
        .iter()
        .filter_map(|nick| config.models.get(nick))
        .any(|model| matching.contains(model.provider.as_str()))
}

/// True when `api_key_ref` is `oauth://<oauth_id>` or
/// `oauth://<oauth_id>#<seat-label>`.
fn api_key_ref_names_oauth(api_key_ref: Option<&str>, oauth_id: &str) -> bool {
    let Some(rest) = api_key_ref.and_then(|r| r.strip_prefix("oauth://")) else {
        return false;
    };
    let provider = rest.split_once('#').map_or(rest, |(p, _)| p);
    provider == oauth_id
}

/// All model nicknames reachable from the alias table, following nested
/// alias keys (alias wins over model nickname, matching the router's
/// shadowing rule). Glob alias values are followed like any other value,
/// so a `"claude-*" = "sonnet"` entry makes `sonnet` reachable.
fn reachable_model_nicknames(config: &Config) -> BTreeSet<String> {
    let mut reached = BTreeSet::new();
    for value in config.aliases.values() {
        collect_reachable(config, value, 0, &mut reached);
    }
    reached
}

fn collect_reachable(
    config: &Config,
    value: &AliasValue,
    depth: usize,
    reached: &mut BTreeSet<String>,
) {
    if depth > MAX_ALIAS_DEPTH {
        return;
    }
    for name in value.nicknames() {
        if let Some(nested) = config.aliases.get(name) {
            collect_reachable(config, nested, depth + 1, reached);
        } else if config.models.contains_key(name) {
            reached.insert(name.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AliasValue, ModelEntry, ProviderEntry};

    fn state_with(entries: Vec<(&str, ActivationEntry)>) -> ActivationState {
        ActivationState {
            entries: entries
                .into_iter()
                .map(|(id, e)| (id.to_string(), e))
                .collect(),
        }
    }

    fn entry(kind: &'static str, status: ActivationStatus) -> ActivationEntry {
        ActivationEntry {
            provider_kind: kind,
            status,
            referenced_by_aliases: false,
        }
    }

    #[test]
    fn probe_matrix_maps_each_outcome_to_its_status() {
        let cfg = Config::default();
        let probes = [
            ("anthropic", LocalProbe::Present),
            ("codex", LocalProbe::Missing),
            ("xai", LocalProbe::Expired),
            ("antigravity", LocalProbe::StoreUnavailable),
        ];
        let state = compute_activation(&probes, &cfg);

        assert_eq!(
            state.get("anthropic").unwrap().status,
            ActivationStatus::Activated
        );
        assert_eq!(
            state.get("codex").unwrap().status,
            ActivationStatus::Unresolved {
                reason: UnresolvedReason::OauthMissing
            }
        );
        assert_eq!(
            state.get("xai").unwrap().status,
            ActivationStatus::Unresolved {
                reason: UnresolvedReason::OauthExpired
            }
        );
        // antigravity -> gemini is not cataloged, so the NotCataloged gate
        // wins over the store-unavailable probe outcome.
        assert_eq!(
            state.get("antigravity").unwrap().status,
            ActivationStatus::Unresolved {
                reason: UnresolvedReason::NotCataloged
            }
        );
    }

    #[test]
    fn store_unavailable_maps_to_reason_not_panic() {
        let cfg = Config::default();
        let state = compute_activation(&[("anthropic", LocalProbe::StoreUnavailable)], &cfg);
        assert_eq!(
            state.get("anthropic").unwrap().status,
            ActivationStatus::Unresolved {
                reason: UnresolvedReason::OauthStoreUnavailable
            }
        );
    }

    #[test]
    fn uncataloged_kind_gates_before_probe() {
        // A present token on an uncataloged kind is still Unresolved.
        let cfg = Config::default();
        let state = compute_activation(&[("antigravity", LocalProbe::Present)], &cfg);
        let e = state.get("antigravity").unwrap();
        assert_eq!(e.provider_kind, "gemini");
        assert_eq!(
            e.status,
            ActivationStatus::Unresolved {
                reason: UnresolvedReason::NotCataloged
            }
        );
    }

    #[test]
    fn referenced_by_aliases_true_when_alias_reaches_oauth_provider() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "anthropic".to_string(),
            ProviderEntry::anthropic_api("oauth://anthropic"),
        );
        cfg.models.insert(
            "sonnet".to_string(),
            ModelEntry::new("anthropic", "claude-sonnet-4-5"),
        );
        cfg.aliases.insert(
            "default".to_string(),
            AliasValue::Single("sonnet".to_string()),
        );

        let state = compute_activation(&[("anthropic", LocalProbe::Present)], &cfg);
        let e = state.get("anthropic").unwrap();
        assert!(e.referenced_by_aliases);
        assert_eq!(e.status, ActivationStatus::Activated);
    }

    #[test]
    fn referenced_by_aliases_true_with_seat_label_and_chain() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "anthropic".to_string(),
            ProviderEntry::anthropic_api("oauth://anthropic#seat-b"),
        );
        cfg.models.insert(
            "sonnet".to_string(),
            ModelEntry::new("anthropic", "claude-sonnet-4-5"),
        );
        // Nested alias: "fast" -> "primary" -> "sonnet".
        cfg.aliases.insert(
            "primary".to_string(),
            AliasValue::Single("sonnet".to_string()),
        );
        cfg.aliases.insert(
            "fast".to_string(),
            AliasValue::Chain(vec!["primary".to_string()]),
        );

        let state = compute_activation(&[("anthropic", LocalProbe::Present)], &cfg);
        assert!(state.get("anthropic").unwrap().referenced_by_aliases);
    }

    #[test]
    fn referenced_by_aliases_false_for_bare_login_empty_config() {
        let cfg = Config::default();
        let state = compute_activation(&[("anthropic", LocalProbe::Present)], &cfg);
        let e = state.get("anthropic").unwrap();
        assert!(!e.referenced_by_aliases);
        assert_eq!(e.status, ActivationStatus::Activated);
    }

    #[test]
    fn referenced_by_aliases_false_when_provider_not_alias_reachable() {
        // Provider references the oauth ref, and a model targets it, but
        // no alias reaches that model.
        let mut cfg = Config::default();
        cfg.providers.insert(
            "anthropic".to_string(),
            ProviderEntry::anthropic_api("oauth://anthropic"),
        );
        cfg.models.insert(
            "sonnet".to_string(),
            ModelEntry::new("anthropic", "claude-sonnet-4-5"),
        );
        let state = compute_activation(&[("anthropic", LocalProbe::Present)], &cfg);
        assert!(!state.get("anthropic").unwrap().referenced_by_aliases);
    }

    #[test]
    fn referenced_by_aliases_false_when_ref_names_other_provider() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "other".to_string(),
            ProviderEntry::anthropic_api("oauth://codex"),
        );
        cfg.models
            .insert("m".to_string(), ModelEntry::new("other", "x"));
        cfg.aliases
            .insert("default".to_string(), AliasValue::Single("m".to_string()));
        let state = compute_activation(&[("anthropic", LocalProbe::Present)], &cfg);
        assert!(!state.get("anthropic").unwrap().referenced_by_aliases);
    }

    #[test]
    fn referenced_by_aliases_true_via_glob_alias_value() {
        // A glob alias key whose VALUE targets the oauth-backed model makes
        // that model reachable, so referenced_by_aliases is true.
        let mut cfg = Config::default();
        cfg.providers.insert(
            "anthropic".to_string(),
            ProviderEntry::anthropic_api("oauth://anthropic"),
        );
        cfg.models.insert(
            "sonnet".to_string(),
            ModelEntry::new("anthropic", "claude-sonnet-4-5"),
        );
        cfg.aliases.insert(
            "claude-*".to_string(),
            AliasValue::Single("sonnet".to_string()),
        );
        let state = compute_activation(&[("anthropic", LocalProbe::Present)], &cfg);
        assert!(state.get("anthropic").unwrap().referenced_by_aliases);
    }

    #[test]
    fn referenced_by_aliases_follows_alias_key_shadowing_a_model_name() {
        // An alias key that shares a model nickname wins (router shadowing
        // rule): the alias value's target is followed, not the shadowed
        // model. Here `opus` is both a model AND an alias -> `sonnet`.
        let mut cfg = Config::default();
        cfg.providers.insert(
            "anthropic".to_string(),
            ProviderEntry::anthropic_api("oauth://anthropic"),
        );
        cfg.providers.insert(
            "other".to_string(),
            ProviderEntry::openai_compat("https://example.com/v1", "literal:k"),
        );
        // The shadowed model routes to a NON-oauth provider; the alias of
        // the same name routes to the oauth-backed model. Reachability must
        // follow the alias.
        cfg.models
            .insert("opus".to_string(), ModelEntry::new("other", "x"));
        cfg.models.insert(
            "sonnet".to_string(),
            ModelEntry::new("anthropic", "claude-sonnet-4-5"),
        );
        cfg.aliases
            .insert("opus".to_string(), AliasValue::Single("sonnet".to_string()));
        let state = compute_activation(&[("anthropic", LocalProbe::Present)], &cfg);
        assert!(state.get("anthropic").unwrap().referenced_by_aliases);
    }

    #[test]
    fn diff_reports_newly_activated() {
        let prev = state_with(vec![(
            "anthropic",
            entry(
                "anthropic-api",
                ActivationStatus::Unresolved {
                    reason: UnresolvedReason::OauthMissing,
                },
            ),
        )]);
        let next = state_with(vec![(
            "anthropic",
            entry("anthropic-api", ActivationStatus::Activated),
        )]);
        let delta = diff(&prev, &next);
        assert_eq!(delta.newly_activated.len(), 1);
        assert_eq!(delta.newly_activated[0].provider_id, "anthropic");
        assert_eq!(delta.newly_activated[0].provider_kind, "anthropic-api");
        assert!(delta.newly_deactivated.is_empty());
    }

    #[test]
    fn diff_reports_newly_activated_from_absent_prev() {
        let prev = ActivationState::default();
        let next = state_with(vec![(
            "anthropic",
            entry("anthropic-api", ActivationStatus::Activated),
        )]);
        let delta = diff(&prev, &next);
        assert_eq!(delta.newly_activated.len(), 1);
    }

    #[test]
    fn diff_reports_newly_deactivated_with_reason() {
        let prev = state_with(vec![(
            "anthropic",
            entry("anthropic-api", ActivationStatus::Activated),
        )]);
        let next = state_with(vec![(
            "anthropic",
            entry(
                "anthropic-api",
                ActivationStatus::Unresolved {
                    reason: UnresolvedReason::OauthExpired,
                },
            ),
        )]);
        let delta = diff(&prev, &next);
        assert!(delta.newly_activated.is_empty());
        assert_eq!(delta.newly_deactivated.len(), 1);
        assert_eq!(
            delta.newly_deactivated[0].reason,
            UnresolvedReason::OauthExpired
        );
    }

    #[test]
    fn diff_empty_when_unchanged() {
        let s = state_with(vec![
            (
                "anthropic",
                entry("anthropic-api", ActivationStatus::Activated),
            ),
            (
                "codex",
                entry(
                    "openai-responses",
                    ActivationStatus::Unresolved {
                        reason: UnresolvedReason::OauthMissing,
                    },
                ),
            ),
        ]);
        assert!(diff(&s, &s).is_empty());
    }

    #[test]
    fn diff_ignores_reason_only_change_among_unresolved() {
        let prev = state_with(vec![(
            "anthropic",
            entry(
                "anthropic-api",
                ActivationStatus::Unresolved {
                    reason: UnresolvedReason::OauthMissing,
                },
            ),
        )]);
        let next = state_with(vec![(
            "anthropic",
            entry(
                "anthropic-api",
                ActivationStatus::Unresolved {
                    reason: UnresolvedReason::OauthExpired,
                },
            ),
        )]);
        assert!(diff(&prev, &next).is_empty());
    }

    #[test]
    fn every_known_oauth_id_has_a_kind_mapping() {
        for id in routectl_auth::oauth::known_provider_ids() {
            assert!(
                provider_kind_for_id(id).is_some(),
                "no id->kind mapping for oauth provider `{id}`"
            );
        }
    }

    #[test]
    fn mapped_kind_catalog_membership_is_pinned() {
        // Pins which oauth providers' own-credential kinds carry baked
        // catalog rows. anthropic/codex/xai are cataloged (-> Activated on
        // a present token); antigravity maps to `gemini`, which has no
        // baked rows yet and is intentionally NOT cataloged (-> always
        // Unresolved(NotCataloged) until a gemini catalog lands). If this
        // set drifts, the reason-code expectations above must be
        // re-reviewed.
        let cataloged: Vec<&str> = routectl_auth::oauth::known_provider_ids()
            .iter()
            .copied()
            .filter(|id| provider_kind_for_id(id).is_some_and(is_cataloged_provider_kind))
            .collect();
        assert_eq!(cataloged, ["anthropic", "codex", "xai"]);
    }

    #[test]
    fn reason_codes_are_snake_case_discriminants() {
        assert_eq!(UnresolvedReason::OauthMissing.as_str(), "oauth_missing");
        assert_eq!(UnresolvedReason::OauthExpired.as_str(), "oauth_expired");
        assert_eq!(
            UnresolvedReason::OauthStoreUnavailable.as_str(),
            "oauth_store_unavailable"
        );
        assert_eq!(UnresolvedReason::NotCataloged.as_str(), "not_cataloged");
        assert_eq!(UnresolvedReason::Unknown.as_str(), "unknown");
        assert_eq!(UnresolvedReason::OauthMissing.to_string(), "oauth_missing");
    }
}
