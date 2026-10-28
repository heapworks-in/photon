# Java setup help and the Call Trails empty panel

Mock (approved): `2026-10-28-java-setup-and-call-trails-empty-mock.html`.

## 1. Java for jdtls

jdtls needs Java 21+. Photon starts it with the login shell's `JAVA_HOME`, often older (jenv, Java 17),
and it fails with "class file version up to 61.0".

**A. A newer Java is installed.** When a Java buffer would start jdtls and the user has not set
`lsp.jdtls.settings.java_home`, Photon checks the Java that would be used (`JAVA_HOME`, else `java`
on the shell PATH) once per app run. If it is older than 21, it looks for Java 21+ in
`/usr/libexec/java_home -V`, Homebrew (`/opt/homebrew/opt/openjdk*`), SDKMAN (`~/.sdkman/candidates/java`)
and jenv (`~/.jenv/versions`), takes the newest, writes `lsp.jdtls.settings.java_home` to user settings,
and (re)starts jdtls. One notification: "Using Java N for Java code intelligence", body says builds
and the terminal still use the old one, buttons **Change…** (list of the Javas found) and **OK**.

**B. None installed.** One notification per project: "Java code intelligence needs Java 21", body
"Go to definition, errors and completions for Java need Java 21 or newer. This Mac has Java N.",
**Install Java 21** and **Not now**. The status bar item reads "Needs Java 21 · Set up" and opens the
same choices (the existing setup-help popover gains the Install button). Without Homebrew the button
is **Download Java 21** and opens https://adoptium.net/temurin/releases/?version=21.

**C. Install.** Opens a terminal tab titled "Install Java 21" running `brew install openjdk@21`.
Nothing runs without the click; the user can watch and stop it.

**D. Done.** When that command exits 0, run A (finds openjdk@21), then "Java 21 installed · jdtls is
running". Non-zero exit: "Java 21 didn't install" with **Show Log** (the terminal tab).

Detection is a few `stat`s and one `java -version`/`java_home` per app run, in the background,
only when a Java buffer opens. Nothing at idle.

## 2. Call Trails empty panel

- **Nothing selected, server ready:** small trail picture (two callers → the function → …), "Who calls
  this, and what does it call?", "Put the cursor on a function and press ⌘K ⌘H, or pick one below.",
  **Recent** (last five functions traced in this project, in memory) and **Start points** (up to five
  main / HTTP handler / test functions, from the existing tag detection over project symbols, found
  once when the panel opens). Clicking one traces it.
- **Language server missing, failed, or starting** for the active file's language, whether or not a
  function is selected: a clear message in the panel, e.g. "No call trails for Java yet" with
  "jdtls is starting… This panel fills in as soon as it's ready", "jdtls failed to start" with the
  known fix's sentence, or "No language server for Java". Button **Set up language server** dispatches
  `photon_navigation::ShowLanguageServerHelp`. The panel updates on the language server events, no polling.
- **Server can't do call hierarchy:** says which server, same button.

## Rules

Photon crates only (`photon_navigation`, `photon_call_trails`, app wiring in `zed/src/zed/photon.rs`);
Zed's look (notifications, status bar, panel, `Label`/`Button` sizes); GPUI tests for every state.
