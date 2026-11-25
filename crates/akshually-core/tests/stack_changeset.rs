//! Seam A: the Stack Changeset of a Session that holds a Stack, in the core review service with a
//! fake Forge adapter that can compare two commits (ADR 0004).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use akshually_core::forge::{
    Comparison, ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread,
    Review,
};
use akshually_core::{Anchor, Changeset, Error, FileChange, Service, SessionId, SessionView, Side, Verdict};

#[derive(Default)]
struct World {
    /// The open Pull Requests of `acme/web`, bottom to top: number, head branch, base branch.
    open: Vec<(u64, String, String)>,
    /// The head commit of a Pull Request that was pushed to.
    pushed: HashMap<u64, String>,
    /// The Forge can't compare commits.
    broken: bool,
    /// The Forge cuts every comparison short.
    cut_off: bool,
    /// Every comparison asked for: base, then head.
    compared: Vec<(String, String)>,
}

#[derive(Clone, Default)]
struct FakeForge {
    world: Arc<Mutex<World>>,
}

fn pr_ref(number: u64) -> PullRequestRef {
    PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number }
}

fn url(number: u64) -> String {
    format!("https://forge.test/acme/web/pull/{number}")
}

/// What the file of the Stack holds at a commit: each Member adds a line to it, and the top one
/// takes the middle one's line out again.
fn text_at(commit: &str) -> &'static str {
    match commit {
        "head1" => "x\nbottom\n",
        "head2" => "x\nbottom\nmiddle\n",
        "head3" => "x\nbottom\ntop\n",
        "head3b" => "x\nbottom\ntop\npushed\n",
        _ => "x\n",
    }
}

impl FakeForge {
    /// A Stack `#1 <- #2 <- #3` on `main`, branches `a`, `b` and `c`.
    fn stack_of_three() -> Self {
        let forge = FakeForge::default();
        forge.world.lock().unwrap().open =
            vec![(1, "a".into(), "main".into()), (2, "b".into(), "a".into()), (3, "c".into(), "b".into())];
        forge
    }

    fn lone() -> Self {
        let forge = FakeForge::default();
        forge.world.lock().unwrap().open = vec![(1, "a".into(), "main".into())];
        forge
    }

    fn service(&self) -> Service {
        Service::new().with_forges(vec![Box::new(self.clone())])
    }

    /// Pull Request `number` is pushed to: its head is now `head`.
    fn push(&self, number: u64, head: &str) {
        self.world.lock().unwrap().pushed.insert(number, head.into());
    }

    fn close(&self, number: u64) {
        self.world.lock().unwrap().open.retain(|(n, ..)| *n != number);
    }

    fn cut_comparisons_short(&self) {
        self.world.lock().unwrap().cut_off = true;
    }

    fn break_comparisons(&self, broken: bool) {
        self.world.lock().unwrap().broken = broken;
    }

    fn compared(&self) -> Vec<(String, String)> {
        self.world.lock().unwrap().compared.clone()
    }

    fn lookup(&self, matches: impl Fn(&(u64, String, String)) -> bool) -> Vec<PullRequest> {
        let world = self.world.lock().unwrap();
        world.open.iter().filter(|p| matches(p)).map(|(n, ..)| self.pull_request_of(&world, *n)).collect()
    }

    fn pull_request_of(&self, world: &World, number: u64) -> PullRequest {
        let (head, base) = world
            .open
            .iter()
            .find(|(n, ..)| *n == number)
            .map_or(("gone".to_owned(), "main".to_owned()), |(_, h, b)| (h.clone(), b.clone()));
        PullRequest {
            id: pr_ref(number),
            url: url(number),
            title: format!("Pull Request {number}"),
            author: "maria".into(),
            head_branch: head,
            head_sha: world.pushed.get(&number).cloned().unwrap_or_else(|| format!("head{number}")),
            base_sha: format!("base{number}"),
            base_branch: base,
            from_fork: false,
            ..Default::default()
        }
    }
}

impl ForgeAdapter for FakeForge {
    fn host(&self) -> &str {
        "forge.test"
    }

    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        let number = url.strip_prefix("https://forge.test/acme/web/pull/")?;
        Some(pr_ref(number.split(['/', '#']).next()?.parse().ok()?))
    }

    fn pull_request(&self, id: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        let world = self.world.lock().unwrap();
        Ok(self.pull_request_of(&world, id.number))
    }

    fn open_with_head_branch(&self, _: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok(self.lookup(|(_, head, _)| head == branch))
    }

    fn open_with_target_branch(&self, _: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok(self.lookup(|(_, _, base)| base == branch))
    }

    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        let path = format!("f{}.rs", pr.id.number);
        // A push adds a line on top, which moves the code a Thread is on.
        let pushed = self.world.lock().unwrap().pushed.contains_key(&pr.id.number);
        let new = if pushed { "// pushed\nfn a() {\n    b();\n}\n" } else { "fn a() {\n    b();\n}\n" };
        Ok(Changeset {
            base: pr.base_sha.clone(),
            files: vec![FileChange::text(&path, Some("fn a() {}\n"), Some(new))],
        })
    }

    fn changeset_between(&self, repo: &str, base: &str, head: &str) -> Result<Comparison, ForgeError> {
        assert_eq!(repo, "acme/web");
        let mut world = self.world.lock().unwrap();
        world.compared.push((base.into(), head.into()));
        if world.broken {
            return Err(ForgeError::Api { host: "forge.test".into(), status: 500, message: "compare is down".into() });
        }
        // Named as the first Member's file, so a Thread of that Member would be placed on it.
        let file = FileChange::text("f1.rs", Some(text_at(base)), Some(text_at(head)));
        let changeset = Changeset { base: base.into(), files: vec![file] };
        Ok(Comparison { changeset, cut_off: world.cut_off.then(|| "the Forge lists at most 300 files".to_owned()) })
    }

    fn threads(&self, pr: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(vec![RemoteThread {
            id: format!("RT_{}", pr.id.number),
            anchor: RemoteAnchor::Lines { path: format!("f{}.rs", pr.id.number), side: Side::New, start: 2, end: 2 },
            outdated: false,
            resolved_by: None,
            diff_hunk: String::new(),
            comments: vec![RemoteComment { author: "lee".into(), body: "A question".into() }],
        }])
    }

    fn viewer(&self) -> Result<String, ForgeError> {
        Ok("me".into())
    }

    fn verdicts(&self, _: &PullRequest, _: bool) -> Result<Vec<Verdict>, ForgeError> {
        Ok(Verdict::ALL.to_vec())
    }

    fn publish(&self, pr: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        Ok(format!("{}#review", pr.url))
    }
}

fn open(svc: &Service, number: u64) -> SessionId {
    svc.open_pull_request(&url(number)).unwrap()
}

fn session(svc: &Service, id: &SessionId) -> SessionView {
    svc.session(id).unwrap()
}

/// The text the Stack Changeset's one file ends in.
fn stack_text(session: &SessionView) -> String {
    let changeset = session.stack_changeset().expect("the Stack Changeset was fetched").expect("it was fetched whole");
    changeset.files[0].new.clone().unwrap()
}

#[test]
fn a_stack_of_two_or_more_offers_the_stack_changeset_and_a_stack_of_one_does_not() {
    let stack = FakeForge::stack_of_three().service();
    let lone = FakeForge::lone().service();

    let of_three = session(&stack, &open(&stack, 2));
    let alone_id = open(&lone, 1);
    let of_one = session(&lone, &alone_id);

    assert!(of_three.offers_stack_changeset());
    assert!(!of_one.offers_stack_changeset());
    let refused = lone.select_stack_changeset(&alone_id);
    assert!(matches!(refused, Err(Error::NoStackChangeset)), "{refused:?}");
}

#[test]
fn the_stack_changeset_of_three_members_has_what_the_bottom_and_the_top_add_and_not_what_the_top_took_out() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 2);

    svc.select_stack_changeset(&id).unwrap();

    let session = session(&svc, &id);
    assert!(session.stack_changeset_selected());
    let text = stack_text(&session);
    assert!(text.contains("bottom"), "the line the bottom member added: {text:?}");
    assert!(text.contains("top"), "the line the top member added: {text:?}");
    assert!(!text.contains("middle"), "the line the middle member added and the top one removed: {text:?}");
    assert_eq!(
        forge.compared(),
        [("base1".to_owned(), "head3".to_owned())],
        "from the bottom's base to the top's head"
    );
    assert_eq!(session.shown().files[0].new.as_deref(), Some(text.as_str()), "it is the Changeset the Window shows");
}

#[test]
fn the_stack_changeset_is_fetched_when_first_selected_and_only_again_when_the_stack_changes() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 2);
    assert!(forge.compared().is_empty(), "nothing is fetched while it isn't selected");

    svc.select_stack_changeset(&id).unwrap();
    svc.select_member(&id, &pr_ref(1)).unwrap();
    svc.select_stack_changeset(&id).unwrap();

    assert_eq!(forge.compared().len(), 1);
}

#[test]
fn starting_a_thread_on_the_stack_changeset_is_refused_for_the_human() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 2);
    svc.select_stack_changeset(&id).unwrap();
    let lines = Anchor::lines(Side::New, "f1.rs", 2..=2);

    let human = svc.comment(&id, lines.clone(), "Here.");
    let general = svc.comment(&id, Anchor::Review, "Overall.");

    for refused in [human, general] {
        assert!(matches!(refused, Err(Error::StackChangesetReadOnly)), "{refused:?}");
    }
    assert_eq!(session(&svc, &id).drafts(), 0);
}

#[test]
fn an_agents_draft_on_a_member_lands_in_its_review_whatever_the_human_is_looking_at() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 2);
    svc.select_stack_changeset(&id).unwrap();
    let lines = Anchor::lines(Side::New, "f3.rs", 2..=2);

    let thread = svc.draft(url(3).as_str(), lines.clone(), "From the agent.").unwrap();

    let shown = session(&svc, &id);
    assert!(shown.stack_changeset_selected(), "the human still has the Stack Changeset in front of them");
    let drafts: Vec<_> = shown.stack().iter().map(|m| m.drafts).collect();
    assert_eq!(drafts, [0, 0, 1]);
    svc.select_member(&id, &pr_ref(3)).unwrap();
    let on_member = session(&svc, &id);
    let drafted = on_member.threads.iter().find(|t| t.id == thread).expect("the Thread is in #3's Review");
    assert_eq!(drafted.excerpt, "    b();\n", "excerpted from #3's own Changeset");
    assert_eq!(drafted.anchor, lines);
}

#[test]
fn a_reload_while_the_stack_changeset_is_shown_moves_the_threads_with_the_members_code() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 2);
    let thread = svc.comment(&id, Anchor::lines(Side::New, "f2.rs", 2..=2), "On the call.").unwrap();
    svc.select_stack_changeset(&id).unwrap();
    forge.push(2, "head2b");

    svc.reload(&id).unwrap();

    let reloaded = session(&svc, &id);
    assert!(reloaded.stack_changeset_selected());
    let moved = reloaded.threads.iter().find(|t| t.id == thread).unwrap();
    assert_eq!(moved.anchor, Anchor::lines(Side::New, "f2.rs", 3..=3), "one line down, with the code");
    assert!(!moved.outdated);
}

#[test]
fn a_push_found_by_a_check_fetches_the_stack_changeset_again_without_a_reload() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 3);
    svc.select_stack_changeset(&id).unwrap();
    forge.push(3, "head3b");

    svc.check_pull_requests();

    assert!(stack_text(&session(&svc, &id)).contains("pushed"));
    assert_eq!(forge.compared().last().unwrap(), &("base1".to_owned(), "head3b".to_owned()));
}

#[test]
fn ticking_a_file_as_viewed_while_the_stack_changeset_is_shown_is_refused() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 2);
    svc.select_stack_changeset(&id).unwrap();

    let ticked = svc.mark_viewed(&id, "f2.rs", true);

    assert!(matches!(ticked, Err(Error::StackChangesetReadOnly)), "{ticked:?}");
    assert!(session(&svc, &id).viewed.is_empty(), "no file of a Member that isn't shown was ticked");
}

#[test]
fn selecting_a_member_brings_back_its_changeset_and_its_threads_and_nothing_of_the_stack_changeset_is_kept_shown() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    let member = session(&svc, &id);
    svc.select_stack_changeset(&id).unwrap();
    let whole = session(&svc, &id);

    svc.select_member(&id, &pr_ref(1)).unwrap();

    let back = session(&svc, &id);
    assert!(!back.stack_changeset_selected());
    assert_eq!(back.shown(), member.shown());
    assert_eq!(back.placed_threads().len(), 1, "the member's Thread is back on its lines");
    assert_ne!(back.changeset_rev, whole.changeset_rev, "the Window rebuilds its diff");
    svc.comment(&id, Anchor::Review, "Fine again.").unwrap();
}

#[test]
fn the_threads_of_the_members_are_not_placed_on_the_stack_changeset() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    assert_eq!(session(&svc, &id).placed_threads().len(), 1);

    svc.select_stack_changeset(&id).unwrap();

    let whole = session(&svc, &id);
    assert!(whole.placed_threads().is_empty());
    assert_eq!(whole.open_threads_on("f1.rs"), 0);
    assert_eq!(whole.anchor_for_selection(0, (Side::New, 2), (Side::New, 2)), None);
}

#[test]
fn after_a_members_head_changes_selecting_the_stack_changeset_shows_the_new_content() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 3);
    svc.select_stack_changeset(&id).unwrap();
    forge.push(3, "head3b");
    svc.reload(&id).unwrap();

    svc.select_stack_changeset(&id).unwrap();

    assert!(stack_text(&session(&svc, &id)).contains("pushed"));
    assert_eq!(forge.compared().last().unwrap(), &("base1".to_owned(), "head3b".to_owned()));
}

#[test]
fn a_reload_while_the_stack_changeset_is_shown_shows_the_new_content_without_selecting_it_again() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 3);
    svc.select_stack_changeset(&id).unwrap();
    forge.push(3, "head3b");

    svc.reload(&id).unwrap();

    let session = session(&svc, &id);
    assert!(session.stack_changeset_selected());
    assert!(stack_text(&session).contains("pushed"));
}

#[test]
fn a_failed_fetch_shows_an_error_in_its_place_and_the_members_stay_usable() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 2);
    svc.comment(&id, Anchor::Review, "A note.").unwrap();
    forge.break_comparisons(true);

    svc.select_stack_changeset(&id).unwrap();

    let failed = session(&svc, &id);
    assert!(failed.stack_changeset_selected());
    let error = failed.stack_changeset().expect("an answer").expect_err("the fetch failed");
    assert!(error.contains("compare is down"), "{error}");
    assert!(failed.shown().files.is_empty());
    // The Members can be selected and Published, and the Stack Changeset fetched again.
    svc.select_member(&id, &pr_ref(2)).unwrap();
    assert!(!session(&svc, &id).stack_changeset_selected());
    svc.publish(&id, Verdict::Feedback, "Done.").unwrap();
    forge.break_comparisons(false);
    svc.select_stack_changeset(&id).unwrap();
    assert!(stack_text(&session(&svc, &id)).contains("bottom"));
}

#[test]
fn a_member_that_left_the_stack_is_not_counted_in_the_stack_changeset() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 3);
    // Its Draft keeps #3 in the Session after it is merged.
    svc.comment(&id, Anchor::Review, "Before it merges.").unwrap();
    forge.close(3);
    svc.check_pull_requests();
    assert_eq!(session(&svc, &id).stack().len(), 3, "#3 stays for its Draft");

    svc.select_stack_changeset(&id).unwrap();

    let text = stack_text(&session(&svc, &id));
    assert!(text.contains("middle"), "up to the top member still in the Stack: {text:?}");
    assert_eq!(forge.compared(), [("base1".to_owned(), "head2".to_owned())]);
}

#[test]
fn the_stack_changeset_is_no_longer_offered_when_one_member_is_left_in_the_stack() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 1);
    svc.select_stack_changeset(&id).unwrap();

    forge.close(2);
    forge.close(3);
    svc.check_pull_requests();

    let session = session(&svc, &id);
    assert!(!session.offers_stack_changeset());
    assert!(!session.stack_changeset_selected());
    assert_eq!(session.shown().files[0].path, "f1.rs");
    assert_eq!(session.placed_threads().len(), 1);
}

#[test]
fn a_stack_changeset_the_forge_cut_short_says_so_instead_of_passing_as_complete() {
    let forge = FakeForge::stack_of_three();
    forge.cut_comparisons_short();
    let svc = forge.service();
    let id = open(&svc, 2);

    svc.select_stack_changeset(&id).unwrap();

    let shown = session(&svc, &id);
    assert_eq!(shown.stack_changeset_cut_off(), Some("the Forge lists at most 300 files"));
    svc.select_member(&id, &pr_ref(2)).unwrap();
    assert_eq!(session(&svc, &id).stack_changeset_cut_off(), None, "only while the Stack Changeset is shown");
}
