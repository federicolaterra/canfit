//! Folding processes into applications.
//!
//! An application is one row in every view: Chrome with all its helpers, renderers and XPC
//! services counts once. Grouping is shared code fed by backend hints (ADR-004): on macOS the
//! hint is the outermost `.app` bundle of the executable; on Windows it is the executable of the
//! app's root process, for helpers the app started from its own install folder (ADR-016); on
//! Linux it is the systemd service, Flatpak or Snap, else the root process's executable
//! (ADR-017).
//! Processes without a hint are grouped by executable name. Processes are never folded into a
//! parent from another program's folder, so a model server started from a terminal stays its own
//! row instead of disappearing into the terminal app.

use std::collections::BTreeMap;

use super::{AppGroup, GroupKind, Process};

/// Returns the outermost `.app` bundle in `path`, e.g.
/// `/Applications/Google Chrome.app` for a Chrome renderer helper nested several bundles deep.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // only macOS has app bundles
pub fn outermost_app_bundle(path: &str) -> Option<&str> {
    let mut offset = 0;
    for component in path.split('/') {
        let end = offset + component.len();
        if component.len() > ".app".len() && component.ends_with(".app") {
            return Some(&path[..end]);
        }
        offset = end + 1;
    }
    None
}

/// The display name of an app hint: the last path component without `.app` (a macOS bundle),
/// `.exe` (the root executable of a Windows app) or `.service` (a Linux systemd service),
/// ignoring the extension's case.
pub fn bundle_display_name(bundle_path: &str) -> &str {
    let last = bundle_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(bundle_path);
    for ext in [".app", ".exe", ".service"] {
        if last.len() > ext.len()
            && last.is_char_boundary(last.len() - ext.len())
            && last[last.len() - ext.len()..].eq_ignore_ascii_case(ext)
        {
            return &last[..last.len() - ext.len()];
        }
    }
    last
}

/// Shared system process families that serve several apps and have no bundle of their own.
/// Public APIs cannot tell which app a WebKit content process works for, so they form one honest
/// group instead of 150 rows named `com.apple.WebKit.WebContent`.
const FAMILIES: &[(&str, &str)] = &[("com.apple.WebKit.", "Safari and other web views (WebKit)")];

fn family_name(process_name: &str) -> Option<&'static str> {
    FAMILIES
        .iter()
        .find(|(prefix, _)| process_name.starts_with(prefix))
        .map(|&(_, name)| name)
}

/// Groups processes into applications, largest memory first.
pub fn group_processes(processes: &[Process]) -> Vec<AppGroup> {
    let mut groups: BTreeMap<(GroupKind, String), AppGroup> = BTreeMap::new();
    for process in processes {
        let (kind, key, name, bundle_path) = match &process.app_bundle {
            Some(bundle) => (
                GroupKind::AppBundle,
                bundle.clone(),
                bundle_display_name(bundle).to_string(),
                Some(bundle.clone()),
            ),
            None => {
                let name = family_name(&process.name).unwrap_or(&process.name);
                (
                    GroupKind::Executable,
                    name.to_string(),
                    name.to_string(),
                    None,
                )
            }
        };
        let group = groups.entry((kind, key)).or_insert_with(|| AppGroup {
            name,
            kind,
            bundle_path,
            memory_bytes: 0,
            memory_kinds: Vec::new(),
            process_count: 0,
            unreadable_count: 0,
            pids: Vec::new(),
        });
        group.process_count += 1;
        group.pids.push(process.pid);
        match &process.memory {
            Some(memory) => {
                group.memory_bytes += memory.bytes;
                if !group.memory_kinds.contains(&memory.kind) {
                    group.memory_kinds.push(memory.kind);
                }
            }
            None => group.unreadable_count += 1,
        }
    }
    let mut groups: Vec<AppGroup> = groups.into_values().collect();
    groups.sort_by(|a, b| {
        b.memory_bytes
            .cmp(&a.memory_bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{MemoryKind, Process, ProcessMemory, Provenance};

    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/macos/executable_paths.txt"
    ));

    #[test]
    fn recorded_paths_resolve_to_their_outermost_bundle() {
        // Each line: `<executable path>\t<expected bundle or "-">`.
        let mut checked = 0;
        for line in FIXTURE.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let (path, expected) = line.split_once('\t').expect("tab-separated fixture line");
            let expected = (expected != "-").then_some(expected);
            assert_eq!(outermost_app_bundle(path), expected, "path: {path}");
            checked += 1;
        }
        assert!(checked >= 8);
    }

    #[test]
    fn bundle_names_drop_the_extension() {
        assert_eq!(
            bundle_display_name("/Applications/Google Chrome.app"),
            "Google Chrome"
        );
        assert_eq!(bundle_display_name("Ollama.app"), "Ollama");
        assert_eq!(
            bundle_display_name(r"C:\Program Files\Docker\Docker\Docker Desktop.exe"),
            "Docker Desktop"
        );
        assert_eq!(bundle_display_name(r"C:\x\NOTEPAD.EXE"), "NOTEPAD");
        assert_eq!(bundle_display_name(".exe"), ".exe");
        assert_eq!(
            bundle_display_name("/system.slice/ollama.service"),
            "ollama"
        );
        assert_eq!(bundle_display_name("/snap/firefox"), "firefox");
    }

    fn process(pid: u32, name: &str, bundle: Option<&str>, bytes: Option<u64>) -> Process {
        Process {
            pid,
            ppid: Some(1),
            name: name.to_string(),
            exe: None,
            user: None,
            memory: bytes.map(|bytes| ProcessMemory {
                bytes,
                kind: MemoryKind::Footprint,
                provenance: Provenance::Measured,
            }),
            app_bundle: bundle.map(str::to_string),
        }
    }

    #[test]
    fn helpers_fold_into_their_app_and_groups_sort_by_memory() {
        let chrome = Some("/Applications/Google Chrome.app");
        let processes = [
            process(10, "Google Chrome", chrome, Some(500)),
            process(11, "Google Chrome Helper (Renderer)", chrome, Some(700)),
            process(12, "Google Chrome Helper (GPU)", chrome, None),
            process(20, "llama-server", None, Some(5000)),
            process(21, "zsh", None, Some(5)),
            process(22, "zsh", None, Some(6)),
            process(30, "com.apple.WebKit.WebContent", None, Some(1)),
            process(31, "com.apple.WebKit.Networking", None, Some(2)),
        ];
        let groups = group_processes(&processes);
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0].name, "llama-server");
        assert_eq!(groups[1].name, "Google Chrome");
        assert_eq!(groups[1].kind, GroupKind::AppBundle);
        assert_eq!(groups[1].memory_bytes, 1200);
        assert_eq!(groups[1].process_count, 3);
        assert_eq!(groups[1].unreadable_count, 1);
        assert_eq!(groups[2].name, "zsh");
        assert_eq!(groups[2].memory_bytes, 11);
        assert_eq!(groups[2].pids, [21, 22]);
        assert_eq!(groups[3].name, "Safari and other web views (WebKit)");
        assert_eq!(groups[3].process_count, 2);
    }
}
