# Coding standards

How code in this repo is written. Most of these rules describe what the code already does, and new code should match it. Tooling enforces some of them (see [Enforced by tooling](#enforced-by-tooling)); reviewers check the rest.

Read [`GLOSSARY.md`](GLOSSARY.md) and the ADRs in [`docs/adr/`](docs/adr/) first: several rules below point at them.

## Architecture

- **Review logic lives in `akshually-core`.** `akshually-ui` is a thin Rune shell: it turns the service's plain data (`SessionView`, `InboxView`, …) into a view and sends every human action straight back to `Service`. If a UI function decides something about Sessions, Threads, Rounds or Verdicts, move it into the core and test it there ([ADR 0001](docs/adr/0001-rune-behind-a-thin-ui-shell.md)).
- **Only `akshually-ui` depends on Rune** (`rune`, `rune-kit`, `rune-notify`). The core has no UI dependency at all.
- **`Service` is the single entry point.** The UI calls it in-process. The CLI reaches it through the local channel in `ipc` ([ADR 0002](docs/adr/0002-persistent-window-blocking-cli.md)). Don't add a second path into the state.
- **The core is blocking.** There is no async runtime: HTTP goes through `ureq`, and slow work runs on its own thread. The UI never blocks its thread on the network or on `git`.
- **Forge details stay in the Forge adapters.** `forge::github` and `forge::gitlab` map their Forge's API to Forge-neutral core types (`PullRequest`, `Changeset`, `RemoteThread`, `Review`). The service never sees GitHub or GitLab shapes.
- **Shell out to `git`** through `crate::git` rather than linking a git library, so akshually gives the same answers as the user's git.
- **akshually never launches agents.** Agents call the CLI, and the CLI stays agent-agnostic.
- **Hold the service lock briefly.** Callbacks run under the lock (`Notifier`, subscribers, `show_window`): they must return quickly and never call back into the service. Network calls happen outside the lock.

## Domain language

- **Use the glossary's terms** in identifiers, doc comments, error messages and CLI output: Session, Changeset, Round, Thread, Draft, Verdict, Publish, Forge, Pull Request, and so on. Don't use the synonyms it lists under _Avoid_ (MR, PR in prose, diff for a Changeset, queue for the Inbox, …). The exceptions are quoting a Forge's own vocabulary (GitLab's "draft notes", "merge request") and short local variable names (`pr`).
- **Capitalise domain terms** as the glossary does ("the human's Turn", "a Draft"). Use lower case when you mean the ordinary word, such as an OS thread or GitHub's pending review.
- **A new domain concept goes into `GLOSSARY.md` first** (see `/domain-modeling`), before code names it.
- **Write prose in British English**: recognise, colour, behaviour. Names that come from other crates or APIs keep their own spelling (`Color`).

## Modules and visibility

- **Every module starts with a `//!` line** saying what it is, in domain terms.
- **Keep modules private and re-export the API from the crate root** (`mod changeset;` and `pub use changeset::{…}`). Make a module `pub` only when its path is part of the API (`forge`, `git`, `ipc`).
- **Use the narrowest visibility that works.** Use `pub(crate)` or `pub(super)` for items shared inside a crate. Don't mark items `pub` in a binary or in a private module where nothing outside can reach them.
- **Split a module when it holds more than one concern**, as `service` is split into `co_review`, `drafts`, `handoff`, `inbox` and the rest. A submodule extends `Service` with its own `impl Service` block.

## Errors

- **Libraries return typed errors.** Each module boundary has a `thiserror` enum (`Error`, `GitError`, `ForgeError`, `SettingsError`, `ClientError`), wrapped upward with `#[from]` or `#[error(transparent)]`. Don't use `anyhow` or `Box<dyn Error>` in a library crate.
- **The binary uses `anyhow`**, and adds `.context("can't …")` to every fallible call that the user's environment can break.
- **Error messages are written for the person who reads them.** Start in lower case, end without a full stop, use domain terms, and when the reader can do something about it, say what: ``"no Session for {0}; run `akshually review --pr {0}` first"``.
- **Don't panic on input, environment, disk or network.** Return an error instead. `unwrap()` is not allowed in production code. Use `expect("…")` only for an invariant the code itself guarantees, and write the message as the invariant (`expect("stdin is piped")`). Give `unreachable!` the reason too.
- **Recover from a poisoned mutex** with `into_inner()` (`lock().unwrap_or_else(|e| e.into_inner())`). The service keeps its state consistent between statements, so a panicking subscriber must not take down the app.
- **Best-effort work logs and carries on.** When a failure must not undo or block what already happened (saving Sessions, skipping an unreadable saved file), log `eprintln!("akshually: …")` and leave a comment saying why it doesn't fail.

## Documentation and comments

- **Document every public type, trait and non-trivial function** with `///`. Say what it does in domain terms, including edge cases (``"`None` when HEAD is detached"``) and constraints (blocks on the network, runs under the lock, must return quickly). Trivial constructors and accessors (`new`, `as_str`) can go without one.
- **Document a struct field or enum variant when its name doesn't say everything**: units (`/// Seconds since the Unix epoch.`), when it's `None`, where it comes from.
- **Name parameters and code in backticks** in docs and comments: `` `path` ``, `` `akshually review` ``.
- **Comments explain why, not what.** Write them for a constraint, a Forge or OS quirk, a workaround, or a decision, and point at the ADR or issue when one exists. Don't narrate code that reads plainly.
- **Name magic numbers.** A duration, limit or size becomes a `const` whose doc says why it has that value, as `PUSH_CHECK_INTERVAL` and `INBOX_REFRESH_INTERVAL` do.

## Style

- **Group imports** in this order, each group separated by a blank line: `std`, external crates, then `crate`, `super` and workspace crates (`akshually_core`). Prefer one `use` per crate with nested braces.
- **Prefer early returns and `let … else`** to deep nesting.
- **Prefer plain functions and data to new abstractions.** Add a trait only for a real seam that has more than one implementation, or one that tests fake (`ForgeAdapter`, `Notifier`).
- **Builder-style options take `mut self` and return `Self`** (`ReviewRequest::new(cwd).base("main").with_ack()`).
- **Borrow what you don't consume.** Take `&T` or `&str` unless the function keeps the value.

## Dependencies

- **Declare every dependency once, in `[workspace.dependencies]`**, and use `{ workspace = true }` in crates. If it isn't obvious why a dependency is there, say so in a comment.
- **Think twice before adding a dependency.** A short, tested function (such as `forge::epoch_seconds`) beats a new crate, especially in the core. Never add an async runtime to the core.
- **Rune is pinned to one revision** and upgraded on purpose. Bump all its entries together, including the repeated `[patch.crates-io]` ones (ADR 0001).

## Tests

- **Test through the seams, not through private functions.** There are no `#[cfg(test)]` unit tests: each behaviour is pinned through a public interface.
  - **Seam A**: the core `Service` in-process, in `crates/akshually-core/tests`.
  - **Seam B**: the CLI contract (blocking, stdout, exit codes, attaching to the app instance), in `crates/akshually/tests`. Cover only what seam A can't.
  - **Seam C**: a Forge adapter at its HTTP boundary, against `mockito`, a fake `gh` or `glab`, and a controlled environment.
  - **UI**: headless renders only (`ui_smoke.rs`): a smoke check of the rendered pixels, and the UI's own state (an open dropdown, which tab shows what) read from Rune's `--tree` accessibility tree after a `--script` of input. Keep the UI thin enough that this is enough: review logic is pinned at seam A.
- **Every test file opens with a `//!` line naming its seam** and what it covers: `//! Seam A: the Inbox in the core review service, with fake Forge adapters.`
- **Name a test after the behaviour it pins**, as a sentence: `review_blocks_until_the_human_acts`, `approving_returns_approved_to_the_waiting_review_and_ends_the_session`.
- **One behaviour per test, laid out as arrange, act, assert**, with each part separated by a blank line.
- **Use real things where they're cheap.** Use real throwaway git repositories (`akshually_testkit::TestRepo`) and short temp dirs (`TestDirs`). Fake only the boundaries: `ForgeAdapter`, `Notifier`, HTTP. A test never touches the network, real credentials, or the user's own config or state dirs.
- **Don't sleep to synchronise.** Wait on a condition with a timeout (`wait_until`, `PendingReview::wait_timeout`). A short sleep is fine only to prove that something does *not* happen.
- **Tests may `unwrap()`.** Give `expect` and `assert!` messages that say what went wrong: `assert!(still_running(&mut child), "review returned before the human acted")`.
- **A bug fix starts with a failing test** at the seam where the bug shows.

## Enforced by tooling

CI (`.github/workflows/ci.yml`) runs these, and any warning fails the build:

- `cargo fmt --all --check`, with `rustfmt.toml` (`max_width = 120`).
- `cargo clippy --workspace --all-targets --locked -- -D warnings`, with the lints in `[workspace.lints]` in `Cargo.toml`: no `unsafe`, no `dbg!`, `todo!` or `unimplemented!`, and no `pub` items that can't be reached.
- `clippy::unwrap_used` in the production crates (`akshually-core`, `akshually-ui`, `akshually`), set at each crate root, so tests and `akshually-testkit` can still `unwrap()`.
- `cargo test --workspace --locked` on Linux and macOS.

Run all four before you push.
