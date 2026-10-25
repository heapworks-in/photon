import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";

// Excalidraw falls back to fonts on esm.sh, and an SVG export whose font subsetting fails gets
// that remote URL in its @font-face. Without the fallback a font that can't be embedded is left
// out of the export instead. Each edit must match exactly once, so an Excalidraw upgrade that
// moves this code fails the build rather than shipping the fallback.
const FONT_FALLBACK_EDITS: [string, string][] = [
  [
    'P(jn,"ASSETS_FALLBACK_URL",`https://esm.sh/${M.PKG_NAME?`${M.PKG_NAME}@${M.PKG_VERSION}`:"@excalidraw/excalidraw"}/dist/prod/`);',
    'P(jn,"ASSETS_FALLBACK_URL","");',
  ],
  ["return r.push(new URL(n,jn.ASSETS_FALLBACK_URL)),r}", "return r}"],
  ['this.urls.length?this.urls[this.urls.length-1].toString():""}', '""}'],
  [".then(r=>`@font-face {", ".then(r=>r&&`@font-face {"],
];

function noRemoteFontFallback(): Plugin {
  let edited = 0;
  return {
    name: "photon-no-remote-font-fallback",
    transform(code, id) {
      if (!id.includes("@excalidraw/excalidraw/dist/prod/") || !code.includes("ASSETS_FALLBACK_URL")) {
        return null;
      }
      for (const [from, to] of FONT_FALLBACK_EDITS) {
        const count = code.split(from).length - 1;
        if (count !== 1) {
          throw new Error(`expected one ${JSON.stringify(from)} in ${id}, found ${count}`);
        }
        code = code.replace(from, () => to);
      }
      edited += 1;
      return { code, map: null };
    },
    buildEnd() {
      if (edited !== 1) throw new Error(`edited ${edited} Excalidraw font modules, expected 1`);
    },
  };
}

export default defineConfig({
  plugins: [react(), noRemoteFontFallback()],
  // The page is served from photon-web://excalidraw/editor/, so asset URLs must be relative.
  base: "./",
  build: {
    outDir: "dist/editor",
    emptyOutDir: true,
    sourcemap: false,
    // Writes the licences of every bundled npm package to .vite/license.md.
    license: true,
  },
});
