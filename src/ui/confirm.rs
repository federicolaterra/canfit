//! The closing flow's questions inside the monitor: a dialog over the dashboard that shows the
//! full process list and waits for `y`. It implements [`Confirm`], so the monitor closes apps
//! through exactly the same flow as `canfit free` (ADR-013): full list, first "yes", graceful
//! request, wait, full list of survivors, second "yes", forced close.

use std::io;

use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

use super::app::App;
use super::{glyphs, view};
use crate::advisor::close::Confirm;

/// Where the dialog reads events from: the terminal, or a script in tests.
pub trait EventSource {
    /// Blocks until the next event.
    fn next(&mut self) -> io::Result<Event>;
}

pub struct TerminalEvents;

impl EventSource for TerminalEvents {
    fn next(&mut self) -> io::Result<Event> {
        event::read()
    }
}

pub struct DialogConfirm<'a, B: Backend> {
    pub terminal: &'a mut Terminal<B>,
    pub events: &'a mut dyn EventSource,
    /// The dashboard drawn under the dialog.
    pub app: &'a App,
    /// What the flow last showed (the process list), one entry per line.
    shown: Vec<String>,
}

impl<'a, B: Backend> DialogConfirm<'a, B> {
    pub fn new(
        terminal: &'a mut Terminal<B>,
        events: &'a mut dyn EventSource,
        app: &'a App,
    ) -> Self {
        DialogConfirm {
            terminal,
            events,
            app,
            shown: Vec::new(),
        }
    }

    fn draw(&mut self, question: &str, scroll: u16) -> bool {
        let app = self.app;
        let shown = &self.shown;
        self.terminal
            .draw(|f| {
                view::draw(f, app);
                view::draw_dialog(f, app.theme, shown, question, scroll);
                glyphs::finish(f);
            })
            .is_ok()
    }

    fn busy(&mut self, text: &str) {
        let app = self.app;
        let _ = self.terminal.draw(|f| {
            view::draw(f, app);
            view::draw_busy(f, app.theme, text);
            glyphs::finish(f);
        });
    }
}

impl<B: Backend> Confirm for DialogConfirm<'_, B> {
    fn show(&mut self, text: &str) {
        self.shown = text.lines().map(str::to_string).collect();
    }

    /// Only `y` or `Y` is a yes. Scrolling keys move the list; resizes and mouse movement redraw;
    /// any other key, Ctrl-C or a read error is a no.
    fn confirm(&mut self, question: &str) -> bool {
        let mut scroll: u16 = 0;
        loop {
            if !self.draw(question, scroll) {
                return false;
            }
            let event = match self.events.next() {
                Ok(e) => e,
                Err(_) => return false,
            };
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Press => {}
                Event::Key(key) if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
                Event::Key(key) => match key.code {
                    KeyCode::Char('y' | 'Y') => {
                        let forced = question.contains("Force");
                        self.busy(if forced {
                            "Forcing the remaining processes to quit…"
                        } else {
                            "Asking the apps to quit. canfit waits up to 10 seconds for them; \
                             they may ask you to save first."
                        });
                        return true;
                    }
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::PageDown => scroll = scroll.saturating_add(10),
                    _ => return false,
                },
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => scroll = scroll.saturating_sub(1),
                    MouseEventKind::ScrollDown => scroll = scroll.saturating_add(1),
                    _ => {}
                },
                _ => {}
            }
            // The dialog clamps the offset; keep ours from running away past the list.
            scroll = scroll.min(self.shown.len() as u16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::tests::sample_app;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyEvent, MouseEvent};
    use std::collections::VecDeque;

    struct Script(VecDeque<Event>);

    impl EventSource for Script {
        fn next(&mut self) -> io::Result<Event> {
            self.0
                .pop_front()
                .ok_or_else(|| io::Error::other("script ended"))
        }
    }

    fn key(c: KeyCode) -> Event {
        Event::Key(KeyEvent::new(c, KeyModifiers::NONE))
    }

    fn run(events: Vec<Event>, lines: usize) -> (bool, String) {
        let app = sample_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut script = Script(events.into());
        let mut ui = DialogConfirm::new(&mut terminal, &mut script, &app);
        let list: String = (0..lines)
            .map(|i| format!("    Helper {i} (pid {})\n", 1000 + i))
            .collect();
        ui.show(&format!(
            "1 app ({lines} processes) will be asked to quit:\n{list}"
        ));
        let answer = ui.confirm("Ask them to quit now?");
        let screen = terminal.backend().to_string();
        (answer, screen)
    }

    #[test]
    fn only_y_is_yes() {
        assert!(run(vec![key(KeyCode::Char('y'))], 2).0);
        assert!(run(vec![key(KeyCode::Char('Y'))], 2).0);
        for no in [
            KeyCode::Char('n'),
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Char('x'),
            KeyCode::Char('q'),
        ] {
            assert!(!run(vec![key(no)], 2).0, "{no:?}");
        }
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!run(vec![ctrl_c], 2).0);
        // The event source failing is a no.
        assert!(!run(vec![], 2).0);
    }

    #[test]
    fn resizes_mouse_and_scrolling_do_not_answer() {
        let wheel = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        let events = vec![
            Event::Resize(80, 24),
            wheel,
            key(KeyCode::Down),
            key(KeyCode::Up),
            key(KeyCode::Char('y')),
        ];
        assert!(run(events, 40).0);
    }

    #[test]
    fn the_dialog_shows_the_list_and_how_to_scroll_it() {
        let (_, screen) = run(vec![key(KeyCode::Char('n'))], 40);
        assert!(screen.contains("Close apps"), "{screen}");
        assert!(screen.contains("1 app (40 processes) will be asked to quit:"));
        assert!(screen.contains("Helper 0 (pid 1000)"));
        assert!(screen.contains("Ask them to quit now?"));
        assert!(screen.contains("more below"), "{screen}");
    }
}
