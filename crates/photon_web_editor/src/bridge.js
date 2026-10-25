(() => {
  let listener = null;
  let shortcuts = new Set();

  const keystroke = (event) => {
    const parts = [];
    if (event.ctrlKey) parts.push("ctrl");
    if (event.altKey) parts.push("alt");
    if (event.shiftKey) parts.push("shift");
    if (event.metaKey) parts.push("cmd");
    // With shift held, event.key is the shifted symbol ("{"), so brackets come from the physical key.
    const key =
      event.code === "BracketLeft" ? "[" :
      event.code === "BracketRight" ? "]" :
      event.key.toLowerCase();
    parts.push(key);
    return parts.join("-");
  };

  window.photon = {
    version: 1,
    onMessage(callback) {
      listener = callback;
    },
    send(message) {
      window.ipc.postMessage(JSON.stringify(message));
    },
    // Called by the host with a JSON string.
    _receive(json) {
      const message = JSON.parse(json);
      if (message.type === "load") {
        shortcuts = new Set(message.shortcuts);
      }
      if (listener) listener(message);
    },
  };

  window.addEventListener(
    "keydown",
    (event) => {
      const stroke = keystroke(event);
      if (shortcuts.has(stroke)) {
        event.preventDefault();
        event.stopPropagation();
        window.photon.send({ type: "shortcut", keystroke: stroke });
      }
    },
    true,
  );
})();
