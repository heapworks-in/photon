import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import {
  CaptureUpdateAction,
  Excalidraw,
  MainMenu,
  getSceneVersion,
  restore,
  restoreLibraryItems,
  serializeAsJSON,
  serializeLibraryAsJSON,
} from "@excalidraw/excalidraw";
import "@excalidraw/excalidraw/index.css";
import "./photon.css";
import {
  BUNDLED_ID_PREFIX,
  createSession,
  isBundledLibraryItem,
} from "./session.js";

type Api = any;
type LibraryItem = any;

const photon = (window as any).photon;
const BASE = "photon-web://excalidraw/";

// Online features are removed from the UI, not just blocked: no export to link (no backend
// callback is passed), no collaboration, no AI, no web embeds, no theme toggle (Photon owns it),
// and Photon owns the file, so no open or save-as.
export const UI_OPTIONS = {
  canvasActions: {
    loadScene: false,
    saveToActiveFile: false,
    export: false as const,
    toggleTheme: false,
    saveAsImage: true,
    clearCanvas: true,
    changeViewBackgroundColor: true,
  },
};

let api: Api | null = null;
let readySent = false;
let userItems: LibraryItem[] = [];
let userLibraryText = serializeLibraryAsJSON([]);
let bundledItems: Promise<LibraryItem[]> | null = null;
let setTheme: (theme: "light" | "dark") => void = () => {};
let setFileName: (name: string) => void = () => {};
let setError: (message: string | null) => void = () => {};

const session = createSession({
  restore: (data: any) =>
    restore(data, null, null, { repairBindings: true, refreshDimensions: false }),
  serialize: (elements: any, appState: any, files: any) =>
    serializeAsJSON(elements, appState, files, "local"),
  sceneVersion: getSceneVersion,
  send: (message: any) => {
    if (message.type === "error") setError(message.message);
    photon.send(message);
  },
});

function libraryItemsOf(data: any) {
  return data.libraryItems ?? data.library ?? [];
}

async function loadBundledLibraries(): Promise<LibraryItem[]> {
  const index = await (await fetch(`${BASE}libraries/index.json`)).json();
  const libraries = await Promise.all(
    index.map(async (library: { name: string; file: string }) => {
      const data = await (await fetch(`${BASE}libraries/${library.file}`)).json();
      return restoreLibraryItems(libraryItemsOf(data), "published").map((item: LibraryItem) => ({
        ...item,
        // Prefixed so the user's own items can be told apart when the library is saved.
        id: `${BUNDLED_ID_PREFIX}${library.file}:${item.id}`,
        name: item.name ? `${library.name}: ${item.name}` : library.name,
      }));
    }),
  );
  return libraries.flat();
}

async function applyLibrary() {
  bundledItems ??= loadBundledLibraries().catch((error) => {
    console.error("bundled Excalidraw libraries failed to load", error);
    return [];
  });
  const bundled = await bundledItems;
  await api?.updateLibrary({ libraryItems: [...userItems, ...bundled], merge: false });
}

function onLibraryChange(items: readonly LibraryItem[]) {
  const text = serializeLibraryAsJSON(items.filter((item) => !isBundledLibraryItem(item)));
  if (text !== userLibraryText) {
    userLibraryText = text;
    photon.send({ type: "library_changed", text });
  }
}

function load(text: string, fileName: string) {
  setFileName(fileName.replace(/\.excalidraw$/i, ""));
  const scene = session.load(text);
  if (!scene) {
    api?.updateScene({ elements: [], captureUpdate: CaptureUpdateAction.NEVER });
    return;
  }
  setError(null);
  const { viewBackgroundColor, gridSize, gridStep, gridModeEnabled } = scene.appState;
  api?.updateScene({
    elements: scene.elements,
    appState: { viewBackgroundColor, gridSize, gridStep, gridModeEnabled },
    captureUpdate: CaptureUpdateAction.NEVER,
  });
  api?.addFiles(Object.values(scene.files));
  api?.history.clear();
  api?.scrollToContent(undefined, { animate: false });
  applyLibrary();
}

photon.onMessage((message: any) => {
  switch (message.type) {
    case "load":
      load(message.text, message.file_name);
      break;
    case "theme":
      setTheme(message.appearance === "dark" ? "dark" : "light");
      break;
    case "flush":
      session.flush(message.request);
      break;
    case "library":
      try {
        userItems = restoreLibraryItems(libraryItemsOf(JSON.parse(message.text)), "unpublished");
      } catch (error) {
        console.error("saved Excalidraw library is unreadable", error);
        return;
      }
      userLibraryText = serializeLibraryAsJSON(userItems);
      applyLibrary();
      break;
  }
});

function App() {
  const [theme, setThemeState] = useState<"light" | "dark">("light");
  const [fileName, setFileNameState] = useState("");
  const [error, setErrorState] = useState<string | null>(null);

  // Assigned during render so they exist before Excalidraw mounts and the first message arrives.
  setTheme = setThemeState;
  setFileName = setFileNameState;
  setError = setErrorState;

  return (
    <div className="photon-editor">
      <Excalidraw
        excalidrawAPI={(instance: Api) => {
          api = instance;
          if (!readySent) {
            readySent = true;
            photon.send({ type: "ready", version: 1 });
          }
        }}
        onChange={(elements, appState, files) => session.change(elements, appState, files)}
        onLibraryChange={onLibraryChange}
        theme={theme}
        name={fileName}
        viewModeEnabled={error !== null}
        UIOptions={UI_OPTIONS}
        aiEnabled={false}
        isCollaborating={false}
        validateEmbeddable={() => false}
        libraryReturnUrl={window.location.href}
        onLinkOpen={(_element, event) => event.preventDefault()}
        autoFocus
      >
        <MainMenu>
          <MainMenu.DefaultItems.SaveAsImage />
          <MainMenu.DefaultItems.SearchMenu />
          <MainMenu.DefaultItems.Help />
          <MainMenu.DefaultItems.ClearCanvas />
          <MainMenu.Separator />
          <MainMenu.DefaultItems.ChangeCanvasBackground />
        </MainMenu>
      </Excalidraw>
      {error !== null && <div className="photon-error">{error}</div>}
    </div>
  );
}

const root = document.getElementById("root");
if (root) createRoot(root).render(<App />);
