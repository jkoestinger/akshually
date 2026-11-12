//! The single `akshually` binary: the CLI agents call, and (`akshually app`) the app itself.

mod launch;

use std::process::ExitCode;
use std::sync::Arc;

use akshually_core::ipc::{BindError, Server};
use akshually_core::{Outcome, Paths, Service};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Exit codes of the blocking calls. 2 is left to usage errors, which clap reports with it.
mod exit {
    pub const APPROVED: u8 = 0;
    pub const ERROR: u8 = 1;
    pub const ABANDONED: u8 = 3;
}

#[derive(Parser)]
#[command(version, about = "Human-in-the-loop code review for agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ask the human to review your local work: this branch against its merge-base with the
    /// default branch, plus uncommitted changes. Blocks until the human answers. Exits 0 when
    /// they approve, 3 when they abandon the Session (stop working on it), 1 on error.
    Review,
    /// Run the app (launched by the first CLI call). Arguments after the options go to Rune,
    /// e.g. `--snap out.png --scene session` for a headless render.
    #[command(hide = true)]
    App {
        /// Serve the CLI without any UI (for tests).
        #[arg(long)]
        headless: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rune_args: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Review => review(),
        Command::App { headless, rune_args } => app(headless, rune_args),
    };
    result.unwrap_or_else(|error| {
        eprintln!("akshually: {error:#}");
        ExitCode::from(exit::ERROR)
    })
}

fn review() -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    let outcome = launch::connect_or_launch(&Paths::from_env())?.review(&cwd)?;
    let code = match outcome {
        Outcome::Approved => {
            println!("Approved.");
            exit::APPROVED
        }
        Outcome::Abandoned => {
            println!("Abandoned: the human closed this Session without a Verdict. Stop working on this change.");
            exit::ABANDONED
        }
    };
    Ok(ExitCode::from(code))
}

fn app(headless: bool, rune_args: Vec<String>) -> Result<ExitCode> {
    let service = Arc::new(Service::new());
    if !headless && akshually_ui::is_headless_run(&rune_args) {
        // A headless render (`--snap`, ...) of the UI alone: no socket, no state.
        akshually_ui::run(service, rune_args).map_err(anyhow::Error::msg)?;
        return Ok(ExitCode::SUCCESS);
    }
    let server = match Server::bind(&Paths::from_env(), service.clone()) {
        Ok(server) => server,
        // Another instance serves the CLI already; leave it to that one.
        Err(BindError::AlreadyRunning) => return Ok(ExitCode::SUCCESS),
        Err(error) => return Err(error.into()),
    };
    if headless {
        let server =
            if std::env::var_os("AKSHUALLY_TEST_CONTROL").is_some() { server.with_test_control() } else { server };
        server.serve()?;
    } else {
        std::thread::spawn(move || server.serve());
        akshually_ui::run(service, rune_args).map_err(anyhow::Error::msg)?;
    }
    Ok(ExitCode::SUCCESS)
}
