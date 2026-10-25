use std::borrow::Cow;
use std::path::{Component, Path};

use gpui::AssetSource;

use crate::{WebEditor, WebEditorSource};

pub const SCHEME: &str = "photon-web";

// Pages may fetch their own files (Excalidraw reads its libraries and, to embed them in SVG exports,
// its fonts); requests for another editor's files are refused by the scheme handler. WebAssembly
// is allowed (Excalidraw subsets fonts with it) but `eval` is not.
pub const CSP: &str = "default-src photon-web: 'unsafe-inline'; script-src photon-web: 'unsafe-inline' 'wasm-unsafe-eval'; img-src photon-web: data: blob:; font-src photon-web: data:; connect-src photon-web:";

pub struct Served {
    pub bytes: Cow<'static, [u8]>,
    pub mime: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("path is outside the editor folder")]
    OutsideEditor,
    #[error("file not found")]
    NotFound,
    #[error("could not read file: {0}")]
    Io(#[from] std::io::Error),
}

/// `request_path` is relative to the editor folder, without a leading slash.
pub fn serve(
    editor: &WebEditor,
    request_path: &str,
    assets: &dyn AssetSource,
) -> Result<Served, ServeError> {
    let decoded = urlencoding::decode(request_path).map_err(|_| ServeError::OutsideEditor)?;
    let relative = Path::new(decoded.as_ref());
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(ServeError::OutsideEditor);
    }

    let bytes = match &editor.source {
        WebEditorSource::Embedded { prefix } => {
            let path = format!("{}/{}", prefix.trim_end_matches('/'), decoded);
            assets
                .load(&path)
                .map_err(|_| ServeError::NotFound)?
                .ok_or(ServeError::NotFound)?
        }
        WebEditorSource::Directory(root) => {
            let root = root.canonicalize()?;
            let path = match root.join(relative).canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ServeError::NotFound);
                }
                Err(error) => return Err(error.into()),
            };
            if !path.starts_with(&root) {
                return Err(ServeError::OutsideEditor);
            }
            Cow::Owned(std::fs::read(path)?)
        }
    };

    Ok(Served {
        bytes,
        mime: mime_type(relative),
    })
}

fn mime_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase());
    match extension.as_deref() {
        Some("html") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_allows_only_photon_web_and_inline_data() {
        for directive in CSP.split(';') {
            for source in directive.split_whitespace().skip(1) {
                assert!(
                    [
                        "photon-web:",
                        "'unsafe-inline'",
                        "'wasm-unsafe-eval'",
                        "data:",
                        "blob:"
                    ]
                    .contains(&source),
                    "{directive}"
                );
            }
        }
        assert!(CSP.contains("connect-src photon-web:"));
        // Excalidraw subsets fonts for SVG export with WebAssembly; plain eval stays blocked.
        assert!(CSP.contains("script-src photon-web: 'unsafe-inline' 'wasm-unsafe-eval'"));
        assert!(!CSP.contains("'unsafe-eval'"));
    }
    use crate::{WebEditor, WebEditorSource};
    use gpui::SharedString;
    use std::borrow::Cow;

    fn directory_editor(path: &std::path::Path) -> WebEditor {
        WebEditor {
            id: "e".into(),
            extension_id: "ext".into(),
            name: "E".to_string(),
            file_suffixes: vec![],
            entry: "index.html".to_string(),
            source: WebEditorSource::Directory(path.to_path_buf()),
            built_in: false,
        }
    }

    #[test]
    fn serves_files_inside_the_editor_folder() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("fonts"))?;
        std::fs::write(dir.path().join("index.html"), "<html>")?;
        std::fs::write(dir.path().join("fonts/x.woff2"), "f")?;
        std::fs::write(dir.path().join("app.js"), "1")?;
        std::fs::write(dir.path().join("blob.bin"), "1")?;
        let editor = directory_editor(dir.path());

        let served = serve(&editor, "index.html", &())?;
        assert_eq!(served.mime, "text/html");
        assert_eq!(served.bytes.as_ref(), b"<html>");
        assert_eq!(serve(&editor, "fonts/x.woff2", &())?.mime, "font/woff2");
        assert_eq!(serve(&editor, "app.js", &())?.mime, "text/javascript");
        assert_eq!(
            serve(&editor, "blob.bin", &())?.mime,
            "application/octet-stream"
        );
        assert!(matches!(
            serve(&editor, "missing.js", &()),
            Err(ServeError::NotFound)
        ));
        Ok(())
    }

    #[test]
    fn refuses_paths_outside_the_folder() -> anyhow::Result<()> {
        let outer = tempfile::tempdir()?;
        let inner = outer.path().join("editor");
        std::fs::create_dir(&inner)?;
        std::fs::write(outer.path().join("secret"), "s")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(outer.path().join("secret"), inner.join("link"))?;
        let editor = directory_editor(&inner);

        let mut attempts = vec![
            "../secret",
            "/etc/passwd",
            "%2e%2e/secret",
            "a/../../secret",
        ];
        if cfg!(unix) {
            attempts.push("link");
        }
        for attempt in attempts {
            assert!(
                matches!(serve(&editor, attempt, &()), Err(ServeError::OutsideEditor)),
                "{attempt} should be refused"
            );
        }
        Ok(())
    }

    struct FakeAssets;

    impl gpui::AssetSource for FakeAssets {
        fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
            Ok((path == "editors/draw/index.html").then(|| Cow::Borrowed(&b"<p>"[..])))
        }

        fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
            Ok(vec![])
        }
    }

    #[test]
    fn serves_embedded_built_ins() -> anyhow::Result<()> {
        let mut editor = directory_editor(std::path::Path::new("/unused"));
        editor.source = WebEditorSource::Embedded {
            prefix: "editors/draw".to_string(),
        };
        let served = serve(&editor, "index.html", &FakeAssets)?;
        assert_eq!(served.mime, "text/html");
        assert_eq!(served.bytes.as_ref(), b"<p>");
        assert!(matches!(
            serve(&editor, "nope.js", &FakeAssets),
            Err(ServeError::NotFound)
        ));
        assert!(matches!(
            serve(&editor, "../x", &FakeAssets),
            Err(ServeError::OutsideEditor)
        ));
        Ok(())
    }
}
