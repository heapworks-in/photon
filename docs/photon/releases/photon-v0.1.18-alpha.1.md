Java servers that use far less memory with many project tabs, and a call hierarchy that says when Java hasn't loaded your file's module.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew update && brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### Memory with many Java projects

Each project tab runs its own Java language server, and that's where the memory went, not Photon itself.

- **Idle tabs let go.** A project tab you haven't used for 10 minutes has its Java server paused. Show the tab and it starts again from its saved workspace, without indexing the project from scratch. With 10 Java projects open, idle memory for Java went from 10 servers and 1.7 GB to 1 server and 0.6 GB in our test.
- **Each Java server starts small and stops at 2 GB,** like VS Code, instead of starting at 1 GB with no limit. Set `lsp.jdtls.settings.max_memory` to give a large project more.

### Call hierarchy

- **Says when the file's module isn't loaded.** If Java loaded your build but not the module your file is in (for example, a module missing from `settings.gradle`), the panel says so and names the modules it did load, instead of "No function at the cursor". **Try Again** loads the build again, and editing `settings.gradle` or `pom.xml` does it for you.
- Every empty result is written to Photon's log with the file, line and reason, so a report carries the evidence.
