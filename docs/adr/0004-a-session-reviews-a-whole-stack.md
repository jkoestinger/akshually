# A Session reviews a whole Stack

Pull Requests often come as a Stack, each built on the one below, and reviewing them in separate Sessions loses the thread between them. So a Session on Pull Requests holds one Review per Pull Request of a Stack, each with its own Changeset, Threads, Verdict and Publish, and opening any member opens them all; a lone Pull Request is a Stack of one, on the same path. The Session ends when the human closes it or the last member is Published, and its Ending is Published if at least one member was.

The Stack is detected, not declared: a Pull Request belongs to the Stack of the open Pull Request, in the same repository, whose branch it asks to merge into. A Stack is a line, so it stops below a fork. The Session follows the Stack as it changes, but two open Sessions never merge.

## Considered Options

- **One Review and one Verdict for the Stack, fanned out to every Pull Request**: dropped because a Forge takes a review per Pull Request, and a Stack is often approved at the bottom while its top still needs changes.
- **Separate Sessions, linked for navigation**: keeps "one Session, one Changeset", but an agent co-reviewing a Stack would block on one Pull Request at a time, and the Stack Changeset would belong to no Session.
- **Comments on the Stack Changeset**: left out. A Forge anchors a comment in one Pull Request's own diff, and mapping a line of the Stack Changeset back through the heads in between is unreliable, so the Stack Changeset is for reading only.
