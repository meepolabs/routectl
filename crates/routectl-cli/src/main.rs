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
    },
    /// Remove a provider's tokens from the routectl credentials store.
    /// First-time logout (no record present) is reported but is not an
    /// error.
    Logout {
        /// Which provider to log out of.
        #[arg(value_parser = provider_value_parser())]
        provider: String,
    },
    /// Force a token refresh for a provider through the per-provider
    /// single-flight gate, regardless of expiry. Useful when a token
    /// has been revoked server-side or before a long-running session.
    Refresh {
        /// Which provider to refresh.
        #[arg(value_parser = provider_value_parser())]
        provider: String,
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
            let config = load_config(cli.config.as_deref())?;
            let mut config = config;

            if let Some(h) = host {
                config.server.host = h;
            }
            if let Some(p) = port {
                config.server.port = p;
            }

            let host = config.server.host.clone();
            let port = config.server.port;
            let config = Arc::new(config);

            if let Err(e) = server::serve(config, &host, port, unsafe_public).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Login {
            provider,
            print_url,
            callback_port,
        } => {
            if let Err(e) = commands::login::run(&provider, print_url, callback_port).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Logout { provider } => {
            if let Err(e) = commands::logout::run(&provider).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Refresh { provider } => {
            if let Err(e) = commands::refresh::run(&provider).await {
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
    }

    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt::format::FmtSpan, EnvFilter};
    let filter = EnvFilter::try_from_env("ROUTECTL_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_span_events(FmtSpan::CLOSE)
        .init();
}

fn load_config(
    explicit: Option<&std::path::Path>,
) -> Result<routectl_router::Config, Box<dyn std::error::Error>> {
    let path = if let Some(p) = explicit {
        p.to_path_buf()
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(dirs::config_dir)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
            });
        base.join("routectl").join("config.toml")
    };

    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read config `{}`: {e}", path.display()))?;

    let cfg: routectl_router::Config = toml::from_str(&text)
        .map_err(|e| format!("config parse error in `{}`: {e}", path.display()))?;

    Ok(cfg)
}
