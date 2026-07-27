use std::{path::PathBuf, process::ExitCode, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use rustgrid_agent::{config::AppContext, runner};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ExecutionProvider {
    GithubActions,
}

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
    /// Execute one ephemeral mission dispatched by an execution provider.
    Execute {
        /// Hosted execution provider that authenticated this process.
        #[arg(long, value_enum)]
        provider: ExecutionProvider,
        /// RustGrid mission execution identifier.
        #[arg(long)]
        execution_id: Uuid,
    },
    /// Best-effort terminal callback for a hosted workflow that failed before execution completed.
    ReportEmergencyFailure {
        /// Hosted execution provider that authenticated this process.
        #[arg(long, value_enum)]
        provider: ExecutionProvider,
        /// RustGrid mission execution identifier.
        #[arg(long)]
        execution_id: Uuid,
    },
}

fn run() -> Result<()> {
    if rustgrid_agent::command::contained_child_requested() {
        return rustgrid_agent::command::exec_contained_child();
    }
    rustgrid_agent::shutdown::install()?;
    let cli = Cli::parse();
    match &cli.command {
        Commands::Execute {
            provider: ExecutionProvider::GithubActions,
            execution_id,
        } => return rustgrid_agent::hosted::execute_github_actions(*execution_id),
        Commands::ReportEmergencyFailure {
            provider: ExecutionProvider::GithubActions,
            execution_id,
        } => return rustgrid_agent::hosted::report_emergency_failure(*execution_id),
        _ => {}
    }
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
        Commands::Execute { .. } | Commands::ReportEmergencyFailure { .. } => {
            unreachable!("hosted commands return before local configuration is resolved")
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
