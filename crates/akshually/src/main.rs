//! The single `akshually` binary: the CLI agents call, and (`akshually app`) the app itself.

mod launch;
mod render;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use akshually_core::ipc::{BindError, Server};
use akshually_core::{LocalOptions, Outcome, Paths, Service, Settings};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Exit codes of the blocking calls. 2 is left to usage errors, which clap reports with it.
/// How often the app looks for changes on disk in Sessions waiting on the human.
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(2);

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
    /// Snapshot the working tree as the Session base before you start working, so `review`
    /// shows the human only your changes, not uncommitted work that was already there.
    /// Returns at once.
    Start,
    /// Ask the human to review your local work: this branch against its merge-base with the
    /// default branch, plus uncommitted changes and untracked files. Blocks until the human
    /// answers. Exits 0 when they approve, 4 when they hand over a Round (printed as markdown:
    /// address it, then run `review` again), 3 when they abandon the Session (stop working on
    /// it), 1 on error.
    Review(ReviewArgs),
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

#[derive(clap::Args)]
struct ReviewArgs {
    /// Review the changes since HEAD's merge-base with this ref, instead of the default branch.
    #[arg(long, value_name = "REF", conflicts_with = "range")]
    base: Option<String>,
    /// Include untracked, non-ignored files, whatever the human's setting says.
    #[arg(long, conflicts_with = "no_untracked")]
    untracked: bool,
    /// Leave out untracked files, whatever the human's setting says.
    #[arg(long)]
    no_untracked: bool,
    /// Review exactly a commit range instead: `<from>..<to>`, or `<from>...<to>` for the
    /// changes since their merge-base. Uncommitted work is not part of it.
    #[arg(value_name = "RANGE", conflicts_with_all = ["untracked", "no_untracked"])]
    range: Option<String>,
}

impl ReviewArgs {
    fn local(self) -> LocalOptions {
        let untracked = match (self.untracked, self.no_untracked) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        };
        LocalOptions { base: self.base, range: self.range, untracked }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Start) => start(),
        Some(Command::Review(args)) => review(args.local()),
        Some(Command::App { headless, rune_args }) => app(headless, rune_args),
        None => app(false, Vec::new()),
    };
    result.unwrap_or_else(|error| {
        eprintln!("akshually: {error:#}");
        ExitCode::from(exit::ERROR)
    })
}

fn start() -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    launch::connect_or_launch(&Paths::from_env())?.start(&cwd)?;
    println!("Session base recorded: `akshually review` will show only the changes made from now on.");
    Ok(ExitCode::SUCCESS)
}

fn review(local: LocalOptions) -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    let outcome = launch::connect_or_launch(&Paths::from_env())?.review(&cwd, local)?;
    let code = match outcome {
        Outcome::Round(round) => {
            print!("{}", render::round_markdown(&round));
            exit::ROUND
        }
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
    service.set_settings(Settings::load(&Settings::file_from_env())?);
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
        // Offers changes on disk during the human's Turn as reloads (the banner).
        service.watch_disk(DISK_CHECK_INTERVAL);
        akshually_ui::run(service, rune_args).map_err(anyhow::Error::msg)?;
    }
    Ok(ExitCode::SUCCESS)
}
