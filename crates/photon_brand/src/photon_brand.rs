pub const DEDICATION: &str = "Dedicated to Maha & Bhumi.";

/// Base background (Warp Dark). Bhumi: the ground everything rests on.
pub const BHUMI: u32 = 0x050505;
/// Accent (Warp Dark). Maha: the color that stands out.
pub const MAHA: u32 = 0x19AAD8;
/// Photon Black's palette is Tokyo Night's, on `BHUMI`.
pub const FOREGROUND: u32 = 0xC0CAF5;

/// Black, red, green, yellow, blue, magenta, cyan, white.
pub const ANSI_NORMAL: [u32; 8] = [
    0x15161E, 0xF7768E, 0x9ECE6A, 0xE0AF68, 0x7AA2F7, 0xBB9AF7, 0x7DCFFF, 0xA9B1D6,
];
pub const ANSI_BRIGHT: [u32; 8] = [
    0x414868, 0xFF899D, 0x9FE044, 0xFABA4A, 0x8DB0FF, 0xC7A9FF, 0xA4DAFF, 0xC0CAF5,
];

/// WCAG 2.x contrast ratio between two opaque `0xRRGGBB` colors.
pub fn contrast_ratio(foreground: u32, background: u32) -> f32 {
    let (lighter, darker) = {
        let first = relative_luminance(foreground);
        let second = relative_luminance(background);
        (first.max(second), first.min(second))
    };
    (lighter + 0.05) / (darker + 0.05)
}

fn relative_luminance(color: u32) -> f32 {
    let channel = |shift: u32| {
        let value = ((color >> shift) & 0xFF) as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_matches_wcag_reference() {
        assert!((contrast_ratio(0xFFFFFF, 0x000000) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(0x616161, BHUMI) - 3.3).abs() < 0.05);
        assert!((contrast_ratio(MAHA, BHUMI) - 7.6).abs() < 0.05);
    }

    #[test]
    fn text_colors_pass_aa_on_bhumi() {
        // ANSI black is a background color; bright black is the dim-text slot (shell
        // autosuggestions), deliberately low contrast.
        for color in [FOREGROUND, MAHA]
            .iter()
            .chain(&ANSI_NORMAL[1..])
            .chain(&ANSI_BRIGHT[1..])
        {
            assert!(contrast_ratio(*color, BHUMI) >= 4.5, "{color:06X}");
        }
    }

    /// Text users read says Photon. This scans the places it lives (UI string literals, settings
    /// descriptions, comments in the settings and keymap files users open) and fails on "Zed"
    /// that isn't one of the allowed uses.
    #[test]
    fn user_facing_text_says_photon_not_zed() {
        // Zed's own services (kept off in Photon), identifiers users type or that are stored,
        // attribution, and Photon's own hook comments.
        const ALLOWED: &[&str] = &[
            "photon:",
            "zed.dev",
            "Based on Zed",
            "Zed Pro",
            "Zed Business",
            "Zed Student",
            "Zed Free",
            "Zed AI",
            "Zed account",
            "Zed Account",
            "Zed Predict",
            "Zed's Edit Predictions",
            "Zed Edit Predictions",
            "Zeta",
            "Zed server",
            "Zed team",
            "Zed's hosted",
            "Zed-hosted",
            "Zed models",
            "signed in to Zed",
            "Sign in to Zed",
            "Zed rejected",
            "Zed Keybind Context",
            "Zed Default",
            "Zed (Default)",
            ".ZedSans",
            ".ZedMono",
            "\"Zed\"",
            "Zed.exe",
            "Zed.dmg",
            "Zed.app",
            "Zed Industries",
            "Zed Agent\")",
            "ZedPredictModal",
            "{zed,Zed}",
            "in Zed agent",
            "the Zed agent",
            "Zed VIP",
            "Zed's edit predictions",
            "Fast Mode for Zed",
            "whether Zed may collect",
            "IconName::Zed",
            "\"Zed/{}",
            "sharing a project in Zed",
            "Zed Plex",
            "Zed X Copilot",
            "Zed Extension CLI",
            "Plan::Zed",
            "GPU Zed is running on",
        ];
        const SKIPPED_CRATES: &[&str] = &[
            "collab",
            "docs_preprocessor",
            "eval_cli",
            "eval",
            "benchmarks",
            "extension_cli",
            "auto_update_helper",
            "windows_resources",
            "etw_tracing",
            "explorer_command_injector",
            "gpui_windows",
            "gpui",
            "agent/src/tools/evals",
        ];
        const USER_TEXT: &[&str] = &[
            "Label::new(\"",
            "Headline::new(\"",
            "Tooltip::text(\"",
            "description: \"",
            ".label(\"",
            "title: \"",
            "Notification::new(\"",
            "ListBulletItem::new(\"",
            "Button::new(\"",
            ".placeholder(\"",
            "detail: \"",
            "message: \"",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let allowed = |line: &str| {
            let mut rest = line.to_string();
            for allowed in ALLOWED {
                rest = rest.replace(allowed, "");
            }
            !rest.contains("Zed")
        };
        let mut found = Vec::new();

        let mut stack = vec![root.join("crates")];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory)
                .expect("read crates")
                .flatten()
            {
                let path = entry.path();
                let relative = path
                    .strip_prefix(root.join("crates"))
                    .expect("inside crates");
                let relative = relative.to_string_lossy();
                if SKIPPED_CRATES.iter().any(|skipped| {
                    relative == *skipped || relative.starts_with(&format!("{skipped}/"))
                }) || relative.contains("/tests/")
                    || relative.contains("/benches/")
                    || relative.ends_with("_tests.rs")
                    || relative.contains("test_fixtures")
                {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|extension| extension != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source");
                // Test modules sit at the end of a file in this codebase.
                let shipped = text.split("#[cfg(test)]").next().unwrap_or_default();
                // A string on its own line is user text unless the line before starts a log
                // message or an internal error.
                const NOT_SHOWN: &[&str] = &[
                    "log::",
                    "debug!(",
                    "info!(",
                    "warn!(",
                    "error!(",
                    "trace!(",
                    "eprintln!(",
                    "println!(",
                    "log!(",
                    "anyhow!(",
                    "bail!(",
                    "context(",
                    "with_context(",
                    "panic!(",
                    "expect(",
                    "assert",
                    "Err(",
                    "#[doc",
                    "telemetry::event!(",
                ];
                let mut previous: Vec<&str> = Vec::new();
                for (number, line) in shipped.lines().enumerate() {
                    // Photon's own hook comments name Zed on purpose.
                    let line = line.split("// photon:").next().unwrap_or_default();
                    let bare_string = line.trim_start().starts_with('"')
                        && !previous
                            .iter()
                            .any(|line| NOT_SHOWN.iter().any(|marker| line.contains(marker)));
                    if !line.trim().is_empty() {
                        previous.push(line);
                        if previous.len() > 3 {
                            previous.remove(0);
                        }
                    }
                    let user_text = bare_string || USER_TEXT.iter().any(|call| line.contains(call));
                    if user_text && line.contains("Zed") && !allowed(line) {
                        found.push(format!("crates/{relative}:{}: {}", number + 1, line.trim()));
                    }
                }
            }
        }

        let mut files = vec![root.join("assets/settings/default.json")];
        for directory in [
            "assets/keymaps",
            "assets/keymaps/macos",
            "assets/keymaps/linux",
        ] {
            for entry in std::fs::read_dir(root.join(directory))
                .expect("read keymaps")
                .flatten()
            {
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    files.push(entry.path());
                }
            }
        }
        for path in files {
            let text = std::fs::read_to_string(&path).expect("read json");
            for (number, line) in text.lines().enumerate() {
                if let Some((_, comment)) = line.split_once("//")
                    && comment.contains("Zed")
                    && !comment.contains("photon:")
                    && !allowed(comment)
                {
                    found.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
        assert!(
            found.is_empty(),
            "user-facing text names Zed:\n{}",
            found.join("\n")
        );
    }
}
