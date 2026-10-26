# Language server setup help

Roadmap item 16. A user opened a Java project, jdtls failed with "class file version up to 61.0"
(it needs Java 21, the machine had 17), and nothing told them what to do.

## What the user sees

The status item next to the language server button (`SetUpLanguageServer` in
`crates/photon_navigation`) has two states, both for the active file only:

| State | Status bar | Popover (opened by the button) |
|---|---|---|
| A server for the file's language failed to download or start | "Language server failed · Help" | server name, the error's first lines, a known fix when one matches, **Restart**, **Show Log** |
| No server runs for the file | "No language server · Set up" | the system requirement and install command when known, **Install Extension**, **Detect & Start** |

The popover is a `PopoverMenu` with a `ContextMenu`, like the language server button's menu. A known
fix is one sentence, a shell command with a copy button, and optionally a settings snippet with a
copy button. Photon never runs the command; the user does.

- **Restart** and **Detect & Start** call `LspStore::restart_language_servers_for_buffers` for the
  active buffer (the path behind "editor: restart language server").
- **Show Log** opens the full error, including the server's stderr, read-only in a new tab. A server
  that failed to start never reaches the server log view (it is only added once running), so the error
  text is its log.
- **Install Extension** opens the extension page, as "Set up" did before.

## Where failures come from

`LspStoreEvent::LanguageServerUpdate` with a binary `StatusUpdate`. `Failed` carries the error (with
stderr for start and initialize failures); any other status for that server name clears it. A
failure is shown when its server name is one of the adapters registered for the active file's
language. No polling: the item reacts to that event, `LanguageServerAdded/Removed`,
`LanguageDetected`, and active item changes.

Not covered: a server that crashes after it started. Photon has no event for that today (the
process exit is not reported), so it would need an upstream hook.

## Known fixes

A table in `photon_navigation/src/language_server_fixes.rs`, matched by language (or extension id)
and, for failures, by error text. Entries with no error patterns match any failure of that language.

| Languages | Error contains | Fix |
|---|---|---|
| Java, Gradle | class file version, UnsupportedClassVersionError, requires at least Java, Java executable, Unable to locate a Java Runtime | jdtls needs Java 21 or newer. `brew install openjdk@21`, settings `lsp.jdtls.settings.java_home` |
| Go | any | gopls needs Go. `brew install go` |
| Ruby | any | ruby-lsp needs Ruby. `gem install ruby-lsp` |
| C, C++ | any | clangd comes with the Xcode command line tools. `xcode-select --install` |

`java_home` is the key the Java extension reads (`lsp.jdtls.settings.java_home`, confirmed in the
extension's own error message).
