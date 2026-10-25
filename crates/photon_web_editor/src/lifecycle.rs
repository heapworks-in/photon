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
    hidden_since: Option<Instant>,
}

impl Lifecycle {
    pub fn on_frame(&mut self, visible: bool, focused: bool, now: Instant) -> Action {
        if !visible {
            if self.state == State::Absent {
                return Action::None;
            }
            let hidden_since = *self.hidden_since.get_or_insert(now);
            if now.saturating_duration_since(hidden_since) >= DROP_AFTER {
                self.state = State::Absent;
                self.hidden_since = None;
                return Action::Drop;
            }
            return self.transition(State::Snapshot);
        }

        self.hidden_since = None;
        if self.state == State::Absent {
            // Created hidden; the next frame shows it natively if focused.
            self.state = State::Snapshot;
            return Action::Create;
        }
        self.transition(if focused {
            State::Native
        } else {
            State::Snapshot
        })
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
                lifecycle.on_frame(*visible, *focused, start + Duration::from_secs(*seconds))
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
                &[(0, false, false), (100, false, true), (200, true, false)]
            ),
            [Action::None, Action::None, Action::Create]
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
                    (0, true, false),
                    (10, false, false),
                    (70, false, false),
                    (80, false, false)
                ]
            ),
            [Action::Create, Action::None, Action::Drop, Action::None]
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
                    (62, true, true),
                    (62, true, true)
                ]
            ),
            [
                Action::Create,
                Action::ShowNative,
                Action::ShowSnapshot,
                Action::Drop,
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
                    (0, true, false),
                    (1, false, false),
                    (60, false, false),
                    (60, true, false),
                    (61, false, false),
                    (120, false, false),
                    (121, false, false)
                ]
            ),
            [
                Action::Create,
                Action::None,
                Action::None,
                Action::None,
                Action::None,
                Action::None,
                Action::Drop
            ]
        );
    }
}
