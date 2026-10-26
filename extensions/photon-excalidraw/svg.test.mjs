// Round-trips scenes through the adapter's .excalidraw.svg encoding with the real Excalidraw.
// Run by script/build-photon-excalidraw; it needs node_modules, so cargo's tests can't run it.
import assert from "node:assert/strict";
import { Window } from "happy-dom";
import { build } from "vite";

// Excalidraw renders the SVG with the DOM: happy-dom provides it, as the page's globals.
const window = new Window();
for (const name of Object.getOwnPropertyNames(window)) {
  if (!(name in globalThis)) globalThis[name] = window[name];
}
// happy-dom has no canvas; Excalidraw only measures text with it.
window.HTMLCanvasElement.prototype.getContext = () => ({
  filter: "none",
  font: "",
  measureText: (text) => ({
    width: text.length * 8,
    actualBoundingBoxAscent: 8,
    actualBoundingBoxDescent: 2,
  }),
});
// Nor fonts: Photon's are bundled files, and an export that can't embed one leaves it out.
globalThis.FontFace = class {
  constructor(family, source, descriptors) {
    Object.assign(this, { family, source, ...descriptors });
  }
};
// Excalidraw reads blobs with FileReader, which takes only its own window's Blob and lacks DONE.
window.FileReader.DONE = 2;
for (const name of ["window", "navigator", "Blob"]) {
  Object.defineProperty(globalThis, name, { value: window[name] ?? window, configurable: true });
}
// The editor makes no network calls; neither may its export.
globalThis.fetch = async (url) => {
  throw new Error(`fetch ${url}`);
};

// Bundled like the page (with the project's Vite config), but for node.
const outDir = "dist/svg-test";
await build({
  logLevel: "error",
  ssr: { noExternal: true },
  build: {
    ssr: true,
    rollupOptions: { input: { svg: "src/svg.js", excalidraw: "@excalidraw/excalidraw" } },
    outDir,
    emptyOutDir: true,
    license: false,
  },
});
const { encodeSvg, decodeSvg } = await import(`./${outDir}/svg.js`);
const { convertToExcalidrawElements, restoreAppState, restoreElements } = await import(
  `./${outDir}/excalidraw.js`
);

// Restored, as the editor holds them, so a round trip must give them back unchanged.
const elements = restoreElements(convertToExcalidrawElements([
  { type: "rectangle", id: "box", x: 10, y: 20, width: 100, height: 50, backgroundColor: "#a5d8ff" },
  { type: "ellipse", id: "round", x: 200, y: 20, width: 80, height: 80 },
  { type: "arrow", id: "link", x: 110, y: 45, points: [[0, 0], [90, 15]] },
  { type: "text", id: "label", x: 10, y: 120, text: "Photon" },
 ], { regenerateIds: false }), null, { repairBindings: true, refreshDimensions: false });
const appState = restoreAppState({ viewBackgroundColor: "#fff9db" }, null);

const svg = await encodeSvg(elements, appState, {});
assert.match(svg, /^<svg[^>]*xmlns="http:\/\/www.w3.org\/2000\/svg"/);
assert.match(svg, /<!-- payload-type:application\/vnd.excalidraw\+json -->/);
assert.ok(svg.includes("<path") && svg.includes(">Photon</text>"), "the drawing is rendered, not only embedded");
assert.deepEqual(
  svg.match(/https?:\/\/[^\s"')]+/g).filter((url) => !url.startsWith("http://www.w3.org/")),
  [],
  "the SVG points nowhere remote",
);

// The scene is stored as JSON, which leaves out undefined fields.
const asStored = (value) => JSON.parse(JSON.stringify(value));
const scene = await decodeSvg(svg);
assert.deepEqual(asStored(scene.elements), asStored(elements));
assert.equal(scene.appState.viewBackgroundColor, "#fff9db");

const again = await decodeSvg(await encodeSvg(scene.elements, scene.appState, scene.files));
assert.deepEqual(asStored(again.elements), asStored(elements));

const deleted = elements.map((element) =>
  element.id === "round" ? { ...element, isDeleted: true } : element,
);
const withoutDeleted = await decodeSvg(await encodeSvg(deleted, appState, {}));
assert.deepEqual(withoutDeleted.elements.map((element) => element.id), ["box", "link", "label"]);

const plain =
  '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>';
assert.equal(await decodeSvg(plain), null, "an SVG without a scene has nothing to edit");
await assert.rejects(
  decodeSvg(svg.replace(/(<!-- payload-start -->\s*)\S{8}/, "$1!!!!!!!!")),
  "a damaged scene is an error, not an empty drawing",
);

console.log("svg round trip: ok");
