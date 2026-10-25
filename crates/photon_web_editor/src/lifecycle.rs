use std::time::{Duration, Instant};

pub const DROP_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Create,
    ShowNative,
    ShowSnapshot,
    Drop,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Absent,
    Native,
    Snapshot,
}

/// Decides when the native web view exists and when it is on screen, from what each frame shows.
#[derive(Default)]
pub struct Lifecycle {
    state: State,
    not_native_since: Option<Instant>,
    visible: bool,
}

impl Lifecycle {
    /// `visible` is `None` for calls that aren't frames (focus loss, the drop timer), where the tab
    /// is still where it was last seen.
    pub fn on_frame(&mut self, visible: Option<bool>, focused: bool, now: Instant) -> Action {
        if let Some(visible) = visible {
            self.visible = visible;
        }
        let wants_native = self.visible && focused;
        match self.state {
            // Unfocused tabs paint their last snapshot (or a placeholder), so nothing is created.
            State::Absent if !wants_native => Action::None,
            State::Absent => {
                // Created hidden; the caller calls again at once, which returns `ShowNative`.
                self.state = State::Snapshot;
                self.not_native_since = Some(now);
                Action::Create
            }
            _ if wants_native => {
                self.not_native_since = None;
                self.transition(State::Native)
            }
            _ => {
                let since = *self.not_native_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= DROP_AFTER {
                    self.state = State::Absent;
                    self.not_native_since = None;
                    return Action::Drop;
                }
                self.transition(State::Snapshot)
            }
        }
    }

    pub fn is_native(&self) -> bool {
        self.state == State::Native
    }

    /// A snapshot is a full-size image in memory and in the GPU atlas, so only a tab on screen
    /// keeps one.
    pub fn keeps_snapshot(&self) -> bool {
        self.visible
    }

    fn transition(&mut self, state: State) -> Action {
        if self.state == state {
            return Action::None;
        }
        self.state = state;
        match state {
            State::Native => Action::ShowNative,
            State::Snapshot => Action::ShowSnapshot,
            State::Absent => Action::Drop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(
        lifecycle: &mut Lifecycle,
        start: Instant,
        steps: &[(u64, bool, bool)],
    ) -> Vec<Action> {
        steps
            .iter()
            .map(|(seconds, visible, focused)| {
                lifecycle.on_frame(
                    Some(*visible),
                    *focused,
                    start + Duration::from_secs(*seconds),
                )
            })
            .collect()
    }

    #[test]
    fn nothing_is_created_until_first_shown() {
        let mut lifecycle = Lifecycle::default();
        let start = Instant::now();
        assert_eq!(
            frames(
                &mut lifecycle,
                start,
                &[(0, false, false), (100, false, true), (200, true, true)]
            ),
            [Action::None, Action::None, Action::Create]
        );
    }

    #[test]
    fn unfocused_first_show_creates_nothing() {
        let mut lifecycle = Lifecycle::default();
        let start = Instant::now();
        assert_eq!(
            frames(
                &mut lifecycle,
                start,
                &[
                    (0, true, false),
                    (10, true, false),
                    (20, true, true),
                    (20, true, true)
                ]
            ),
            [
                Action::None,
                Action::None,
                Action::Create,
                Action::ShowNative
            ]
        );
    }

    #[test]
    fn native_view_only_while_focused() {
        let mut lifecycle = Lifecycle::default();
        let start = Instant::now();
        assert_eq!(
            frames(
                &mut lifecycle,
                start,
                &[
                    (0, true, true),
                    (0, true, true),
                    (1, true, true),
                    (2, true, false),
                    (3, true, true)
                ]
            ),
            [
                Action::Create,
                Action::ShowNative,
                Action::None,
                Action::ShowSnapshot,
                Action::ShowNative
            ]
        );
    }

    #[test]
    fn unfocusing_switches_to_a_snapshot_once() {
        let mut lifecycle = Lifecycle::default();
        let start = Instant::now();
        assert_eq!(
            frames(
                &mut lifecycle,
                start,
                &[
                    (0, true, true),
                    (0, true, true),
                    (1, true, false),
                    (2, true, false),
                    (3, false, false)
                ]
            ),
            [
                Action::Create,
                Action::ShowNative,
                Action::ShowSnapshot,
                Action::None,
                Action::None
            ]
        );
    }

    #[test]
    fn hiding_a_native_view_switches_to_a_snapshot() {
        let mut lifecycle = Lifecycle::default();
        let start = Instant::now();
        assert_eq!(
            frames(
                &mut lifecycle,
                start,
                &[(0, true, true), (0, true, true), (1, false, true)]
            ),
            [Action::Create, Action::ShowNative, Action::ShowSnapshot]
        );
    }

    #[test]
    fn hidden_for_sixty_seconds_drops_the_view() {
        let mut lifecycle = Lifecycle::default();
        let start = Instant::now();
        assert_eq!(
            frames(
                &mut lifecycle,
                start,
                &[
                    (0, true, true),
                    (0, true, true),
                    (10, false, false),
                    (69, false, false),
                    (70, false, false),
                    (80, false, false)
                ]
            ),
            [
                Action::Create,
                Action::ShowNative,
                Action::ShowSnapshot,
                Action::None,
                Action::Drop,
                Action::None
            ]
        );
    }

    #[test]
    fn unfocused_for_sixty_seconds_drops_the_view() {
        let mut lifecycle = Lifecycle::default();
        let start = Instant::now();
        assert_eq!(
            frames(
                &mut lifecycle,
                start,
                &[
                    (0, true, true),
                    (0, true, true),
                    (1, true, false),
                    (61, true, false),
                    (62, true, false),
                    (63, true, true)
                ]
            ),
            [
                Action::Create,
                Action::ShowNative,
                Action::ShowSnapshot,
                Action::Drop,
                Action::None,
                Action::Create
            ]
        );
    }

    #[test]
    fn shown_again_after_drop_recreates_it() {
        let mut lifecycle = Lifecycle::default();
        let start = Instant::now();
        assert_eq!(
            frames(
                &mut lifecycle,
                start,
                &[
                    (0, true, true),
                    (0, true, true),
                    (1, false, false),
                    (61, false, false),
                    (62, true, false),
                    (62, true, true),
                    (62, true, true)
                ]
            ),
            [
                Action::Create,
                Action::ShowNative,
                Action::ShowSnapshot,
                Action::Drop,
                Action::None,
                Action::Create,
                Action::ShowNative
            ]
        );
    }

    #[test]
    fn hidden_less_than_sixty_seconds_keeps_it() {
        let mut lifecycle = Lifecycle::default();
        let start = Instant::now();
        assert_eq!(
            frames(
                &mut lifecycle,
                start,
                &[
                    (0, true, true),
                    (0, true, true),
                    (1, false, false),
                    (60, false, false),
                    (60, true, true),
                    (61, false, false),
                    (120, false, false),
                    (121, false, false)
                ]
            ),
            [
                Action::Create,
                Action::ShowNative,
                Action::ShowSnapshot,
                Action::None,
                Action::ShowNative,
                Action::ShowSnapshot,
                Action::None,
                Action::Drop
            ]
        );
    }

    #[test]
    fn only_a_visible_tab_keeps_its_snapshot() {
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);

        // A split that lost focus stays on screen: its snapshot is what it shows.
        let mut split = Lifecycle::default();
        split.on_frame(Some(true), true, at(0));
        split.on_frame(Some(true), true, at(0));
        assert_eq!(split.on_frame(None, false, at(1)), Action::ShowSnapshot);
        assert!(split.keeps_snapshot());
        // The drop check isn't a frame: it doesn't know the tab is still on screen.
        assert_eq!(split.on_frame(None, false, at(61)), Action::Drop);
        assert!(
            split.keeps_snapshot(),
            "a dropped page still shows its snapshot"
        );

        // A tab another one replaced holds nothing.
        let mut hidden = Lifecycle::default();
        hidden.on_frame(Some(true), true, at(0));
        hidden.on_frame(Some(true), true, at(0));
        hidden.on_frame(None, false, at(1));
        assert_eq!(hidden.on_frame(Some(false), false, at(1)), Action::None);
        assert!(!hidden.keeps_snapshot());
        assert_eq!(hidden.on_frame(None, false, at(61)), Action::Drop);
        assert!(!hidden.keeps_snapshot());

        // Shown again unfocused, it may keep the next snapshot.
        hidden.on_frame(Some(true), false, at(62));
        assert!(hidden.keeps_snapshot());

        assert!(!Lifecycle::default().keeps_snapshot());
    }
}
