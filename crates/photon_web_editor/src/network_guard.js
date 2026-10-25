(() => {
  // WebRTC opens UDP connections (ICE/STUN) that CSP connect-src does not cover.
  for (const name of [
    "RTCPeerConnection",
    "webkitRTCPeerConnection",
    "RTCDataChannel",
    "RTCSessionDescription",
    "RTCIceCandidate",
  ]) {
    try {
      delete window[name];
    } catch (_) {}
    try {
      Object.defineProperty(window, name, { value: undefined, writable: false, configurable: false });
    } catch (_) {}
  }

  // Resource hints can resolve or reach hosts outside the CSP's fetch rules.
  const hints = /(^|\s)(dns-prefetch|preconnect|prefetch|prerender)(\s|$)/i;
  const strip = (node) => {
    if (node.nodeType !== 1) return;
    if (node.tagName === "LINK" && hints.test(node.rel || "")) {
      node.remove();
      return;
    }
    for (const link of node.querySelectorAll ? node.querySelectorAll("link") : []) {
      if (hints.test(link.rel || "")) link.remove();
    }
  };
  new window.MutationObserver((records) => {
    for (const record of records) for (const node of record.addedNodes) strip(node);
  }).observe(window.document, { childList: true, subtree: true });
})();
