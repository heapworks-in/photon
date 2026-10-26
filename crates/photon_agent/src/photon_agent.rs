//! Photon's additions to the agent panel: which files each thread is about, for the threads
//! sidebar's This File filter.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use acp_thread::{AcpThread, AgentThreadEntry, MentionUri};
use agent_client_protocol::schema::v2 as acp_v2;
use db::kvp::KeyValueStore;
use gpui::{App, Entity, Global, Task};
use ui::{IconButton, IconName, IconSize, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

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

/// Records the files the latest prompt mentioned. Called when a root thread's turn stops.
pub fn turn_stopped(thread: &Entity<AcpThread>, cx: &mut App) {
    let thread = thread.read(cx);
    let path_style = thread.project().read(cx).path_style(cx);
    let Some(prompt) = thread.entries().iter().rev().find_map(|entry| match entry {
        AgentThreadEntry::UserMessage(message) => Some(message),
        _ => None,
    }) else {
        return;
    };
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
