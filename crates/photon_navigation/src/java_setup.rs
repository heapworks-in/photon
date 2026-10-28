use std::{
    collections::{HashMap, HashSet},
    hash::BuildHasher,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use fs::Fs;
use gpui::{App, AppContext as _, Context, DismissEvent, Entity, Global, Task, WeakEntity, Window};
use language::LanguageServerName;
use picker::{Picker, PickerDelegate};
use project::{Project, project_settings::ProjectSettings};
use settings::{Settings as _, SettingsContent};
use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

pub const MIN_JAVA: u32 = 21;
const JDTLS: &str = "jdtls";
const INSTALL_LABEL: &str = "Install Java 21";
const DOWNLOAD_LABEL: &str = "Download Java 21";
const DOWNLOAD_URL: &str = "https://adoptium.net/temurin/releases/?version=21";

#[derive(Clone, Debug, PartialEq)]
pub struct JavaInstall {
    pub major: u32,
    pub home: PathBuf,
}

/// What the machine has: the Java jdtls would start with, and every Java installed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JavaScan {
    pub current: Option<u32>,
    pub installs: Vec<JavaInstall>,
    pub has_brew: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NeedsJava {
    pub current: Option<u32>,
    pub has_brew: bool,
}

#[derive(Debug, PartialEq)]
pub enum JavaPlan {
    Ready,
    Use(JavaInstall),
    Install(NeedsJava),
}

pub type Detect = Rc<dyn Fn(&Entity<Project>, &mut App) -> Task<JavaScan>>;

/// App-wide, because Java is installed per machine; detection runs at most once per app run.
pub struct JavaSetup {
    detect: Detect,
    checked: bool,
    needs_java: Option<NeedsJava>,
    installing: bool,
}

impl Global for JavaSetup {}

impl JavaSetup {
    pub fn new(detect: Detect) -> Self {
        Self {
            detect,
            checked: false,
            needs_java: None,
            installing: false,
        }
    }

    pub fn needs_java(cx: &App) -> Option<NeedsJava> {
        cx.try_global::<Self>()
            .and_then(|setup| setup.needs_java.clone())
    }

    pub fn installing(cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|setup| setup.installing)
    }
}

pub fn real_detect() -> Detect {
    Rc::new(|project, cx| {
        let environment = project.update(cx, |project, cx| {
            project
                .environment()
                .update(cx, |environment, cx| environment.default_environment(cx))
        });
        cx.spawn(async move |cx| {
            let environment = environment.await.unwrap_or_default();
            cx.background_spawn(async move { scan(&environment).await })
                .await
        })
    })
}

fn major(version: &str) -> Option<u32> {
    let mut parts = version.split(|character: char| !character.is_ascii_digit());
    let first: u32 = parts.next()?.parse().ok()?;
    // Java 8 and older report themselves as 1.N.
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// The major version from `java -version` output, e.g. `openjdk version "17.0.2"`.
pub fn version_from_java_output(output: &str) -> Option<u32> {
    major(output.split_once("version \"")?.1)
}

/// The major version from a JDK's `release` file (`JAVA_VERSION="21.0.2"`).
pub fn version_from_release_file(contents: &str) -> Option<u32> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))
        .and_then(|version| major(version.trim().trim_matches('"')))
}

/// The JDKs in `/usr/libexec/java_home -V` output, newest first.
pub fn installs_from_java_home_listing(output: &str) -> Vec<JavaInstall> {
    output
        .lines()
        .filter(|line| line.starts_with(char::is_whitespace))
        .filter_map(|line| {
            let line = line.trim();
            let major = major(line.split_whitespace().next()?)?;
            let home = line.rsplit_once('"')?.1.trim();
            home.starts_with('/').then(|| JavaInstall {
                major,
                home: PathBuf::from(home),
            })
        })
        .collect()
}

/// Every install jdtls can run on, newest first, without duplicates.
pub fn usable(installs: &[JavaInstall]) -> Vec<JavaInstall> {
    let mut usable: Vec<JavaInstall> = installs
        .iter()
        .filter(|install| install.major >= MIN_JAVA)
        .cloned()
        .collect();
    usable.sort_by_key(|install| std::cmp::Reverse(install.major));
    let mut seen = HashSet::new();
    usable.retain(|install| seen.insert(install.home.clone()));
    usable
}

pub fn plan(scan: &JavaScan) -> JavaPlan {
    if scan.current.is_some_and(|current| current >= MIN_JAVA) {
        return JavaPlan::Ready;
    }
    match usable(&scan.installs).into_iter().next() {
        Some(install) => JavaPlan::Use(install),
        None => JavaPlan::Install(NeedsJava {
            current: scan.current,
            has_brew: scan.has_brew,
        }),
    }
}

async fn scan(environment: &HashMap<String, String, impl BuildHasher>) -> JavaScan {
    let listing = match util::command::new_command("/usr/libexec/java_home")
        .arg("-V")
        .output()
        .await
    {
        Ok(output) => installs_from_java_home_listing(&String::from_utf8_lossy(&output.stderr)),
        Err(error) => {
            log::info!("java_home: {error}");
            Vec::new()
        }
    };
    let home = util::paths::home_dir();
    // Homebrew's `opt` links come first: they survive `brew upgrade`, the Cellar folders they
    // point at don't.
    let mut homes: Vec<PathBuf> = children(Path::new("/opt/homebrew/opt"))
        .into_iter()
        .filter(|dir| {
            dir.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("openjdk"))
        })
        .map(|dir| dir.join("libexec/openjdk.jdk/Contents/Home"))
        .collect();
    homes.extend(listing.iter().map(|install| install.home.clone()));
    homes.extend(children(&home.join(".sdkman/candidates/java")));
    homes.extend(children(&home.join(".jenv/versions")));
    let installs = installs_at(homes);

    let path = environment.get("PATH").cloned().unwrap_or_default();
    let on_path = |name: &str| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    };
    let current = match environment.get("JAVA_HOME") {
        Some(java_home) => release_version(Path::new(java_home)),
        None => match on_path("java") {
            // The macOS stub runs the default JDK, which java_home lists first; running the stub
            // with no JDK installed opens an install dialog.
            Some(java) if java == Path::new("/usr/bin/java") => {
                listing.first().map(|install| install.major)
            }
            Some(java) => match java
                .canonicalize()
                .ok()
                .and_then(|java| Some(java.parent()?.parent()?.to_path_buf()))
                .and_then(|home| release_version(&home))
            {
                Some(major) => Some(major),
                None => java_version(&java, environment).await,
            },
            None => None,
        },
    };
    let has_brew = on_path("brew").is_some()
        || Path::new("/opt/homebrew/bin/brew").is_file()
        || Path::new("/usr/local/bin/brew").is_file();
    JavaScan {
        current,
        installs,
        has_brew,
    }
}

fn children(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

/// The Java installs among `homes`, each once. The first path seen for an install is the one kept,
/// so a stable link wins over the versioned folder it resolves to.
fn installs_at(homes: impl IntoIterator<Item = PathBuf>) -> Vec<JavaInstall> {
    let mut seen = HashSet::new();
    let mut installs = Vec::new();
    for home in homes {
        let Ok(resolved) = std::fs::canonicalize(&home) else {
            continue;
        };
        if !seen.insert(resolved.clone()) {
            continue;
        }
        if let Some(major) = release_version(&resolved) {
            installs.push(JavaInstall { major, home });
        }
    }
    installs
}

fn release_version(home: &Path) -> Option<u32> {
    version_from_release_file(&std::fs::read_to_string(home.join("release")).ok()?)
}

async fn java_version(
    java: &Path,
    environment: &HashMap<String, String, impl BuildHasher>,
) -> Option<u32> {
    let output = util::command::new_command(java)
        .arg("-version")
        .env_clear()
        .envs(environment)
        .output()
        .await
        .log_err()?;
    version_from_java_output(&String::from_utf8_lossy(&output.stderr))
}

pub fn java_home_is_set(cx: &App) -> bool {
    ProjectSettings::get_global(cx)
        .lsp
        .get(&LanguageServerName::new_static(JDTLS))
        .and_then(|settings| settings.settings.as_ref())
        .is_some_and(|settings| settings.get("java_home").is_some())
}

pub(crate) fn set_java_home(settings: &mut SettingsContent, home: &str, overwrite: bool) {
    let jdtls = settings.project.lsp.0.entry(JDTLS.into()).or_default();
    if let Some(settings) = jdtls
        .settings
        .get_or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        && (overwrite || !settings.contains_key("java_home"))
    {
        settings.insert("java_home".into(), home.into());
    }
}

/// Saves `home` as jdtls's Java, then restarts jdtls for the open Java files.
fn use_java(
    project: &Entity<Project>,
    home: &Path,
    overwrite: bool,
    cx: &mut App,
) -> Task<anyhow::Result<()>> {
    let home = home.to_string_lossy().to_string();
    let saved = settings::update_settings_file_with_completion(
        <dyn Fs>::global(cx),
        cx,
        move |settings, _| set_java_home(settings, &home, overwrite),
    );
    let project = project.downgrade();
    cx.spawn(async move |cx| {
        saved.await??;
        project.update(cx, |project, cx| {
            let buffers = project
                .opened_buffers(cx)
                .into_iter()
                .filter(|buffer| {
                    buffer
                        .read(cx)
                        .language()
                        .is_some_and(|language| language.name().as_ref() == "Java")
                })
                .collect();
            project.lsp_store().update(cx, |lsp_store, cx| {
                lsp_store.restart_language_servers_for_buffers(buffers, HashSet::default(), cx)
            });
        })
    })
}

/// Checks the machine's Java once per app run, when a Java file opens and the user hasn't chosen
/// a Java for jdtls.
pub fn check_once(workspace: WeakEntity<Workspace>, project: Entity<Project>, cx: &mut App) {
    if java_home_is_set(cx) || !project.read(cx).is_local() {
        return;
    }
    let Some(detect) = cx
        .try_global::<JavaSetup>()
        .filter(|setup| !setup.checked)
        .map(|setup| setup.detect.clone())
    else {
        return;
    };
    cx.update_global::<JavaSetup, _>(|setup, _| setup.checked = true);
    let scanned = detect(&project, cx);
    cx.spawn(async move |cx| {
        let scan = scanned.await;
        match plan(&scan) {
            JavaPlan::Ready => Ok(()),
            JavaPlan::Install(needs) => {
                cx.update_global::<JavaSetup, _>(|setup, _| setup.needs_java = Some(needs));
                Ok(())
            }
            JavaPlan::Use(install) => {
                cx.update(|cx| use_java(&project, &install.home, false, cx))
                    .await?;
                workspace.update(cx, |workspace, cx| {
                    show_using_java(workspace, &project, &scan, &install, cx)
                })
            }
        }
    })
    .detach_and_log_err(cx);
}

pub(crate) struct UsingJavaNotice;
pub(crate) struct NeedsJavaNotice;
pub(crate) struct InstalledNotice;
pub(crate) struct InstallFailedNotice;

fn show_using_java(
    workspace: &mut Workspace,
    project: &Entity<Project>,
    scan: &JavaScan,
    install: &JavaInstall,
    cx: &mut Context<Workspace>,
) {
    let body = match scan.current {
        Some(current) => format!(
            "jdtls needs Java {MIN_JAVA} or newer; your shell's JAVA_HOME is Java {current}. \
             Your builds and terminal still use {current}."
        ),
        None => {
            format!("jdtls needs Java {MIN_JAVA} or newer. Your builds and terminal are unchanged.")
        }
    };
    let title = format!("Using Java {} for Java code intelligence", install.major);
    let installs = usable(&scan.installs);
    let project = project.clone();
    let workspace_handle = cx.weak_entity();
    workspace.show_notification(NotificationId::unique::<UsingJavaNotice>(), cx, |cx| {
        cx.new(|cx| {
            MessageNotification::new(body, cx)
                .with_title(title)
                .content_icon(IconName::Info, Color::Accent)
                .show_suppress_button(false)
                .primary_message("Change…")
                .primary_on_click(move |window, cx| {
                    let project = project.clone();
                    let installs = installs.clone();
                    workspace_handle
                        .update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, |window, cx| {
                                JavaPicker::new(project, installs, window, cx)
                            })
                        })
                        .log_err();
                })
                .secondary_message("OK")
        })
    });
}

pub fn show_needs_java(workspace: &mut Workspace, needs: &NeedsJava, cx: &mut Context<Workspace>) {
    let has = match needs.current {
        Some(current) => format!("This Mac has Java {current}."),
        None => "This Mac has no Java.".to_string(),
    };
    let body = format!(
        "Go to definition, errors and completions for Java need Java {MIN_JAVA} or newer. {has}"
    );
    let workspace_handle = cx.weak_entity();
    let has_brew = needs.has_brew;
    workspace.show_notification(NotificationId::unique::<NeedsJavaNotice>(), cx, |cx| {
        cx.new(|cx| {
            MessageNotification::new(body, cx)
                .with_title(format!("Java code intelligence needs Java {MIN_JAVA}"))
                .content_icon(IconName::Warning, Color::Warning)
                .show_suppress_button(false)
                .primary_message(install_label(has_brew))
                .primary_on_click(move |window, cx| {
                    install_java(workspace_handle.clone(), window, cx)
                })
                .secondary_message("Not now")
        })
    });
}

pub fn install_label(has_brew: bool) -> &'static str {
    if has_brew {
        INSTALL_LABEL
    } else {
        DOWNLOAD_LABEL
    }
}

/// Runs the install in a terminal tab the user can watch, then picks up the new Java.
pub fn install_java(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let Some(needs) = JavaSetup::needs_java(cx) else {
        return;
    };
    if !needs.has_brew {
        cx.open_url(DOWNLOAD_URL);
        return;
    }
    let Some(detect) = cx
        .try_global::<JavaSetup>()
        .map(|setup| setup.detect.clone())
    else {
        return;
    };
    let task = SpawnInTerminal {
        id: TaskId("photon-install-java-21".into()),
        full_label: INSTALL_LABEL.into(),
        label: INSTALL_LABEL.into(),
        command: Some("brew".into()),
        args: vec!["install".into(), "openjdk@21".into()],
        command_label: "brew install openjdk@21".into(),
        use_new_terminal: true,
        reveal: RevealStrategy::Always,
        reveal_target: RevealTarget::Center,
        show_command: true,
        show_summary: true,
        ..SpawnInTerminal::default()
    };
    let Some((finished, project)) = workspace
        .update(cx, |workspace, cx| {
            (
                workspace.spawn_in_terminal(task, window, cx),
                workspace.project().clone(),
            )
        })
        .log_err()
    else {
        return;
    };
    cx.update_global::<JavaSetup, _>(|setup, _| setup.installing = true);
    cx.spawn(async move |cx| {
        let succeeded = matches!(finished.await, Some(Ok(status)) if status.success());
        let installed = if succeeded {
            let scan = cx.update(|cx| detect(&project, cx)).await;
            usable(&scan.installs).into_iter().next()
        } else {
            None
        };
        let used = match &installed {
            Some(install) => cx
                .update(|cx| use_java(&project, &install.home, true, cx))
                .await
                .log_err()
                .is_some(),
            None => false,
        };
        cx.update_global::<JavaSetup, _>(|setup, _| {
            setup.installing = false;
            if used {
                setup.needs_java = None;
            }
        });
        workspace.update(cx, |workspace, cx| {
            if used {
                show_installed(workspace, cx)
            } else {
                show_install_failed(workspace, cx)
            }
        })
    })
    .detach_and_log_err(cx);
}

fn show_installed(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    workspace.show_notification(NotificationId::unique::<InstalledNotice>(), cx, |cx| {
        cx.new(|cx| {
            MessageNotification::new("Java code intelligence is on for this project.", cx)
                .with_title(format!("Java {MIN_JAVA} installed · jdtls is running"))
                .content_icon(IconName::Check, Color::Success)
                .show_suppress_button(false)
                .primary_message("OK")
        })
    });
}

fn show_install_failed(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let workspace_handle = cx.weak_entity();
    workspace.show_notification(NotificationId::unique::<InstallFailedNotice>(), cx, |cx| {
        cx.new(|cx| {
            MessageNotification::new(
                format!("The install command failed. Its output is in the {INSTALL_LABEL} tab."),
                cx,
            )
            .with_title(format!("Java {MIN_JAVA} didn't install"))
            .content_icon(IconName::XCircle, Color::Error)
            .show_suppress_button(false)
            .primary_message("Show Log")
            .primary_on_click(move |window, cx| {
                workspace_handle
                    .update(cx, |workspace, cx| {
                        let tab = workspace
                            .items(cx)
                            .find(|item| item.tab_content_text(0, cx) == INSTALL_LABEL)
                            .map(|item| item.boxed_clone());
                        if let Some(tab) = tab {
                            workspace.activate_item(tab.as_ref(), true, true, window, cx);
                        }
                    })
                    .log_err();
            })
        })
    });
}

pub struct JavaPicker {
    picker: Entity<Picker<JavaPickerDelegate>>,
}

impl JavaPicker {
    fn new(
        project: Entity<Project>,
        installs: Vec<JavaInstall>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = JavaPickerDelegate {
            picker: cx.weak_entity(),
            project,
            installs,
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent))
            .detach();
        Self { picker }
    }
}

impl gpui::EventEmitter<DismissEvent> for JavaPicker {}
impl workspace::ModalView for JavaPicker {}

impl gpui::Focusable for JavaPicker {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for JavaPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

pub struct JavaPickerDelegate {
    picker: WeakEntity<JavaPicker>,
    project: Entity<Project>,
    installs: Vec<JavaInstall>,
    selected_index: usize,
}

impl PickerDelegate for JavaPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "java picker"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Java for Java code intelligence".into()
    }

    fn match_count(&self) -> usize {
        self.installs.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        _: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(install) = self.installs.get(self.selected_index) {
            use_java(&self.project, &install.home, true, cx).detach_and_log_err(cx);
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let install = self.installs.get(index)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(format!("Java {}", install.major)))
                        .child(
                            Label::new(install.home.to_string_lossy().to_string())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_homebrew_link_is_kept_over_its_versioned_folder() {
        let root = std::env::temp_dir().join(format!("photon-java-{}", std::process::id()));
        let cellar = root.join("Cellar/openjdk/27.0.1/Home");
        std::fs::create_dir_all(&cellar).expect("cellar");
        std::fs::write(cellar.join("release"), "JAVA_VERSION=\"27.0.1\"\n").expect("release");
        let link = root.join("opt-openjdk");
        std::os::unix::fs::symlink(&cellar, &link).expect("link");

        let installs = installs_at([link.clone(), cellar, root.join("missing")]);
        std::fs::remove_dir_all(&root).expect("clean up");

        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].home, link);
        assert_eq!(installs[0].major, 27);
    }

    fn install(major: u32, home: &str) -> JavaInstall {
        JavaInstall {
            major,
            home: PathBuf::from(home),
        }
    }

    #[test]
    fn reads_versions_from_java_and_release_files() {
        assert_eq!(
            version_from_java_output(
                "openjdk version \"17.0.2\" 2022-01-18\nOpenJDK Runtime Environment"
            ),
            Some(17)
        );
        assert_eq!(
            version_from_java_output("java version \"1.8.0_292\""),
            Some(8)
        );
        assert_eq!(version_from_java_output("openjdk version \"21\""), Some(21));
        assert_eq!(version_from_java_output("command not found"), None);
        assert_eq!(
            version_from_release_file("IMPLEMENTOR=\"Homebrew\"\nJAVA_VERSION=\"27.0.1\"\n"),
            Some(27)
        );
        assert_eq!(
            version_from_release_file("JAVA_VERSION=\"1.8.0_402\""),
            Some(8)
        );
        assert_eq!(version_from_release_file("IMPLEMENTOR=\"x\""), None);
    }

    #[test]
    fn reads_the_java_home_listing() {
        let listing = "Matching Java Virtual Machines (3):\n    \
            21.0.2 (arm64) \"Eclipse Adoptium\" - \"OpenJDK 21.0.2\" /Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home\n    \
            17.0.9 (arm64) \"Homebrew\" - \"OpenJDK 17.0.9\" /opt/homebrew/Cellar/openjdk@17/17.0.9/libexec/openjdk.jdk/Contents/Home\n    \
            1.8.0_292 (x86_64) \"AdoptOpenJDK\" - \"AdoptOpenJDK 8\" /Library/Java/JavaVirtualMachines/My JDK 8/Contents/Home\n\
            /Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home\n";
        assert_eq!(
            installs_from_java_home_listing(listing),
            vec![
                install(
                    21,
                    "/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home"
                ),
                install(
                    17,
                    "/opt/homebrew/Cellar/openjdk@17/17.0.9/libexec/openjdk.jdk/Contents/Home"
                ),
                install(
                    8,
                    "/Library/Java/JavaVirtualMachines/My JDK 8/Contents/Home"
                ),
            ]
        );
        assert_eq!(
            installs_from_java_home_listing("Unable to find any JVMs matching version \"(null)\"."),
            vec![]
        );
    }

    #[test]
    fn picks_the_newest_java_21_or_later() {
        let installs = vec![
            install(17, "/jdk17"),
            install(21, "/jdk21"),
            install(27, "/jdk27"),
            install(21, "/jdk21"),
        ];
        assert_eq!(
            usable(&installs),
            vec![install(27, "/jdk27"), install(21, "/jdk21")]
        );
        let scan = |current, installs: Vec<JavaInstall>| JavaScan {
            current,
            installs,
            has_brew: true,
        };
        assert_eq!(
            plan(&scan(Some(17), installs.clone())),
            JavaPlan::Use(install(27, "/jdk27"))
        );
        assert_eq!(plan(&scan(Some(21), installs)), JavaPlan::Ready);
        assert_eq!(
            plan(&scan(Some(17), vec![install(11, "/jdk11")])),
            JavaPlan::Install(NeedsJava {
                current: Some(17),
                has_brew: true
            })
        );
        assert_eq!(
            plan(&scan(None, vec![])),
            JavaPlan::Install(NeedsJava {
                current: None,
                has_brew: true
            })
        );
    }

    #[test]
    fn never_overwrites_a_chosen_java_home_unless_asked() {
        let mut settings = SettingsContent::default();
        set_java_home(&mut settings, "/jdk21", false);
        set_java_home(&mut settings, "/jdk27", false);
        let java_home = |settings: &SettingsContent| {
            settings.project.lsp.0[JDTLS]
                .settings
                .as_ref()
                .and_then(|settings| settings.get("java_home"))
                .cloned()
        };
        assert_eq!(java_home(&settings), Some("/jdk21".into()));
        set_java_home(&mut settings, "/jdk27", true);
        assert_eq!(java_home(&settings), Some("/jdk27".into()));
    }
}
