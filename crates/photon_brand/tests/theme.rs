use photon_brand::{ANSI_BRIGHT, ANSI_NORMAL, BHUMI, FOREGROUND, contrast_ratio};
use serde_json::Value;

const ANSI_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

const THEME_NAMES: [&str; 4] = [
    "Photon Black",
    "Photon Mocha",
    "Photon Rosé",
    "Photon Vesper",
];

/// Parsed strictly, the way the bundled theme loader reads it.
fn photon_themes() -> Vec<Value> {
    let family: Value =
        serde_json::from_str(include_str!("../../../assets/themes/photon/photon.json"))
            .expect("photon.json parses");
    family["themes"].as_array().expect("themes array").clone()
}

fn theme(name: &str) -> Value {
    photon_themes()
        .into_iter()
        .find(|theme| theme["name"] == name)
        .unwrap_or_else(|| panic!("{name} theme"))
}

/// `#rrggbb` or `#rrggbbaa` -> `0xrrggbb`, ignoring alpha.
fn hex(value: &Value) -> u32 {
    let text = value
        .as_str()
        .expect("color string")
        .trim_start_matches('#');
    u32::from_str_radix(&text[..6], 16).expect("hex color")
}

#[test]
fn four_dark_themes_share_photons_black_chrome() {
    let themes = photon_themes();
    let names: Vec<&str> = themes
        .iter()
        .map(|theme| theme["name"].as_str().expect("theme name"))
        .collect();
    assert_eq!(names, THEME_NAMES);

    let chrome = |theme: &Value| {
        [
            "background",
            "editor.background",
            "terminal.background",
            "panel.background",
            "surface.background",
            "elevated_surface.background",
            "element.background",
            "element.selected",
            "border",
            "border.variant",
        ]
        .map(|key| theme["style"][key].clone())
    };
    let black_chrome = chrome(&themes[0]);
    for theme in &themes {
        assert_eq!(theme["appearance"], "dark");
        assert_eq!(
            hex(&theme["style"]["background"]),
            BHUMI,
            "{}",
            theme["name"]
        );
        assert_eq!(chrome(theme), black_chrome, "{}", theme["name"]);
    }

    let black = theme("Photon Black");
    assert_eq!(hex(&black["style"]["syntax"]["string"]["color"]), 0x9ECE6A);
}

#[test]
fn photon_black_matches_brand() {
    let style = &theme("Photon Black")["style"];
    assert_eq!(hex(&style["background"]), BHUMI);
    assert_eq!(hex(&style["editor.background"]), BHUMI);
    assert_eq!(hex(&style["terminal.background"]), BHUMI);
    assert_eq!(hex(&style["text"]), FOREGROUND);
    for (index, name) in ANSI_NAMES.iter().enumerate() {
        assert_eq!(
            hex(&style[format!("terminal.ansi.{name}")]),
            ANSI_NORMAL[index],
            "{name}"
        );
        assert_eq!(
            hex(&style[format!("terminal.ansi.bright_{name}")]),
            ANSI_BRIGHT[index],
            "bright_{name}"
        );
    }
}

#[test]
fn syntax_and_text_pass_aa() {
    // The origin palettes keep comments, ghost text and muted labels dim on purpose; they only
    // need to stay legible (3:1), everything else is body text (4.5:1).
    let dim = [
        "comment",
        "comment.doc",
        "predictive",
        "text.muted",
        "text.placeholder",
    ];
    let minimum = |name: &str| if dim.contains(&name) { 3.0 } else { 4.5 };
    for theme in photon_themes() {
        let theme_name = &theme["name"];
        let style = &theme["style"];
        for (name, entry) in style["syntax"].as_object().expect("syntax map") {
            if let Some(color) = entry["color"].as_str() {
                let color = hex(&Value::from(color));
                assert!(
                    contrast_ratio(color, BHUMI) >= minimum(name),
                    "{theme_name} syntax.{name} {color:06X}"
                );
            }
        }
        for name in ["text", "text.muted", "text.placeholder", "text.accent"] {
            let color = hex(&style[name]);
            assert!(
                contrast_ratio(color, BHUMI) >= minimum(name),
                "{theme_name} {name} {color:06X}"
            );
        }
    }
}

#[test]
fn photon_black_chrome_is_neutral_grey_with_photons_accent() {
    let style = &theme("Photon Black")["style"];
    for name in [
        "text",
        "text.muted",
        "text.placeholder",
        "hint",
        "ignored",
        "hidden",
        "predictive",
        "unreachable",
        "terminal.dim_foreground",
    ] {
        let color = hex(&style[name]);
        let (red, green, blue) = (color >> 16, (color >> 8) & 0xff, color & 0xff);
        assert!(
            red == green && green == blue,
            "{name} is #{color:06x}; Photon Black's chrome stays grey"
        );
    }
    for name in [
        "text.accent",
        "icon.accent",
        "border.focused",
        "border.selected",
        "pane.focused_border",
        "panel.focused_border",
    ] {
        assert_eq!(hex(&style[name]), photon_brand::MAHA, "{name}");
    }
}
