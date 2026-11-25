//! The single `akshually` binary: the CLI agents call, and (`akshually app`) the app itself.

#![warn(clippy::unwrap_used)] // Tests may unwrap; production code may not (CODING_STANDARDS.md).

mod launch;
mod render;

use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use akshually_core::{
    Anchor, LocalOptions, NewComment, Outcome, Paths, Service, Settings, Side, ThreadId, forge,
    ipc::{self, BindError, ClientError, Connection, ErrorCode, Server},
    update::{self, Releases, Updater},
};

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

/// How often the running app looks for a newer release, after the look at launch (ADR 0003).
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a second app waits for the running one to listen: it may have only just taken the
/// lock, and listens moments later.
const RUNNING_APP_TIMEOUT: Duration = Duration::from_secs(2);
/// How often a second app tries to reach the running one meanwhile.
const RUNNING_APP_RETRY: Duration = Duration::from_millis(50);

/// What a call that fails exits with, its error explained on stderr. 2 is left to usage errors,
/// which clap reports with it. Every outcome of `review` exits 0: agent shells show any other
/// code as a failed command, so stdout tells the outcomes apart (ADR 0002).
const EXIT_ERROR: u8 = 1;

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
    /// answers, prints what they did (markdown, or JSON with `--format json`) and exits 0; the
    /// first line says which outcome it was: `# Round N` (address it, reply to or resolve its
    /// Threads, then run `review` again), `Approved.`, or `Abandoned: …` (stop working on it).
    /// Exits 1 on error. After `draft` on this branch, it hands your Drafts to the human
    /// instead: a Round holds their questions, and `Closed: …` ends the co-review. With
    /// `--pr <url>`, hand your Drafts on that Pull Request to the human: `Published with the
    /// Verdict …` gives the Verdict and the review's link; on a Stack of Pull Requests, `Published: …`
    /// lists each one.
    Review(ReviewArgs),
    /// Reply to a Thread of this branch's Session (or with `--pr`, of that Pull Request's),
    /// e.g. `akshually reply T3 "Done: …"`. The human sees it at once; returns immediately.
    Reply {
        /// The Pull Request you co-review, instead of this branch.
        #[arg(long, value_name = "URL")]
        pr: Option<String>,
        /// The Thread ID from the Round, e.g. `T3`.
        thread: String,
        /// Markdown, `-` to read it from stdin.
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
    /// Update akshually to the newest release, in place: the newest stable one, or with a
    /// pre-release installed, the newest of any. Tells a running app, which then offers to
    /// restart into it. Exits 0 when it updated or was already up to date, 1 on error.
    Update,
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

impl Command {
    /// The Pull Request the command names with `--pr`, if any.
    fn pr(&self) -> Option<&str> {
        match self {
            Command::Review(ReviewArgs { pr, .. })
            | Command::Draft(DraftArgs { pr, .. })
            | Command::Reply { pr, .. }
            | Command::Resolve { pr, .. } => pr.as_deref(),
            Command::Start | Command::AgentHelp | Command::Update | Command::App { .. } => None,
        }
    }
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
    let pr = cli.command.as_ref().and_then(Command::pr).map(str::to_owned);
    let reviewing = matches!(cli.command, Some(Command::Review(_)));
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
        Some(Command::Reply { pr, thread, body }) => read_body(body).and_then(|body| {
            agent_call(|conn, cwd| conn.reply(cwd, pr.as_deref(), &ThreadId::new(thread), &body).map_err(Into::into))
        }),
        Some(Command::Resolve { pr, thread }) => {
            agent_call(|conn, cwd| conn.resolve(cwd, pr.as_deref(), &ThreadId::new(thread)).map_err(Into::into))
        }
        Some(Command::AgentHelp) => {
            print!("{SKILL}");
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Update) => update(),
        Some(Command::App { headless, rune_args }) => app(headless, rune_args),
        None => app(false, Vec::new()),
    };
    result.unwrap_or_else(|error| {
        eprintln!("akshually: {error:#}{}", hint(&error, pr.as_deref(), reviewing));
        ExitCode::from(EXIT_ERROR)
    })
}

/// What the agent can do about `error`, said after the app's own message, which states only the
/// fact; empty when there's nothing to add. `pr` is the command's `--pr`, and `reviewing` says
/// the command was `review`. Picked by the reply's `ErrorCode`, never by its wording.
fn hint(error: &anyhow::Error, pr: Option<&str>, reviewing: bool) -> String {
    let code = error.chain().find_map(|e| match e.downcast_ref::<ClientError>() {
        Some(ClientError::App(reply)) => Some(reply.code),
        _ => None,
    });
    let review = match pr {
        Some(url) => format!("`akshually review --pr {url}`"),
        None => "`akshually review`".to_owned(),
    };
    match code {
        // `review` itself is refused as `Ended` only when there's nothing left to collect.
        Some(ErrorCode::Ended) if reviewing => String::new(),
        Some(ErrorCode::Ended) => format!("; your next {review} collects its outcome"),
        Some(ErrorCode::NoSession) => format!("; run {review} first"),
        _ => String::new(),
    }
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
    launch::connect_or_launch(&Paths::from_env())?.review(&cwd, local, print)?;
    Ok(ExitCode::SUCCESS)
}

/// `review --pr`: the co-Reviewer hands its Drafts to the human and waits for the outcome.
fn co_review(url: &str, format: Format) -> Result<ExitCode> {
    launch::connect_or_launch(&Paths::from_env())?.co_review(url, |o| print_outcome(o, Some(url), format))?;
    Ok(ExitCode::SUCCESS)
}

fn draft(args: DraftArgs) -> Result<ExitCode> {
    let anchor = args.anchor();
    let body = read_body(args.body)?;
    let comment = NewComment::from_markdown(&body);
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    let thread = launch::connect_or_launch(&Paths::from_env())?.draft(&cwd, args.pr.as_deref(), anchor, comment)?;
    println!("{thread}");
    Ok(ExitCode::SUCCESS)
}

/// The markdown body of a `reply` or `draft`: `body` as given, or all of stdin when it is `-`.
fn read_body(body: String) -> Result<String> {
    match body.as_str() {
        "-" => io::read_to_string(io::stdin()).context("can't read the Comment from stdin"),
        _ => Ok(body),
    }
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

/// A call that acts on this branch's Session and returns at once, exiting 0 when it succeeds.
fn agent_call(f: impl FnOnce(Connection, &Path) -> Result<()>) -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("can't read the current directory")?;
    f(launch::connect_or_launch(&Paths::from_env())?, &cwd)?;
    Ok(ExitCode::SUCCESS)
}

/// The running executable's real path, symlinks resolved: the install an update replaces, and
/// the binary a restart runs. Resolve it before any update, as Linux then names the replaced
/// binary "(deleted)".
fn install_path() -> Result<std::path::PathBuf> {
    std::env::current_exe().and_then(|p| p.canonicalize()).context("can't find the akshually binary")
}

/// `update`: installs the newest release this install is offered, then tells the running app,
/// if any. This binary is the old one, so it speaks the running app's protocol.
fn update() -> Result<ExitCode> {
    let updater = Updater::new(Releases::from_env(), install_path()?);
    let running = updater.running().clone();
    let Some(release) = updater.newer().context("can't look for a newer release of akshually")? else {
        println!("akshually {running} is up to date");
        return Ok(ExitCode::SUCCESS);
    };
    updater.install(&release).with_context(|| format!("can't update akshually to {}", release.version))?;
    let version = &release.version;
    // Never launches the app: only one already running needs telling.
    match ipc::connect(&Paths::from_env()) {
        Ok(app) => {
            // Best-effort: an app too old to understand still runs the old binary until restarted.
            let _ = app.update_installed(version);
            println!("updated akshually {running} → {version}; restart the app to use it");
        }
        Err(_) => println!("updated akshually {running} → {version}"),
    }
    Ok(ExitCode::SUCCESS)
}

/// Replaces this process with `akshually app` from `exe`, as the update notice's Restart does:
/// the single-instance lock and the socket close with the old image, so the new one takes
/// them, and Sessions come back from disk. Returns only if it couldn't.
fn relaunch(exe: &Path, headless: bool) -> io::Error {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("app");
    if headless {
        cmd.arg("--headless");
    }
    cmd.exec()
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

/// Brings the Linux launcher entry and icon up to this version's, when the entry is installed: an
/// update replaces only the binary, so this is how an existing install gets the icon. Logged, never
/// fatal: the launcher is cosmetic.
fn repair_launcher() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let Some(data_home) = dirs::data_dir() else { return };
    if let Err(error) = akshually_core::repair_launcher(&data_home) {
        eprintln!("akshually: can't update the launcher entry: {error}");
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
    let exe = install_path()?;
    // Only reads the saved Sessions: it writes nothing until a Session changes, which a
    // second instance never sees, since it quits below.
    let forges = forge::adapters(&settings.hosts, &forge::CredentialSources::from_env());
    let service = Service::open(&paths.state)
        .with_context(|| format!("can't open the saved Sessions in {}", paths.state.display()))?
        .with_forges(forges)
        .with_updater(Updater::new(Releases::from_env(), exe.clone()));
    // No window means nothing to notify about, and tests never show real notifications.
    let service = Arc::new(if headless { service } else { service.with_notifier(akshually_ui::DesktopNotifier) });
    service.set_settings(settings);
    // An Update may have renamed the macOS bundle, so the binary to start isn't always at `exe`.
    service.on_restart(move || relaunch(&update::installed_exe(&exe), headless));
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
        repair_launcher();
        let notifying_service = service.clone();
        std::thread::spawn(move || {
            // Logged and notified rather than fatal: the window stays up (ADR 0002), though
            // agents can't reach the app any more (#36).
            if let Err(error) = server.serve().context("can't serve the CLI") {
                eprintln!("akshually: {error:#}");
                notifying_service.local_channel_lost();
            }
        });
        // Offers changes on disk during the human's Turn as reloads (the banner).
        service.watch_disk(DISK_CHECK_INTERVAL);
        // The headless app (tests) never asks the Forges on its own.
        service.watch_inbox(INBOX_REFRESH_INTERVAL);
        // Offers pushes to open Pull Requests as reloads (the banner).
        service.watch_pull_requests(PUSH_CHECK_INTERVAL);
        // At launch, then daily; the headless app (tests) never looks.
        service.watch_updates(UPDATE_CHECK_INTERVAL);
        akshually_ui::run(service, rune_args).map_err(anyhow::Error::msg)?;
    }
    Ok(ExitCode::SUCCESS)
}
