//! Confirmed closing (spec §10, ADR-013): show the full list, ask, close gracefully, wait, and
//! force only after a second confirmation.
//!
//! The flow talks to the user only through [`Confirm`] and to the OS only through
//! [`ProcessControl`], so tests prove that no signal is sent without a "yes".

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::thread;
use std::time::{Duration, Instant};

use super::SafetyClass;
use super::plan::Suggestion;
use crate::core::Process;
use crate::platform::{CloseSignal, ProcessControl};

/// How the closing flow asks the user.
pub trait Confirm {
    /// Shows information (the full list of processes).
    fn show(&mut self, text: &str);
    /// Asks a yes/no question; anything but an explicit yes is a no.
    fn confirm(&mut self, question: &str) -> bool;
}

#[derive(Debug, Clone, Copy)]
pub struct CloseOptions {
    /// How long to wait for apps to quit after the graceful request.
    pub graceful_timeout: Duration,
    /// How long to wait for processes to disappear after a forced close.
    pub force_timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for CloseOptions {
    fn default() -> Self {
        CloseOptions {
            graceful_timeout: Duration::from_secs(10),
            force_timeout: Duration::from_secs(3),
            poll_interval: Duration::from_millis(100),
        }
    }
}

/// One process that will be (or was) asked to close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub app: String,
    pub pid: u32,
    pub name: String,
    /// True for the processes that receive the graceful request: those whose parent is not part
    /// of the same app. Helpers are left to their app, which closes them itself.
    pub root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseError {
    /// A never-class app reached the closing flow: refused without asking.
    NeverClass(String),
    /// Nothing to close.
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CloseReport {
    /// The user declined the first confirmation; nothing was signalled.
    pub cancelled: bool,
    /// The user was asked to force-close and declined.
    pub force_declined: bool,
    pub targets: Vec<Target>,
    /// Processes that were closed (gracefully or forced).
    pub closed: Vec<Target>,
    /// Processes that needed a forced close.
    pub forced: Vec<Target>,
    pub still_running: Vec<Target>,
    /// Signals the OS refused, with its reason.
    pub errors: Vec<(Target, String)>,
}

fn targets(apps: &[Suggestion], processes: &[Process]) -> Vec<Target> {
    let by_pid: HashMap<u32, &Process> = processes.iter().map(|p| (p.pid, p)).collect();
    let mut out = Vec::new();
    for app in apps {
        let pids: BTreeSet<u32> = app.pids.iter().copied().collect();
        for pid in &app.pids {
            // A pid missing from the snapshot can't be verified, so it is not touched.
            let Some(p) = by_pid.get(pid) else { continue };
            let root = p.ppid.is_none_or(|ppid| !pids.contains(&ppid));
            out.push(Target {
                app: app.name.clone(),
                pid: p.pid,
                name: p.name.clone(),
                root,
            });
        }
    }
    out
}

/// The complete list of processes, grouped by app, as shown before each confirmation.
pub fn describe(targets: &[Target]) -> String {
    let mut out = String::new();
    let mut apps: Vec<&str> = Vec::new();
    for t in targets {
        if !apps.contains(&t.app.as_str()) {
            apps.push(&t.app);
        }
    }
    for app in apps {
        let list: Vec<&Target> = targets.iter().filter(|t| t.app == app).collect();
        let processes = match list.len() {
            1 => "1 process".to_string(),
            n => format!("{n} processes"),
        };
        let _ = writeln!(out, "  {app} ({processes}):");
        for t in list {
            let _ = writeln!(out, "    {} (pid {})", t.name, t.pid);
        }
    }
    out
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn running(control: &dyn ProcessControl, targets: &[Target]) -> Vec<Target> {
    targets
        .iter()
        .filter(|t| control.is_running(t.pid, &t.name))
        .cloned()
        .collect()
}

fn wait_until_gone(
    control: &dyn ProcessControl,
    targets: &[Target],
    timeout: Duration,
    poll: Duration,
) -> Vec<Target> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = running(control, targets);
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        thread::sleep(poll);
    }
}

/// Closes the given apps after the user confirms. Never-class apps are refused before anything
/// is asked or signalled.
pub fn close_apps(
    apps: &[Suggestion],
    processes: &[Process],
    control: &mut dyn ProcessControl,
    ui: &mut dyn Confirm,
    options: CloseOptions,
) -> Result<CloseReport, CloseError> {
    if let Some(app) = apps.iter().find(|a| a.class == SafetyClass::Never) {
        return Err(CloseError::NeverClass(app.name.clone()));
    }
    let targets = running(control, &targets(apps, processes));
    if targets.is_empty() {
        return Err(CloseError::Empty);
    }
    let mut report = CloseReport {
        targets: targets.clone(),
        ..CloseReport::default()
    };

    let app_count = apps
        .iter()
        .filter(|a| targets.iter().any(|t| t.app == a.name))
        .count();
    let mut by_hand = String::new();
    for app in apps.iter().filter(|a| !a.manual_commands.is_empty()) {
        by_hand.push_str(&super::manual::describe(&app.name, &app.manual_commands));
    }
    if !by_hand.is_empty() {
        by_hand.insert_str(
            0,
            "\nTo close them yourself instead (canfit never runs these), safest first:\n",
        );
    }
    ui.show(&format!(
        "{} ({}) will be asked to quit:\n{}{by_hand}",
        plural(app_count, "app", "apps"),
        plural(targets.len(), "process", "processes"),
        describe(&targets)
    ));
    let question = if apps.iter().any(|a| a.class == SafetyClass::Caution) {
        "Ask them to quit now? Apps marked ▲ may lose unsaved work."
    } else {
        "Ask them to quit now?"
    };
    if !ui.confirm(question) {
        report.cancelled = true;
        return Ok(report);
    }

    for t in targets.iter().filter(|t| t.root) {
        if !control.is_running(t.pid, &t.name) {
            continue;
        }
        if let Err(e) = control.signal(t.pid, CloseSignal::Graceful) {
            report.errors.push((t.clone(), e));
        }
    }
    let left = wait_until_gone(
        control,
        &targets,
        options.graceful_timeout,
        options.poll_interval,
    );
    if left.is_empty() {
        report.closed = targets;
        return Ok(report);
    }

    ui.show(&format!(
        "{} still running after {} s:\n{}",
        plural(left.len(), "process is", "processes are"),
        options.graceful_timeout.as_secs(),
        describe(&left)
    ));
    if !ui.confirm("Force them to quit? Unsaved work in them will be lost.") {
        report.force_declined = true;
        report.closed = targets
            .iter()
            .filter(|t| !left.contains(t))
            .cloned()
            .collect();
        report.still_running = left;
        return Ok(report);
    }
    for t in &left {
        if !control.is_running(t.pid, &t.name) {
            continue;
        }
        match control.signal(t.pid, CloseSignal::Force) {
            Ok(()) => report.forced.push(t.clone()),
            Err(e) => report.errors.push((t.clone(), e)),
        }
    }
    let still = wait_until_gone(control, &left, options.force_timeout, options.poll_interval);
    report.closed = targets
        .iter()
        .filter(|t| !still.contains(t))
        .cloned()
        .collect();
    report.still_running = still;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::classify::tests::{MB, ME, proc};
    use crate::core::Provenance;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// A fake process table: records every signal; processes listed in `ignore_term` survive
    /// the graceful request.
    struct FakeControl {
        alive: RefCell<HashMap<u32, String>>,
        ignore_term: BTreeSet<u32>,
        signals: Vec<(u32, CloseSignal)>,
        /// Children that exit when their parent exits (helpers).
        children: HashMap<u32, Vec<u32>>,
    }

    impl FakeControl {
        fn new(processes: &[Process]) -> Self {
            let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
            for p in processes {
                if let Some(ppid) = p.ppid {
                    children.entry(ppid).or_default().push(p.pid);
                }
            }
            FakeControl {
                alive: RefCell::new(processes.iter().map(|p| (p.pid, p.name.clone())).collect()),
                ignore_term: BTreeSet::new(),
                signals: Vec::new(),
                children,
            }
        }

        fn kill_tree(&self, pid: u32) {
            self.alive.borrow_mut().remove(&pid);
            for child in self.children.get(&pid).cloned().unwrap_or_default() {
                self.kill_tree(child);
            }
        }
    }

    impl ProcessControl for FakeControl {
        fn is_running(&self, pid: u32, name: &str) -> bool {
            self.alive.borrow().get(&pid).is_some_and(|n| n == name)
        }

        fn signal(&mut self, pid: u32, signal: CloseSignal) -> Result<(), String> {
            self.signals.push((pid, signal));
            if signal == CloseSignal::Force || !self.ignore_term.contains(&pid) {
                self.kill_tree(pid);
            }
            Ok(())
        }
    }

    /// Scripted answers; records everything shown and asked.
    struct Script {
        answers: VecDeque<bool>,
        shown: Vec<String>,
        asked: Vec<String>,
    }

    impl Script {
        fn new(answers: &[bool]) -> Self {
            Script {
                answers: answers.iter().copied().collect(),
                shown: Vec::new(),
                asked: Vec::new(),
            }
        }
    }

    impl Confirm for Script {
        fn show(&mut self, text: &str) {
            self.shown.push(text.to_string());
        }
        fn confirm(&mut self, question: &str) -> bool {
            self.asked.push(question.to_string());
            self.answers.pop_front().unwrap_or(false)
        }
    }

    fn app(name: &str, class: SafetyClass, pids: &[u32]) -> Suggestion {
        Suggestion {
            name: name.to_string(),
            class,
            reason: "test".to_string(),
            memory_bytes: 500 * MB,
            memory_provenance: Provenance::Measured,
            process_count: pids.len(),
            unreadable_count: 0,
            pids: pids.to_vec(),
            manual_commands: Vec::new(),
        }
    }

    fn table() -> Vec<Process> {
        vec![
            proc(
                300,
                1,
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                ME,
                MB,
            ),
            proc(
                301,
                300,
                "/Applications/Google Chrome.app/Contents/Helpers/Renderer",
                ME,
                MB,
            ),
            proc(
                302,
                300,
                "/Applications/Google Chrome.app/Contents/Helpers/GPU",
                ME,
                MB,
            ),
            proc(
                400,
                1,
                "/Applications/Slack.app/Contents/MacOS/Slack",
                ME,
                MB,
            ),
        ]
    }

    fn fast() -> CloseOptions {
        CloseOptions {
            graceful_timeout: Duration::from_millis(30),
            force_timeout: Duration::from_millis(30),
            poll_interval: Duration::from_millis(5),
        }
    }

    fn apps() -> Vec<Suggestion> {
        vec![
            app("Google Chrome", SafetyClass::Safe, &[300, 301, 302]),
            app("Slack", SafetyClass::Safe, &[400]),
        ]
    }

    #[test]
    fn declining_sends_no_signal_after_showing_the_full_list() {
        let processes = table();
        let mut control = FakeControl::new(&processes);
        for answers in [&[false][..], &[]] {
            let mut ui = Script::new(answers);
            let report = close_apps(&apps(), &processes, &mut control, &mut ui, fast()).unwrap();
            assert!(report.cancelled);
            assert!(report.closed.is_empty());
            assert_eq!(ui.asked.len(), 1);
            let shown = &ui.shown[0];
            assert!(
                shown.starts_with("2 apps (4 processes) will be asked to quit:"),
                "{shown}"
            );
            for p in &processes {
                assert!(
                    shown.contains(&format!("{} (pid {})", p.name, p.pid)),
                    "{shown}"
                );
            }
        }
        assert!(control.signals.is_empty());
    }

    #[test]
    fn the_first_question_also_says_how_to_close_them_by_hand() {
        let processes = table();
        let mut control = FakeControl::new(&processes);
        let mut ui = Script::new(&[false]);
        let mut list = apps();
        list[1].manual_commands = vec![super::super::manual::ManualCommand {
            command: "kill 400".to_string(),
            note: "asks it to quit".to_string(),
            forced: false,
        }];
        close_apps(&list, &processes, &mut control, &mut ui, fast()).unwrap();
        let shown = &ui.shown[0];
        assert!(
            shown.contains("To close them yourself instead (canfit never runs these)"),
            "{shown}"
        );
        assert!(
            shown.contains("  Slack:\n    kill 400   (asks it to quit)"),
            "{shown}"
        );
        assert!(control.signals.is_empty());
    }

    #[test]
    fn graceful_close_signals_only_app_roots_and_asks_once() {
        let processes = table();
        let mut control = FakeControl::new(&processes);
        let mut ui = Script::new(&[true]);
        let report = close_apps(&apps(), &processes, &mut control, &mut ui, fast()).unwrap();
        assert_eq!(
            control.signals,
            [(300, CloseSignal::Graceful), (400, CloseSignal::Graceful)]
        );
        assert_eq!(report.closed.len(), 4);
        assert!(report.still_running.is_empty() && report.forced.is_empty());
        assert_eq!(ui.asked.len(), 1);
    }

    #[test]
    fn forcing_needs_a_second_confirmation() {
        let processes = table();
        let mut control = FakeControl::new(&processes);
        control.ignore_term.insert(400);
        let mut ui = Script::new(&[true, false]);
        let report = close_apps(&apps(), &processes, &mut control, &mut ui, fast()).unwrap();
        assert!(report.force_declined);
        assert!(
            control
                .signals
                .iter()
                .all(|&(_, s)| s == CloseSignal::Graceful)
        );
        assert_eq!(report.still_running.len(), 1);
        assert_eq!(report.still_running[0].pid, 400);
        assert_eq!(report.closed.len(), 3);
        assert_eq!(ui.asked.len(), 2);
        assert!(ui.asked[1].starts_with("Force them to quit?"));
        assert!(ui.shown[1].contains("Slack (pid 400)"), "{}", ui.shown[1]);

        let mut ui = Script::new(&[true, true]);
        let report = close_apps(&apps(), &processes, &mut control, &mut ui, fast()).unwrap();
        assert_eq!(report.targets.len(), 1, "only Slack is still running");
        assert_eq!(report.forced.len(), 1);
        assert!(report.still_running.is_empty());
        assert_eq!(control.signals.last(), Some(&(400, CloseSignal::Force)));
    }

    #[test]
    fn never_class_is_refused_before_asking() {
        let processes = table();
        let mut control = FakeControl::new(&processes);
        let mut ui = Script::new(&[true, true]);
        let mut list = apps();
        list.push(app("Finder", SafetyClass::Never, &[103]));
        let err = close_apps(&list, &processes, &mut control, &mut ui, fast()).unwrap_err();
        assert_eq!(err, CloseError::NeverClass("Finder".to_string()));
        assert!(control.signals.is_empty());
        assert!(ui.asked.is_empty() && ui.shown.is_empty());
    }

    #[test]
    fn reused_or_vanished_pids_are_not_touched() {
        let processes = table();
        let mut control = FakeControl::new(&processes);
        // pid 400 now belongs to another program.
        control.alive.borrow_mut().insert(400, "other".to_string());
        let mut ui = Script::new(&[true]);
        let report = close_apps(&apps(), &processes, &mut control, &mut ui, fast()).unwrap();
        assert!(control.signals.iter().all(|&(pid, _)| pid != 400));
        assert!(!ui.shown[0].contains("Slack"));
        assert_eq!(report.targets.len(), 3);
        // Nothing left to close at all.
        let mut ui = Script::new(&[true]);
        let slack = [app("Slack", SafetyClass::Safe, &[400])];
        assert_eq!(
            close_apps(&slack, &processes, &mut control, &mut ui, fast()),
            Err(CloseError::Empty)
        );
        assert!(ui.asked.is_empty());
    }

    #[test]
    fn caution_apps_are_named_in_the_question() {
        let processes = table();
        let mut control = FakeControl::new(&processes);
        let mut ui = Script::new(&[false]);
        let list = [app("Slack", SafetyClass::Caution, &[400])];
        close_apps(&list, &processes, &mut control, &mut ui, fast()).unwrap();
        assert!(ui.asked[0].contains("may lose unsaved work"));
    }

    /// Real signals, only to child processes this test spawns itself (macOS and Linux).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    mod real {
        use super::*;
        use crate::platform::process_control;
        use std::process::{Child, Command, Stdio};

        /// Kills and reaps the test's own child even when an assertion fails.
        struct Guard(Child);

        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        fn spawn(exe: &str, args: &[&str]) -> Guard {
            Guard(
                Command::new(exe)
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn test child"),
            )
        }

        fn row(child: &Guard, exe: &str) -> Process {
            proc(child.0.id(), std::process::id(), exe, ME, MB)
        }

        #[test]
        fn closes_own_children_gracefully_then_forced_after_second_yes() {
            let polite = spawn("/bin/sleep", &["30"]);
            let stubborn = spawn(
                "/bin/bash",
                &["-c", "trap '' TERM; while :; do /bin/sleep 0.05; done"],
            );
            // Let bash install its trap before it is signalled.
            thread::sleep(Duration::from_millis(300));
            let processes = vec![row(&polite, "/bin/sleep"), row(&stubborn, "/bin/bash")];
            let (polite, stubborn) = (&polite.0, &stubborn.0);
            let list = [
                app("polite", SafetyClass::Safe, &[polite.id()]),
                app("stubborn", SafetyClass::Safe, &[stubborn.id()]),
            ];
            let mut control = process_control().expect("this OS can close processes");
            let options = CloseOptions {
                graceful_timeout: Duration::from_millis(800),
                force_timeout: Duration::from_secs(3),
                poll_interval: Duration::from_millis(20),
            };

            // First "no": nothing happens.
            let mut ui = Script::new(&[false]);
            let report = close_apps(&list, &processes, control.as_mut(), &mut ui, options).unwrap();
            assert!(report.cancelled);
            assert!(control.is_running(polite.id(), "sleep"));
            assert!(control.is_running(stubborn.id(), "bash"));

            // "yes", then "no" to forcing: the polite child quit, the stubborn one survives.
            let mut ui = Script::new(&[true, false]);
            let report = close_apps(&list, &processes, control.as_mut(), &mut ui, options).unwrap();
            assert!(report.force_declined);
            assert_eq!(report.closed.len(), 1);
            assert_eq!(report.closed[0].pid, polite.id());
            assert!(!control.is_running(polite.id(), "sleep"));
            assert!(control.is_running(stubborn.id(), "bash"));

            // "yes" to forcing: it is killed.
            let mut ui = Script::new(&[true, true]);
            let report = close_apps(&list, &processes, control.as_mut(), &mut ui, options).unwrap();
            assert_eq!(report.forced.len(), 1);
            assert!(report.still_running.is_empty(), "{report:?}");
            assert!(!control.is_running(stubborn.id(), "bash"));
        }

        #[test]
        fn refuses_process_groups_and_launchd() {
            let mut control = process_control().unwrap();
            assert!(control.signal(0, CloseSignal::Graceful).is_err());
            assert!(control.signal(1, CloseSignal::Graceful).is_err());
            assert!(!control.is_running(0, "kernel_task"));
        }
    }

    /// Real closing on Windows, only of child processes this test spawns itself: copies of the
    /// test binary that open a window (it closes on `WM_CLOSE`) or wait without one (only a
    /// forced close ends it).
    #[cfg(target_os = "windows")]
    mod real_windows {
        use super::*;
        use crate::platform::windows_model::file_name;
        use crate::platform::{process_control, window_count};
        use std::process::{Child, Command, Stdio};

        const ROLE: &str = "CANFIT_TEST_CHILD_ROLE";
        const CHILD_TEST: &str = "advisor::close::tests::real_windows::child";

        /// Kills and reaps the test's own child even when an assertion fails.
        struct Guard(Child);

        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        fn spawn(role: &str) -> Guard {
            let exe = std::env::current_exe().expect("test binary path");
            Guard(
                Command::new(exe)
                    .args(["--exact", CHILD_TEST, "--ignored", "--test-threads=1"])
                    .env(ROLE, role)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn test child"),
            )
        }

        /// The spawned children's entry point; does nothing in an ordinary test run.
        #[test]
        #[ignore = "helper process for closes_own_children_on_windows"]
        fn child() {
            match std::env::var(ROLE).as_deref() {
                Ok("window") => run_window(),
                Ok("wait") => thread::sleep(Duration::from_secs(60)),
                _ => {}
            }
        }

        /// Opens one visible window and runs its message loop until the window is closed.
        fn run_window() {
            use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
            use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use ::windows::Win32::UI::WindowsAndMessaging::{
                CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG,
                PostQuitMessage, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WM_DESTROY,
                WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
            };
            use ::windows::core::w;

            unsafe extern "system" fn procedure(
                hwnd: HWND,
                msg: u32,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> LRESULT {
                // SAFETY: standard window procedure calls on the window Windows passed in.
                unsafe {
                    if msg == WM_DESTROY {
                        PostQuitMessage(0);
                        return LRESULT(0);
                    }
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }

            // SAFETY: plain window creation and message loop on this thread.
            unsafe {
                let instance = GetModuleHandleW(None).expect("module handle");
                let class = WNDCLASSW {
                    lpfnWndProc: Some(procedure),
                    hInstance: instance.into(),
                    lpszClassName: w!("canfit-test-window"),
                    ..Default::default()
                };
                RegisterClassW(&class);
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("canfit-test-window"),
                    w!("canfit test window"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    320,
                    200,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
                .expect("create test window");
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        fn row(child: &Guard, exe: &str) -> Process {
            Process {
                name: file_name(exe).to_string(),
                ..proc(child.0.id(), std::process::id(), exe, ME, MB)
            }
        }

        #[test]
        fn closes_own_children_gracefully_then_forced_after_second_yes() {
            let exe = std::env::current_exe().unwrap().display().to_string();
            let name = file_name(&exe).to_string();
            let polite = spawn("window");
            let stubborn = spawn("wait");
            let deadline = Instant::now() + Duration::from_secs(15);
            while window_count(polite.0.id()) == 0 {
                assert!(
                    Instant::now() < deadline,
                    "the child's window never appeared"
                );
                thread::sleep(Duration::from_millis(50));
            }
            let processes = vec![row(&polite, &exe), row(&stubborn, &exe)];
            let (polite, stubborn) = (&polite.0, &stubborn.0);
            let list = [
                app("polite", SafetyClass::Safe, &[polite.id()]),
                app("stubborn", SafetyClass::Safe, &[stubborn.id()]),
            ];
            let mut control = process_control().expect("Windows can close processes");
            let options = CloseOptions {
                graceful_timeout: Duration::from_secs(3),
                force_timeout: Duration::from_secs(5),
                poll_interval: Duration::from_millis(20),
            };

            // First "no": nothing happens.
            let mut ui = Script::new(&[false]);
            let report = close_apps(&list, &processes, control.as_mut(), &mut ui, options).unwrap();
            assert!(report.cancelled);
            assert!(control.is_running(polite.id(), &name));
            assert!(control.is_running(stubborn.id(), &name));

            // "yes", then "no" to forcing: the windowed child closed on WM_CLOSE; the child
            // without a window can't be asked and survives.
            let mut ui = Script::new(&[true, false]);
            let report = close_apps(&list, &processes, control.as_mut(), &mut ui, options).unwrap();
            assert!(report.force_declined, "{report:?}");
            assert_eq!(report.closed.len(), 1, "{report:?}");
            assert_eq!(report.closed[0].pid, polite.id());
            assert!(
                report
                    .errors
                    .iter()
                    .any(|(t, e)| t.pid == stubborn.id() && e.contains("no window"))
            );
            assert!(!control.is_running(polite.id(), &name));
            assert!(control.is_running(stubborn.id(), &name));

            // "yes" to forcing: it is terminated.
            let mut ui = Script::new(&[true, true]);
            let report = close_apps(&list, &processes, control.as_mut(), &mut ui, options).unwrap();
            assert_eq!(report.forced.len(), 1, "{report:?}");
            assert!(report.still_running.is_empty(), "{report:?}");
            assert!(!control.is_running(stubborn.id(), &name));
        }
    }
}
