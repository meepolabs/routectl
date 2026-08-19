//! The write-ordered `init` orchestration: the wizard flow that drives from a
//! detected [`Offer`] inventory to the committed config, split out from the
//! contracts in [`super`] (the [`InitIo`] seam, [`InitArgs`], the leaf types,
//! and the next-steps renderer). Everything here is the interactive/
//! side-effecting core -- answer collection, plan application, the empty-offer
//! credential capture branch, and the terminal-IO helpers -- exercised through
//! the [`InitIo`] seam so the whole flow is testable without a TTY.

use std::collections::BTreeMap;
use std::path::Path;

use routectl_auth::{LocalProbe, OAuthStore};
use routectl_core::{Error, Result};
use routectl_router::{Config, parse_config};

use super::plan::{WizardAnswers, WizardPlan, build_plan};
use super::scaffold::{self, ScaffoldError};
use super::write::commit_models_aliases;
use super::{CredentialCapture, InitArgs, InitIo, Offer, OfferSource, detect, next_steps};
use crate::commands::edit_pipeline::preflight;
use crate::commands::provider_add::{self, AddIo};
use crate::commands::provider_env::env_var_for_kind;

/// Print `label`, flush, and read one trimmed line. `None` on a read error
/// (e.g. EOF stdin), so a non-interactive invocation declines rather than
/// hangs -- matching the confirm pattern the mutating commands use.
pub(super) fn prompt(label: &str) -> Option<String> {
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
pub(super) fn yes_no(label: &str) -> bool {
    let answer = prompt(label).unwrap_or_default().to_ascii_lowercase();
    matches!(answer.as_str(), "y" | "yes")
}

/// The write-ordered core, separated from detection so it drives from an
/// explicit [`Offer`] list and an explicit config-presence flag -- a unit
/// test exercises the full fresh-vs-existing / ack / recovery behavior without
/// touching the real oauth store or process environment.
pub(super) async fn orchestrate(
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

    if offers.is_empty() {
        match capture_missing_credential(args, io) {
            Some(offer) => offers.push(offer),
            None => {
                print!("{}", missing_credential_next_steps());
                return Ok(());
            }
        }
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

/// The empty-offer capture branch: on a credential-less interactive run,
/// offer to set up a provider now (oauth login or a hidden api-key prompt) and
/// synthesize the matching [`Offer`] so the normal plan pipeline wires it
/// through the SAME `provider add` seams a detected offer uses. Returns `None`
/// when the run is non-interactive (`--yes`) or the operator declines -- the
/// caller then prints the actionable next step instead of dead-ending at the
/// missing route. No provider-catalog UI and no probe: a fixed two-choice
/// branch over the one oauth id / api-key kind `provider add` can build.
fn capture_missing_credential(args: &InitArgs, io: &dyn InitIo) -> Option<Offer> {
    if args.yes {
        return None;
    }
    match io.offer_credential_capture() {
        CredentialCapture::OauthLogin => Some(oauth_capture_offer()),
        CredentialCapture::ApiKey => Some(api_key_capture_offer()),
        CredentialCapture::Skip => None,
    }
}

/// The synthesized oauth offer for the empty-offer capture branch: the
/// `anthropic` login id (the sole id `provider add` builds an oauth block for),
/// wired post-confirm through `io.login` exactly as a detected oauth offer is.
fn oauth_capture_offer() -> Offer {
    Offer {
        provider_name: "anthropic".to_string(),
        kind: "anthropic-api".to_string(),
        source: OfferSource::Oauth,
        credential_class: "oauth".to_string(),
    }
}

/// The synthesized api-key offer for the empty-offer capture branch: an
/// `anthropic-api` provider whose credential is captured at `provider add`'s
/// existing hidden prompt (no env var, no forwarded source), landing in the
/// managed secret store as a `file://` ref.
fn api_key_capture_offer() -> Offer {
    Offer {
        provider_name: "anthropic".to_string(),
        kind: "anthropic-api".to_string(),
        source: OfferSource::ApiKeyPrompt,
        credential_class: "api-key".to_string(),
    }
}

/// The actionable next step shown when a credential-less run cannot capture one
/// (non-interactive, or the operator declined): the two supported setup paths
/// and the re-run hint. Deliberately NEVER the raw `MissingDefaultRoute` -- a
/// fresh machine has nothing to route, which is a setup gap to guide through,
/// not a routing error to surface.
fn missing_credential_next_steps() -> String {
    let env_var = env_var_for_kind("anthropic-api").unwrap_or("ANTHROPIC_API_KEY");
    format!(
        "no credentials detected, so there is nothing to route yet.\n\
         set up a provider, then re-run `routectl init`:\n  \
         - log in to your Anthropic account:      routectl login anthropic\n  \
         - or set an API key in the environment:  export {env_var}=<your-key>\n"
    )
}

/// Gather the operator's answers -- from `io` prompts interactively, or from
/// the flags/env under `--yes`. Errors actionably (before any write) when a
/// required value is missing under `--yes`; a missing model id surfaces later
/// through [`build_plan`], also before any side effect.
fn collect_answers(args: &InitArgs, offers: &[Offer], io: &dyn InitIo) -> Result<WizardAnswers> {
    let selected = select_offers(args, offers, io);
    let model_ids = collect_model_ids(args, &selected, io)?;
    let default_route = choose_default_route(args, &selected, io)?;
    Ok(WizardAnswers {
        selected,
        model_ids,
        default_route,
        yes: args.yes,
        probe: args.probe,
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
///
/// Every captured id is validated at this boundary ([`validate_model_id`]): an
/// unusable id from `--default-model` (including an explicit empty value) fails
/// here with an actionable message before any write, and the interactive prompt
/// has already re-prompted past unusable entries, so a doctor-clean config can
/// no longer carry a model id the first real request would reject. A blank
/// interactive entry stays the documented "blank to skip" decline (it leaves
/// the provider absent, which `build_plan` reports before any write); only an
/// explicit `--default-model ""` is treated as an empty value to reject.
fn collect_model_ids(
    args: &InitArgs,
    selected: &[Offer],
    io: &dyn InitIo,
) -> Result<BTreeMap<String, String>> {
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
        let Some(id) = id else { continue };
        if id.is_empty() && args.default_model.is_none() {
            continue;
        }
        validate_model_id(&id).map_err(|reason| {
            Error::Config(format!(
                "model id `{id}` for provider `{}` is not usable: {reason}. \
                 supply a plain upstream model id, e.g. {}",
                offer.provider_name,
                example_hint_for_kind(&offer.kind),
            ))
        })?;
        ids.insert(offer.provider_name.clone(), id);
    }
    Ok(ids)
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

    // The operator's probe choice rides on every provider arg (build_plan
    // threads `answers.probe` onto each); capture it before the loop consumes
    // them, and record which providers this run actually wrote so the
    // post-routing offer never touches a pre-existing, unchanged block.
    let probe = plan.provider_args.first().and_then(|arg| arg.probe);
    let mut written: Vec<String> = Vec::new();
    for provider_arg in plan.provider_args {
        let name = provider_arg.name.clone();
        let outcome = provider_add::run_with_io(config_path, provider_arg, io as &dyn AddIo)
            .await
            .map_err(recovery_error)?;
        if outcome == provider_add::AddResult::Written {
            written.push(name);
        }
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

    // Routing has now landed, so each provider written THIS run resolves to a
    // lane. Offer the scoped probe here for the providers whose lane did NOT
    // exist during the in-loop `provider add` hook (the fresh-onboarding case
    // -- models were written only just above). A provider that ALREADY had a
    // selectable model before this run was eligible for the in-loop offer, so
    // it is skipped here: the pair yields at most one offer per provider.
    for name in &written {
        let had_lane_before = existing_config
            .models
            .values()
            .any(|model| model.selectable && model.provider == *name);
        if had_lane_before {
            continue;
        }
        offer_post_routing_probe(config_path, name, probe, io as &dyn AddIo).await;
    }

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

/// The post-routing capability-probe offer for a provider `init` added THIS
/// run, scoped to its now-routable lane. Same semantics as the `provider add`
/// hook: `--no-probe` suppresses entirely, `--probe` dispatches without
/// prompting, and the interactive default asks `confirm_probe` after the cost
/// line. The probe writes only the capability ledger, never config. The heavy
/// probe future is boxed so it stays off `apply_plan`'s own future.
async fn offer_post_routing_probe(
    config_path: &Path,
    provider: &str,
    probe: Option<bool>,
    io: &dyn AddIo,
) {
    if probe == Some(false) {
        return;
    }
    let force = probe == Some(true);
    Box::pin(crate::commands::probe::capabilities::offer_scoped_probe(
        config_path,
        provider,
        |_estimate| force || io.confirm_probe(),
    ))
    .await;
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
pub(super) fn load_existing(config_path: &Path) -> Result<Option<Config>> {
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
pub(super) async fn gather_probes() -> Vec<(&'static str, LocalProbe)> {
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

/// Wrap a post-ack failure with an explicit recovery message: the
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

/// Conservatively reject a model id that a config write or the first real
/// request would choke on. This is a shape check at the capture boundary, NOT
/// a catalog or network lookup: any id the provider actually serves passes.
/// Rejects the empty / all-whitespace id, any embedded whitespace, control
/// characters, and the quote / backslash characters that break a TOML string
/// value. On rejection the `Err` carries a short, operator-facing reason.
fn validate_model_id(id: &str) -> std::result::Result<(), &'static str> {
    if id.trim().is_empty() {
        return Err("it is empty");
    }
    for ch in id.chars() {
        if ch.is_whitespace() {
            return Err("it contains whitespace");
        }
        if ch.is_control() {
            return Err("it contains a control character");
        }
        if matches!(ch, '"' | '\'' | '\\') {
            return Err("it contains a quote or backslash");
        }
    }
    Ok(())
}

/// The interactive model-id capture loop shared by [`super::RealInitIo::prompt_model_id`].
/// `read` yields the next entry (already trimmed); `None` (EOF / read error)
/// or a blank line skips (returns `None`). An entry [`validate_model_id`]
/// rejects is surfaced through `warn` and the loop reads again, so an unusable
/// id never leaves this boundary -- every returned `Some` is a validated id.
pub(super) fn capture_model_id(
    mut read: impl FnMut() -> Option<String>,
    mut warn: impl FnMut(&str),
) -> Option<String> {
    loop {
        let entry = read()?;
        if entry.is_empty() {
            return None;
        }
        match validate_model_id(&entry) {
            Ok(()) => return Some(entry),
            Err(reason) => warn(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::init::ModelWiring;
    use async_trait::async_trait;
    use routectl_router::CURRENT_CONFIG_VERSION;

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
        confirm_probe: bool,
        credential_capture: CredentialCapture,
        login_calls: std::sync::Mutex<u32>,
        prompt_hidden_calls: std::sync::Mutex<u32>,
        confirm_probe_calls: std::sync::Mutex<u32>,
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
                confirm_probe: false,
                credential_capture: CredentialCapture::Skip,
                login_calls: std::sync::Mutex::new(0),
                prompt_hidden_calls: std::sync::Mutex::new(0),
                confirm_probe_calls: std::sync::Mutex::new(0),
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
        fn confirm_probe(&self) -> bool {
            *self.confirm_probe_calls.lock().unwrap() += 1;
            self.confirm_probe
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
        fn offer_credential_capture(&self) -> CredentialCapture {
            self.credential_capture
        }
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
        // Existing wizard tests suppress the post-add probe offer so they stay
        // hermetic (no overlay/store/ledger access); the probe-specific tests
        // opt in via `init_args_probe`.
        init_args_probe(scaffold, yes, default_model, forwarded, Some(false))
    }

    fn init_args_probe(
        scaffold: bool,
        yes: bool,
        default_model: Option<&str>,
        forwarded: bool,
        probe: Option<bool>,
    ) -> InitArgs {
        InitArgs {
            scaffold,
            yes,
            default_model: default_model.map(str::to_string),
            forwarded,
            probe,
        }
    }

    fn default_alias_of(cfg: &Config) -> Option<String> {
        cfg.aliases
            .get("default")
            .and_then(|v| v.nicknames().next())
            .map(str::to_string)
    }

    // -----------------------------------------------------------------
    // Post-routing capability-probe offer. On a fresh init the lane does
    // not exist until the final models/aliases write, so the in-loop
    // `provider add` hook cannot scope; `apply_plan` re-offers after routing
    // lands, scoped to each provider written THIS run. A provider whose lane
    // pre-existed (re-init overwrite) is offered in-loop and skipped
    // post-routing, so the pair fires at most once per provider per run.
    // These tests count `confirm_probe` calls as the offer signal (the fake
    // declines, so nothing dispatches -- no store/ledger access needed).
    // -----------------------------------------------------------------

    #[tokio::test]
    #[serial_test::serial]
    async fn fresh_init_offers_the_probe_post_routing_for_the_added_provider() {
        let xdg = tempfile::tempdir().unwrap();
        let _env = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo {
            offer_selection: vec![0],
            model_id: Some("claude-sonnet-4-5".to_string()),
            default_route: Some("anthropic-forwarded".to_string()),
            confirm_probe: false, // the offer fires but declines, so nothing dispatches
            ..Default::default()
        };

        // probe = None (interactive): the post-routing offer asks confirm_probe.
        orchestrate(
            &path,
            &init_args_probe(false, false, None, false, None),
            &Config::default(),
            false,
            vec![forwarded_test_offer()],
            &fake,
        )
        .await
        .expect("fresh wizard writes a routed config");

        assert_eq!(
            *fake.confirm_probe_calls.lock().unwrap(),
            1,
            "the offer fires exactly once, post-routing, for the single added provider"
        );
        let cfg = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            cfg.providers.contains_key("anthropic-forwarded"),
            "the offer never rolls the added provider back"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fresh_init_with_no_probe_offers_nothing() {
        let xdg = tempfile::tempdir().unwrap();
        let _env = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo {
            offer_selection: vec![0],
            model_id: Some("claude-sonnet-4-5".to_string()),
            default_route: Some("anthropic-forwarded".to_string()),
            confirm_probe: true, // would consent IF asked -- it must not be asked
            ..Default::default()
        };

        orchestrate(
            &path,
            &init_args_probe(false, false, None, false, Some(false)),
            &Config::default(),
            false,
            vec![forwarded_test_offer()],
            &fake,
        )
        .await
        .expect("fresh wizard writes a routed config");

        assert_eq!(
            *fake.confirm_probe_calls.lock().unwrap(),
            0,
            "--no-probe suppresses the offer entirely, in-loop and post-routing"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reinit_overwrite_offers_the_probe_at_most_once_per_provider() {
        let xdg = tempfile::tempdir().unwrap();
        let _env = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // A config already routing model `m` -> provider `p`, both on disk and
        // in the existing_config the wizard reasons about, so the in-loop hook
        // can resolve the lane the moment `p` is overwritten.
        let existing_text = format!("version = {CURRENT_CONFIG_VERSION}\n")
            + "\
[providers.p]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:2\"
api_key_ref = \"literal:test-key\"

[models.m]
provider = \"p\"
upstream = \"gpt-4o\"

[aliases]
default = \"m\"
";
        std::fs::write(&path, &existing_text).unwrap();
        let existing = parse_config(&existing_text).unwrap();

        // Overwrite `p` with a DIFFERENT block so the add is a real `Written`,
        // arming the in-loop offer (the lane already exists on disk).
        let plan = WizardPlan {
            provider_args: vec![provider_add::ProviderAddArgs {
                kind: "openai-compat".to_string(),
                name: "p".to_string(),
                base_url: Some("http://127.0.0.1:1".to_string()),
                api_key_env: None,
                secret_ref: Some("file:///abs/key".to_string()),
                api_key_stdin: false,
                credential_source: None,
                overwrite: true,
                yes: true,
                probe: None,
            }],
            models: vec![ModelWiring {
                nick: "m".to_string(),
                provider: "p".to_string(),
                upstream: "gpt-4o".to_string(),
            }],
            default_alias: "m".to_string(),
        };

        let fake = FakeInitIo {
            confirm_probe: false,
            ..Default::default()
        };

        apply_plan(&path, true, plan, &existing, &fake)
            .await
            .expect("apply_plan overwrites and routes");

        assert_eq!(
            *fake.confirm_probe_calls.lock().unwrap(),
            1,
            "a provider whose lane pre-existed is offered once in-loop and skipped \
             post-routing -- never twice"
        );
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
        let existing =
            format!("version = {CURRENT_CONFIG_VERSION}\n[server]\nhost = \"10.0.0.1\"\n");
        std::fs::write(&path, &existing).unwrap();

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

    // -----------------------------------------------------------------
    // Empty-offer credential capture (H2): a credential-less first run no
    // longer dead-ends at the missing-route error. The oauth branch is the
    // hermetic end-to-end vehicle (it wires an `oauth://anthropic` ref via
    // the fake login seam, touching no managed secret store); the api-key
    // branch's provider-arg mapping is pinned in plan.rs.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn empty_offers_oauth_capture_reaches_a_valid_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo {
            credential_capture: CredentialCapture::OauthLogin,
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
            Vec::new(),
            &fake,
        )
        .await
        .expect("an empty-offer oauth capture writes a routed config");

        assert_eq!(
            *fake.login_calls.lock().unwrap(),
            1,
            "the synthesized oauth offer drives exactly one login"
        );
        let cfg = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = cfg
            .providers
            .get("anthropic")
            .expect("captured provider wired");
        assert_eq!(entry.api_key_ref(), Some("oauth://anthropic"));
        assert_eq!(
            default_alias_of(&cfg).as_deref(),
            Some("anthropic"),
            "the captured credential becomes the default route"
        );
    }

    #[tokio::test]
    async fn empty_offers_declined_capture_writes_nothing_and_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fake = FakeInitIo {
            credential_capture: CredentialCapture::Skip,
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
        .expect("declining capture is a graceful exit, not a routing error");

        assert!(!path.exists(), "a declined capture writes nothing");
        assert_eq!(
            *fake.login_calls.lock().unwrap(),
            0,
            "no login when the operator declines"
        );
    }

    #[tokio::test]
    async fn empty_offers_under_yes_exits_cleanly_without_a_routing_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // No offers, no --forwarded: the non-interactive path cannot capture a
        // credential, so it prints the actionable next step and returns Ok --
        // never the raw MissingDefaultRoute the bare plan would produce.
        orchestrate(
            &path,
            &init_args(false, true, Some("claude-sonnet-4-5"), false),
            &Config::default(),
            false,
            Vec::new(),
            &FakeInitIo::default(),
        )
        .await
        .expect("an empty-offer --yes run must not surface the raw routing error");

        assert!(
            !path.exists(),
            "nothing is written when there is no credential to route"
        );
    }

    #[test]
    fn missing_credential_next_steps_names_login_and_env_but_no_routing_error() {
        let msg = missing_credential_next_steps();

        assert!(
            msg.contains("routectl login anthropic"),
            "names the parser-valid login command: {msg}"
        );
        assert!(
            msg.contains("ANTHROPIC_API_KEY"),
            "names the conventional env var: {msg}"
        );
        assert!(
            msg.contains("routectl init"),
            "tells the operator to re-run init: {msg}"
        );
        assert!(
            !msg.to_ascii_lowercase().contains("default route"),
            "never surfaces the raw routing error: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // Model-id validation at the capture boundary: a doctor-clean config
    // can no longer carry an id the first real request would reject.
    // `validate_model_id` is a pure shape check; `collect_model_ids` is
    // the non-interactive fail seam; `capture_model_id` is the interactive
    // re-prompt loop.
    // -----------------------------------------------------------------

    #[test]
    fn validate_model_id_accepts_a_plain_id_and_rejects_unusable_ones() {
        assert!(validate_model_id("claude-sonnet-4-5").is_ok());
        assert!(validate_model_id("gpt-4o").is_ok());
        assert!(validate_model_id("provider/model:v1.2").is_ok());

        assert!(validate_model_id("").is_err(), "empty is rejected");
        assert!(
            validate_model_id("   ").is_err(),
            "whitespace-only is rejected"
        );
        assert!(
            validate_model_id("gpt 4o").is_err(),
            "embedded whitespace is rejected"
        );
        assert!(
            validate_model_id("gpt\t4o").is_err(),
            "an embedded tab is rejected"
        );
        assert!(
            validate_model_id("gpt\n4o").is_err(),
            "an embedded newline is rejected"
        );
        assert!(
            validate_model_id("gpt\u{7}4o").is_err(),
            "an embedded control char is rejected"
        );
        assert!(
            validate_model_id("gpt\"4o").is_err(),
            "an embedded quote is rejected"
        );
        assert!(
            validate_model_id("gpt\\4o").is_err(),
            "an embedded backslash is rejected"
        );
    }

    #[test]
    fn collect_model_ids_rejects_an_unparseable_default_model_before_any_write() {
        let offers = vec![forwarded_test_offer()];
        let err = collect_model_ids(
            &init_args(false, true, Some("claude sonnet"), true),
            &offers,
            &FakeInitIo::default(),
        )
        .expect_err("an unparseable --default-model must fail at capture");

        let msg = err.to_string();
        assert!(msg.contains("not usable"), "actionable message: {msg}");
        assert!(msg.contains("whitespace"), "names the reason: {msg}");
    }

    #[test]
    fn collect_model_ids_rejects_an_empty_default_model() {
        let offers = vec![forwarded_test_offer()];
        let err = collect_model_ids(
            &init_args(false, true, Some("   "), true),
            &offers,
            &FakeInitIo::default(),
        )
        .expect_err("a whitespace-only --default-model must fail at capture");
        assert!(err.to_string().contains("not usable"), "err: {err}");
    }

    #[test]
    fn collect_model_ids_rejects_an_exact_empty_default_model() {
        // An explicit `--default-model ""` is an empty VALUE (not the
        // interactive blank-to-skip decline), so it is rejected at capture
        // rather than falling through to the later missing-model error.
        let offers = vec![forwarded_test_offer()];
        let err = collect_model_ids(
            &init_args(false, true, Some(""), true),
            &offers,
            &FakeInitIo::default(),
        )
        .expect_err("an explicit empty --default-model must fail at capture");
        assert!(err.to_string().contains("not usable"), "err: {err}");
        assert!(err.to_string().contains("empty"), "names the reason: {err}");
    }

    #[test]
    fn collect_model_ids_accepts_a_valid_id_unchanged() {
        let offers = vec![forwarded_test_offer()];
        let ids = collect_model_ids(
            &init_args(false, true, Some("claude-sonnet-4-5"), true),
            &offers,
            &FakeInitIo::default(),
        )
        .expect("a valid model id passes capture");

        assert_eq!(
            ids.get("anthropic-forwarded").map(String::as_str),
            Some("claude-sonnet-4-5"),
            "the valid id is stored verbatim"
        );
    }

    #[tokio::test]
    async fn unparseable_default_model_errors_before_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let err = orchestrate(
            &path,
            &init_args(false, true, Some("bad id"), true),
            &Config::default(),
            false,
            vec![forwarded_test_offer()],
            &FakeInitIo::default(),
        )
        .await
        .expect_err("an unusable model id must abort the run");

        assert!(err.to_string().contains("not usable"), "err: {err}");
        assert!(!path.exists(), "no write before the capture-time error");
    }

    #[test]
    fn interactive_capture_reprompts_past_an_unusable_id() {
        // The reader pops from the end, so the unusable entry is read first.
        let mut inputs = vec!["claude-sonnet-4-5".to_string(), "gpt 4o".to_string()];
        let mut warnings = 0;
        let got = capture_model_id(|| inputs.pop(), |_reason| warnings += 1);

        assert_eq!(
            got.as_deref(),
            Some("claude-sonnet-4-5"),
            "the loop returns the first valid id"
        );
        assert_eq!(
            warnings, 1,
            "the unusable id triggered exactly one re-prompt"
        );
    }

    #[test]
    fn interactive_capture_treats_a_blank_entry_as_skip() {
        let got = capture_model_id(|| Some(String::new()), |_| panic!("blank must not warn"));
        assert_eq!(got, None, "a blank entry skips the provider");
    }

    #[test]
    fn interactive_capture_treats_eof_as_skip() {
        let got = capture_model_id(|| None, |_| panic!("EOF must not warn"));
        assert_eq!(got, None, "a read error / EOF skips rather than loops");
    }
}
