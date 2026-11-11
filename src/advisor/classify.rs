//! Safe · caution · never classification of application groups (spec §10, ADR-013).
//!
//! Pure functions over a [`Snapshot`]: tested on fake process tables, no system calls.

use std::collections::{BTreeSet, HashMap};

use super::SafetyClass;
use super::rules::{RuleSet, Subject};
use crate::core::group::bundle_display_name;
use crate::core::{AppGroup, Process, Snapshot};

/// What the advisor knows about the canfit process that is asking.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// canfit's own process id.
    pub own_pid: u32,
    /// The user canfit runs as; `None` when unknown, in which case nothing is suggested.
    pub current_user: Option<String>,
    /// Every ancestor of canfit: its shell, the terminal app, and so on up to `launchd`.
    pub ancestors: BTreeSet<u32>,
    /// The user's protected list (config file).
    pub protected: Vec<String>,
}

impl Context {
    /// Finds canfit's own process, its user and its ancestors in the snapshot.
    pub fn from_snapshot(snapshot: &Snapshot, own_pid: u32, protected: Vec<String>) -> Context {
        let by_pid: HashMap<u32, &Process> =
            snapshot.processes.iter().map(|p| (p.pid, p)).collect();
        let current_user = by_pid.get(&own_pid).and_then(|p| p.user.clone());
        let mut ancestors = BTreeSet::new();
        let mut pid = own_pid;
        // Follow parent links; the bound guards against a corrupt (cyclic) table.
        for _ in 0..by_pid.len() {
            let Some(ppid) = by_pid.get(&pid).and_then(|p| p.ppid) else {
                break;
            };
            if ppid == 0 || !ancestors.insert(ppid) {
                break;
            }
            pid = ppid;
        }
        Context {
            own_pid,
            current_user,
            ancestors,
            protected,
        }
    }

    fn is_protected(&self, candidates: &[&str]) -> bool {
        self.protected.iter().any(|entry| {
            let entry = entry.trim();
            !entry.is_empty() && candidates.iter().any(|c| c.eq_ignore_ascii_case(entry))
        })
    }
}

/// One application group with its class and a short plain-language reason.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedGroup {
    pub group: AppGroup,
    pub class: SafetyClass,
    pub reason: String,
}

const REASON_SELF: &str = "canfit itself";
const REASON_ANCESTOR: &str =
    "canfit is running inside it (the current terminal); closing it would close canfit too";
const REASON_PROTECTED: &str = "protected in your canfit config";
const REASON_ROOT: &str = "runs as root: part of the system";
const REASON_OTHER_USER: &str = "belongs to another user or to the system";
const REASON_UNKNOWN_USER: &str = "canfit can't tell who owns it";
const REASON_UNKNOWN: &str = "canfit doesn't know this app: you may lose unsaved work";
const REASON_NO_RULES: &str = "canfit has no process rules for this system yet";

/// Classifies one process. Hard rules come first and cannot be overridden by the rules file.
fn classify_process(
    p: &Process,
    group: &AppGroup,
    ctx: &Context,
    rules: Option<&RuleSet>,
) -> (SafetyClass, String) {
    let never = |reason: &str| (SafetyClass::Never, reason.to_string());
    if p.pid == ctx.own_pid {
        return never(REASON_SELF);
    }
    if ctx.ancestors.contains(&p.pid) {
        return never(REASON_ANCESTOR);
    }
    let bundle_name = p.app_bundle.as_deref().map(bundle_display_name);
    let mut names = vec![group.name.as_str(), p.name.as_str()];
    names.extend(p.app_bundle.as_deref());
    names.extend(bundle_name);
    if ctx.is_protected(&names) {
        return never(REASON_PROTECTED);
    }
    match (&ctx.current_user, &p.user) {
        (Some(me), Some(owner)) if me == owner => {}
        (_, Some(owner)) if owner == "root" => return never(REASON_ROOT),
        (Some(_), Some(_)) => return never(REASON_OTHER_USER),
        _ => return never(REASON_UNKNOWN_USER),
    }
    let Some(rules) = rules else {
        return never(REASON_NO_RULES);
    };
    let subject = Subject {
        name: &p.name,
        bundle_name,
        exe: p.exe.as_deref(),
    };
    match rules.first_match(&subject) {
        Some(rule) => (rule.class, rule.reason.clone()),
        None => (SafetyClass::Caution, REASON_UNKNOWN.to_string()),
    }
}

/// Classifies every application group of the snapshot. A group takes the most restrictive class
/// of its processes, so an app hosting canfit's terminal, or with a single root helper, is
/// "never". Groups keep the snapshot order (largest memory first).
pub fn classify(
    snapshot: &Snapshot,
    ctx: &Context,
    rules: Option<&RuleSet>,
) -> Vec<ClassifiedGroup> {
    let by_pid: HashMap<u32, &Process> = snapshot.processes.iter().map(|p| (p.pid, p)).collect();
    snapshot
        .app_groups
        .iter()
        .map(|group| {
            let mut decided: Option<(SafetyClass, String)> = None;
            for pid in &group.pids {
                let verdict = match by_pid.get(pid) {
                    Some(p) => classify_process(p, group, ctx, rules),
                    // A pid without a process entry can't be checked: fail closed.
                    None => (SafetyClass::Never, REASON_UNKNOWN_USER.to_string()),
                };
                if decided.as_ref().is_none_or(|(class, _)| verdict.0 > *class) {
                    decided = Some(verdict);
                }
            }
            let (class, reason) =
                decided.unwrap_or((SafetyClass::Never, REASON_UNKNOWN_USER.to_string()));
            ClassifiedGroup {
                group: group.clone(),
                class,
                reason,
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::advisor::rules;
    use crate::core::group::group_processes;
    use crate::core::sampler::tests::replay;
    use crate::core::{MemoryKind, ProcessMemory, Provenance};

    pub const MB: u64 = 1024 * 1024;
    pub const GB: u64 = 1024 * MB;
    pub const ME: &str = "fede";

    /// A process row for fake tables: `exe` decides the bundle as on macOS.
    pub fn proc(pid: u32, ppid: u32, exe: &str, user: &str, bytes: u64) -> Process {
        let name = exe.rsplit('/').next().unwrap().to_string();
        Process {
            pid,
            ppid: Some(ppid),
            name,
            exe: Some(exe.to_string()),
            user: Some(user.to_string()),
            memory: Some(ProcessMemory {
                bytes,
                kind: MemoryKind::Footprint,
                provenance: Provenance::Measured,
            }),
            app_bundle: crate::core::group::outermost_app_bundle(exe).map(str::to_string),
        }
    }

    pub const CANFIT_PID: u32 = 900;

    /// A typical desktop: system processes, a terminal running canfit, and a few apps.
    pub fn desktop() -> Vec<Process> {
        vec![
            proc(0, 0, "kernel_task", "root", 0),
            proc(1, 0, "/sbin/launchd", "root", 30 * MB),
            proc(
                100,
                1,
                "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer",
                "_windowserver",
                900 * MB,
            ),
            proc(
                101,
                1,
                "/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow",
                ME,
                120 * MB,
            ),
            proc(
                102,
                1,
                "/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock",
                ME,
                90 * MB,
            ),
            proc(
                103,
                1,
                "/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder",
                ME,
                190 * MB,
            ),
            proc(
                200,
                1,
                "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
                ME,
                300 * MB,
            ),
            proc(201, 200, "/usr/bin/login", "root", 2 * MB),
            proc(202, 201, "/bin/zsh", ME, 5 * MB),
            proc(CANFIT_PID, 202, "/opt/homebrew/bin/canfit", ME, 10 * MB),
            proc(
                300,
                1,
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                ME,
                800 * MB,
            ),
            proc(
                301,
                300,
                "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer)",
                ME,
                2200 * MB,
            ),
            proc(
                400,
                1,
                "/Applications/Slack.app/Contents/MacOS/Slack",
                ME,
                1000 * MB,
            ),
            proc(
                500,
                1,
                "/Applications/Visual Studio Code.app/Contents/MacOS/Electron",
                ME,
                1500 * MB,
            ),
            proc(
                600,
                1,
                "/Applications/Some Tool.app/Contents/MacOS/Some Tool",
                ME,
                400 * MB,
            ),
            proc(
                700,
                1,
                "/Applications/Spotify.app/Contents/MacOS/Spotify",
                ME,
                300 * MB,
            ),
            proc(
                800,
                1,
                "/Applications/Ollama.app/Contents/Resources/ollama",
                ME,
                4 * GB,
            ),
        ]
    }

    pub fn snapshot_with(processes: Vec<Process>) -> Snapshot {
        let (_, mut s) = replay("calm");
        s.app_groups = group_processes(&processes);
        s.processes = processes;
        s
    }

    pub fn classify_all(snapshot: &Snapshot, protected: &[&str]) -> Vec<ClassifiedGroup> {
        let ctx = Context::from_snapshot(
            snapshot,
            CANFIT_PID,
            protected.iter().map(|s| s.to_string()).collect(),
        );
        classify(snapshot, &ctx, rules::for_os("macos").as_ref())
    }

    fn class_of(groups: &[ClassifiedGroup], name: &str) -> SafetyClass {
        groups
            .iter()
            .find(|g| g.group.name == name)
            .unwrap_or_else(|| panic!("no group {name}"))
            .class
    }

    #[test]
    fn context_finds_user_and_every_ancestor() {
        let s = snapshot_with(desktop());
        let ctx = Context::from_snapshot(&s, CANFIT_PID, vec![]);
        assert_eq!(ctx.current_user.as_deref(), Some(ME));
        assert_eq!(ctx.ancestors, BTreeSet::from([202, 201, 200, 1]));
    }

    #[test]
    fn desktop_processes_get_the_expected_classes() {
        let groups = classify_all(&snapshot_with(desktop()), &[]);
        for (name, class) in [
            ("kernel_task", SafetyClass::Never),
            ("launchd", SafetyClass::Never),
            ("WindowServer", SafetyClass::Never),
            ("loginwindow", SafetyClass::Never),
            ("Dock", SafetyClass::Never),
            ("Finder", SafetyClass::Never),
            ("Terminal", SafetyClass::Never),
            ("login", SafetyClass::Never),
            ("zsh", SafetyClass::Never),
            ("canfit", SafetyClass::Never),
            ("Google Chrome", SafetyClass::Safe),
            ("Slack", SafetyClass::Safe),
            ("Spotify", SafetyClass::Safe),
            ("Visual Studio Code", SafetyClass::Caution),
            ("Ollama", SafetyClass::Caution),
            ("Some Tool", SafetyClass::Caution),
        ] {
            assert_eq!(class_of(&groups, name), class, "{name}");
        }
        let terminal = groups.iter().find(|g| g.group.name == "Terminal").unwrap();
        assert!(
            terminal.reason.contains("current terminal"),
            "{}",
            terminal.reason
        );
        let unknown = groups.iter().find(|g| g.group.name == "Some Tool").unwrap();
        assert!(unknown.reason.contains("unsaved work"));
    }

    #[test]
    fn canfit_inside_an_app_makes_that_app_never() {
        // canfit started from VS Code's integrated terminal: VS Code hosts canfit's shell.
        let mut processes = desktop();
        let shell = processes.iter_mut().find(|p| p.pid == 202).unwrap();
        shell.ppid = Some(500);
        let groups = classify_all(&snapshot_with(processes), &[]);
        assert_eq!(class_of(&groups, "Visual Studio Code"), SafetyClass::Never);
        // Terminal.app is no longer an ancestor: an ordinary terminal app is caution.
        assert_eq!(class_of(&groups, "Terminal"), SafetyClass::Caution);
    }

    #[test]
    fn one_foreign_helper_makes_the_whole_group_never() {
        let mut processes = desktop();
        processes.push(proc(
            302,
            1,
            "/Applications/Google Chrome.app/Contents/Helpers/updater",
            "root",
            10 * MB,
        ));
        let groups = classify_all(&snapshot_with(processes), &[]);
        assert_eq!(class_of(&groups, "Google Chrome"), SafetyClass::Never);
    }

    #[test]
    fn protected_entries_match_names_and_bundles_ignoring_case() {
        let groups = classify_all(&snapshot_with(desktop()), &["slack", "ollama", " "]);
        assert_eq!(class_of(&groups, "Slack"), SafetyClass::Never);
        assert_eq!(class_of(&groups, "Ollama"), SafetyClass::Never);
        assert_eq!(class_of(&groups, "Google Chrome"), SafetyClass::Safe);
        let by_path = classify_all(
            &snapshot_with(desktop()),
            &["/Applications/Google Chrome.app"],
        );
        assert_eq!(class_of(&by_path, "Google Chrome"), SafetyClass::Never);
    }

    #[test]
    fn unknown_user_or_os_fails_closed() {
        let s = snapshot_with(desktop());
        // canfit not in the table: the current user is unknown, nothing is suggestable.
        let ctx = Context::from_snapshot(&s, 4242, vec![]);
        assert!(ctx.current_user.is_none());
        let groups = classify(&s, &ctx, rules::for_os("macos").as_ref());
        assert!(groups.iter().all(|g| g.class == SafetyClass::Never));
        // No rules for the OS: nothing is suggestable either.
        let ctx = Context::from_snapshot(&s, CANFIT_PID, vec![]);
        let groups = classify(&s, &ctx, None);
        assert!(groups.iter().all(|g| g.class == SafetyClass::Never));
    }

    #[test]
    fn unreadable_owner_is_never() {
        let mut processes = desktop();
        processes.iter_mut().find(|p| p.pid == 700).unwrap().user = None;
        let groups = classify_all(&snapshot_with(processes), &[]);
        assert_eq!(class_of(&groups, "Spotify"), SafetyClass::Never);
    }
}
