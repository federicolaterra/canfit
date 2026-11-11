//! Terminal UI (ADR-002, spec §6.1, §11): the live monitor that `canfit` opens on a terminal, and
//! the coloured cards the fit check prints. ratatui + crossterm, cards with rounded borders,
//! colours paired with symbols, dark / light / high-contrast themes, `NO_COLOR`.
//!
//! Layout and state are pure ([`app`], [`view`]) and tested with ratatui's `TestBackend`; this
//! module owns the terminal: raw mode, alternate screen, mouse capture, and restoring all of it on
//! exit, on errors and on panics.

pub mod app;
pub mod confirm;
pub mod glyphs;
pub mod history;
pub mod inline;
pub mod theme;
pub mod view;

use std::io::{self, IsTerminal, Stdout};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Local;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::advisor::classify::ClassifiedGroup;
use crate::advisor::close::{self, CloseError, CloseOptions, CloseReport};
use crate::advisor::plan::Suggestion;
use crate::advisor::{self, Advice, config};
use crate::cli::{EXIT_RUNTIME, card, history as history_cli, report::ONE_SHOT_WINDOW};
use crate::core::sampler::Sampler;
use crate::core::{Provenance, Snapshot, format_bytes};
use crate::history::recorder::{PANEL_WINDOW_MS, Recorder};
use crate::history::{self as history_store, HistorySettings};
use crate::platform::{self, ProcessControl, SystemSource};
use app::{App, Effect, Notice, Overlay, Timeline};
use confirm::{DialogConfirm, TerminalEvents};
use theme::{Theme, ThemeName};

/// The theme for cards printed on stdout: `None` (plain text) when stdout is not a terminal or
/// colour is disabled.
pub fn stdout_theme(name: ThemeName) -> Option<Theme> {
    let theme = Theme::from_env(name);
    (io::stdout().is_terminal() && theme.color && glyphs::ansi_supported()).then_some(theme)
}

/// The theme for text canfit writes on stderr (`canfit watch`, whose stdout belongs to the
/// command it runs): `None` when stderr is not a terminal or colour is disabled.
pub fn stderr_theme(name: ThemeName) -> Option<Theme> {
    let theme = Theme::from_env(name);
    (io::stderr().is_terminal() && theme.color && glyphs::ansi_supported()).then_some(theme)
}

/// Whether the live monitor can run: both stdin and stdout are terminals.
pub fn monitor_possible() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Puts the terminal back: leaves the alternate screen, stops mouse capture, shows the cursor and
/// leaves raw mode. Safe to call more than once.
fn restore_terminal() {
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        ratatui::crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
}

/// Owns the terminal setup; dropping it restores the terminal, also while unwinding a panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<TerminalGuard> {
        // Restore before the panic message is printed, so it is readable.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
        enable_raw_mode()?;
        let guard = TerminalGuard;
        // Clear with an escape sequence: ratatui's own clear asks the terminal for the cursor
        // position, which some terminals never answer.
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            Clear(ClearType::All)
        )?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// The per-process scan is the expensive part of a reading (about 10 ms for 700 processes on an
/// M1 Pro), so the app list is rescanned at most this often; memory, swap, CPU and pressure follow
/// the refresh interval. Keeps the monitor under 1 % CPU at the default 1 s refresh.
pub const PROCESS_SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// Reads one snapshot from `source` with the per-process scan, and classifies it.
fn read(
    source: &mut dyn SystemSource,
    sampler: &mut Sampler,
    protected: &[String],
) -> Result<(Snapshot, Advice), String> {
    let snapshot = sampler.record(source.sample(true).map_err(|e| e.to_string())?);
    let advice = advisor::advise(&snapshot, std::process::id(), protected.to_vec());
    Ok((snapshot, advice))
}

/// Reads the system-wide values only, keeping the previous app list and classification; the
/// "Why?" paragraph is rebuilt for the new pressure.
fn read_light(
    source: &mut dyn SystemSource,
    sampler: &mut Sampler,
    previous: &App,
) -> Result<(Snapshot, Advice), String> {
    let snapshot = sampler.record(source.sample(false).map_err(|e| e.to_string())?);
    Ok(carry_over(snapshot, previous))
}

fn carry_over(mut snapshot: Snapshot, previous: &App) -> (Snapshot, Advice) {
    snapshot.processes = previous.snapshot.processes.clone();
    snapshot.app_groups = previous.snapshot.app_groups.clone();
    let groups = previous.advice.groups.clone();
    let suggestions = previous.advice.suggestions.clone();
    let why = advisor::why::explain(&snapshot, &groups, &suggestions);
    (
        snapshot,
        Advice {
            groups,
            suggestions,
            why,
        },
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// History recording for one run of the monitor (ADR-018). Failing to record never stops the
/// monitor: it says why in the footer and in the history panel.
struct Recording {
    recorder: Option<Recorder>,
    /// The last line of the history panel: where history goes, or why it doesn't.
    status: String,
    /// With recording off for this run (`--no-history`): the database whose past events the
    /// history panel still shows, read-only.
    read_only: Option<std::path::PathBuf>,
}

impl Recording {
    fn start(settings: &HistorySettings) -> (Recording, Option<Notice>) {
        let off = |status: String, warn: bool| {
            let notice = warn.then(|| Notice {
                text: status.clone(),
                warning: true,
            });
            (
                Recording {
                    recorder: None,
                    status,
                    read_only: None,
                },
                notice,
            )
        };
        if let Some(reason) = history_store::recording_off() {
            let read_only = history_store::database_path();
            let status = format!(
                "History is off for this run ({}): nothing is recorded; past events are shown \
                 read-only.",
                reason.switch()
            );
            return (
                Recording {
                    recorder: None,
                    status,
                    read_only,
                },
                None,
            );
        }
        if !settings.enabled {
            return off(
                "History is off: [history] enabled = false in canfit's config.toml.".to_string(),
                false,
            );
        }
        let Some(path) = history_store::database_path() else {
            return off(
                "History is off: canfit found no data directory on this system.".to_string(),
                true,
            );
        };
        match Recorder::start(&path, settings, now_ms()) {
            Ok(recorder) => (
                Recording {
                    recorder: Some(recorder),
                    read_only: None,
                    status: format!(
                        "Recorded while the monitor is open, in {}. canfit history shows more.",
                        path.display()
                    ),
                },
                None,
            ),
            Err(err) => off(format!("History is off: {err}"), true),
        }
    }

    /// Records one reading; on a database error, recording stops and the footer says why.
    fn observe(&mut self, snapshot: &Snapshot) -> Option<Notice> {
        let recorder = self.recorder.as_mut()?;
        let err = recorder.observe(snapshot).err()?;
        self.recorder = None;
        self.status = format!("History stopped: {err}");
        Some(Notice {
            text: self.status.clone(),
            warning: true,
        })
    }

    /// The history panel's content, worded for the local clock.
    fn timeline(&self) -> Timeline {
        let now = Local::now();
        let until = now_ms();
        let since = until.saturating_sub(PANEL_WINDOW_MS);
        let (events, recorded): (Vec<_>, u64) = if let Some(recorder) = &self.recorder {
            let events = recorder
                .recent()
                .iter()
                .filter(|e| e.end_unix_ms.unwrap_or(e.last_seen_unix_ms) >= since)
                .cloned()
                .collect();
            (events, recorder.recorded_ms(since, until).unwrap_or(0))
        } else if let Some(path) = &self.read_only {
            // Recording is off: past events, read without writing anything.
            match history_cli::load(path, since, until) {
                Ok((events, recorded, _)) => (events, recorded),
                Err(err) => {
                    return Timeline {
                        summary: "canfit is not recording pressure events in this run.".to_string(),
                        rows: Vec::new(),
                        status: format!("{} {err}", self.status),
                    };
                }
            }
        } else {
            return Timeline {
                summary: "canfit is not recording pressure events in this run.".to_string(),
                rows: Vec::new(),
                status: self.status.clone(),
            };
        };
        let rows: Vec<_> = history_cli::rows(&events, &now)
            .into_iter()
            .map(Some)
            .collect();
        Timeline {
            summary: history_cli::summary_sentence(&events, "in the last 24 hours", recorded, &now),
            rows: card::lines(&rows),
            status: self.status.clone(),
        }
    }

    fn finish(self) {
        if let Some(recorder) = self.recorder {
            // The next start closes the event anyway if this write fails.
            let _ = recorder.finish(now_ms());
        }
    }
}

/// `canfit` on a terminal: the live monitor. Returns 0 on quit, 3 when the system can't be read.
pub fn run_monitor(interval: Duration, theme: ThemeName) -> ExitCode {
    match monitor(interval, theme) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn monitor(interval: Duration, theme: ThemeName) -> Result<(), String> {
    // Everything that can fail before drawing fails here, on the normal screen.
    let mut source = platform::native().map_err(|e| e.to_string())?;
    let (config, _) = config::load().map_err(|e| e.to_string())?;
    let protected = config.processes.protected;
    let (mut recording, history_notice) = Recording::start(&config.history);
    let mut sampler = Sampler::new(64, Duration::from_secs(10));
    sampler.record(source.sample(false).map_err(|e| e.to_string())?);
    thread::sleep(ONE_SHOT_WINDOW);
    let (snapshot, advice) = read(source.as_mut(), &mut sampler, &protected)?;
    let mut control = platform::process_control();
    let observe_notice = recording.observe(&snapshot);
    let mut app = App::new(
        snapshot,
        advice,
        Theme::from_env(theme),
        interval,
        control.is_some(),
    );
    app.notice = history_notice.or(observe_notice);
    app.history_off = history_store::recording_off().is_some();

    let _guard =
        TerminalGuard::enter().map_err(|e| format!("canfit could not set up the terminal: {e}"))?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .map_err(|e| format!("canfit could not set up the terminal: {e}"))?;

    let mut next_tick = Instant::now() + interval;
    let mut last_scan = Instant::now();
    let mut dirty = true;
    loop {
        if dirty {
            terminal
                .draw(|f| {
                    view::draw(f, &app);
                    glyphs::finish(f);
                })
                .map_err(|e| e.to_string())?;
            dirty = false;
        }
        let timeout = next_tick.saturating_duration_since(Instant::now());
        if event::poll(timeout).map_err(|e| e.to_string())? {
            let action = match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) => app::key_action(key),
                Event::Mouse(mouse) => app::mouse_action(mouse),
                Event::Resize(..) => {
                    dirty = true;
                    None
                }
                _ => None,
            };
            let Some(action) = action else { continue };
            dirty = true;
            let size = terminal.size().map_err(|e| e.to_string())?;
            let list = view::layout(size.into(), &app).list_rows();
            match app.apply(action, list) {
                Effect::None => {
                    if app.overlay == Overlay::History {
                        app.timeline = recording.timeline();
                    }
                }
                Effect::Quit => break,
                Effect::Close(group) => {
                    app.notice = Some(close_group(
                        &mut terminal,
                        &app,
                        &group,
                        control.as_deref_mut(),
                    ));
                    // Show the effect of closing right away, with a fresh app list.
                    next_tick = Instant::now();
                    last_scan = Instant::now() - PROCESS_SCAN_INTERVAL;
                }
            }
        }
        if Instant::now() >= next_tick {
            // Half an interval of slack, so a 1 s refresh scans every second tick exactly.
            let scan = last_scan.elapsed() + interval / 2 >= PROCESS_SCAN_INTERVAL;
            let reading = if scan {
                last_scan = Instant::now();
                read(source.as_mut(), &mut sampler, &protected)
            } else {
                read_light(source.as_mut(), &mut sampler, &app)
            };
            match reading {
                Ok((snapshot, advice)) => {
                    if let Some(notice) = recording.observe(&snapshot) {
                        app.notice = Some(notice);
                    }
                    app.update(snapshot, advice);
                    if app.overlay == Overlay::History {
                        app.timeline = recording.timeline();
                    }
                }
                Err(message) => {
                    app.notice = Some(Notice {
                        text: message,
                        warning: true,
                    })
                }
            }
            let size = terminal.size().map_err(|e| e.to_string())?;
            let rows = view::layout(size.into(), &app).list_rows();
            app.scroll_into_view(usize::from(rows.height));
            dirty = true;
            // Skip missed ticks instead of catching up (after a long close, or a suspended laptop).
            next_tick = (next_tick + interval).max(Instant::now());
        }
    }
    recording.finish();
    Ok(())
}

/// The suggestion the closing flow needs for one app of the list, with the commands to close it
/// by hand (shown in the dialog).
fn as_suggestion(g: &ClassifiedGroup, snapshot: &crate::core::Snapshot) -> Suggestion {
    Suggestion {
        name: g.group.name.clone(),
        class: g.class,
        reason: g.reason.clone(),
        memory_bytes: g.group.memory_bytes,
        memory_provenance: Provenance::Measured,
        process_count: g.group.process_count,
        unreadable_count: g.group.unreadable_count,
        pids: g.group.pids.clone(),
        manual_commands: crate::advisor::manual::commands(
            &g.group,
            g.class,
            &snapshot.processes,
            &snapshot.platform.os,
        ),
    }
}

/// Runs the confirmed closing flow (ADR-013) for one app with the dialog, and says what happened.
fn close_group(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &App,
    group: &ClassifiedGroup,
    control: Option<&mut (dyn ProcessControl + 'static)>,
) -> Notice {
    let Some(control) = control else {
        return Notice {
            text: "canfit can't close apps on this system yet.".to_string(),
            warning: true,
        };
    };
    let mut events = TerminalEvents;
    let mut ui = DialogConfirm::new(terminal, &mut events, app);
    let result = close::close_apps(
        &[as_suggestion(group, &app.snapshot)],
        &app.snapshot.processes,
        control,
        &mut ui,
        CloseOptions::default(),
    );
    close_notice(&group.group.name, group.group.memory_bytes, result)
}

/// One footer line describing the outcome of the closing flow.
fn close_notice(name: &str, memory_bytes: u64, result: Result<CloseReport, CloseError>) -> Notice {
    let (text, warning) = match result {
        Ok(r) if r.cancelled => (
            format!("Nothing was closed; {name} is still running."),
            false,
        ),
        Ok(r) if r.still_running.is_empty() && r.errors.is_empty() => {
            let forced = if r.forced.is_empty() {
                String::new()
            } else {
                format!(", {} forced", r.forced.len())
            };
            (
                format!(
                    "Closed {name} ({} of {} processes{forced}); about {} should be freed \
                     (estimated).",
                    r.closed.len(),
                    r.targets.len(),
                    format_bytes(memory_bytes)
                ),
                false,
            )
        }
        Ok(r) => {
            let hint = if r.force_declined {
                " It may be waiting for you to save; check its windows."
            } else {
                ""
            };
            let errors = match r.errors.first() {
                Some((t, e)) => format!(" Could not close {} (pid {}): {e}.", t.name, t.pid),
                None => String::new(),
            };
            (
                format!(
                    "{name}: {} of {} processes closed, {} still running.{hint}{errors}",
                    r.closed.len(),
                    r.targets.len(),
                    r.still_running.len()
                ),
                true,
            )
        }
        Err(CloseError::Empty) => (format!("{name} has already quit."), false),
        Err(CloseError::NeverClass(n)) => (format!("canfit never closes {n}."), true),
    };
    Notice { text, warning }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::close::Target;
    use ratatui::backend::TestBackend;

    fn target(pid: u32) -> Target {
        Target {
            app: "Slack".into(),
            pid,
            name: "Slack".into(),
            root: pid == 1,
        }
    }

    #[test]
    fn close_outcomes_are_explained() {
        let cancelled = CloseReport {
            cancelled: true,
            ..CloseReport::default()
        };
        let n = close_notice("Slack", 0, Ok(cancelled));
        assert_eq!(n.text, "Nothing was closed; Slack is still running.");

        let done = CloseReport {
            targets: vec![target(1), target(2)],
            closed: vec![target(1), target(2)],
            ..CloseReport::default()
        };
        let n = close_notice("Slack", 1024 * 1024 * 1024, Ok(done));
        assert!(!n.warning);
        assert_eq!(
            n.text,
            "Closed Slack (2 of 2 processes); about 1.0 GB should be freed (estimated)."
        );

        let stuck = CloseReport {
            targets: vec![target(1)],
            still_running: vec![target(1)],
            force_declined: true,
            ..CloseReport::default()
        };
        let n = close_notice("Slack", 0, Ok(stuck));
        assert!(n.warning);
        assert!(
            n.text.contains("1 still running. It may be waiting"),
            "{}",
            n.text
        );

        assert!(
            close_notice("Slack", 0, Err(CloseError::Empty))
                .text
                .contains("already quit")
        );
    }

    #[test]
    fn suggestions_carry_the_group_and_its_class() {
        let app = app::tests::sample_app();
        let g = app.rows().find(|g| g.group.name == "Slack").unwrap();
        let s = as_suggestion(g, &app.snapshot);
        assert_eq!(s.class, g.class);
        assert_eq!(s.pids, g.group.pids);
        assert_eq!(s.memory_provenance, Provenance::Measured);
        // The dialog also says how to close it by hand, graceful before forced.
        let commands: Vec<&str> = s
            .manual_commands
            .iter()
            .map(|c| c.command.as_str())
            .collect();
        assert!(
            commands.iter().any(|c| c.starts_with("kill ")),
            "{commands:?}"
        );
        assert!(s.manual_commands.last().unwrap().forced);
    }

    #[test]
    fn light_readings_keep_the_app_list_and_refresh_the_explanation() {
        let app = app::tests::sample_app();
        let (_, mut light) = crate::core::sampler::tests::replay("critical");
        light.processes.clear();
        light.app_groups.clear();
        let (snapshot, advice) = carry_over(light, &app);
        assert_eq!(snapshot.app_groups, app.snapshot.app_groups);
        assert_eq!(snapshot.processes.len(), app.snapshot.processes.len());
        assert_eq!(advice.groups, app.advice.groups);
        assert!(
            advice.why.starts_with("Memory is critical"),
            "{}",
            advice.why
        );
    }

    #[test]
    fn the_monitor_draws_on_any_backend() {
        let app = app::tests::sample_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| view::draw(f, &app)).unwrap();
    }
}
