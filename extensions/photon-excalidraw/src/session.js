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

export function createSession({
  restore,
  serialize,
  sceneVersion,
  send,
  debounceMs = 300,
  timers = globalThis,
}) {
  // Key of the scene as loaded or last sent; null while nothing editable is loaded, so changes are ignored.
  let saved = null;
  let pending = null;
  let timer = null;

  // These are saved in the file but changing them does not bump the scene version.
  const keyOf = (elements, appState) =>
    JSON.stringify([
      sceneVersion(elements),
      appState?.viewBackgroundColor,
      appState?.gridModeEnabled,
      appState?.gridSize,
      appState?.gridStep,
    ]);

  const cancel = () => {
    if (timer !== null) timers.clearTimeout(timer);
    timer = null;
    pending = null;
  };

  const sendPending = () => {
    if (!pending) return;
    const { elements, appState, files, key } = pending;
    cancel();
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
      if (timer !== null) timers.clearTimeout(timer);
      timer = timers.setTimeout(sendPending, debounceMs);
    },

    flush(request) {
      sendPending();
      send({ type: "flushed", request });
    },
  };
}
