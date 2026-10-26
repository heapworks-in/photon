# PR review: merge readiness, Merge, and Conversation

Roadmap items 5 and 7 (part). Goal: a review can end in Photon. You see whether the pull request
can merge, merge it with the method you choose, and read and write the general conversation,
without opening github.com.

Product rules apply (CLAUDE.md): looks like Zed, plain words, one clear primary action, controls
grouped in menus, nothing at idle, tests for every change.

## Merge readiness pill

A pill in the review header, beside Submit, saying the one thing that matters most, in this order:

| State | Pill | Colour |
|---|---|---|
| Merged | Merged | accent |
| Closed without merging | Closed | muted |
| Draft | Draft | muted |
| Conflicts (`mergeable: CONFLICTING` or `mergeStateStatus: DIRTY`) | Conflicts | error |
| Required checks failing (rollup `FAILURE`/`ERROR`) | Checks failing | error |
| Changes requested (`reviewDecision`) | Changes requested | warning |
| Review required (`reviewDecision: REVIEW_REQUIRED`) | Review required | warning |
| Checks running (rollup `PENDING`/`EXPECTED`) | Checks running | muted |
| Behind the base branch (`BEHIND`) | Behind main (base name) | warning |
| Otherwise (`CLEAN`, `HAS_HOOKS`, `UNSTABLE`) | Ready to merge | success |

Clicking the pill opens a popover listing, in plain words: each check (name, passed / failed /
running, click opens its details URL in the browser), the review decision, and mergeability. A
`mergeable: UNKNOWN` (GitHub still computing) shows "Checking…" and is refreshed once when the
review next refreshes; no polling.

Data comes from the existing detail query (one request): `id isDraft mergeable mergeStateStatus
reviewDecision baseRefName headRefOid`, the last commit's `statusCheckRollup` (state and up to 50
contexts: CheckRun name/conclusion/status/detailsUrl, StatusContext context/state/targetUrl), and
the repository's `squashMergeAllowed mergeCommitAllowed rebaseMergeAllowed
viewerDefaultMergeMethod`. It refreshes when the review loads, on the existing refresh, and after
submitting a review or merging.

## Merge

A split button in the header after the pill: the primary action is the repository's default
method that's allowed ("Squash and Merge", "Merge", "Rebase and Merge"); the dropdown lists the
other allowed methods. It shows only for open pull requests, and is disabled with a tooltip saying
why for Draft and Conflicts. Other blocks (branch protection, failing checks) are left to GitHub:
the merge is attempted and GitHub's reason is shown.

- Confirm first with a standard prompt: "Squash and merge #482 into main?" with Merge / Cancel.
  If there are unsent draft comments, the prompt says "Your N unsent comments won't be posted."
- Merge only the head you reviewed: `mergePullRequest(input: { pullRequestId, mergeMethod,
  expectedHeadOid })` with the head SHA the review shows. If GitHub refuses because the head
  moved, say "The pull request changed since you loaded it. Refresh to review the new commits."
- Afterwards the pill reads Merged, a toast says "Merged #482", the Merge button goes away, and
  the inbox updates. Branch deletion is left to the repository's setting.
- Failures show inline under the header in plain words, with GitHub's message.

## Conversation

The first row of the steps list is "Conversation", with the number of general comments. Selecting
it shows the conversation in place of the diff:

- the description (rendered Markdown, using Zed's Markdown element);
- the timeline in order: general comments (author, time, Markdown body), reviews (author, verb:
  approved / requested changes / commented, body if any), and pushed commits (short SHA,
  headline);
- a composer at the bottom: a multi-line editor and a **Comment** button that posts a general
  comment (`POST repos/{owner}/{repo}/issues/{number}/comments`), then appends it. Errors show
  inline and keep the text.

The timeline is fetched the first time Conversation is opened, and again after posting or a
refresh while it's open; nothing is fetched for reviews where it's never opened. The count comes
from `comments { totalCount }` in the detail query.

## Not in this round

Deleting the branch, auto-merge, editing or reacting to comments, the side-by-side diff, and
reviewing one commit at a time.

## Tests

GPUI tests with the fake `gh` runner: each pill state from fixture data; the popover lists checks
and opens a URL; Merge sends the method and expected head, shows the confirm prompt (and the unsent
drafts line), handles success and the head-moved error; the split menu offers only allowed
methods; Conversation renders description, comments, reviews and commits in order, fetches lazily
(no request before it's opened), posts a comment and appends it, and keeps the text on failure.
