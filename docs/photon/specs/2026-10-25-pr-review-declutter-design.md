# PR review, decluttered

## Why

The review tab grew one control at a time. The header holds up to eleven things, progress is
shown three times, a 260px Impact column is always open, and ten checklist items mean thirty
buttons of equal weight. The approved mockup
(`.superpowers/brainstorm/90065-1792812581/content/pr-review-declutter.html`, Alternative A) keeps
every capability and moves most of them one click away.

## What changes

1. **One-line header.** Title, `author → base`, a checks icon (tooltip says passing, failing or
   pending) and a `ui::DiffStat` (tooltip gives the file count). On the right: **View ▾**, a **⋯**
   menu, and one primary button: **Submit review**, or **Push changes…** on your own PR (where
   Submit review stays as a quiet button, since the author can still post comments).
2. **View menu** gains **Only changes since your review** (a toggle, shown only when offered, with
   its key binding). While it is offered and the menu hasn't been opened yet, a dot sits on the
   View button. The header button for it goes. The trigger reads "View · since review" while on.
   Hidden code: Photon drops a hidden file from the map entirely, so there is no step and no place
   in the diff to fold it into. This pass keeps the list, moved behind a **Show what's hidden**
   toggle in the View menu, and draws it above the diff instead of at the bottom of the map.
3. **⋯ menu**: Pre-review with agent (reviewers only; reads "Cancel agent pre-review" while it
   runs), Run locally, Add your own checklist…, Open on GitHub. Running and result notes (agent,
   changes since review, comment notes) become workspace toasts; the agent's running toast has a
   Cancel action.
4. **Progress** ("x of y steps viewed") appears once, at the top of the review map. The checklist
   count lives only on the checklist's own line; the Submit popover repeats both as facts.
5. **Checklist** starts folded to one line with its count (the existing Disclosure). Open, each
   item has one `Checkbox` (ticked = Yes) and a ⋯ menu: "No, add a note", "Doesn't apply",
   "Clear answer". A failed auto check has no checkbox and offers only "Doesn't apply".
   "Checked by Photon" and "The author ticked" each fold to a one-line summary. Path-specific
   items under a step become one link line ("N checklist items apply to these files") that opens
   the checklist and scrolls the map to it, instead of a second copy.
6. **No Impact column.** A changed function with a caller that wasn't updated gets a red
   "N not updated" tag on its row in the step. Clicking a function opens its callers in a popover.
   Legends become tooltips on the function rows. While callers load, nothing extra is shown; a
   function row becomes clickable once its callers are known. Callers are computed once when the
   call information arrives, not per frame or per cursor move.
7. **One visible action per card.** Feedback rows: click the row to show the thread; Edit and Done
   are icon buttons that appear on hover. Saved drafts: Edit and Delete appear as icon buttons on
   hover. Composers: Add to review stands out; Suggest change and Cancel are quiet. Threads:
   Reply stands out; Resolve is quiet. Outdated drafts: one ⋯ menu each (Place on the selected lines,
   Include again, Post as a general comment, Delete draft).
8. **Submit popover**: verdict radio (Comment / Approve / Request changes; only Comment on your own
   PR), the summary editor, draft count, steps viewed, checklist status, the left-out and
   maybe-posted warnings, and the last error. The button label follows the verdict. Success closes
   the popover and shows a toast.

## What stays

- Every action: step viewed ticks, the View toggles, since-review mode and its key binding,
  agent pre-review, run locally, own checklists, push, threads, suggestions, drafts, outdated
  drafts, all three verdicts.
- Review logic, storage and GitHub requests are untouched; only where things are drawn moves.
- No new per-frame work and no polling: tags and callers are computed when call data arrives,
  popovers build their contents when opened.
