# akshually

A code review tool that keeps a human in the loop. A human and an agent review a Changeset together, or one of them answers the other's review. The Changeset comes either from the agent's local work or from a Pull Request on a Forge.

## Reviewing

**Review**:
The act of examining one Changeset, whatever its Source, usually ending in a Verdict.
_Avoid_: Code review session, inspection

**Changeset**:
The set of changes under Review, shown as a diff against a base.
_Avoid_: Diff, patch, PR (a PR is one possible Source of a Changeset)

**Session base**:
A snapshot of the working tree taken before an agent starts, so its Changeset can exclude work that was already there.
_Avoid_: Checkpoint, baseline

**Source**:
Where a Changeset comes from: local work in a repository, or a Pull Request on a Forge.
_Avoid_: Provider, backend

**Forge**:
A hosted code platform with pull/merge requests: GitHub or GitLab.
_Avoid_: Remote, host, provider

**Pull Request**:
A Forge's request to merge a branch; GitLab's merge requests are called Pull Requests here too.
_Avoid_: MR, merge request (except when quoting GitLab)

**Inbox**:
The open Pull Requests, across every configured Forge, where the human is a requested reviewer or assignee.
_Avoid_: Queue, dashboard

**Verdict**:
The final outcome of a Review, which ends it, unlike a Round: approve, request changes, or feedback on a Pull Request; only approve for an agent's local work.
_Avoid_: Reject, decision, status, "comment" (GitLab and GitHub's name for feedback)

**Publish**:
Sending the human's finished Review, meaning the kept Drafts and the Verdict, to a Forge as a single review.
_Avoid_: Post, push, sync

## Participants

**Author**:
The side whose Changeset is under Review and who answers Threads; an agent when it asks for review of its own local work.
_Avoid_: Developer, submitter

**Reviewer**:
The side raising Threads and giving the Verdict. The human is always a Reviewer, and an agent can be a co-Reviewer.
_Avoid_: Approver, critic

**Session**:
One continuous Review of one Changeset between the human and at most one agent, spanning every Round until the Verdict and the human closes it. Several Sessions can run at once.
_Avoid_: Tab, window, conversation

**Turn**:
Whose move a Session is waiting on: the human's or the agent's. A Session that becomes the human's Turn needs attention.
_Avoid_: State, status

**Abandoned**:
The outcome of a Session the human closes without a Verdict while an agent is its Author; the agent is told to stop. Closing a Session where the agent is a co-Reviewer of local work is a normal end, not Abandoned.
_Avoid_: Rejected, cancelled

## Conversation

**Round**:
One batch of feedback handed between the human and the agent in one go. It does not end the Review.
_Avoid_: Pass, iteration, submission

**Thread**:
A conversation anchored to one place in a Changeset, which persists across Rounds until it is Resolved.
_Avoid_: Note, discussion

**Comment**:
A single message inside a Thread.
_Avoid_: Remark, annotation

**Draft**:
A Comment not yet Published to a Forge, whether the human or a co-Reviewer agent wrote it; only the human can keep it for Publish.
_Avoid_: Suggestion, pending comment

**Anchor**:
Where a Thread is attached: a line range on the old or new side of a file, a whole file, or the Review as a whole.
_Avoid_: Position, location

**Outdated**:
The state of a Thread whose Anchor no longer exists in the current Changeset; it still shows the code it was written against.
_Avoid_: Stale, orphaned

**Resolved**:
The state of a Thread that either party has marked as dealt with; it can be reopened.
_Avoid_: Closed, done
