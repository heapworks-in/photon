mod language_server_fixes;
mod set_up_language_server;

use std::rc::Rc;

use editor::{
    DefinitionFallback, Editor, GotoDefinitionKind, MultiBufferOffset, Navigated,
    hover_links::HoverLink,
};
use futures::channel::oneshot;
use gpui::{App, Context, Entity, Global, Task, Window};
use language::{Buffer, CharKind, Location, ToOffset as _};
use project::{
    LocationLink, Project,
    search::{SearchQuery, SearchResult},
};
use util::{ResultExt as _, paths::PathMatcher};
use workspace::Toast;

pub use set_up_language_server::SetUpLanguageServer;

pub fn init(cx: &mut App) {
    cx.set_global(DefinitionFallback(Rc::new(definition_fallback)));
}

/// The search behind the latest go-to-definition; replacing it drops, and so cancels, the previous one.
struct DefinitionSearch {
    _search: Task<()>,
}

impl Global for DefinitionSearch {}

fn definition_fallback(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Task<anyhow::Result<Navigated>>> {
    let project = editor.project()?.clone();
    let workspace = editor.workspace()?.downgrade();
    let head = editor
        .selections
        .newest::<MultiBufferOffset>(&editor.display_snapshot(cx))
        .head();
    let (buffer, position) = editor
        .buffer()
        .read(cx)
        .text_anchor_for_position(head, cx)?;
    if has_definition_server(&project, &buffer, cx) {
        return None;
    }
    let snapshot = buffer.read(cx).snapshot();
    let (word, kind) = snapshot.surrounding_word(position, None);
    if kind != Some(CharKind::Word) {
        return None;
    }
    let name: String = snapshot.text_for_range(word).collect();
    let query = SearchQuery::regex(
        definition_pattern(&name),
        false,
        true,
        false,
        false,
        PathMatcher::default(),
        PathMatcher::default(),
        false,
        None,
    )
    .log_err()?;

    let search = project.update(cx, |project, cx| project.search(query, cx));
    let (send_locations, locations) = oneshot::channel();
    let search = cx.spawn({
        let name = name.clone();
        async move |_, cx| {
            // Move the whole result in: capturing only `rx` would drop the task that runs the search.
            let search = search;
            let mut locations = Vec::new();
            while let Ok(result) = search.rx.recv().await {
                if let SearchResult::Buffer { buffer, ranges } = result {
                    let name_ranges = buffer.read_with(cx, |buffer, _| {
                        ranges
                            .iter()
                            .filter_map(|range| {
                                let start = range.start.to_offset(buffer);
                                let matched: String =
                                    buffer.text_for_range(range.clone()).collect();
                                let name_start = start + matched.rfind(&name)?;
                                Some(
                                    buffer.anchor_after(name_start)
                                        ..buffer.anchor_before(name_start + name.len()),
                                )
                            })
                            .collect::<Vec<_>>()
                    });
                    locations.extend(name_ranges.into_iter().map(|range| Location {
                        buffer: buffer.clone(),
                        range,
                    }));
                }
            }
            send_locations.send(locations).ok();
        }
    });
    cx.set_global(DefinitionSearch { _search: search });

    Some(cx.spawn_in(window, async move |editor, cx| {
        let Ok(locations) = locations.await else {
            return Ok(Navigated::No);
        };
        if locations.is_empty() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        no_definition_notification_id(),
                        format!("No definition found for {name}"),
                    )
                    .autohide(),
                    cx,
                )
            })?;
            return Ok(Navigated::No);
        }
        let links = locations
            .into_iter()
            .map(|target| {
                HoverLink::Text(LocationLink {
                    origin: None,
                    target,
                })
            })
            .collect();
        editor
            .update_in(cx, |editor, window, cx| {
                editor.navigate_to_hover_links(
                    Some(GotoDefinitionKind::Symbol),
                    links,
                    None,
                    false,
                    window,
                    cx,
                )
            })?
            .await
    }))
}

// ponytail: only local projects can be checked for servers, so remote ones keep Zed's behavior.
fn has_definition_server(project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut App) -> bool {
    if !project.read(cx).is_local() {
        return true;
    }
    let lsp_store = project.read(cx).lsp_store();
    buffer.update(cx, |buffer, cx| {
        lsp_store.update(cx, |lsp_store, cx| {
            lsp_store
                .running_language_servers_for_local_buffer(buffer, cx)
                .any(|(_, server)| {
                    !matches!(
                        server.capabilities().definition_provider,
                        None | Some(lsp::OneOf::Left(false))
                    )
                })
        })
    })
}

/// A declaration keyword directly before the name: `class Foo`, `def bar`, `func (s *S) bar`, `fun <T> String.bar`.
const KEYWORD_DECLARATION: &str = r"\b(?:class|interface|enum|record|struct|trait|type|typealias|object|protocol|module|union|fn|func|def|function|fun|sub|const|let|var|val)[ \t]+(?:<[^>\n]*>[ \t]*|\([^)\n]*\)[ \t]*)?(?:[\w.]+\.)?";

/// A modifier earlier on the line, then the name before a parameter list, body, type or value:
/// `public static int bar(`, `void bar(`, `private int bar = 3;`, `public string Bar { get; }`.
/// The gap may not contain `=`, `;`, `(` or `{`, so calls inside bodies and assignments don't match.
const MODIFIER_DECLARATION: &str = r"\b(?:void|static|public|private|protected|internal|override|abstract|final|virtual|async|suspend|inline|extern|export)\b[^\n=;({]*?\b";

/// A regex that matches common definition forms of `name` across languages.
pub fn definition_pattern(name: &str) -> String {
    let name = regex::escape(name);
    format!(r"{KEYWORD_DECLARATION}{name}\b|{MODIFIER_DECLARATION}{name}[ \t]*[(<{{:;=]")
}

struct NoDefinitionNotice;

fn no_definition_notification_id() -> workspace::notifications::NotificationId {
    workspace::notifications::NotificationId::unique::<NoDefinitionNotice>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::actions::GoToDefinition;
    use editor::test::editor_test_context::EditorTestContext;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use indoc::indoc;
    use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
    use project::{FakeFs, Project};
    use regex::RegexBuilder;
    use serde_json::json;
    use std::sync::Arc;
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace, Workspace};

    fn is_definition(name: &str, line: &str) -> bool {
        RegexBuilder::new(&definition_pattern(name))
            .multi_line(true)
            .build()
            .expect("pattern compiles")
            .is_match(line)
    }

    #[track_caller]
    fn assert_definitions(name: &str, lines: &[&str]) {
        for line in lines {
            assert!(is_definition(name, line), "should define {name}: {line}");
        }
    }

    #[track_caller]
    fn assert_usages(name: &str, lines: &[&str]) {
        for line in lines {
            assert!(
                !is_definition(name, line),
                "should not define {name}: {line}"
            );
        }
    }

    #[test]
    fn java_definitions() {
        assert_definitions(
            "Foo",
            &[
                "public class Foo {",
                "class Foo extends Bar {",
                "public Foo(int x) {",
                "enum Foo {",
                "record Foo(int x) {}",
                "interface Foo<T> {",
            ],
        );
        assert_definitions(
            "bar",
            &[
                "void bar() {",
                "  public static int bar(String s) {",
                "@Override protected List<String> bar(int x) {",
                "private int bar = 3;",
            ],
        );
        assert_usages(
            "bar",
            &[
                "bar();",
                "  return bar(x);",
                "int y = bar(2);",
                "this.bar(1);",
                "public void run() { bar(); }",
                "private Foo bar2 = bar(1);",
            ],
        );
        assert_usages(
            "Foo",
            &[
                "Foo foo = new Foo();",
                "private Foo foo = new Foo();",
                "import com.example.Foo;",
                "private List<Foo> items;",
                "class Bar extends Foo {",
            ],
        );
    }

    #[test]
    fn kotlin_definitions() {
        assert_definitions(
            "bar",
            &[
                "fun bar(x: Int) {",
                "override fun bar() = 1",
                "suspend fun bar()",
                "fun <T> bar(x: T)",
                "fun String.bar(): Int",
                "val bar = 3",
                "private val bar: Int = 3",
            ],
        );
        assert_definitions(
            "Foo",
            &[
                "data class Foo(val x: Int)",
                "object Foo {",
                "typealias Foo = Int",
            ],
        );
        assert_usages("bar", &["val x = bar()", "bar(1)", "foo.bar()"]);
    }

    #[test]
    fn python_definitions() {
        assert_definitions("bar", &["def bar(x):", "    async def bar(self):"]);
        assert_definitions("Foo", &["class Foo(Base):", "class Foo:"]);
        assert_usages(
            "bar",
            &["bar(1)", "x = bar()", "from module import bar", "type(bar)"],
        );
    }

    #[test]
    fn go_definitions() {
        assert_definitions(
            "bar",
            &["func bar(x int) int {", "func (s *Server) bar() {"],
        );
        assert_definitions("Foo", &["type Foo struct {", "type Foo interface {"]);
        assert_usages("bar", &["x := bar()", "func main() { bar() }", "s.bar()"]);
    }

    #[test]
    fn javascript_and_typescript_definitions() {
        assert_definitions(
            "bar",
            &[
                "function bar(x) {",
                "export function bar() {",
                "async function bar() {",
                "const bar = () => 1;",
                "let bar = function () {}",
                "export const bar = 1;",
                "  public bar(): void {",
                "  async bar() {",
                "  private readonly bar: string;",
            ],
        );
        assert_definitions(
            "Foo",
            &[
                "class Foo {",
                "export class Foo extends Bar {",
                "interface Foo {",
                "type Foo = { x: number };",
                "enum Foo {",
            ],
        );
        assert_usages(
            "bar",
            &[
                "bar();",
                "const x = bar(1);",
                "import { bar } from './bar';",
                "export { bar };",
                "async () => bar()",
                "obj.bar()",
            ],
        );
    }

    #[test]
    fn ruby_definitions() {
        assert_definitions("bar", &["def bar(x)", "  def self.bar", "def bar"]);
        assert_definitions("Foo", &["class Foo < Base", "module Foo"]);
        assert_usages("bar", &["bar(1)", "Foo.new.bar", "x = bar"]);
    }

    #[test]
    fn csharp_definitions() {
        assert_definitions(
            "Foo",
            &[
                "public class Foo : IBar",
                "public record Foo(int X);",
                "struct Foo {",
                "public Foo() {",
            ],
        );
        assert_definitions(
            "Bar",
            &[
                "public void Bar() {",
                "public static async Task<int> Bar(int x)",
                "public string Bar { get; set; }",
                "internal override string Bar(string s)",
            ],
        );
        assert_usages("Foo", &["var x = new Foo();", "Foo.Bar();", "using Foo;"]);
        assert_usages("Bar", &["x.Bar();", "return Bar(1);"]);
    }

    #[test]
    fn names_are_escaped_and_whole_words() {
        assert_definitions("$bar", &["function $bar() {"]);
        assert_usages("bar", &["function barista() {", "class Foobar {"]);
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let state = AppState::test(cx);
            editor::init(cx);
            init(cx);
            drop(state);
        });
    }

    async fn open_in_workspace(
        files: serde_json::Value,
        languages: Vec<(Arc<Language>, Option<FakeLspAdapter>)>,
        open: &str,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<Editor>,
        Option<lsp::FakeLanguageServer>,
        VisualTestContext,
    ) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), files).await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let mut fake_servers = None;
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        for (language, adapter) in languages {
            registry.add(language.clone());
            if let Some(adapter) = adapter {
                fake_servers = Some(registry.register_fake_lsp(language.name(), adapter));
            }
        }
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id()
        });
        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path(open)), None, true, window, cx)
            })
            .await
            .expect("file opens")
            .downcast::<Editor>()
            .expect("an editor");
        cx.run_until_parked();
        let fake_server = match fake_servers {
            Some(mut servers) => {
                use futures::StreamExt as _;
                servers.next().await
            }
            None => None,
        };
        (workspace, editor, fake_server, cx.clone())
    }

    fn java() -> Arc<Language> {
        Arc::new(Language::new(
            LanguageConfig {
                name: "Java".into(),
                matcher: Arc::new(LanguageMatcher {
                    path_suffixes: vec!["java".into()],
                    ..LanguageMatcher::default()
                }),
                ..LanguageConfig::default()
            },
            None,
        ))
    }

    fn move_cursor_to(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) {
        editor.update_in(cx, |editor, window, cx| {
            let offset = editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text()
                .find(text)
                .expect("text in buffer");
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let position = snapshot.anchor_before(editor::MultiBufferOffset(offset + 1));
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([position..position])
            });
        });
    }

    fn go_to_definition(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
        editor
            .update_in(cx, |editor, window, cx| {
                editor.go_to_definition(&GoToDefinition::default(), window, cx)
            })
            .detach();
        cx.run_until_parked();
    }

    fn active_editor(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Editor> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("an active editor")
        })
    }

    fn active_text(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (String, String) {
        let editor = active_editor(workspace, cx);
        editor.update(cx, |editor, cx| {
            let offset = editor
                .selections
                .newest::<editor::MultiBufferOffset>(&editor.display_snapshot(cx))
                .start
                .0;
            let text = editor.buffer().read(cx).snapshot(cx).text();
            let line_start = text[..offset].rfind('\n').map_or(0, |ix| ix + 1);
            let line_end = text[offset..]
                .find('\n')
                .map_or(text.len(), |ix| offset + ix);
            (
                text[line_start..line_end].to_string(),
                text[offset..line_end].to_string(),
            )
        })
    }

    const FOO_JAVA: &str = indoc! {"
        package example;

        public class Foo {
            public void bar(int x) {
            }
        }
    "};

    const MAIN_JAVA: &str = indoc! {"
        package example;

        class Main {
            void run() {
                Foo foo = new Foo();
                foo.bar(1);
                foo.missing();
            }
        }
    "};

    #[gpui::test]
    async fn go_to_definition_without_server_opens_the_text_match(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, editor, _, mut cx) = open_in_workspace(
            json!({ "Foo.java": FOO_JAVA, "Main.java": MAIN_JAVA }),
            Vec::new(),
            "Main.java",
            cx,
        )
        .await;

        move_cursor_to(&editor, "bar(1)", &mut cx);
        go_to_definition(&editor, &mut cx);

        assert_eq!(
            active_text(&workspace, &mut cx),
            ("    public void bar(int x) {".into(), "bar(int x) {".into())
        );
    }

    #[gpui::test]
    async fn cmd_click_without_server_opens_the_text_match(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, editor, _, mut cx) = open_in_workspace(
            json!({ "Foo.java": FOO_JAVA, "Main.java": MAIN_JAVA }),
            Vec::new(),
            "Main.java",
            cx,
        )
        .await;

        let mut editor_cx = EditorTestContext::for_editor_in(editor, &mut cx).await;
        let position = editor_cx.pixel_position(&MAIN_JAVA.replacen("new Foo", "new Fˇoo", 1));
        let modifiers = editor_cx.update(|_, cx| gpui::Modifiers::secondary_key(cx));
        editor_cx.simulate_click(position, modifiers);
        cx.run_until_parked();

        assert_eq!(
            active_text(&workspace, &mut cx),
            ("public class Foo {".into(), "Foo {".into())
        );
    }

    #[gpui::test]
    async fn several_definitions_open_a_multibuffer(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, editor, _, mut cx) = open_in_workspace(
            json!({
                "Foo.java": FOO_JAVA,
                "Other.java": "class Other {\n    void bar() {}\n}\n",
                "Main.java": MAIN_JAVA,
            }),
            Vec::new(),
            "Main.java",
            cx,
        )
        .await;

        move_cursor_to(&editor, "bar(1)", &mut cx);
        go_to_definition(&editor, &mut cx);

        let results = active_editor(&workspace, &mut cx);
        assert_ne!(results, editor);
        let (excerpts, text) = results.update(&mut cx, |results, cx| {
            let buffer = results.buffer().read(cx);
            (buffer.as_singleton().is_none(), buffer.snapshot(cx).text())
        });
        assert!(excerpts, "results open in a multibuffer");
        assert!(text.contains("public void bar(int x)"), "{text}");
        assert!(text.contains("void bar() {}"), "{text}");
    }

    #[gpui::test]
    async fn no_definition_shows_a_notice(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, editor, _, mut cx) = open_in_workspace(
            json!({ "Foo.java": FOO_JAVA, "Main.java": MAIN_JAVA }),
            Vec::new(),
            "Main.java",
            cx,
        )
        .await;

        move_cursor_to(&editor, "missing", &mut cx);
        go_to_definition(&editor, &mut cx);

        assert_eq!(active_editor(&workspace, &mut cx), editor);
        workspace.read_with(&cx, |workspace, _| {
            assert!(workspace.has_notification(&no_definition_notification_id()));
        });
    }

    #[gpui::test]
    async fn language_server_answers_without_the_fallback(cx: &mut TestAppContext) {
        init_test(cx);
        let adapter = FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            ..FakeLspAdapter::default()
        };
        let (workspace, editor, fake_server, mut cx) = open_in_workspace(
            json!({ "Foo.java": FOO_JAVA, "Main.java": MAIN_JAVA }),
            vec![(java(), Some(adapter))],
            "Main.java",
            cx,
        )
        .await;
        let fake_server = fake_server.expect("server started");
        fake_server.set_request_handler::<lsp::request::GotoDefinition, _, _>(
            |params, _| async move {
                let uri = params.text_document_position_params.text_document.uri;
                Ok(Some(lsp::GotoDefinitionResponse::Scalar(lsp::Location {
                    uri,
                    range: lsp::Range::new(lsp::Position::new(2, 6), lsp::Position::new(2, 10)),
                })))
            },
        );

        move_cursor_to(&editor, "bar(1)", &mut cx);
        go_to_definition(&editor, &mut cx);

        assert_eq!(active_editor(&workspace, &mut cx), editor);
        assert_eq!(
            active_text(&workspace, &mut cx),
            ("class Main {".into(), "Main {".into())
        );
    }

    #[gpui::test]
    async fn a_new_search_cancels_the_previous_one(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, editor, _, mut cx) = open_in_workspace(
            json!({ "Foo.java": FOO_JAVA, "Main.java": MAIN_JAVA }),
            Vec::new(),
            "Main.java",
            cx,
        )
        .await;

        move_cursor_to(&editor, "bar(1)", &mut cx);
        let superseded = editor.update_in(&mut cx, |editor, window, cx| {
            editor.go_to_definition(&GoToDefinition::default(), window, cx)
        });
        move_cursor_to(&editor, "Foo()", &mut cx);
        go_to_definition(&editor, &mut cx);

        assert_eq!(superseded.await.ok(), Some(Navigated::No));
        assert_eq!(
            active_text(&workspace, &mut cx),
            ("public class Foo {".into(), "Foo {".into())
        );
    }
}
