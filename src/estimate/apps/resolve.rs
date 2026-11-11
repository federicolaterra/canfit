//! Which app a target names (ADR-020): profile names and aliases first (ignoring case), then
//! `APP: VM` for one virtual machine of a VM app, then running app groups. Nothing is guessed:
//! an unknown or ambiguous name gets a list of close matches instead.

use std::collections::HashMap;

use super::Os;
use super::profiles::{Matchers, Profile};
use crate::core::{AppGroup, Process, Snapshot};

/// What an app target resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution<'a> {
    /// A profile, optionally narrowed to one virtual machine (`UTM: Ubuntu`).
    Profile {
        profile: &'a Profile,
        vm: Option<String>,
    },
    /// No profile, but a running app group has this name.
    Running { group: &'a AppGroup },
    /// A profile for an app that does not run on this OS.
    OtherOs { profile: &'a Profile },
    /// Several profiles share the name.
    Ambiguous(Vec<String>),
    /// Nothing matches; the closest names, if any.
    Unknown { suggestions: Vec<String> },
}

/// Lower case with runs of whitespace collapsed, and `.exe` / `.app` dropped.
pub fn normalize(name: &str) -> String {
    let lower = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    for ext in [".exe", ".app"] {
        if let Some(stem) = lower.strip_suffix(ext)
            && !stem.is_empty()
        {
            return stem.to_string();
        }
    }
    lower
}

fn named<'a>(
    profiles: &'a [Profile],
    name: &str,
    on: impl Fn(&Profile) -> bool,
) -> Vec<&'a Profile> {
    let wanted = normalize(name);
    profiles
        .iter()
        .filter(|p| on(p) && p.names().any(|n| normalize(n) == wanted))
        .collect()
}

/// Resolves `target` against `profiles` on `os`, then against the snapshot's running apps.
pub fn resolve<'a>(
    target: &str,
    profiles: &'a [Profile],
    os: Os,
    groups: &'a [AppGroup],
) -> Resolution<'a> {
    let here = |p: &Profile| p.runs_on(os);
    let pick = |found: Vec<&'a Profile>, vm: Option<String>| -> Option<Resolution<'a>> {
        match found.as_slice() {
            [] => None,
            [one] => Some(Resolution::Profile { profile: one, vm }),
            many => Some(Resolution::Ambiguous(
                many.iter().map(|p| p.name.clone()).collect(),
            )),
        }
    };
    if let Some(r) = pick(named(profiles, target, here), None) {
        return r;
    }
    // `APP: VM` names one virtual machine of a VM app.
    if let Some((app, vm)) = target.split_once(':') {
        let vm = vm.trim();
        if !vm.is_empty()
            && let Some(r) = pick(named(profiles, app, here), Some(vm.to_string()))
        {
            return r;
        }
    }
    if let Some(profile) = named(profiles, target, |_| true).first() {
        return Resolution::OtherOs { profile };
    }
    let wanted = normalize(target);
    if let Some(group) = groups.iter().find(|g| normalize(&g.name) == wanted) {
        return Resolution::Running { group };
    }
    Resolution::Unknown {
        suggestions: suggestions(target, profiles, os, groups),
    }
}

/// Edit distance between two short strings (Levenshtein).
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            let next = (row[j + 1] + 1).min(row[j] + 1).min(prev + cost);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Whether `name` is close to `target`: one contains the other, or a small edit distance.
pub fn is_close(target: &str, name: &str) -> bool {
    let wanted = normalize(target.split(':').next().unwrap_or(target));
    if wanted.chars().count() < 2 {
        return false;
    }
    let n = normalize(name);
    let limit = if wanted.chars().count() <= 4 { 1 } else { 2 };
    (n.contains(&wanted) || (wanted.contains(&n) && n.chars().count() >= 3))
        || distance(&n, &wanted) <= limit
}

/// Up to five names close to `target`: profile names (aliases point to their profile) and
/// running apps, by containment or a small edit distance.
pub fn suggestions(target: &str, profiles: &[Profile], os: Os, groups: &[AppGroup]) -> Vec<String> {
    let close = |name: &str| is_close(target, name);
    let mut out: Vec<String> = Vec::new();
    for p in profiles.iter().filter(|p| p.runs_on(os)) {
        if p.names().any(close) && !out.contains(&p.name) {
            out.push(p.name.clone());
        }
    }
    // A running app named like a profile (the `docker` daemon, an alias of Docker Desktop) is
    // that profile: suggest the profile once, not the process as well.
    let profile_names: Vec<String> = profiles
        .iter()
        .filter(|p| p.runs_on(os))
        .flat_map(|p| p.names().map(normalize))
        .collect();
    for g in groups {
        let name = normalize(&g.name);
        if close(&g.name)
            && !profile_names.contains(&name)
            && !out.iter().any(|o| normalize(o) == name)
        {
            out.push(g.name.clone());
        }
    }
    out.truncate(5);
    out
}

/// Whether a running app group belongs to an app described by `matchers`: its name, or the name
/// of any of its processes, is one of the profile's names (ignoring case and `.exe` / `.app`).
fn group_matches(group: &AppGroup, by_pid: &HashMap<u32, &Process>, names: &[String]) -> bool {
    let wanted: Vec<String> = names.iter().map(|n| normalize(n)).collect();
    if wanted.is_empty() {
        return false;
    }
    wanted.contains(&normalize(&group.name))
        || group
            .pids
            .iter()
            .filter_map(|pid| by_pid.get(pid))
            .any(|p| wanted.contains(&normalize(&p.name)))
}

/// The running app groups of the app described by `matchers`, and the shared groups that may
/// hold part of its memory.
pub fn running_groups<'a>(
    matchers: &Matchers,
    snapshot: &'a Snapshot,
) -> (Vec<&'a AppGroup>, Vec<&'a AppGroup>) {
    let by_pid: HashMap<u32, &Process> = snapshot.processes.iter().map(|p| (p.pid, p)).collect();
    let own: Vec<String> = matchers
        .bundles
        .iter()
        .chain(&matchers.names)
        .cloned()
        .collect();
    let mine: Vec<&AppGroup> = snapshot
        .app_groups
        .iter()
        .filter(|g| group_matches(g, &by_pid, &own))
        .collect();
    let shared: Vec<&AppGroup> = snapshot
        .app_groups
        .iter()
        .filter(|g| !mine.contains(g) && group_matches(g, &by_pid, &matchers.shared))
        .collect();
    (mine, shared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::GroupKind;
    use crate::estimate::apps::profiles::embedded;

    fn group(name: &str, mb: u64) -> AppGroup {
        AppGroup {
            name: name.to_string(),
            kind: GroupKind::AppBundle,
            bundle_path: None,
            memory_bytes: mb * 1024 * 1024,
            memory_kinds: Vec::new(),
            process_count: 1,
            unreadable_count: 0,
            pids: Vec::new(),
        }
    }

    fn name_of(r: &Resolution<'_>) -> String {
        match r {
            Resolution::Profile { profile, vm } => format!("{} / {vm:?}", profile.name),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn names_and_aliases_ignore_case_and_spacing() {
        let profiles = embedded();
        for (target, expected) in [
            ("Docker Desktop", "Docker Desktop / None"),
            ("docker", "Docker Desktop / None"),
            ("  docker   desktop ", "Docker Desktop / None"),
            ("intellij", "IntelliJ IDEA / None"),
            ("IntelliJ IDEA CE", "IntelliJ IDEA / None"),
            ("utm", "UTM / None"),
            ("UTM: Ubuntu", "UTM / Some(\"Ubuntu\")"),
            ("utm:ubuntu 24.04", "UTM / Some(\"ubuntu 24.04\")"),
            ("VMware", "VMware Fusion / None"),
        ] {
            let r = resolve(target, &profiles, Os::Macos, &[]);
            assert_eq!(name_of(&r), expected, "{target}");
        }
        assert_eq!(
            name_of(&resolve("vmware", &profiles, Os::Linux, &[])),
            "VMware Workstation / None"
        );
    }

    #[test]
    fn other_os_running_and_unknown() {
        let profiles = embedded();
        assert!(matches!(
            resolve("UTM", &profiles, Os::Windows, &[]),
            Resolution::OtherOs { .. }
        ));
        let groups = vec![
            group("Slack", 900),
            group("Google Chrome", 1500),
            group("docker", 300),
        ];
        assert!(matches!(
            resolve("slack", &profiles, Os::Macos, &groups),
            Resolution::Running { group } if group.name == "Slack"
        ));
        match resolve("Dokcer", &profiles, Os::Macos, &groups) {
            Resolution::Unknown { suggestions } => assert_eq!(suggestions, ["Docker Desktop"]),
            other => panic!("{other:?}"),
        }
        match resolve("chrome", &profiles, Os::Macos, &groups) {
            Resolution::Unknown { suggestions } => assert_eq!(suggestions, ["Google Chrome"]),
            other => panic!("{other:?}"),
        }
        match resolve("Storm", &profiles, Os::Macos, &groups) {
            Resolution::Unknown { suggestions } => {
                assert_eq!(suggestions, ["WebStorm", "PhpStorm"]);
            }
            other => panic!("{other:?}"),
        }
        match resolve("zz", &profiles, Os::Macos, &groups) {
            Resolution::Unknown { suggestions } => assert!(suggestions.is_empty()),
            other => panic!("{other:?}"),
        }
        // A colon that names no VM app is just an unknown name.
        assert!(matches!(
            resolve("Thing: other", &profiles, Os::Macos, &groups),
            Resolution::Unknown { .. }
        ));
    }

    #[test]
    fn duplicate_user_names_are_ambiguous() {
        let mut profiles = embedded();
        let mut copy = profiles[0].clone();
        copy.name = "Docker Engine".to_string();
        profiles.push(copy);
        match resolve("docker", &profiles, Os::Macos, &[]) {
            Resolution::Ambiguous(names) => {
                assert_eq!(names, ["Docker Desktop", "Docker Engine"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn edit_distance() {
        assert_eq!(distance("docker", "dokcer"), 2);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("utm", "utm"), 0);
        assert_eq!(normalize("Docker Desktop.exe"), "docker desktop");
        assert_eq!(normalize("UTM.app"), "utm");
        assert_eq!(normalize(".exe"), ".exe");
    }
}
