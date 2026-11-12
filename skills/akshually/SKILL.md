---
name: akshually
description: Get a human's review of your local code changes with the `akshually` CLI, one Round of Threads at a time until they approve, or hand a human your review of a Pull Request or of local work as Drafts and answer their questions. Use when you're asked to have your work reviewed, to wait for review, to co-review a Pull Request or local changes with the human, or to use akshually.
---

# akshually

akshually is a desktop review app. Usually you are the **Author**: you ask the human to review your local work, they hand you **Rounds** of **Threads** (comments anchored to lines, a file, or the whole Review), and you answer each Thread, until they give the **Verdict**. Run every command from inside the repository, on the branch you are working on: the repository and branch pick the **Session**.

When you are asked to review a Pull Request or someone's local changes instead, you are the human's **co-Reviewer**: see [Co-reviewing](#co-reviewing). A Session has at most one agent, in one role.

## The loop

1. Before changing anything, run `akshually start`. It snapshots the working tree as the Session base, so the human can see only your changes rather than uncommitted work that was already there. It returns at once.
2. Do the work.
3. Run `akshually review`. It opens the app, or reuses the one already running, and **blocks** until the human answers. Branch on its exit code:
   - `4`, a Round: stdout holds the Threads. Address every one, answer each with `reply` or `resolve`, then go back to 3.
   - `0`, approved: the Review is over. Stop.
   - `3`, Abandoned: the human closed the Session without a Verdict. Stop working on this change.
   - `1`: an error, explained on stderr.

The human can take minutes or hours. Run `review` without a timeout if your shell tool allows it. If the call is killed anyway (a tool timeout, a restart), run `akshually review` again: a Round or Verdict the human gave meanwhile is kept for the next call, and nothing is lost. The human can still close a Session whose Verdict no call collected: your next `review` then starts a new Session, with a fresh Round 1. Run only one `review` per branch at a time; a second one fails with `another akshually review is already waiting on this Session`.

## Commands

- `akshually start`: record the Session base (step 1).
- `akshually review [--base REF] [--untracked | --no-untracked] [--format markdown|json]`: review this branch against its merge-base with the default branch, plus uncommitted changes and untracked, non-ignored files.
  - `--base REF`: compare against the merge-base with REF instead, e.g. a parent branch in a stack.
  - `--untracked` / `--no-untracked`: include or leave out untracked files, whatever the human's setting says (it includes them by default).
  - `akshually review A..B` or `A...B`: review exactly a commit range (`...` means since their merge-base). Uncommitted work is not part of it, and it can't be combined with `--base` or the untracked flags.
  - `--format json`: print the outcome as one line of JSON instead of markdown: `{"outcome":"approved"}`, `{"outcome":"abandoned"}`, `{"outcome":"closed"}`, or `{"outcome":"round","number":N,"threads":[…]}`, each Thread with its `id`, `anchor`, `excerpt`, `comments` (`author`, `body`, `round`, optional `suggestion`), `resolved`, `outdated` and `reopened`.
- `akshually reply [--pr <url>] <thread> <body>`: answer a Thread, e.g. `akshually reply T3 "Renamed it to parse_range."`. The body is markdown. The human sees it at once; the call returns at once. `--pr`: in the Session of the Pull Request you co-review, not this branch's.
- `akshually resolve [--pr <url>] <thread>`: mark a Thread Resolved when you've dealt with it. The human can reopen it.
- `akshually draft [--pr <url>] (--lines PATH:N[-M] [--old] | --file PATH | --review) <body>`: as the human's **co-Reviewer**, add a **Draft** to the Pull Request at `<url>`, or without `--pr` to this branch's local work, e.g. `akshually draft --pr https://github.com/acme/web/pull/7 --lines src/lib.rs:2-3 "Validate the state first."`. It opens the Session in the app if needed, prints the Draft's Thread ID and returns at once.
  - `--lines`: lines of a file, 1-based and inclusive, in the new code (`--old`: in the base). `--file`: the whole file. `--review`: the Review as a whole.
  - The body is markdown; `-` reads it from stdin. A ```` ```suggestion ```` fence in it proposes the exact replacement for new-side `--lines`.
  - Your Drafts are only proposals: the human keeps, edits, discards or questions each one, and only what they keep is published, under their name.
- `akshually review --pr <url> [--format markdown|json]`: once you've added your Drafts, hand them to the human and **block** until they finish the Review: see [Co-reviewing](#co-reviewing).
- `akshually agent-help`: print this guide.

`reply` and `resolve` act on Threads already handed to you in a Round. They fail with exit 1 when there is no such Session, or no such Thread.

## Reading a Round

The markdown starts with `# Round N`, then one section per Thread:

- The heading names the Thread ID and its Anchor: `## T1 · src/lib.rs, new lines 2-3`, `## T2 · src/lib.rs, whole file` or `## T3 · whole Review`. Line numbers are 1-based and inclusive; `new` lines are in your current code, `old` lines in the base.
- A Lines Thread shows the code it was written against in a fence, then the Comments, oldest first.
- `· Outdated`: that code is no longer in the Changeset. Judge whether your change already covers the Comment.
- `· Reopened`: the human reopened a Thread you or they had Resolved; it isn't dealt with yet.
- `Suggested change: replace new lines 2-3 of src/lib.rs with`, followed by a fence: the human wrote the exact replacement for those lines. Apply it unless you have a reason not to, and say so in a `reply`.

Thread IDs stay the same across Rounds, so `T3` in Round 2 is the Thread `T3` from Round 1.

## Answering Threads

- Fix what the Thread asks, then `resolve` it, or `reply` first when the human should know what you did or why.
- When you disagree or need an answer, `reply` without resolving and leave it open for the human.
- Answer every Thread before running `review` again.

## Co-reviewing

You review someone else's code alongside the human: a Pull Request (`--pr <url>` on every command), or the local work on this branch (no `--pr`; run the commands inside that repository, on that branch). The roles are reversed: the human decides, and you answer their questions about your Drafts.

1. Read the code, then add one `akshually draft` per finding. Keep each to one point, anchored as narrowly as you can.
2. Run `akshually review --pr <url>`, or `akshually review` for local work. It hands your Drafts to the human and **blocks** until they act. Branch on its exit code:
   - `4`, a Round of questions: stdout holds the Threads of the Drafts the human questioned, with their questions. Answer each with `reply` (with the same `--pr`). To revise a Draft, add a new one with `draft` and say so in your reply; the human discards the old one. Then go back to 2.
   - `5`, Published (Pull Request only): the human published their Review; stdout has the Verdict and the review's link (`{"outcome":"published","verdict":"approve","url":"…"}` with `--format json`). Report it back. Stop.
   - `6`, Closed (local work only): the human finished with your co-review. A co-review of local work has no Verdict, so this is its normal end. Stop.
   - `3`, Abandoned (Pull Request only): the human closed it without publishing. Stop.
   - `1`: an error, explained on stderr.

Run `draft` before `review` on local work: the first `draft` is what makes you this branch's co-Reviewer. A branch whose Author is another agent refuses your Drafts.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Approved: the Review is over. Any command that returns at once also exits 0 on success. |
| `1` | Error, explained on stderr after `akshually:`. |
| `2` | Usage error: a wrong subcommand, flag or combination. |
| `3` | Abandoned: stop working on this change. For `review --pr`, the human closed the Pull Request without publishing. |
| `4` | A Round was handed over: address it and run `review` again. As co-Reviewer, it holds the human's questions on your Drafts. |
| `5` | Published (`review --pr` only): the human published their Review of the Pull Request; stdout has the Verdict and the link. |
| `6` | Closed (co-review of local work only): the human finished with your co-review, a normal end. Stop. |
