//! The `init` detect step: compose the sorted [`Offer`] inventory the wizard
//! presents and `--yes` selects from. Pure reads only -- it holds no lock,
//! touches no network, and mutates nothing. It forks no new detection logic:
//! the oauth arm feeds shipped local probes through
//! [`routectl_router::compute_activation`] (the SAME inventory the server's
//! activation path consumes), the env arm reuses the conventional-var table,
//! and the forwarded arm keys off config presence.

use routectl_auth::{LocalProbe, env_ref};
use routectl_router::{ActivationStatus, Config, compute_activation};

use super::{Offer, OfferSource};
use crate::commands::provider_env::env_var_for_kind;

/// Provider kinds an env-detected credential may be OFFERED for. Restricted
/// to kinds `provider add` can build non-interactively from a bare
/// `env://VAR` ref with NO extra required input: `anthropic-api` resolves its
/// whole entry from the ref alone. `openai-compat` is deliberately excluded --
/// it additionally requires a `--base-url` an env-presence check cannot
/// supply -- and `openai-responses` is not a kind `provider add` constructs
/// from an api-key ref at all. An env var whose kind is outside this set is
/// surfaced by no offer even when it resolves.
const ENV_OFFERABLE_KINDS: &[&str] = &["anthropic-api"];

/// OAuth login ids `init` may OFFER. Restricted to the ids `provider add` can
/// actually build a provider block for through the oauth login path -- today
/// only `anthropic` (its sentinel `--kind anthropic` routes the login flow and
/// writes an `oauth://anthropic` ref). Every other activated login id maps to a
/// kind `provider add` has no oauth constructor for (`codex` -> openai-responses,
/// `xai` -> openai-compat, `antigravity` -> gemini), so offering one would
/// misroute it -- `Offer::provider_add_kind` funnels ALL oauth offers through
/// the `anthropic` sentinel, wiring `oauth://anthropic` under a foreign name. An
/// activated id outside this set is surfaced by no offer until `provider add`
/// grows a constructor for it. Mirrors [`ENV_OFFERABLE_KINDS`].
const OAUTH_OFFERABLE_IDS: &[&str] = &["anthropic"];

/// The config kind the forwarded (MITM/RC) lane routes through.
const FORWARDED_KIND: &str = "anthropic-api";

/// Compose the offered credential inventory from the operator config and a
/// set of local oauth probes. PURE: reads the process environment and the
/// passed-in config/probes only -- no store construction, no lock, no
/// network, no mutation. The caller supplies `probes` (each
/// `routectl_auth::oauth::known_provider_ids()` id paired with its
/// `OAuthStore::probe_local` outcome) exactly as the server's activation path
/// does, so this function stays testable without a live store.
///
/// The result is sorted deterministically by `(source, kind, provider_name)`
/// so the wizard's presentation and `--yes` selection are reproducible across
/// runs on identical inputs.
#[must_use]
pub fn detect_offers(config: &Config, probes: &[(&str, LocalProbe)]) -> Vec<Offer> {
    let mut offers = Vec::new();
    collect_oauth_offers(config, probes, &mut offers);
    collect_env_offers(&mut offers);
    collect_forwarded_offer(config, &mut offers);
    offers.sort_by(|a, b| offer_sort_key(a).cmp(&offer_sort_key(b)));
    offers
}

/// One `Activated` activation entry becomes one oauth offer -- but ONLY for a
/// login id `provider add` can build (see [`OAUTH_OFFERABLE_IDS`]). An entry
/// that is `Unresolved` for any reason (missing / expired / not cataloged /
/// store unavailable), or an activated id outside the offerable set, produces
/// no offer.
fn collect_oauth_offers(config: &Config, probes: &[(&str, LocalProbe)], offers: &mut Vec<Offer>) {
    let state = compute_activation(probes, config);
    for (oauth_id, entry) in state.iter() {
        if matches!(entry.status, ActivationStatus::Activated)
            && OAUTH_OFFERABLE_IDS.contains(&oauth_id)
        {
            offers.push(Offer {
                provider_name: oauth_id.to_string(),
                kind: entry.provider_kind.to_string(),
                source: OfferSource::Oauth,
                credential_class: "oauth".to_string(),
            });
        }
    }
}

/// A conventional credential var that resolves to a non-empty value NOW
/// yields one env offer. A set-but-empty or unset var yields none
/// (`env_ref` errors on both).
fn collect_env_offers(offers: &mut Vec<Offer>) {
    for kind in ENV_OFFERABLE_KINDS {
        let Some(var) = env_var_for_kind(kind) else {
            continue;
        };
        if env_ref(var).is_ok() {
            offers.push(Offer {
                provider_name: (*kind).to_string(),
                kind: (*kind).to_string(),
                source: OfferSource::Env,
                credential_class: "env".to_string(),
            });
        }
    }
}

/// A config carrying a `[mitm]` block (the MITM/RC lane is configured) yields
/// exactly one forwarded offer; its absence yields none.
fn collect_forwarded_offer(config: &Config, offers: &mut Vec<Offer>) {
    if config.mitm.is_some() {
        offers.push(forwarded_offer());
    }
}

/// The single forwarded (MITM/RC) offer. Shared by detection and the
/// orchestrator's `--forwarded` synthesis so both name the provider and kind
/// identically -- a synthesized offer must match a detected one exactly, or
/// the two paths would wire divergent provider blocks.
pub(super) fn forwarded_offer() -> Offer {
    Offer {
        provider_name: "anthropic-forwarded".to_string(),
        kind: FORWARDED_KIND.to_string(),
        source: OfferSource::Forwarded,
        credential_class: "forwarded".to_string(),
    }
}

/// Deterministic sort key. `OfferSource` carries no `Ord`, so rank it
/// explicitly (declaration order); ties break on kind then provider name.
const fn offer_sort_key(offer: &Offer) -> (u8, &str, &str) {
    let source_rank = match offer.source {
        OfferSource::Oauth => 0,
        OfferSource::Env => 1,
        OfferSource::Forwarded => 2,
        OfferSource::ApiKeyPrompt => 3,
    };
    (
        source_rank,
        offer.kind.as_str(),
        offer.provider_name.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_router::MitmConfig;

    /// Restore an env var to its prior value (or absence) on drop, so a
    /// serial env-touching test leaves the process environment untouched.
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: env-touching tests are serialized via serial_test, so
            // no other thread reads or writes the environment concurrently.
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }

        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: see `set`.
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                // SAFETY: see `set`.
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    /// The conventional var whose presence drives the sole env offer.
    const ANTHROPIC_ENV_VAR: &str = "ANTHROPIC_API_KEY";

    #[test]
    #[serial_test::serial]
    fn oauth_present_cataloged_kind_yields_an_offer() {
        let _guard = EnvGuard::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config::default();
        let offers = detect_offers(&cfg, &[("anthropic", LocalProbe::Present)]);

        assert_eq!(offers.len(), 1);
        let offer = &offers[0];
        assert_eq!(offer.source, OfferSource::Oauth);
        assert_eq!(offer.kind, "anthropic-api");
        assert_eq!(offer.provider_name, "anthropic");
        assert_eq!(offer.credential_class, "oauth");
    }

    #[test]
    #[serial_test::serial]
    fn oauth_missing_expired_or_uncataloged_yields_no_offer() {
        let _guard = EnvGuard::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config::default();
        // `codex` missing / `xai` expired -> Unresolved; `antigravity`
        // (gemini) is present but not cataloged -> Unresolved.
        let offers = detect_offers(
            &cfg,
            &[
                ("codex", LocalProbe::Missing),
                ("xai", LocalProbe::Expired),
                ("antigravity", LocalProbe::Present),
            ],
        );
        assert!(
            offers.is_empty(),
            "no activated oauth entry, got {offers:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn activated_non_anthropic_oauth_id_is_not_offered() {
        let _guard = EnvGuard::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config::default();
        // `xai` -> openai-compat and `codex` -> openai-responses are BOTH
        // cataloged, so a `Present` probe activates each. But `provider add`
        // has no oauth constructor for either: every oauth offer funnels
        // through the `anthropic` sentinel, so offering these would misroute
        // them to `oauth://anthropic` under a foreign name. They must yield no
        // offer.
        let offers = detect_offers(
            &cfg,
            &[("xai", LocalProbe::Present), ("codex", LocalProbe::Present)],
        );
        assert!(
            offers.iter().all(|o| o.source != OfferSource::Oauth),
            "a non-anthropic activated oauth id must not be offered, got {offers:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn env_resolvable_var_yields_an_env_offer() {
        let _guard = EnvGuard::set(ANTHROPIC_ENV_VAR, "sk-ant-not-a-real-key");
        let cfg = Config::default();
        let offers = detect_offers(&cfg, &[]);

        assert_eq!(offers.len(), 1);
        let offer = &offers[0];
        assert_eq!(offer.source, OfferSource::Env);
        assert_eq!(offer.kind, "anthropic-api");
        assert_eq!(offer.credential_class, "env");
    }

    #[test]
    #[serial_test::serial]
    fn env_set_but_empty_var_yields_no_offer() {
        let _guard = EnvGuard::set(ANTHROPIC_ENV_VAR, "");
        let cfg = Config::default();
        let offers = detect_offers(&cfg, &[]);
        assert!(offers.is_empty(), "a set-but-empty var must not offer");
    }

    #[test]
    #[serial_test::serial]
    fn env_unset_var_yields_no_offer() {
        let _guard = EnvGuard::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config::default();
        let offers = detect_offers(&cfg, &[]);
        assert!(offers.is_empty(), "an unset var must not offer");
    }

    #[test]
    #[serial_test::serial]
    fn forwarded_offer_present_only_with_a_mitm_block() {
        let _guard = EnvGuard::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config {
            mitm: Some(MitmConfig::default()),
            ..Config::default()
        };
        let offers = detect_offers(&cfg, &[]);

        assert_eq!(offers.len(), 1);
        let offer = &offers[0];
        assert_eq!(offer.source, OfferSource::Forwarded);
        assert_eq!(offer.kind, "anthropic-api");
        assert_eq!(offer.credential_class, "forwarded");
    }

    #[test]
    #[serial_test::serial]
    fn no_mitm_block_yields_no_forwarded_offer() {
        let _guard = EnvGuard::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config::default();
        let offers = detect_offers(&cfg, &[]);
        assert!(
            offers.iter().all(|o| o.source != OfferSource::Forwarded),
            "no [mitm] block must yield no forwarded offer",
        );
    }

    #[test]
    #[serial_test::serial]
    fn result_is_sorted_deterministically_across_repeated_calls() {
        let _guard = EnvGuard::set(ANTHROPIC_ENV_VAR, "sk-ant-not-a-real-key");
        let cfg = Config {
            mitm: Some(MitmConfig::default()),
            ..Config::default()
        };
        let probes = [("anthropic", LocalProbe::Present)];

        let first = detect_offers(&cfg, &probes);
        let second = detect_offers(&cfg, &probes);
        assert_eq!(first, second, "repeated calls must be byte-identical");

        // All three sources present, sorted by (source, kind, name):
        // Oauth < Env < Forwarded.
        let sources: Vec<OfferSource> = first.iter().map(|o| o.source).collect();
        assert_eq!(
            sources,
            vec![OfferSource::Oauth, OfferSource::Env, OfferSource::Forwarded],
        );
    }
}
