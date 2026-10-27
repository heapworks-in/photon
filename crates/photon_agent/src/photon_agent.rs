//! Photon's additions to the agent panel: which files each thread is about (for the threads
//! sidebar's This File filter), and what each agent turn changed (chips and Revert Turn).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use acp_thread::{AcpThread, AcpThreadEvent, AgentThreadEntry, MentionUri, UserMessage};
use agent_client_protocol::schema::v2 as acp_v2;
use db::kvp::KeyValueStore;
use file_icons::FileIcons;
use git_ui_core::file_diff_view::{FileDiffView, build_buffer_diff};
use gpui::{AnyElement, App, Entity, Global, Task, WeakEntity};
use project::Project;
use project::git_store::{
    GitStore, GitStoreCheckpoint, LocalRepositoryState, Repository, RepositoryState,
};
use ui::{Button, IconButton, IconName, IconSize, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::notifications::NotifyTaskExt as _;

mod edit_requests;
pub use edit_requests::{
    EditRequestSender, QueueEditRequest, queue_edit_request, set_edit_request_sender,
};

const THREAD_FILES_KEY: &str = "photon_agent_thread_files";

/// Session id → files its prompts mentioned or its turns changed. Read from the database the first
/// time it's used, written back whole when it changes.
#[derive(Default)]
struct ThreadFiles {
    files: Option<HashMap<String, BTreeSet<PathBuf>>>,
    _save: Option<Task<()>>,
}

impl Global for ThreadFiles {}

fn thread_files(cx: &mut App) -> &mut HashMap<String, BTreeSet<PathBuf>> {
    if cx.default_global::<ThreadFiles>().files.is_none() {
        let loaded = KeyValueStore::global(cx)
            .read_kvp(THREAD_FILES_KEY)
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).log_err())
            .unwrap_or_default();
        cx.global_mut::<ThreadFiles>().files = Some(loaded);
    }
    cx.global_mut::<ThreadFiles>().files.get_or_insert_default()
}

/// Remembers that the thread is about these files.
pub fn record_thread_files(
    session_id: &acp_v2::SessionId,
    paths: impl IntoIterator<Item = PathBuf>,
    cx: &mut App,
) {
    let files = thread_files(cx).entry(session_id.to_string()).or_default();
    let count = files.len();
    files.extend(paths);
    if files.len() == count {
        return;
    }
    let Some(json) = serde_json::to_string(&cx.global::<ThreadFiles>().files).log_err() else {
        return;
    };
    let store = KeyValueStore::global(cx);
    let save = cx.background_spawn(async move {
        store
            .write_kvp(THREAD_FILES_KEY.to_string(), json)
            .await
            .log_err();
    });
    cx.global_mut::<ThreadFiles>()._save = Some(save);
}

/// Reads the stored thread files, so [`thread_is_about`] can answer. Call before filtering.
pub fn load_thread_files(cx: &mut App) {
    thread_files(cx);
}

/// Whether the thread mentioned or changed `path`, or mentioned a folder holding it.
pub fn thread_is_about(session_id: &acp_v2::SessionId, path: &Path, cx: &App) -> bool {
    cx.try_global::<ThreadFiles>()
        .and_then(|thread_files| thread_files.files.as_ref()?.get(&*session_id.0))
        .is_some_and(|files| files.iter().any(|file| path.starts_with(file)))
}

/// Records the files the latest prompt mentioned, and works out which files the turn changed.
/// Called when a root thread's turn stops.
pub fn turn_stopped(thread_entity: &Entity<AcpThread>, cx: &mut App) {
    let thread = thread_entity.read(cx);
    let path_style = thread.project().read(cx).path_style(cx);
    let Some((prompt_index, prompt)) =
        thread
            .entries()
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, entry)| match entry {
                AgentThreadEntry::UserMessage(message) => Some((index, message)),
                _ => None,
            })
    else {
        return;
    };
    let before_turn = prompt
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.git_checkpoint().clone());
    let paths = prompt
        .content
        .source_blocks()
        .iter()
        .filter_map(|block| match block {
            acp_v2::ContentBlock::ResourceLink(link) => Some(link.uri.as_str()),
            acp_v2::ContentBlock::Resource(resource) => match &resource.resource {
                acp_v2::EmbeddedResourceResource::TextResourceContents(text) => {
                    Some(text.uri.as_str())
                }
                acp_v2::EmbeddedResourceResource::BlobResourceContents(blob) => {
                    Some(blob.uri.as_str())
                }
                _ => None,
            },
            _ => None,
        })
        .filter_map(|uri| MentionUri::parse(uri, path_style).ok())
        .filter_map(|mention| mention.abs_path().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    let session_id = thread.session_id().clone();
    record_thread_files(&session_id, paths, cx);
    if let Some(before_turn) = before_turn {
        review_turn(thread_entity, prompt_index, before_turn, cx);
    }
}

/// A file an agent turn changed, in the repository whose work directory is `work_directory`.
struct TurnFile {
    work_directory: Arc<Path>,
    repo_path: String,
    abs_path: PathBuf,
}

/// Snapshots from before and after a turn, and the files that differ between them.
struct TurnChanges {
    before: GitStoreCheckpoint,
    after: GitStoreCheckpoint,
    files: Vec<TurnFile>,
}

/// Turns keyed by their snapshot from before the turn, which the thread keeps on the prompt.
/// `None` while the turn's changes are being worked out.
#[derive(Default)]
struct TurnReviews(HashMap<Vec<String>, Option<Rc<TurnChanges>>>);

impl Global for TurnReviews {}

fn turn_key(before_turn: &GitStoreCheckpoint) -> Vec<String> {
    let mut key = before_turn
        .checkpoints_by_work_dir_abs_path()
        .values()
        .map(|checkpoint| checkpoint.commit_sha.to_string())
        .collect::<Vec<_>>();
    key.sort();
    key
}

fn turn_changes(before_turn: &GitStoreCheckpoint, cx: &App) -> Option<Rc<TurnChanges>> {
    cx.try_global::<TurnReviews>()?
        .0
        .get(&turn_key(before_turn))?
        .clone()
}

fn repository_for(
    git_store: &Entity<GitStore>,
    work_directory: &Path,
    cx: &App,
) -> Option<Entity<Repository>> {
    git_store
        .read(cx)
        .repositories()
        .values()
        .find(|repository| &*repository.read(cx).work_directory_abs_path == work_directory)
        .cloned()
}

/// Paths in a `git diff` patch, relative to the repository. Renames give the new path.
fn changed_paths(patch: &str) -> Vec<String> {
    // ponytail: paths git quotes (unusual characters) come out quoted; parse them if they show up.
    patch
        .lines()
        .filter_map(|line| line.strip_prefix("diff --git "))
        .filter_map(|header| match header.rsplit_once(" b/") {
            Some((_, path)) => Some(path),
            None => header.strip_suffix(" /dev/null")?.strip_prefix("a/"),
        })
        .map(str::to_string)
        .collect()
}

/// Takes a snapshot after the turn and asks git which files differ from the one before it.
fn review_turn(
    thread: &Entity<AcpThread>,
    prompt_index: usize,
    before_turn: GitStoreCheckpoint,
    cx: &mut App,
) {
    let key = turn_key(&before_turn);
    let reviews = &mut cx.default_global::<TurnReviews>().0;
    if key.is_empty() || reviews.contains_key(&key) {
        return;
    }
    reviews.insert(key.clone(), None);

    let session_id = thread.read(cx).session_id().clone();
    let git_store = thread.read(cx).project().read(cx).git_store().clone();
    let after_turn = git_store.update(cx, |git_store, cx| git_store.checkpoint(cx));
    let thread = thread.downgrade();
    cx.spawn(async move |cx| {
        let changes = async {
            let after = after_turn.await?;
            let mut files = Vec::new();
            for (work_directory, before_checkpoint) in
                before_turn.checkpoints_by_work_dir_abs_path()
            {
                let Some(after_checkpoint) =
                    after.checkpoints_by_work_dir_abs_path().get(work_directory)
                else {
                    continue;
                };
                let Some(repository) =
                    cx.update(|cx| repository_for(&git_store, work_directory, cx))
                else {
                    continue;
                };
                let patch = repository
                    .update(cx, |repository, _| {
                        repository
                            .diff_checkpoints(before_checkpoint.clone(), after_checkpoint.clone())
                    })
                    .await??;
                files.extend(changed_paths(&patch).into_iter().map(|repo_path| TurnFile {
                    abs_path: work_directory.join(&repo_path),
                    work_directory: work_directory.clone(),
                    repo_path,
                }));
            }
            anyhow::Ok(TurnChanges {
                before: before_turn,
                after,
                files,
            })
        }
        .await;
        cx.update(|cx| match changes {
            Ok(changes) => {
                let changed_files = changes
                    .files
                    .iter()
                    .map(|file| file.abs_path.clone())
                    .collect::<Vec<_>>();
                cx.default_global::<TurnReviews>()
                    .0
                    .insert(key, Some(Rc::new(changes)));
                record_thread_files(&session_id, changed_files, cx);
                thread
                    .update(cx, |_, cx| {
                        cx.emit(AcpThreadEvent::EntryUpdated(prompt_index))
                    })
                    .ok();
            }
            Err(error) => {
                log::error!("Couldn't work out what an agent turn changed: {error:#}");
                cx.default_global::<TurnReviews>().0.remove(&key);
            }
        });
    })
    .detach();
}

/// The changed-file chips and Revert Turn under a prompt whose turn changed files.
pub fn render_turn_review(
    thread: &Entity<AcpThread>,
    prompt: &UserMessage,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> Option<AnyElement> {
    let checkpoint = prompt.checkpoint.as_ref()?;
    let changes = turn_changes(checkpoint.git_checkpoint(), cx)?;
    if changes.files.is_empty() {
        return None;
    }
    // Agents that can rewind already offer Restore Checkpoint above the prompt.
    let can_rewind = thread.read(cx).can_rewind_to(prompt.client_id.as_ref(), cx);
    let project = thread.read(cx).project().clone();

    let chips = changes.files.iter().enumerate().map(|(index, file)| {
        let name = file
            .abs_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.repo_path.clone());
        let icon = FileIcons::get_icon(&file.abs_path, cx)
            .map(Icon::from_path)
            .unwrap_or_else(|| Icon::new(IconName::File));
        let changes = changes.clone();
        let project = project.clone();
        let workspace = workspace.clone();
        div()
            .debug_selector({
                let name = name.clone();
                move || format!("photon-turn-file-{name}")
            })
            .child(
                Button::new(("photon-turn-file", index), name)
                    .start_icon(icon.size(IconSize::XSmall).color(Color::Muted))
                    .label_size(LabelSize::XSmall)
                    .tooltip(Tooltip::text(format!(
                        "Show this turn's changes to {}",
                        file.repo_path
                    )))
                    .on_click(move |_, window, cx| {
                        open_turn_diff(&changes, index, &project, workspace.clone(), window, cx)
                    }),
            )
    });

    Some(
        h_flex()
            .px_3()
            .gap_1()
            .flex_wrap()
            .children(chips)
            .when(!can_rewind, |row| {
                let git_store = project.read(cx).git_store().clone();
                let before = changes.before.clone();
                row.child(
                    div().debug_selector(|| "photon-revert-turn".into()).child(
                        Button::new("photon-revert-turn", "Revert Turn")
                            .start_icon(
                                Icon::new(IconName::Undo)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .label_size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .tooltip(Tooltip::text(
                                "Puts every file back the way it was before this turn.",
                            ))
                            .on_click(move |_, window, cx| {
                                git_store
                                    .update(cx, |git_store, cx| {
                                        git_store.restore_checkpoint(before.clone(), cx)
                                    })
                                    .detach_and_notify_err(workspace.clone(), window, cx);
                            }),
                    ),
                )
            })
            .into_any_element(),
    )
}

/// Opens one file's changes in a turn: its text in the snapshots from before and after the turn.
fn open_turn_diff(
    changes: &Rc<TurnChanges>,
    file_index: usize,
    project: &Entity<Project>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(file) = changes.files.get(file_index) else {
        return;
    };
    let git_store = project.read(cx).git_store().clone();
    let revisions = [&changes.before, &changes.after]
        .map(|checkpoint| {
            checkpoint
                .checkpoints_by_work_dir_abs_path()
                .get(&file.work_directory)
                .map(|checkpoint| format!("{}:{}", checkpoint.commit_sha, file.repo_path))
        })
        .into_iter()
        .collect::<Option<Vec<_>>>();
    let (Some(repository), Some(revisions)) = (
        repository_for(&git_store, &file.work_directory, cx),
        revisions,
    ) else {
        return;
    };
    let texts = repository.update(cx, |repository, _| {
        repository.send_job("photon turn diff", None, move |state, _| async move {
            match state {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.load_revisions(revisions).await
                }
                RepositoryState::Remote(_) => {
                    anyhow::bail!("Turn changes can only be shown for a local repository.")
                }
            }
        })
    });
    let title: SharedString = format!(
        "{} (this turn)",
        file.abs_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.repo_path.clone())
    )
    .into();
    let abs_path = file.abs_path.clone();
    let project = project.clone();
    let languages = project.read(cx).languages().clone();
    window
        .spawn(cx, {
            let workspace = workspace.clone();
            async move |cx| {
                let mut texts = texts.await??.into_iter().map(|text| {
                    text.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_default()
                });
                let before = texts.next().unwrap_or_default();
                let after = texts.next().unwrap_or_default();
                let language = languages.load_language_for_file_path(&abs_path).await.ok();
                let [old_buffer, new_buffer] = [before, after].map(|text| {
                    project.update(cx, |project, cx| {
                        let buffer =
                            project.create_local_buffer(&text, language.clone(), false, cx);
                        buffer.update(cx, |buffer, cx| {
                            buffer.set_capability(language::Capability::ReadOnly, cx)
                        });
                        buffer
                    })
                });
                let diff = build_buffer_diff(&old_buffer, &new_buffer, cx).await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    let workspace_entity = cx.entity();
                    let view = cx.new(|cx| {
                        let mut view = FileDiffView::new(
                            old_buffer,
                            new_buffer,
                            diff,
                            project,
                            workspace_entity,
                            window,
                            cx,
                        );
                        view.set_title(title, cx);
                        view
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
            }
        })
        .detach_and_notify_err(workspace, window, cx);
}

/// The file open in the workspace's active editor tab.
pub fn active_file(workspace: &Entity<Workspace>, cx: &App) -> Option<PathBuf> {
    let workspace = workspace.read(cx);
    let project_path = workspace.active_item(cx)?.project_path(cx)?;
    workspace
        .project()
        .read(cx)
        .absolute_path(&project_path, cx)
}

/// The threads sidebar's This File toggle.
pub fn this_file_filter_button(
    on: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .debug_selector(|| "photon-this-file-filter".into())
        .child(
            IconButton::new("photon-this-file-filter", IconName::File)
                .icon_size(IconSize::Small)
                .toggle_state(on)
                .tooltip(Tooltip::text(if on {
                    "Show All Threads"
                } else {
                    "Only Threads About This File"
                }))
                .on_click(on_click),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn changed_paths_reads_git_diff_headers() {
        let patch = "\
diff --git a/src/main.rs b/src/main.rs
index 1..2 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1 +1 @@
-old
+new
diff --git a/old name.rs b/new name.rs
similarity index 90%
diff --git a/gone.rs /dev/null
diff --git /dev/null b/added.rs
";
        assert_eq!(
            changed_paths(patch),
            ["src/main.rs", "new name.rs", "gone.rs", "added.rs"]
        );
    }

    #[gpui::test]
    fn thread_files_survive_a_restart(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let session_id = acp_v2::SessionId::new("session");
        cx.update(|cx| {
            record_thread_files(&session_id, [PathBuf::from("/project/src")], cx);
            assert!(thread_is_about(
                &session_id,
                Path::new("/project/src/a.rs"),
                cx
            ));
            assert!(!thread_is_about(
                &session_id,
                Path::new("/project/b.rs"),
                cx
            ));
        });
        cx.run_until_parked();

        cx.update(|cx| {
            cx.set_global(ThreadFiles::default());
            assert!(!thread_is_about(
                &session_id,
                Path::new("/project/src/a.rs"),
                cx
            ));
            load_thread_files(cx);
            assert!(thread_is_about(
                &session_id,
                Path::new("/project/src/a.rs"),
                cx
            ));
        });
    }
}
