//! The single `akshually` binary: the CLI agents call, and (`akshually app`) the app itself.

mod launch;
mod render;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use akshually_core::ipc::{BindError, Connection, Server};
use akshually_core::{Outcome, Paths, Service, ThreadId};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Exit codes of the blocking calls. 2 is left to usage errors, which clap reports with it.
mod exit {
    pub const APPROVED: u8 = 0;
    pub const ERROR: u8 = 1;
    pub const ABANDONED: u8 = 3;
    pub const ROUND: u8 = 4;
}

#[derive(Parser)]
#[command(version, about = "Human-in-the-loop code review for agents")]
struct Cli {
    /// With no command, opens the app (as the `.desktop` entry and the macOS `.app` do).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Ask the human to review your local work: this branch against its merge-base with the
    /// default branch, plus uncommitted changes. Blocks until the human answers. Exits 0 when
    /// they approve, 4 when they hand over a Round (printed as markdown, or JSON with
    /// `--format json`: address it, reply to or resolve its Threads, then run `review` again), 3 when they abandon the Session (stop working on it), 1 on error.
    Review {
        /// How to print the outcome: markdown, or JSON (`{"outcome":"round",...}`).
        #[arg(long, value_enum, default_value_t = Format::Markdown)]
        format: Format,
    },
    /// Reply to a Thread of this branch's Session, e.g. `akshually reply T3 "Done: …"`. The
    /// human sees it at once; returns immediately.
    Reply {
        /// The Thread ID from the Round, e.g. `T3`.
        thread: String,
        /// Markdown.
        body: String,
    },
    /// Mark a Thread of this branch's Session Resolved, e.g. `akshually resolve T3`. The human
    /// can reopen it. Returns immediately.
    Resolve {
        /// The Thread ID from the Round, e.g. `T3`.
        thread: String,
    },
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
        Some(Command::Review { format }) => review(format),
        Some(Command::Reply { thread, body }) => {
            agent_call(|conn, cwd| conn.reply(cwd, &ThreadId::new(thread), &body).map_err(Into::into))
        }
        Some(Command::Resolve { thread }) => {
            agent_call(|conn, cwd| conn.resolve(cwd, &ThreadId::new(thread)).map_err(Into::into))
        }
        Some(Command::App { headless, rune_args }) => app(headless, rune_args),
        None => app(false, Vec::new()),
    };
    result.unwrap_or_else(|error| {
        eprintln!("akshually: {error:#}");
        ExitCode::from(exit::ERROR)
    })
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Format {
    Markdown,
    Json,
}

fn review(format: Format) -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    let outcome = launch::connect_or_launch(&Paths::from_env())?.review(&cwd)?;
    let code = match outcome {
        Outcome::Round(_) => exit::ROUND,
        Outcome::Approved => exit::APPROVED,
        Outcome::Abandoned => exit::ABANDONED,
    };
    match format {
        Format::Markdown => print!("{}", render::outcome_markdown(&outcome)),
        Format::Json => println!("{}", serde_json::to_string(&outcome).context("can't print the outcome as JSON")?),
    }
    Ok(ExitCode::from(code))
}

/// A call that acts on this branch's Session and returns at once, exiting 0 when it succeeds.
fn agent_call(f: impl FnOnce(Connection, &Path) -> Result<()>) -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    f(launch::connect_or_launch(&Paths::from_env())?, &cwd)?;
    Ok(ExitCode::SUCCESS)
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
