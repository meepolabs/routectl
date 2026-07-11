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
//!   pricing  Inspect or stamp the cache-economics pricing manifest.
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
    /// persists tokens to `~/.config/routectl/credentials.json`.
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
    /// Inspect or stamp the cache-economics pricing manifest.
    Pricing {
        #[command(subcommand)]
        action: PricingCmd,
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
    /// Print the resolved config (secrets redacted).
    Show,
    /// Print the example config to stdout.
    Example,
}

#[derive(Debug, Subcommand)]
enum PricingCmd {
    /// List the effective catalog (baked table merged with the on-disk
    /// overlay): every selector renders PRESENT (with provenance and a
    /// staleness marker) or DISABLED (overlay `null`). A catalog-only
    /// listing never enumerates a selector missing from BOTH layers, so
    /// MISSING never appears here -- it is a real merge outcome the two-
    /// layer merge still distinguishes at request-lookup time.
    List,
    /// Stamp an EXISTING overlay cell's `verified_at` to today (its
    /// `source` becomes `user`). Errors if the selector has no overlay
    /// cell yet (baked-only, or unknown) -- creating one is a later
    /// import/set verb. `selector` is `"provider_kind:model_glob"` (e.g.
    /// `openai-compat:grok-*`).
    Verify { selector: String },
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
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        } => {
            if let Err(e) =
                commands::login::run(&provider, print_url, callback_port, label.as_deref()).await
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
        Cmd::Test { target, prompt } => {
            let config = load_config(cli.config.as_deref())?;
            if let Err(e) = commands::test::run(config, &target, &prompt).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Config { action } => match action {
            ConfigCmd::Check => {
                let config = load_config(cli.config.as_deref())?;
                if let Err(e) = commands::config::check(&config).await {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            ConfigCmd::Show => {
                let config = load_config(cli.config.as_deref())?;
                if let Err(e) = commands::config::show(&config) {
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
        Cmd::Pricing { action } => match action {
            PricingCmd::List => {
                let loaded = load_config_with_overlay(cli.config.as_deref())?;
                if let Err(e) = commands::pricing::list(&loaded.catalog_overlay) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            PricingCmd::Verify { selector } => {
                if let Err(e) = commands::pricing::verify(&selector) {
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
/// `Cmd::Serve`, `Cmd::PromptSize`, and `Cmd::Pricing`'s `list` verb -- the
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
