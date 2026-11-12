//! Seam A, the core review service: desktop notifications through a fake Notifier.

use std::sync::{Arc, Mutex};

use akshually_core::{Anchor, Notification, Notifier, ReviewRequest, Service, SessionId, Settings, Side};
use akshually_testkit::{TestDirs, TestRepo};

/// A repository on branch `agent` with a committed change to `lib.rs`.
fn agent_branch() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("agent's work");
    repo
}

/// Records every notification instead of showing it.
#[derive(Clone, Default)]
struct FakeNotifier(Arc<Mutex<Vec<Notification>>>);

impl FakeNotifier {
    fn sessions(&self) -> Vec<SessionId> {
        self.0.lock().unwrap().iter().filter_map(|n| n.session.clone()).collect()
    }

    fn all(&self) -> Vec<Notification> {
        self.0.lock().unwrap().clone()
    }
}

impl Notifier for FakeNotifier {
    fn notify(&self, notification: Notification) {
        self.0.lock().unwrap().push(notification);
    }
}

fn service(notifier: &FakeNotifier) -> Service {
    Service::new().with_notifier(notifier.clone())
}

#[test]
fn a_session_opening_while_the_window_is_unfocused_is_notified() {
    let repo = agent_branch();
    let notifier = FakeNotifier::default();
    let svc = service(&notifier);
    svc.set_window_focused(false);

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(notifier.sessions(), vec![pending.session().clone()]);
}

#[test]
fn a_session_opening_while_the_window_is_focused_is_not_notified() {
    let repo = agent_branch();
    let notifier = FakeNotifier::default();
    let svc = service(&notifier);
    svc.set_window_focused(true);

    let _pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(notifier.sessions(), vec![]);
}

#[test]
fn the_notifier_is_called_each_time_the_session_becomes_the_humans_turn_and_only_then() {
    let repo = agent_branch();
    let notifier = FakeNotifier::default();
    let svc = service(&notifier);
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    assert_eq!(notifier.sessions(), vec![id.clone()], "a new Session is the human's Turn");

    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b?").unwrap();
    drop(first);
    svc.hand_over_round(&id).unwrap();
    let picks_up_the_round = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(notifier.sessions(), vec![id.clone()], "the agent's Turn notifies no one");

    drop(picks_up_the_round);
    let _waits_again = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(notifier.sessions(), vec![id.clone(), id], "the human's Turn again");
}

#[test]
fn losing_the_local_channel_is_notified_once_without_a_session() {
    let notifier = FakeNotifier::default();
    let svc = service(&notifier);

    svc.local_channel_lost();

    let notifications = notifier.all();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    assert_eq!(notifications[0].session, None);
    assert!(notifications[0].body.contains("restart"), "says how to fix it: {:?}", notifications[0]);
}

#[test]
fn notifications_are_on_by_default() {
    assert!(Settings::default().notifications);
}

#[test]
fn turning_notifications_off_in_the_settings_stops_them() {
    let repo = agent_branch();
    let notifier = FakeNotifier::default();
    let svc = service(&notifier);
    svc.set_settings(Settings { notifications: false, ..svc.settings() });

    let _pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(notifier.sessions(), vec![]);
}

#[test]
fn the_config_file_can_turn_notifications_off() {
    let dir = TestDirs::new();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "notifications = false\n").unwrap();

    assert!(!Settings::load(&file).unwrap().notifications);
}

/// Records each time the service asks for the window, instead of showing one.
fn window_shows(svc: &Service) -> Arc<Mutex<u32>> {
    let shown = Arc::new(Mutex::new(0));
    let sink = shown.clone();
    svc.on_show_window(move |_| *sink.lock().unwrap() += 1);
    shown
}

#[test]
fn the_window_shows_when_a_session_becomes_the_humans_turn_and_not_for_other_changes() {
    let repo = agent_branch();
    let svc = Service::new();
    let shown = window_shows(&svc);

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(*shown.lock().unwrap(), 1, "a new Session in the human's Turn");

    let id = pending.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b?").unwrap();
    repo.write("lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    svc.check_disk();
    assert!(!svc.session(&id).unwrap().changed_on_disk.is_empty(), "the change on disk wasn't offered");
    assert_eq!(*shown.lock().unwrap(), 1, "Drafts and changes on disk don't take the screen");

    svc.hand_over_round(&id).unwrap();
    pending.wait().unwrap();
    let _next = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(*shown.lock().unwrap(), 2, "the human's Turn again");
}
