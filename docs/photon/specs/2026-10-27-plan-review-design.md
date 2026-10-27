# Plan review

Review a plan or spec the way Photon reviews a pull request. Review first: the human reads the
plan, Photon shows what it claims and what doesn't add up, and the verdict goes back to the agent
that wrote it. Mock: `2026-10-27-plan-review-mock.html`.

## What opens as a plan review

A Markdown file that has tasks: Superpowers plans (`### Task N:` headings with `**Files:**` and
numbered steps), Spec Kit `tasks.md` (`- [ ] T001 …`), Kiro and OpenSpec task lists, or any file
with two or more H2/H3 sections that contain a checklist. Opening such a file in the preview shows
a **Review this plan** button in the header; `plan: review` on the command palette does the same.
Plain documents keep the normal preview.

## The review item

A tab like the PR review:

- **Header:** the plan's title, its path, what kind of plan and how many tasks, "spec linked" when
  the plan names a spec file that exists, a status pill (Not reviewed, Approved, Changes requested),
  **Request changes**, **Approve plan**, and ⋯ (Open source, Open the spec, Send to agent).
- **Map (left):** progress ("2 of 7 tasks read · 3 gaps"), an Overview step, then one step per
  task with a tick box and a summary (files, tests). The selected task shows chips: each file with
  its state (exists, new, not found), and the test count. Below the steps: **Gaps** (findings) and
  **Decisions** (lines the plan marks as decisions or rulings), each pointing at its task.
- **Document (right):** the selected task rendered, with a facts strip on top: Files, Produces
  (interfaces this task defines, from `**Produces:**`/`Interfaces` blocks or signatures in its code
  blocks), Uses (identifiers it depends on, each marked with the task that defines it or
  "undefined"), Tests. Findings appear inline as cards where they apply, with **Not a problem** and
  **Add to review**. Any paragraph can take a comment (`c`, or the + in the gutter); comments are
  drafts until sent, saved like PR drafts.
- **Footer:** keys. ⌘↵ next task, `c` comment, ⌥T ask the agent about this task (sends the task's
  text and facts to a new thread).

## Checks (no AI; text plus the repository)

1. A file a task names doesn't exist and no earlier task creates it.
2. An identifier a task uses (function or type in backticks or code) that no task defines and the
   repository doesn't contain (checked with a project search, lazily, when the task is selected or
   when the review loads in the background at low priority).
3. A task that adds code (has a code block or a "modify" file) but names no test.
4. Two tasks that modify the same file: listed so the reviewer checks their assumptions agree.
5. When a spec is linked: a requirement heading in the spec with no task mentioning its words.

Findings are shown with the evidence and never block anything.

## The verdict

- **Approve plan** and **Request changes** open a popover: summary text, the counts (comments,
  gaps added, tasks read), and where it goes: the agent thread that produced the plan (when the
  plan was written by a thread in this workspace, found by file path; otherwise a new thread with
  the plan attached), or a `<plan>.review.md` file next to the plan. Send writes the verdict and
  the comments, each quoted with its task and paragraph.
- Tick state and drafts are stored locally per plan path. When the plan file changes, a task
  stays ticked if its text is unchanged; changed tasks show "Changed since you read it".

## Not in this round

Rendered diffs of plan revisions, diagram diffs, spec drift for ordinary documents, ⌥T in the
plain preview. They follow in the next two rounds.

## Rules

Photon crate `photon_plan_review` (parser, checks, review item), reusing photon_review's
patterns (map, drafts, popover) and photon_markdown where useful; nothing at idle; lazy checks;
every behaviour covered by GPUI tests driving the UI, parser unit tests on fixtures for each plan
format, and screenshot-runner scenes (`plan-review`, `cu-plan-gaps`) for the website.
