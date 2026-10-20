# SP4a.1 Smooth PR Review Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Click → a tab opens instantly showing the file list → nothing jumps afterwards → switching PRs is instant.

**Architecture:**
- All PRs open as tabs (`ReviewTab`, a new workspace item in `review_tab.rs`) in one dedicated Review window. That window's project holds only PR worktrees, each **visible**, so SP4a's trust and per-worktree language-server rules apply unchanged. The tab exists from the click onwards. It shows the PR summary and a progress stage, then hosts the existing `ReviewItem` once the file list is known. Files then stream into that `ReviewItem`.
- The open pipeline gets shorter:
  - the `gh` login is cached;
  - the clone comes from the inbox window's repo instead of a recent-projects scan;
  - the detail call runs alongside the fetch;
  - a warm worktree opens from local data and a cached detail;
  - base texts come from one `git cat-file --batch`.
- `ReviewItem` stops moving the screen:
  - the map is stable (path order), and call-flow order is opt-in through a chip;
  - the status strip has a fixed height;
  - each code hunk has one fixed-height context row that expands on click;
  - scroll is kept per step, and expanding keeps the clicked row in place;
  - drafts are re-anchored before their cards are drawn, and later changes redraw only one card;
  - Since review swaps only diff content;
  - the impact list has reserved space.

**Tech Stack:** Rust, GPUI, `editor` (blocks, scroll), `project` (`find_or_create_worktree`, `remove_worktree`, `trusted_worktrees`), `workspace` (`Workspace::new_local`, `MultiWorkspace`), sqlez, `gh` and `git` subprocesses.

**Spec:** `docs/photon/specs/2026-10-20-sp4a1-smooth-pr-review-design.md`. The SP4a spec and plan (`docs/photon/plans/2026-10-19-sp4a-pr-review.md`) still bind except where this spec changes them.

## Global Constraints

- Fork rules (CLAUDE.md): all code stays in `crates/photon_review`. This plan changes no Zed file. If a task finds that it needs one, the change is a minimal hook marked `// photon: <why>`.
- **Anti-goals (verbatim):** Anything that moves on screen without a user action. A blank wait with no stage shown. Opening a new window for each PR. Only the one Review window is allowed. Weakening any SP4a safety rule for speed.
- **SP4a rules that still hold:**
  - the network rule (only `gh` plus git `clone`/`fetch`/`worktree add`/`checkout`, and only from user actions; no polling);
  - the no-token rule;
  - never touch the user's checkout;
  - `forget_stale_trust` runs before a moved worktree is opened;
  - `refuse_parent_trust` stays in place;
  - a PR's code is never trusted automatically.
- Background work started by the click (the new-commit fetch and the detail refresh after a warm open) counts as the user's action. Nothing repeats on a timer.
- **Budgets:**
  - The tab is visible on the frame after the click.
  - First diff in under 1 s for a warm worktree with an unchanged head.
  - Zero layout shift after the first diff, except from user actions.
  - Switching PR tabs keeps each tab's scroll, step and selection.
- **Pinned copy:**
  - Stages: `Checking GitHub login…`, `Fetching commits…`, `Preparing worktree…`, `Loading {n} file(s)…`. In the item's strip while files stream: `Loading files… {loaded} of {total}`.
  - Inbox: the button reads `Opening…` while the tab loads. Empty states:
    - `No GitHub repository in this window.`
    - remote windows: `Pull request review isn't available in remote windows.`
  - Chips: `Order by call flow` and `Order by path`. New commits: `{n} new commit(s) · Reload`.
  - Hunk context row:
    - while loading: `callers: loading…`;
    - with no call information: `callers: unavailable`;
    - when ready: `{n} caller(s)`, plus ` · {k} not updated` when k > 0. Zero callers reads `no callers`. A trailing ` ▸` (collapsed) or ` ▾` (expanded) appears when there is code to show.
  - Retry button: `Retry`.
- Sizes: the status strip is `STATUS_STRIP_HEIGHT = px(28.)` everywhere, in both the tab's progress line and the item's strip. Each hunk context row is exactly 1 editor line. The impact list reserves `IMPACT_LIST_ROWS = 6` rows. The impact panel stays `px(260.)` wide.
- **Tests:**
  - No test touches the network or the real repo. Use `FakeGh`, `GitFixture` (local `file://` repos), `ReviewDb::open_test_db` and `COMMAND_LOG`.
  - Tests that need git skip when `git` is missing.
  - Use GPUI executor timers, never `smol::Timer`.
- **Commands:**
  - tests: `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`;
  - lint: `CARGO_INCREMENTAL=0 ./script/clippy -p photon_review`.
  
  Disk is tight, so always set `CARGO_INCREMENTAL=0`.
- Commits start with `photon: ` and end with your own model's trailer, e.g. `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Keep `open_review_from_inside_a_workspace_update_does_not_panic` passing. Nothing reads the clicked `Workspace` entity through `workspace_store` while that workspace is being updated, and the same holds for `InboxPanel::refresh`, which runs inside workspace actions.

## Review Focus

1. **Two PR tabs in the Review window, one trusted and one not.** Trusting one PR must load its callers. The untrusted one keeps waiting, and neither is blocked or trusted by the other. Test: Task 10 `untrusted_pr_does_not_block_a_trusted_one`.
2. **Odd paths in the batched base read:** spaces, unicode, renames, deleted, binary and added files. A path containing a newline (which `--batch` can't carry) falls back to `git show`. Test: Task 1 `base_texts_match_git_show_for_odd_paths`.
3. **A window opened on a fork** (origin = fork, upstream = base) lists the base repo's PRs. A PR whose repo matches no remote in the window opens from the private clone. Tests: Task 4 `fork_window_lists_upstream_prs`, `pr_repo_matching_no_remote_uses_private_clone`.
4. **Clicking a PR again while its tab is opening, or after it failed.** Either click focuses the same tab and starts no second pipeline. Retry reruns once. Test: Task 10 `second_click_while_opening_focuses_the_same_tab`.
5. **A stale inbox head or a force-push.** A worktree whose HEAD differs from the inbox head is not warm. A force-pushed head still offers Reload with a count. Tests: Task 3 `stale_inbox_head_is_not_warm`, `force_push_offers_reload`.

---

### Task 1: Batched base text

**Files:**
- Modify: `crates/photon_review/src/checkout.rs` (add `git_with_stdin` next to `git_raw`, logging to `COMMAND_LOG` the same way)
- Modify: `crates/photon_review/src/diff.rs` (add `base_texts`; keep `base_text` for the fallback)

**Interfaces:**
- Produces:
```rust
// checkout.rs
pub(crate) async fn git_with_stdin(dir: &Path, args: &[&str], stdin: String) -> Result<Vec<u8>>;
// diff.rs: one entry per input file, same order; None where base_text would return None
pub async fn base_texts(worktree: &Path, commit: &str, files: &[FileDiff]) -> Result<Vec<Option<String>>>;
```
- Run one `git cat-file --batch` over `{commit}:{old_path}\n` lines, using the same old-path rules as `base_text`: a rename's `from`, `None` for Added and opaque files. Parse each `<oid> <type> <size>\n<bytes>\n` reply by its byte size. A `<name> missing` reply is an error naming the path. Contents are decoded with `String::from_utf8_lossy`, as `git_raw` does. Paths containing `\n` use `base_text`.

- [ ] **Step 1: Write failing tests** in `diff.rs` (real repo, as in `diff_handles_renames_deletes_binaries_and_spaces`):
```rust
#[gpui::test] async fn base_texts_match_git_show_for_odd_paths()
    // files: "dir/a b.txt" modified, old.rs->new.rs renamed, gone.rs deleted, logo.png binary, "café.rs" added
    // assert_eq!(base_texts(..).await?, files.iter().map(|f| base_text(..f).await?).collect::<Vec<_>>())
#[gpui::test] async fn base_texts_runs_one_git_process()
    // COMMAND_LOG cleared, 5 modified files -> exactly one entry containing "cat-file", none containing "show"
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review base_texts`. Expected: FAIL (`base_texts` missing).
- [ ] **Step 3: Implement** `git_with_stdin` and `base_texts`.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `photon: read every base text in one git cat-file`.

### Task 2: Cached login, detail alongside fetch, detail cache

**Files:**
- Modify: `crates/photon_review/src/gh.rs` (`LoginCache`)
- Modify: `crates/photon_review/src/store.rs` (one new migration appended; never edit the old ones)
- Modify: `crates/photon_review/src/review.rs` (`fetch_review`, `finish_review`, `prepare_review`)
- Modify: `crates/photon_review/src/inbox.rs` (`load_inbox` fills the cache)

**Interfaces:**
- Consumes: `base_texts` (Task 1).
- Produces:
```rust
// gh.rs. Global (default empty); cloned handles share one map.
#[derive(Clone, Default)] pub struct LoginCache(Arc<Mutex<HashMap<String, String>>>); // host (lowercase) -> login
impl LoginCache {
    pub fn global(cx: &App) -> Self;
    pub fn remember(&self, hosts: &[HostAuth]);           // logged-in hosts replace their entry; logged-out hosts are removed
    pub fn forget(&self, host: &str);
    pub async fn login(&self, host: &str, runner: &dyn GhRunner) -> Result<String>; // runs auth_status_request only on a miss; LoggedOut{Some(host)} if none
}
// store.rs. Migration: CREATE TABLE pr_details(host, owner, repo, number, head_sha TEXT, json TEXT, PRIMARY KEY(host, owner, repo, number)) STRICT;
//                      CREATE TABLE review_ui(host, owner, repo, number, call_flow INTEGER NOT NULL, PRIMARY KEY(host, owner, repo, number)) STRICT;
pub async fn save_detail(&self, key: PrKey, head_sha: String, json: String) -> Result<()>;
pub fn cached_detail(&self, key: &PrKey, head_sha: &str) -> Result<Option<String>>; // None if the stored head differs
pub fn call_flow_order(&self, key: &PrKey) -> Result<bool>;                          // false when no row
pub async fn set_call_flow_order(&self, key: PrKey, on: bool) -> Result<()>;
// `untrack` also deletes the key's pr_details row.
// review.rs
pub(crate) async fn fetch_review(summary: &PrSummary, fetch_url: String, root: PathBuf, clone_dirs: Vec<PathBuf>,
    runner: Arc<dyn GhRunner>, logins: LoginCache, database: ReviewDb) -> Result<(String, FetchedCheckout, PrDetail)>;
pub(crate) async fn finish_review(summary: &PrSummary, login: String, fetched: FetchedCheckout, detail: PrDetail,
    database: ReviewDb) -> Result<PreparedReview>;
#[cfg(test)] pub(crate) async fn prepare_review(summary: &PrSummary, fetch_url: String, root: PathBuf, clone_dirs: Vec<PathBuf>,
    runner: Arc<dyn GhRunner>, logins: LoginCache, database: ReviewDb) -> Result<PreparedReview>;
```
- `fetch_review`:
  1. Get the login.
  2. Run `try_join(fetch_checkout(...), detail)`, where the detail future runs `detail_request` and `parse_detail`, then stores the raw JSON with `save_detail(key, detail.summary.head_sha, json)`.
  3. A `GhError::LoggedOut` from any `gh` call calls `logins.forget(host)` before returning the error.
- `finish_review` calls `finish_checkout`, `track`, `git_diff` and then `base_texts`. It makes no `gh` call.
- `load_inbox` calls `LoginCache::remember` with the parsed hosts. Inbox refresh is a user action, so this is also where an expired login gets noticed.

- [ ] **Step 1: Write failing tests:**
```rust
// gh.rs
#[gpui::test] async fn login_is_checked_once_per_session()     // two cache.login("github.com") -> fake.requests() has 1 auth request
#[gpui::test] async fn logged_out_host_is_forgotten()          // remember(alice) then forget -> next login() runs auth again
// store.rs
#[gpui::test] async fn detail_cache_is_keyed_by_head()         // save(head "a") -> cached_detail(key,"a") Some, (key,"b") None; untrack -> None
#[gpui::test] async fn call_flow_order_round_trips()           // default false; set true -> true
// review.rs (GitFixture)
#[gpui::test] async fn open_review_leaves_user_clone_untouched() // existing; adapt to new prepare_review args; also assert database.cached_detail(&key(), &fixture.pr_sha) is Some
#[gpui::test] async fn detail_failure_after_auth_expiry_forgets_login()
    // cache has alice; detail responds Err(LoggedOut{Some("github.com")}) -> error text "Not logged in"; next login() issues auth
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review login detail_cache call_flow open_review_leaves logged_out`. Expected: FAIL.
- [ ] **Step 3: Implement.** Update `logged_out_fails_before_checkout` to the new signature. Its assertion stays: one request, no root created.
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS. Concurrency between fetch and detail has no test; the reviewer checks the `try_join`.
- [ ] **Step 5: Commit** `photon: cache the gh login and fetch the PR detail alongside the commits`.

### Task 3: Warm open and the new-commit check

**Files:**
- Modify: `crates/photon_review/src/checkout.rs`
- Modify: `crates/photon_review/src/review.rs` (`prepare_warm`)

**Interfaces:**
- Consumes: `cached_detail`, `LoginCache` (Task 2); `base_texts` (Task 1).
- Produces:
```rust
// checkout.rs. Local git only: rev-parse and merge-base. Never fetch, clone or checkout.
pub async fn warm_checkout(key: &PrKey, inbox_head: &str, root: &Path) -> Option<Checkout>;
    // Some when worktree_path(root,key)/.git exists, `rev-parse HEAD` == inbox_head and
    // `merge-base HEAD refs/photon/pr/<N>/base` succeeds; repo_dir = common git dir; head_changed = false
pub struct NewCommits { pub fetched: FetchedCheckout, pub count: usize }
pub async fn check_new_commits(checkout: &Checkout, key: &PrKey, base_ref: &str, fetch_url: &str) -> Result<Option<NewCommits>>;
    // fetch_checkout(.., Some(checkout.repo_dir), root-of(checkout.worktree)); None when fetched.head_sha == checkout.head_sha;
    // count = `rev-list --count <old>..<new>` in repo_dir, minimum 1 (a force-push with no new-only commits still counts 1)
// review.rs. Builds a PreparedReview with no network: login from the cache (no gh call when cached), detail from cached_detail.
pub(crate) async fn prepare_warm(summary: &PrSummary, root: PathBuf, logins: LoginCache, database: ReviewDb) -> Option<PreparedReview>;
    // None when warm_checkout is None, the login isn't cached, or no cached detail exists for summary.head_sha
```
- `PreparedReview` gains `pub warm: bool`. On a warm open, the caller (Task 9) starts `check_new_commits` and `refresh_detail` in the background after the first diff.

- [ ] **Step 1: Write failing tests** (GitFixture):
```rust
#[gpui::test] async fn warm_open_runs_no_network()
    // cold prepare_review first; COMMAND_LOG cleared; new FakeGh with no responses
    // prepare_warm(...) is Some, warm == true, checkout.head_sha == fixture.pr_sha, files == cold files;
    // fake.requests().is_empty(); no COMMAND_LOG entry contains "fetch", "clone" or "checkout"
#[gpui::test] async fn stale_inbox_head_is_not_warm()     // summary.head_sha = "0"*40 -> prepare_warm is None
#[gpui::test] async fn new_commit_is_found_without_moving_the_worktree()
    // after cold open, push_pr_head -> check_new_commits Some(count 1, fetched.head_sha == new sha); worktree HEAD still old sha
#[gpui::test] async fn force_push_offers_reload()          // amend + force-push -> Some, count == 1
#[gpui::test] async fn unchanged_head_has_no_new_commits() // -> None
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review warm new_commit force_push unchanged_head`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: open a warm PR worktree with no network and check for new commits after`.

### Task 4: Inbox scoped to the window's repo

**Files:**
- Modify: `crates/photon_review/src/checkout.rs` (replace `candidate_repos` with `window_repos`; delete `candidate_repos`, which is Photon code)
- Modify: `crates/photon_review/src/github.rs` (`inbox_request` takes repos)
- Modify: `crates/photon_review/src/inbox.rs`
- Modify: `crates/photon_review/src/run_locally.rs` (clone lookup from the window, like `open_review`)
- Modify: `crates/photon_review/src/review.rs` (`open_review` gets clone dirs from `workspace.project()`)

**Interfaces:**
- Produces:
```rust
// checkout.rs. Read from Repository snapshots (remote_origin_url, remote_upstream_url); synchronous, no git.
// Skips repositories under review_root(). One entry per (work dir, parsed remote).
pub fn window_repos(project: &Project, cx: &App) -> Vec<(PathBuf, RepoId)>;
pub fn clone_dirs_for(repos: &[(PathBuf, RepoId)], repo: &RepoId) -> Vec<PathBuf>; // matching work dirs; empty -> private clone fallback
// github.rs: adds one `repo:{owner}/{repo}` qualifier per repo to each of the three searches
pub fn inbox_request(host: &str, login: &str, tracked_node_ids: &[String], repos: &[RepoId]) -> GhRequest;
// inbox.rs
pub enum InboxState { Idle, Loading, NoRepository, Remote, Loaded(Vec<HostInbox>), Failed(GhError) } // two new variants
pub fn row_meta(summary: &PrSummary, has_drafts: bool, opening: bool) -> RowMeta;      // opening -> primary_action "Opening…"
```
- `InboxPanel::refresh` reads the workspace's project **inside its spawned task**, never synchronously, because it runs inside workspace actions.
  - A non-local project gives `Remote`, with no `gh` call.
  - No repos gives `NoRepository`, with no `gh` call.
  - Otherwise, `auth status` runs, then one `inbox_request` per host that has window repos, with only that host's repos. Other hosts are not queried.
- The rows read `opening` from the `OpeningReviews` global (Task 9 makes it a plain `HashSet<PrKey>` updated through `cx.update_global`). The panel re-renders through `cx.observe_global::<OpeningReviews>`.
- `open_review` and `run_locally` compute `clone_dirs_for(&window_repos(workspace.project().read(cx), cx), &repo_id(&summary))` synchronously. This is safe because it reads the project, not the workspace being updated. Both pass the result to `find_clone`.
- Update the existing inbox tests' `add_panel` helper: give the `FakeFs` project a repo at `/root/.git` with origin `git@github.com:acme/web.git` (`FakeFs::set_remote_for_repo`).

- [ ] **Step 1: Write failing tests:**
```rust
// github.rs
#[test] fn inbox_request_scopes_to_repos()      // stdin query contains "repo:acme/web" three times; two repos -> both qualifiers in each search
// checkout.rs
#[gpui::test] async fn window_repos_reads_origin_and_upstream() // origin fork/web, upstream acme/web -> both RepoIds, same dir
// inbox.rs
#[gpui::test] async fn window_without_github_repo_shows_note()  // no remote -> state NoRepository; fake.requests().is_empty()
#[gpui::test] async fn remote_window_shows_note()               // Project::is_local false (use a remote test project or the variant's guard fn) -> Remote; no requests
#[gpui::test] async fn fork_window_lists_upstream_prs()         // origin fork, upstream acme/web -> inbox stdin contains "repo:acme/web" and "repo:fork/web"
#[gpui::test] async fn refresh_from_inside_a_workspace_update_does_not_panic() // dispatch ToggleInbox via workspace.update_in -> no panic, then Loaded
#[test] fn row_meta_texts()                                     // existing; add opening == true -> "Opening…"
// review.rs (GitFixture)
#[gpui::test] async fn pr_repo_matching_no_remote_uses_private_clone() // clone_dirs empty -> checkout.repo_dir under root/clones
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review inbox window_repos remote fork row_meta private_clone`. Expected: FAIL.
- [ ] **Step 3: Implement.** If `remote_window_shows_note` can't build a remote project cheaply, put the check in `fn inbox_scope(project: &Project, cx: &App) -> Result<Vec<RepoId>, InboxState>` and test that function with `Project::is_local()` stubbed through a `#[cfg(test)]` flag. Say which you chose in the commit body.
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS, including `open_review_from_inside_a_workspace_update_does_not_panic`.
- [ ] **Step 5: Commit** `photon: inbox lists only the window's repo and opens from its clone`.

### Task 5: Stable map, call-flow chip, fixed status strip

**Files:**
- Modify: `crates/photon_review/src/review_map.rs`
- Modify: `crates/photon_review/src/review.rs`

**Interfaces:**
- Consumes: `call_flow_order` and `set_call_flow_order` (Task 2).
- Produces:
```rust
// review_map.rs: one step per non-test file in path order (key = path, title = file name,
// subtitle "{path} · {status_label}", functions = indices of `functions` in that file),
// then one "tests" step (same key/title/subtitle as build_review_map) holding every test file.
pub fn stable_review_map(files: &[MapFile], functions: &[MapFunction]) -> Vec<Step>;
// review.rs
impl ReviewItem {
    pub fn call_flow_order(&self) -> bool;
    pub fn set_call_flow_order(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>); // rebuilds map, keeps selected step when its key survives, persists
}
pub(crate) const STATUS_STRIP_HEIGHT: Pixels = px(28.);
```
- `rebuild_map` uses `stable_review_map` unless `call_flow`, in which case it uses `build_review_map(files, functions, edges)`. In stable mode, `set_analyses` and `set_contexts` only fill in `Step::functions`. They never change keys, titles or order. `map_inputs` uses `classify_path` alone for `kind` in stable mode, so a file's kind cannot change once analysis finishes.
- `ReviewItem::new` reads `database.call_flow_order(&key)`.
- `render_code`'s strip is always rendered at `STATUS_STRIP_HEIGHT`, whatever the `ContextState`. It holds:
  - on the left, the context status (`finding callers…` / the unavailable text and Trust button / empty when Ready);
  - on the right, the chip. The chip shows only when `ContextState::Ready`. It reads `Order by call flow` when off and `Order by path` when on.

  The chip never appears outside the strip.

- [ ] **Step 1: Write failing tests:**
```rust
// review_map.rs
#[test] fn stable_map_is_path_order_with_tests_last() // b.rs, a.rs, docs/x.md, tests/t.rs -> keys ["a.rs","b.rs","docs/x.md","tests"]
#[test] fn stable_map_keys_ignore_functions()         // keys equal with and without functions; functions land on their file's step
// review.rs
#[gpui::test] async fn call_edges_never_reorder_the_map()  // replaces map_reorders_when_edges_arrive: step_keys equal before and after settle()
#[gpui::test] async fn call_flow_chip_reorders_and_is_remembered()
    // settle; set_call_flow_order(true) -> step_keys == build_review_map order from the old test; reopen with same database -> call_flow_order() true
#[gpui::test] async fn status_strip_height_is_fixed()      // layout bounds of the strip equal STATUS_STRIP_HEIGHT in Finding, Ready and Unavailable
```
  Measure the strip's height in `status_strip_height_is_fixed` with `cx.debug_bounds("photon-review-status-strip")`: give the strip `.debug_selector(|| "photon-review-status-strip".into())`.
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review stable_map call_edges call_flow status_strip`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: stable review map with an opt-in call-flow order`.

### Task 6: One context row per hunk, scroll memory, anchored expand

**Files:**
- Modify: `crates/photon_review/src/review.rs` (`show_step`, `label_renderer`, `select_step`, `expand_context`, `set_contexts`)

**Interfaces:**
- Consumes: `stable_review_map` (Task 5).
- Produces:
```rust
pub(crate) type HunkKey = (String, u32);                  // (path, first changed row of the hunk)
pub(crate) fn hunk_context_label(state: &ContextState, callers: usize, not_updated: usize, has_code: bool, expanded: bool) -> String;
struct StepView { scroll: gpui::Point<ScrollOffset>, cursor: Option<(String, Point)> }
impl ReviewItem {
    pub fn toggle_hunk_context(&mut self, hunk: HunkKey, window: &mut Window, cx: &mut Context<Self>);
    pub fn expand_context(&mut self, excerpt_index: usize, window: &mut Window, cx: &mut Context<Self>); // gains `window`
    pub fn select_step(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>);           // now saves/restores StepView
    fn keep_row_on_screen(&mut self, target: (Entity<Buffer>, Point), window: &mut Window, cx: &mut Context<Self>,
        change: impl FnOnce(&mut Self, &mut Context<Self>));
    fn refresh_hunk_rows(&mut self, cx: &mut Context<Self>); // editor.replace_blocks only; never insert/remove
}
```
- **Hunk rows:**
  - `show_step` inserts one `BlockPlacement::Above(hunk start)` block with `height: Some(1)` for each hunk of a file whose `classify_path` is `Code`. Rows exist from the first draw, before any analysis.
  - A hunk's callers are the callers of every changed function overlapping it, deduplicated. "Not updated" uses `classify_caller(.., caller_touched(..)) == CallerMark::NotUpdated`, as `update_impact` does.
  - `has_code` means `context_excerpts` returns anything for those functions.
- **Context excerpts** are added only for hunks in `expanded_hunks: HashSet<HunkKey>`. The existing 8-line collapse and `expand_context` stay.
- `set_contexts` and `set_analyses` call `refresh_hunk_rows`, plus `show_step` only in call-flow mode. They never insert or remove context excerpts.
- **`keep_row_on_screen`:** record `(display row of target) - scroll_position.y`, run `change`, then set `scroll_position.y` so that offset is the same. Get the display row with `editor.display_snapshot(cx)` and `editor::display_map::ToDisplayPoint`. Both toggles run through it.
- **`select_step`:**
  1. Store the current step's `StepView` under its key in `step_views: HashMap<String, StepView>`.
  2. Run `show_step`.
  3. Restore the new step's saved view with `set_scroll_position`, and move the cursor to the saved `(path, point)` when that buffer is still shown. Without a saved view, scroll to `(0., 0.)`.
- **Tests to update:** `workspace_shows_changed_hunk_then_unchanged_callee`, `long_unchanged_excerpt_collapses_and_expands` and `comment_on_unchanged_excerpt_creates_draft` now call `toggle_hunk_context` before they read callee text. `diff_shows_before_callers_arrive` asserts that the row reads `callers: loading…`.

- [ ] **Step 1: Write failing tests:**
```rust
#[test] fn hunk_context_label_copy()
    // Finding -> "callers: loading…"; Unavailable -> "callers: unavailable"; Ready 0,0,false -> "no callers";
    // Ready 3,1,true,false -> "3 callers · 1 not updated ▸"; Ready 1,0,true,true -> "1 caller ▾"
#[gpui::test] async fn context_never_inserts_lines_until_clicked()
    // gate closed: editor text T0; release gate, settle -> text == T0 and hunk row label "1 caller ▸"; toggle -> text contains callee body
#[gpui::test] async fn expanding_keeps_the_clicked_row_on_screen()
    // scroll so the hunk row is 5 rows below the top; toggle_hunk_context -> hunk display row - scroll.y still 5
#[gpui::test] async fn step_scroll_and_cursor_are_restored()
    // two steps; in step 0 scroll to y=7 and put cursor on row 9; select 1; select 0 -> scroll.y == 7, cursor row 9
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review hunk_context context_never expanding_keeps step_scroll`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: one context row per hunk that expands in place`.

### Task 7: Comment cards: re-anchor first, then draw once

**Files:**
- Modify: `crates/photon_review/src/review.rs` (`reanchor_drafts`, `apply_reanchor`, `show_comments`, single-card callers)

**Interfaces:**
- Produces:
```rust
#[derive(Clone, PartialEq, Eq, Hash)] enum Card { Draft(usize /* local_id */), Thread(String /* thread id */) }
impl ReviewItem {
    fn reanchor_in_memory(&mut self, path: &str);             // synchronous; drafts whose file text is loaded (both sides); no drawing
    fn reanchor_drafts(&self, cx: &mut Context<Self>) -> Task<()>; // only drafts whose file left the PR; applies all results in ONE update, then one show_comments
    fn show_card(&mut self, card: &Card, cx: &mut Context<Self>);  // insert, replace_blocks (same height) or resize_blocks+replace, or remove: that card only
}
```
- `comment_blocks` becomes `card_blocks: HashMap<Card, (CustomBlockId, u32 /* height */)>`. `show_comments` (the full redraw) runs only from `show_step`, from the batched `reanchor_drafts` result, and from `refresh_detail`.
- `ReviewItem::new` calls `reanchor_in_memory` for every loaded file before the first `show_step`. Task 9's `file_loaded` does the same for its own file. A draft card is therefore placed already re-anchored the first time it is drawn.
- The callers that change one draft or thread switch from `show_comments` to `show_card`:
  - `open_composer`, `collapse_draft`, `discard_draft`, `cancel_draft`, `replace_draft`, `include_again`, `add_comment`;
  - `set_thread_resolved` and `reply` (for their thread);
  - the submit marks (each changed draft).

- [ ] **Step 1: Write failing tests:**
```rust
#[gpui::test] async fn stored_drafts_are_drawn_once_already_reanchored()
    // stored draft at an old commit in a.rs; count editor block inserts via a test-only counter (cfg(test) static) -> exactly one show_comments;
    // the card's anchor row == reanchored line - 1 at first draw
#[gpui::test] async fn editing_one_draft_keeps_other_cards()
    // two drafts; record card_blocks ids; edit_draft(first) then collapse -> second's CustomBlockId unchanged
#[gpui::test] async fn reanchored_draft_saved_with_new_commit() // existing; still passes
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review drafts_are_drawn editing_one_draft reanchored_draft`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS (all draft, submit and reply tests).
- [ ] **Step 5: Commit** `photon: draw review cards once, after re-anchoring`.

### Task 8: Since review keeps the map; impact rows reserved

**Files:**
- Modify: `crates/photon_review/src/review.rs` (`toggle_changes_since_review`, `set_files`, `render_impact`)

**Interfaces:**
- Produces:
```rust
// `files` always holds the full PR; `full_files` is replaced by:
since_review_files: Option<Vec<LoadedFile>>,
fn shown_files(&self) -> &[LoadedFile];   // since_review_files or files; used by show_step, step_rows, commentable, changed_rows
fn swap_diff_files(&mut self, since: Option<Vec<LoadedFile>>, cx: &mut Context<Self>); // replaces set_files
const IMPACT_LIST_ROWS: usize = 6;
```
- `swap_diff_files` recomputes `changed_rows`, `commentable` and the since-review LSP handles, then calls `show_step`. It keeps `steps`, `selected`, `analyses`, `functions`, `contexts`, `expanded_hunks` and `step_views`. A step with no since-review changes shows an empty editor, and the strip reads `No changes in this step since your review`.
- The impact list is a fixed-height `v_flex` of `IMPACT_LIST_ROWS` line heights with `overflow_y_scroll`. While `ContextState::Finding` and a function is under the cursor, its first row reads `finding callers…`. The legend and the "Your review" section never move.

- [ ] **Step 1: Write failing tests:**
```rust
#[gpui::test] async fn since_review_keeps_map_selection_and_context()
    // select step 1, settle contexts; toggle since review -> step_keys, selected_step() equal; contexts non-empty; toggle back -> same
#[gpui::test] async fn impact_section_has_fixed_height()
    // debug_bounds("photon-review-impact-list") height equal with 0 rows, Finding, and 3 caller rows
#[gpui::test] async fn since_review_mode_blocks_base_comments_and_viewed() // existing; still passes
#[gpui::test] async fn second_click_while_loading_cancels()               // existing; still passes
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review since_review impact_section second_click_while_loading`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: since-review swaps only the diff; impact rows fill reserved space`.

### Task 9: Instant tab with stages, errors and streaming files

**Files:**
- Create: `crates/photon_review/src/review_tab.rs` (new logical component; add `mod review_tab; pub use review_tab::*;` in `photon_review.rs`)
- Modify: `crates/photon_review/src/review.rs` (streaming constructor, `file_loaded`, `OpeningReviews`)

**Interfaces:**
- Consumes: `fetch_review`, `finish_review` (Task 2); `prepare_warm`, `check_new_commits` (Task 3); `reanchor_in_memory` (Task 7); `stable_review_map`, `STATUS_STRIP_HEIGHT` (Task 5).
- Produces:
```rust
#[derive(Clone, Debug, PartialEq)]
pub enum OpenStage { CheckingLogin, Fetching, PreparingWorktree, LoadingFiles { total: usize }, Ready, Failed(SharedString) }
impl OpenStage { pub fn label(&self) -> Option<SharedString>; } // the pinned copy; None for Ready/Failed
pub struct ReviewTab { /* summary, stage, review: Option<Entity<ReviewItem>>, project, clone_dirs, worktree_id: Option<WorktreeId>, focus_handle, _open_task */ }
impl ReviewTab {
    pub fn new(summary: PrSummary, project: Entity<Project>, clone_dirs: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self; // starts loading
    pub fn key(&self) -> &PrKey;
    pub fn stage(&self) -> &OpenStage;
    pub fn review(&self) -> Option<&Entity<ReviewItem>>;
    pub fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>);   // only from Failed; reruns the pipeline once
}
impl Item for ReviewTab { /* tab_content_text "#{n} {title}", tab_icon PullRequest */ }
// review.rs
#[derive(Default)] pub(crate) struct OpeningReviews(pub HashSet<PrKey>); // plain set; replaces the Arc<Mutex> + OpeningGuard
impl ReviewItem {
    pub(crate) fn new_streaming(project: Entity<Project>, detail: PrDetail, login: String, checkout: Checkout,
        diffs: Vec<FileDiff>, database: ReviewDb, window: &mut Window, cx: &mut Context<Self>) -> Self;
    pub(crate) fn file_loaded(&mut self, file: LoadedFile, cx: &mut Context<Self>);
}
```
- **Pipeline** (`_open_task`, spawned in `new` and `retry`):
  1. Try `prepare_warm`. Otherwise run `fetch_review` (stages `CheckingLogin` → `Fetching`), then `forget_stale_trust` before the worktree is shown, then `finish_review` (`PreparingWorktree`).
  2. `find_or_create_worktree(worktree, true)` on the tab's project, storing `worktree_id`. Task 10 makes this the Review window's project.
  3. `ReviewItem::new_streaming` with the diffs (`LoadingFiles { total }`). The map shows every file at once.
  4. `build_loaded_file` in `stable_review_map` order through `futures::stream::iter(..).buffered(8)`, calling `file_loaded` for each.
  5. On a warm open, after the first `file_loaded`, run `check_new_commits` and `refresh_detail` in the background. Task 11 uses the result.
- **Errors:** any error sets `Failed(format!("{error:#}"))`. The tab body shows that message (`GhError`'s Display already says what to do) and a `Retry` button. No toast.
- **`new_streaming`:**
  - Each file starts as `LoadedFile { diff, contents: None }`, and opaque files are never pending.
  - `pending_files: HashSet<String>` holds the rest.
  - The item strip reads `Loading files… {loaded} of {total}` while `pending_files` is non-empty.
  - `file_loaded` swaps the entry in and updates `changed_rows`, `commentable` and the LSP handles. It calls `reanchor_in_memory(path)`, and calls `show_step` only when the path is in the selected step.
  - When `pending_files` empties, it starts `load_context` and the background `reanchor_drafts`.
  - `ReviewItem::new` stays as it is for existing tests: all files are loaded and nothing is pending.
- **`OpeningReviews`:** the tab inserts its key in `new` and `retry`, and removes it on `Ready` or `Failed`. Both go through `cx.update_global`.
- **Layout:** before a `ReviewItem` exists, the tab renders a header line (`#{n} {title}`, `{author} → {base_ref}`) and a progress line of height `STATUS_STRIP_HEIGHT`, so the swap to the item's header and strip doesn't move them.

- [ ] **Step 1: Write failing tests** in `review_tab.rs`:
```rust
#[test] fn stage_labels()                 // LoadingFiles{total:1} -> "Loading 1 file…"; {12} -> "Loading 12 files…"; Fetching -> "Fetching commits…"
#[gpui::test] async fn tab_shows_summary_before_any_work()
    // FakeGh with no responses; add ReviewTab to a test workspace; WITHOUT run_until_parked:
    // stage == CheckingLogin, tab_content_text == "#482 <title>", fake.requests().is_empty()
#[gpui::test] async fn failure_shows_in_the_tab_with_retry()
    // logged-out auth -> run_until_parked -> stage Failed(text contains "Not logged in"); no toast in workspace.notification_ids();
    // respond auth ok + detail; retry -> stage advances past CheckingLogin; OpeningReviews no longer has the key at the end
#[gpui::test] async fn files_stream_in_path_order()
    // GitFixture with 3 files; record the order of file_loaded paths via a cfg(test) log -> stable map order; map steps exist before the first file_loaded
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review review_tab stage_labels`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: review tab appears on click and streams its files`.

### Task 10: One Review window, one tab per PR

**Files:**
- Modify: `crates/photon_review/src/review_tab.rs` (window lookup, `open_review` moves here)
- Modify: `crates/photon_review/src/review.rs` (delete `open_in_new_window`, `place_review`, `find_open_review`, `OpeningGuard`; `worktree_restricted` checks only this PR's worktree)

**Interfaces:**
- Consumes: `ReviewTab` (Task 9), `window_repos` and `clone_dirs_for` (Task 4).
- Produces:
```rust
#[derive(Default)] struct ReviewWindow { window: Option<WindowHandle<MultiWorkspace>>,
    creating: Option<Shared<Task<Result<WindowHandle<MultiWorkspace>, Arc<anyhow::Error>>>>> } // Global
pub(crate) fn review_window(app_state: Arc<AppState>, cx: &mut App) -> Shared<Task<Result<WindowHandle<MultiWorkspace>, Arc<anyhow::Error>>>>;
    // 1. remembered handle whose window is still open; 2. a window whose project has ≥1 worktree, all under review_root()
    // (a restored session window); 3. else Workspace::new_local(Vec::new(), app_state, None, None, None, OpenMode::NewWindow, cx).
    // Concurrent callers share `creating`, so two quick clicks make one window.
#[cfg(test)] pub(crate) fn remember_review_window(window: WindowHandle<MultiWorkspace>, cx: &mut App);
pub fn open_review(summary: PrSummary, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) -> Task<Result<()>>; // same signature
```
- **`open_review`:**
  1. Compute `clone_dirs` synchronously (Task 4).
  2. `cx.defer` the rest, so the clicked workspace's update has ended before another window is touched. This is also correct when the click came from the Review window itself.
  3. Await `review_window`, then in one `window.update` on the Review window: find a `ReviewTab` with the key among `workspace.items_of_type::<ReviewTab>(cx)`. If one exists, `activate_item` it. If it is `Ready` with a head different from `summary.head_sha`, start the Task 11 new-commit check. Otherwise `add_item_to_active_pane` a new `ReviewTab`.
  4. `window.activate_window()` either way.
  
  The returned task errors, and the clicked window shows a toast, only when the Review window itself can't be created.
- `ReviewTab::on_removed` calls `project.remove_worktree(worktree_id)`, which stops that worktree's language servers.
- `worktree_restricted` checks only the worktree whose `abs_path()` equals `checkout.worktree`.
- **Tests to delete:** `reopening_after_new_head_replaces_the_review`. Task 11 replaces it with `reload_moves_the_worktree_in_place`.

- [ ] **Step 1: Write failing tests:**
```rust
#[gpui::test] async fn prs_open_as_tabs_in_one_review_window()
    // remember_review_window(test window); open two PRs from a separate user window ->
    // review window has 2 ReviewTabs; cx.windows().len() unchanged; user window has 0 ReviewTabs
#[gpui::test] async fn second_click_while_opening_focuses_the_same_tab()
    // two open_review calls in one frame -> 1 ReviewTab, it is the active item, fake.requests() has one auth request
#[gpui::test] async fn pr_worktrees_are_visible()
    // after open: the PR worktree is in project.visible_worktrees(); still untrusted (worktree_trusted false) under init_trust("/")
#[gpui::test] async fn closing_a_pr_tab_removes_its_worktree()   // close item -> project.worktrees() no longer has it; other PR's remains
#[gpui::test] async fn untrusted_pr_does_not_block_a_trusted_one()
    // project with two PR worktrees under trust enforcement; trust A -> A's item context_state != Unavailable(TRUST_REASON); B's still is
#[gpui::test] async fn open_review_from_inside_a_workspace_update_does_not_panic() // existing; now asserts the tab shows Failed("Not logged in…")
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review review_window prs_open second_click pr_worktrees closing untrusted_pr open_review_from_inside`. Expected: FAIL.
- [ ] **Step 3: Implement.** Set up `Workspace::new_local` in tests the way `crates/zed/src/zed.rs` (around line 7437) does, or use `remember_review_window` with `MultiWorkspace::test_new`.
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: open every PR as a tab in one Review window`.

### Task 11: Reload in place

**Files:**
- Modify: `crates/photon_review/src/review_tab.rs`
- Modify: `crates/photon_review/src/review.rs` (chip in the strip, event)
- Modify: `crates/photon_review/src/run_locally.rs` (replace `refuse_head_change_under_open_window`'s toast)

**Interfaces:**
- Consumes: `NewCommits`, `check_new_commits` (Task 3); `StepView` and `step_views` (Task 6).
- Produces:
```rust
pub enum ReviewItemEvent { Reload }                        // impl EventEmitter<ReviewItemEvent> for ReviewItem
pub(crate) struct ViewState { selected_key: Option<String>, step_views: HashMap<String, StepView>, call_flow: bool }
impl ReviewItem {
    pub(crate) fn set_new_commits(&mut self, count: Option<usize>, cx: &mut Context<Self>); // chip "{n} new commit(s) · Reload" in the strip
    pub(crate) fn view_state(&self) -> ViewState;
    pub(crate) fn restore_view_state(&mut self, state: ViewState, window: &mut Window, cx: &mut Context<Self>); // keys that still exist only
}
impl ReviewTab {
    pub(crate) fn offer_reload(&mut self, new_commits: NewCommits, cx: &mut Context<Self>);
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>);
}
// run_locally.rs (replaces refuse_head_change_under_open_window)
pub(crate) async fn offer_reload_instead(key: &PrKey, fetched: &FetchedCheckout, cx: &mut AsyncWindowContext) -> Result<bool>;
```
- **`reload`:**
  1. Take `view_state()` from the current item.
  2. `remove_worktree`.
  3. `forget_stale_trust` (the checkout's `head_changed` is true).
  4. `finish_checkout(fetched)`, then `track`.
  5. `find_or_create_worktree(.., true)`.
  6. `git_diff` and `base_texts`, then a new `ReviewItem::new_streaming` in the **same** `ReviewTab`, followed by `restore_view_state`. Drafts re-anchor through Task 7.
  
  The old item stays on screen, showing the old head, until the new one has its first file.
- **`offer_reload_instead`:** when a `ReviewTab` for the key is open and the fetched head differs from its checkout head, it activates the Review window and the tab and calls `offer_reload`. It returns `true`, and `run_locally` then stops without moving the worktree. The `Close the window for PR #N…` toast and `NewCommitsToast` are deleted.

- [ ] **Step 1: Write failing tests** (GitFixture, real fs project as in the old replace test):
```rust
#[gpui::test] async fn new_commits_show_a_chip_and_nothing_moves()
    // open; push_pr_head; check_new_commits -> offer_reload -> chip text "1 new commit · Reload"; item head still old; editor text unchanged
#[gpui::test] async fn reload_moves_the_worktree_in_place()
    // select step "a.rs", scroll 3; reload -> same ReviewTab entity; review head == new sha; text contains "    2;";
    // selected step key "a.rs"; scroll.y == 3; worktree_trusted == false
#[gpui::test] async fn run_locally_offers_reload_instead_of_moving_an_open_tab()
    // open tab at old head; push; run_locally -> worktree HEAD still old; tab shows chip; no toast
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review new_commits_show reload_moves run_locally_offers`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: reload new commits in place from a chip`.

### Task 12: Budget tests and gates

**Files:**
- Modify: `crates/photon_review/src/review.rs` (tests), `crates/photon_review/src/review_tab.rs` (tests)

**Interfaces:**
- Consumes: everything above.
- Test helper (in `review.rs` tests):
```rust
// step keys, selected key, display row of every changed-row start in the selected step, editor scroll position
fn layout_fingerprint(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> (Vec<String>, Option<String>, Vec<u32>, gpui::Point<f64>);
```

- [ ] **Step 1: Write the budget tests:**
```rust
#[gpui::test] async fn zero_layout_shift_after_first_diff()
    // login_graph fixture (a caller exists) + a stored draft at an old commit in the selected file; call-hierarchy gate closed.
    // Build the item like open_item_with_detail but take F0 = layout_fingerprint BEFORE the first run_until_parked.
    // run_until_parked (analysis, reanchor) -> == F0; release gate, settle() (call edges, context, retries) -> == F0;
    // refresh_hunk_rows, update_impact -> == F0. Only toggle_hunk_context may change it.
#[gpui::test] async fn tab_is_visible_on_the_frame_after_the_click()
    // review window remembered; open_review inside workspace.update_in; immediately after (no parking) the review window has the ReviewTab
#[gpui::test] async fn switching_pr_tabs_keeps_scroll_step_and_selection()
    // two Ready tabs; in A select step 1, scroll 4, select text; activate B then A -> same step, scroll, selection
#[gpui::test] async fn warm_open_runs_no_network()          // from Task 3; keep as the <1 s proxy
```
- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review zero_layout_shift tab_is_visible switching_pr_tabs warm_open`. Expected: PASS. A failure here is a bug in the task that owns that behaviour. Fix it there and do not loosen the test.
- [ ] **Step 3: Run the gates:**
  - `CARGO_INCREMENTAL=0 cargo nextest run -p photon_review`;
  - `CARGO_INCREMENTAL=0 ./script/clippy -p photon_review`;
  - `cargo fmt --all -- --check`;
  - `script/photon-netcheck`: clean, so idle startup makes no `gh` or git network call;
  - `git grep -n "photon:" -- ':!crates/photon_*' ':!docs'`: no new Zed hooks.
- [ ] **Step 4: Commit** `photon: SP4a.1 budget tests`.
- [ ] **Step 5: Manual walkthrough** on a real PR, recorded in the PR description with no code changes:
  - the first click opens the Review window;
  - a second PR adds a tab;
  - a warm reopen shows its first diff in under 1 s (stopwatch);
  - nothing moves while callers load;
  - Since review keeps the map;
  - push to the PR and Reload in place.
