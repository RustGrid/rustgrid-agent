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
    Run { ticket_id: String },
    /// Poll RustGrid for tickets and run them one at a time.
    Watch {
        /// Seconds between empty queue polls.
        #[arg(long, default_value_t = 15)]
        interval: u64,
        /// Poll once and exit; useful for schedulers and smoke tests.
        #[arg(long)]
        once: bool,
    },
    /// Run the production worker daemon with continuous supervision.
    Serve {
        /// Seconds between empty queue polls.
        #[arg(long, default_value_t = 15)]
        interval: u64,
    },
    /// Show local configuration, credentials, repository, and worker status.
    Status {
        /// Emit machine-readable JSON for health probes.
        #[arg(long)]
        json: bool,
    },
}

fn run() -> Result<()> {
    rustgrid_agent::shutdown::install()?;
    let cli = Cli::parse();
    let context = AppContext::load(&cli.config)?;

    match cli.command {
        Commands::Register => runner::register(&context),
        Commands::Run { ticket_id } => runner::run_ticket(&context, &ticket_id).map(|_| ()),
        Commands::Watch { interval, once } => {
            runner::watch(&context, Duration::from_secs(interval), once)
        }
        Commands::Serve { interval } => {
            runner::watch(&context, Duration::from_secs(interval), false)
        }
        Commands::Status { json } => runner::status(&context, json),
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
