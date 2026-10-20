use serde_json::Value;

fn default_settings() -> Value {
    serde_json_lenient::from_str(include_str!("../../../assets/settings/default.json"))
        .expect("default.json parses")
}

#[test]
fn offline_defaults() {
    let settings = default_settings();
    assert_eq!(settings["telemetry"]["diagnostics"], false);
    assert_eq!(settings["telemetry"]["metrics"], false);
    assert_eq!(settings["auto_update"], false);
    assert_eq!(settings["title_bar"]["show_sign_in"], false);
    assert_eq!(settings["title_bar"]["show_user_menu"], false);
    assert_eq!(settings["title_bar"]["show_user_picture"], false);
    assert_eq!(settings["collaboration_panel"]["button"], false);
    assert_eq!(settings["edit_predictions"]["provider"], "none");
}

#[test]
fn warp_typography_defaults() {
    let settings = default_settings();
    assert_eq!(settings["theme"]["mode"], "dark");
    assert_eq!(settings["theme"]["dark"], "Photon Black");
    assert_eq!(settings["buffer_font_family"], "Lilex");
    assert_eq!(settings["buffer_font_size"], 14);
    assert_eq!(settings["buffer_line_height"]["custom"], 1.2);
    assert_eq!(settings["cursor_shape"], "bar");
    assert_eq!(settings["ui_font_size"], 14);
    let terminal = &settings["terminal"];
    // The terminal follows the editor's font rather than repeating it.
    assert!(terminal["font_family"].is_null());
    assert!(terminal["font_size"].is_null());
    assert_eq!(terminal["line_height"]["custom"], 1.2);
    assert_eq!(terminal["cursor_shape"], "bar");
    assert_eq!(terminal["blinking"], "on");
}

#[test]
fn agent_and_terminal_defaults() {
    let settings = default_settings();
    let devin = &settings["agent_servers"]["devin"];
    assert_eq!(devin["type"], "custom");
    assert_eq!(devin["command"], "devin");
    assert_eq!(devin["args"], serde_json::json!(["acp"]));
    assert_eq!(settings["terminal"]["button"], false);
}

#[test]
fn new_settings_file_keeps_photon_defaults() {
    // Written the first time anything saves a setting; pinning values here overrides Photon's defaults.
    let initial: Value = serde_json_lenient::from_str(include_str!(
        "../../../assets/settings/initial_user_settings.json"
    ))
    .expect("initial_user_settings.json parses");
    assert_eq!(initial, serde_json::json!({}));
}
