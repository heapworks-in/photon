//! Photon's own sections on the welcome page: where terminals open, panel sides, the font style
//! and language extensions. Each control writes an ordinary setting the Settings screen also shows.

use extension_host::ExtensionStore;
use fs::Fs;
use gpui::{AnyElement, App, Context, Hsla, Subscription};
use project::DisableAiSettings;
use settings::{
    DockPosition, DockSide, Settings as _, SettingsContent, SettingsStore, TerminalLayout,
    update_settings_file,
};
use theme_settings::ThemeSettings;
use ui::{
    SwitchField, ToggleButtonGroup, ToggleButtonGroupSize, ToggleButtonGroupStyle,
    ToggleButtonSimple, prelude::*,
};

/// Seconds a command runs before its finish notifies, when the switch turns notifications on.
const NOTIFY_AFTER_SECONDS: u64 = 10;

/// The font styles, as (label, buffer font, the app's action that applies the style).
const FONT_STYLES: [(&str, &str, &str); 4] = [
    ("Mono", "Lilex", "photon::UseMonoFontStyle"),
    ("Hack", "Hack", "photon::UseHackFontStyle"),
    ("Menlo", "Menlo", "photon::UseMenloFontStyle"),
    ("Monaco", "Monaco", "photon::UseMonacoFontStyle"),
];

/// Language extensions Photon doesn't ship with, as (extension id, name).
const LANGUAGES: [(&str, &str); 8] = [
    ("java", "Java"),
    ("kotlin", "Kotlin"),
    ("scala", "Scala"),
    ("php", "PHP"),
    ("ruby", "Ruby"),
    ("elixir", "Elixir"),
    ("dart", "Dart"),
    ("swift", "Swift"),
];

fn notify_is_on(seconds: u64) -> bool {
    seconds > 0
}

fn set_layout(settings: &mut SettingsContent, layout: TerminalLayout) {
    settings.terminal.get_or_insert_default().layout = Some(layout);
}

fn set_notify(settings: &mut SettingsContent, on: bool) {
    settings
        .terminal
        .get_or_insert_default()
        .notify_after_seconds = Some(if on { NOTIFY_AFTER_SECONDS } else { 0 });
}

fn set_file_panel(settings: &mut SettingsContent, side: DockSide) {
    settings.project_panel.get_or_insert_default().dock = Some(side);
}

fn set_agent_panel(settings: &mut SettingsContent, side: DockSide) {
    settings.agent.get_or_insert_default().dock = Some(match side {
        DockSide::Left => DockPosition::Left,
        DockSide::Right => DockPosition::Right,
    });
}

fn selected_font_style(buffer_font: &str) -> Option<usize> {
    FONT_STYLES
        .iter()
        .position(|(_, family, _)| *family == buffer_font)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardState {
    Install,
    Installing,
    Installed,
}

fn card_state(installed: bool, installing: bool) -> CardState {
    if installed {
        CardState::Installed
    } else if installing {
        CardState::Installing
    } else {
        CardState::Install
    }
}

fn write(cx: &App, update: impl 'static + Send + FnOnce(&mut SettingsContent)) {
    update_settings_file(<dyn Fs>::global(cx), cx, move |settings, _| {
        update(settings)
    });
}

fn section(title: &'static str) -> Div {
    v_flex().gap_2().child(Label::new(title))
}

fn switch(
    id: &'static str,
    label: &'static str,
    description: &'static str,
    on: bool,
    tab_index: &mut isize,
    update: fn(&mut SettingsContent, bool),
) -> SwitchField {
    SwitchField::new(
        id,
        Some(label),
        Some(description.into()),
        on,
        move |state, _, cx| {
            let on = *state == ToggleState::Selected;
            write(cx, move |settings| update(settings, on));
        },
    )
    .tab_index({
        *tab_index += 1;
        *tab_index - 1
    })
}

/// A small drawing of a window: a file panel beside an editor area holding either a terminal
/// (Center) or an editor above a terminal strip (Bottom Panel).
fn layout_tile(layout: TerminalLayout, selected: bool, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    let (id, label) = match layout {
        TerminalLayout::Center => ("onboarding-terminal-layout-center", "Center"),
        TerminalLayout::Panel => ("onboarding-terminal-layout-panel", "Bottom Panel"),
    };
    let line = |width: f32, color: Hsla| div().h_1().w(relative(width)).rounded_sm().bg(color);
    let code = colors.text_muted.opacity(0.5);
    let prompt = colors.text_accent.opacity(0.8);
    let terminal = |rows: usize| {
        v_flex()
            .gap_1()
            .p_1p5()
            .children((0..rows).map(move |row| line(if row % 2 == 0 { 0.6 } else { 0.85 }, prompt)))
    };
    let editor = v_flex()
        .gap_1()
        .p_1p5()
        .children([0.7, 0.5, 0.8].map(|width| line(width, code)));
    let main = match layout {
        TerminalLayout::Center => v_flex().flex_1().child(terminal(4)),
        TerminalLayout::Panel => v_flex().flex_1().child(div().flex_1().child(editor)).child(
            div()
                .border_t_1()
                .border_color(colors.border)
                .bg(colors.panel_background)
                .child(terminal(2)),
        ),
    };
    let window = h_flex()
        .h(rems(6.))
        .w_full()
        .overflow_hidden()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .child(
            v_flex()
                .w(relative(0.25))
                .h_full()
                .gap_1()
                .p_1p5()
                .border_r_1()
                .border_color(colors.border)
                .bg(colors.panel_background)
                .children([0.8, 0.6, 0.7].map(|width| line(width, code))),
        )
        .child(main);
    v_flex()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex_1()
        .gap_1()
        .p_1()
        .rounded_lg()
        .border_2()
        .border_color(if selected {
            colors.border_selected
        } else {
            colors.border_transparent
        })
        .cursor_pointer()
        .hover(|style| style.border_color(colors.border))
        .on_click(move |_, _, cx| write(cx, move |settings| set_layout(settings, layout)))
        .child(window)
        .child(
            div()
                .text_center()
                .child(Label::new(label).size(LabelSize::Small)),
        )
        .into_any_element()
}

/// Where terminals open, and the block switches.
pub fn render_terminal_section(tab_index: &mut isize, cx: &mut App) -> AnyElement {
    let terminal = cx
        .global::<SettingsStore>()
        .merged_settings()
        .terminal
        .clone();
    let terminal = terminal.unwrap_or_default();
    let layout = terminal.layout.unwrap_or_default();
    section("Terminal")
        .child(
            h_flex()
                .gap_2()
                .child(layout_tile(TerminalLayout::Center, layout == TerminalLayout::Center, cx))
                .child(layout_tile(TerminalLayout::Panel, layout == TerminalLayout::Panel, cx)),
        )
        .child(switch(
            "onboarding-command-blocks",
            "Command Blocks",
            "Each command and its output becomes a block you can select, copy and re-run.",
            terminal.blocks.unwrap_or(true),
            tab_index,
            |settings, on| settings.terminal.get_or_insert_default().blocks = Some(on),
        ))
        .child(switch(
            "onboarding-smart-blocks",
            "Smart Blocks",
            "Blocks recognise test runs, build errors, git output, servers and tables.",
            terminal.smart_blocks.unwrap_or(true),
            tab_index,
            |settings, on| settings.terminal.get_or_insert_default().smart_blocks = Some(on),
        ))
        .child(switch(
            "onboarding-notify-long-commands",
            "Notify When Long Commands Finish",
            "A notification when a command that ran 10 seconds or more finishes while Photon is in the background.",
            notify_is_on(terminal.notify_after_seconds.unwrap_or(NOTIFY_AFTER_SECONDS)),
            tab_index,
            set_notify,
        ))
        .into_any_element()
}

fn side_row(
    id: &'static str,
    label: &'static str,
    side: DockSide,
    tab_index: &mut isize,
    update: fn(&mut SettingsContent, DockSide),
) -> AnyElement {
    h_flex()
        .debug_selector(move || id.to_string())
        .justify_between()
        .child(Label::new(label))
        .child(
            ToggleButtonGroup::single_row(
                id,
                [DockSide::Left, DockSide::Right].map(|choice| {
                    ToggleButtonSimple::new(
                        match choice {
                            DockSide::Left => "Left",
                            DockSide::Right => "Right",
                        },
                        move |_, _, cx| write(cx, move |settings| update(settings, choice)),
                    )
                }),
            )
            .selected_index(match side {
                DockSide::Left => 0,
                DockSide::Right => 1,
            })
            .size(ToggleButtonGroupSize::Medium)
            .style(ToggleButtonGroupStyle::Outlined)
            .tab_index(tab_index),
        )
        .into_any_element()
}

/// Which side the file panel and the agent panel sit on.
pub fn render_panels_section(tab_index: &mut isize, cx: &mut App) -> AnyElement {
    let merged = cx.global::<SettingsStore>().merged_settings();
    let file_side = merged
        .project_panel
        .as_ref()
        .and_then(|panel| panel.dock)
        .unwrap_or(DockSide::Left);
    let agent_side = match merged.agent.as_ref().and_then(|agent| agent.dock) {
        Some(DockPosition::Left) => DockSide::Left,
        _ => DockSide::Right,
    };
    let ai = !DisableAiSettings::get_global(cx).disable_ai;
    section("Panels")
        .child(side_row(
            "onboarding-file-panel",
            "File Panel",
            file_side,
            tab_index,
            set_file_panel,
        ))
        .when(ai, |this| {
            this.child(side_row(
                "onboarding-agent-panel",
                "Agent Panel",
                agent_side,
                tab_index,
                set_agent_panel,
            ))
        })
        .into_any_element()
}

/// The font style for code, the terminal and menus, each option set in its own font.
pub fn render_font_section(_tab_index: &mut isize, cx: &mut App) -> AnyElement {
    let buffer_font = ThemeSettings::get_global(cx).buffer_font.family.clone();
    let selected = selected_font_style(buffer_font.as_ref());
    let colors = cx.theme().colors();
    section("Font")
        .child(
            h_flex()
                .gap_2()
                .children(FONT_STYLES.iter().enumerate().map(
                    |(index, (label, family, action))| {
                        let id = format!("onboarding-font-{}", label.to_lowercase());
                        v_flex()
                            .id(SharedString::from(id.clone()))
                            .debug_selector(move || id)
                            .flex_1()
                            .gap_1()
                            .p_2()
                            .rounded_lg()
                            .border_2()
                            .border_color(if selected == Some(index) {
                                colors.border_selected
                            } else {
                                colors.border
                            })
                            .bg(colors.editor_background)
                            .cursor_pointer()
                            .on_click(move |_, _, cx| match cx.build_action(action, None) {
                                Ok(action) => cx.dispatch_action(action.as_ref()),
                                Err(error) => log::error!("font style {action}: {error}"),
                            })
                            .child(
                                div()
                                    .font_family(SharedString::new_static(family))
                                    .text_sm()
                                    .child("fn main() {}"),
                            )
                            .child(
                                Label::new(*label)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                    },
                )),
        )
        .into_any_element()
}

/// Language extensions to install up front, so a project in that language works when it opens.
/// Nothing is downloaded until Install is clicked.
pub fn render_languages_section(cx: &mut App) -> Option<AnyElement> {
    let store = ExtensionStore::try_global(cx)?;
    let store = store.read(cx);
    let installed = store.installed_extensions();
    let installing = store.outstanding_operations();
    let cards = LANGUAGES.iter().map(|(id, name)| {
        let state = card_state(installed.contains_key(*id), installing.contains_key(*id));
        let selector = format!("onboarding-language-{id}");
        h_flex()
            .debug_selector(move || selector)
            .justify_between()
            .gap_2()
            .py_1()
            .child(Label::new(*name))
            .child(match state {
                CardState::Installed => Label::new("✓ Installed")
                    .size(LabelSize::Small)
                    .color(Color::Success)
                    .into_any_element(),
                CardState::Installing => Label::new("Installing…")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
                CardState::Install => {
                    Button::new(SharedString::from(format!("install-{id}")), "Install")
                        .style(ButtonStyle::Outlined)
                        .on_click(move |_, _, cx| {
                            if let Some(store) = ExtensionStore::try_global(cx) {
                                store.update(cx, |store, cx| {
                                    store.install_latest_extension((*id).into(), cx)
                                });
                            }
                        })
                        .into_any_element()
                }
            })
    });
    Some(
        section("Languages")
            .child(
                Label::new("Install support for languages Photon doesn't include.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(cards)
            .into_any_element(),
    )
}

/// Redraws `T` when language extensions install or uninstall.
pub fn observe_languages<T: 'static>(cx: &mut Context<T>) -> Option<Subscription> {
    let store = ExtensionStore::try_global(cx)?;
    Some(cx.observe(&store, |_, _, cx| cx.notify()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Render, TestAppContext, UpdateGlobal as _};

    struct Sections;

    impl Render for Sections {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let mut tab_index = 0;
            v_flex()
                .child(render_terminal_section(&mut tab_index, cx))
                .child(render_panels_section(&mut tab_index, cx))
                .child(render_font_section(&mut tab_index, cx))
        }
    }

    fn drawn(user_settings: &str, selector: &'static str, cx: &mut TestAppContext) -> bool {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(user_settings, cx)
                    .expect("settings");
            });
        });
        let (_, cx) = cx.add_window_view(|_, _| Sections);
        cx.run_until_parked();
        cx.debug_bounds(selector).is_some()
    }

    #[gpui::test]
    fn the_sections_draw_their_choices(cx: &mut TestAppContext) {
        for selector in [
            "onboarding-terminal-layout-center",
            "onboarding-terminal-layout-panel",
            "onboarding-file-panel",
            "onboarding-agent-panel",
            "onboarding-font-hack",
        ] {
            assert!(drawn("{}", selector, cx), "{selector}");
        }
    }

    #[gpui::test]
    fn the_agent_panel_row_hides_without_ai(cx: &mut TestAppContext) {
        assert!(!drawn(
            r#"{"disable_ai": true}"#,
            "onboarding-agent-panel",
            cx
        ));
    }

    #[test]
    fn notify_switch_reads_any_wait_as_on() {
        assert!(!notify_is_on(0));
        assert!(notify_is_on(10));
        assert!(notify_is_on(30));
    }

    #[test]
    fn terminal_choices_write_their_settings() {
        let mut settings = SettingsContent::default();
        set_layout(&mut settings, TerminalLayout::Panel);
        set_notify(&mut settings, true);
        let terminal = settings.terminal.as_ref().expect("terminal");
        assert_eq!(terminal.layout, Some(TerminalLayout::Panel));
        assert_eq!(terminal.notify_after_seconds, Some(10));
        set_notify(&mut settings, false);
        assert_eq!(
            settings
                .terminal
                .as_ref()
                .and_then(|t| t.notify_after_seconds),
            Some(0)
        );
    }

    #[test]
    fn panel_sides_write_their_settings() {
        let mut settings = SettingsContent::default();
        set_file_panel(&mut settings, DockSide::Right);
        set_agent_panel(&mut settings, DockSide::Left);
        assert_eq!(
            settings.project_panel.as_ref().and_then(|panel| panel.dock),
            Some(DockSide::Right)
        );
        assert_eq!(
            settings.agent.as_ref().and_then(|agent| agent.dock),
            Some(settings::DockPosition::Left)
        );
    }

    #[test]
    fn font_selection_follows_the_buffer_font() {
        assert_eq!(selected_font_style("Lilex"), Some(0));
        assert_eq!(selected_font_style("Hack"), Some(1));
        assert_eq!(selected_font_style("Menlo"), Some(2));
        assert_eq!(selected_font_style("Monaco"), Some(3));
        assert_eq!(selected_font_style("Fira Code"), None);
    }

    #[test]
    fn card_state_prefers_installed_then_installing() {
        assert_eq!(card_state(true, true), CardState::Installed);
        assert_eq!(card_state(true, false), CardState::Installed);
        assert_eq!(card_state(false, true), CardState::Installing);
        assert_eq!(card_state(false, false), CardState::Install);
    }

    #[test]
    fn languages_list_is_photons_choice() {
        let ids: Vec<&str> = LANGUAGES.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [
                "java", "kotlin", "scala", "php", "ruby", "elixir", "dart", "swift"
            ]
        );
    }
}
