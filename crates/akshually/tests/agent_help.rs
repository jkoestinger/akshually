//! Seam B: `akshually agent-help` and the shipped skill it prints.

mod support;

use std::collections::BTreeSet;

use akshually_core::{Anchor, Service, SessionId, Side};
use akshually_testkit::TestRepo;
use support::{HostedApp, finish};

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

#[test]
fn the_skill_documents_exactly_the_cli_subcommands() {
    assert_eq!(documented_subcommands(), cli_subcommands());
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

/// Runs `review` in a repository with work and lets the human answer it with `human`.
fn review_answered(human: impl FnOnce(&Service, &SessionId)) -> Option<i32> {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let child = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    human(&app.service, &session.id);
    finish(child).status.code()
}

/// What the CLI exits with in a situation the skill gives `code` for, or `None` when there is
/// no such situation.
fn exit_code_reached_for(code: i32) -> Option<i32> {
    let outside_a_repo =
        |args: &[&str]| HostedApp::start().akshually(&std::env::temp_dir(), args).output().unwrap().status.code();
    match code {
        0 => review_answered(|service, id| service.approve(id).unwrap()),
        1 => outside_a_repo(&["review"]),
        2 => outside_a_repo(&["review", "--bogus"]),
        3 => review_answered(|service, id| service.abandon(id).unwrap()),
        4 => review_answered(|service, id| {
            service.comment(id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b?").unwrap();
            service.hand_over_round(id).unwrap();
        }),
        _ => None,
    }
}

#[test]
fn every_exit_code_the_skill_documents_is_one_the_cli_exits_with() {
    let documented = documented_exit_codes();
    assert_eq!(documented, BTreeSet::from([0, 1, 2, 3, 4]), "every exit code the CLI has");
    for code in documented {
        assert_eq!(exit_code_reached_for(code), Some(code), "exit code {code}");
    }
}
