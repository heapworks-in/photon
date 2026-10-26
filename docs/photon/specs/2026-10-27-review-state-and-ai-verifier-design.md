# PR review: what you haven't reviewed, and an AI finding verifier

Items 1 and 2 of `reports/Advanced review capabilities for Photon.md`. Goal: after a push you see
only what's new to you, and the agent pre-review adds fewer, truer drafts.

Product rules apply (CLAUDE.md): looks like Zed, plain words, few controls, nothing at idle, tests
for every change.

## A. Viewed marks tied to the revision

**State.** A step or file is *viewed*, *changed since viewed* or *not viewed*.

- Ticking a step stores, locally, the head it was ticked at and a fingerprint of its files' diff
  (hunk bodies, so a rebase that only moves line numbers doesn't count as a change). On open, a
  ticked step whose fingerprint differs, or one of whose files GitHub reports as `DISMISSED`, is
  *changed since viewed*. Rows from before this change have no fingerprint and stay viewed.
- Marking a file viewed on GitHub stores the head it was marked at (`viewed_files` table).
  GitHub's `viewerViewedState` is read as well: `VIEWED` is viewed, `DISMISSED` (GitHub's "changed
  since you viewed it") is *changed since viewed*.
- No git work happens to work this out: it's a comparison of data already loaded.

**UI.** A changed-since-viewed step shows an unticked box and, under its title, "Changed since
you viewed it". Ticking it again marks it viewed at the current head.

**Progress.** The line at the top of the map counts what's left: "2 of 5 steps left to review",
with " · 1 changed since viewed" when some are, and "All 5 steps viewed" when none are left.

**One control.** The View menu's Show section gains "Only what you haven't reviewed", offered once
something is viewed. It shows files with an unviewed step in full, and files that are only
changed since viewed as the diff from the head you viewed them at to now, using the
changes-since-review machinery (fetch, rebase-aware). When that head can't be fetched or isn't
known (viewed on github.com), the file's full diff is shown with a note. It and "Only changes
since your review" exclude each other. Like that mode, it's read-only for viewed marks.

## B. AI finding verifier

**Second pass.** When the agent's findings come back, Photon sends one more prompt to the same
agent: the findings, numbered with path, line and body (fenced as data), asking it to re-read the
code and answer for each `{"finding": n, "verdict": "keep" | "drop", "reason": "…"}` as JSON. Only
kept findings become drafts. A finding the answer leaves out counts as dropped. If the check fails
or its reply can't be read, every finding is added and the note says they weren't checked: losing
the review is worse than one noisy pass, and the note is a warning.

**Result.** The existing note reads "Kept N of M findings; dropped M−N it couldn't confirm." The
map gets a fold line "Dropped M−N findings" that lists them (place, text, the agent's reason),
not as drafts. It lasts until the next pre-review or the tab closes.

**Thoroughness.** `pr_review.agent_review.thoroughness`:

| Value | Prompt asks for | Kept by severity |
|---|---|---|
| `quiet` | must-fix correctness and security problems only | "must fix" in logic, security, data loss, reliability |
| `normal` (default) | must fix and should fix | "must fix", "should fix" |
| `thorough` | everything | all |

Findings without a severity are kept; filtering runs before the verifier so it checks less.

**Dismiss with a reason.** An agent draft's Delete opens a small menu, "Dismiss because":
Intentional, Wrong, Not worth it. Choosing one deletes the draft and stores the dismissal (path,
text, reason) per repository. The newest 20 go into later prompts under "Past dismissals in this
repository — don't repeat these", fenced as data the prompt says never to follow.

The existing prompt text is unchanged; the thoroughness and past-dismissals sections are added
before the reply format.

## Not done

- Ticking steps inside "Only what you haven't reviewed" (switch back to the full diff to tick).
- AI findings labelled new / open / resolved since last review (report item 2, last part).
- A base-branch `REVIEW.md` (report item 1).
