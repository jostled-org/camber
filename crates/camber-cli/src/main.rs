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
    Serve {
        /// Path to config file
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
