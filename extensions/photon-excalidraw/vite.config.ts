import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // The page is served from photon-web://excalidraw/editor/, so asset URLs must be relative.
  base: "./",
  build: {
    outDir: "dist/editor",
    emptyOutDir: true,
    sourcemap: false,
  },
});
