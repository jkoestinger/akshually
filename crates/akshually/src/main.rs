//! The single `akshually` binary: the CLI agents call, and (`akshually app`) the app itself.

mod launch;
mod render;

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use akshually_core::ipc::{BindError, Connection, Server};
use akshually_core::{LocalOptions, Outcome, Paths, Service, Settings, ThreadId, forge};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// The agent skill `agent-help` prints.
const SKILL: &str = include_str!("../../../skills/akshually/SKILL.md");

/// How often the app looks for changes on disk in Sessions waiting on the human.
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// How often open Pull Requests are polled for pushes. Each poll is a conditional request,
/// which GitHub doesn't count against the rate limit while nothing changed.
const PUSH_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// How often the app refreshes the Inbox: 2 search requests per GitHub host each time, well
/// within GitHub's 30 searches a minute.
const INBOX_REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 60);

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
    /// Snapshot the working tree as the Session base before you start working, so `review`
    /// shows the human only your changes, not uncommitted work that was already there.
    /// Returns at once.
    Start,
    /// Ask the human to review your local work: this branch against its merge-base with the
    /// default branch, plus uncommitted changes and untracked files. Blocks until the human
    /// answers. Exits 0 when they approve, 4 when they hand over a Round (printed as markdown,
    /// or JSON with `--format json`: address it, reply to or resolve its Threads, then run
    /// `review` again), 3 when they abandon the Session (stop working on it), 1 on error.
    Review(ReviewArgs),
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
    /// Print the guide for agents: the review loop, the commands and the exit codes. It is
    /// also a `SKILL.md` you can install as an agent skill.
    AgentHelp,
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
    /// How to print the outcome: markdown, or JSON (`{"outcome":"round",...}`).
    #[arg(long, value_enum, default_value_t = Format::Markdown)]
    format: Format,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Format {
    Markdown,
    Json,
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
        Some(Command::Review(args)) => {
            let format = args.format;
            review(args.local(), format)
        }
        Some(Command::Reply { thread, body }) => {
            agent_call(|conn, cwd| conn.reply(cwd, &ThreadId::new(thread), &body).map_err(Into::into))
        }
        Some(Command::Resolve { thread }) => {
            agent_call(|conn, cwd| conn.resolve(cwd, &ThreadId::new(thread)).map_err(Into::into))
        }
        Some(Command::AgentHelp) => {
            print!("{SKILL}");
            Ok(ExitCode::SUCCESS)
        }
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

fn review(local: LocalOptions, format: Format) -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    // Printed and flushed before the app hears back, so the outcome is acknowledged only once
    // the agent can see it.
    let outcome = launch::connect_or_launch(&Paths::from_env())?.review(&cwd, local, |outcome| {
        let text = match format {
            Format::Markdown => render::outcome_markdown(outcome),
            Format::Json => format!("{}\n", serde_json::to_string(outcome).map_err(io::Error::other)?),
        };
        let mut stdout = io::stdout().lock();
        stdout.write_all(text.as_bytes())?;
        stdout.flush()
    })?;
    let code = match outcome {
        Outcome::Round(_) => exit::ROUND,
        Outcome::Approved => exit::APPROVED,
        Outcome::Abandoned => exit::ABANDONED,
    };
    Ok(ExitCode::from(code))
}

/// A call that acts on this branch's Session and returns at once, exiting 0 when it succeeds.
fn agent_call(f: impl FnOnce(Connection, &Path) -> Result<()>) -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    f(launch::connect_or_launch(&Paths::from_env())?, &cwd)?;
    Ok(ExitCode::SUCCESS)
}

fn app(headless: bool, rune_args: Vec<String>) -> Result<ExitCode> {
    let settings = Settings::load(&Settings::file_from_env())?;
    if !headless && akshually_ui::is_headless_run(&rune_args) {
        // A headless render (`--snap`, ...) of the UI alone: no socket, no state.
        let service = Arc::new(Service::new());
        service.set_settings(settings);
        akshually_ui::run(service, rune_args).map_err(anyhow::Error::msg)?;
        return Ok(ExitCode::SUCCESS);
    }
    let paths = Paths::from_env();
    // Only reads the saved Sessions: it writes nothing until a Session changes, which a
    // second instance never sees, since it quits below.
    let forges = forge::adapters(&settings.hosts, &forge::CredentialSources::from_env());
    let service = Service::open(&paths.state)?.with_forges(forges);
    // No window means nothing to notify about, and tests never show real notifications.
    let service = Arc::new(if headless { service } else { service.with_notifier(akshually_ui::DesktopNotifier) });
    service.set_settings(settings);
    let server = match Server::bind(&paths, service.clone()) {
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
        // The headless app (tests) never asks the Forges on its own.
        service.watch_inbox(INBOX_REFRESH_INTERVAL);
        // Offers pushes to open Pull Requests as reloads (the banner).
        service.watch_pull_requests(PUSH_CHECK_INTERVAL);
        akshually_ui::run(service, rune_args).map_err(anyhow::Error::msg)?;
    }
    Ok(ExitCode::SUCCESS)
}
