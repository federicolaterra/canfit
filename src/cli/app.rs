//! The app fit check on the command line (ADR-020): resolving the target, the card, the answer
//! sentence and the JSON entry. Verdicts reuse the model bands (ADR-012) against available
//! memory only, worded for apps: fits comfortably · fits but will slow down · at risk.

use std::fmt::Write as _;

use serde::Serialize;

use super::card::Row;
use super::fit::{FitReport, Totals};
use crate::core::{Provenance, Snapshot, format_bytes};
use crate::estimate::apps::catalog::{self, Catalog};
use crate::estimate::apps::check::{self, AppCheck};
use crate::estimate::apps::learned::AppHistory;
use crate::estimate::apps::profiles::{self, Profile};
use crate::estimate::apps::resolve::{self, Resolution};
use crate::estimate::apps::{Confidence, Env, Os, ReadingKind};
use crate::estimate::fit::{self, LimitKind, TightCause, Verdict};

/// Everything an app target may be estimated from, in order (ADR-021): app profiles (declared
/// configuration), canfit's history of this machine, the seed catalog.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    pub profiles: Vec<Profile>,
    pub history: Option<AppHistory>,
    /// Why the history could not be read, when it couldn't (the check goes on without it).
    pub history_problem: Option<String>,
    pub catalog: Option<Catalog>,
    /// The time history ages are measured at.
    pub now_ms: u64,
}

impl Sources {
    /// Loads the profiles (a broken user file is an error), the history (problems become a
    /// note) and the catalog.
    pub fn load() -> Result<Sources, String> {
        let profiles = profiles::load().map_err(|e| e.to_string())?;
        let (history, history_problem) = match crate::history::database_path() {
            None => (None, None),
            Some(path) => match AppHistory::load(&path) {
                Ok(h) => (h, None),
                Err(e) => (None, Some(e.to_string())),
            },
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        Ok(Sources {
            profiles,
            history,
            history_problem,
            catalog: Some(catalog::embedded()),
            now_ms,
        })
    }
}

/// One app target, checked.
#[derive(Debug, Clone, Serialize)]
pub struct AppEntry {
    #[serde(flatten)]
    pub check: AppCheck,
    pub margin_percent: u32,
    /// For a VM app checked as a whole: each VM's own verdict.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vm_verdicts: Vec<VmVerdict>,
    /// App groups that are the target itself (or may hold its memory): never suggested for
    /// closing to make room for it.
    #[serde(skip)]
    pub excluded_groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmVerdict {
    pub name: String,
    /// Declared memory plus the margin.
    pub needed_bytes: u64,
    pub verdict: Verdict,
}

/// The words of a verdict for an app (spec §6.3).
pub fn verdict_label(v: Verdict) -> &'static str {
    match v {
        Verdict::Fits => "fits comfortably",
        Verdict::Tight => "fits but will slow down",
        Verdict::DoesntFit => "at risk",
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// Resolves an app target and estimates its need: declared configuration, then history, then
/// the catalog (ADR-021). Errors are one plain sentence (exit 3).
pub fn check_target(
    target: &str,
    sources: &Sources,
    snapshot: &Snapshot,
) -> Result<AppCheck, String> {
    let os = Os::from_key(&snapshot.platform.os)
        .ok_or_else(|| "canfit can't check apps on this operating system yet.".to_string())?;
    let profiles = &sources.profiles;
    let mut names: Vec<String> = vec![target.to_string()];
    let mut display: Option<String> = None;
    let mut config_error: Option<String> = None;
    let mut running: Option<(String, u64)> = None;
    match resolve::resolve(target, profiles, os, &snapshot.app_groups) {
        Resolution::Profile { profile, vm } => {
            let env = Env::system(snapshot.memory.total_bytes)
                .ok_or_else(|| "canfit could not find your home folder.".to_string())?;
            match check::check(profile, vm.as_deref(), &env, snapshot) {
                Ok(c) => return Ok(c),
                // One virtual machine can only come from its settings.
                Err(e) if vm.is_some() => return Err(e),
                Err(e) => {
                    config_error = Some(e);
                    names = check::profile_names(profile, os);
                    display = Some(profile.name.clone());
                }
            }
        }
        Resolution::OtherOs { profile } => {
            return Err(format!(
                "{} runs on {} only, so canfit has no settings to read for it here.",
                profile.name,
                list(&profile.os.iter().map(|o| os_name(o)).collect::<Vec<_>>())
            ));
        }
        Resolution::Ambiguous(names) => {
            return Err(format!(
                "\"{target}\" matches several apps: {}. Use the full name.",
                list(&names.iter().map(String::as_str).collect::<Vec<_>>())
            ));
        }
        Resolution::Running { group } => {
            names.push(group.name.clone());
            running = Some((group.name.clone(), group.memory_bytes));
        }
        Resolution::Unknown { .. } => {}
    }
    let entry = sources
        .catalog
        .as_ref()
        .and_then(|c| c.find(display.as_deref().unwrap_or(target), os));
    if let Some(e) = entry {
        names.extend(e.names().map(str::to_string));
        names.extend(e.processes.iter().cloned());
    }
    let fallback = |c: &mut AppCheck, what: &str| {
        if let Some(e) = &config_error {
            c.notes
                .insert(0, format!("{e} So canfit uses {what} instead."));
        }
        if let Some(p) = &sources.history_problem {
            c.notes
                .push(format!("canfit could not read its history: {p}"));
        }
    };
    if let Some(learned) = sources
        .history
        .as_ref()
        .and_then(|h| h.learn(&names, sources.now_ms))
    {
        let name = display
            .clone()
            .or_else(|| entry.map(|e| e.name.clone()))
            .or_else(|| learned.names.first().cloned())
            .unwrap_or_else(|| target.to_string());
        let mut c = check::from_history(&name, learned, &names, sources.now_ms, snapshot);
        fallback(&mut c, "what it measured the app use");
        return Ok(c);
    }
    if let (Some(e), Some(cat)) = (entry, sources.catalog.as_ref()) {
        let mut c = check::from_catalog(e, &cat.reviewed, snapshot);
        fallback(&mut c, "the seed catalog's estimate");
        return Ok(c);
    }
    if let Some(e) = config_error {
        return Err(e);
    }
    if let Some(e) = sources
        .catalog
        .as_ref()
        .and_then(|c| c.find_elsewhere(target))
    {
        return Err(format!(
            "{} runs on {} only, so canfit has no estimate for it here.",
            e.name,
            list(&e.os.iter().map(|o| os_name(o)).collect::<Vec<_>>())
        ));
    }
    if let Some((name, bytes)) = running {
        return Err(format!(
            "{name} is running and uses {} (measured), but canfit has no settings, history or \
             catalog value for it, so it can't tell how much memory it needs. Keep the monitor \
             (canfit) open while you use {name} and canfit learns it; docs/open-source/apps.md \
             explains how to add a profile or a catalog entry.",
            format_bytes(bytes)
        ));
    }
    let mut suggestions = resolve::suggestions(target, profiles, os, &snapshot.app_groups);
    let extra: Vec<String> = sources
        .catalog
        .iter()
        .flat_map(|c| c.apps.iter().filter(|a| a.runs_on(os)))
        .filter(|a| a.names().any(|n| resolve::is_close(target, n)))
        .map(|a| a.name.clone())
        .chain(
            sources
                .history
                .iter()
                .flat_map(|h| h.names())
                .filter(|n| resolve::is_close(target, n))
                .map(str::to_string),
        )
        .collect();
    for name in extra {
        if !suggestions
            .iter()
            .any(|s| resolve::normalize(s) == resolve::normalize(&name))
        {
            suggestions.push(name);
        }
    }
    suggestions.truncate(5);
    let hint = if suggestions.is_empty() {
        let known: Vec<&str> = profiles
            .iter()
            .filter(|p| p.runs_on(os))
            .map(|p| p.name.as_str())
            .collect();
        format!(
            " canfit reads the settings of {}, and knows common apps from its catalog (see \
             docs/open-source/apps.md).",
            known.join(", ")
        )
    } else {
        format!(
            " Did you mean {}?",
            suggestions
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(" or ")
        )
    };
    Err(format!(
        "canfit doesn't know an app called \"{target}\": it isn't a model file or folder, canfit \
         has no settings, history or catalog value for it.{hint}"
    ))
}

fn os_name(key: &str) -> &str {
    match key {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

fn with_margin(bytes: u64, margin_percent: u32) -> (u64, u64) {
    let margin = bytes * u64::from(margin_percent) / 100;
    (margin, bytes + margin)
}

/// One checked app with its margin; `vms` adds each VM's own verdict for a VM app checked as a
/// whole.
pub fn entry(check: AppCheck, margin_percent: u32, snapshot: &Snapshot, vms: bool) -> AppEntry {
    let vm_verdicts =
        if vms && check.reading.kind == ReadingKind::VirtualMachines && check.vm.is_none() {
            check
                .reading
                .items
                .iter()
                .map(|i| {
                    let (_, needed) = with_margin(i.total_bytes(), margin_percent);
                    VmVerdict {
                        name: i.name.clone(),
                        needed_bytes: needed,
                        verdict: fit::evaluate_memory(i.total_bytes(), needed, snapshot).verdict,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
    AppEntry {
        excluded_groups: check.related_groups.clone(),
        check,
        margin_percent,
        vm_verdicts,
    }
}

/// The margin of an app's need.
pub fn margin_of(entry: &AppEntry) -> u64 {
    with_margin(entry.check.need_bytes, entry.margin_percent).0
}

/// The fit report of one checked app.
pub fn build(check: AppCheck, margin_percent: u32, snapshot: &Snapshot) -> FitReport {
    let base = check.need_bytes;
    let (margin, total) = with_margin(base, margin_percent);
    let result = fit::evaluate_memory(base, total, snapshot);
    let items = &check.reading.items;
    let overhead = match check.reading.kind {
        ReadingKind::App => items.iter().map(|i| i.overhead_bytes).sum(),
        ReadingKind::VirtualMachines => items[check.deciding_item].overhead_bytes,
    };
    let deciding = items[check.deciding_item].declared_provenance;
    let uniform = match check.reading.kind {
        ReadingKind::App => items.iter().all(|i| i.declared_provenance == deciding),
        ReadingKind::VirtualMachines => true,
    };
    let provenance = if uniform && overhead == 0 && margin == 0 {
        deciding
    } else {
        Provenance::Estimated
    };
    let entry = entry(check, margin_percent, snapshot, true);
    FitReport {
        models: Vec::new(),
        apps: vec![entry],
        gpu_part: None,
        total: Totals {
            weights_bytes: 0,
            kv_cache_bytes: 0,
            overhead_bytes: overhead,
            apps_bytes: base,
            margin_bytes: margin,
            base_bytes: base,
            total_bytes: total,
            provenance,
        },
        result,
        snapshot_memory_total: snapshot.memory.total_bytes,
    }
}

/// What the answer talks about: the app, the VM, or a VM app's largest VM.
fn subject(entry: &AppEntry) -> String {
    let c = &entry.check;
    if c.reading.kind == ReadingKind::VirtualMachines && c.vm.is_none() {
        if c.reading.items.len() == 1 {
            return format!("{} ({})", c.profile, c.reading.items[0].name);
        }
        return format!(
            "{}'s largest VM ({})",
            c.profile, c.reading.items[c.deciding_item].name
        );
    }
    c.name.clone()
}

/// The one-sentence answer for an app.
pub fn summary_sentence(report: &FitReport) -> String {
    let entry = &report.apps[0];
    let name = subject(entry);
    let needed = format_bytes(report.total.total_bytes);
    let check = report.result.deciding();
    let limit = format_bytes(check.limit_bytes);
    let symbol = report.result.verdict.symbol();
    let up_to = if entry.check.running.as_ref().is_some_and(|r| r.subtracted) {
        "it still needs about"
    } else {
        match entry.check.source {
            "history" if is_watched(&entry.check) => "canfit measured it use up to",
            "history" => "canfit has seen it use up to about",
            "catalog" => "canfit's catalog estimates about",
            _ => "it is set to use up to",
        }
    };
    match (report.result.verdict, check.tight_cause) {
        (Verdict::Fits, _) => format!(
            "{symbol} Yes, {name} fits comfortably: {up_to} {needed} with the safety margin and \
             {limit} is available now."
        ),
        (Verdict::Tight, Some(TightCause::PressureCritical)) => format!(
            "{symbol} {name} fits on paper ({needed} of {limit} available), but memory pressure \
             is already critical, so it will slow down."
        ),
        (Verdict::Tight, _) => format!(
            "{symbol} {name} fits but will slow down: {up_to} {needed} with the safety margin \
             and {limit} is available, so the system will compress or swap memory."
        ),
        (Verdict::DoesntFit, _) => format!(
            "{symbol} No, {name} is at risk right now: {up_to} {needed} with the safety margin \
             and only {limit} is available."
        ),
    }
}

/// Sentences that follow the answer: the app already running, the other VMs.
pub fn follow_up(entry: &AppEntry) -> Vec<String> {
    let mut out = Vec::new();
    let c = &entry.check;
    if let Some(r) = &c.running {
        if r.subtracted {
            out.push(format!(
                "{} is already running: {} in use of {}.",
                c.profile,
                format_bytes(r.held_bytes),
                format_bytes(c.declared_bytes)
            ));
        } else {
            out.push(format!(
                "{} is already running ({} in use).",
                c.profile,
                format_bytes(r.held_bytes)
            ));
        }
    }
    if entry.vm_verdicts.len() > 1 {
        let mut parts = Vec::new();
        for v in [Verdict::Fits, Verdict::Tight, Verdict::DoesntFit] {
            let names: Vec<&str> = entry
                .vm_verdicts
                .iter()
                .filter(|x| x.verdict == v)
                .map(|x| x.name.as_str())
                .collect();
            let one = names.len() == 1;
            let verb = match v {
                Verdict::Fits if one => "fits comfortably",
                Verdict::Fits => "fit comfortably",
                Verdict::Tight if one => "fits but will slow down",
                Verdict::Tight => "fit but will slow down",
                Verdict::DoesntFit if one => "is at risk",
                Verdict::DoesntFit => "are at risk",
            };
            if !names.is_empty() {
                parts.push(format!("{} {verb}", list(&names)));
            }
        }
        out.push(format!(
            "Of {}'s {}, {}. Check one with canfit \"{}: NAME\".",
            c.profile,
            plural(
                entry.vm_verdicts.len(),
                "virtual machine",
                "virtual machines"
            ),
            parts.join("; "),
            c.profile
        ));
    }
    out
}

pub fn confidence_label(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
}

/// The card rows of an app check.
pub fn rows(report: &FitReport) -> Vec<Option<Row>> {
    let entry = &report.apps[0];
    let c = &entry.check;
    let mut rows = Vec::new();
    let vms = c.reading.kind == ReadingKind::VirtualMachines;
    for (i, item) in c.reading.items.iter().enumerate() {
        let mut text = format!(
            "{} · {}",
            value_label(c, item.declared_provenance),
            item.detail
        );
        if let Some(v) = entry.vm_verdicts.get(i) {
            text = format!(
                "{} · {} {}",
                item.declared_provenance.label(),
                v.verdict.symbol(),
                verdict_label(v.verdict)
            );
        }
        rows.push(Some(Row::new(
            item.name.clone(),
            format_bytes(item.declared_bytes),
            text,
        )));
        if item.overhead_bytes > 0 && is_watched(c) {
            rows.push(Some(Row::new(
                "Model file",
                format_bytes(item.overhead_bytes),
                format!(
                    "read from file · {}",
                    item.overhead_detail.as_deref().unwrap_or("mapped weights")
                ),
            )));
        } else if item.overhead_bytes > 0 && (!vms || i == c.deciding_item) {
            rows.push(Some(Row::new(
                "Beyond the heap",
                format_bytes(item.overhead_bytes),
                format!(
                    "estimated · {}",
                    item.overhead_detail.as_deref().unwrap_or("overhead")
                ),
            )));
        }
    }
    if let Some(r) = c.running.as_ref().filter(|r| r.subtracted) {
        rows.push(Some(Row::new(
            "Already in use",
            format!("−{}", format_bytes(r.held_bytes)),
            format!("measured · {} is running", c.profile),
        )));
    }
    let t = &report.total;
    rows.push(Some(Row::new(
        "Margin",
        format_bytes(t.margin_bytes),
        format!("estimated · {}% safety margin", entry.margin_percent),
    )));
    let needed_text = if vms && c.vm.is_none() && c.reading.items.len() > 1 {
        format!(
            "{} · largest VM · confidence {}",
            t.provenance.label(),
            confidence_label(c.confidence)
        )
    } else {
        format!(
            "{} · confidence {}",
            t.provenance.label(),
            confidence_label(c.confidence)
        )
    };
    rows.push(Some(Row::new(
        "Needed",
        format_bytes(t.total_bytes),
        needed_text,
    )));
    rows.push(None);
    for check in &report.result.checks {
        let label = match check.limit {
            LimitKind::AvailableMemory => "Available now",
            LimitKind::GpuMemoryLimit => "GPU limit",
        };
        let symbol = check.verdict.symbol();
        let headroom = format_bytes(check.headroom_bytes.unsigned_abs());
        let status = match (check.verdict, check.tight_cause) {
            (Verdict::Fits, _) => format!("{symbol} {headroom} to spare"),
            (Verdict::Tight, Some(TightCause::PressureCritical)) => {
                format!("{symbol} pressure is critical")
            }
            (Verdict::Tight, _) => format!("{symbol} only without the margin"),
            (Verdict::DoesntFit, _) => format!("{symbol} {headroom} short"),
        };
        rows.push(Some(Row::new(
            label,
            format_bytes(check.limit_bytes),
            format!("{} · {status}", check.provenance.label()),
        )));
    }
    rows
}

/// How a value of `c` is labelled: read from config, measured by canfit, estimated, or
/// estimate from catalog.
pub fn value_label(c: &AppCheck, provenance: Provenance) -> String {
    if let Some(w) = c.history.as_ref().and_then(|h| h.watch.as_ref()) {
        return format!("measured by watch on {}", local_date(w.started_unix_ms));
    }
    match (c.source, provenance) {
        ("catalog", _) => "estimate from catalog",
        ("history", Provenance::Measured) => "measured by canfit",
        ("history", _) => "estimated from little history",
        (_, p) => p.label(),
    }
    .to_string()
}

/// True when an app's need comes from a run measured by `canfit watch` (ADR-023).
pub fn is_watched(c: &AppCheck) -> bool {
    c.history.as_ref().is_some_and(|h| h.watch.is_some())
}

/// A Unix time as a local date, `2026-11-10`.
pub fn local_date(unix_ms: u64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_millis_opt(i64::try_from(unix_ms).unwrap_or(i64::MAX))
        .single()
        .map_or_else(|| "?".to_string(), |d| d.format("%Y-%m-%d").to_string())
}

/// The sentence naming where an app's need comes from.
pub fn source_sentence(c: &AppCheck) -> String {
    match c.source {
        "history" if is_watched(c) => "From canfit's history of this machine: the largest memory \
                      the command's whole process tree used while `canfit watch` ran it."
            .to_string(),
        "history" => "From canfit's history of this machine: the memory the app's processes used \
                      while the monitor was open (95th percentile)."
            .to_string(),
        "catalog" => format!(
            "From canfit's seed catalog (reviewed {}).",
            c.catalog.as_ref().map_or("?", |k| k.reviewed.as_str())
        ),
        _ => format!("Read from {}.", c.reading.source),
    }
}

/// Notes printed under an app card.
pub fn notes(report: &FitReport) -> Vec<String> {
    let c = &report.apps[0].check;
    let mut notes = vec![source_sentence(c)];
    notes.extend(c.notes.iter().cloned());
    notes.push(if c.source == "config" {
        "The memory an app is configured to use is a maximum: it may use less. Available memory \
         includes file cache the system can reclaim."
            .to_string()
    } else {
        "Available memory includes file cache the system can reclaim.".to_string()
    });
    if report.result.pressure_level != crate::core::PressureLevel::Calm {
        notes.push(format!(
            "Memory pressure is {} right now.",
            report.result.pressure_level.label()
        ));
    }
    notes
}

/// The card title.
pub fn title(report: &FitReport) -> String {
    format!("Can {} fit?", report.apps[0].check.name)
}

/// The follow-up sentences as one block of lines.
pub fn follow_up_text(report: &FitReport) -> String {
    let mut out = String::new();
    for line in follow_up(&report.apps[0]) {
        let _ = writeln!(out, "{line}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sampler::tests::replay;
    use crate::core::{AppGroup, GroupKind, MemoryKind, Process, ProcessMemory};
    use crate::estimate::apps::learned::tests::{history, series};
    use crate::history::{DAY_MS, HOUR_MS};

    const MIB: u64 = 1024 * 1024;
    const T0: u64 = 1_800_000_000_000;

    fn snapshot(groups: &[(&str, u64)]) -> Snapshot {
        let (_, mut s) = replay("calm");
        s.platform.os = "macos".to_string();
        s.app_groups.clear();
        s.processes.clear();
        for (i, (name, mb)) in groups.iter().enumerate() {
            let pid = 2000 + i as u32;
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

    fn sources(apps: &[(&str, Vec<(u64, u64)>)]) -> Sources {
        Sources {
            profiles: profiles::embedded(),
            history: Some(history(apps)),
            history_problem: None,
            catalog: Some(catalog::embedded()),
            now_ms: T0 + DAY_MS + HOUR_MS,
        }
    }

    fn long_watch() -> Vec<(u64, u64)> {
        let mut s = series(T0, 100);
        s.extend(series(T0 + DAY_MS, 100));
        s
    }

    #[test]
    fn history_comes_before_the_catalog() {
        let src = sources(&[("Slack", long_watch())]);
        let c = check_target("slack", &src, &snapshot(&[])).unwrap();
        assert_eq!(c.source, "history");
        assert_eq!(c.name, "Slack");
        assert_eq!(c.confidence, Confidence::High);
        assert_eq!(c.need_bytes, 194 * MIB);
        let h = c.history.as_ref().unwrap();
        assert_eq!(h.samples, 200);
        assert_eq!(h.provenance, Provenance::Measured);
        assert!(
            c.reading.items[0]
                .detail
                .contains("95th percentile of 200 samples"),
            "{}",
            c.reading.items[0].detail
        );
        assert!(c.reading.items[0].detail.contains("over 2 days"));
        // A catalog alias finds history recorded under the app's group name.
        let src = sources(&[("Google Chrome", series(T0, 5))]);
        let c = check_target("chrome", &src, &snapshot(&[])).unwrap();
        assert_eq!((c.source, c.name.as_str()), ("history", "Google Chrome"));
        assert_eq!(c.confidence, Confidence::Low);
        assert!(
            c.notes.iter().any(|n| n.contains("only briefly")),
            "{:?}",
            c.notes
        );
        // An app only the history knows.
        let src = sources(&[("Some Tool", long_watch())]);
        let c = check_target("some tool", &src, &snapshot(&[])).unwrap();
        assert_eq!((c.source, c.name.as_str()), ("history", "Some Tool"));
    }

    #[test]
    fn the_catalog_answers_for_unseen_apps_and_unknown_apps_fail() {
        let src = sources(&[]);
        let c = check_target("Spotify", &src, &snapshot(&[])).unwrap();
        assert_eq!(c.source, "catalog");
        assert_eq!(c.confidence, Confidence::Low);
        assert_eq!(
            c.reading.items[0].declared_provenance,
            Provenance::Estimated
        );
        assert_eq!(c.need_bytes, 800 * MIB);
        assert_eq!(
            value_label(&c, Provenance::Estimated),
            "estimate from catalog"
        );
        let err = check_target("Qzxwv", &src, &snapshot(&[])).unwrap_err();
        assert!(
            err.contains("no settings, history or catalog value"),
            "{err}"
        );
        // Close matches come from the catalog and the history too.
        let src = sources(&[("Some Tool", series(T0, 3))]);
        let err = check_target("Spotfy", &src, &snapshot(&[])).unwrap_err();
        assert!(err.contains("Did you mean \"Spotify\"?"), "{err}");
        let err = check_target("Some Tol", &src, &snapshot(&[])).unwrap_err();
        assert!(err.contains("\"Some Tool\""), "{err}");
        // Running, but nothing known about it.
        let err = check_target("Qzxwv", &src, &snapshot(&[("Qzxwv", 300)])).unwrap_err();
        assert!(err.contains("is running and uses 300 MB"), "{err}");
        // A catalog app for another OS.
        let mut s = snapshot(&[]);
        s.platform.os = "linux".to_string();
        let err = check_target("Safari", &src, &s).unwrap_err();
        assert!(err.contains("runs on macOS only"), "{err}");
    }

    #[test]
    fn running_apps_need_only_the_rest_and_are_excluded_from_make_room() {
        let src = sources(&[("Slack", long_watch())]);
        let s = snapshot(&[("Slack", 150), ("Google Chrome", 900)]);
        let c = check_target("Slack", &src, &s).unwrap();
        let r = c.running.as_ref().unwrap();
        assert!(r.subtracted);
        assert_eq!(c.need_bytes, (194 - 150) * MIB);
        assert_eq!(c.related_groups, ["Slack"]);
        let report = build(c, 10, &s);
        assert_eq!(report.apps[0].excluded_groups, ["Slack"]);
        assert!(
            summary_sentence(&report).contains("it still needs about"),
            "{}",
            summary_sentence(&report)
        );
        // Catalog apps too.
        let s = snapshot(&[("Spotify", 300)]);
        let c = check_target("spotify", &sources(&[]), &s).unwrap();
        assert_eq!(c.need_bytes, 500 * MIB);
        assert_eq!(c.related_groups, ["Spotify"]);
    }

    #[test]
    fn unreadable_settings_fall_back_to_history_with_a_note() {
        // No Docker settings on a fixture-less system home: history wins, and says why.
        let mut src = sources(&[("Docker", long_watch())]);
        src.catalog = None;
        let s = snapshot(&[]);
        // The names check_target looks the history up with when the settings can't be read.
        let profile = src
            .profiles
            .iter()
            .find(|p| p.name == "Docker Desktop")
            .unwrap();
        let names = check::profile_names(profile, Os::Macos);
        assert!(names.iter().any(|n| n == "Docker"), "{names:?}");
        let learned = src
            .history
            .as_ref()
            .unwrap()
            .learn(&names, src.now_ms)
            .unwrap();
        let c = check::from_history("Docker Desktop", learned, &names, src.now_ms, &s);
        assert_eq!(c.source, "history");
        assert_eq!(c.name, "Docker Desktop");
    }

    #[test]
    fn history_and_catalog_cards_label_their_values() {
        let src = sources(&[("Slack", long_watch())]);
        let s = snapshot(&[]);
        let report = build(check_target("Slack", &src, &s).unwrap(), 10, &s);
        let text = super::super::fit::render_text(&report, None, None);
        assert!(
            text.starts_with("● Yes, Slack fits comfortably: canfit has seen it use up to about"),
            "{text}"
        );
        assert!(
            text.contains("measured by canfit · 95th percentile"),
            "{text}"
        );
        assert!(text.contains("confidence high"), "{text}");
        assert!(
            text.contains("From canfit's history of this machine"),
            "{text}"
        );
        let v: serde_json::Value =
            serde_json::from_str(&super::super::fit::to_json(&report, None)).unwrap();
        assert_eq!(v["apps"][0]["source"], "history");
        assert_eq!(v["apps"][0]["history"]["samples"], 200);
        assert_eq!(v["apps"][0]["history"]["confidence"], "high");
        assert_eq!(v["apps"][0]["history"]["days"], 2);

        let report = build(check_target("Figma", &sources(&[]), &s).unwrap(), 10, &s);
        let text = super::super::fit::render_text(&report, None, None);
        assert!(text.contains("canfit's catalog estimates about"), "{text}");
        assert!(
            text.contains("estimate from catalog · design app"),
            "{text}"
        );
        assert!(text.contains("confidence low"), "{text}");
    }
}
