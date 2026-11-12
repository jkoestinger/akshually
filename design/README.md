# Design

`akshually.figcraft` is the visual reference for akshually's screens and interaction states. Issues point here. Open it in FigCraft. `png/` holds exports for viewing on GitHub; re-export them when the file changes.

| Page | Frame | Shows |
| --- | --- | --- |
| Screens | 1 · Session — your Turn (Round 2) | Tabs with attention markers, Session header, since-last-Round toggle, viewed ticks, an inline Thread, a Draft being written, an Outdated Thread, a collapsed generated file |
| Screens | 2 · Session — agent's Turn (Light theme) | Waiting banner, drafting during the agent's Turn, Take Turn back |
| Screens | 3 · Inbox | Pull Requests across Forges, new / updated / Session-open states, open by URL |
| Screens | 4 · Publish dialog | Kept and discarded Drafts, reply on an existing Thread, Verdict choice |
| States & interactions | States | Tab states, reload banners, agent Draft with a suggestion, Resolved Thread, interaction rules |

## Design tokens

The Rune theme JSON in the repo is the source of truth for colours, radii and type. The `akshually` variable collection here (Dark and Light modes) mirrors it. Until the theme JSON exists (see the spec, #7), these variables are the provisional values to start it from.

## Constraints from Rune

v1 starts from Rune's kit look; our own pieces are the tab strip, Threads, Drafts and attention markers. The diff gutter (line numbers, change bar, padding) is hard-coded in `rune-kit` and is not restylable without forking it, so designs keep it close to the default. Threads render through the diff view's per-line annotation slot, and file-header controls through its header actions slot.
