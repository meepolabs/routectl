//! `routectl init` -- the guided first-run setup. This module defines the
//! stable contracts the init flow is built from: the [`InitIo`] interactive
//! seam (a superset of [`AddIo`], so ONE fake drives every step), the
//! [`InitArgs`] flag surface, the leaf data types passed between steps
//! ([`Offer`], [`OfferSource`], [`ModelWiring`]), and the single-sourced
//! next-steps renderer f3's doctor path reuses.
//!
//! The clap `Cmd::Init` arm and the `run`/`run_with_io` orchestration land in
//! a later step; a dispatch arm may not point at an unimplemented run, so the
//! wiring is deferred and this module is the pure-contracts foundation.

pub mod detect;
pub mod plan;
pub mod scaffold;
pub mod write;

use std::collections::BTreeMap;
use std::path::Path;

use routectl_auth::{LocalProbe, OAuthStore};
use routectl_core::{Error, Result};
use routectl_router::{Config, parse_config};

use async_trait::async_trait;

use super::edit_pipeline::preflight;
use super::provider_add::{self, AddIo, RealAddIo};
use plan::{WizardAnswers, WizardPlan, build_plan};
use scaffold::ScaffoldError;
use write::commit_models_aliases;

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
}

/// One `[models.<nick>]` wiring the final write emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelWiring {
    pub nick: String,
    pub provider: String,
    pub upstream: String,
}

/// The exact command that starts the local server; single-sourced so the
/// wizard's closing hint and f3's doctor path can never print it differently.
pub const SERVE_COMMAND: &str = "routectl serve";

/// The run-doctor hint the wizard's next-steps block and f3's doctor path
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
        match prompt(&label) {
            Some(value) if !value.is_empty() => Some(value),
            _ => None,
        }
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
}

/// Print `label`, flush, and read one trimmed line. `None` on a read error
/// (e.g. EOF stdin), so a non-interactive invocation declines rather than
/// hangs -- matching the confirm pattern the mutating commands use.
fn prompt(label: &str) -> Option<String> {
    use std::io::Write as _;
    print!("{label}");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return None;
    }
    Some(input.trim().to_string())
}

/// A `[y/N]` confirmation defaulting to no: only an explicit yes accepts, and
/// a read error / EOF declines.
fn yes_no(label: &str) -> bool {
    let answer = prompt(label).unwrap_or_default().to_ascii_lowercase();
    matches!(answer.as_str(), "y" | "yes")
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
    orchestrate(
        config_path,
        &args,
        &config_for_detect,
        config_exists,
        offers,
        io,
    )
    .await
}

/// The write-ordered core, separated from detection so it drives from an
/// explicit [`Offer`] list and an explicit config-presence flag -- a unit
/// test exercises the full fresh-vs-existing / ack / recovery behavior without
/// touching the real oauth store or process environment.
async fn orchestrate(
    config_path: &Path,
    args: &InitArgs,
    existing_config: &Config,
    config_exists: bool,
    mut offers: Vec<Offer>,
    io: &dyn InitIo,
) -> Result<()> {
    if args.scaffold {
        if config_exists {
            return Err(Error::Config(format!(
                "config already exists at `{}`; `--scaffold` never overwrites it. \
                 re-run `routectl init` without `--scaffold` to walk the existing config",
                config_path.display()
            )));
        }
        return confirmed_scaffold(config_path, args.yes, io);
    }

    if config_exists {
        preflight(&read_config_text(config_path)?)?;
    } else if !args.yes && io.choose_scaffold_or_wizard() {
        return confirmed_scaffold(config_path, args.yes, io);
    }

    if args.forwarded && !offers.iter().any(|o| o.source == OfferSource::Forwarded) {
        offers.push(detect::forwarded_offer());
    }

    let answers = collect_answers(args, &offers, io)?;
    let plan =
        build_plan(&answers, existing_config, &offers).map_err(|e| Error::Config(e.to_string()))?;

    if !args.yes && !io.confirm_wizard_ack() {
        println!("aborted; nothing written.");
        return Ok(());
    }

    apply_plan(config_path, config_exists, plan, existing_config, io).await
}

/// Gather the operator's answers -- from `io` prompts interactively, or from
/// the flags/env under `--yes`. Errors actionably (before any write) when a
/// required value is missing under `--yes`; a missing model id surfaces later
/// through [`build_plan`], also before any side effect.
fn collect_answers(args: &InitArgs, offers: &[Offer], io: &dyn InitIo) -> Result<WizardAnswers> {
    let selected = select_offers(args, offers, io);
    let model_ids = collect_model_ids(args, &selected, io);
    let default_route = choose_default_route(args, &selected, io)?;
    Ok(WizardAnswers {
        selected,
        model_ids,
        default_route,
        yes: args.yes,
    })
}

/// The chosen offers. Under `--yes`, every detected offer is selected, but the
/// egress-shifting forwarded offer is opt-in and included only with
/// `--forwarded`. Interactively, the operator picks from the list; `--forwarded`
/// additionally forces any forwarded offer into the selection.
fn select_offers(args: &InitArgs, offers: &[Offer], io: &dyn InitIo) -> Vec<Offer> {
    if args.yes {
        return offers
            .iter()
            .filter(|o| o.source != OfferSource::Forwarded || args.forwarded)
            .cloned()
            .collect();
    }
    let mut selected: Vec<Offer> = io
        .select_offers(offers)
        .iter()
        .filter_map(|i| offers.get(*i).cloned())
        .collect();
    if args.forwarded {
        for offer in offers.iter().filter(|o| o.source == OfferSource::Forwarded) {
            if !selected.contains(offer) {
                selected.push(offer.clone());
            }
        }
    }
    selected
}

/// The upstream model id for each selected provider: `--default-model` covers
/// every provider non-interactively; otherwise the per-provider prompt asks
/// (with a per-kind example hint). A provider left without an id is absent from
/// the map, and [`build_plan`] turns that into an actionable `MissingModelId`.
fn collect_model_ids(
    args: &InitArgs,
    selected: &[Offer],
    io: &dyn InitIo,
) -> BTreeMap<String, String> {
    let mut ids = BTreeMap::new();
    for offer in selected {
        let id = if let Some(model) = &args.default_model {
            Some(model.clone())
        } else if args.yes {
            None
        } else {
            io.prompt_model_id(
                &offer.provider_name,
                &offer.kind,
                example_hint_for_kind(&offer.kind),
            )
        };
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            ids.insert(offer.provider_name.clone(), id);
        }
    }
    ids
}

/// The provider whose model becomes `aliases.default`. Interactively the
/// operator chooses; under `--yes` the default is the single selected provider
/// when unambiguous -- there is NO implicit first-of-many fallback, so two or
/// more selected providers under `--yes` error actionably before any write.
fn choose_default_route(
    args: &InitArgs,
    selected: &[Offer],
    io: &dyn InitIo,
) -> Result<Option<String>> {
    if args.yes {
        return match selected {
            [] => Ok(None),
            [single] => Ok(Some(single.provider_name.clone())),
            _ => Err(Error::Config(
                "more than one provider selected, but `--yes` cannot pick a default route \
                 (there is no implicit first-provider fallback); re-run without `--yes` to \
                 choose interactively, or select a single provider"
                    .into(),
            )),
        };
    }
    let names: Vec<String> = selected.iter().map(|o| o.provider_name.clone()).collect();
    Ok(io.choose_default_route(&names))
}

/// Execute the confirmed plan in the write order the decision fixes: seed a
/// base config on a fresh machine, compose each provider via `provider add`
/// (each atomic + gated + idempotent), then the ONE final models/aliases
/// write where routing lands. A failure at any step is wrapped with the
/// explicit recovery message -- config on disk stays valid, and re-running
/// init completes the setup.
async fn apply_plan(
    config_path: &Path,
    config_exists: bool,
    plan: WizardPlan,
    existing_config: &Config,
    io: &dyn InitIo,
) -> Result<()> {
    if !config_exists {
        match scaffold::scaffold_seed(config_path) {
            Ok(()) | Err(ScaffoldError::AlreadyExists) => {}
            Err(e) => return Err(recovery_error(Error::Config(e.to_string()))),
        }
    }

    for provider_arg in plan.provider_args {
        provider_add::run_with_io(config_path, provider_arg, io as &dyn AddIo)
            .await
            .map_err(recovery_error)?;
    }

    let snapshot = std::fs::read(config_path).map_err(|e| {
        recovery_error(Error::Config(format!(
            "read config after wiring providers: {e}"
        )))
    })?;
    let snapshot_text = String::from_utf8(snapshot.clone())
        .map_err(|e| recovery_error(Error::Config(format!("config is not UTF-8: {e}"))))?;
    commit_models_aliases(
        config_path,
        &snapshot,
        &snapshot_text,
        &plan.models,
        &plan.default_alias,
    )
    .map_err(recovery_error)?;

    println!(
        "routing configured; default route -> `{}`.",
        plan.default_alias
    );
    print!(
        "{}",
        next_steps(
            &existing_config.server.host,
            existing_config.server.port,
            &plan.default_alias,
        )
    );
    Ok(())
}

/// Confirm before the scaffold write, then drop the starter config. The
/// scaffolded file lands at the live config path where a running daemon may
/// pick it up immediately, so it rides the SAME pre-write high-consequence
/// ack the wizard uses; `--yes` is the only bypass. A declined ack writes
/// nothing.
fn confirmed_scaffold(config_path: &Path, yes: bool, io: &dyn InitIo) -> Result<()> {
    if !yes && !io.confirm_wizard_ack() {
        println!("aborted; nothing written.");
        return Ok(());
    }
    scaffold_path(config_path)
}

/// The `--scaffold` fast-path (and the interactive "expert scaffold" choice):
/// drop the committed starter config, then print the closing next steps
/// addressing its default alias.
fn scaffold_path(config_path: &Path) -> Result<()> {
    scaffold::scaffold_fresh(config_path).map_err(|e| Error::Config(e.to_string()))?;

    let cfg = parse_config(&read_config_text(config_path)?)
        .map_err(|e| Error::Config(format!("scaffolded config does not parse: {e}")))?;
    let default_alias = cfg
        .aliases
        .get("default")
        .and_then(|value| value.nicknames().next())
        .unwrap_or("default");

    println!("wrote starter config to `{}`.", config_path.display());
    print!(
        "{}",
        next_steps(&cfg.server.host, cfg.server.port, default_alias)
    );
    Ok(())
}

/// Load the config at `config_path` for detection, or `None` when the file is
/// absent (the fresh-machine path). A present-but-unparseable config is a hard
/// error -- init edits it surgically and cannot reason about a broken file.
fn load_existing(config_path: &Path) -> Result<Option<Config>> {
    match std::fs::read_to_string(config_path) {
        Ok(text) => {
            let config = parse_config(&text).map_err(|e| {
                Error::Config(format!(
                    "current config `{}` does not parse; fix it before running init: {e}",
                    config_path.display()
                ))
            })?;
            Ok(Some(config))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Config(format!(
            "cannot read config `{}`: {e}",
            config_path.display()
        ))),
    }
}

fn read_config_text(config_path: &Path) -> Result<String> {
    std::fs::read_to_string(config_path).map_err(|e| {
        Error::Config(format!(
            "cannot read config `{}`: {e}",
            config_path.display()
        ))
    })
}

/// Local-only probe of every routectl-owned oauth provider id, mirroring the
/// server's activation path. When the store cannot be opened every id yields
/// [`LocalProbe::StoreUnavailable`], so detection degrades to "no oauth offers"
/// rather than failing.
async fn gather_probes() -> Vec<(&'static str, LocalProbe)> {
    let ids = routectl_auth::oauth::known_provider_ids();
    match OAuthStore::open_default().await {
        Ok(store) => {
            let mut probes = Vec::with_capacity(ids.len());
            for id in ids {
                probes.push((*id, store.probe_local(id).await));
            }
            probes
        }
        Err(_) => ids
            .iter()
            .map(|id| (*id, LocalProbe::StoreUnavailable))
            .collect(),
    }
}

/// Wrap a post-ack failure with the f1-style explicit recovery message: the
/// on-disk config is valid, any providers/credentials already written persist
/// and are reused on re-run (ref paths are deterministic), so re-running init
/// completes the setup. No rollback engine, no secret auto-delete.
fn recovery_error(err: Error) -> Error {
    Error::Config(format!(
        "init did not complete ({err}). the config on disk is valid; any providers \
         already added and any credentials already captured persist and are reused \
         on re-run -- re-run `routectl init` to complete the setup"
    ))
}

/// A representative upstream model id for a provider kind, shown as a prompt
/// hint (there is no catalog directory to pick from -- the operator types the
/// id). Purely illustrative; any real id the provider serves is accepted.
fn example_hint_for_kind(kind: &str) -> &'static str {
    match kind {
        "anthropic-api" => "claude-sonnet-4-5",
        "openai-compat" => "gpt-4o",
        "openai-responses" => "gpt-5",
        "gemini" => "gemini-2.5-pro",
        _ => "the provider's upstream model id",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configurable [`InitIo`] fake: no real TTY, stdin, prompt, or browser.
    /// Drives every wizard seam AND the inherited [`AddIo`] credential seams
    /// from one value; downstream unit tests reuse this pattern. Interior
    /// counters use `Mutex` so the fake stays `Send + Sync` for the async
    /// `login` seam while recording whether a secret prompt/login ever fired.
    struct FakeInitIo {
        scaffold: bool,
        offer_selection: Vec<usize>,
        model_id: Option<String>,
        default_route: Option<String>,
        ack: bool,
        is_tty: bool,
        stdin_value: String,
        offer_env: bool,
        prompt_value: String,
        login_ok: bool,
        login_calls: std::sync::Mutex<u32>,
        prompt_hidden_calls: std::sync::Mutex<u32>,
    }

    impl Default for FakeInitIo {
        fn default() -> Self {
            Self {
                scaffold: false,
                offer_selection: Vec::new(),
                model_id: None,
                default_route: None,
                ack: true,
                is_tty: false,
                stdin_value: String::new(),
                offer_env: false,
                prompt_value: String::new(),
                login_ok: true,
                login_calls: std::sync::Mutex::new(0),
                prompt_hidden_calls: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl AddIo for FakeInitIo {
        fn stdin_is_terminal(&self) -> bool {
            self.is_tty
        }
        fn read_stdin(&self) -> Result<String> {
            Ok(self.stdin_value.clone())
        }
        fn confirm_env_offer(&self, _var: &str) -> bool {
            self.offer_env
        }
        fn prompt_hidden(&self, _provider_name: &str) -> Result<String> {
            *self.prompt_hidden_calls.lock().unwrap() += 1;
            Ok(self.prompt_value.clone())
        }
        async fn login(&self, _provider: &str) -> Result<()> {
            *self.login_calls.lock().unwrap() += 1;
            if self.login_ok {
                Ok(())
            } else {
                Err(routectl_core::Error::Auth("login failed".into()))
            }
        }
    }

    impl InitIo for FakeInitIo {
        fn choose_scaffold_or_wizard(&self) -> bool {
            self.scaffold
        }
        fn select_offers(&self, _offers: &[Offer]) -> Vec<usize> {
            self.offer_selection.clone()
        }
        fn prompt_model_id(&self, _p: &str, _k: &str, _h: &str) -> Option<String> {
            self.model_id.clone()
        }
        fn choose_default_route(&self, _candidates: &[String]) -> Option<String> {
            self.default_route.clone()
        }
        fn confirm_wizard_ack(&self) -> bool {
            self.ack
        }
    }

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

    #[tokio::test]
    async fn fake_init_io_drives_every_wizard_and_credential_seam() {
        let fake = FakeInitIo {
            scaffold: true,
            offer_selection: vec![0, 2],
            model_id: Some("claude-sonnet-4-5".to_string()),
            default_route: Some("main".to_string()),
            ack: false,
            ..Default::default()
        };

        let io: &dyn InitIo = &fake;
        assert!(io.choose_scaffold_or_wizard());
        let offers = vec![Offer {
            provider_name: "a".into(),
            kind: "anthropic-api".into(),
            source: OfferSource::Oauth,
            credential_class: "oauth".into(),
        }];
        assert_eq!(io.select_offers(&offers), vec![0, 2]);
        assert_eq!(
            io.prompt_model_id("a", "anthropic-api", "claude-sonnet-4-5"),
            Some("claude-sonnet-4-5".to_string())
        );
        assert_eq!(
            io.choose_default_route(&["main".to_string()]),
            Some("main".to_string())
        );
        assert!(!io.confirm_wizard_ack());

        // The inherited AddIo half resolves through the same fake.
        let add: &dyn AddIo = &fake;
        assert!(!add.stdin_is_terminal());
        add.login("anthropic").await.expect("default login ok");
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

    // -----------------------------------------------------------------
    // Orchestration: the write-ordered core driven by FakeInitIo. The
    // forwarded offer is the deterministic vehicle -- it wires with no
    // secret, no login, and no managed-store touch, so these tests need
    // neither the process environment nor a global oauth store.
    // -----------------------------------------------------------------

    fn forwarded_test_offer() -> Offer {
        Offer {
            provider_name: "anthropic-forwarded".to_string(),
            kind: "anthropic-api".to_string(),
            source: OfferSource::Forwarded,
            credential_class: "forwarded".to_string(),
        }
    }

    fn init_args(
        scaffold: bool,
        yes: bool,
        default_model: Option<&str>,
        forwarded: bool,
    ) -> InitArgs {
        InitArgs {
            scaffold,
            yes,
            default_model: default_model.map(str::to_string),
            forwarded,
        }
    }

    fn default_alias_of(cfg: &Config) -> Option<String> {
        cfg.aliases
            .get("default")
            .and_then(|v| v.nicknames().next())
            .map(str::to_string)
    }

    #[tokio::test]
    async fn fresh_wizard_composes_providers_then_the_one_routing_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo {
            offer_selection: vec![0],
            model_id: Some("claude-sonnet-4-5".to_string()),
            default_route: Some("anthropic-forwarded".to_string()),
            ..Default::default()
        };
        let args = init_args(false, false, None, false);

        orchestrate(
            &path,
            &args,
            &Config::default(),
            false,
            vec![forwarded_test_offer()],
            &fake,
        )
        .await
        .expect("fresh wizard writes a routed config");

        let cfg =
            parse_config(&std::fs::read_to_string(&path).unwrap()).expect("routed config parses");
        assert!(
            cfg.providers.contains_key("anthropic-forwarded"),
            "provider wired"
        );
        assert!(
            cfg.models
                .values()
                .any(|m| m.provider == "anthropic-forwarded"),
            "a model targets the wired provider"
        );
        assert_eq!(
            default_alias_of(&cfg).as_deref(),
            Some("anthropic-forwarded"),
            "the default route lands in aliases.default"
        );
    }

    #[tokio::test]
    async fn yes_flag_path_produces_the_same_config_as_the_wizard() {
        let wizard_dir = tempfile::tempdir().unwrap();
        let wizard_path = wizard_dir.path().join("config.toml");
        let wizard_io = FakeInitIo {
            offer_selection: vec![0],
            model_id: Some("claude-sonnet-4-5".to_string()),
            default_route: Some("anthropic-forwarded".to_string()),
            ..Default::default()
        };
        orchestrate(
            &wizard_path,
            &init_args(false, false, None, false),
            &Config::default(),
            false,
            vec![forwarded_test_offer()],
            &wizard_io,
        )
        .await
        .expect("wizard run");

        let yes_dir = tempfile::tempdir().unwrap();
        let yes_path = yes_dir.path().join("config.toml");
        orchestrate(
            &yes_path,
            &init_args(false, true, Some("claude-sonnet-4-5"), true),
            &Config::default(),
            false,
            Vec::new(),
            &FakeInitIo::default(),
        )
        .await
        .expect("--yes run");

        assert_eq!(
            std::fs::read(&wizard_path).unwrap(),
            std::fs::read(&yes_path).unwrap(),
            "the --yes flag path produces a byte-identical config"
        );
    }

    #[tokio::test]
    async fn declining_the_ack_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo {
            offer_selection: vec![0],
            model_id: Some("claude-sonnet-4-5".to_string()),
            default_route: Some("anthropic-forwarded".to_string()),
            ack: false,
            ..Default::default()
        };

        orchestrate(
            &path,
            &init_args(false, false, None, false),
            &Config::default(),
            false,
            vec![forwarded_test_offer()],
            &fake,
        )
        .await
        .expect("declining is not an error");

        assert!(!path.exists(), "a declined ack writes nothing at all");
    }

    #[tokio::test]
    async fn missing_model_id_errors_before_any_side_effect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // --yes with no --default-model: the model id is missing and cannot be
        // prompted. The error must precede the seed / any write.
        let err = orchestrate(
            &path,
            &init_args(false, true, None, true),
            &Config::default(),
            false,
            Vec::new(),
            &FakeInitIo::default(),
        )
        .await
        .expect_err("a missing model id must error");

        assert!(
            err.to_string().contains("--default-model"),
            "the error is actionable: {err}"
        );
        assert!(
            !path.exists(),
            "no partial write before the actionable error"
        );
    }

    #[tokio::test]
    async fn multiple_providers_under_yes_error_on_the_ambiguous_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let offers = vec![
            Offer {
                provider_name: "a".to_string(),
                kind: "anthropic-api".to_string(),
                source: OfferSource::Env,
                credential_class: "env".to_string(),
            },
            Offer {
                provider_name: "b".to_string(),
                kind: "anthropic-api".to_string(),
                source: OfferSource::Env,
                credential_class: "env".to_string(),
            },
        ];

        let err = orchestrate(
            &path,
            &init_args(false, true, Some("claude-sonnet-4-5"), false),
            &Config::default(),
            false,
            offers,
            &FakeInitIo::default(),
        )
        .await
        .expect_err("an ambiguous default under --yes must error");

        assert!(err.to_string().contains("default route"), "err: {err}");
        assert!(!path.exists(), "no write on the ambiguous-default error");
    }

    #[tokio::test]
    async fn forwarded_selection_wires_without_a_secret_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo::default();

        orchestrate(
            &path,
            &init_args(false, true, Some("claude-sonnet-4-5"), true),
            &Config::default(),
            false,
            Vec::new(),
            &fake,
        )
        .await
        .expect("forwarded wiring");

        assert_eq!(
            *fake.login_calls.lock().unwrap(),
            0,
            "forwarded runs no login"
        );
        assert_eq!(
            *fake.prompt_hidden_calls.lock().unwrap(),
            0,
            "forwarded prompts for no secret"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("credential_source = \"forwarded\""), "{text}");
        assert!(text.contains("[providers.anthropic-forwarded]"), "{text}");
    }

    #[tokio::test]
    async fn final_write_failure_reports_recovery_and_leaves_a_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // A plan whose model targets a provider no `provider add` step wires:
        // the seed lands, the provider loop is empty, and the final
        // models/aliases write fails the shared gate.
        let plan = WizardPlan {
            provider_args: Vec::new(),
            models: vec![ModelWiring {
                nick: "m".to_string(),
                provider: "ghost".to_string(),
                upstream: "x".to_string(),
            }],
            default_alias: "m".to_string(),
        };

        let err = apply_plan(
            &path,
            false,
            plan,
            &Config::default(),
            &FakeInitIo::default(),
        )
        .await
        .expect_err("the final write must fail");

        let msg = err.to_string();
        assert!(msg.contains("persist"), "recovery notes persistence: {msg}");
        assert!(msg.contains("re-run"), "recovery says re-run: {msg}");
        parse_config(&std::fs::read_to_string(&path).unwrap())
            .expect("the seed config left on disk is still valid");
    }

    #[tokio::test]
    async fn existing_config_rerun_is_a_byte_identical_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        orchestrate(
            &path,
            &init_args(false, true, Some("claude-sonnet-4-5"), true),
            &Config::default(),
            false,
            Vec::new(),
            &FakeInitIo::default(),
        )
        .await
        .expect("first run");
        let after_first = std::fs::read(&path).unwrap();

        let existing = parse_config(&String::from_utf8(after_first.clone()).unwrap()).unwrap();
        orchestrate(
            &path,
            &init_args(false, true, Some("claude-sonnet-4-5"), true),
            &existing,
            true,
            Vec::new(),
            &FakeInitIo::default(),
        )
        .await
        .expect("re-run walk-through");

        assert_eq!(
            std::fs::read(&path).unwrap(),
            after_first,
            "re-running init on a complete config is a byte-identical no-op"
        );
    }

    #[tokio::test]
    async fn scaffold_refuses_an_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let existing = "version = 3\n[server]\nhost = \"10.0.0.1\"\n";
        std::fs::write(&path, existing).unwrap();

        let err = orchestrate(
            &path,
            &init_args(true, false, None, false),
            &Config::default(),
            true,
            Vec::new(),
            &FakeInitIo::default(),
        )
        .await
        .expect_err("scaffold must refuse an existing config");

        assert!(err.to_string().contains("already exists"), "err: {err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            existing,
            "the existing config is left byte-identical"
        );
    }

    #[tokio::test]
    async fn scaffold_flag_writes_a_valid_starter_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        orchestrate(
            &path,
            &init_args(true, false, None, false),
            &Config::default(),
            false,
            Vec::new(),
            &FakeInitIo::default(),
        )
        .await
        .expect("scaffold fast-path writes");

        let cfg = parse_config(&std::fs::read_to_string(&path).unwrap()).expect("starter parses");
        assert!(!cfg.providers.is_empty(), "starter carries providers");
        assert!(!cfg.models.is_empty(), "starter carries models");
    }

    #[tokio::test]
    async fn interactive_scaffold_choice_takes_the_fast_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo {
            scaffold: true,
            ..Default::default()
        };

        orchestrate(
            &path,
            &init_args(false, false, None, false),
            &Config::default(),
            false,
            Vec::new(),
            &fake,
        )
        .await
        .expect("scaffold via the interactive choice");

        let cfg = parse_config(&std::fs::read_to_string(&path).unwrap()).expect("starter parses");
        assert!(
            !cfg.providers.is_empty(),
            "choosing the fast path drops the starter config"
        );
    }

    #[tokio::test]
    async fn existing_stale_version_is_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let stale = "version = 99\n[server]\nhost = \"127.0.0.1\"\n";
        std::fs::write(&path, stale).unwrap();

        let err = orchestrate(
            &path,
            &init_args(false, true, Some("claude-sonnet-4-5"), true),
            &Config::default(),
            true,
            Vec::new(),
            &FakeInitIo::default(),
        )
        .await
        .expect_err("a stale-version config must be refused");

        assert!(!err.to_string().is_empty(), "err: {err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            stale,
            "a refused preflight leaves the file byte-identical"
        );
    }

    #[tokio::test]
    async fn scaffold_declining_the_ack_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo {
            ack: false,
            ..Default::default()
        };

        orchestrate(
            &path,
            &init_args(true, false, None, false),
            &Config::default(),
            false,
            Vec::new(),
            &fake,
        )
        .await
        .expect("declining the scaffold ack is not an error");

        assert!(
            !path.exists(),
            "a scaffold whose ack is declined writes nothing at the live config path"
        );
    }

    #[tokio::test]
    async fn scaffold_yes_bypasses_the_ack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // ack would decline, but `--yes` is the sanctioned non-interactive
        // bypass, so the scaffold still lands.
        let fake = FakeInitIo {
            ack: false,
            ..Default::default()
        };

        orchestrate(
            &path,
            &init_args(true, true, None, false),
            &Config::default(),
            false,
            Vec::new(),
            &fake,
        )
        .await
        .expect("--yes bypasses the scaffold ack");

        parse_config(&std::fs::read_to_string(&path).unwrap())
            .expect("starter written under --yes");
    }

    #[tokio::test]
    async fn interactive_single_provider_routes_through_choose_default_route() {
        // Foundations 5: routing is a confirmed step even with a sole
        // candidate. When choose_default_route yields None (the operator did
        // not confirm), nothing is auto-assigned -- the flow errors rather
        // than silently routing the single provider.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo {
            offer_selection: vec![0],
            model_id: Some("claude-sonnet-4-5".to_string()),
            default_route: None,
            ..Default::default()
        };

        let err = orchestrate(
            &path,
            &init_args(false, false, None, false),
            &Config::default(),
            false,
            vec![forwarded_test_offer()],
            &fake,
        )
        .await
        .expect_err("an unconfirmed default route must not auto-assign");

        assert!(err.to_string().contains("default route"), "err: {err}");
        assert!(
            !path.exists(),
            "no write when the single-provider default route is not confirmed"
        );
    }

    #[tokio::test]
    async fn anthropic_oauth_offer_still_wires_end_to_end() {
        // The finding-1 restriction removes only the MISROUTING non-anthropic
        // oauth offers; the supported anthropic oauth offer must still compose
        // through the `anthropic` sentinel into an `oauth://anthropic` ref.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let offer = Offer {
            provider_name: "anthropic".to_string(),
            kind: "anthropic-api".to_string(),
            source: OfferSource::Oauth,
            credential_class: "oauth".to_string(),
        };
        let fake = FakeInitIo {
            offer_selection: vec![0],
            model_id: Some("claude-sonnet-4-5".to_string()),
            default_route: Some("anthropic".to_string()),
            ..Default::default()
        };

        orchestrate(
            &path,
            &init_args(false, false, None, false),
            &Config::default(),
            false,
            vec![offer],
            &fake,
        )
        .await
        .expect("the anthropic oauth offer wires end to end");

        assert_eq!(
            *fake.login_calls.lock().unwrap(),
            1,
            "the oauth offer delegates to the login flow exactly once"
        );
        let cfg = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = cfg.providers.get("anthropic").expect("provider wired");
        assert_eq!(entry.kind_str(), "anthropic-api");
        assert_eq!(entry.api_key_ref(), Some("oauth://anthropic"));
        assert_eq!(
            default_alias_of(&cfg).as_deref(),
            Some("anthropic"),
            "the oauth provider is the confirmed default route"
        );
    }
}
