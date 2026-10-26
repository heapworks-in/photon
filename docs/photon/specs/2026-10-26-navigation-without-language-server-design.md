# Navigation without a language server

Photon never installs extensions or language servers on its own, so a Java file opened without the
Java extension has no server. Before this change ⌘-click and F12 on a method did nothing at all.

## "No language server · Set up" (status bar)

- `photon_navigation::SetUpLanguageServer`, a left status bar item next to Zed's language server
  button. Zed's button hides itself when no server exists, so there was no indicator to reuse.
- Shown when the active editor's buffer has no language server registered and either
  - Photon has no language for the file but `extension_suggest` maps its name or suffix to an
    extension (the `.java` case), or
  - the file has a language with server adapters registered, none of them started for the file.
- "Set up" dispatches `zed::Extensions { id: <suggested extension, or the lowercased language name> }`.
- Refreshes on active item changes and on `LanguageServerAdded`, `LanguageServerRemoved` and
  `LanguageDetected`. No polling. Hidden for remote projects, where the servers aren't visible locally.

## Definition fallback

- Hook: `editor::DefinitionFallback`, a global the editor calls in two places, once go-to-definition
  (`editor::GoToDefinition`) or ⌘-click has found nothing and before Zed's own
  `go_to_definition_fallback`. It returns `None` when a running server for the buffer supports
  definitions, so the fallback never replaces a language server.
- `photon_navigation::init` sets the global. The fallback takes the word under the cursor, runs a
  regex project search (respecting ignored and excluded files), and hands the matches to
  `navigate_to_hover_links`: one match opens it at the name, several open a "Definitions" multibuffer
  the way Find References does, none shows the toast "No definition found for NAME".
- The running search sits in a global. A new search replaces it, and dropping the old one cancels it.

## Definition pattern

Two alternatives, both case-sensitive and whole-word on NAME (escaped):

1. A declaration keyword right before the name (`class|interface|enum|record|struct|trait|type|
   typealias|object|protocol|module|union|fn|func|def|function|fun|sub|const|let|var|val`), with
   an optional generic list or Go receiver, plus a qualifier such as `self.` or `String.`.
2. A modifier earlier on the line (`void|static|public|private|protected|internal|override|abstract|
   final|virtual|async|suspend|inline|extern|export`), then the name followed by `( < { : ; =`. The
   gap may not contain `= ; ( {`, which keeps calls inside bodies and assignments out.

Known misses: Java/C# methods with no modifier (`String bar(`) and JS methods without a keyword.
