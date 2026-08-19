//! routectl CLI.
//!
//! Subcommands:
//!   serve    Start the local OpenAI-compatible HTTP server.
//!   login    Run the OAuth 2.0 PKCE flow against a managed provider;
//!            tokens persist to ~/.config/routectl/credentials.json.
//!   logout   Remove a provider's tokens from the routectl credentials
//!            store.
//!   refresh  Force a token refresh through the per-provider
//!            single-flight gate, regardless of expiry.
//!   whoami   Print the OAuth provider state from the routectl
//!            credentials store.
//!   test     One-shot completion against an alias or model nickname.
//!   config   Validate or print the resolved config.
//!   catalog  Inspect, verify, import, or edit the cache-economics catalog
//!            (hidden alias: `pricing`).
//!   rc       Print MITM-proxy env vars, or force a CA rotation.

use std::path::PathBuf;
use std::sync::Arc;

use routectl_cli::{commands, server};

use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand};

/// Build a clap value-parser that accepts exactly the OAuth provider ids
/// the auth registry knows about. Driven by
/// `routectl_auth::oauth::known_provider_ids()` so `login` / `logout` /
/// `refresh` stay in lockstep with the registry: a new provider added in
/// routectl-auth is accepted here with zero edits, and an unknown value is
/// rejected by clap with the valid set listed in the error.
fn provider_value_parser() -> PossibleValuesParser {
    PossibleValuesParser::new(routectl_auth::oauth::known_provider_ids())
}

/// Build a clap value-parser that accepts exactly the four v1 capability
/// keys the probe can exercise. Driven by `ProbeCapability::ALL` so the
/// accepted `--only` tokens stay in lockstep with the probe's capability
/// set: an unknown token is rejected by clap with the valid set listed in
/// the error.
fn capability_value_parser() -> PossibleValuesParser {
    PossibleValuesParser::new(
        commands::probe::capabilities::ProbeCapability::ALL
            .iter()
            .map(|cap| cap.capability_key()),
    )
}

#[derive(Debug, Parser)]
#[command(
    name = "routectl",
    version,
    about = "Local LLM router with fallback chains"
)]
struct Cli {
    /// Path to config file. Defaults to `$XDG_CONFIG_HOME/routectl/config.toml`
    /// or `~/.config/routectl/config.toml`.
    #[arg(long, env = "ROUTECTL_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start the local HTTP server.
    Serve {
        /// Host bind. Defaults to 127.0.0.1 (loopback only).
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Allow non-loopback bind. Refuses without this flag.
        #[arg(long)]
        unsafe_public: bool,
    },
    /// Log into a managed OAuth provider (`anthropic` for claude.ai,
    /// `codex` for OpenAI ChatGPT/Codex). Spawns a local callback
    /// server, opens the browser to the provider's auth URL, and
    /// persists tokens to `~/.config/routectl/credentials.json`. Then
    /// offers the `config.toml` change that makes the new seat
    /// reachable; declining it, or having no editable config, still
    /// exits 0 -- the credential is stored either way.
    Login {
        /// Which provider to log into.
        #[arg(value_parser = provider_value_parser())]
        provider: String,
        /// Print the auth URL to stdout and read the redirect from
        /// stdin instead of launching a browser. For SSH/headless.
        #[arg(long)]
        print_url: bool,
        /// Override the local callback port. Default: random ephemeral.
        #[arg(long)]
        callback_port: Option<u16>,
        /// Seat label. Omit to write the default seat (today's
        /// behavior); pass a name to register an additional same-provider
        /// seat (`oauth://<provider>#<label>`) without overwriting the
        /// default.
        #[arg(long)]
        label: Option<String>,
        /// Apply the offered config change without the confirmation
        /// prompt. The change is still printed before it is written.
        #[arg(long)]
        yes: bool,
    },
    /// Remove a provider's tokens from the routectl credentials store.
    /// First-time logout (no record present) is reported but is not an
    /// error.
    Logout {
        /// Which provider to log out of.
        #[arg(value_parser = provider_value_parser())]
        provider: String,
        /// Seat label. Omit to remove the default seat (today's
        /// behavior); pass a name to remove only that one seat.
        #[arg(long)]
        label: Option<String>,
    },
    /// Force a token refresh for a provider through the per-provider
    /// single-flight gate, regardless of expiry. Useful when a token
    /// has been revoked server-side or before a long-running session.
    Refresh {
        /// Which provider to refresh.
        #[arg(value_parser = provider_value_parser())]
        provider: String,
        /// Seat label. Omit to refresh the default seat (today's
        /// behavior); pass a name to refresh only that one seat.
        #[arg(long)]
        label: Option<String>,
    },
    /// Print the OAuth provider state from the routectl credentials
    /// store. Exits 0 when at least one provider is logged in,
    /// non-zero otherwise.
    Whoami,
    /// Read-only health report: provider activation, config schema
    /// version, and OAuth credential state. Mutates nothing. Exits 0 when
    /// no check fails, non-zero on any failure.
    Doctor {
        /// Emit the report as JSON (`{schema_version, findings, panels}`)
        /// instead of the human battery. The schema is UNSTABLE.
        #[arg(long)]
        json: bool,
    },
    /// Actively probe a scoped model's true capabilities by dispatching a
    /// small, bounded set of canary calls straight at the provider (never
    /// through the router or a serving handler) and settling the learned-
    /// capability ledger from the structural evidence. Prints a cost estimate
    /// and asks for confirmation before any call unless `--yes` is given; the
    /// estimate prints either way. Scope the probe to exactly one target with
    /// `--alias` (a model nickname) or `--provider` (a configured provider).
    #[command(group(
        clap::ArgGroup::new("probe_target")
            .args(["alias", "provider"])
            .required(true),
    ))]
    Probe {
        /// Run the capability probe. Currently the only probe mode.
        #[arg(long, required = true)]
        capabilities: bool,
        /// Target a `[models.X]` nickname (resolves both the provider and the
        /// upstream model id the canaries hit).
        #[arg(long)]
        alias: Option<String>,
        /// Target a `[providers.X]` key (model id resolved from the single
        /// selectable model referencing it).
        #[arg(long)]
        provider: Option<String>,
        /// Restrict the probe to a comma-separated subset of the four
        /// capabilities; omit to probe all of them.
        #[arg(long, value_delimiter = ',', value_parser = capability_value_parser())]
        only: Vec<String>,
        /// Skip the confirmation prompt. The cost estimate still prints.
        #[arg(long)]
        yes: bool,
        /// Emit the report as JSON (schema UNSTABLE) instead of text.
        #[arg(long)]
        json: bool,
    },
    /// One-shot completion against an alias key or model nickname.
    Test {
        /// Alias key (`[aliases]` entry) or model nickname (`[models.X]` table key).
        target: String,
        /// User prompt. Defaults to a small smoke prompt.
        #[arg(short, long, default_value = "Say hi in exactly five words.")]
        prompt: String,
    },
    /// Validate or print the resolved config.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Guided first-run setup: detect stored oauth logins and resolvable
    /// credential env vars, wire the chosen providers plus a single default
    /// route, and write `config.toml` through the same atomic, re-validated
    /// write path the other config commands use. Composes `provider add`
    /// once per selected provider under ONE confirmation, then the single
    /// models/aliases write. Ends at config-written -- it prints the next
    /// steps (run doctor, start serve, a sample curl) but embeds no probe,
    /// installs no service, and launches nothing.
    Init {
        /// Drop the committed starter `config.toml` (expert fast-path)
        /// instead of walking the guided wizard. Fresh path only; refuses
        /// an existing config. Mutually exclusive with the wizard flags.
        #[arg(long, conflicts_with_all = ["default_model", "forwarded"])]
        scaffold: bool,
        /// Non-interactive: select every detected credential and take the
        /// remaining answers from the flags/env. A required value with no
        /// TTY errors actionably instead of prompting.
        #[arg(long)]
        yes: bool,
        /// Upstream model id wired for every selected provider -- the
        /// non-interactive answer to the per-provider model-id prompt.
        #[arg(long = "default-model")]
        default_model: Option<String>,
        /// Include the zero-config forwarded (claude.ai relay) provider and
        /// its alias, captured with no secret prompt.
        #[arg(long)]
        forwarded: bool,
        /// After each provider is configured, run a capability probe against
        /// it without prompting. Conflicts with `--no-probe`.
        #[arg(long, conflicts_with = "no_probe")]
        probe: bool,
        /// Suppress the post-configuration capability-probe offer entirely.
        #[arg(long = "no-probe")]
        no_probe: bool,
    },
    /// Add or overwrite a provider entry in `config.toml`, routed through
    /// the same atomic, re-validated write path as `config set`. The secret
    /// is supplied by reference (`--secret-ref`) or environment variable
    /// (`--api-key-env`) -- never a bare value on argv. A new or overwritten
    /// provider block is egress-defining and prompts for confirmation unless
    /// `--yes` is given.
    Provider {
        #[command(subcommand)]
        action: ProviderCmd,
    },
    /// Offline report of a request fixture's token footprint and what
    /// routectl's cache / reduction machinery would do to it. Never
    /// dispatches upstream and never resolves secrets or touches the network.
    PromptSize {
        /// Alias key (`[aliases]` entry) or model nickname (`[models.X]`
        /// table key) whose target provider's cache capability is consulted.
        #[arg(long)]
        alias: String,
        /// Path to a request body fixture (JSON) parsed as a canonical
        /// ChatRequest (OpenAI Chat Completions or Anthropic Messages shape).
        #[arg(long)]
        request: PathBuf,
        /// OPTIONAL cache-break economics projection: the size (in tokens)
        /// of a proposed cache-prefix cut. Supplying this flag turns ON the
        /// projection section; omitting it leaves the report unchanged.
        /// Mutually exclusive with `--steady-state`.
        #[arg(long = "hypothetical-d", conflicts_with = "steady_state")]
        hypothetical_d: Option<u64>,
        /// OPTIONAL: compute and price the REAL deterministic steady-state
        /// trim candidate for the request (front-anchored old-tool-content
        /// elision) instead of a hypothetical cut. Turns ON the projection
        /// section. Mutually exclusive with `--hypothetical-d`.
        #[arg(long = "steady-state")]
        steady_state: bool,
        /// OPTIONAL assumed future-reuse count. When given, also print a
        /// keep/break VERDICT for the cut. When omitted, only the break-even
        /// K* threshold is printed.
        #[arg(long = "hypothetical-k")]
        hypothetical_k: Option<f64>,
        /// OPTIONAL tokens at/after the edit point that must re-write.
        /// Defaults to C (the oldest-first conservative case).
        #[arg(long = "c-after")]
        c_after: Option<u64>,
        /// OPTIONAL cache TTL tier to price (Anthropic / Bedrock differ;
        /// other providers ignore it). One of `5m` or `1h`.
        #[arg(long = "ttl-tier", value_parser = ["5m", "1h"], default_value = "5m")]
        ttl_tier: String,
    },
    /// Summarize recorded usage from the local usage DB (read-only).
    ///
    /// With no window flag and no `--since`, prints a multi-window
    /// summary (today / this week / this month / all time). Calendar
    /// windows use LOCAL time; the week starts Monday.
    Usage {
        /// Usage since local midnight today.
        #[arg(long, group = "window")]
        today: bool,
        /// Usage since Monday 00:00 local of the current week.
        #[arg(long = "this-week", group = "window")]
        this_week: bool,
        /// Usage since the 1st of the current month, 00:00 local.
        #[arg(long = "this-month", group = "window")]
        this_month: bool,
        /// All recorded usage.
        #[arg(long, group = "window")]
        all: bool,
        /// Ad-hoc range start (YYYY-MM-DD, local). Conflicts with the
        /// window flags.
        #[arg(long, conflicts_with = "window")]
        since: Option<String>,
        /// Ad-hoc range end (YYYY-MM-DD, local). Defaults to now.
        /// Only valid with `--since`.
        #[arg(long, requires = "since")]
        until: Option<String>,
        /// Break the report down by this dimension instead of a single
        /// total row.
        #[arg(long, value_parser = ["model", "provider", "alias"])]
        by: Option<String>,
        /// Show extra columns (cache-write 5m/1h, ttft p50/p95, tok/s,
        /// server-tool counts) plus a per-window latency summary line.
        #[arg(long)]
        detail: bool,
        /// Override the usage DB path. Defaults to `[usage] db_path`.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Print the K-estimator calibration diagnostic over all history
        /// and exit. Window, --by, and --detail flags are ignored.
        #[arg(long = "k-calibration")]
        k_calibration: bool,
    },
    /// Inspect, verify, import, or edit the cache-economics catalog.
    /// Hidden alias `pricing` kept for muscle memory (dropped at 1.0).
    #[command(alias = "pricing")]
    Catalog {
        #[command(subcommand)]
        action: CatalogCmd,
    },
    /// Print MITM front-proxy env vars, or force a CA rotation.
    /// Requires a `[mitm]` config block.
    Rc {
        #[command(subcommand)]
        action: RcCmd,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Validate config syntax + provider references.
    Check,
    /// Print the resolved config (secrets redacted). With `--effective`,
    /// also print the provenance-annotated view: catalog cells tagged
    /// baked/import/user/disabled, retry classes tagged config/baked-default.
    Show {
        /// Append the provenance-annotated effective view (catalog cells +
        /// retry class policy) after the plain config dump.
        #[arg(long)]
        effective: bool,
    },
    /// Set a config value by dotted path (e.g. `server.port 8788`),
    /// re-validating the whole file through the shared gate before an
    /// atomic write. The value's scalar type is inferred (bool / int /
    /// float / string). An egress-defining change (a provider `base_url`
    /// or `credential_source`, a `[mitm]` origin) prompts for confirmation
    /// unless `--yes` is given.
    Set {
        /// Dotted config path to the scalar leaf to set.
        path: String,
        /// The value to assign; its scalar type is inferred.
        value: String,
        /// Skip the confirmation prompt for a high-consequence edit.
        #[arg(long)]
        yes: bool,
    },
    /// Remove a config override by dotted path (e.g. `retry.max_attempts`),
    /// so the value falls back to its inherited or catalog default. Parent
    /// tables the removal empties are pruned. The whole file is re-validated
    /// through the shared gate before an atomic write. Removing an
    /// egress-defining override prompts for confirmation unless `--yes` is
    /// given; removing a key that is not set writes nothing.
    Unset {
        /// Dotted config path to the key (or override table) to remove.
        path: String,
        /// Skip the confirmation prompt for a high-consequence edit.
        #[arg(long)]
        yes: bool,
    },
    /// Print the example config to stdout.
    Example,
    /// Migrate a legacy `config.toml` forward to the current schema version,
    /// re-validating the result through the same shared gate as `config set`
    /// before an atomic write. A v1 file chains v1->v2->v3 (folding the
    /// retired `[cache_pricing]` table into the catalog overlay); a v2 file
    /// migrates v2->v3, retiring the per-status `retry_allowlist` /
    /// `retry_denylist` keys. A config whose retry lists carry behavior that
    /// cannot be folded losslessly is refused with hand-edit guidance and
    /// nothing is written. The write requires acknowledgement -- an
    /// interactive `y`, or `--yes` when non-interactive.
    Migrate {
        /// Render the exact rewritten candidate plus a change summary without
        /// writing anything (needs no acknowledgement). The candidate is
        /// byte-exact and unredacted, so it may carry credentials anywhere the
        /// file does -- e.g. userinfo, query, or fragment in a `base_url`,
        /// `literal:` key refs, or a secret placed in `header_extras`. Never
        /// paste it into a bug report, and rotate anything already exposed.
        #[arg(long)]
        dry_run: bool,
        /// Acknowledge the schema break without an interactive prompt
        /// (required to migrate in a non-interactive run).
        #[arg(long)]
        yes: bool,
        /// Deprecated alias for `--yes`, kept for one release; prefer `--yes`.
        #[arg(long, hide = true)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderCmd {
    /// Add a provider block to `config.toml` (or overwrite one with
    /// `--overwrite`). The credential is given by reference only.
    Add {
        /// Provider kind. `openai-compat` (requires `--base-url`),
        /// `anthropic-api` (base URL defaults to api.anthropic.com; pair
        /// with `--credential-source forwarded` for a no-key forwarded
        /// provider), or `anthropic` (oauth-backed -- delegates to the
        /// `login` flow and stores an `oauth://` ref).
        #[arg(long)]
        kind: String,
        /// Operator-facing provider name; the `[providers.<name>]` key.
        #[arg(long)]
        name: String,
        /// Upstream base URL. Required for `openai-compat`; optional for
        /// `anthropic-api` (its constructor supplies the default).
        #[arg(long = "base-url")]
        base_url: Option<String>,
        /// Environment variable holding the API key; stored as
        /// `api_key_ref = "env://<VAR>"` after verifying it resolves now.
        #[arg(long = "api-key-env", group = "secret_source")]
        api_key_env: Option<String>,
        /// A secret reference written verbatim to `api_key_ref`
        /// (`env://VAR`, `file:///abs/key`, `oauth://...`). `literal:` refs
        /// are rejected; use `--api-key-stdin`, the hidden prompt, or `env://`.
        #[arg(long = "secret-ref", group = "secret_source")]
        secret_ref: Option<String>,
        /// Read the API key from stdin (pipe it in), capturing it to the
        /// managed 0600 `file://` store. Errors immediately if stdin is a
        /// TTY -- it never blocks waiting for keyboard input.
        #[arg(long = "api-key-stdin", group = "secret_source")]
        api_key_stdin: bool,
        /// Credential source for an `anthropic-api` provider: `own`
        /// (default -- routectl's configured key) or `forwarded` (relay the
        /// client's captured claude.ai bearer; captures no key, pins the
        /// base URL to api.anthropic.com).
        #[arg(
            long = "credential-source",
            value_parser = clap::builder::PossibleValuesParser::new(["own", "forwarded"])
        )]
        credential_source: Option<String>,
        /// Overwrite an existing provider of the same name (still prompts
        /// for the egress-defining confirmation unless `--yes`).
        #[arg(long)]
        overwrite: bool,
        /// Skip the high-consequence confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// After a successful add, run a capability probe against the new
        /// provider without prompting (only when a model already routes to
        /// it). Conflicts with `--no-probe`.
        #[arg(long, conflicts_with = "no_probe")]
        probe: bool,
        /// Suppress the post-add capability-probe offer entirely.
        #[arg(long = "no-probe")]
        no_probe: bool,
    },
    /// Probe configured providers for reachability without billing a model
    /// call. Read-only: never refreshes a token or mutates config/creds.
    /// With `<name>` probes one provider; omitted probes every configured
    /// provider. Exits nonzero if any probe fails.
    Probe {
        /// Probe only this provider; omit to probe every configured one.
        name: Option<String>,
        /// Emit the report as JSON (schema UNSTABLE) instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Hidden: env-gated Bedrock envelope-capture harness. Requires
    /// `ROUTECTL_BEDROCK_ENVELOPE_CAPTURE=1` and exactly one explicit
    /// `--provider` or `--alias` target. CLI-only; never reachable from
    /// the serving listener.
    #[command(hide = true)]
    CaptureEnvelope {
        /// Target a Bedrock `[providers.X]` key (model id resolved from the
        /// single selectable model referencing it).
        #[arg(long)]
        provider: Option<String>,
        /// Target a `[models.X]` nickname (resolves both provider and model
        /// id).
        #[arg(long)]
        alias: Option<String>,
        /// Directory the byte-exact response bodies are written to.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum CatalogCmd {
    /// List the effective catalog (baked table merged with the on-disk
    /// overlay): every selector renders PRESENT (with provenance and a
    /// staleness marker) or DISABLED (overlay `null`). A catalog-only
    /// listing never enumerates a selector missing from BOTH layers, so
    /// MISSING never appears here -- it is a real merge outcome the two-
    /// layer merge still distinguishes at request-lookup time. Headed by
    /// an overlay summary line (revision + counts by source + disabled
    /// count).
    List,
    /// Stamp an EXISTING overlay cell's `verified_at` to today (its
    /// `source` becomes `user`). Errors if the selector has no overlay
    /// cell yet (baked-only, or unknown) -- creating one is a `set`
    /// concern. `selector` is `"provider_kind:model_glob"` (e.g.
    /// `openai-compat:grok-*`).
    Verify { selector: String },
    /// Opt-in refresh of the catalog overlay from the litellm + models.dev
    /// sources: fetches both (or reads both from disk via the `--*-file`
    /// flags), builds one candidate, shows an impact-labeled diff, and --
    /// on confirmation -- applies it ALL-OR-NOTHING. Never runs at
    /// startup; a fetch failure on either source aborts the whole apply
    /// with the overlay left byte-identical.
    Import {
        /// Read the litellm source from this file instead of the network.
        /// Must be given together with `--models-dev-file`.
        #[arg(long)]
        litellm_file: Option<PathBuf>,
        /// Read the models.dev source from this file instead of the
        /// network. Must be given together with `--litellm-file`.
        #[arg(long)]
        models_dev_file: Option<PathBuf>,
        /// Skip the y/N confirmation prompt (scripting).
        #[arg(long)]
        yes: bool,
        /// Bypass ONLY the shrink guard's per-source/per-family floors.
        /// Never bypasses a fetch failure, a cross-check skip, a
        /// `source: user` conflict, or a revision conflict.
        #[arg(long)]
        allow_shrink: bool,
    },
    /// Write a `source: user` cell for a KNOWN selector (an existing
    /// baked row, or an existing overlay cell of either provenance),
    /// field by field. `selector` is `"provider_kind:model_glob"`.
    /// Rejects a selector unknown to the catalog -- creating a brand-new
    /// one is not supported by this verb.
    ///
    /// Each `field` is a `field=value` pair; supported fields are `wm`
    /// (f32), `rm` (f32), `ttl_seconds` (u32), `min_prefix_tokens` (u32),
    /// `max_context_tokens` (u32), `input_cost_per_token` (f32),
    /// `output_cost_per_token` (f32), and a capability flag via
    /// `cap:<name>=true|false` (e.g. `cap:web_search=true`).
    /// `auto_cacher` / `has_storage_rent` / `storage_rent` /
    /// `verified_at` are hard-rejected: the first three live only on the
    /// baked catalog table, and `verified_at` is always stamped
    /// automatically to today (UTC).
    ///
    /// A `wm` below the conservative sentinel (2.0) needs
    /// `--acknowledge-cost-risk`; `rm` must be `> 0`; `max_context_tokens`
    /// must not be `0`; the per-token rates are dollars per token and must
    /// not be negative.
    Set {
        selector: String,
        #[arg(required = true, num_args = 1..)]
        fields: Vec<String>,
        /// Required to set a `wm` below the conservative sentinel (2.0):
        /// a too-cheap write multiplier can make a cache break look
        /// falsely profitable.
        #[arg(long = "acknowledge-cost-risk")]
        acknowledge_cost_risk: bool,
    },
    /// Write a JSON-null overlay cell for a KNOWN selector, disabling it
    /// regardless of what it previously carried. Rejects a selector
    /// unknown to the catalog. Re-enabling is a fresh `set`.
    Disable { selector: String },
    /// Serialize the on-disk overlay (`catalog_overlay.json`) to pretty
    /// JSON, printed to stdout or written to `--out <path>`. The export is
    /// catalog cells ONLY -- it does NOT back up credentials (provider
    /// keys, OAuth tokens, and every other secret live in separate files
    /// this command never reads). To restore, place the exported JSON back
    /// at the overlay path; there is no separate overlay-import format
    /// (`import` consumes vendor economics snapshots, not this dump).
    Export {
        /// Write the export to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum RcCmd {
    /// Print `HTTPS_PROXY` and `NODE_EXTRA_CA_CERTS` for the configured
    /// MITM listener. Non-zero exit if `[mitm]` is not configured.
    Env,
    /// Re-mint the MITM CA + leaf certificate pair and print the new CA
    /// path. Non-zero exit if `[mitm]` is not configured.
    RegenCa,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        // Every error that reaches here propagates from a `load_config*`
        // helper -- the loader inlines the offending config VALUE (a secret
        // mistyped into a non-string field), user-controlled keys, and local
        // paths. Route the Display string through the shared fail-safe
        // redactor, the same one `doctor` and `config edit` use. It preserves
        // a message it does not recognize as config-parse/IO verbatim, so a
        // non-loader error stays actionable and its multi-line rendering
        // survives.
        eprintln!(
            "error: {}",
            commands::parse_error_redaction::redact_config_load_error(&e.to_string())
        );
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Serve {
            host,
            port,
            unsafe_public,
        } => {
            let resolved_config_path = resolve_config_path(cli.config.as_deref());
            let loaded = load_config_with_overlay(cli.config.as_deref())?;
            let mut config = loaded.config;

            if let Some(h) = host {
                config.server.host = h;
            }
            if let Some(p) = port {
                config.server.port = p;
            }

            let host = config.server.host.clone();
            let port = config.server.port;
            let config = Arc::new(config);
            let catalog_overlay = Arc::new(loaded.catalog_overlay);

            if let Err(e) = server::serve(
                config,
                catalog_overlay,
                &host,
                port,
                unsafe_public,
                Some(resolved_config_path),
            )
            .await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Login {
            provider,
            print_url,
            callback_port,
            label,
            yes,
        } => {
            let config_path = resolve_config_path(cli.config.as_deref());
            if let Err(e) = commands::login::run(
                &provider,
                print_url,
                callback_port,
                label.as_deref(),
                commands::login::ConfigSurface::Auto {
                    config_path: &config_path,
                    yes,
                },
            )
            .await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Logout { provider, label } => {
            if let Err(e) = commands::logout::run(&provider, label.as_deref()).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Refresh { provider, label } => {
            if let Err(e) = commands::refresh::run(&provider, label.as_deref()).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Whoami => match commands::whoami::run().await {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Cmd::Doctor { json } => {
            let path = resolve_config_path(cli.config.as_deref());
            if let Ok(loaded) = load_config_with_overlay(cli.config.as_deref()) {
                emit_staleness_hint_for(&loaded, json);
            }
            std::process::exit(commands::doctor::run(&path, json).await);
        }
        Cmd::Probe {
            capabilities: _,
            alias,
            provider,
            only,
            yes,
            json,
        } => {
            let config_path = resolve_config_path(cli.config.as_deref());
            std::process::exit(
                commands::probe::capabilities::run(&config_path, provider, alias, &only, yes, json)
                    .await,
            );
        }
        Cmd::Test { target, prompt } => {
            let config = load_config(cli.config.as_deref())?;
            if let Err(e) = commands::test::run(config, &target, &prompt).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Config { action } => match action {
            ConfigCmd::Check => {
                // `check` is the showcase validation surface: it runs the
                // full shared validator suite itself and renders EVERY error
                // with a source line. Load WITHOUT the fail-fast validation
                // gate so a parseable-but-semantically-invalid config reaches
                // the renderer intact instead of aborting on the first error.
                let config = load_config_unvalidated(cli.config.as_deref())?;
                // Re-read the raw TOML so `check` can render semantic errors
                // with the source line they came from. A read failure here is
                // not fatal: `check` falls back to the plain message.
                let raw = std::fs::read_to_string(resolve_config_path(cli.config.as_deref())).ok();
                if let Err(e) = commands::config::check(&config, raw.as_deref()).await {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            ConfigCmd::Show { effective } => {
                let loaded = load_config_with_overlay(cli.config.as_deref())?;
                emit_staleness_hint_for(&loaded, false);
                let result = if effective {
                    commands::config_effective::show_effective(
                        &loaded.config,
                        &loaded.catalog_overlay,
                    )
                } else {
                    commands::config::show(&loaded.config)
                };
                if let Err(e) = result {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            ConfigCmd::Set { path, value, yes } => {
                let config_path = resolve_config_path(cli.config.as_deref());
                if let Err(e) = commands::config_edit::run(
                    &config_path,
                    &path,
                    commands::config_edit::EditKind::Set(value),
                    yes,
                ) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            ConfigCmd::Unset { path, yes } => {
                let config_path = resolve_config_path(cli.config.as_deref());
                if let Err(e) = commands::config_edit::run(
                    &config_path,
                    &path,
                    commands::config_edit::EditKind::Unset,
                    yes,
                ) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            ConfigCmd::Example => {
                if let Err(e) = commands::config::example() {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            ConfigCmd::Migrate {
                dry_run,
                yes,
                force,
            } => {
                let config_path = resolve_config_path(cli.config.as_deref());
                if force {
                    eprintln!(
                        "warning: `--force` is deprecated for `config migrate`; use `--yes`."
                    );
                }
                if let Err(e) =
                    commands::config_migrate_cmd::run(&config_path, dry_run, yes || force).await
                {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        },
        Cmd::Init {
            scaffold,
            yes,
            default_model,
            forwarded,
            probe,
            no_probe,
        } => {
            let config_path = resolve_config_path(cli.config.as_deref());
            let probe = probe_choice(probe, no_probe);
            let args = commands::init::InitArgs {
                scaffold,
                yes,
                default_model,
                forwarded,
                probe,
            };
            if let Err(e) = commands::init::run(&config_path, args).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Provider { action } => match action {
            ProviderCmd::Add {
                kind,
                name,
                base_url,
                api_key_env,
                secret_ref,
                api_key_stdin,
                credential_source,
                overwrite,
                yes,
                probe,
                no_probe,
            } => {
                let config_path = resolve_config_path(cli.config.as_deref());
                let probe = probe_choice(probe, no_probe);
                let args = commands::provider_add::ProviderAddArgs {
                    kind,
                    name,
                    base_url,
                    api_key_env,
                    secret_ref,
                    api_key_stdin,
                    credential_source,
                    overwrite,
                    yes,
                    probe,
                };
                if let Err(e) = commands::provider_add::run(&config_path, args).await {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            ProviderCmd::Probe { name, json } => {
                let config_path = resolve_config_path(cli.config.as_deref());
                std::process::exit(commands::probe::run(&config_path, name, json).await);
            }
            ProviderCmd::CaptureEnvelope {
                provider,
                alias,
                out,
            } => {
                let config_path = resolve_config_path(cli.config.as_deref());
                let args = commands::probe::capture::CaptureArgs {
                    provider,
                    alias,
                    out,
                };
                std::process::exit(commands::probe::capture::run(&config_path, args).await);
            }
        },
        Cmd::PromptSize {
            alias,
            request,
            hypothetical_d,
            steady_state,
            hypothetical_k,
            c_after,
            ttl_tier,
        } => {
            let loaded = load_config_with_overlay(cli.config.as_deref())?;
            let projection = commands::prompt_size::ProjectionArgs {
                hypothetical_d,
                hypothetical_k,
                c_after,
                ttl_tier: &ttl_tier,
                steady_state,
            };
            if let Err(e) = commands::prompt_size::run(
                loaded.config,
                &loaded.catalog_overlay,
                &alias,
                &request,
                projection,
            ) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Usage {
            today,
            this_week,
            this_month,
            all,
            since,
            until,
            by,
            detail,
            db,
            k_calibration,
        } => {
            let config = load_config(cli.config.as_deref())?;
            let window = if today {
                commands::usage::WindowFlag::Today
            } else if this_week {
                commands::usage::WindowFlag::ThisWeek
            } else if this_month {
                commands::usage::WindowFlag::ThisMonth
            } else if all {
                commands::usage::WindowFlag::All
            } else {
                commands::usage::WindowFlag::None
            };
            let args = commands::usage::UsageArgs {
                window,
                since,
                until,
                by: by.as_deref().and_then(commands::usage::GroupDim::parse),
                detail,
                db,
                k_calibration,
            };
            if let Err(e) = commands::usage::run(&config, &args) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Catalog { action } => match action {
            CatalogCmd::List => {
                let loaded = load_config_with_overlay(cli.config.as_deref())?;
                emit_staleness_hint_for(&loaded, false);
                if let Err(e) = commands::catalog::list(&loaded.catalog_overlay) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            CatalogCmd::Verify { selector } => {
                if let Err(e) = commands::catalog::verify(&selector) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            CatalogCmd::Import {
                litellm_file,
                models_dev_file,
                yes,
                allow_shrink,
            } => {
                let args = commands::catalog_import::ImportArgs {
                    litellm_file,
                    models_dev_file,
                    yes,
                    allow_shrink,
                };
                if let Err(e) = commands::catalog_import::run(&args).await {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            CatalogCmd::Set {
                selector,
                fields,
                acknowledge_cost_risk,
            } => {
                if let Err(e) = commands::catalog::set(&selector, &fields, acknowledge_cost_risk) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            CatalogCmd::Disable { selector } => {
                if let Err(e) = commands::catalog::disable(&selector) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            CatalogCmd::Export { out } => {
                if let Err(e) = commands::catalog::export(out.as_deref()) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        },
        Cmd::Rc { action } => {
            let config = load_config(cli.config.as_deref())?;
            let result = match action {
                RcCmd::Env => commands::rc::env(&config),
                RcCmd::RegenCa => commands::rc::regen_ca(&config),
            };
            match result {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};
    let filter = EnvFilter::try_from_env("ROUTECTL_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_span_events(FmtSpan::CLOSE)
        .with_writer(std::io::stderr)
        .init();
}

/// Cold-start config load, via the SINGLE shared loader
/// (`server::load_effective_config`) also used by
/// the hot-reload path -- closes the pre-existing split-brain where only
/// this path merged the `pricing_verifications.json` sidecar /
/// `[cache_pricing]` overrides and loaded the catalog overlay.
///
/// Discards the loaded catalog overlay: the subcommands that call this
/// (`test`, `config check/show`, `usage`, `rc`) never consult it.
/// `Cmd::Serve`, `Cmd::PromptSize`, and `Cmd::Catalog`'s `list` verb -- the
/// callers that DO need the overlay (list renders the two-layer merge) --
/// use [`load_config_with_overlay`] instead.
fn load_config(
    explicit: Option<&std::path::Path>,
) -> Result<routectl_router::Config, Box<dyn std::error::Error>> {
    Ok(load_config_with_overlay(explicit)?.config)
}

/// Cold-start config + catalog-overlay load, via the SAME shared loader
/// [`load_config`] wraps. `Cmd::Serve` threads both into the Router build;
/// `Cmd::PromptSize` threads both into the offline economics projection so
/// it prices through the real overlay instead of an implicit empty one.
fn load_config_with_overlay(
    explicit: Option<&std::path::Path>,
) -> Result<server::LoadedConfig, Box<dyn std::error::Error>> {
    let path = resolve_config_path(explicit);
    Ok(server::load_effective_config(&path)?)
}

/// Emit the catalog-overlay staleness hint for a loaded config, reading the
/// real emission gates (stderr terminal-ness, `CI`, the
/// `ROUTECTL_NO_STALENESS_HINT` kill switch) and the freshest overlay stamp.
/// The pure gate logic lives in [`commands::staleness_hint`]; this seam only
/// binds it to the live environment. `is_json` is the calling verb's JSON
/// posture -- the hint never rides a machine-readable stream.
fn emit_staleness_hint_for(loaded: &server::LoadedConfig, is_json: bool) {
    use std::io::IsTerminal as _;

    let Some(verified_at) = commands::staleness_hint::freshest_verified_at(&loaded.catalog_overlay)
    else {
        return;
    };
    let threshold_days =
        i64::try_from(loaded.config.capability.staleness_hint_days).unwrap_or(i64::MAX);
    let is_tty = std::io::stderr().is_terminal();
    let is_ci = std::env::var_os("CI").is_some();
    let kill_switch = std::env::var_os("ROUTECTL_NO_STALENESS_HINT").is_some();
    let mut err = std::io::stderr().lock();
    commands::staleness_hint::emit_staleness_hint(
        &mut err,
        &verified_at,
        threshold_days,
        routectl_router::today_epoch_day(),
        is_tty,
        is_ci,
        kill_switch,
        is_json,
    );
}

/// Cold-start config load that PARSES and migrates but SKIPS the fail-fast
/// semantic validation gate. Only `config check` uses this: it runs the full
/// shared validator suite itself and renders every error with a source line,
/// so it must receive a parseable-but-semantically-invalid config intact
/// rather than have the load abort on the first semantic error. Parse-level
/// failures still propagate (nothing for `check` to render against).
fn load_config_unvalidated(
    explicit: Option<&std::path::Path>,
) -> Result<routectl_router::Config, Box<dyn std::error::Error>> {
    let path = resolve_config_path(explicit);
    Ok(server::load_effective_config_unvalidated(&path)?.config)
}

/// Resolve the config path the same way `load_config` does, but
/// without reading or parsing. Used to register the file-watch
/// target in `Cmd::Serve`.
fn resolve_config_path(explicit: Option<&std::path::Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join("routectl").join("config.toml")
}

/// Fold the mutually-exclusive `--probe` / `--no-probe` flag pair into the
/// tri-state the post-add probe offer consumes: `Some(true)` dispatches
/// without prompting, `Some(false)` suppresses the offer, and `None` (neither
/// flag) leaves it interactive. `--probe` and `--no-probe` conflict at the
/// clap layer, so both true never reaches here.
const fn probe_choice(probe: bool, no_probe: bool) -> Option<bool> {
    if no_probe {
        Some(false)
    } else if probe {
        Some(true)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pricing` is a HIDDEN clap alias for the renamed `catalog` family
    /// (muscle memory, dropped at 1.0): `routectl pricing list` must still
    /// parse to the SAME variant as `routectl catalog list`.
    #[test]
    fn pricing_alias_still_resolves_to_the_catalog_family() {
        let cli = Cli::parse_from(["routectl", "pricing", "list"]);
        assert!(matches!(
            cli.cmd,
            Cmd::Catalog {
                action: CatalogCmd::List
            }
        ));

        let cli = Cli::parse_from(["routectl", "catalog", "list"]);
        assert!(matches!(
            cli.cmd,
            Cmd::Catalog {
                action: CatalogCmd::List
            }
        ));
    }

    /// The secret-source arg group is mutually exclusive: `--api-key-env`
    /// and `--secret-ref` together is a clap-layer rejection, so a bare
    /// value never has two competing sources to resolve.
    #[test]
    fn provider_add_rejects_two_secret_sources() {
        let result = Cli::try_parse_from([
            "routectl",
            "provider",
            "add",
            "--kind",
            "openai-compat",
            "--name",
            "x",
            "--base-url",
            "http://127.0.0.1:1",
            "--api-key-env",
            "SOME_VAR",
            "--secret-ref",
            "file:///abs/key",
        ]);
        assert!(
            result.is_err(),
            "clap must reject two secret sources at once"
        );
    }

    /// `--api-key-stdin` joins the same mutually-exclusive group, so it
    /// cannot be combined with `--api-key-env` (or `--secret-ref`).
    #[test]
    fn provider_add_rejects_stdin_plus_env() {
        let result = Cli::try_parse_from([
            "routectl",
            "provider",
            "add",
            "--kind",
            "openai-compat",
            "--name",
            "x",
            "--base-url",
            "http://127.0.0.1:1",
            "--api-key-stdin",
            "--api-key-env",
            "SOME_VAR",
        ]);
        assert!(
            result.is_err(),
            "clap must reject --api-key-stdin together with --api-key-env"
        );
    }

    /// `--credential-source` only accepts the two known values.
    #[test]
    fn provider_add_rejects_unknown_credential_source() {
        let result = Cli::try_parse_from([
            "routectl",
            "provider",
            "add",
            "--kind",
            "anthropic-api",
            "--name",
            "x",
            "--credential-source",
            "bogus",
        ]);
        assert!(
            result.is_err(),
            "clap must reject an unknown --credential-source value"
        );
    }

    /// A single secret source parses cleanly to the `provider add` variant.
    #[test]
    fn provider_add_accepts_one_secret_source() {
        let cli = Cli::parse_from([
            "routectl",
            "provider",
            "add",
            "--kind",
            "anthropic-api",
            "--name",
            "claude",
            "--secret-ref",
            "env://ANTHROPIC_API_KEY",
        ]);
        assert!(matches!(
            cli.cmd,
            Cmd::Provider {
                action: ProviderCmd::Add { .. }
            }
        ));
    }

    /// `init` parses the full flag surface and reaches the `Cmd::Init` variant.
    #[test]
    fn init_parses_the_scaffold_and_wizard_flags() {
        let cli = Cli::parse_from(["routectl", "init", "--scaffold"]);
        assert!(matches!(cli.cmd, Cmd::Init { scaffold: true, .. }));

        let cli = Cli::parse_from([
            "routectl",
            "init",
            "--yes",
            "--default-model",
            "gpt-4o",
            "--forwarded",
        ]);
        assert!(matches!(
            cli.cmd,
            Cmd::Init {
                scaffold: false,
                yes: true,
                forwarded: true,
                default_model: Some(_),
                ..
            }
        ));
    }

    /// `--scaffold` is mutually exclusive with the wizard-flow flags at the
    /// clap layer, so the fast-path and the guided flow can never be requested
    /// at once.
    #[test]
    fn init_scaffold_conflicts_with_the_wizard_flags() {
        assert!(
            Cli::try_parse_from([
                "routectl",
                "init",
                "--scaffold",
                "--default-model",
                "gpt-4o"
            ])
            .is_err(),
            "--scaffold with --default-model must be rejected"
        );
        assert!(
            Cli::try_parse_from(["routectl", "init", "--scaffold", "--forwarded"]).is_err(),
            "--scaffold with --forwarded must be rejected"
        );
    }

    /// `config migrate` speaks the unified skip-confirm dialect: `--yes` is the
    /// canonical acknowledgement, and the deprecated `--force` still parses to
    /// the same variant (kept one release; hidden from help).
    #[test]
    fn migrate_accepts_yes_and_the_deprecated_force_alias() {
        let cli = Cli::parse_from(["routectl", "config", "migrate", "--yes"]);
        assert!(matches!(
            cli.cmd,
            Cmd::Config {
                action: ConfigCmd::Migrate {
                    dry_run: false,
                    yes: true,
                    force: false,
                }
            }
        ));

        let cli = Cli::parse_from(["routectl", "config", "migrate", "--force"]);
        assert!(matches!(
            cli.cmd,
            Cmd::Config {
                action: ConfigCmd::Migrate {
                    dry_run: false,
                    yes: false,
                    force: true,
                }
            }
        ));
    }

    /// `provider add` overwrite is spelled `--overwrite`; the old `--force`
    /// spelling no longer parses on this command.
    #[test]
    fn provider_add_overwrite_parses_and_force_is_rejected() {
        let cli = Cli::parse_from([
            "routectl",
            "provider",
            "add",
            "--kind",
            "openai-compat",
            "--name",
            "x",
            "--base-url",
            "http://127.0.0.1:1",
            "--secret-ref",
            "file:///abs/key",
            "--overwrite",
        ]);
        assert!(matches!(
            cli.cmd,
            Cmd::Provider {
                action: ProviderCmd::Add {
                    overwrite: true,
                    ..
                }
            }
        ));

        assert!(
            Cli::try_parse_from([
                "routectl",
                "provider",
                "add",
                "--kind",
                "openai-compat",
                "--name",
                "x",
                "--base-url",
                "http://127.0.0.1:1",
                "--secret-ref",
                "file:///abs/key",
                "--force",
            ])
            .is_err(),
            "provider add no longer accepts --force"
        );
    }

    /// Config-load failures surface as a boxed `String` error (see
    /// `load_effective_config -> Result<_, String>`, converted by `?`).
    /// `run`'s `Err` arm prints that error via `Display` (`{e}`), not `Debug`
    /// (`{e:?}`). The messages are deliberately multi-line and actionable, so
    /// `Display` must preserve REAL newlines and add no surrounding quotes or
    /// literal backslash-n -- the escaping `Debug` would introduce.
    #[test]
    fn config_load_error_display_keeps_real_newlines_no_escaping() {
        let message =
            "failed to load config:\n  alias `fast`: blocked by policy\n  alias `cheap`: no route";
        let err: Box<dyn std::error::Error> = message.to_string().into();

        let shown = format!("{err}");
        assert_eq!(
            shown, message,
            "Display must render the raw multi-line message unchanged"
        );
        assert!(shown.contains('\n'), "Display must preserve real newlines");
        assert!(
            !shown.contains("\\n"),
            "Display must not contain a literal backslash-n"
        );
        assert!(
            !shown.starts_with('"') && !shown.ends_with('"'),
            "Display must not wrap the message in quotes"
        );

        let debug = format!("{err:?}");
        assert!(
            debug.contains("\\n") && !debug.contains('\n'),
            "Debug (the old rendering) escapes newlines to literal backslash-n"
        );
    }

    /// `run`'s propagated config-load errors reach `main`'s `Err` arm, whose
    /// old `eprintln!("error: {e}")` printed the loader diagnostic verbatim --
    /// inlining the offending config VALUE (a secret mistyped into a
    /// non-string field) and the local config path. `main` now routes that
    /// Display string through the shared fail-safe redactor before printing.
    /// Pin the call shape: the secret and path are gone, the source
    /// line/column and safe field name survive, and the multi-line rendering
    /// is preserved with real newlines (no literal backslash-n).
    #[test]
    fn main_err_arm_redacts_a_secret_bearing_config_load_error() {
        let raw = "config parse error in `/home/someone/.config/routectl/config.toml`: \
                   TOML parse error at line 5, column 8\n  |\n5 | port = \"sk-live-LEAKED\"\n  \
                   |        ^^^^^^^^^^^^^^^^\ninvalid type: string \"sk-live-LEAKED\", expected u16";
        let err: Box<dyn std::error::Error> = raw.to_string().into();

        let rendered = commands::parse_error_redaction::redact_config_load_error(&err.to_string());

        assert!(!rendered.contains("sk-live-LEAKED"), "{rendered}");
        assert!(!rendered.contains("/home/someone"), "{rendered}");
        assert!(rendered.contains("line 5, column 8"), "{rendered}");
        assert!(rendered.contains("port"), "{rendered}");
        assert!(
            rendered.contains('\n'),
            "real newlines must survive: {rendered}"
        );
        assert!(
            !rendered.contains("\\n"),
            "no literal backslash-n: {rendered}"
        );
    }

    /// The redactor `main` applies is fail-safe: a message it does not
    /// recognize as config-parse/IO passes through unchanged, so a legitimate
    /// multi-line validation error stays actionable and unmangled.
    #[test]
    fn main_err_arm_passes_a_non_config_error_through_unchanged() {
        let message =
            "failed to load config:\n  alias `fast`: blocked by policy\n  alias `cheap`: no route";
        let err: Box<dyn std::error::Error> = message.to_string().into();

        let rendered = commands::parse_error_redaction::redact_config_load_error(&err.to_string());

        assert_eq!(rendered, message, "{rendered}");
    }
}
