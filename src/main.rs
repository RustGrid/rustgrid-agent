use std::{path::PathBuf, process::ExitCode, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use rustgrid_agent::{config::AppContext, runner};

#[derive(Debug, Parser)]
#[command(name = "rustgrid-agent", version, about)]
struct Cli {
    /// Path to the RustGrid agent configuration file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create or update a production-ready worker configuration.
    Setup {
        /// Maximum number of isolated jobs to run concurrently.
        #[arg(long, value_name = "JOBS")]
        max_concurrency: Option<usize>,
    },
    /// Authenticate this worker using a browser and one-time code.
    Login {
        /// Print the URL without launching a browser.
        #[arg(long)]
        no_browser: bool,
        /// RustGrid control-plane instance URL (the /api/v1 suffix is optional).
        #[arg(long, value_name = "URL")]
        instance: Option<String>,
    },
    /// Deprecated compatibility command. Use `login` instead.
    Register,
    /// Revoke this worker credential and remove it from local secure storage.
    Logout,
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
    let config_path = match &cli.command {
        Commands::Setup { .. } => rustgrid_agent::setup::setup_config_path(cli.config.as_deref())?,
        _ => rustgrid_agent::config::resolve_config_path(cli.config.as_deref())?,
    };

    match cli.command {
        Commands::Setup { max_concurrency } => {
            rustgrid_agent::setup::run(&config_path, max_concurrency)
        }
        Commands::Login {
            no_browser,
            instance,
        } => {
            let mut context = AppContext::load_for_login(&config_path, instance.as_deref())?;
            rustgrid_agent::auth::login(&mut context, !no_browser)
        }
        Commands::Logout => {
            let mut context = AppContext::load(&config_path)?;
            rustgrid_agent::auth::logout(&mut context)
        }
        Commands::Register => {
            eprintln!("[warning] `register` is deprecated; use `rustgrid-agent login`");
            let context = AppContext::load(&config_path)?;
            runner::register(&context)
        }
        Commands::Run { ticket_id } => {
            let context = AppContext::load(&config_path)?;
            runner::run_ticket(&context, &ticket_id).map(|_| ())
        }
        Commands::Watch { interval, once } => {
            let context = AppContext::load(&config_path)?;
            runner::watch(&context, Duration::from_secs(interval), once)
        }
        Commands::Serve { interval } => {
            let context = AppContext::load(&config_path)?;
            runner::serve(&context, Duration::from_secs(interval))
        }
        Commands::Status { json } => {
            let context = AppContext::load(&config_path)?;
            runner::status(&context, json)
        }
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
