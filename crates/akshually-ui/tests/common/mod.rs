//! Helpers the UI tests share: a headless window over a real service, and a repository to review.

// Each test file uses some of these, not all.
#![allow(dead_code)]

use std::sync::Arc;

use rune::{Env, Runtime};

use akshually_core::Service;
use akshually_testkit::TestRepo;
use akshually_ui::{State, view};

/// A headless window over `service`, its first frame built.
pub(crate) fn window(service: &Arc<Service>) -> Runtime<State> {
    let mut rt = Runtime::new(State::new(service.clone()), view, (1280.0, 800.0), Env::default());
    rt.frame(0.0);
    rt
}

/// `window`, with the app's key handler wired.
pub(crate) fn window_with_keys(service: &Arc<Service>) -> Runtime<State> {
    let mut rt = Runtime::new(State::new(service.clone()), view, (1280.0, 800.0), Env::default());
    rt.on_key = Some(akshually_ui::on_key);
    rt.frame(0.0);
    rt
}

/// A repository with one uncommitted edit to `a.txt`, so its Session's Changeset has one file
/// with one line to show.
pub(crate) fn repo_with_one_edit() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "two\n");
    repo
}
