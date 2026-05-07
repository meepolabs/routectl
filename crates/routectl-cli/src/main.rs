//! routectl CLI.
//!
//! Subcommands:
//!   serve   Start the local OpenAI-compatible HTTP server.
//!   login   Capture a consumer-session cookie (claude.ai, chatgpt.com).
//!   test    One-shot completion against a configured provider/alias.
//!   config  Validate or print the resolved config.

use std::path::PathBuf;
use std::sync::Arc;

use routectl_cli::{commands, server};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "routectl", version, about = "Local LLM router with fallback chains")]
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
    /// Capture a consumer-session cookie via webview popup.
    Login {
        /// Which provider to log into.
        #[arg(value_parser = ["claude", "chatgpt"])]
        provider: String,
    },
    /// One-shot completion against an alias or `provider:model`.
    Test {
        /// Alias or `provider:model` target.
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
        Cmd::Login { provider } => {
            if let Err(e) = commands::login::run(&provider) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
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
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("ROUTECTL_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
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
            .or_else(|| dirs::config_dir())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
            });
        base.join("routectl").join("config.toml")
    };

    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!("cannot read config `{}`: {e}", path.display())
    })?;

    let cfg: routectl_router::Config = toml::from_str(&text).map_err(|e| {
        format!("config parse error in `{}`: {e}", path.display())
    })?;

    Ok(cfg)
}
