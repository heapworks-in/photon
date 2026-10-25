# Photon roadmap

Motto: **review first, review fast.** Every item follows the product rules in CLAUDE.md: looks like
Zed, simple UX, never resource hungry, fully tested.

## In progress
- [ ] Idle memory regression in 0.1.5 (+56 MB vs 0.1.4, no diagram open): root cause, then fix.
- [ ] Open a pull request from its URL (branch `open-pr-by-url`, built and tested).
- [ ] Call hierarchy tree panel: "Keep open" from Zed's call hierarchy popup; multi-level, lazily expanded tree of callers or calls.

## Next: review agent work
1. [ ] Agent threads anchored to a selection (⌥T, `path:lines` chip), "this file" filter, list badges (running, failed, finished in background).
2. [ ] Inline edits with your agent (⌥E), queued and sent with **Apply all**; each batch kept as a thread.
3. [ ] Snapshot before each agent turn, **Revert turn**, and per-turn changed-file chips with that turn's diff.
4. [ ] Plan review (spec-driven, v1): plans from Superpowers, Spec Kit, Kiro, OpenSpec shown as tasks, files, interfaces, tests and decisions made for you; gap checks; approvals and comments go back to the agent.

## Then: finish PR review
5. [ ] Merge readiness pill and Merge (squash, merge, rebase), checking the reviewed head.
6. [ ] Apply review comments with an agent; rebase or merge with AI, force-push with lease.
7. [ ] Side-by-side diff, Conversation (general comments, timeline), Commits (review one commit at a time).
8. [ ] Your own edits marked "YOU" in their own section; Push / Pull / Force-push in the review; AI squash message.
9. [ ] Thread scope in PR reviews: whole PR, my changes, selection.
10. [ ] GitLab, Gitea/Forgejo, Bitbucket.

## Then: git
11. [ ] "Changes" view in the file tree with M/A/D/U/R badges.
12. [ ] Commit history: expandable commits, hover cards with Copy SHA; commit-pinned diffs with ‹ › file stepping.
13. [ ] Commit message with your agent, `git.commitMessageInstruction`, keep the draft when a hook fails.
14. [ ] Pull fast-forward only with a "can't fast-forward" card (rebase or merge with AI); `-u` on first push; one persistent git error card.

## Then: understand code
15. [ ] Find usages of the selection (⌥U).
16. [ ] Language server setup help: "Set up" status with the install command and Detect & Start; fallback outline without a server.

## Then: docs and diagrams
17. [ ] Markdown preview: source ↔ preview in one tab, edit from the section you're reading, Mermaid zoom/fullscreen and theme re-render, image lightbox, "not found" image card, code copy button, relative links open in a tab.
18. [ ] Image viewer: pan, zoom, background contrast, pixel-exact mode.
19. [ ] Excalidraw: `.excalidraw.svg` / `.png`, diagrams in Markdown preview, Reveal in Finder after export, "Open as text" for broken files.

## Then: workbench
20. [ ] Open a folder or PR by path or URL with autocomplete; pinned recent projects and PRs with status; stop idle workspaces; CPU · RAM · language server readout.

## Quality and housekeeping
- [ ] Look over the PR review redesign by eye (View dot, Submit popover, hover icons).
- [ ] Confirm Excalifont's licence text.
- [ ] Flaky tests: tab restore order, extension suggestions, settings scroll.
- [ ] Release-build performance check in CI.
- [ ] Keep rust-analyzer from building whole PR checkouts.
- [ ] GitHub on the enterprise machine (needs the error text).
