//! The wizard's pure decision engine: given the operator's collected answers,
//! the existing config, and the sorted detection offers, produce the exact
//! `provider add` argument list, one `[models.<nick>]` wiring per selected
//! provider, and the single `aliases.default` nickname the final write
//! commits. No I/O, no lock, no network -- a plain function so the whole
//! routing decision is table-testable, and re-running with the same answers
//! yields byte-identical args (the re-init idempotence contract).

use std::collections::{BTreeMap, BTreeSet};

use routectl_router::Config;
use thiserror::Error;

use crate::commands::provider_add::ProviderAddArgs;
use crate::commands::provider_env::env_var_for_kind;

use super::{ModelWiring, Offer, OfferSource};

/// The operator's collected answers, gathered from the interactive prompts or
/// the flag surface before any write. `selected` is the chosen subset of the
/// offers; `model_ids` maps each selected provider name to its upstream model
/// id; `default_route` names the provider whose model becomes
/// `aliases.default`.
#[derive(Debug, Clone)]
pub struct WizardAnswers {
    pub selected: Vec<Offer>,
    pub model_ids: BTreeMap<String, String>,
    pub default_route: Option<String>,
    pub yes: bool,
}

/// The fully-resolved plan the orchestrator executes: one [`ProviderAddArgs`]
/// per selected provider (in stable order), one [`ModelWiring`] per provider,
/// and the single default-route nickname. Routing lands ONLY through
/// `default_alias`.
pub struct WizardPlan {
    pub provider_args: Vec<ProviderAddArgs>,
    pub models: Vec<ModelWiring>,
    pub default_alias: String,
}

/// Actionable, typed failures the orchestrator renders directly -- never a
/// panic and never a string the caller must parse. Each corresponds to a
/// missing operator input that must be resolved before any side effect.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanError {
    #[error(
        "provider `{provider}` has no model id; enter one when prompted or pass \
         `--default-model <UPSTREAM>`"
    )]
    MissingModelId { provider: String },
    #[error(
        "no default route chosen; pick one when prompted or pass \
         `--default-model <UPSTREAM>`"
    )]
    MissingDefaultRoute,
    #[error("default route `{provider}` is not among the selected providers")]
    DefaultRouteNotSelected { provider: String },
}

/// Turn already-collected answers into the ordered `provider add` args, the
/// per-provider model wirings, and the single default alias. Pure: no I/O, no
/// lock, no network. Deterministic -- `provider_args` and `models` follow the
/// offers' stable sorted order, so a re-run with the same answers produces
/// byte-identical output.
pub fn build_plan(
    answers: &WizardAnswers,
    existing: &Config,
    offers: &[Offer],
) -> Result<WizardPlan, PlanError> {
    let working = selected_in_order(&answers.selected, offers);

    let mut provider_args = Vec::with_capacity(working.len());
    let mut models = Vec::with_capacity(working.len());
    let mut used_nicks: BTreeSet<String> = BTreeSet::new();
    let mut nick_by_provider: BTreeMap<String, String> = BTreeMap::new();

    for offer in &working {
        let upstream = answers.model_ids.get(&offer.provider_name).ok_or_else(|| {
            PlanError::MissingModelId {
                provider: offer.provider_name.clone(),
            }
        })?;

        let nick = assign_nick(&offer.provider_name, existing, &used_nicks);
        used_nicks.insert(nick.clone());
        nick_by_provider.insert(offer.provider_name.clone(), nick.clone());

        provider_args.push(provider_args_for(offer));
        models.push(ModelWiring {
            nick,
            provider: offer.provider_name.clone(),
            upstream: upstream.clone(),
        });
    }

    let default_route = answers
        .default_route
        .as_deref()
        .ok_or(PlanError::MissingDefaultRoute)?;
    let default_alias = nick_by_provider
        .get(default_route)
        .cloned()
        .ok_or_else(|| PlanError::DefaultRouteNotSelected {
            provider: default_route.to_string(),
        })?;

    Ok(WizardPlan {
        provider_args,
        models,
        default_alias,
    })
}

/// The chosen offers rendered in a total, stable order: sorted by provider
/// name, then by source so two offers sharing a name (e.g. an oauth and an env
/// credential for the same provider) order deterministically. Filtering the
/// canonical `offers` list keeps identity anchored to detection, not to
/// whatever order the operator picked them in.
fn selected_in_order(selected: &[Offer], offers: &[Offer]) -> Vec<Offer> {
    let mut working: Vec<Offer> = offers
        .iter()
        .filter(|offer| selected.contains(offer))
        .cloned()
        .collect();
    working.sort_by(|a, b| {
        a.provider_name
            .cmp(&b.provider_name)
            .then_with(|| source_rank(a.source).cmp(&source_rank(b.source)))
    });
    working
}

const fn source_rank(source: OfferSource) -> u8 {
    match source {
        OfferSource::Oauth => 0,
        OfferSource::Env => 1,
        OfferSource::Forwarded => 2,
        OfferSource::ApiKeyPrompt => 3,
    }
}

/// Pick the deterministic model nickname for `provider`: the provider name
/// itself, disambiguated with a numeric suffix only against a name already
/// used earlier in this plan or an existing `[models.<nick>]` that targets a
/// DIFFERENT provider. An existing model under the same nick already pointing
/// at THIS provider is reused verbatim -- that reuse is what makes a re-init
/// re-emit a byte-identical model block instead of minting a new one.
fn assign_nick(provider: &str, existing: &Config, used: &BTreeSet<String>) -> String {
    let mut candidate = provider.to_string();
    let mut suffix = 1u32;
    while nick_taken(&candidate, provider, existing, used) {
        suffix += 1;
        candidate = format!("{provider}{suffix}");
    }
    candidate
}

fn nick_taken(candidate: &str, provider: &str, existing: &Config, used: &BTreeSet<String>) -> bool {
    if used.contains(candidate) {
        return true;
    }
    matches!(existing.models.get(candidate), Some(model) if model.provider != provider)
}

/// Map one selected offer to the `provider add` args that add it. The oauth
/// sentinel and the env credential var are the only source-specific pieces;
/// every arg carries `yes: true` because init owns the one wizard-level ack
/// and calls `provider add` with its confirm bypassed.
fn provider_args_for(offer: &Offer) -> ProviderAddArgs {
    let (credential_source, api_key_env) = match offer.source {
        OfferSource::Forwarded => (Some("forwarded".to_string()), None),
        OfferSource::Oauth => (None, None),
        OfferSource::Env => (None, env_var_for_kind(&offer.kind).map(str::to_string)),
        // No env var and no forwarded source: `provider add` falls through to
        // its interactive hidden-key prompt and captures to the managed store.
        OfferSource::ApiKeyPrompt => (None, None),
    };
    ProviderAddArgs {
        kind: offer.provider_add_kind().to_string(),
        name: offer.provider_name.clone(),
        base_url: None,
        api_key_env,
        secret_ref: None,
        api_key_stdin: false,
        credential_source,
        overwrite: false,
        yes: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_router::ModelEntry;

    fn offer(name: &str, kind: &str, source: OfferSource) -> Offer {
        Offer {
            provider_name: name.to_string(),
            kind: kind.to_string(),
            source,
            credential_class: match source {
                OfferSource::Oauth => "oauth",
                OfferSource::Env => "env",
                OfferSource::Forwarded => "forwarded",
                OfferSource::ApiKeyPrompt => "api-key",
            }
            .to_string(),
        }
    }

    fn model_ids(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(p, m)| ((*p).to_string(), (*m).to_string()))
            .collect()
    }

    fn answers(selected: Vec<Offer>, ids: &[(&str, &str)], default: Option<&str>) -> WizardAnswers {
        WizardAnswers {
            selected,
            model_ids: model_ids(ids),
            default_route: default.map(str::to_string),
            yes: false,
        }
    }

    #[test]
    fn maps_each_source_to_the_expected_provider_add_args() {
        let offers = vec![
            offer("claude-sub", "anthropic-api", OfferSource::Oauth),
            offer("claude-env", "anthropic-api", OfferSource::Env),
            offer("claude-fwd", "anthropic-api", OfferSource::Forwarded),
        ];
        let a = answers(
            offers.clone(),
            &[
                ("claude-sub", "claude-3-5-sonnet"),
                ("claude-env", "claude-3-5-sonnet"),
                ("claude-fwd", "claude-3-5-sonnet"),
            ],
            Some("claude-fwd"),
        );

        let plan = build_plan(&a, &Config::default(), &offers).expect("plan builds");

        // Sorted by provider name: claude-env, claude-fwd, claude-sub.
        let env = &plan.provider_args[0];
        assert_eq!(env.name, "claude-env");
        assert_eq!(env.kind, "anthropic-api");
        assert_eq!(env.credential_source, None);
        assert_eq!(env.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(env.yes);

        let fwd = &plan.provider_args[1];
        assert_eq!(fwd.name, "claude-fwd");
        assert_eq!(fwd.kind, "anthropic-api");
        assert_eq!(fwd.credential_source.as_deref(), Some("forwarded"));
        assert_eq!(fwd.api_key_env, None);
        assert!(fwd.yes);

        let oauth = &plan.provider_args[2];
        assert_eq!(oauth.name, "claude-sub");
        assert_eq!(oauth.kind, "anthropic", "oauth routes through the sentinel");
        assert_eq!(oauth.credential_source, None);
        assert_eq!(oauth.api_key_env, None);
        assert!(oauth.yes);
    }

    #[test]
    fn api_key_prompt_offer_maps_to_bare_anthropic_api_args_for_the_hidden_prompt() {
        // The empty-offer capture branch's api-key offer carries no env var and
        // no forwarded source, so `provider add` falls through to its hidden
        // prompt and captures to the managed store. Anything else here (an env
        // var, a forwarded source) would short-circuit that capture.
        let offers = vec![offer(
            "anthropic",
            "anthropic-api",
            OfferSource::ApiKeyPrompt,
        )];
        let a = answers(
            offers.clone(),
            &[("anthropic", "claude-sonnet-4-5")],
            Some("anthropic"),
        );

        let plan = build_plan(&a, &Config::default(), &offers).expect("plan builds");
        let args = &plan.provider_args[0];
        assert_eq!(args.kind, "anthropic-api");
        assert_eq!(args.api_key_env, None);
        assert_eq!(args.credential_source, None);
        assert_eq!(args.secret_ref, None);
        assert!(!args.api_key_stdin);
        assert!(
            args.yes,
            "init owns the ack; provider add's confirm is bypassed"
        );
    }

    #[test]
    fn every_arg_omits_base_url_secret_ref_stdin_and_overwrite() {
        let offers = vec![offer("p", "openai-compat", OfferSource::Env)];
        let a = answers(offers.clone(), &[("p", "gpt-4o")], Some("p"));

        let plan = build_plan(&a, &Config::default(), &offers).expect("plan builds");
        let args = &plan.provider_args[0];
        assert_eq!(args.base_url, None);
        assert_eq!(args.secret_ref, None);
        assert!(!args.api_key_stdin);
        assert!(!args.overwrite);
        assert_eq!(args.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn one_model_wiring_per_provider_and_a_single_default_alias() {
        let offers = vec![
            offer("a", "anthropic-api", OfferSource::Forwarded),
            offer("b", "openai-compat", OfferSource::Env),
        ];
        let a = answers(
            offers.clone(),
            &[("a", "claude-3-5-sonnet"), ("b", "gpt-4o")],
            Some("b"),
        );

        let plan = build_plan(&a, &Config::default(), &offers).expect("plan builds");

        assert_eq!(plan.models.len(), 2);
        assert_eq!(plan.provider_args.len(), 2);
        assert_eq!(plan.models[0].nick, "a");
        assert_eq!(plan.models[0].provider, "a");
        assert_eq!(plan.models[0].upstream, "claude-3-5-sonnet");
        assert_eq!(plan.models[1].nick, "b");
        assert_eq!(plan.models[1].upstream, "gpt-4o");
        // Routing appears only through the single default alias.
        assert_eq!(plan.default_alias, "b");
    }

    #[test]
    fn ordering_is_stable_across_a_shuffle_of_the_selection() {
        let offers = vec![
            offer("alpha", "anthropic-api", OfferSource::Forwarded),
            offer("bravo", "openai-compat", OfferSource::Env),
            offer("charlie", "anthropic-api", OfferSource::Oauth),
        ];
        let ids = &[("alpha", "m-a"), ("bravo", "m-b"), ("charlie", "m-c")];

        let forward = answers(offers.clone(), ids, Some("alpha"));
        let mut shuffled_sel = offers.clone();
        shuffled_sel.reverse();
        let reversed = answers(shuffled_sel, ids, Some("alpha"));

        let plan_a = build_plan(&forward, &Config::default(), &offers).unwrap();
        let plan_b = build_plan(&reversed, &Config::default(), &offers).unwrap();

        let names_a: Vec<&str> = plan_a.models.iter().map(|m| m.nick.as_str()).collect();
        let names_b: Vec<&str> = plan_b.models.iter().map(|m| m.nick.as_str()).collect();
        assert_eq!(names_a, vec!["alpha", "bravo", "charlie"]);
        assert_eq!(
            names_a, names_b,
            "selection order must not change output order"
        );
    }

    #[test]
    fn only_the_selected_subset_is_planned() {
        let offers = vec![
            offer("keep", "anthropic-api", OfferSource::Forwarded),
            offer("drop", "openai-compat", OfferSource::Env),
        ];
        let a = answers(
            vec![offer("keep", "anthropic-api", OfferSource::Forwarded)],
            &[("keep", "claude-3-5-sonnet")],
            Some("keep"),
        );

        let plan = build_plan(&a, &Config::default(), &offers).expect("plan builds");
        assert_eq!(plan.models.len(), 1);
        assert_eq!(plan.models[0].provider, "keep");
    }

    #[test]
    fn missing_model_id_is_a_typed_error_naming_the_provider() {
        let offers = vec![offer("p", "anthropic-api", OfferSource::Forwarded)];
        let a = answers(offers.clone(), &[], Some("p"));

        let Err(err) = build_plan(&a, &Config::default(), &offers) else {
            panic!("missing model id must error");
        };
        assert_eq!(
            err,
            PlanError::MissingModelId {
                provider: "p".to_string()
            }
        );
    }

    #[test]
    fn missing_default_route_is_a_typed_error() {
        let offers = vec![offer("p", "anthropic-api", OfferSource::Forwarded)];
        let a = answers(offers.clone(), &[("p", "claude-3-5-sonnet")], None);

        let Err(err) = build_plan(&a, &Config::default(), &offers) else {
            panic!("missing default route must error");
        };
        assert_eq!(err, PlanError::MissingDefaultRoute);
    }

    #[test]
    fn default_route_outside_the_selection_is_a_typed_error() {
        let offers = vec![offer("p", "anthropic-api", OfferSource::Forwarded)];
        let a = answers(offers.clone(), &[("p", "claude-3-5-sonnet")], Some("other"));

        let Err(err) = build_plan(&a, &Config::default(), &offers) else {
            panic!("default route outside the selection must error");
        };
        assert_eq!(
            err,
            PlanError::DefaultRouteNotSelected {
                provider: "other".to_string()
            }
        );
    }

    #[test]
    fn two_offers_sharing_a_name_get_distinct_nicks() {
        let offers = vec![
            offer("anthropic", "anthropic-api", OfferSource::Oauth),
            offer("anthropic", "anthropic-api", OfferSource::Env),
        ];
        let a = answers(
            offers.clone(),
            &[("anthropic", "claude-3-5-sonnet")],
            Some("anthropic"),
        );

        let plan = build_plan(&a, &Config::default(), &offers).expect("plan builds");
        let nicks: BTreeSet<&str> = plan.models.iter().map(|m| m.nick.as_str()).collect();
        assert_eq!(nicks.len(), 2, "two providers must get two distinct nicks");
        assert!(nicks.contains("anthropic"));
        assert!(nicks.contains("anthropic2"));
    }

    #[test]
    fn existing_model_for_the_same_provider_reuses_the_nick() {
        let mut existing = Config::default();
        existing.models.insert(
            "claude".to_string(),
            ModelEntry::new("claude", "claude-3-5-sonnet"),
        );

        let offers = vec![offer("claude", "anthropic-api", OfferSource::Forwarded)];
        let a = answers(
            offers.clone(),
            &[("claude", "claude-3-5-sonnet")],
            Some("claude"),
        );

        let plan = build_plan(&a, &existing, &offers).expect("plan builds");
        assert_eq!(
            plan.models[0].nick, "claude",
            "an existing model targeting the same provider is reused, not bumped"
        );
        assert_eq!(plan.default_alias, "claude");
    }

    #[test]
    fn existing_model_for_a_different_provider_is_not_clobbered() {
        let mut existing = Config::default();
        existing
            .models
            .insert("foo".to_string(), ModelEntry::new("bar", "unrelated-model"));

        let offers = vec![offer("foo", "anthropic-api", OfferSource::Forwarded)];
        let a = answers(offers.clone(), &[("foo", "claude-3-5-sonnet")], Some("foo"));

        let plan = build_plan(&a, &existing, &offers).expect("plan builds");
        assert_eq!(
            plan.models[0].nick, "foo2",
            "a nick colliding with an unrelated existing model is disambiguated"
        );
        assert_eq!(plan.default_alias, "foo2");
    }

    #[test]
    fn re_init_with_the_same_answers_emits_byte_identical_args() {
        let offers = vec![
            offer("claude", "anthropic-api", OfferSource::Oauth),
            offer("grok", "openai-compat", OfferSource::Env),
        ];
        let a = answers(
            offers.clone(),
            &[("claude", "claude-3-5-sonnet"), ("grok", "grok-2")],
            Some("claude"),
        );

        let first = build_plan(&a, &Config::default(), &offers).unwrap();
        let second = build_plan(&a, &Config::default(), &offers).unwrap();

        assert_eq!(first.provider_args.len(), second.provider_args.len());
        for (x, y) in first.provider_args.iter().zip(second.provider_args.iter()) {
            assert_eq!(x.kind, y.kind);
            assert_eq!(x.name, y.name);
            assert_eq!(x.base_url, y.base_url);
            assert_eq!(x.api_key_env, y.api_key_env);
            assert_eq!(x.secret_ref, y.secret_ref);
            assert_eq!(x.api_key_stdin, y.api_key_stdin);
            assert_eq!(x.credential_source, y.credential_source);
            assert_eq!(x.overwrite, y.overwrite);
            assert_eq!(x.yes, y.yes);
        }
        assert_eq!(first.models, second.models);
        assert_eq!(first.default_alias, second.default_alias);
    }
}
