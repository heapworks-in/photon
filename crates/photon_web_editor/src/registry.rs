use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::Global;

pub enum WebEditorSource {
    Embedded { prefix: String },
    Directory(PathBuf),
}

pub struct WebEditor {
    pub id: Arc<str>,
    pub extension_id: Arc<str>,
    pub name: String,
    pub file_suffixes: Vec<String>,
    pub entry: String,
    pub source: WebEditorSource,
    pub built_in: bool,
}

#[derive(Default)]
pub struct WebEditorRegistry {
    editors: Vec<WebEditor>,
}

impl Global for WebEditorRegistry {}

impl WebEditorRegistry {
    pub fn register(&mut self, editor: WebEditor) {
        self.editors.retain(|existing| existing.id != editor.id);
        self.editors.push(editor);
    }

    pub fn unregister_extension(&mut self, extension_id: &str) {
        self.editors
            .retain(|editor| editor.extension_id.as_ref() != extension_id);
    }

    pub fn editor(&self, id: &str) -> Option<&WebEditor> {
        self.editors.iter().find(|editor| editor.id.as_ref() == id)
    }

    pub fn editor_for_path(&self, path: &Path) -> Option<&WebEditor> {
        let file_name = path.file_name()?.to_string_lossy().to_lowercase();
        self.editors
            .iter()
            .filter_map(|editor| {
                let longest_suffix = editor
                    .file_suffixes
                    .iter()
                    .filter(|suffix| {
                        let suffix = suffix.to_lowercase();
                        file_name
                            .strip_suffix(&suffix)
                            .is_some_and(|stem| stem.ends_with('.'))
                    })
                    .map(|suffix| suffix.len())
                    .max()?;
                Some((longest_suffix, !editor.built_in, editor))
            })
            .max_by_key(|(length, installed, _)| (*length, *installed))
            .map(|(_, _, editor)| editor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(id: &str, extension_id: &str, suffixes: &[&str], built_in: bool) -> WebEditor {
        WebEditor {
            id: id.into(),
            extension_id: extension_id.into(),
            name: id.to_string(),
            file_suffixes: suffixes.iter().map(|suffix| suffix.to_string()).collect(),
            entry: "index.html".to_string(),
            source: WebEditorSource::Embedded {
                prefix: "x".to_string(),
            },
            built_in,
        }
    }

    #[test]
    fn suffixes_match_whole_dotted_names() {
        let mut registry = WebEditorRegistry::default();
        registry.register(editor("draw", "ext", &["excalidraw"], false));
        registry.register(editor("long", "ext", &["excalidraw.svg"], false));

        let id = |path: &str| {
            registry
                .editor_for_path(Path::new(path))
                .map(|editor| editor.id.to_string())
        };
        assert_eq!(id("/a/b.excalidraw"), Some("draw".to_string()));
        assert_eq!(id("/a/b.EXCALIDRAW"), Some("draw".to_string()));
        assert_eq!(id("/a/b.excalidraw.svg"), Some("long".to_string()));
        assert_eq!(id("/a/b.excalidraw.png"), None);
        assert_eq!(id("/a/excalidraw"), None);
        assert_eq!(id("/a/b.excalidraws"), None);
    }

    #[test]
    fn installed_editor_overrides_built_in() {
        let mut registry = WebEditorRegistry::default();
        registry.register(editor("builtin", "photon", &["excalidraw"], true));
        registry.register(editor("installed", "ext", &["excalidraw"], false));
        assert_eq!(
            registry
                .editor_for_path(Path::new("a.excalidraw"))
                .map(|editor| editor.id.to_string()),
            Some("installed".to_string())
        );
        registry.register(editor("late-builtin", "photon", &["excalidraw"], true));
        assert_eq!(
            registry
                .editor_for_path(Path::new("a.excalidraw"))
                .map(|editor| editor.id.to_string()),
            Some("installed".to_string())
        );
    }

    #[test]
    fn unregistering_an_extension_removes_its_editors() {
        let mut registry = WebEditorRegistry::default();
        registry.register(editor("a", "ext", &["aaa"], false));
        registry.register(editor("b", "other", &["bbb"], false));
        registry.unregister_extension("ext");
        assert!(registry.editor("a").is_none());
        assert!(registry.editor_for_path(Path::new("x.aaa")).is_none());
        assert!(registry.editor("b").is_some());
    }
}
