import {
  MIME_TYPES,
  exportToSvg,
  getNonDeletedElements,
  loadFromBlob,
} from "@excalidraw/excalidraw";

// The scene is embedded in the SVG's metadata, as Excalidraw's "Embed scene" export does, so the
// file is both an image and an editable drawing.
export async function encodeSvg(elements, appState, files) {
  const svg = await exportToSvg({
    elements: getNonDeletedElements(elements),
    appState: { ...appState, exportEmbedScene: true },
    files,
  });
  return svg.outerHTML;
}

// Returns the restored scene, or null when the SVG has none embedded.
export async function decodeSvg(text) {
  try {
    return await loadFromBlob(new Blob([text], { type: MIME_TYPES.svg }), null, null);
  } catch (error) {
    if (error?.code === "IMAGE_NOT_CONTAINS_SCENE_DATA") return null;
    throw error;
  }
}
