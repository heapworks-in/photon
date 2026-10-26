# Review agent work

Roadmap items 1–3 ("Next: review agent work"). Photon runs Claude Code and Codex over ACP; Zed's
native agent is off. This spec builds only what Zed lacks for those agents and wires Photon's
shortcuts to what Zed already has.

## What Zed already has

| Need | Zed today | Where |
|---|---|---|
| Put a selection in a thread | `agent::AddSelectionToThread` (cmd->) inserts a selection mention into the active thread's message editor; an empty selection takes the cursor line | `crates/agent_ui/src/agent_panel.rs:691`, `completion_provider.rs:58` |
| A new thread with the selection | Not one action, but `["action::Sequence", [ToggleFocus, NewThread, ToggleFocus, AddSelectionToThread]]` works and is tested upstream (`test_add_selection_to_loading_thread`) | `agent_panel.rs:9536`, `crates/settings/src/keymap_file.rs:1477` |
| Selection chip | Mention crease labelled `file.rs (12:14)` | `crates/acp_thread/src/mention.rs:842` |
| Thread list badges | Threads sidebar rows show running (spinner), waiting (warning), error (x) and "finished in background" (dot, set when a thread goes Running → Completed while not active) | `crates/sidebar/src/sidebar.rs:1797`, `crates/ui/src/components/ai/thread_item.rs:314` |
| Thread list filter | Text filter over titles and worktree names | `sidebar.rs:1874` |
| Snapshot per turn | A git checkpoint before each user message and "Restore Checkpoint" (rewinds the conversation, restores files), **only when the agent supports truncation** | `crates/acp_thread/src/acp_thread.rs:6858`, `:4109`, `:7342`; `conversation_view/thread_view.rs:7264` |
| Whole-thread review | Agent diff pane, keep/reject per hunk, from the action log | `crates/agent_ui/src/agent_diff.rs` |
| Diff of two texts | `FileDiffView::new(old_buffer, new_buffer, diff, …)` | `crates/git_ui_core/src/file_diff_view.rs:105` |
| Inline edit at a selection | Inline assistant (ctrl-enter) | `crates/agent_ui/src/inline_assistant.rs` — calls a language model directly, not an ACP agent, so it has nothing to talk to in Photon |

The ACP connections Photon uses (`crates/agent_servers/src/acp/v1.rs:1223`) implement neither
`truncate` nor client message ids, so for Claude Code and Codex Zed takes **no** checkpoints and never
shows "Restore Checkpoint". That is the main gap for item 3.

## 1. Ask about a selection

- **⌥T** in an editor with a selection: a new thread in the agent panel with the selection as a
  chip. Bound in `assets/keymaps/default-macos.json` to the `action::Sequence` above. No new code.
- The chip reads `file.rs:12-14` (the `path:lines` form Photon uses everywhere), from
  `selection_name` (hook in `mention.rs`).
- Badges: already in the threads sidebar. Nothing to build.
- **This file** filter: a toggle button (file icon) in the threads sidebar's search row. When on, the
  list keeps threads whose prompts mentioned the active editor's file (file, selection or symbol
  mentions) or whose turns changed it (item 3). The index lives in `photon_agent::ThreadFiles`:
  session id → absolute paths, recorded as user messages and turns arrive, stored as one key-value
  row. It is read the first time the filter is turned on, never at startup.

### Keybindings

On macOS ⌥T types `†` and ⌥E starts the `´` dead key, and GPUI prefers key bindings over character
input. So both bind only in `Editor && mode == full && has_selection`: typing is untouched unless
text is selected, and typing a character over a selection (which would replace it) is the only thing
lost. `has_selection` is a new editor key-context flag (one-line hook in `editor.rs`), set when the
newest selection is not empty — the same check the editor already makes for `start_of_input`.
Neither key is bound anywhere else in the default macOS keymap.

## 3. Per-turn review

- **Snapshot before each turn**: `send_inner` also checkpoints root threads that cannot rewind
  (hook). The checkpoint is stored on the user message as before; Zed's "Restore Checkpoint" stays
  hidden for these agents because it needs truncation.
- When a turn stops, `photon_agent` takes a second checkpoint and asks git which files differ
  (`Repository::diff_checkpoints`, one call per repository). Nothing runs while idle.
- Under the user message of a turn that changed files: one chip per file (`ButtonLike` with the
  file icon and name, like the edits bar) and **Revert Turn**.
  - A chip opens that file's diff for the turn in a `FileDiffView` tab titled `file.rs (turn)`: before
    and after texts read from the two checkpoints (`load_revisions("<sha>:<path>")`).
  - **Revert Turn** restores the files to the snapshot taken before the turn
    (`GitStore::restore_checkpoint`, Zed's restore without rewinding the conversation). Like Zed's
    restore it puts back every file, so later turns' edits to other files are undone too; the
    tooltip says so.
- Remote projects: chips and Revert Turn need a local repository; they don't show for remote ones.
- Not covered: ACP v2 receipt submissions (Zed checkpoints none of them); an agent that commits
  during a turn (the commit's files are then not in the turn's diff).

## 2. Inline edit requests (built last, if time allows)

- **⌥E** on a selection opens a one-line prompt block under the selection (Zed's editor block, as
  the inline assistant uses). Enter queues the request; Escape cancels.
- A queued request shows as a highlight on its lines and a one-line block with the instruction,
  **Apply All** and a remove button.
- **Apply All** starts one thread with every request as `path:lines` chip + instruction, sent at
  once. The thread stays in the list like any other. The queue empties.

## Upstream hooks (all marked `// photon:`)

- `editor.rs`: `has_selection` key context.
- `acp_thread/src/mention.rs`: chip label.
- `acp_thread/src/acp_thread.rs`: checkpoint for non-rewinding root threads; `Checkpoint` accessor.
- `project/src/git_store.rs`: read the per-repository checkpoints.
- `agent_ui`: record thread files and turn ends; render the turn row.
- `sidebar`: the This File toggle and filter.
- `git_ui_core/src/file_diff_view.rs`: tab title override.
- `fs/src/fake_git_repo.rs` (test support): read a file at a fake checkpoint.

## Tests

GPUI tests with stub ACP connections (`StubAgentConnection`, with truncation switched off to match
the real ACP connection): ⌥T from an editor with a selection opens a new thread whose message editor
holds the `path:lines` chip; ⌥T without a selection types; This File keeps only matching threads;
a turn that edits a file shows its chip, the chip opens the turn diff, Revert Turn restores the file.
