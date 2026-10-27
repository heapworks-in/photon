# Call Trails redesign

The call hierarchy panel (`crates/photon_call_trails`) is a bare tree: names and paths crammed
together, "No callers" rows, no sense of how a function is called or which callers matter. The
user asked for a far better, creative but professional view, inspired by Devin's Codemaps
(grouped traces linked to exact lines, with a list view and a diagram view of the same map).

The approved mock is `2026-10-27-call-trails-mock.html` (open it in a browser). Build both views.

## List view (default)

- **Header:** a kind glyph and the root's name (with its owner, e.g. `Gateway::charge`), its
  signature in one muted line, and a "changed in PR #N" chip when the root is changed in an open
  review. Below: a Callers / Calls segmented control, a List / Map toggle, and a summary
  ("4 callers · 2 entry points · 1 test").
- **Trail breadcrumb:** the path from the root to the selected row (`main › run_nightly ›
  retry_failed_invoices › charge` for callers; reversed meaning for calls). Clicking a crumb
  selects that row.
- **Rows:** indent guides, a chevron, a kind glyph (function, method, test, entry point), the name,
  the file name (full path in a tooltip), chips only when useful (entry point, HTTP handler or
  similar when detectable, test, recursive), and a call-site count (×2) on the right. No
  "No callers" rows; a leaf simply has no chevron.
- **Call sites inline:** expanding a row's call site (space, or clicking the count) shows the
  call line with one line of context above and below, syntax highlighted, the call line marked.
  Clicking opens the file at that line.
- **Footer:** keyboard hints (↵ open, space show call site, ⌥← make root) and an optional
  "Explain this trail…" that asks the user's own agent (through the existing agent path) for a
  short explanation of the selected trail. Off the critical path; nothing runs unless clicked.

## Map view

The same explored trail drawn left to right with GPUI (no web view): nodes as rounded pills with
glyph, name and a muted detail; edges as curves into the root; the selected trail lit in the
accent colour, everything else dimmed. Click a node to select it and open its code; double-click
to expand it. Layout is deterministic (columns by depth, rows ordered as in the list) and stays
readable with ~30 nodes (scroll/pan beyond that).

## Rules

- Data comes from the language server (call hierarchy) and loads lazily as rows expand; nothing at
  idle; entry points/tests detected cheaply (e.g. `main`, `#[test]`/test modules, framework
  handler attributes when visible in the symbol data) — never by scanning the whole project.
- Look and behave like Zed: Zed's list, indent guides, chips, colours and type scale.
- Keep the existing behaviours (Keep Open from the popup, Callers/Calls, recursion marks, make
  root) working.
