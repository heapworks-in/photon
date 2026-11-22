//! "Diagnose Code Navigation": asks the language server behind the file at the cursor what it sees
//! there, and opens a report that names the root cause, to read or to share. Nothing is guessed:
//! every line of the report is an answer from the server or from Photon.

use std::{sync::Arc, time::Duration};

use editor::Editor;
use gpui::{AppContext as _, AsyncWindowContext, Context, Entity, TaskExt as _, Window};
use language::{Buffer, ToPoint as _};
use project::Project;
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::{
    DiagnoseNavigation, LIST_SOURCE_FOLDERS, build_error_summary, build_files_with_errors,
    first_build_error, modules_without, parse_source_folders, server_offering,
};

/// A busy server (one still loading a large project) gets this long per question.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Probe<T> {
    Answered(T),
    Failed(String),
    TimedOut,
}

impl<T> Probe<T> {
    fn from_connection<R>(result: util::ConnectionResult<R>, answer: impl FnOnce(R) -> T) -> Self {
        match result {
            util::ConnectionResult::Result(Ok(response)) => Probe::Answered(answer(response)),
            util::ConnectionResult::Result(Err(error)) => Probe::Failed(format!("{error:#}")),
            util::ConnectionResult::ConnectionReset => {
                Probe::Failed("the server reset the connection".into())
            }
            util::ConnectionResult::Timeout => Probe::TimedOut,
        }
    }
}

/// The innermost symbol the server reports around the cursor.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SymbolAtCursor {
    pub name: String,
    /// The cursor is on the symbol's name, not just inside its body.
    pub on_name: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ServerFacts {
    pub name: String,
    pub version: Option<String>,
    /// Where the server keeps what it learned about the project (jdtls's `-data`).
    pub workspace_folder: Option<String>,
    pub offers_call_hierarchy: bool,
    /// The names it returns for the cursor.
    pub call_hierarchy: Probe<Vec<String>>,
    pub definitions: Probe<usize>,
    pub hover: Probe<bool>,
    /// How many symbols it sees in the file, and the one around the cursor.
    pub symbols: Probe<(usize, Option<SymbolAtCursor>)>,
    /// The modules it loaded, when it can say.
    pub modules: Option<Vec<String>>,
    /// Whether the file is outside every source folder it loaded, when it can say.
    pub outside_modules: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Facts {
    pub language: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// The position as sent to the server: zero-based line and UTF-16 character.
    pub sent_position: (u32, u32),
    pub word: String,
    pub errors_in_file: usize,
    pub server: Option<ServerFacts>,
    /// What Photon's own call hierarchy request found, as the panel shows it.
    pub photon_found: Probe<usize>,
    /// A build file with an error from the server, and the line that names it.
    pub build_error: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Verdict {
    pub title: String,
    pub why: String,
    pub fix: String,
}

pub(crate) fn verdict(facts: &Facts) -> Verdict {
    let language = &facts.language;
    let share = "If that doesn't fix it, share this report.";
    let Some(server) = &facts.server else {
        return Verdict {
            title: format!("No language server is running for this {language} file"),
            why: "Nothing can answer navigation questions for it.".into(),
            fix: "Open the language server menu in the status bar to start one, or set one up."
                .into(),
        };
    };
    let name = &server.name;
    let fresh_workspace = match &server.workspace_folder {
        Some(folder) => format!(
            "Quit Photon, delete `{folder}` (only {name}'s saved copy of the project), reopen \
             the project and wait for {language} to finish loading. {share}"
        ),
        None => format!("Restart {name} from the language server menu. {share}"),
    };
    if !server.offers_call_hierarchy {
        return Verdict {
            title: format!("{name} doesn't offer call hierarchy"),
            why: "It didn't list call hierarchy among the features it supports.".into(),
            fix: "Use another language server for this language, if one exists.".into(),
        };
    }
    match &server.call_hierarchy {
        Probe::TimedOut => {
            return Verdict {
                title: format!("{name} didn't answer"),
                why: format!(
                    "It gave no answer within {} seconds; it may still be loading the project.",
                    PROBE_TIMEOUT.as_secs()
                ),
                fix: format!(
                    "Wait for the {language} progress in the status bar to finish, then diagnose again. {share}"
                ),
            };
        }
        Probe::Failed(error) => {
            return Verdict {
                title: format!("{name} answered with an error"),
                why: error.clone(),
                fix: fresh_workspace,
            };
        }
        Probe::Answered(names) if !names.is_empty() => {
            return if facts.photon_found == Probe::Answered(0) {
                Verdict {
                    title: "Photon dropped the language server's answer".into(),
                    why: format!(
                        "{name} returned {} for the cursor, but Photon's own request found nothing.",
                        names.join(", ")
                    ),
                    fix: "This is a bug in Photon. Please share this report.".into(),
                }
            } else {
                Verdict {
                    title: "Call hierarchy works here".into(),
                    why: format!("{name} returned {} for the cursor.", names.join(", ")),
                    fix: "Press ⌘K ⌘H again. If it failed before, the server was probably still loading."
                        .into(),
                }
            };
        }
        Probe::Answered(_) => {}
    }
    if let Some((file, summary)) = &facts.build_error {
        return Verdict {
            title: format!("{language} couldn't load this project's build"),
            why: format!("{name} reports an error in {file}: {summary}"),
            fix: format!(
                "Fix the error in {file} (often a dependency or plugin it can't download: check \
                 VPN and repository access), then click Try Again in the Call Hierarchy panel."
            ),
        };
    }
    if server.outside_modules == Some(true) {
        let modules = server
            .modules
            .as_ref()
            .filter(|modules| !modules.is_empty())
            .map_or("none".to_string(), |modules| modules.join(", "));
        return Verdict {
            title: format!("{language} hasn't loaded this file's module"),
            why: format!(
                "{} isn't in the source folders of the modules {name} loaded: {modules}.",
                facts.file
            ),
            fix: format!(
                "Check that the module is part of the build (settings.gradle or the parent \
                 pom.xml), then click Try Again in the Call Hierarchy panel. {}",
                "If it is, a stale workspace can cause this: ".to_string() + &fresh_workspace
            ),
        };
    }
    match &server.symbols {
        Probe::Answered((0, _)) => Verdict {
            title: format!("{name} sees no code in this file"),
            why: format!(
                "It returns no classes or methods for {}, so it isn't treating the file as part \
                 of a project.",
                facts.file
            ),
            fix: fresh_workspace,
        },
        Probe::Answered((_, Some(symbol))) if symbol.on_name => Verdict {
            title: format!(
                "{name} knows `{}` but returns no call hierarchy for it",
                symbol.name
            ),
            why: format!(
                "The cursor is on `{}`'s name, which the server reports as a symbol, yet it \
                 answers the call hierarchy question with nothing.",
                symbol.name
            ),
            fix: fresh_workspace,
        },
        Probe::Answered((_, Some(symbol))) => Verdict {
            title: "The cursor isn't on a function's name".into(),
            why: format!(
                "It's inside `{}`, on `{}`. Call hierarchy starts from a function's name.",
                symbol.name, facts.word
            ),
            fix: format!(
                "Put the cursor on `{}`'s name and press ⌘K ⌘H.",
                symbol.name
            ),
        },
        _ => Verdict {
            title: format!("{name} returns nothing at this spot"),
            why: format!("It found no function at the cursor, on `{}`.", facts.word),
            fix: fresh_workspace,
        },
    }
}

fn describe<T>(probe: &Probe<T>, answered: impl FnOnce(&T) -> String) -> String {
    match probe {
        Probe::Answered(value) => answered(value),
        Probe::Failed(error) => format!("failed: {error}"),
        Probe::TimedOut => format!("no answer in {} s", PROBE_TIMEOUT.as_secs()),
    }
}

pub(crate) fn report(facts: &Facts, verdict: &Verdict, photon_version: &str) -> String {
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    let mut rows = vec![
        (
            "File".to_string(),
            format!(
                "`{}`, line {}, column {}, on `{}`",
                facts.file, facts.line, facts.column, facts.word
            ),
        ),
        (
            "Position sent to the server".to_string(),
            format!(
                "line {}, character {} (zero-based, UTF-16)",
                facts.sent_position.0, facts.sent_position.1
            ),
        ),
        ("Language".to_string(), facts.language.clone()),
        (
            "Call hierarchy, as Photon showed it".to_string(),
            describe(&facts.photon_found, |count| format!("{count} found")),
        ),
    ];
    match &facts.server {
        None => rows.push(("Language server".into(), "none running".into())),
        Some(server) => {
            rows.push((
                "Language server".into(),
                match &server.version {
                    Some(version) => format!("{} {version}", server.name),
                    None => server.name.clone(),
                },
            ));
            rows.push((
                "Offers call hierarchy".into(),
                yes_no(server.offers_call_hierarchy).into(),
            ));
            rows.push((
                "Call hierarchy, asked directly".into(),
                describe(&server.call_hierarchy, |names| {
                    if names.is_empty() {
                        "nothing".into()
                    } else {
                        names.join(", ")
                    }
                }),
            ));
            rows.push((
                "Go to definition".into(),
                describe(&server.definitions, |count| format!("{count} found")),
            ));
            rows.push((
                "Hover".into(),
                describe(&server.hover, |found| yes_no(*found).into()),
            ));
            rows.push((
                "Symbols in this file".into(),
                describe(&server.symbols, |(count, around)| match around {
                    Some(symbol) if symbol.on_name => {
                        format!("{count}; the cursor is on `{}`'s name", symbol.name)
                    }
                    Some(symbol) => format!("{count}; the cursor is inside `{}`", symbol.name),
                    None => format!("{count}; none around the cursor"),
                }),
            ));
            rows.push((
                "Modules loaded".into(),
                match &server.modules {
                    Some(modules) if modules.is_empty() => "none".into(),
                    Some(modules) => modules.join(", "),
                    None => "the server can't say".into(),
                },
            ));
            rows.push((
                "File in a loaded module".into(),
                match server.outside_modules {
                    Some(outside) => yes_no(!outside).into(),
                    None => "the server can't say".into(),
                },
            ));
            if let Some(folder) = &server.workspace_folder {
                rows.push(("Server's saved workspace".into(), format!("`{folder}`")));
            }
        }
    }
    rows.push((
        "Build errors".into(),
        match &facts.build_error {
            Some((file, summary)) => format!("{file}: {summary}"),
            None => "none".into(),
        },
    ));
    rows.push((
        "Errors in this file".into(),
        facts.errors_in_file.to_string(),
    ));

    let mut text = format!(
        "# Code navigation diagnosis\n\n**{}**\n\n**Why:** {}\n\n**Fix:** {}\n\n## What was checked\n\n| Check | Result |\n|---|---|\n",
        verdict.title, verdict.why, verdict.fix
    );
    for (check, result) in rows {
        text.push_str(&format!("| {check} | {} |\n", result.replace('|', "\\|")));
    }
    text.push_str(&format!(
        "\nPhoton {photon_version}, {} {}\n\n## Ask for help\n\nThis report has file paths, module names and the word under the cursor, but no source code. Paste it to the Photon team, or to an AI assistant with:\n\n> Photon's code navigation finds nothing at this spot in my project. Below is its diagnosis report. What's the root cause, and exactly what should I change to fix it?\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    text
}

pub(crate) fn diagnose(
    workspace: &mut Workspace,
    _: &DiagnoseNavigation,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let toast = NotificationId::unique::<DiagnoseNavigation>();
    let cursor = workspace.active_item_as::<Editor>(cx).and_then(|editor| {
        editor.update(cx, |editor, cx| {
            let head = editor.selections.newest_anchor().head();
            editor.buffer().read(cx).text_anchor_for_position(head, cx)
        })
    });
    let Some((buffer, position)) = cursor else {
        workspace.show_toast(
            Toast::new(
                toast,
                "Put the cursor where navigation fails in a file, then diagnose again.",
            ),
            cx,
        );
        return;
    };
    workspace.show_toast(Toast::new(toast.clone(), "Diagnosing code navigation…"), cx);
    let project = workspace.project().clone();
    let photon_version = release_channel::AppVersion::global(cx).to_string();
    cx.spawn_in(window, async move |workspace, cx| {
        let facts = gather(&project, &buffer, position, cx).await?;
        let verdict = verdict(&facts);
        log::info!("navigation diagnosis: {}: {}", verdict.title, verdict.why);
        let text = report(&facts, &verdict, &photon_version);
        let markdown = project
            .read_with(cx, |project, _| {
                project.languages().language_for_name("Markdown")
            })
            .await
            .ok();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.dismiss_toast(&toast, cx);
            let buffer = project.update(cx, |project, cx| {
                project.create_local_buffer(&text, markdown, false, cx)
            });
            let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        })
    })
    .detach_and_log_err(cx);
}

async fn gather(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    position: language::Anchor,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<Facts> {
    let (mut facts, server, abs_path, photon_request, build_files) = cx.update(|_, cx| {
        let snapshot = buffer.read(cx).snapshot();
        let point = position.to_point(&snapshot);
        let utf16 = snapshot.point_to_point_utf16(point);
        let (word, _) = snapshot.surrounding_word(position, None);
        let errors_in_file = snapshot
            .diagnostics_in_range::<usize, usize>(0..snapshot.len(), false)
            .filter(|entry| entry.diagnostic.severity == lsp::DiagnosticSeverity::ERROR)
            .count();
        let lsp_store = project.read(cx).lsp_store();
        let server_ids = lsp_store.update(cx, |lsp_store, cx| {
            buffer.update(cx, |buffer, cx| {
                lsp_store.language_servers_for_local_buffer(buffer, cx)
            })
        });
        let servers = server_ids
            .iter()
            .filter_map(|id| lsp_store.read(cx).language_server_for_id(*id))
            .collect::<Vec<_>>();
        let offers_call_hierarchy =
            |server: &Arc<lsp::LanguageServer>| match server.capabilities().call_hierarchy_provider
            {
                Some(lsp::CallHierarchyServerCapability::Simple(supported)) => supported,
                Some(lsp::CallHierarchyServerCapability::Options(_)) => true,
                None => false,
            };
        let server = servers
            .iter()
            .find(|server| offers_call_hierarchy(server))
            .or(servers.first())
            .cloned();
        let buffer_ref = buffer.read(cx);
        let facts = Facts {
            language: buffer_ref
                .language()
                .map_or("This".to_string(), |language| language.name().to_string()),
            file: buffer_ref
                .file()
                .map(|file| file.path().as_unix_str().to_string())
                .unwrap_or_default(),
            line: point.row + 1,
            column: point.column + 1,
            sent_position: (utf16.row, utf16.column),
            word: snapshot.text_for_range(word).collect::<String>(),
            errors_in_file,
            server: server.as_ref().map(|server| {
                let arguments = server
                    .binary()
                    .arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy().to_string())
                    .collect::<Vec<_>>();
                ServerFacts {
                    name: server.name().0.to_string(),
                    version: server
                        .readable_version()
                        .or(server.version())
                        .map(|version| version.to_string()),
                    workspace_folder: arguments
                        .iter()
                        .position(|argument| argument == "-data")
                        .and_then(|index| arguments.get(index + 1).cloned()),
                    offers_call_hierarchy: offers_call_hierarchy(server),
                    call_hierarchy: Probe::TimedOut,
                    definitions: Probe::TimedOut,
                    hover: Probe::TimedOut,
                    symbols: Probe::TimedOut,
                    modules: None,
                    outside_modules: None,
                }
            }),
            photon_found: Probe::TimedOut,
            build_error: None,
        };
        let abs_path = buffer_ref
            .file()
            .and_then(|file| file.as_local())
            .map(|file| file.abs_path(cx));
        let photon_request = project.update(cx, |project, cx| {
            project.prepare_call_hierarchy(buffer, position, cx)
        });
        let build_files = build_files_with_errors(project, buffer, cx);
        (facts, server, abs_path, photon_request, build_files)
    })?;

    facts.photon_found = match photon_request.await {
        Ok(items) => Probe::Answered(items.map_or(0, |items| items.len())),
        Err(error) => Probe::Failed(format!("{error:#}")),
    };
    facts.build_error = first_build_error(project, build_files, cx)
        .await
        .map(|error| {
            (
                error.path.path.as_unix_str().to_string(),
                build_error_summary(&error.message).to_string(),
            )
        });

    let (Some(server), Some(abs_path), Some(server_facts)) =
        (server, abs_path, facts.server.as_mut())
    else {
        return Ok(facts);
    };
    let uri = lsp::Uri::from_file_path(&abs_path)
        .map_err(|()| anyhow::anyhow!("{} isn't a valid file URI", abs_path.display()))?;
    let at = lsp::TextDocumentPositionParams {
        text_document: lsp::TextDocumentIdentifier { uri: uri.clone() },
        position: lsp::Position::new(facts.sent_position.0, facts.sent_position.1),
    };
    // Asked together, so a busy server costs one time limit, not one per question.
    let server_id = server.server_id();
    let source_folders = cx.update(|_, cx| {
        server_offering(project, server_id, LIST_SOURCE_FOLDERS, cx).map(|server| {
            server.request::<lsp::request::ExecuteCommand>(
                lsp::ExecuteCommandParams {
                    command: LIST_SOURCE_FOLDERS.to_string(),
                    arguments: Vec::new(),
                    work_done_progress_params: Default::default(),
                },
                PROBE_TIMEOUT,
            )
        })
    })?;
    let source_folders = async move {
        match source_folders {
            Some(request) => Some(request.await),
            None => None,
        }
    };
    let (call_hierarchy, definitions, hover, symbols, source_folders) = futures::join!(
        server.request::<lsp::request::CallHierarchyPrepare>(
            lsp::CallHierarchyPrepareParams {
                text_document_position_params: at.clone(),
                work_done_progress_params: Default::default(),
            },
            PROBE_TIMEOUT,
        ),
        server.request::<lsp::request::GotoDefinition>(
            lsp::GotoDefinitionParams {
                text_document_position_params: at.clone(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            PROBE_TIMEOUT,
        ),
        server.request::<lsp::request::HoverRequest>(
            lsp::HoverParams {
                text_document_position_params: at.clone(),
                work_done_progress_params: Default::default(),
            },
            PROBE_TIMEOUT,
        ),
        server.request::<lsp::request::DocumentSymbolRequest>(
            lsp::DocumentSymbolParams {
                text_document: lsp::TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            PROBE_TIMEOUT,
        ),
        source_folders,
    );
    server_facts.call_hierarchy = Probe::from_connection(call_hierarchy, |items| {
        items
            .unwrap_or_default()
            .into_iter()
            .map(|item| item.name)
            .collect()
    });
    server_facts.definitions = Probe::from_connection(definitions, |response| match response {
        Some(lsp::GotoDefinitionResponse::Scalar(_)) => 1,
        Some(lsp::GotoDefinitionResponse::Array(locations)) => locations.len(),
        Some(lsp::GotoDefinitionResponse::Link(links)) => links.len(),
        None => 0,
    });
    server_facts.hover = Probe::from_connection(hover, |hover| hover.is_some());
    server_facts.symbols =
        Probe::from_connection(symbols, |response| symbols_around(response, at.position));

    if let Some(answer) = source_folders
        && let Some(folders) = answer
            .into_response()
            .log_err()
            .flatten()
            .as_ref()
            .and_then(parse_source_folders)
    {
        let mut modules = Vec::new();
        for (_, module) in &folders {
            if !module.is_empty() && !modules.contains(module) {
                modules.push(module.clone());
            }
        }
        let fs = cx.update(|_, cx| project.read(cx).fs().clone())?;
        let outside = modules_without(&abs_path, folders, fs.as_ref()).await;
        let server_facts = facts.server.as_mut().expect("checked above");
        server_facts.outside_modules = Some(outside.is_some());
        server_facts.modules = Some(modules);
    }
    Ok(facts)
}

fn contains(range: &lsp::Range, position: lsp::Position) -> bool {
    range.start <= position && position <= range.end
}

/// How many symbols the response has, and the innermost one whose range holds `position`.
fn symbols_around(
    response: Option<lsp::DocumentSymbolResponse>,
    position: lsp::Position,
) -> (usize, Option<SymbolAtCursor>) {
    match response {
        None => (0, None),
        Some(lsp::DocumentSymbolResponse::Flat(symbols)) => {
            let around = symbols
                .iter()
                .filter(|symbol| contains(&symbol.location.range, position))
                .min_by_key(|symbol| {
                    let range = symbol.location.range;
                    (range.end.line - range.start.line, range.end.character)
                })
                .map(|symbol| SymbolAtCursor {
                    name: symbol.name.clone(),
                    // A flat list has no name ranges; it can't tell name from body.
                    on_name: true,
                });
            (symbols.len(), around)
        }
        Some(lsp::DocumentSymbolResponse::Nested(symbols)) => {
            fn walk(
                symbols: &[lsp::DocumentSymbol],
                position: lsp::Position,
                count: &mut usize,
                around: &mut Option<SymbolAtCursor>,
            ) {
                for symbol in symbols {
                    *count += 1;
                    if contains(&symbol.range, position) {
                        *around = Some(SymbolAtCursor {
                            name: symbol.name.clone(),
                            on_name: contains(&symbol.selection_range, position),
                        });
                    }
                    walk(
                        symbol.children.as_deref().unwrap_or_default(),
                        position,
                        count,
                        around,
                    );
                }
            }
            let mut count = 0;
            let mut around = None;
            walk(&symbols, position, &mut count, &mut around);
            (count, around)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn java_server() -> ServerFacts {
        ServerFacts {
            name: "jdtls".into(),
            version: Some("1.63.0".into()),
            workspace_folder: Some("/Users/me/Library/Caches/jdtls-abc".into()),
            offers_call_hierarchy: true,
            call_hierarchy: Probe::Answered(Vec::new()),
            definitions: Probe::Answered(0),
            hover: Probe::Answered(false),
            symbols: Probe::Answered((12, None)),
            modules: Some(vec!["core".into(), "web".into()]),
            outside_modules: Some(false),
        }
    }

    fn facts(server: Option<ServerFacts>) -> Facts {
        Facts {
            language: "Java".into(),
            file: "api/src/main/java/app/Shipments.java".into(),
            line: 64,
            column: 56,
            sent_position: (63, 55),
            word: "getShipment".into(),
            errors_in_file: 0,
            server,
            photon_found: Probe::Answered(0),
            build_error: None,
        }
    }

    fn title(facts: &Facts) -> String {
        verdict(facts).title
    }

    #[test]
    fn names_each_root_cause() {
        assert_eq!(
            title(&facts(None)),
            "No language server is running for this Java file"
        );

        let mut server = java_server();
        server.offers_call_hierarchy = false;
        assert_eq!(
            title(&facts(Some(server))),
            "jdtls doesn't offer call hierarchy"
        );

        let mut server = java_server();
        server.call_hierarchy = Probe::TimedOut;
        assert_eq!(title(&facts(Some(server))), "jdtls didn't answer");

        let mut server = java_server();
        server.call_hierarchy = Probe::Failed("Internal error".into());
        assert_eq!(title(&facts(Some(server))), "jdtls answered with an error");

        let mut server = java_server();
        server.call_hierarchy = Probe::Answered(vec!["getShipment(long)".into()]);
        assert_eq!(
            title(&facts(Some(server.clone()))),
            "Photon dropped the language server's answer"
        );
        let mut found = facts(Some(server));
        found.photon_found = Probe::Answered(1);
        assert_eq!(title(&found), "Call hierarchy works here");

        let mut broken = facts(Some(java_server()));
        broken.build_error = Some(("pom.xml".into(), "Non-resolvable parent POM".into()));
        assert_eq!(title(&broken), "Java couldn't load this project's build");

        let mut server = java_server();
        server.outside_modules = Some(true);
        let outcome = verdict(&facts(Some(server)));
        assert_eq!(outcome.title, "Java hasn't loaded this file's module");
        assert!(outcome.why.contains("core, web"), "{}", outcome.why);

        let mut server = java_server();
        server.symbols = Probe::Answered((0, None));
        assert_eq!(
            title(&facts(Some(server))),
            "jdtls sees no code in this file"
        );

        let mut server = java_server();
        server.symbols = Probe::Answered((
            12,
            Some(SymbolAtCursor {
                name: "getShipment(long)".into(),
                on_name: true,
            }),
        ));
        let outcome = verdict(&facts(Some(server)));
        assert_eq!(
            outcome.title,
            "jdtls knows `getShipment(long)` but returns no call hierarchy for it"
        );
        assert!(
            outcome.fix.contains("/Users/me/Library/Caches/jdtls-abc"),
            "{}",
            outcome.fix
        );

        let mut server = java_server();
        server.symbols = Probe::Answered((
            12,
            Some(SymbolAtCursor {
                name: "getShipment(long)".into(),
                on_name: false,
            }),
        ));
        assert_eq!(
            title(&facts(Some(server))),
            "The cursor isn't on a function's name"
        );

        assert_eq!(
            title(&facts(Some(java_server()))),
            "jdtls returns nothing at this spot"
        );
    }

    #[test]
    fn the_report_shows_every_check_and_a_prompt_to_share() {
        let facts = facts(Some(java_server()));
        let text = report(&facts, &verdict(&facts), "0.1.19-alpha.1");
        for expected in [
            "**jdtls returns nothing at this spot**",
            "| File | `api/src/main/java/app/Shipments.java`, line 64, column 56, on `getShipment` |",
            "| Position sent to the server | line 63, character 55 (zero-based, UTF-16) |",
            "| Call hierarchy, as Photon showed it | 0 found |",
            "| Language server | jdtls 1.63.0 |",
            "| Call hierarchy, asked directly | nothing |",
            "| Modules loaded | core, web |",
            "| File in a loaded module | yes |",
            "| Build errors | none |",
            "Photon 0.1.19-alpha.1",
            "What's the root cause",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn finds_the_symbol_around_the_cursor_and_whether_its_on_the_name() {
        #[allow(deprecated)]
        let method = lsp::DocumentSymbol {
            name: "getShipment(long)".into(),
            detail: None,
            kind: lsp::SymbolKind::METHOD,
            tags: None,
            deprecated: None,
            range: lsp::Range::new(lsp::Position::new(63, 4), lsp::Position::new(70, 5)),
            selection_range: lsp::Range::new(
                lsp::Position::new(63, 48),
                lsp::Position::new(63, 59),
            ),
            children: None,
        };
        #[allow(deprecated)]
        let class = lsp::DocumentSymbol {
            name: "Shipments".into(),
            detail: None,
            kind: lsp::SymbolKind::CLASS,
            tags: None,
            deprecated: None,
            range: lsp::Range::new(lsp::Position::new(10, 0), lsp::Position::new(200, 1)),
            selection_range: lsp::Range::new(
                lsp::Position::new(10, 13),
                lsp::Position::new(10, 22),
            ),
            children: Some(vec![method]),
        };
        let response = || Some(lsp::DocumentSymbolResponse::Nested(vec![class.clone()]));
        assert_eq!(
            symbols_around(response(), lsp::Position::new(63, 55)),
            (
                2,
                Some(SymbolAtCursor {
                    name: "getShipment(long)".into(),
                    on_name: true
                })
            )
        );
        assert_eq!(
            symbols_around(response(), lsp::Position::new(66, 10)),
            (
                2,
                Some(SymbolAtCursor {
                    name: "getShipment(long)".into(),
                    on_name: false
                })
            )
        );
        assert_eq!(symbols_around(None, lsp::Position::new(0, 0)), (0, None));
    }
}
