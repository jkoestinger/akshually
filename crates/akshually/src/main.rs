//! The single `akshually` binary: the CLI agents call, and (`akshually app`) the app itself.

#![warn(clippy::unwrap_used)] // Tests may unwrap; production code may not (CODING_STANDARDS.md).

mod launch;
mod render;

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use akshually_core::ipc::{self, BindError, Connection, Server};
use akshually_core::{Anchor, LocalOptions, NewComment, Outcome, Paths, Service, Settings, Side, ThreadId, forge};

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

/// How long a second app waits for the running one to listen: it may have only just taken the
/// lock, and listens moments later.
const RUNNING_APP_TIMEOUT: Duration = Duration::from_secs(2);
/// How often a second app tries to reach the running one meanwhile.
const RUNNING_APP_RETRY: Duration = Duration::from_millis(50);

/// Exit codes of the blocking calls. 2 is left to usage errors, which clap reports with it.
mod exit {
    pub(crate) const APPROVED: u8 = 0;
    pub(crate) const ERROR: u8 = 1;
    pub(crate) const ABANDONED: u8 = 3;
    pub(crate) const ROUND: u8 = 4;
    pub(crate) const PUBLISHED: u8 = 5;
    pub(crate) const CLOSED: u8 = 6;
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
    /// After `draft` on this branch, it hands your Drafts to the human instead: exits 4 when
    /// they question them, 6 when they close the co-review. With `--pr <url>`, hand your Drafts
    /// on that Pull Request to the human: exits 5 when they Publish (printing the Verdict and
    /// the review's link), 4 when they question your Drafts, 3 when they abandon it.
    Review(ReviewArgs),
    /// Reply to a Thread of this branch's Session (or with `--pr`, of that Pull Request's),
    /// e.g. `akshually reply T3 "Done: …"`. The human sees it at once; returns immediately.
    Reply {
        /// The Pull Request you co-review, instead of this branch.
        #[arg(long, value_name = "URL")]
        pr: Option<String>,
        /// The Thread ID from the Round, e.g. `T3`.
        thread: String,
        /// Markdown.
        body: String,
    },
    /// Mark a Thread of this branch's Session (or with `--pr`, of that Pull Request's)
    /// Resolved, e.g. `akshually resolve T3`. The human can reopen it. Returns immediately.
    Resolve {
        /// The Pull Request you co-review, instead of this branch.
        #[arg(long, value_name = "URL")]
        pr: Option<String>,
        /// The Thread ID from the Round, e.g. `T3`.
        thread: String,
    },
    /// As co-Reviewer of a Pull Request (`--pr <url>`) or of this branch's local work, add a
    /// Draft for the human to keep, edit, discard or question, e.g.
    /// `akshually draft --pr <url> --lines src/lib.rs:2-3 "…"`. Prints its Thread ID. Then run
    /// `akshually review` (with the same `--pr`) to hand your Drafts to the human.
    Draft(DraftArgs),
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
    /// Co-review this Pull Request instead of your local work: the human gets your Drafts.
    #[arg(long, value_name = "URL", conflicts_with_all = ["base", "untracked", "no_untracked", "range"])]
    pr: Option<String>,
}

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("anchor").required(true).args(["lines", "file", "review"])))]
struct DraftArgs {
    /// The Pull Request you co-review; without it, this branch's local work.
    #[arg(long, value_name = "URL")]
    pr: Option<String>,
    /// Anchor to lines of a file, 1-based and inclusive: `src/lib.rs:2-3` or `src/lib.rs:2`.
    #[arg(long, value_name = "PATH:N[-M]", value_parser = parse_lines)]
    lines: Option<(String, u32, u32)>,
    /// With `--lines`: count them in the old version of the file, not the new one.
    #[arg(long, requires = "lines")]
    old: bool,
    /// Anchor to a whole file.
    #[arg(long, value_name = "PATH")]
    file: Option<String>,
    /// Anchor to the Review as a whole.
    #[arg(long)]
    review: bool,
    /// Markdown, `-` to read it from stdin. A ```suggestion fence proposes the exact new code
    /// for new-side lines.
    body: String,
}

impl DraftArgs {
    fn anchor(&self) -> Anchor {
        match (&self.lines, &self.file) {
            (Some((path, start, end)), _) => {
                Anchor::lines(if self.old { Side::Old } else { Side::New }, path, *start..=*end)
            }
            (None, Some(path)) => Anchor::file(path),
            (None, None) => Anchor::Review,
        }
    }
}

/// `PATH:N` or `PATH:N-M`.
fn parse_lines(value: &str) -> Result<(String, u32, u32), String> {
    let bad = || format!("expected PATH:N or PATH:N-M, got `{value}`");
    let (path, range) = value.rsplit_once(':').ok_or_else(bad)?;
    let (start, end) = range.split_once('-').unwrap_or((range, range));
    let (start, end) = (start.parse().map_err(|_| bad())?, end.parse().map_err(|_| bad())?);
    if path.is_empty() || start == 0 || start > end {
        return Err(bad());
    }
    Ok((path.to_owned(), start, end))
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
            match args.pr.clone() {
                Some(url) => co_review(&url, format),
                None => review(args.local(), format),
            }
        }
        Some(Command::Draft(args)) => draft(args),
        Some(Command::Reply { pr, thread, body }) => {
            agent_call(|conn, cwd| conn.reply(cwd, pr.as_deref(), &ThreadId::new(thread), &body).map_err(Into::into))
        }
        Some(Command::Resolve { pr, thread }) => {
            agent_call(|conn, cwd| conn.resolve(cwd, pr.as_deref(), &ThreadId::new(thread)).map_err(Into::into))
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
    let print = |o: &Outcome| print_outcome(o, None, format);
    let outcome = launch::connect_or_launch(&Paths::from_env())?.review(&cwd, local, print)?;
    Ok(exit_code(&outcome))
}

/// `review --pr`: the co-Reviewer hands its Drafts to the human and waits for the outcome.
fn co_review(url: &str, format: Format) -> Result<ExitCode> {
    let outcome =
        launch::connect_or_launch(&Paths::from_env())?.co_review(url, |o| print_outcome(o, Some(url), format))?;
    Ok(exit_code(&outcome))
}

fn draft(args: DraftArgs) -> Result<ExitCode> {
    let anchor = args.anchor();
    let body = match args.body.as_str() {
        "-" => io::read_to_string(io::stdin()).context("can't read the Draft from stdin")?,
        _ => args.body,
    };
    let comment = NewComment::from_markdown(&body);
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    let thread = launch::connect_or_launch(&Paths::from_env())?.draft(&cwd, args.pr.as_deref(), anchor, comment)?;
    println!("{thread}");
    Ok(ExitCode::SUCCESS)
}

/// Prints what `review` received; `pr` is the Pull Request co-reviewed, if any.
fn print_outcome(outcome: &Outcome, pr: Option<&str>, format: Format) -> io::Result<()> {
    let text = match format {
        Format::Markdown => render::outcome_markdown(outcome, pr),
        Format::Json => format!("{}\n", serde_json::to_string(outcome).map_err(io::Error::other)?),
    };
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

fn exit_code(outcome: &Outcome) -> ExitCode {
    let code = match outcome {
        Outcome::Round(_) => exit::ROUND,
        Outcome::Approved => exit::APPROVED,
        Outcome::Abandoned => exit::ABANDONED,
        Outcome::Published { .. } => exit::PUBLISHED,
        Outcome::Closed => exit::CLOSED,
    };
    ExitCode::from(code)
}

/// A call that acts on this branch's Session and returns at once, exiting 0 when it succeeds.
fn agent_call(f: impl FnOnce(Connection, &Path) -> Result<()>) -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    f(launch::connect_or_launch(&Paths::from_env())?, &cwd)?;
    Ok(ExitCode::SUCCESS)
}

/// The app already running on this state dir. It may have only just taken the lock (two
/// launches racing), so give it a moment to listen. One that never answers here listens
/// elsewhere: it was started with another runtime dir.
fn connect_to_running(paths: &Paths) -> Result<Connection> {
    let deadline = std::time::Instant::now() + RUNNING_APP_TIMEOUT;
    loop {
        match ipc::connect(paths) {
            Ok(connection) => return Ok(connection),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(RUNNING_APP_RETRY),
            Err(error) => anyhow::bail!(
                "another akshually app is already running on {}, but it doesn't listen at {} ({error}): it was \
                 started with another XDG_RUNTIME_DIR or AKSHUALLY_RUNTIME_DIR. Run akshually with the same \
                 environment as that app, or quit it",
                paths.state.display(),
                paths.socket().display()
            ),
        }
    }
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
    let service = Service::open(&paths.state)
        .with_context(|| format!("can't open the saved Sessions in {}", paths.state.display()))?
        .with_forges(forges);
    // No window means nothing to notify about, and tests never show real notifications.
    let service = Arc::new(if headless { service } else { service.with_notifier(akshually_ui::DesktopNotifier) });
    service.set_settings(settings);
    let server = match Server::bind(&paths, service.clone()) {
        Ok(server) => server,
        // Another instance serves the CLI already; leave it to that one, and bring its window
        // up, which may have been closed: this is the human opening akshually again.
        Err(BindError::AlreadyRunning) => {
            let running = connect_to_running(&paths)?;
            if !headless {
                let token = std::env::var("XDG_ACTIVATION_TOKEN").ok().filter(|t| !t.is_empty());
                running.show(token).context("can't show the running akshually app")?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        Err(error) => return Err(error.into()),
    };
    if headless {
        let server =
            if std::env::var_os("AKSHUALLY_TEST_CONTROL").is_some() { server.with_test_control() } else { server };
        server.serve().context("can't serve the CLI")?;
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
