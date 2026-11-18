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
use crate::commands::provider_add::LOGIN_VALUES;
use crate::commands::provider_env::env_var_for_kind;

/// Provider kinds an env-detected credential may be OFFERED for. Restricted
/// to kinds `provider add` can build non-interactively from a bare
/// `env://VAR` ref with NO extra required input: `anthropic-api` resolves its
/// whole entry from the ref alone. `openai-compat` and `openai-responses` are
/// deliberately excluded: they additionally require a `--base-url` an
/// env-presence check cannot supply. An env var whose kind is outside this set is
/// surfaced by no offer even when it resolves.
const ENV_OFFERABLE_KINDS: &[&str] = &[routectl_core::provider_kind::config::ANTHROPIC_API];

/// OAuth login ids `init` may OFFER: exactly the managed logins `provider add
/// --login` builds an entry for, since init writes providers only through
/// `provider add`. An activated id outside this set (`xai`, `antigravity`) is
/// surfaced by no offer.
const OAUTH_OFFERABLE_IDS: &[&str] = &LOGIN_VALUES;

/// The config kind the forwarded (MITM/RC) lane routes through.
const FORWARDED_KIND: &str = routectl_core::provider_kind::config::ANTHROPIC_API;

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
        if !matches!(entry.status, ActivationStatus::Activated) {
            continue;
        }
        let Some(vendor) = OAUTH_OFFERABLE_IDS
            .iter()
            .copied()
            .find(|id| *id == oauth_id)
        else {
            continue;
        };
        offers.push(Offer {
            provider_name: oauth_id.to_string(),
            kind: entry.provider_kind.to_string(),
            source: OfferSource::Oauth { vendor },
            credential_class: "oauth".to_string(),
        });
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
        OfferSource::Oauth { .. } => 0,
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
    use routectl_testkit::ScopedEnv;

    /// The conventional var whose presence drives the sole env offer.
    const ANTHROPIC_ENV_VAR: &str = "ANTHROPIC_API_KEY";

    /// Managed logins `provider add --login` does not build.
    const NON_OFFERABLE_IDS: [&str; 2] = ["xai", "antigravity"];

    #[test]
    #[serial_test::serial]
    fn activated_anthropic_and_codex_probes_each_yield_an_offer_of_their_vendor() {
        let _guard = ScopedEnv::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config::default();

        let offers = detect_offers(
            &cfg,
            &[
                ("anthropic", LocalProbe::Present),
                ("codex", LocalProbe::Present),
            ],
        );

        let expected = [
            ("anthropic", "anthropic-api"),
            ("codex", "openai-responses"),
        ]
        .map(|(vendor, kind)| Offer {
            provider_name: vendor.to_string(),
            kind: kind.to_string(),
            source: OfferSource::Oauth { vendor },
            credential_class: "oauth".to_string(),
        });
        assert_eq!(offers, expected);
    }

    #[test]
    #[serial_test::serial]
    fn activated_xai_and_antigravity_probes_yield_no_offer() {
        let _guard = ScopedEnv::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config::default();
        let probes = NON_OFFERABLE_IDS.map(|id| (id, LocalProbe::Present));
        let activation = compute_activation(&probes, &cfg);
        for id in NON_OFFERABLE_IDS {
            let status = activation.get(id).map(|entry| entry.status);
            assert_eq!(
                status,
                Some(ActivationStatus::Activated),
                "`{id}` must be activated in the fixture"
            );
        }

        let offers = detect_offers(&cfg, &probes);

        assert!(
            offers.is_empty(),
            "an activated id outside the offerable set must not be offered, got {offers:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn unresolved_offerable_probes_yield_no_offer() {
        let _guard = ScopedEnv::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config::default();
        let rows = [
            ("missing", LocalProbe::Missing),
            ("expired", LocalProbe::Expired),
            ("store unavailable", LocalProbe::StoreUnavailable),
        ];
        for (row, probe) in rows {
            let offers = detect_offers(&cfg, &[("anthropic", probe), ("codex", probe)]);
            assert!(offers.is_empty(), "row `{row}`: got {offers:?}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn env_resolvable_var_yields_an_env_offer() {
        let _guard = ScopedEnv::set(ANTHROPIC_ENV_VAR, "sk-ant-not-a-real-key");
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
        let _guard = ScopedEnv::set(ANTHROPIC_ENV_VAR, "");
        let cfg = Config::default();
        let offers = detect_offers(&cfg, &[]);
        assert!(offers.is_empty(), "a set-but-empty var must not offer");
    }

    #[test]
    #[serial_test::serial]
    fn env_unset_var_yields_no_offer() {
        let _guard = ScopedEnv::unset(ANTHROPIC_ENV_VAR);
        let cfg = Config::default();
        let offers = detect_offers(&cfg, &[]);
        assert!(offers.is_empty(), "an unset var must not offer");
    }

    #[test]
    #[serial_test::serial]
    fn forwarded_offer_present_only_with_a_mitm_block() {
        let _guard = ScopedEnv::unset(ANTHROPIC_ENV_VAR);
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
        let _guard = ScopedEnv::unset(ANTHROPIC_ENV_VAR);
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
        let _guard = ScopedEnv::set(ANTHROPIC_ENV_VAR, "sk-ant-not-a-real-key");
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
            vec![
                OfferSource::Oauth {
                    vendor: "anthropic"
                },
                OfferSource::Env,
                OfferSource::Forwarded
            ],
        );
    }
}
