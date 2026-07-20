//! `routectl init` -- the guided first-run setup. This module defines the
//! stable contracts the init flow is built from: the [`InitIo`] interactive
//! seam (a superset of [`AddIo`], so ONE fake drives every step), the
//! [`InitArgs`] flag surface, the leaf data types passed between steps
//! ([`Offer`], [`OfferSource`], [`ModelWiring`]), and the single-sourced
//! next-steps renderer the doctor path reuses.
//!
//! The clap `Cmd::Init` arm and the `run`/`run_with_io` orchestration land in
//! a later step; a dispatch arm may not point at an unimplemented run, so the
//! wiring is deferred and this module is the pure-contracts foundation.

pub mod detect;
pub mod plan;
pub mod scaffold;
pub mod write;

mod orchestrate;

use std::path::Path;

use routectl_core::Result;

use async_trait::async_trait;

use super::provider_add::{AddIo, RealAddIo};
use orchestrate::{capture_model_id, gather_probes, load_existing, prompt, yes_no};

/// The interactive seams the `init` wizard touches on top of the ones
/// [`AddIo`] already covers (stdin, hidden prompt, env offer, oauth login).
/// Every choice the guided flow asks the operator flows through here, so a
/// single fake drives an end-to-end wizard test without a TTY, and production
/// implements one trait hierarchy in [`RealInitIo`].
pub trait InitIo: AddIo {
    /// Fresh machine, no config: expert scaffold fast-path (true) vs guided
    /// wizard (false).
    fn choose_scaffold_or_wizard(&self) -> bool;
    /// Pick which offered providers to configure, from the sorted offer list.
    /// Returns the chosen indices into `offers`.
    fn select_offers(&self, offers: &[Offer]) -> Vec<usize>;
    /// Prompt for the upstream model id for one selected provider, with a
    /// per-kind example hint. `None` = skipped.
    fn prompt_model_id(
        &self,
        provider_name: &str,
        kind: &str,
        example_hint: &str,
    ) -> Option<String>;
    /// Which selected provider is the default route (aliases.default)?
    fn choose_default_route(&self, candidates: &[String]) -> Option<String>;
    /// The ONE wizard-level high-consequence ack before any write
    /// (live-daemon / config-migrate pattern; declining writes nothing).
    fn confirm_wizard_ack(&self) -> bool;
    /// Credential-less interactive run: how to capture a credential now
    /// (oauth login / hidden api-key prompt), or skip to the actionable
    /// next-step message. Only consulted when the detected offer list is empty.
    fn offer_credential_capture(&self) -> CredentialCapture;
}

/// Flag surface for `init`, assembled from the clap layer.
pub struct InitArgs {
    pub scaffold: bool,
    pub yes: bool,
    pub default_model: Option<String>,
    pub forwarded: bool,
}

/// One detected credential the wizard may OFFER (never auto-routes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// Proposed `[providers.<name>]` key.
    pub provider_name: String,
    /// kind_str token: anthropic-api / openai-compat / ... This is the CONFIG
    /// kind, which is asymmetric with the `provider add` `--kind` sentinel: an
    /// oauth offer's config kind is `anthropic-api`, but `provider add` routes
    /// the login path off the sentinel `anthropic`. Use
    /// [`Offer::provider_add_kind`] to feed `provider add`, never `kind`
    /// directly.
    pub kind: String,
    pub source: OfferSource,
    /// Scheme label: oauth / env / forwarded.
    pub credential_class: String,
}

impl Offer {
    /// The `--kind` token to hand `provider add` for this offer. An oauth
    /// offer maps to the sentinel `anthropic` (the login path); every other
    /// source uses its config `kind` verbatim. This oauth->`anthropic` mapping
    /// is only sound because detection restricts oauth offers to the `anthropic`
    /// login id (see `detect::OAUTH_OFFERABLE_IDS`): `provider add` has no oauth
    /// constructor for any other login id, so an offer for one would misroute
    /// here into the anthropic path. Keep the two in lockstep.
    pub fn provider_add_kind(&self) -> &str {
        match self.source {
            OfferSource::Oauth => "anthropic",
            _ => &self.kind,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferSource {
    Oauth,
    Env,
    Forwarded,
    /// Synthesis-only: a credential the operator will type at the interactive
    /// hidden prompt. Detection never produces it (it cannot see a key not yet
    /// entered); the empty-offer capture branch synthesizes it so `provider
    /// add` reaches its existing hidden-prompt capture through the normal plan
    /// pipeline, landing the value in the managed store as a `file://` ref.
    ApiKeyPrompt,
}

/// How the operator wants to supply a missing credential on a credential-less
/// interactive run, chosen from the empty-offer capture branch. Not a provider
/// catalog -- a fixed choice over the two capture seams `provider add` already
/// implements ([`AddIo::login`] and [`AddIo::prompt_hidden`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialCapture {
    /// Run the oauth login flow for the `anthropic` id.
    OauthLogin,
    /// Enter an API key at the hidden prompt (captured to the managed store).
    ApiKey,
    /// Decline -- fall through to the actionable next-step message.
    Skip,
}

/// One `[models.<nick>]` wiring the final write emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelWiring {
    pub nick: String,
    pub provider: String,
    pub upstream: String,
}

/// The exact command that starts the local server; single-sourced so the
/// wizard's closing hint and the doctor path can never print it differently.
pub const SERVE_COMMAND: &str = "routectl serve";

/// The run-doctor hint the wizard's next-steps block and the doctor path
/// both surface.
pub const DOCTOR_NEXT_HINT: &str =
    "run `routectl doctor` to verify providers, credentials, and routing";

/// Render the closing "next steps" block: the run-doctor hint, the exact
/// serve command, and a sample `curl` against the local server addressing the
/// default alias. Pure text -- it never probes the server or launches
/// anything (init ends at config-written).
pub fn next_steps(host: &str, port: u16, default_alias: &str) -> String {
    let base = format!("http://{host}:{port}");
    let curl = format!(
        "curl {base}/v1/chat/completions \\\n    \
         -H 'content-type: application/json' \\\n    \
         -d '{{\"model\":\"{default_alias}\",\"messages\":\
         [{{\"role\":\"user\",\"content\":\"hello\"}}]}}'"
    );
    format!(
        "next steps:\n  \
         1. {DOCTOR_NEXT_HINT}\n  \
         2. start the local server:\n       {SERVE_COMMAND}\n  \
         3. send a request through it:\n       {curl}\n"
    )
}

/// Production [`InitIo`]. The wizard-only choices read from stdout/stdin with
/// the same confirm pattern the config-mutating commands use; the [`AddIo`]
/// half delegates to [`RealAddIo`] so the credential seams behave identically
/// to `provider add`.
pub struct RealInitIo;

#[async_trait]
impl AddIo for RealInitIo {
    fn stdin_is_terminal(&self) -> bool {
        RealAddIo.stdin_is_terminal()
    }
    fn read_stdin(&self) -> Result<String> {
        RealAddIo.read_stdin()
    }
    fn confirm_env_offer(&self, var: &str) -> bool {
        RealAddIo.confirm_env_offer(var)
    }
    fn prompt_hidden(&self, provider_name: &str) -> Result<String> {
        RealAddIo.prompt_hidden(provider_name)
    }
    async fn login(&self, provider: &str) -> Result<()> {
        RealAddIo.login(provider).await
    }
}

impl InitIo for RealInitIo {
    fn choose_scaffold_or_wizard(&self) -> bool {
        println!("a fresh machine can scaffold a starter config fast, or walk a guided setup.");
        yes_no("use the expert scaffold fast-path? [y/N] ")
    }

    fn select_offers(&self, offers: &[Offer]) -> Vec<usize> {
        if offers.is_empty() {
            return Vec::new();
        }
        println!("detected credentials you can configure:");
        for (i, offer) in offers.iter().enumerate() {
            println!(
                "  {}. {} ({}, {})",
                i + 1,
                offer.provider_name,
                offer.kind,
                offer.credential_class
            );
        }
        let answer = prompt("select providers by number (comma-separated, blank = all): ")
            .unwrap_or_default();
        if answer.is_empty() {
            return (0..offers.len()).collect();
        }
        let mut chosen: Vec<usize> = answer
            .split(',')
            .filter_map(|token| token.trim().parse::<usize>().ok())
            .filter(|n| (1..=offers.len()).contains(n))
            .map(|n| n - 1)
            .collect();
        chosen.sort_unstable();
        chosen.dedup();
        chosen
    }

    fn prompt_model_id(
        &self,
        provider_name: &str,
        kind: &str,
        example_hint: &str,
    ) -> Option<String> {
        let label = format!(
            "upstream model id for `{provider_name}` ({kind}), e.g. {example_hint} \
             (blank to skip): "
        );
        capture_model_id(
            || prompt(&label),
            |reason| eprintln!("  that model id is not usable: {reason}. try again."),
        )
    }

    fn choose_default_route(&self, candidates: &[String]) -> Option<String> {
        match candidates {
            [] => None,
            [only] => {
                // Routing is an explicit, confirmed step even with a single
                // candidate (foundations 5: availability is not routing). A
                // sole provider is NOT silently assigned as the default route.
                println!("`{only}` is the only selected provider.");
                if yes_no(&format!(
                    "set `{only}` as the default route (aliases.default)? [y/N] "
                )) {
                    Some(only.clone())
                } else {
                    None
                }
            }
            _ => {
                println!("which route should be the default (aliases.default)?");
                for (i, candidate) in candidates.iter().enumerate() {
                    println!("  {}. {candidate}", i + 1);
                }
                prompt("default route by number (blank to skip): ")
                    .unwrap_or_default()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=candidates.len()).contains(n))
                    .map(|n| candidates[n - 1].clone())
            }
        }
    }

    fn confirm_wizard_ack(&self) -> bool {
        println!(
            "this writes a new config.toml defining how routectl routes and \
             authenticates traffic."
        );
        yes_no("write it now? [y/N] ")
    }

    fn offer_credential_capture(&self) -> CredentialCapture {
        println!("no credentials detected. set one up now, or skip and configure later:");
        println!("  1. log in to your Anthropic account (oauth)");
        println!("  2. enter an API key");
        println!("  3. skip for now");
        match prompt("choose 1-3 (blank to skip): ")
            .unwrap_or_default()
            .as_str()
        {
            "1" => CredentialCapture::OauthLogin,
            "2" => CredentialCapture::ApiKey,
            _ => CredentialCapture::Skip,
        }
    }
}

/// Run the guided `init` flow against `config_path` with the production I/O
/// seams ([`RealInitIo`]). Thin wrapper over [`run_with_io`].
pub async fn run(config_path: &Path, args: InitArgs) -> Result<()> {
    run_with_io(config_path, args, &RealInitIo).await
}

/// Run the guided `init` flow. Detection is pure (no lock, no writes): it
/// loads the existing config (or the defaults on a fresh machine), probes the
/// local oauth store, and composes the sorted [`Offer`] inventory. The
/// interactive/side-effecting steps flow through `io` so the whole flow is
/// testable without a TTY.
pub async fn run_with_io(config_path: &Path, args: InitArgs, io: &dyn InitIo) -> Result<()> {
    let existing = load_existing(config_path)?;
    let config_exists = existing.is_some();
    let config_for_detect = existing.unwrap_or_default();
    let probes = gather_probes().await;
    let offers = detect::detect_offers(&config_for_detect, &probes);
    orchestrate::orchestrate(
        config_path,
        &args,
        &config_for_detect,
        config_exists,
        offers,
        io,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_steps_lists_the_doctor_hint_serve_command_and_a_curl() {
        let out = next_steps("127.0.0.1", 8787, "gpt");

        assert!(
            out.contains(DOCTOR_NEXT_HINT),
            "run-doctor hint missing: {out}"
        );
        assert!(out.contains(SERVE_COMMAND), "serve command missing: {out}");
        assert!(out.contains("curl"), "curl sample missing: {out}");
        assert!(
            out.contains("http://127.0.0.1:8787/v1/chat/completions"),
            "curl targets the local server: {out}"
        );
        assert!(
            out.contains("\"model\":\"gpt\""),
            "curl addresses the default alias: {out}"
        );
    }

    #[test]
    fn init_args_carry_the_flag_surface() {
        let args = InitArgs {
            scaffold: true,
            yes: false,
            default_model: Some("gpt".into()),
            forwarded: false,
        };
        assert!(args.scaffold);
        assert!(!args.yes);
        assert_eq!(args.default_model.as_deref(), Some("gpt"));
        assert!(!args.forwarded);
    }

    #[test]
    fn model_wiring_and_offer_source_hold_their_fields() {
        let wiring = ModelWiring {
            nick: "gpt".into(),
            provider: "fast".into(),
            upstream: "gpt-4o".into(),
        };
        assert_eq!(wiring.nick, "gpt");
        assert_eq!(wiring.provider, "fast");
        assert_eq!(wiring.upstream, "gpt-4o");
        assert_ne!(OfferSource::Env, OfferSource::Forwarded);
    }

    #[test]
    fn provider_add_kind_maps_oauth_to_the_login_sentinel() {
        let offer = |source| Offer {
            provider_name: "claude".into(),
            kind: "anthropic-api".into(),
            source,
            credential_class: "oauth".into(),
        };
        assert_eq!(offer(OfferSource::Oauth).provider_add_kind(), "anthropic");
        assert_eq!(
            offer(OfferSource::Env).provider_add_kind(),
            "anthropic-api",
            "a non-oauth offer keeps its config kind"
        );
        assert_eq!(
            offer(OfferSource::Forwarded).provider_add_kind(),
            "anthropic-api"
        );
    }

    #[test]
    fn real_init_io_is_a_trait_object_for_both_halves() {
        let real = RealInitIo;
        let _init: &dyn InitIo = &real;
        let _add: &dyn AddIo = &real;
    }
}
