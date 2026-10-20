# Terminal font size follow-ups

**Priority: high.** Photon's default font size became 14 on 2026-10-20, so this may affect normal use.

- `drag_selection_unaffected_by_overlay` fails whenever the terminal font is 14px. It passes at 13px.
  - Setup: select a block with cmd-↑, then drag from the start of its header row to the toolbar at the row's right end.
  - At 13px the drag selects "cargo test". At 14px the mouse press starts no selection at all: `last_content.selection` stays `None`. Starting the drag at column 3 instead of 0 makes no difference.
  - The header row index (5) is the same at both sizes. What changes is the cell size (7.8px → 8.4px), and the grid bounds move (origin y 834 → 838.5, height 992 → 986) because the input slot grows with the font.
  - Next step: check whether a hitbox or overlay (the block input slot, `smart-block-toolbar`, or the block chrome) takes the mouse-down at 14px; `terminal_element.rs` registers the mouse handlers.
  - Until it's fixed, the test is pinned to 13px.
