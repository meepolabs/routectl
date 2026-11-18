//! `routectl provider add` -- add or overwrite a `[providers.<name>]` block
//! in `config.toml`, funneled through the ONE write path
//! ([`routectl_router::edit_config_toml`]) exactly as [`super::config_edit`]
//! does.
//!
//! Secret sources: `--secret-ref` / `--api-key-env` (flag-driven, no
//! capture), `--api-key-stdin` (piped value captured to the managed
//! `file://` store), an interactive hidden prompt (missing key + a TTY),
//! `--credential-source forwarded` (no secret at all), and `--login
//! <vendor>` (delegated to the `login` flow, ref `oauth://<vendor>`, or
//! `oauth://<vendor>#<label>` with `--label`).
//!
//! Every step runs IN MEMORY before any byte touches disk, so any refusal
//! (a stale/legacy version, an unknown secret scheme, a candidate that fails
//! the shared gate, an existing-block conflict, a declined confirmation)
//! leaves the file byte-identical. The lock-discipline invariant: the
//! secret file `put` (and any oauth login) happens AFTER the confirmation
//! and BEFORE the config write, and the advisory config lock is NEVER held
//! across a prompt, an oauth login, env probing, or the `put`.
//!
//!   1. RAW version + legacy preflights on the snapshot bytes
//!      (`super::edit_pipeline::preflight`, shared with `config set`).
//!   2. Resolve the format (or managed login), the auth, the name, the
//!      carrier (base URL or AWS region), and the secret source into a
//!      [`routectl_router::ProviderEntry`] plus a credential CLASS label (scheme only, never
//!      the value) and a `PendingSecret` (the deferred `put`/login, if
//!      any). A captured file ref's STRING is computed deterministically
//!      here via `ManagedSecretStore::ref_path` -- no bytes written yet.
//!   3. Serialize the entry to a standard `toml_edit` table. If a provider
//!      of the same name already exists, a byte-identical (normalized) block
//!      is an idempotent NO-OP; a different block is refused unless
//!      `--overwrite` is given.
//!      With `--pool`, the pool edit joins the block in ONE candidate
//!      (`toml_edit::Candidate`): the entry first takes the pool's explicit
//!      cloak keys, and every later step treats block and pool as one.
//!   4. Gate the candidate through the SAME shared gate the reload path runs
//!      (`super::edit_pipeline::gate`); any failure renders and writes
//!      nothing.
//!   5. A new/overwritten provider block is always egress-defining, so it
//!      prompts for confirmation before the lock is acquired (`--yes`
//!      bypasses; a non-interactive run without it declines). A declined
//!      confirm captures NO secret and logs in NOT.
//!   6. NOW execute the pending secret side effect (file `put` / oauth
//!      login), then [`routectl_router::edit_config_toml`] re-reads under the advisory lock,
//!      re-applies the same deterministic insert, re-gates, and commits
//!      atomically (or writes nothing on a no-op). A post-capture write
//!      conflict emits an explicit recovery message -- the secret/login
//!      persists, the config is unchanged, re-run to complete (a login
//!      names the exact command when it can be printed exactly).
//!   7. On a real write, emit exactly one audit event -- surface, verb,
//!      name, family token, credential class -- NEVER the value and NEVER
//!      the full ref string.

use std::io::{IsTerminal, Read};
use std::path::Path;

use async_trait::async_trait;
use routectl_core::{Error, Result};
use routectl_router::{Config, EditOutcome, ProviderEntry, parse_config};

use super::edit_pipeline::{
    confirm_high_consequence, gate, inherit_pool_cloak, preflight, render_gate_errors,
};
use super::rerun::{RerunCommand, chosen_config_path, instruction};
use crate::config_classify::collect_high_consequence_changes;

mod build;
mod capture;
mod toml_edit;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "format_tests.rs"]
mod format_tests;

#[cfg(test)]
#[path = "carrier_tests.rs"]
mod carrier_tests;

#[cfg(test)]
#[path = "pool_tests.rs"]
mod pool_tests;

use self::build::build_entry;
use self::capture::execute_pending;
use self::toml_edit::{Candidate, commit, plan_pool, provider_table};

/// The `--format` values `provider add` builds.
pub const FORMAT_VALUES: [&str; 6] = [
    "anthropic-messages",
    "openai-chat",
    "openai-responses",
    "bedrock-invoke",
    "bedrock-converse",
    "gemini",
];

/// The `--auth` values `provider add` accepts.
pub const AUTH_VALUES: [&str; 4] = ["x-api-key", "bearer", "x-goog-api-key", "aws-sigv4"];

/// The managed logins `provider add --login` builds an entry for.
pub const LOGIN_VALUES: [&str; 2] = ["anthropic", "codex"];

/// The refusal for the retired `--kind` flag.
pub const KIND_REPLACED_MESSAGE: &str = "`--kind` was replaced: use `--format \
    <anthropic-messages|openai-chat|openai-responses|bedrock-invoke|bedrock-converse|gemini>` \
    for a provider with a key or an AWS credential, or \
    `--login <anthropic|codex>` for a managed login";

/// Flag-driven inputs for `provider add`, assembled from the clap surface.
/// Exactly one of `format` and `login` is set.
pub struct ProviderAddArgs {
    pub format: Option<String>,
    pub login: Option<String>,
    /// The seat a `login` writes; only valid with `login`.
    pub label: Option<String>,
    /// The pool the `login` entry joins (or creates) in the same write; only
    /// valid with `login`.
    pub pool: Option<String>,
    /// How the credential is sent; `None` writes the format's default.
    pub auth: Option<String>,
    pub name: String,
    pub base_url: Option<String>,
    /// The AWS region of a `cloud = { name = "aws", region }` entry; never
    /// together with `base_url`.
    pub region: Option<String>,
    pub api_key_env: Option<String>,
    pub secret_ref: Option<String>,
    pub api_key_stdin: bool,
    pub credential_source: Option<String>,
    pub overwrite: bool,
    pub yes: bool,
}

/// The interactive / side-effecting seams `provider add` touches OUTSIDE
/// the pure config-editing pipeline: reading stdin, the hidden key prompt,
/// the env-detect offer, and the oauth login delegation. Injected so the
/// pipeline stays testable without a real TTY or a live browser flow;
/// production wires [`RealAddIo`].
#[async_trait]
pub trait AddIo: Send + Sync {
    /// Whether the process stdin is a terminal. Gates the stdin-capture
    /// error path and the interactive prompt path.
    fn stdin_is_terminal(&self) -> bool;
    /// Read all of stdin to a string (the piped `--api-key-stdin` value).
    fn read_stdin(&self) -> Result<String>;
    /// Offer to use an already-resolvable env var as the credential;
    /// `true` accepts `env://VAR`, `false` falls through to the prompt.
    fn confirm_env_offer(&self, var: &str) -> bool;
    /// Prompt for the API key without echoing it (interactive path).
    fn prompt_hidden(&self, provider_name: &str) -> Result<String>;
    /// Run the oauth login flow for `provider` into the seat `label` names
    /// (`None` is the default seat), persisting its tokens.
    async fn login(&self, provider: &str, label: Option<&str>) -> Result<()>;
}

/// Production [`AddIo`]: std stdin + `rpassword` for the hidden prompt +
/// the existing `login` command for oauth delegation. The oauth login is
/// awaited directly on the async `main` runtime; `provider add` carries the
/// login through its own async path rather than blocking a worker thread.
pub struct RealAddIo;

#[async_trait]
impl AddIo for RealAddIo {
    fn stdin_is_terminal(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn read_stdin(&self) -> Result<String> {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| Error::Config(format!("failed to read the API key from stdin: {e}")))?;
        Ok(buf)
    }

    fn confirm_env_offer(&self, var: &str) -> bool {
        use std::io::Write as _;
        println!("`{var}` is set in the environment and resolves now.");
        print!("use it as this provider's credential (env://{var})? [Y/n] ");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() {
            return false;
        }
        matches!(input.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes")
    }

    fn prompt_hidden(&self, provider_name: &str) -> Result<String> {
        rpassword::prompt_password(format!("API key for provider `{provider_name}`: "))
            .map_err(|e| Error::Config(format!("failed to read the API key prompt: {e}")))
    }

    async fn login(&self, provider: &str, label: Option<&str>) -> Result<()> {
        // `ConfigSurface::Skip` is load-bearing, not a default: this call
        // sits BETWEEN this command's byte snapshot and its commit, so a
        // config-writing login would invalidate the snapshot and make every
        // oauth `provider add` fail its conflict check. The entry the
        // surface would propose is the one this command is already writing.
        super::login::run(
            provider,
            false,
            None,
            label,
            super::login::ConfigSurface::Skip,
        )
        .await
    }
}

/// Outcome of a completed [`run`], for the caller and for tests. Refusals
/// (unknown format, missing credential, gate rejection, conflict without
/// `--overwrite`, write conflict) surface as `Err` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddResult {
    /// The provider block was written (added or overwritten).
    Written,
    /// The provider already existed with identical settings; nothing written.
    NoChange,
    /// The high-consequence confirmation was declined.
    Aborted,
    /// A fresh credential was captured and the managed secret file was
    /// rewritten, but the serialized config block was byte-identical, so
    /// `config.toml` itself was left unchanged. A managed `file://` ref is
    /// derived from the provider NAME rather than the value, so rotating the
    /// key produces the same block -- the secret still rotates.
    Rotated,
}

/// Operator-facing line printed when a file-backed credential is rotated but
/// the config block is byte-identical (so `config.toml` is not rewritten).
/// This exact text is an operator-scriptable contract, pinned by a test:
/// downstream tooling may match on it, so it must not drift silently.
const ROTATED_MESSAGE: &str = "credential rotated; config unchanged";

/// The error for a config write that failed after the secret capture or the
/// login already ran: the side effect persists and the file is unchanged. A
/// login names the command that finishes the wiring, so the rerun lands in
/// the same seat, entry and file.
fn post_side_effect_error(config_path: &Path, args: &ProviderAddArgs, cause: &Error) -> Error {
    if args.login.is_none() {
        return Error::Config(format!(
            "the credential was captured/logged in successfully, but the config was \
             NOT changed ({cause}). config.toml is unchanged; the captured credential \
             persists -- re-run `provider add` to complete the wiring"
        ));
    }
    let rerun = instruction(
        login_rerun_command(chosen_config_path(config_path), args),
        "routectl provider add",
    );
    Error::Config(format!(
        "the login succeeded, but the config was NOT changed ({cause}). config.toml is \
         unchanged; the login persists -- {rerun} to complete the wiring"
    ))
}

/// The `provider add --login` command line these args came from, against
/// `config_path` (`None` for the default file), every operator-written value
/// one exact shell word. `None` when no login was asked, or when a value has
/// no exact printable form.
fn login_rerun_command(config_path: Option<&Path>, args: &ProviderAddArgs) -> Option<String> {
    RerunCommand::routectl(config_path)
        .word("provider")
        .word("add")
        .word("--login")
        .value(args.login.as_deref()?)
        .option("--label", args.label.as_deref())
        .option("--pool", args.pool.as_deref())
        .word("--name")
        .value(&args.name)
        .switch("--overwrite", args.overwrite)
        .switch("--yes", args.yes)
        .render()
}

/// The candidate for `entry` named `name` over the config `prev`: with a
/// `pool`, the entry first takes the pool's explicit cloak keys (members
/// must share them), so the printed and the written block both carry them.
fn build_candidate<'a>(
    prev: &Config,
    name: &'a str,
    pool: Option<&str>,
    entry: &mut ProviderEntry,
) -> Result<Candidate<'a>> {
    if let Some(pool) = pool {
        inherit_pool_cloak(prev, pool, entry);
    }
    Ok(Candidate {
        name,
        block: provider_table(entry)?,
        writes_block: true,
        pool: pool.and_then(|pool| plan_pool(prev, pool, name)),
    })
}

/// Run the `provider add` pipeline against `config_path` with the
/// production I/O seams ([`RealAddIo`]). Thin wrapper over
/// [`run_with_io`].
pub async fn run(config_path: &Path, args: ProviderAddArgs) -> Result<AddResult> {
    run_with_io(config_path, args, &RealAddIo).await
}

/// Run the `provider add` pipeline against `config_path`. Prints the human
/// result line and emits the audit event on a real write; returns the
/// structured outcome for the caller/tests. Validation and refusal paths
/// render their diagnostics and return `Err`. The `io` seam supplies the
/// interactive / side-effecting steps (stdin, hidden prompt, env offer,
/// oauth login) so the pipeline is testable without a TTY or a browser.
pub async fn run_with_io(
    config_path: &Path,
    args: ProviderAddArgs,
    io: &dyn AddIo,
) -> Result<AddResult> {
    let snapshot = std::fs::read(config_path).map_err(|e| {
        Error::Config(format!(
            "cannot read config `{}`: {e}",
            config_path.display()
        ))
    })?;
    let snapshot_text = String::from_utf8(snapshot.clone()).map_err(|e| {
        Error::Config(format!(
            "config `{}` is not UTF-8: {e}",
            config_path.display()
        ))
    })?;

    preflight(&snapshot_text)?;

    if args.name.trim().is_empty() {
        return Err(Error::Config("provider name must not be empty".into()));
    }

    let (mut entry, cred_class, pending) = build_entry(&args, io)?;
    let name = args.name.as_str();
    let prev = parse_config(&snapshot_text).map_err(|e| {
        Error::Config(format!(
            "current config does not parse; fix it before editing: {e}"
        ))
    })?;

    let mut candidate = build_candidate(&prev, name, args.pool.as_deref(), &mut entry)?;
    // The family token of the built entry, so a `--login` add audits what
    // actually lands on disk.
    let kind = entry.kind_str();
    let new_block_norm = candidate.block.to_string();

    if let Some(existing) = prev.providers.get(name) {
        let existing_norm = provider_table(existing)?.to_string();
        let identical = existing_norm == new_block_norm;
        // A fresh file capture rotates the managed secret even when the
        // serialized block is byte-identical, so it must NOT take the
        // identical-block no-op (that silently discards the new key). Every
        // other pending kind keeps the idempotent re-init no-op.
        let keeps_block = identical && !pending.rewrites_secret();
        if keeps_block && candidate.pool.is_none() {
            println!("provider `{name}` is already configured identically; nothing written.");
            return Ok(AddResult::NoChange);
        }
        // Only the pool half is owed: the operator's block keeps its bytes.
        candidate.writes_block = !keeps_block;
        if !args.overwrite && !keeps_block {
            let hint = if identical {
                // A file capture over an identical block: the config would
                // not change, but the credential rotates -- still an explicit
                // opt-in.
                format!(
                    "provider `{name}` already exists; pass `--overwrite` to rotate its credential"
                )
            } else {
                format!(
                    "provider `{name}` already exists with different settings; \
                     pass `--overwrite` to overwrite it"
                )
            };
            return Err(Error::Config(hint));
        }
    }

    let candidate_text = candidate.text(&snapshot_text)?;
    let next = gate(&candidate_text).map_err(|errors| {
        render_gate_errors(&errors);
        Error::Config(format!("{} config error(s)", errors.len()))
    })?;

    // A new or overwritten provider block always sets base_url + credential
    // source, so it is always egress-defining; the collector names the
    // specific fields for the prompt, with a fallback should it ever be empty.
    let mut high = collect_high_consequence_changes(&prev, &next);
    if high.is_empty() {
        high.push("providers.base_url");
    }
    if !args.yes {
        print!("{}", candidate.render());
    }
    if !confirm_high_consequence(&high, args.yes) {
        println!("aborted; nothing written.");
        return Ok(AddResult::Aborted);
    }

    // Only NOW -- after the confirm -- does any secret file get written or
    // any login run. A declined confirm above returns before this point.
    let did_side_effect = pending.is_side_effect();
    execute_pending(pending, io).await?;

    let outcome = match commit(config_path, &snapshot, &snapshot_text, &candidate) {
        Ok(outcome) => outcome,
        Err(e) if did_side_effect => {
            return Err(post_side_effect_error(config_path, &args, &e));
        }
        Err(e) => return Err(e),
    };

    if outcome == EditOutcome::Unchanged {
        if did_side_effect {
            // The config block was byte-identical, so config.toml was not
            // rewritten -- but a fresh capture already rotated the managed
            // secret. Report the rotation truthfully (never "nothing written"
            // after a side effect) and audit it: the credential class only,
            // never the value or the full ref string.
            println!("{ROTATED_MESSAGE}");
            tracing::info!(
                surface = "cli",
                verb = "provider-add",
                name,
                kind,
                credential_source = cred_class,
                config_changed = false,
                "credential rotated",
            );
            return Ok(AddResult::Rotated);
        }
        println!("provider `{name}` already present; nothing written.");
        return Ok(AddResult::NoChange);
    }

    let pool = candidate.pool.as_ref().map(|action| action.pool());
    tracing::info!(
        surface = "cli",
        verb = "provider-add",
        name,
        kind,
        credential_source = cred_class,
        pool = %routectl_core::sanitize_for_log(pool.unwrap_or("<none>")),
        "provider add committed",
    );

    println!(
        "added provider `{name}` (format = \"{}\", auth = \"{}\").",
        entry.format().as_str(),
        entry.auth.as_str()
    );
    if let Some(pool) = pool {
        println!("pool `{pool}` now lists `{name}`.");
    }
    Ok(AddResult::Written)
}
