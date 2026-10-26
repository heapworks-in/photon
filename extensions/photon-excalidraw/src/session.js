// Kept free of Excalidraw, React and the DOM so Photon's tests can run it in node.

export const BUNDLED_ID_PREFIX = "photon-bundled:";

// The same check Excalidraw applies when it opens a file.
export function isExcalidrawData(data) {
  return (
    data !== null &&
    typeof data === "object" &&
    data.type === "excalidraw" &&
    (data.elements === undefined || Array.isArray(data.elements)) &&
    (data.appState === undefined || (data.appState !== null && typeof data.appState === "object"))
  );
}

export function isBundledLibraryItem(item) {
  return typeof item.id === "string" && item.id.startsWith(BUNDLED_ID_PREFIX);
}

// Photon's own toasts draw under the web view, so its confirmations are shown as Excalidraw's.
export function showNotice(api, message) {
  api?.setToast({ message, closable: true, duration: 3000 });
}

// `decodeSvg` and `serializeSvg` read and write .excalidraw.svg files: an image with the scene
// embedded. Both are async, so every send goes through one queue to keep them in order.
export function createSession({
  restore,
  serialize,
  decodeSvg,
  serializeSvg,
  sceneVersion,
  send,
  debounceMs = 300,
  maxWaitMs = 1000,
  timers = globalThis,
}) {
  // Key of the scene as loaded or last sent; null while nothing editable is loaded, so changes are ignored.
  let saved = null;
  let pending = null;
  // Set while edits are coming in; the first change of a burst is sent at once so the tab turns
  // dirty before a quick close, and the rest wait for a pause (or the max wait).
  let quietTimer = null;
  let maxWaitTimer = null;
  let svg = false;
  // Bumped by each load, so a scene still being read or written for an older load is dropped.
  let generation = 0;
  let queue = Promise.resolve();

  const enqueue = (task) => {
    // Caught so one failed task doesn't stop every later send.
    queue = queue.then(task).catch((error) => console.error("Excalidraw session", error));
    return queue;
  };

  // These are saved in the file but changing them does not bump the scene version.
  const keyOf = (elements, appState) =>
    JSON.stringify([
      sceneVersion(elements),
      appState?.viewBackgroundColor,
      appState?.gridModeEnabled,
      appState?.gridSize,
      appState?.gridStep,
    ]);

  const clear = (timer) => {
    if (timer !== null) timers.clearTimeout(timer);
    return null;
  };

  const cancel = () => {
    quietTimer = clear(quietTimer);
    maxWaitTimer = clear(maxWaitTimer);
    pending = null;
  };

  const fail = (message) => {
    cancel();
    saved = null;
    send({ type: "error", message });
    return null;
  };

  const sendPending = () => {
    maxWaitTimer = clear(maxWaitTimer);
    if (!pending) return;
    const { elements, appState, files, key } = pending;
    pending = null;
    saved = key;
    const loaded = generation;
    const write = svg ? serializeSvg : serialize;
    enqueue(async () => {
      let text;
      try {
        text = await write(elements, appState, files);
      } catch (error) {
        if (loaded === generation) fail(`This drawing could not be saved: ${error.message}`);
        return;
      }
      if (loaded === generation) send({ type: "changed", text });
    });
  };

  // Throws errors whose message is shown to the user.
  const read = async (text) => {
    if (text.trim() === "") return restore(null);
    const unreadable = (error) =>
      new Error(`This Excalidraw drawing could not be read: ${error.message}`);
    if (svg) {
      let scene;
      try {
        scene = await decodeSvg(text);
      } catch (error) {
        throw unreadable(error);
      }
      if (!scene) throw new Error("This SVG has no Excalidraw scene to edit");
      return scene;
    }
    let data;
    try {
      data = JSON.parse(text);
    } catch (error) {
      throw new Error(`This file is not valid Excalidraw JSON: ${error.message}`);
    }
    if (!isExcalidrawData(data)) {
      throw new Error("This file is not an Excalidraw drawing");
    }
    try {
      return restore(data);
    } catch (error) {
      throw unreadable(error);
    }
  };

  return {
    // Resolves to the restored scene, null after reporting an error, or undefined when a later
    // load replaced this one.
    async load(text, fileName = "") {
      cancel();
      saved = null;
      svg = /\.svg$/i.test(fileName);
      const loading = ++generation;
      let scene;
      try {
        scene = await read(text);
      } catch (error) {
        return loading === generation ? fail(error.message) : undefined;
      }
      if (loading !== generation) return undefined;
      saved = keyOf(scene.elements, scene.appState);
      return scene;
    },

    change(elements, appState, files) {
      if (saved === null) return;
      const key = keyOf(elements, appState);
      if (key === saved) {
        cancel();
        return;
      }
      pending = { elements, appState, files, key };
      const inBurst = quietTimer !== null;
      clear(quietTimer);
      quietTimer = timers.setTimeout(() => {
        quietTimer = null;
        sendPending();
      }, debounceMs);
      if (!inBurst) {
        sendPending();
      } else if (maxWaitTimer === null) {
        maxWaitTimer = timers.setTimeout(sendPending, maxWaitMs);
      }
    },

    flush(request) {
      sendPending();
      return enqueue(() => send({ type: "flushed", request }));
    },
  };
}
