//! Monitor state, keymap and state transitions. Pure: no terminal, no system calls, so every
//! transition is unit-tested.
//!
//! Keymap (ADR-014): arrows or `j`/`k` move, `x` closes the selected app (through the
//! confirmation flow), `?` explains, `h` shows recorded pressure events, `t` cycles the theme,
//! `q` or Ctrl-C quits.

use std::time::Duration;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use super::history::History;
use super::theme::Theme;
use crate::advisor::classify::ClassifiedGroup;
use crate::advisor::{Advice, SafetyClass};
use crate::core::Snapshot;

/// What a key or mouse event asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    /// Close the selected app (after confirmation).
    Close,
    /// Toggle the explanation panel.
    Explain,
    /// Toggle the history panel.
    History,
    /// Cycle the colour theme.
    Theme,
    /// Dismiss the open panel or message.
    Back,
    Quit,
    /// Mouse click at a terminal position.
    Click(u16, u16),
    ScrollUp,
    ScrollDown,
}

/// Maps a key to an action. Only key presses count (Windows also reports releases).
pub fn key_action(key: KeyEvent) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c' | 'C') => Some(Action::Quit),
            _ => None,
        };
    }
    Some(match key.code {
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Home | KeyCode::Char('g') => Action::Top,
        KeyCode::End | KeyCode::Char('G') => Action::Bottom,
        KeyCode::Char('x') => Action::Close,
        KeyCode::Char('?') => Action::Explain,
        KeyCode::Char('h') => Action::History,
        KeyCode::Char('t') => Action::Theme,
        KeyCode::Esc => Action::Back,
        KeyCode::Char('q' | 'Q') => Action::Quit,
        _ => return None,
    })
}

/// Maps a mouse event to an action: left click selects, the wheel scrolls.
pub fn mouse_action(mouse: MouseEvent) -> Option<Action> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => Some(Action::Click(mouse.column, mouse.row)),
        MouseEventKind::ScrollUp => Some(Action::ScrollUp),
        MouseEventKind::ScrollDown => Some(Action::ScrollDown),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Explain,
    History,
}

/// What the history panel shows, already worded by the event loop (it needs the clock and the
/// database; the view stays pure).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Timeline {
    /// The panel's opening sentence.
    pub summary: String,
    /// One line per event, oldest first.
    pub rows: Vec<String>,
    /// Where history is stored, or why it is not recorded.
    pub status: String,
}

/// What the event loop must do after an action.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Nothing beyond redrawing.
    None,
    Quit,
    /// Run the confirmed closing flow for this app.
    Close(ClassifiedGroup),
}

/// A one-line message shown in the footer until the next key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub warning: bool,
}

#[derive(Debug, Clone)]
pub struct App {
    pub snapshot: Snapshot,
    pub advice: Advice,
    pub history: History,
    pub theme: Theme,
    pub interval: Duration,
    /// Whether canfit can close processes on this OS.
    pub can_close: bool,
    /// Index into [`App::rows`].
    pub selected: usize,
    /// First visible row of the app list.
    pub offset: usize,
    /// The selected app's name, so the selection follows it when the list reorders.
    selected_name: Option<String>,
    pub overlay: Overlay,
    pub notice: Option<Notice>,
    pub timeline: Timeline,
    /// History recording is off for this run (`--no-history`, `CANFIT_NO_HISTORY`): the footer
    /// says so.
    pub history_off: bool,
}

impl App {
    pub fn new(
        snapshot: Snapshot,
        advice: Advice,
        theme: Theme,
        interval: Duration,
        can_close: bool,
    ) -> App {
        let mut history = History::default();
        history.record(&snapshot);
        let mut app = App {
            snapshot,
            advice,
            history,
            theme,
            interval,
            can_close,
            selected: 0,
            offset: 0,
            selected_name: None,
            overlay: Overlay::None,
            notice: None,
            history_off: false,
            timeline: Timeline::default(),
        };
        app.remember_selection();
        app
    }

    /// The app list: every group using memory, largest first, with its class.
    pub fn rows(&self) -> impl Iterator<Item = &ClassifiedGroup> {
        self.advice
            .groups
            .iter()
            .filter(|g| g.group.memory_bytes > 0)
    }

    pub fn row_count(&self) -> usize {
        self.rows().count()
    }

    pub fn selected_group(&self) -> Option<&ClassifiedGroup> {
        self.rows().nth(self.selected)
    }

    fn remember_selection(&mut self) {
        self.selected_name = self.selected_group().map(|g| g.group.name.clone());
    }

    /// Takes a new reading. The selection stays on the same app when it still exists.
    pub fn update(&mut self, snapshot: Snapshot, advice: Advice) {
        self.history.record(&snapshot);
        self.snapshot = snapshot;
        self.advice = advice;
        let found = self
            .selected_name
            .as_ref()
            .and_then(|name| self.rows().position(|g| &g.group.name == name));
        let count = self.row_count();
        self.selected = found.unwrap_or(self.selected.min(count.saturating_sub(1)));
        self.remember_selection();
    }

    fn select(&mut self, index: usize) {
        let count = self.row_count();
        self.selected = index.min(count.saturating_sub(1));
        self.remember_selection();
    }

    /// Keeps the selection inside a list window of `height` rows.
    pub fn scroll_into_view(&mut self, height: usize) {
        let height = height.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        let max_offset = self.row_count().saturating_sub(height);
        self.offset = self.offset.min(max_offset);
    }

    /// Applies an action. `list` is where the app list's rows are drawn (inside its border), used
    /// for mouse clicks and page sizes.
    pub fn apply(&mut self, action: Action, list: Rect) -> Effect {
        let page = usize::from(list.height.max(1));
        // Any key dismisses the last message.
        if !matches!(
            action,
            Action::Click(..) | Action::ScrollUp | Action::ScrollDown
        ) {
            self.notice = None;
        }
        match action {
            Action::Quit => return Effect::Quit,
            Action::Back => self.overlay = Overlay::None,
            Action::Explain => {
                self.overlay = match self.overlay {
                    Overlay::Explain => Overlay::None,
                    _ => Overlay::Explain,
                }
            }
            Action::History => {
                self.overlay = match self.overlay {
                    Overlay::History => Overlay::None,
                    _ => Overlay::History,
                }
            }
            Action::Theme => {
                self.theme = self.theme.cycled();
                self.notice = Some(Notice {
                    text: format!("Theme: {}.", self.theme.name.label()),
                    warning: false,
                });
            }
            Action::Up | Action::ScrollUp => self.select(self.selected.saturating_sub(1)),
            Action::Down | Action::ScrollDown => self.select(self.selected + 1),
            Action::PageUp => self.select(self.selected.saturating_sub(page)),
            Action::PageDown => self.select(self.selected + page),
            Action::Top => self.select(0),
            Action::Bottom => self.select(usize::MAX),
            Action::Click(x, y) => {
                if self.overlay == Overlay::None && list.contains(Position::new(x, y)) {
                    self.select(self.offset + usize::from(y - list.y));
                }
            }
            Action::Close => return self.close_selected(),
        }
        self.scroll_into_view(page);
        Effect::None
    }

    fn close_selected(&mut self) -> Effect {
        let Some(group) = self.selected_group().cloned() else {
            return Effect::None;
        };
        if group.class == SafetyClass::Never {
            self.notice = Some(Notice {
                text: format!(
                    "canfit never closes {}: {}.",
                    group.group.name, group.reason
                ),
                warning: true,
            });
            return Effect::None;
        }
        if !self.can_close {
            self.notice = Some(Notice {
                text: "canfit can't close apps on this system yet.".to_string(),
                warning: true,
            });
            return Effect::None;
        }
        self.overlay = Overlay::None;
        Effect::Close(group)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::advisor;
    use crate::advisor::classify::tests::{CANFIT_PID, desktop, snapshot_with};
    use crate::ui::theme::ThemeName;

    pub(crate) fn sample_app() -> App {
        let s = snapshot_with(desktop());
        let advice = advisor::advise(&s, CANFIT_PID, vec![]);
        App::new(
            s,
            advice,
            Theme::new(ThemeName::Dark, true),
            Duration::from_secs(1),
            true,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    const LIST: Rect = Rect::new(1, 10, 40, 4);

    fn names(app: &App) -> Vec<String> {
        app.rows().map(|g| g.group.name.clone()).collect()
    }

    #[test]
    fn keymap_resolves_the_k_conflict() {
        assert_eq!(key_action(key(KeyCode::Char('k'))), Some(Action::Up));
        assert_eq!(key_action(key(KeyCode::Char('j'))), Some(Action::Down));
        assert_eq!(key_action(key(KeyCode::Up)), Some(Action::Up));
        assert_eq!(key_action(key(KeyCode::Char('x'))), Some(Action::Close));
        assert_eq!(key_action(key(KeyCode::Char('?'))), Some(Action::Explain));
        assert_eq!(key_action(key(KeyCode::Char('h'))), Some(Action::History));
        assert_eq!(key_action(key(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(key_action(key(KeyCode::Char('t'))), Some(Action::Theme));
        assert_eq!(key_action(key(KeyCode::Esc)), Some(Action::Back));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        // Capital K, Delete and Enter close nothing.
        assert_eq!(key_action(key(KeyCode::Char('K'))), None);
        assert_eq!(key_action(key(KeyCode::Delete)), None);
        assert_eq!(key_action(key(KeyCode::Enter)), None);
        let mut release = key(KeyCode::Char('x'));
        release.kind = KeyEventKind::Release;
        assert_eq!(key_action(release), None);
    }

    #[test]
    fn mouse_clicks_and_wheel() {
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 12,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(mouse_action(click), Some(Action::Click(3, 12)));
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            ..click
        };
        assert_eq!(mouse_action(wheel), Some(Action::ScrollDown));
        let moved = MouseEvent {
            kind: MouseEventKind::Moved,
            ..click
        };
        assert_eq!(mouse_action(moved), None);
    }

    #[test]
    fn navigation_is_bounded_and_scrolls() {
        let mut app = sample_app();
        let count = app.row_count();
        assert!(count > LIST.height as usize);
        app.apply(Action::Up, LIST);
        assert_eq!(app.selected, 0);
        for _ in 0..5 {
            app.apply(Action::Down, LIST);
        }
        assert_eq!(app.selected, 5);
        assert_eq!(app.offset, 2, "selection kept in a 4-row window");
        app.apply(Action::Bottom, LIST);
        assert_eq!(app.selected, count - 1);
        app.apply(Action::PageUp, LIST);
        assert_eq!(app.selected, count - 5);
        app.apply(Action::Top, LIST);
        assert_eq!((app.selected, app.offset), (0, 0));
    }

    #[test]
    fn clicking_a_row_selects_it() {
        let mut app = sample_app();
        app.apply(Action::Click(5, LIST.y + 2), LIST);
        assert_eq!(app.selected, 2);
        // Outside the list: nothing changes.
        app.apply(Action::Click(5, 0), LIST);
        assert_eq!(app.selected, 2);
        app.apply(Action::ScrollDown, LIST);
        assert_eq!(app.selected, 3);
    }

    #[test]
    fn selection_follows_the_app_when_the_list_reorders() {
        let mut app = sample_app();
        app.apply(Action::Down, LIST);
        let name = app.selected_group().unwrap().group.name.clone();
        let mut snapshot = app.snapshot.clone();
        snapshot.app_groups.reverse();
        let advice = advisor::advise(&snapshot, CANFIT_PID, vec![]);
        app.update(snapshot, advice);
        assert_eq!(app.selected_group().unwrap().group.name, name);
        assert_eq!(app.history.memory.len(), 2);
    }

    #[test]
    fn close_asks_for_confirmation_only_for_closable_apps() {
        let mut app = sample_app();
        let rows = names(&app);
        let ollama = rows.iter().position(|n| n == "Ollama").unwrap();
        app.apply(Action::Top, LIST);
        for _ in 0..ollama {
            app.apply(Action::Down, LIST);
        }
        match app.apply(Action::Close, LIST) {
            Effect::Close(g) => assert_eq!(g.group.name, "Ollama"),
            other => panic!("expected a close request, got {other:?}"),
        }

        // The terminal hosting canfit is never closed: a message, no close request.
        let terminal = rows.iter().position(|n| n == "Terminal").unwrap();
        app.apply(Action::Top, LIST);
        for _ in 0..terminal {
            app.apply(Action::Down, LIST);
        }
        assert_eq!(app.apply(Action::Close, LIST), Effect::None);
        let notice = app.notice.clone().unwrap();
        assert!(notice.warning);
        assert!(
            notice.text.starts_with("canfit never closes Terminal"),
            "{notice:?}"
        );
        // The next key dismisses the message.
        app.apply(Action::Down, LIST);
        assert!(app.notice.is_none());
    }

    #[test]
    fn close_is_refused_where_canfit_cannot_close() {
        let mut app = sample_app();
        app.can_close = false;
        let ollama = names(&app).iter().position(|n| n == "Ollama").unwrap();
        for _ in 0..ollama {
            app.apply(Action::Down, LIST);
        }
        assert_eq!(app.apply(Action::Close, LIST), Effect::None);
        assert!(app.notice.as_ref().unwrap().text.contains("can't close"));
    }

    #[test]
    fn explain_toggles_and_escape_dismisses() {
        let mut app = sample_app();
        app.apply(Action::Explain, LIST);
        assert_eq!(app.overlay, Overlay::Explain);
        app.apply(Action::Explain, LIST);
        assert_eq!(app.overlay, Overlay::None);
        app.apply(Action::Explain, LIST);
        app.apply(Action::Back, LIST);
        assert_eq!(app.overlay, Overlay::None);
        // The history panel toggles the same way and replaces the explanation.
        app.apply(Action::Explain, LIST);
        app.apply(Action::History, LIST);
        assert_eq!(app.overlay, Overlay::History);
        app.apply(Action::History, LIST);
        assert_eq!(app.overlay, Overlay::None);
        app.apply(Action::History, LIST);
        app.apply(Action::Back, LIST);
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.apply(Action::Quit, LIST), Effect::Quit);
        app.apply(Action::Theme, LIST);
        assert_eq!(app.theme.name, ThemeName::Light);
    }
}
