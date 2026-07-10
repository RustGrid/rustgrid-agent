use std::{path::PathBuf, process::ExitCode, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use rustgrid_agent::{config::AppContext, runner};

#[derive(Debug, Parser)]
#[command(name = "rustgrid-agent", version, about)]
struct Cli {
    /// Path to the RustGrid agent configuration file.
    #[arg(long, global = true, default_value = ".rustgrid-agent.json")]
    config: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Register this machine as a RustGrid worker.
    Register,
    /// Run one RustGrid ticket in the current Git repository.
    Run {
        ticket_id: String,
        /// Permit a dirty tree. Existing dirty paths are never staged or committed.
        #[arg(long)]
        allow_dirty: bool,
    },
    /// Poll RustGrid for tickets and run them one at a time.
    Watch {
        /// Permit a dirty tree. Existing dirty paths are never staged or committed.
        #[arg(long)]
        allow_dirty: bool,
        /// Seconds between empty queue polls.
        #[arg(long, default_value_t = 15)]
        interval: u64,
        /// Poll once and exit; useful for schedulers and smoke tests.
        #[arg(long)]
        once: bool,
    },
    /// Show local configuration, credentials, repository, and worker status.
    Status,
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let context = AppContext::load(&cli.config)?;

    match cli.command {
        Commands::Register => runner::register(&context),
        Commands::Run {
            ticket_id,
            allow_dirty,
        } => runner::run_ticket(&context, &ticket_id, allow_dirty).map(|_| ()),
        Commands::Watch {
            allow_dirty,
            interval,
            once,
        } => runner::watch(&context, allow_dirty, Duration::from_secs(interval), once),
        Commands::Status => runner::status(&context),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\n[error] {error:#}");
            ExitCode::FAILURE
        }
    }
}
