//! From a resolved profile to the memory an app still needs: run the readers, pick the virtual
//! machine, find the running app and subtract what it already holds (ADR-020).

use serde::Serialize;

use super::catalog::CatalogEntry;
use super::learned::Learned;
use super::profiles::{Matchers, Profile};
use super::resolve::{normalize, running_groups};
use super::{Confidence, DeclaredItem, Env, Os, ReadError, Reading, ReadingKind};
use crate::core::{Provenance, Snapshot, format_bytes};

/// The running app, measured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Running {
    /// Measured memory of the app's running processes.
    pub held_bytes: u64,
    pub provenance: Provenance,
    /// The running app groups that make it up.
    pub groups: Vec<String>,
    pub pids: Vec<u32>,
    /// Whether `held_bytes` was subtracted from the need. Not for virtual machines: canfit can't
    /// tell which VM a running process belongs to.
    pub subtracted: bool,
}

/// Everything canfit concluded about one app target.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppCheck {
    /// What the card calls it: "Docker Desktop", "UTM: Ubuntu 24.04".
    pub name: String,
    /// The profile used.
    pub profile: String,
    /// The virtual machine asked for, by its configured name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm: Option<String>,
    /// Where the need comes from: `config` (the app's settings), `history` (what canfit measured
    /// it use) or `catalog` (the seed catalog's estimate). ADR-020, ADR-021.
    pub source: &'static str,
    pub confidence: Confidence,
    pub reading: Reading,
    /// The item the need is computed for: the VM asked for, the largest VM, or the app.
    pub deciding_item: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<Running>,
    /// Declared memory plus overhead of the deciding item.
    pub declared_bytes: u64,
    /// What still has to fit: declared minus what the running app already holds, never below 0.
    pub need_bytes: u64,
    /// Readers that could not read, and other caveats.
    pub notes: Vec<String>,
    /// For `source = "history"`: what canfit measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Learned>,
    /// For `source = "catalog"`: the catalog entry used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogInfo>,
    /// The running app groups that are this app or may hold its memory: never suggested for
    /// closing to make room for it.
    #[serde(skip)]
    pub related_groups: Vec<String>,
}

/// The catalog values behind a catalog estimate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogInfo {
    pub category: String,
    pub typical_bytes: u64,
    pub need_bytes: u64,
    /// Always `estimated`.
    pub provenance: Provenance,
    /// Where the catalog's numbers come from.
    pub source: String,
    /// When the catalog's values were last reviewed.
    pub reviewed: String,
}

/// The running app found by `matchers`, the caveats about it, and every related group.
struct Found {
    running: Option<Running>,
    notes: Vec<String>,
    related: Vec<String>,
}

fn find_running(app: &str, matchers: &Matchers, snapshot: &Snapshot, subtract: bool) -> Found {
    let (mine, shared) = running_groups(matchers, snapshot);
    let running = (!mine.is_empty()).then(|| Running {
        held_bytes: mine.iter().map(|g| g.memory_bytes).sum(),
        provenance: Provenance::Measured,
        groups: mine.iter().map(|g| g.name.clone()).collect(),
        pids: mine.iter().flat_map(|g| g.pids.iter().copied()).collect(),
        subtracted: subtract,
    });
    let mut notes = Vec::new();
    if let Some(r) = &running {
        if !r.subtracted {
            notes.push(format!(
                "{app} is running ({} in use, measured). canfit can't tell which virtual machine a \
                 running process belongs to, so if one is already on, its memory is counted twice \
                 and the need is overstated.",
                format_bytes(r.held_bytes)
            ));
        }
        if !shared.is_empty() {
            let shared_bytes: u64 = shared.iter().map(|g| g.memory_bytes).sum();
            notes.push(format!(
                "Part of {app}'s memory may sit in a shared system process ({}, {} now) that canfit \
                 can't attribute to it, so the need may be overstated.",
                shared[0].name,
                format_bytes(shared_bytes)
            ));
        }
    }
    let related = mine
        .iter()
        .chain(shared.iter())
        .map(|g| g.name.clone())
        .collect();
    Found {
        running,
        notes,
        related,
    }
}

/// An estimate that is one number for the whole app (history or catalog): subtracts what the
/// running app already holds, like a declared one.
fn single(
    name: &str,
    source: &'static str,
    item: DeclaredItem,
    reading_source: String,
    confidence: Confidence,
    matchers: &Matchers,
    snapshot: &Snapshot,
) -> AppCheck {
    let declared = item.total_bytes();
    let found = find_running(name, matchers, snapshot, true);
    let need = match &found.running {
        Some(r) => declared.saturating_sub(r.held_bytes),
        None => declared,
    };
    AppCheck {
        name: name.to_string(),
        profile: name.to_string(),
        vm: None,
        source,
        confidence,
        reading: Reading {
            reader: source.to_string(),
            source: reading_source,
            kind: ReadingKind::App,
            items: vec![item],
            confidence,
            notes: Vec::new(),
        },
        deciding_item: 0,
        running: found.running,
        declared_bytes: declared,
        need_bytes: need,
        notes: found.notes,
        history: None,
        catalog: None,
        related_groups: found.related,
    }
}

/// The check of an app from what canfit measured it use (ADR-021). `names` are the app group
/// names the history was looked up with; they also find the running app.
pub fn from_history(
    name: &str,
    learned: Learned,
    names: &[String],
    now_ms: u64,
    snapshot: &Snapshot,
) -> AppCheck {
    let watched = learned.watch.is_some();
    let percentile = match (watched, learned.p95_bytes) {
        (true, _) => format!(
            "peak of {} samples",
            group_digits_u64(learned.detailed_samples)
        ),
        (false, Some(_)) => format!(
            "95th percentile of {} samples",
            group_digits_u64(learned.detailed_samples)
        ),
        (false, None) => "largest use recorded (the detailed samples were pruned)".to_string(),
    };
    let item = DeclaredItem {
        name: if watched {
            "Measured by watch"
        } else {
            "Seen by canfit"
        }
        .to_string(),
        declared_bytes: learned
            .watch
            .as_ref()
            .map_or(learned.need_bytes, |w| w.peak_bytes),
        declared_provenance: learned.provenance,
        detail: match &learned.watch {
            Some(w) => format!(
                "{percentile} over {}, typical {}",
                crate::estimate::apps::learned::span(w.duration_ms),
                format_bytes(learned.typical_bytes)
            ),
            None => format!(
                "{percentile}, typical {} · {}",
                format_bytes(learned.typical_bytes),
                learned.evidence(now_ms)
            ),
        },
        overhead_bytes: learned
            .watch
            .as_ref()
            .and_then(|w| w.mapped_model_bytes)
            .unwrap_or(0),
        overhead_detail: learned
            .watch
            .as_ref()
            .filter(|w| w.mapped_model_bytes.is_some())
            .map(|_| "model weights it mapped from the file, not in the measured peak".to_string()),
        path: None,
    };
    let matchers = Matchers {
        bundles: Vec::new(),
        names: names
            .iter()
            .cloned()
            .chain(learned.names.iter().cloned())
            .collect(),
        shared: Vec::new(),
    };
    let mut c = single(
        name,
        "history",
        item,
        "canfit's history of this machine".to_string(),
        learned.confidence,
        &matchers,
        snapshot,
    );
    if learned.confidence == Confidence::Low {
        c.notes.push(format!(
            "canfit has seen {name} only briefly ({}), so this is a low-confidence estimate. Keep \
             the monitor (canfit) open while you use {name} and the estimate improves.",
            learned.evidence(now_ms)
        ));
    }
    if let Some(w) = &learned.watch {
        if w.duration_ms < crate::estimate::apps::learned::WATCH_HIGH_MIN_MS {
            c.notes.push(format!(
                "canfit watched {name} for only {}, so it may not have reached its full size. \
                 Watch a longer, typical run (canfit watch -- ...) to be sure.",
                crate::estimate::apps::learned::span(w.duration_ms)
            ));
        }
        c.notes.push(
            "A watch run is sampled about once a second, so a brief spike between samples is \
             not in the peak."
                .to_string(),
        );
    } else {
        c.notes.push(
            "History keeps only apps among the 8 largest and above 100 MB at each sample, so the \
             need leans high (conservative)."
                .to_string(),
        );
    }
    c.history = Some(learned);
    c
}

/// The check of an app from the seed catalog (ADR-021): estimated, low confidence.
pub fn from_catalog(entry: &CatalogEntry, reviewed: &str, snapshot: &Snapshot) -> AppCheck {
    let item = DeclaredItem {
        name: "Catalog estimate".to_string(),
        declared_bytes: entry.need_bytes(),
        declared_provenance: Provenance::Estimated,
        detail: format!(
            "{} app, typical {}",
            entry.category,
            format_bytes(entry.typical_bytes())
        ),
        overhead_bytes: 0,
        overhead_detail: None,
        path: None,
    };
    let mut c = single(
        &entry.name,
        "catalog",
        item,
        "canfit's seed catalog".to_string(),
        Confidence::Low,
        &entry.matchers(),
        snapshot,
    );
    c.notes.push(format!(
        "canfit has never seen {} on this machine, so this is the seed catalog's rough average for \
         a moderate workload ({}), not a measurement. Keep the monitor (canfit) open while you use \
         it and canfit learns its real use.",
        entry.name, entry.source
    ));
    c.catalog = Some(CatalogInfo {
        category: entry.category.clone(),
        typical_bytes: entry.typical_bytes(),
        need_bytes: entry.need_bytes(),
        provenance: Provenance::Estimated,
        source: entry.source.clone(),
        reviewed: reviewed.to_string(),
    });
    c
}

fn group_digits_u64(n: u64) -> String {
    crate::estimate::model::group_digits(n)
}

/// The names that identify `profile`'s app on `os`: its names, aliases and matchers, for a
/// history lookup when its settings can't be read.
pub fn profile_names(profile: &Profile, os: Os) -> Vec<String> {
    let m = profile.matchers(os);
    profile
        .names()
        .map(str::to_string)
        .chain(m.bundles.iter().cloned())
        .chain(m.names.iter().cloned())
        .collect()
}

/// Runs the profile's readers in order; the first that reads a value wins. Readers that fail
/// leave a note. Fails with one sentence when none could read anything.
pub fn read_profile(profile: &Profile, env: &Env) -> Result<(Reading, Vec<String>), String> {
    let mut problems: Vec<String> = Vec::new();
    for reader in &profile.readers {
        match reader.read(&profile.name, env) {
            Ok(reading) => {
                let notes = problems.iter().map(|p| format!("canfit {p}.")).collect();
                return Ok((reading, notes));
            }
            Err(ReadError::UnsupportedOs) => {}
            Err(e) => problems.push(e.to_string()),
        }
    }
    if problems.is_empty() {
        return Err(format!(
            "canfit has no way to read {}'s settings on this OS.",
            profile.name
        ));
    }
    Err(format!(
        "canfit could not read {}'s settings: {}.",
        profile.name,
        problems.join("; ")
    ))
}

/// The item `vm` names: exact name first, then a unique prefix or substring, ignoring case.
pub fn select_vm(items: &[DeclaredItem], vm: &str) -> Result<usize, String> {
    let wanted = normalize(vm);
    let names: Vec<String> = items.iter().map(|i| normalize(&i.name)).collect();
    if let Some(i) = names.iter().position(|n| *n == wanted) {
        return Ok(i);
    }
    let close: Vec<usize> = (0..names.len())
        .filter(|&i| names[i].starts_with(&wanted))
        .collect();
    let close = if close.is_empty() {
        (0..names.len())
            .filter(|&i| names[i].contains(&wanted))
            .collect()
    } else {
        close
    };
    match close.as_slice() {
        [one] => Ok(*one),
        _ => {
            let list: Vec<&str> = if close.is_empty() {
                items.iter().map(|i| i.name.as_str()).collect()
            } else {
                close.iter().map(|&i| items[i].name.as_str()).collect()
            };
            let what = if close.is_empty() {
                "no"
            } else {
                "more than one"
            };
            Err(format!(
                "\"{vm}\" matches {what} virtual machine; choose one of: {}.",
                list.join(", ")
            ))
        }
    }
}

/// Builds the app check for `profile` (narrowed to `vm`) against `snapshot`.
pub fn check(
    profile: &Profile,
    vm: Option<&str>,
    env: &Env,
    snapshot: &Snapshot,
) -> Result<AppCheck, String> {
    let (mut reading, mut notes) = read_profile(profile, env)?;
    let mut name = profile.name.clone();
    let mut chosen_vm = None;
    let deciding_item = match (reading.kind, vm) {
        (ReadingKind::VirtualMachines, Some(vm)) => {
            let i = select_vm(&reading.items, vm)?;
            name = format!("{}: {}", profile.name, reading.items[i].name);
            chosen_vm = Some(reading.items[i].name.clone());
            // Only the chosen VM matters.
            let item = reading.items.swap_remove(i);
            reading.items = vec![item];
            0
        }
        (ReadingKind::VirtualMachines, None) => (0..reading.items.len())
            .max_by_key(|&i| (reading.items[i].total_bytes(), std::cmp::Reverse(i)))
            .unwrap_or(0),
        (ReadingKind::App, Some(vm)) => {
            return Err(format!(
                "{} has no virtual machines, so \"{vm}\" can't be checked on its own. Check \
                 \"{}\" instead.",
                profile.name, profile.name
            ));
        }
        (ReadingKind::App, None) => 0,
    };
    let declared = match reading.kind {
        ReadingKind::App => reading.items.iter().map(DeclaredItem::total_bytes).sum(),
        ReadingKind::VirtualMachines => reading.items[deciding_item].total_bytes(),
    };

    let found = find_running(
        &profile.name,
        profile.matchers(env.os),
        snapshot,
        reading.kind == ReadingKind::App,
    );
    let running = found.running;
    let need = match &running {
        Some(r) if r.subtracted => declared.saturating_sub(r.held_bytes),
        _ => declared,
    };
    notes.extend(found.notes);
    notes.extend(reading.notes.iter().cloned());
    Ok(AppCheck {
        name,
        profile: profile.name.clone(),
        vm: chosen_vm,
        source: "config",
        confidence: reading.confidence,
        deciding_item,
        running,
        declared_bytes: declared,
        need_bytes: need,
        notes,
        reading,
        history: None,
        catalog: None,
        related_groups: found.related,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sampler::tests::replay;
    use crate::core::{AppGroup, GroupKind, MemoryKind, Process, ProcessMemory};
    use crate::estimate::apps::Os;
    use crate::estimate::apps::profiles::embedded;
    use crate::estimate::apps::readers::tests::{GIB, MIB, env};

    fn profile(name: &str) -> Profile {
        embedded().into_iter().find(|p| p.name == name).unwrap()
    }

    /// A calm snapshot with the given running groups (name, bundle, MB).
    fn snapshot(groups: &[(&str, u64)]) -> Snapshot {
        let (_, mut s) = replay("calm");
        s.app_groups.clear();
        s.processes.clear();
        for (i, (name, mb)) in groups.iter().enumerate() {
            let pid = 1000 + i as u32;
            s.processes.push(Process {
                pid,
                ppid: Some(1),
                name: name.to_string(),
                exe: None,
                user: Some("me".to_string()),
                memory: Some(ProcessMemory {
                    bytes: mb * MIB,
                    kind: MemoryKind::Footprint,
                    provenance: Provenance::Measured,
                }),
                app_bundle: None,
            });
            s.app_groups.push(AppGroup {
                name: name.to_string(),
                kind: GroupKind::AppBundle,
                bundle_path: None,
                memory_bytes: mb * MIB,
                memory_kinds: vec![MemoryKind::Footprint],
                process_count: 1,
                unreadable_count: 0,
                pids: vec![pid],
            });
        }
        s
    }

    #[test]
    fn docker_not_running_needs_its_whole_vm() {
        let c = check(
            &profile("Docker Desktop"),
            None,
            &env(Os::Macos, "macos"),
            &snapshot(&[]),
        )
        .unwrap();
        assert_eq!(c.declared_bytes, 10 * GIB);
        assert_eq!(c.need_bytes, 10 * GIB);
        assert!(c.running.is_none());
        assert_eq!(c.source, "config");
        assert_eq!(c.confidence, Confidence::High);
    }

    #[test]
    fn a_running_app_needs_only_the_rest() {
        let s = snapshot(&[
            ("Docker", 3 * 1024),
            ("com.apple.Virtualization.VirtualMachine", 2048),
        ]);
        let c = check(
            &profile("Docker Desktop"),
            None,
            &env(Os::Macos, "macos"),
            &s,
        )
        .unwrap();
        let r = c.running.as_ref().unwrap();
        assert_eq!(r.held_bytes, 3 * GIB);
        assert!(r.subtracted);
        assert_eq!(c.need_bytes, 7 * GIB);
        assert!(
            c.notes.iter().any(|n| n.contains("shared system process")),
            "{:?}",
            c.notes
        );
        // Holding more than declared never makes the need negative.
        let s = snapshot(&[("Docker", 12 * 1024)]);
        let c = check(
            &profile("Docker Desktop"),
            None,
            &env(Os::Macos, "macos"),
            &s,
        )
        .unwrap();
        assert_eq!(c.need_bytes, 0);
    }

    #[test]
    fn vm_apps_decide_on_the_largest_vm_or_the_one_named() {
        let e = env(Os::Macos, "macos");
        let c = check(&profile("UTM"), None, &e, &snapshot(&[])).unwrap();
        assert_eq!(c.reading.items.len(), 3);
        assert_eq!(c.reading.items[c.deciding_item].name, "Ubuntu 24.04");
        assert_eq!(c.need_bytes, 8 * GIB);
        let c = check(&profile("UTM"), Some("kali"), &e, &snapshot(&[])).unwrap();
        assert_eq!(c.name, "UTM: Kali Linux");
        assert_eq!(c.vm.as_deref(), Some("Kali Linux"));
        assert_eq!(c.need_bytes, 4 * GIB);
        assert_eq!(c.reading.items.len(), 1);
        // Running UTM: nothing is subtracted, the caveat is noted.
        let c = check(
            &profile("UTM"),
            Some("Ubuntu 24.04"),
            &e,
            &snapshot(&[("UTM", 900)]),
        )
        .unwrap();
        assert_eq!(c.need_bytes, 8 * GIB);
        assert!(!c.running.as_ref().unwrap().subtracted);
        assert!(c.notes.iter().any(|n| n.contains("counted twice")));
        let err = check(&profile("UTM"), Some("windows"), &e, &snapshot(&[])).unwrap_err();
        assert!(
            err.contains("Kali Linux, Metasploitable 2, Ubuntu 24.04"),
            "{err}"
        );
        let err = check(&profile("Docker Desktop"), Some("x"), &e, &snapshot(&[])).unwrap_err();
        assert!(err.contains("has no virtual machines"), "{err}");
    }

    #[test]
    fn jvm_apps_add_the_estimated_overhead() {
        let c = check(
            &profile("IntelliJ IDEA"),
            None,
            &env(Os::Macos, "macos"),
            &snapshot(&[]),
        )
        .unwrap();
        assert_eq!(c.declared_bytes, 2 * GIB + 512 * MIB + GIB);
        assert_eq!(c.confidence, Confidence::Medium);
        let s = snapshot(&[("IntelliJ IDEA", 1024)]);
        let c = check(
            &profile("IntelliJ IDEA"),
            None,
            &env(Os::Macos, "macos"),
            &s,
        )
        .unwrap();
        assert_eq!(c.need_bytes, GIB + 512 * MIB + GIB);
    }

    #[test]
    fn unreadable_settings_explain_themselves() {
        let err = check(
            &profile("Docker Desktop"),
            None,
            &env(Os::Macos, "broken"),
            &snapshot(&[]),
        )
        .unwrap_err();
        assert!(
            err.starts_with("canfit could not read Docker Desktop's settings"),
            "{err}"
        );
        let err = check(
            &profile("UTM"),
            None,
            &env(Os::Linux, "macos"),
            &snapshot(&[]),
        )
        .unwrap_err();
        assert!(
            err.contains("no way to read UTM's settings on this OS"),
            "{err}"
        );
    }

    #[test]
    fn select_vm_rules() {
        let item = |name: &str| DeclaredItem {
            name: name.to_string(),
            declared_bytes: GIB,
            declared_provenance: Provenance::ReadFromConfig,
            detail: String::new(),
            overhead_bytes: 0,
            overhead_detail: None,
            path: None,
        };
        let items = vec![item("Ubuntu 24.04"), item("Ubuntu 22.04"), item("Kali")];
        assert_eq!(select_vm(&items, "ubuntu 22.04"), Ok(1));
        assert_eq!(select_vm(&items, "ka"), Ok(2));
        assert_eq!(select_vm(&items, "22"), Ok(1));
        assert!(
            select_vm(&items, "ubuntu")
                .unwrap_err()
                .contains("more than one")
        );
        assert!(
            select_vm(&items, "arch")
                .unwrap_err()
                .contains("no virtual machine")
        );
    }
}
