Call hierarchy that works while Java is still loading your project, and says why when it can't.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew update && brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### Call hierarchy

- **Works while Java is loading.** The Java language server answers "nothing here" until it has loaded the project. The panel now asks again as the server finishes its work, so the trace appears without pressing ⌘K ⌘H again.
- **Says when the build can't load.** If Java can't load your `pom.xml` or `build.gradle` (a parent POM, plugin or dependency it can't fetch), the panel shows "Java couldn't load this project's build" and the reason, instead of "No function at the cursor". **Open pom.xml** goes to the error; **Try Again** loads the build again, for example after connecting to your VPN.
- **Recovers by itself.** Fix the build file and Java reloads the project, and the panel traces the function.

### Java

- A changed `pom.xml` or `build.gradle` reloads the project automatically, instead of waiting on a question Photon didn't show.

### Language servers

- Stopping or restarting a language server also stops the processes it started. Before, a restarted Java server could leave its old copy running, using about 1 GB until you restarted your Mac.
