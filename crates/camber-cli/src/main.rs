mod context;
mod new;
mod serve;
mod template;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "camber", about = "The Camber project tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new Camber project
    New {
        /// Project name
        name: String,

        /// Project template
        #[arg(long, default_value = "http")]
        template: String,
    },
    /// Run a config-driven reverse proxy
    ///
    /// The whole config is validated first. An invalid file exits nonzero
    /// before any secret load, cache write, upstream probe, or listener bind.
    Serve {
        /// Path to the TOML config file
        #[arg(
            long_help = "Path to the TOML config file. Unknown fields are refused.\n\n\
                           Top level: listen and connection_limit.\n\n\
                           [tls]: cert and key for manual TLS, or auto = true \
                           with email, staging, and cache_dir. DNS-01 adds \
                           dns_provider, which accepts only \"cloudflare\", and \
                           one of dns_api_token_env or dns_api_token_file.\n\n\
                           [[site]], one or more: host, proxy, root, health_check, \
                           and health_interval. proxy is an http or https URL \
                           with an optional path prefix. It must not carry \
                           credentials, a query, or a fragment."
        )]
        config: PathBuf,
    },
    /// Generate llms.txt API context for LLM code generation
    Context,
}

#[derive(thiserror::Error)]
enum CliError {
    #[error("{0}")]
    Config(Box<str>),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Runtime(#[from] camber::RuntimeError),
}

impl std::fmt::Debug for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl From<String> for CliError {
    fn from(s: String) -> Self {
        Self::Config(s.into())
    }
}

fn main() -> Result<(), CliError> {
    camber::logging::init_logging(
        camber::logging::LogFormat::Text,
        camber::logging::LogLevel::Info,
    );
    let cli = Cli::parse();

    match cli.command {
        Commands::New { name, template } => new::run(&name, &template)?,
        Commands::Serve { config } => serve::run(&config)?,
        Commands::Context => context::run()?,
    }

    Ok(())
}
