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
        /// Exit once the run assigned to this worker has finished. Used on
        /// RustGrid-hosted VMs, which run exactly one task.
        #[arg(long)]
        exit_after_run: bool,
    },
    /// Set up a RustGrid-hosted VM from its one-time bootstrap token.
    Bootstrap {
        /// RustGrid control-plane instance URL.
        #[arg(long, value_name = "URL")]
        instance: String,
        /// File holding the one-time token; deleted after it is read.
        #[arg(long, value_name = "PATH")]
        token_file: PathBuf,
        /// Unprivileged user the coding agent and quality gates run as.
        #[arg(long, value_name = "USER")]
        run_as: String,
        /// Directory for the run's repository workspace.
        #[arg(long, value_name = "PATH")]
        workspace_root: PathBuf,
    },
    /// Trace Claude Code sessions on this computer onto RustGrid tickets.
    Trace {
        #[command(subcommand)]
        command: TraceCommand,
    },
    /// Show local configuration, credentials, repository, and worker status.
    Status {
        /// Emit machine-readable JSON for health probes.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TraceCommand {
    /// Handle one Claude Code hook; reads the hook's JSON from stdin.
    Hook {
        /// The hook event, such as SessionStart, UserPromptSubmit or SessionEnd.
        event: String,
    },
    /// Report what is new in a session's transcript (started by hooks).
    Flush {
        #[arg(long)]
        session: String,
    },
    /// Set up Claude Code on this computer so every session is traced.
    ///
    /// By default the hooks go into Claude Code's managed settings, which
    /// users cannot turn off (sudo asks for your password for that step).
    /// Also signs this computer in if needed and checks that tracing works.
    Install {
        /// Use your own Claude Code settings instead; you can remove them.
        #[arg(long)]
        user: bool,
        /// Do not sign in, for example when device management runs this.
        #[arg(long)]
        no_login: bool,
        /// Print the sign-in URL without launching a browser.
        #[arg(long)]
        no_browser: bool,
        /// RustGrid control-plane instance URL to sign in to.
        #[arg(long, value_name = "URL")]
        instance: Option<String>,
        /// Only write the managed hooks (run through sudo by `install`).
        #[arg(long, hide = true)]
        system_only: bool,
        /// The binary the hooks run (passed through sudo by `install`).
        #[arg(long, hide = true)]
        binary: Option<std::path::PathBuf>,
    },
    /// Show whether Claude Code sessions on this computer are traced.
    Status,
    /// Stop tracing Claude Code sessions on this computer.
    Uninstall {
        /// Only remove the hooks from your own Claude Code settings.
        #[arg(long)]
        user: bool,
        /// Only remove the managed hooks (run through sudo by `uninstall`).
        #[arg(long, hide = true)]
        system_only: bool,
    },
}

/// Runs a Claude Code hook. Exit code 2 blocks the prompt and shows the
/// reason; tracing problems are logged and never block.
fn run_hook(config_path: &std::path::Path, event: &str) -> ExitCode {
    let mut stdin = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin).is_err() {
        return ExitCode::SUCCESS;
    }
    let Ok(input) = serde_json::from_str(&stdin) else {
        return ExitCode::SUCCESS;
    };
    match rustgrid_agent::trace::hook(config_path, event, input) {
        Ok(rustgrid_agent::trace::HookOutcome::Block(reason)) => {
            eprintln!("{reason}");
            ExitCode::from(2)
        }
        Ok(rustgrid_agent::trace::HookOutcome::Context(context)) => {
            println!("{context}");
            ExitCode::SUCCESS
        }
        Ok(rustgrid_agent::trace::HookOutcome::Continue) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("[rustgrid trace] {error:#}");
            ExitCode::SUCCESS
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    rustgrid_agent::shutdown::install()?;
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
            Ok(runner::register(&context)?)
        }
        Commands::Run { ticket_id } => {
            let context = AppContext::load(&config_path)?;
            Ok(runner::run_ticket(&context, &ticket_id).map(|_| ())?)
        }
        Commands::Watch { interval, once } => {
            let context = AppContext::load(&config_path)?;
            Ok(runner::watch(
                &context,
                Duration::from_secs(interval),
                once,
            )?)
        }
        Commands::Serve {
            interval,
            exit_after_run,
        } => {
            let context = AppContext::load(&config_path)?;
            Ok(runner::serve(
                &context,
                Duration::from_secs(interval),
                exit_after_run,
            )?)
        }
        Commands::Bootstrap {
            instance,
            token_file,
            run_as,
            workspace_root,
        } => rustgrid_agent::auth::bootstrap(
            &config_path,
            &instance,
            &token_file,
            &run_as,
            &workspace_root,
        ),
        Commands::Trace {
            command: TraceCommand::Flush { session },
        } => rustgrid_agent::trace::flush(&config_path, &session),
        Commands::Trace {
            command: TraceCommand::Hook { .. },
        } => unreachable!("hooks are handled before configuration loads"),
        Commands::Trace {
            command:
                TraceCommand::Install {
                    user,
                    no_login,
                    no_browser,
                    instance,
                    system_only,
                    binary,
                },
        } => {
            use rustgrid_agent::trace::install;
            if system_only {
                let binary = match binary {
                    Some(binary) => binary,
                    None => install::stable_binary_path(&std::env::current_exe()?),
                };
                return install::install_managed(&binary);
            }
            install::install(
                &config_path,
                install::InstallOptions {
                    scope: if user {
                        install::Scope::User
                    } else {
                        install::Scope::Managed
                    },
                    login: !no_login,
                    open_browser: !no_browser,
                    instance,
                },
            )
        }
        Commands::Trace {
            command: TraceCommand::Status,
        } => rustgrid_agent::trace::install::status(&config_path),
        Commands::Trace {
            command: TraceCommand::Uninstall { user, system_only },
        } => {
            use rustgrid_agent::trace::install;
            if system_only {
                install::uninstall_system_only()
            } else {
                install::uninstall(user.then_some(install::Scope::User))
            }
        }
        Commands::Status { json } => {
            let context = AppContext::load(&config_path)?;
            runner::status(&context, json)
        }
    }
}

fn main() -> ExitCode {
    // Hooks must answer Claude Code quickly and never fail a prompt over tracing.
    let cli = Cli::parse();
    if let Commands::Trace {
        command: TraceCommand::Hook { event },
    } = &cli.command
    {
        let Ok(config_path) = rustgrid_agent::config::resolve_config_path(cli.config.as_deref())
        else {
            return ExitCode::SUCCESS;
        };
        return run_hook(&config_path, event);
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\n[error] {error:#}");
            ExitCode::FAILURE
        }
    }
}
