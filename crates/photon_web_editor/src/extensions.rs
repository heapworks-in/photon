use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use extension_host::{Event, ExtensionIndexEntry, ExtensionStore};
use gpui::{App, Entity};

use crate::{WebEditor, WebEditorRegistry, WebEditorSource, WebEditorView};

pub fn init(cx: &mut App) {
    // Registered after the text editor, so `WebDocument::try_open` is tried first.
    workspace::register_project_item::<WebEditorView>(cx);
    workspace::register_serializable_item::<WebEditorView>(cx);
    cx.set_global(registry_for(&BTreeMap::default(), Path::new("")));
    if let Some(store) = ExtensionStore::try_global(cx) {
        sync(&store, cx);
        cx.subscribe(&store, |store, event, cx| {
            if matches!(event, Event::ExtensionsUpdated) {
                sync(&store, cx);
            }
        })
        .detach();
    }
}

fn sync(store: &Entity<ExtensionStore>, cx: &mut App) {
    let installed_dir = paths::extensions_dir().join("installed");
    let registry = registry_for(store.read(cx).installed_extensions(), &installed_dir);
    cx.set_global(registry);
}

fn built_in_editors() -> Vec<WebEditor> {
    vec![WebEditor {
        id: "excalidraw".into(),
        extension_id: "photon-excalidraw".into(),
        name: "Excalidraw".to_string(),
        file_suffixes: vec!["excalidraw".to_string(), "excalidraw.svg".to_string()],
        entry: "editor/index.html".to_string(),
        source: WebEditorSource::Embedded {
            prefix: "web_editors/excalidraw/".to_string(),
        },
        built_in: true,
    }]
}

// Rebuilt from scratch so an installed editor that reuses a built-in's id shadows it only while installed.
fn registry_for(
    extensions: &BTreeMap<Arc<str>, ExtensionIndexEntry>,
    installed_dir: &Path,
) -> WebEditorRegistry {
    let mut registry = WebEditorRegistry::default();
    for editor in built_in_editors() {
        registry.register(editor);
    }
    for (extension_id, extension) in extensions {
        for (editor_id, editor) in &extension.manifest.web_editors {
            registry.register(WebEditor {
                id: editor_id.clone(),
                extension_id: extension_id.clone(),
                name: editor.name.clone(),
                file_suffixes: editor.file_suffixes.clone(),
                entry: editor.entry.as_unix_str().to_string(),
                source: WebEditorSource::Directory(installed_dir.join(extension_id.as_ref())),
                built_in: false,
            });
        }
    }
    registry
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use extension::ExtensionManifest;

    use super::*;

    fn index_with(
        extension_id: &str,
        editor_id: &str,
        suffix: &str,
    ) -> BTreeMap<Arc<str>, ExtensionIndexEntry> {
        let manifest: ExtensionManifest = serde_json::from_value(serde_json::json!({
            "id": extension_id,
            "name": extension_id,
            "version": "0.0.1",
            "schema_version": 1,
            "web_editors": {
                editor_id: {
                    "name": "Draw",
                    "file_suffixes": [suffix],
                    "entry": "editor/index.html",
                },
            },
        }))
        .expect("manifest parses");
        BTreeMap::from_iter([(
            Arc::from(extension_id),
            ExtensionIndexEntry {
                manifest: Arc::new(manifest),
                dev: false,
            },
        )])
    }

    fn installed_dir() -> PathBuf {
        PathBuf::from("/extensions/installed")
    }

    #[test]
    fn installed_extension_registers_its_editors() {
        let registry = registry_for(&index_with("draw-ext", "draw", "draw"), &installed_dir());

        let editor = registry
            .editor_for_path(Path::new("/a/b.draw"))
            .expect("the extension's editor is registered");
        assert_eq!(editor.id.as_ref(), "draw");
        assert_eq!(editor.extension_id.as_ref(), "draw-ext");
        assert_eq!(editor.name, "Draw");
        assert_eq!(editor.entry, "editor/index.html");
        assert!(!editor.built_in);
        assert!(matches!(
            &editor.source,
            WebEditorSource::Directory(path) if *path == installed_dir().join("draw-ext")
        ));

        let built_in = registry
            .editor_for_path(Path::new("/a/b.excalidraw"))
            .expect("the built-in stays registered");
        assert_eq!(built_in.id.as_ref(), "excalidraw");
        assert!(built_in.built_in);
    }

    #[test]
    fn uninstalled_extension_unregisters_them() {
        let installed = registry_for(
            &index_with("my-excalidraw", "excalidraw", "excalidraw"),
            &installed_dir(),
        );
        let shadowing = installed
            .editor("excalidraw")
            .expect("the installed editor is registered");
        assert!(
            !shadowing.built_in,
            "an installed editor shadows the built-in"
        );

        let uninstalled = registry_for(&BTreeMap::default(), &installed_dir());
        let built_in = uninstalled
            .editor("excalidraw")
            .expect("the built-in comes back");
        assert!(built_in.built_in);

        let installed = registry_for(&index_with("draw-ext", "draw", "draw"), &installed_dir());
        assert!(installed.editor("draw").is_some());
        assert!(uninstalled.editor("draw").is_none());
        assert!(
            uninstalled
                .editor_for_path(Path::new("/a/b.draw"))
                .is_none()
        );
    }

    #[test]
    fn excalidraw_svgs_open_in_excalidraw_and_other_svgs_do_not() {
        let registry = registry_for(&BTreeMap::default(), &installed_dir());
        let id = |path: &str| {
            registry
                .editor_for_path(Path::new(path))
                .map(|editor| editor.id.to_string())
        };
        assert_eq!(
            id("/docs/arch.excalidraw.svg"),
            Some("excalidraw".to_string())
        );
        assert_eq!(
            id("/docs/Arch.Excalidraw.SVG"),
            Some("excalidraw".to_string())
        );
        assert_eq!(id("/docs/arch.excalidraw"), Some("excalidraw".to_string()));
        assert_eq!(id("/docs/logo.svg"), None);
        assert_eq!(id("/docs/excalidraw.svg"), None);
        assert_eq!(id("/docs/arch.excalidraw.svg.bak"), None);

        // An installed editor for plain SVGs doesn't take the drawings, being the shorter match.
        let with_svg_editor = registry_for(&index_with("svg-ext", "svg", "svg"), &installed_dir());
        let id = |path: &str| {
            with_svg_editor
                .editor_for_path(Path::new(path))
                .map(|editor| editor.id.to_string())
        };
        assert_eq!(
            id("/docs/arch.excalidraw.svg"),
            Some("excalidraw".to_string())
        );
        assert_eq!(id("/docs/logo.svg"), Some("svg".to_string()));
    }

    #[test]
    fn excalidraw_extension_manifest_declares_the_editor() {
        let manifest: ExtensionManifest = toml::from_str(include_str!(
            "../../../extensions/photon-excalidraw/extension.toml"
        ))
        .expect("extension.toml parses");
        let built_in = built_in_editors()
            .into_iter()
            .next()
            .expect("a built-in editor");
        assert_eq!(manifest.id.as_ref(), built_in.extension_id.as_ref());
        let declared = &manifest.web_editors[built_in.id.as_ref()];
        assert_eq!(declared.name, built_in.name);
        assert_eq!(declared.file_suffixes, built_in.file_suffixes);
        assert_eq!(declared.entry.as_unix_str(), built_in.entry);
    }
}
