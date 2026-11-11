//! "How to close it yourself" (ADR-022): for each app canfit suggests, the commands that close it
//! by hand, safest first. canfit never runs them; they are shown as text in the cards, the
//! closing prompt, the monitor's close dialog and `--json` (`manual_commands`).
//!
//! Rules, in order:
//! 1. Tools with their own stop command come first: Docker Desktop (`docker desktop stop`),
//!    Ollama (`ollama stop MODEL` unloads a model without stopping the server), LM Studio
//!    (`lms unload --all`).
//! 2. A process managed by a supervisor would only be restarted if killed: a systemd unit
//!    (`systemctl [--user] stop UNIT`, from the cgroup canfit already reads on Linux) or a
//!    Homebrew service on macOS (`brew services stop NAME`, guessed from a Cellar path). Killing
//!    is then not offered for systemd units.
//! 3. A macOS app bundle: `osascript -e 'quit app "NAME"'`, the same as ⌘Q (it may ask to save).
//! 4. Any process: graceful first (`kill PIDS`; Windows `taskkill /PID ...`), then the forced
//!    command as a last resort (`kill -9 PIDS`; Windows `taskkill /F /PID ...` or
//!    `Stop-Process -Id ... -Force`).
//!
//! Never-class apps get no command: canfit never suggests them.

use std::collections::BTreeSet;

use serde::Serialize;

use super::SafetyClass;
use crate::core::{AppGroup, GroupKind, Process};

/// One command the user can run to close an app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManualCommand {
    /// The command line, ready to copy.
    pub command: String,
    /// What it does, in a few words.
    pub note: String,
    /// True for commands that force the app to quit (unsaved work is lost): last resort.
    pub forced: bool,
}

fn cmd(command: impl Into<String>, note: impl Into<String>, forced: bool) -> ManualCommand {
    ManualCommand {
        command: command.into(),
        note: note.into(),
        forced,
    }
}

fn lower(s: &str) -> String {
    s.to_lowercase()
        .trim_end_matches(".exe")
        .trim_end_matches(".app")
        .to_string()
}

/// The systemd unit a Linux app hint names (`/system.slice/ollama.service`), and whether it is
/// a user unit (`/user.slice/.../user@1000.service/.../foo.service`).
pub fn systemd_unit(hint: &str) -> Option<(String, bool)> {
    let leaf = hint.rsplit('/').next()?;
    if !leaf.ends_with(".service") {
        return None;
    }
    let user = hint.contains("/user@") && hint.contains(".service/");
    Some((leaf.to_string(), user))
}

/// The Homebrew formula a macOS executable path belongs to (`/opt/homebrew/Cellar/ollama/0.5/bin/
/// ollama` → `ollama`), for a process outside any app bundle.
pub fn homebrew_formula(exe: &str) -> Option<String> {
    if exe.contains(".app/") {
        return None;
    }
    for marker in ["/Cellar/", "/opt/homebrew/opt/", "/usr/local/opt/"] {
        if let Some((_, rest)) = exe.split_once(marker) {
            let name = rest.split('/').next()?;
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The commands that close `group` by hand on `os`, safest first. Empty for never-class apps.
pub fn commands(
    group: &AppGroup,
    class: SafetyClass,
    processes: &[Process],
    os: &str,
) -> Vec<ManualCommand> {
    if class == SafetyClass::Never || group.pids.is_empty() {
        return Vec::new();
    }
    let members: Vec<&Process> = processes
        .iter()
        .filter(|p| group.pids.contains(&p.pid))
        .collect();
    let in_group: BTreeSet<u32> = group.pids.iter().copied().collect();
    // Graceful requests go to the app's root processes, like canfit's own close (ADR-013).
    let mut roots: Vec<u32> = members
        .iter()
        .filter(|p| p.ppid.is_none_or(|pp| !in_group.contains(&pp)))
        .map(|p| p.pid)
        .collect();
    if roots.is_empty() {
        roots = group.pids.clone();
    }
    roots.sort_unstable();
    let mut all = group.pids.clone();
    all.sort_unstable();
    let join = |pids: &[u32], sep: &str| {
        pids.iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(sep)
    };

    let names: Vec<String> = std::iter::once(lower(&group.name))
        .chain(members.iter().map(|p| lower(&p.name)))
        .collect();
    let has = |n: &str| names.iter().any(|x| x == n);
    let mut out = Vec::new();

    // 1. Tools with their own stop command.
    if has("docker desktop") || (has("docker") && group.kind == GroupKind::AppBundle) {
        out.push(cmd(
            "docker desktop stop",
            "stops Docker Desktop and its VM (Docker Desktop 4.37 or later; else quit it from \
             its menu)",
            false,
        ));
    }
    if has("ollama") || has("ollama app") {
        out.push(cmd(
            "ollama ps",
            "lists the models Ollama has loaded",
            false,
        ));
        out.push(cmd(
            "ollama stop MODEL",
            "unloads one model and frees its memory without stopping the server",
            false,
        ));
    }
    if has("lm studio") || has("lm-studio") {
        out.push(cmd(
            "lms unload --all",
            "unloads every model; LM Studio keeps running",
            false,
        ));
    }

    // 2. Supervisors: killing would only restart the process.
    let hints: BTreeSet<&str> = members
        .iter()
        .filter_map(|p| p.app_bundle.as_deref())
        .collect();
    let mut supervised = false;
    if os == "linux" {
        for hint in &hints {
            if let Some((unit, user)) = systemd_unit(hint) {
                supervised = true;
                let command = if user {
                    format!("systemctl --user stop {unit}")
                } else {
                    format!("sudo systemctl stop {unit}")
                };
                out.push(cmd(
                    command,
                    "a systemd service: stop it through systemd, which would restart a killed \
                     process",
                    false,
                ));
            }
        }
    }
    if os == "macos"
        && let Some(formula) = members
            .iter()
            .filter(|p| p.ppid == Some(1))
            .find_map(|p| p.exe.as_deref().and_then(homebrew_formula))
    {
        out.push(cmd(
            format!("brew services stop {formula}"),
            "if you started it with brew services (launchd would restart a killed process)",
            false,
        ));
    }
    if supervised {
        return out;
    }

    // 3. macOS apps quit like ⌘Q.
    if os == "macos" && group.kind == GroupKind::AppBundle && group.bundle_path.is_some() {
        out.push(cmd(
            format!("osascript -e 'quit app \"{}\"'", quote(&group.name)),
            "asks the app to quit, like ⌘Q; it may ask you to save",
            false,
        ));
    }

    // 4. Plain processes: graceful, then forced as a last resort.
    if os == "windows" {
        let ids = join(&roots, " /PID ");
        out.push(cmd(
            format!("taskkill /PID {ids}"),
            "asks its windows to close, like canfit's graceful close",
            false,
        ));
        out.push(cmd(
            format!("taskkill /F /T /PID {}", join(&roots, " /PID ")),
            "last resort: forces it and its child processes to quit; unsaved work is lost \
             (PowerShell: Stop-Process -Id ... -Force)",
            true,
        ));
    } else {
        out.push(cmd(
            format!("kill {}", join(&roots, " ")),
            "asks it to quit (SIGTERM), like canfit's graceful close",
            false,
        ));
        out.push(cmd(
            format!("kill -9 {}", join(&all, " ")),
            "last resort: forces every process of the app to quit; unsaved work is lost",
            true,
        ));
    }
    out
}

/// How many pids a command shows in text before it is shortened.
pub const SHOWN_PIDS: usize = 6;

impl ManualCommand {
    /// The command for a card or dialog line: long pid lists are cut after [`SHOWN_PIDS`] with a
    /// count of the rest (`--json` has the full command).
    pub fn short(&self) -> String {
        let words: Vec<&str> = self.command.split(' ').collect();
        let pids = words
            .iter()
            .filter(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit()))
            .count();
        if pids <= SHOWN_PIDS + 2 {
            return self.command.clone();
        }
        let mut out: Vec<&str> = Vec::new();
        let mut seen = 0;
        for w in &words {
            if !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit()) {
                seen += 1;
                if seen > SHOWN_PIDS {
                    break;
                }
            } else if seen >= SHOWN_PIDS {
                // A "/PID" separator after the last shown pid.
                break;
            }
            out.push(w);
        }
        format!(
            "{} … ({} more processes; --json lists them all)",
            out.join(" "),
            pids - SHOWN_PIDS
        )
    }
}

/// The commands as indented text lines, for the closing prompt and the monitor's dialog.
pub fn describe(app: &str, commands: &[ManualCommand]) -> String {
    let mut out = format!("  {app}:\n");
    for c in commands {
        out.push_str(&format!("    {}   ({})\n", c.short(), c.note));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{MemoryKind, ProcessMemory, Provenance};

    fn process(pid: u32, ppid: u32, name: &str, exe: &str, hint: Option<&str>) -> Process {
        Process {
            pid,
            ppid: Some(ppid),
            name: name.to_string(),
            exe: Some(exe.to_string()),
            user: Some("me".to_string()),
            memory: Some(ProcessMemory {
                bytes: 1 << 30,
                kind: MemoryKind::Footprint,
                provenance: Provenance::Measured,
            }),
            app_bundle: hint.map(str::to_string),
        }
    }

    fn group(name: &str, kind: GroupKind, bundle: Option<&str>, pids: &[u32]) -> AppGroup {
        AppGroup {
            name: name.to_string(),
            kind,
            bundle_path: bundle.map(str::to_string),
            memory_bytes: 1 << 30,
            memory_kinds: vec![MemoryKind::Footprint],
            process_count: pids.len(),
            unreadable_count: 0,
            pids: pids.to_vec(),
        }
    }

    fn lines(c: &[ManualCommand]) -> Vec<&str> {
        c.iter().map(|c| c.command.as_str()).collect()
    }

    #[test]
    fn macos_apps_quit_like_command_q_then_kill() {
        let bundle = "/Applications/Slack.app";
        let procs = vec![
            process(
                10,
                1,
                "Slack",
                "/Applications/Slack.app/Contents/MacOS/Slack",
                None,
            ),
            process(
                11,
                10,
                "Slack Helper",
                "/Applications/Slack.app/x/Slack Helper",
                None,
            ),
        ];
        let g = group("Slack", GroupKind::AppBundle, Some(bundle), &[10, 11]);
        let c = commands(&g, SafetyClass::Safe, &procs, "macos");
        assert_eq!(
            lines(&c),
            [
                "osascript -e 'quit app \"Slack\"'",
                "kill 10",
                "kill -9 10 11"
            ]
        );
        assert!(c[2].forced && !c[1].forced);
        // Never-class apps get nothing.
        assert!(commands(&g, SafetyClass::Never, &procs, "macos").is_empty());
    }

    #[test]
    fn windows_uses_taskkill_graceful_then_forced() {
        let procs = vec![
            process(40, 4, "chrome.exe", "C:\\Chrome\\chrome.exe", None),
            process(41, 40, "chrome.exe", "C:\\Chrome\\chrome.exe", None),
        ];
        let g = group("Google Chrome", GroupKind::Executable, None, &[40, 41]);
        let c = commands(&g, SafetyClass::Safe, &procs, "windows");
        assert_eq!(lines(&c), ["taskkill /PID 40", "taskkill /F /T /PID 40"]);
        assert!(c[1].forced);
    }

    #[test]
    fn systemd_services_are_stopped_through_systemd_only() {
        let procs = vec![process(
            7,
            1,
            "ollama",
            "/usr/local/bin/ollama",
            Some("/system.slice/ollama.service"),
        )];
        let g = group("ollama.service", GroupKind::Executable, None, &[7]);
        let c = commands(&g, SafetyClass::Caution, &procs, "linux");
        assert_eq!(
            lines(&c),
            [
                "ollama ps",
                "ollama stop MODEL",
                "sudo systemctl stop ollama.service"
            ]
        );
        let user = "/user.slice/user-1000.slice/user@1000.service/app.slice/llama.service";
        let procs = vec![process(
            8,
            1,
            "llama-server",
            "/opt/llama-server",
            Some(user),
        )];
        let g = group("llama.service", GroupKind::Executable, None, &[8]);
        let c = commands(&g, SafetyClass::Caution, &procs, "linux");
        assert_eq!(lines(&c), ["systemctl --user stop llama.service"]);
    }

    #[test]
    fn known_tools_and_homebrew_services() {
        let procs = vec![process(
            20,
            1,
            "ollama",
            "/opt/homebrew/Cellar/ollama/0.12.0/bin/ollama",
            None,
        )];
        let g = group("ollama", GroupKind::Executable, None, &[20]);
        let c = commands(&g, SafetyClass::Caution, &procs, "macos");
        assert_eq!(
            lines(&c),
            [
                "ollama ps",
                "ollama stop MODEL",
                "brew services stop ollama",
                "kill 20",
                "kill -9 20"
            ]
        );
        let procs = vec![process(
            30,
            1,
            "Docker Desktop",
            "/Applications/Docker.app/Contents/MacOS/Docker Desktop",
            None,
        )];
        let g = group(
            "Docker",
            GroupKind::AppBundle,
            Some("/Applications/Docker.app"),
            &[30],
        );
        let c = commands(&g, SafetyClass::Caution, &procs, "macos");
        assert_eq!(c[0].command, "docker desktop stop");
        assert_eq!(c[1].command, "osascript -e 'quit app \"Docker\"'");
        let procs = vec![process(
            31,
            1,
            "LM Studio",
            "/Applications/LM Studio.app/Contents/MacOS/LM Studio",
            None,
        )];
        let g = group(
            "LM Studio",
            GroupKind::AppBundle,
            Some("/Applications/LM Studio.app"),
            &[31],
        );
        assert_eq!(
            commands(&g, SafetyClass::Caution, &procs, "macos")[0].command,
            "lms unload --all"
        );
    }

    #[test]
    fn long_pid_lists_are_shortened_in_text_only() {
        let pids: Vec<u32> = (100..130).collect();
        let procs: Vec<Process> = pids
            .iter()
            .map(|&p| process(p, 1, "WebKit", "/System/x", None))
            .collect();
        let g = group("WebKit", GroupKind::Executable, None, &pids);
        let c = commands(&g, SafetyClass::Caution, &procs, "macos");
        assert_eq!(c[0].command.split(' ').count(), 31, "the full command");
        assert_eq!(
            c[0].short(),
            "kill 100 101 102 103 104 105 … (24 more processes; --json lists them all)"
        );
        let c = commands(&g, SafetyClass::Caution, &procs, "windows");
        assert_eq!(
            c[0].short(),
            "taskkill /PID 100 /PID 101 /PID 102 /PID 103 /PID 104 /PID 105 … (24 more \
             processes; --json lists them all)"
        );
        // Short lists stay whole.
        let g = group("WebKit", GroupKind::Executable, None, &pids[..3]);
        let c = commands(&g, SafetyClass::Caution, &procs, "macos");
        assert_eq!(c[0].short(), "kill 100 101 102");
    }

    #[test]
    fn helpers_parse_units_and_formulas() {
        assert_eq!(
            systemd_unit("/system.slice/ollama.service"),
            Some(("ollama.service".to_string(), false))
        );
        assert_eq!(
            systemd_unit("/user.slice/user-1000.slice/session-2.scope"),
            None
        );
        assert_eq!(systemd_unit("/opt/app/bin/app"), None);
        assert_eq!(
            homebrew_formula("/usr/local/Cellar/redis/7.2/bin/redis-server").as_deref(),
            Some("redis")
        );
        assert_eq!(
            homebrew_formula("/opt/homebrew/opt/postgresql@16/bin/postgres").as_deref(),
            Some("postgresql@16")
        );
        assert_eq!(
            homebrew_formula("/Applications/X.app/Contents/Cellar/a/b"),
            None
        );
        assert_eq!(homebrew_formula("/usr/local/bin/foo"), None);
        assert_eq!(quote("A \"b\""), "A \\\"b\\\"");
    }
}
