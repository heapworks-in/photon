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

export function createSession({
  restore,
  serialize,
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

  const sendPending = () => {
    maxWaitTimer = clear(maxWaitTimer);
    if (!pending) return;
    const { elements, appState, files, key } = pending;
    pending = null;
    send({ type: "changed", text: serialize(elements, appState, files) });
    saved = key;
  };

  const fail = (message) => {
    cancel();
    saved = null;
    send({ type: "error", message });
    return null;
  };

  return {
    // Returns the restored scene, or null after reporting an error.
    load(text) {
      cancel();
      saved = null;
      let data = null;
      if (text.trim() !== "") {
        try {
          data = JSON.parse(text);
        } catch (error) {
          return fail(`This file is not valid Excalidraw JSON: ${error.message}`);
        }
        if (!isExcalidrawData(data)) {
          return fail("This file is not an Excalidraw drawing");
        }
      }
      let scene;
      try {
        scene = restore(data);
      } catch (error) {
        return fail(`This Excalidraw drawing could not be read: ${error.message}`);
      }
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
      send({ type: "flushed", request });
    },
  };
}
