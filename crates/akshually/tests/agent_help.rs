//! Seam B: `akshually agent-help` and the shipped skill it prints.

mod support;

use std::collections::BTreeSet;
use std::process::Output;

use akshually_core::{Anchor, Service, SessionId, Side, Verdict};
use akshually_testkit::TestRepo;
use support::{HostedApp, PR_URL, finish};

/// The shipped skill, which `agent-help` prints.
const SKILL: &str = include_str!("../../../skills/akshually/SKILL.md");

fn agent_help() -> String {
    let app = HostedApp::start();
    let out = app.akshually(&std::env::temp_dir(), &["agent-help"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).expect("UTF-8")
}

#[test]
fn agent_help_prints_the_shipped_skill() {
    assert_eq!(agent_help(), SKILL);
}

/// The subcommands the skill names, as in `akshually reply <thread> <body>`.
fn documented_subcommands() -> BTreeSet<String> {
    SKILL
        .split("`akshually ")
        .skip(1)
        .map(|rest| rest.chars().take_while(|c| c.is_ascii_lowercase() || *c == '-').collect::<String>())
        .filter(|word| !word.is_empty())
        .collect()
}

/// The subcommands `akshually --help` lists (the hidden `app` isn't one).
fn cli_subcommands() -> BTreeSet<String> {
    let app = HostedApp::start();
    let out = app.akshually(&std::env::temp_dir(), &["--help"]).output().unwrap();
    let help = String::from_utf8(out.stdout).expect("UTF-8");
    let commands = help.split("Commands:\n").nth(1).expect("a Commands section");
    commands
        .lines()
        .take_while(|line| line.starts_with("  "))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_owned)
        .collect()
}

/// The subcommands for the human alone, which `--help` lists and the agents' skill leaves out.
const HUMAN_SUBCOMMANDS: [&str; 1] = ["update"];

#[test]
fn the_skill_documents_exactly_the_cli_subcommands_for_agents() {
    let for_agents: BTreeSet<String> =
        cli_subcommands().into_iter().filter(|c| !HUMAN_SUBCOMMANDS.contains(&c.as_str())).collect();

    assert_eq!(documented_subcommands(), for_agents);
}

/// The codes in the skill's exit code table, rows like ``| `4` | A Round was handed over |``.
fn documented_exit_codes() -> BTreeSet<i32> {
    let table = SKILL.split("## Exit codes\n").nth(1).expect("an Exit codes section");
    table
        .lines()
        .filter_map(|row| row.strip_prefix("| `")?.split_once('`'))
        .map(|(code, _)| code.parse().expect("a numeric exit code"))
        .collect()
}

fn repo_with_work() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("add b");
    repo
}

/// Runs `review args` in a repository with work and lets the human answer it with `human`.
fn review_answered(args: &[&str], human: impl FnOnce(&Service, &SessionId)) -> Output {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let child = app.akshually(repo.path(), &[&["review"], args].concat()).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    human(&app.service, &session.id);
    finish(child)
}

fn review_approved(args: &[&str]) -> Output {
    review_answered(args, |service, id| service.approve(id).unwrap())
}

fn review_abandoned(args: &[&str]) -> Output {
    review_answered(args, |service, id| service.abandon(id).unwrap())
}

fn review_round_handed_over(args: &[&str]) -> Output {
    review_answered(args, |service, id| {
        service.comment(id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b?").unwrap();
        service.hand_over_round(id).unwrap();
    })
}

/// Runs `review --pr args` and lets the human Publish the Pull Request.
fn co_review_published(args: &[&str]) -> Output {
    let app = HostedApp::with_pull_request();
    let child = app.akshually(&std::env::temp_dir(), &[&["review", "--pr", PR_URL], args].concat()).spawn().unwrap();
    let session = app.wait_for_co_reviewer();
    app.service.publish(&session.id, Verdict::Approve, "").unwrap();
    finish(child)
}

/// Drafts on local work, runs `review args` and lets the human close the co-review.
fn local_co_review_closed(args: &[&str]) -> Output {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let drafted = app.akshually(repo.path(), &["draft", "--review", "Small."]).output().unwrap();
    assert_eq!(drafted.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&drafted.stderr));
    let child = app.akshually(repo.path(), &[&["review"], args].concat()).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.close(&session.id).unwrap();
    finish(child)
}

/// What the CLI exits with in a situation the skill gives `code` for, or `None` when there is
/// no such situation.
fn exit_code_reached_for(code: i32) -> Option<i32> {
    let outside_a_repo =
        |args: &[&str]| HostedApp::start().akshually(&std::env::temp_dir(), args).output().unwrap().status.code();
    match code {
        0 => review_approved(&[]).status.code(),
        1 => outside_a_repo(&["review"]),
        2 => outside_a_repo(&["review", "--bogus"]),
        _ => None,
    }
}

#[test]
fn every_exit_code_the_skill_documents_is_one_the_cli_exits_with() {
    let documented = documented_exit_codes();
    assert_eq!(documented, BTreeSet::from([0, 1, 2]), "every exit code the CLI has");
    for code in documented {
        assert_eq!(exit_code_reached_for(code), Some(code), "exit code {code}");
    }
}

/// An outcome the skill lists: the first line `review` prints, with `N` for a number and a
/// trailing `…` for the rest of the line, and its `outcome` with `--format json`. Rows like
/// ``- `# Round N` (JSON `round`): …``.
struct DocumentedOutcome {
    first_line: String,
    json: String,
}

impl DocumentedOutcome {
    fn matches(&self, line: &str) -> bool {
        let mut numbered = String::new();
        let mut in_number = false;
        for c in line.chars() {
            match (c.is_ascii_digit(), in_number) {
                (true, true) => {}
                (true, false) => numbered.push('N'),
                (false, _) => numbered.push(c),
            }
            in_number = c.is_ascii_digit();
        }
        match self.first_line.strip_suffix('…') {
            Some(start) => numbered.starts_with(start),
            None => numbered == self.first_line,
        }
    }
}

/// The outcomes in the skill's "What `review` prints" list.
fn documented_outcomes() -> Vec<DocumentedOutcome> {
    let section = SKILL.split("## What `review` prints\n").nth(1).expect("a What `review` prints section");
    let section = section.split("\n## ").next().expect("the section's text");
    section
        .lines()
        .filter_map(|row| {
            let (first_line, rest) = row.strip_prefix("- `")?.split_once('`')?;
            let json = rest.strip_prefix(" (JSON `")?.split_once('`')?.0;
            Some(DocumentedOutcome { first_line: first_line.to_owned(), json: json.to_owned() })
        })
        .collect()
}

#[test]
fn every_outcome_the_skill_lists_is_a_first_line_and_json_outcome_the_cli_prints() {
    let documented = documented_outcomes();
    let reach_outcome: [fn(&[&str]) -> Output; 5] =
        [review_round_handed_over, review_approved, review_abandoned, co_review_published, local_co_review_closed];

    let printed: Vec<(Output, Output)> =
        reach_outcome.iter().map(|reach| (reach(&[]), reach(&["--format", "json"]))).collect();

    let mut listed = BTreeSet::new();
    for (markdown, json) in printed {
        let stdout = String::from_utf8(markdown.stdout).expect("UTF-8");
        let first_line = stdout.lines().next().expect("a first line");
        let outcome: serde_json::Value = serde_json::from_slice(&json.stdout).expect("stdout is JSON");
        assert_eq!((markdown.status.code(), json.status.code()), (Some(0), Some(0)), "{first_line}");
        let rows: Vec<_> = (0..documented.len()).filter(|&i| documented[i].matches(first_line)).collect();
        assert_eq!(rows.len(), 1, "the skill lists `{first_line}` once");
        assert_eq!(outcome["outcome"], documented[rows[0]].json.as_str(), "the JSON outcome of `{first_line}`");
        listed.insert(rows[0]);
    }
    assert_eq!(listed.len(), documented.len(), "a row the CLI never prints");
}

/// The first lines that "The loop" and "Co-reviewing" branch on, items like
/// ``   - `# Round N`, a Round …``.
fn first_lines_the_steps_branch_on() -> Vec<String> {
    SKILL.lines().filter_map(|line| Some(line.strip_prefix("   - `")?.split_once('`')?.0.to_owned())).collect()
}

#[test]
fn the_steps_branch_only_on_first_lines_the_skill_lists() {
    let listed: BTreeSet<String> = documented_outcomes().into_iter().map(|o| o.first_line).collect();

    let branched = first_lines_the_steps_branch_on();

    assert_eq!(branched.len(), 7, "the loop's 3 outcomes and co-reviewing's 4: {branched:?}");
    for line in branched {
        assert!(listed.contains(&line), "a step branches on `{line}`, which What `review` prints doesn't list");
    }
}

#[test]
fn reply_help_and_the_skill_say_a_dash_reads_the_body_from_stdin() {
    let app = HostedApp::start();

    let out = app.akshually(&std::env::temp_dir(), &["reply", "--help"]).output().unwrap();

    let help = String::from_utf8(out.stdout).expect("UTF-8");
    let body_help = help.lines().find(|line| line.trim_start().starts_with("<BODY>")).expect("a <BODY> argument");
    let skill_reply = SKILL.lines().find(|line| line.starts_with("- `akshually reply ")).expect("a `reply` entry");
    for (doc, text) in [("`reply --help`", body_help), ("the skill's `reply` entry", skill_reply)] {
        assert!(text.contains("`-`") && text.contains("stdin"), "{doc} doesn't say `-` reads stdin: {text}");
    }
}
