//! `canfit watch` (spec §5, §6.3, §9 source 4; ADR-023): launch a command, measure the real
//! memory of its whole process tree, warn when pressure turns strained or critical, summarize the
//! run and remember it for later fit checks.
//!
//! - this module: the pure parts, tested on synthetic process lists — which processes belong to
//!   the watched tree ([`Tree`]), the running statistics ([`Stats`]), when to warn ([`Alerts`]),
//!   the name a run is remembered under and how the child's end maps to canfit's exit code;
//! - [`args`]: finding a GGUF model (and its context) in the command's arguments;
//! - [`launch`]: spawning the child and the per-OS parts (signals, Linux subreaper, Windows job
//!   object).
//!
//! canfit never signals, closes or kills anything on its own here: it only forwards a Ctrl-C or
//! a termination request it received itself (spec §10).

pub mod args;
pub mod launch;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::core::{MemoryKind, PressureLevel, Process, Snapshot};
use crate::estimate::apps::learned::percentile;
use crate::history::events::CALM_GRACE_MS;

/// The processes that belong to the watched command: the child, everything it started (by
/// parent links), everything canfit already counted that is still alive under the same name
/// (a grandchild whose parent exited is reparented and keeps counting), and any extra process
/// ids the OS reports for the run (the Windows job object).
#[derive(Debug, Clone)]
pub struct Tree {
    root: u32,
    /// Another parent whose children belong to the tree: canfit itself on Linux, where it is the
    /// subreaper that adopts the command's orphans.
    adopter: Option<u32>,
    known: BTreeMap<u32, String>,
}

impl Tree {
    pub fn new(root: u32, adopter: Option<u32>) -> Tree {
        Tree {
            root,
            adopter,
            known: BTreeMap::new(),
        }
    }

    /// The members of the tree in `processes`, in the order of `processes`. `extra` are process
    /// ids the OS says belong to the run.
    pub fn members<'a>(&mut self, processes: &'a [Process], extra: &[u32]) -> Vec<&'a Process> {
        let by_pid: HashMap<u32, &Process> = processes.iter().map(|p| (p.pid, p)).collect();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in processes {
            if let Some(ppid) = p.ppid.filter(|&pp| pp != p.pid) {
                children.entry(ppid).or_default().push(p.pid);
            }
        }
        let mut members: BTreeSet<u32> = BTreeSet::new();
        let mut queue: Vec<u32> = Vec::new();
        let add = |pid: u32, members: &mut BTreeSet<u32>, queue: &mut Vec<u32>| {
            if by_pid.contains_key(&pid) && members.insert(pid) {
                queue.push(pid);
            }
        };
        add(self.root, &mut members, &mut queue);
        for &pid in extra {
            add(pid, &mut members, &mut queue);
        }
        for (&pid, name) in &self.known {
            if by_pid.get(&pid).is_some_and(|p| &p.name == name) {
                add(pid, &mut members, &mut queue);
            }
        }
        if let Some(adopter) = self.adopter {
            for &pid in children.get(&adopter).into_iter().flatten() {
                add(pid, &mut members, &mut queue);
            }
        }
        while let Some(pid) = queue.pop() {
            for &child in children.get(&pid).into_iter().flatten() {
                add(child, &mut members, &mut queue);
            }
        }
        // Never canfit itself, whatever the parent links say.
        if let Some(adopter) = self.adopter {
            members.remove(&adopter);
        }
        self.known = members
            .iter()
            .filter_map(|pid| by_pid.get(pid).map(|p| (*pid, p.name.clone())))
            .collect();
        processes
            .iter()
            .filter(|p| members.contains(&p.pid))
            .collect()
    }
}

/// One reading of the watched tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    /// Milliseconds since the command started.
    pub offset_ms: u64,
    /// Sum of the readable members' memory.
    pub bytes: u64,
    pub processes: usize,
}

/// The tree's memory in one snapshot: total bytes, member count, members whose memory could not
/// be read, and the kinds of measurement.
pub fn measure(members: &[&Process]) -> (u64, usize, usize, Vec<MemoryKind>) {
    let mut bytes = 0;
    let mut unreadable = 0;
    let mut kinds: Vec<MemoryKind> = Vec::new();
    for p in members {
        match &p.memory {
            Some(m) => {
                bytes += m.bytes;
                if !kinds.contains(&m.kind) {
                    kinds.push(m.kind);
                }
            }
            None => unreadable += 1,
        }
    }
    (bytes, members.len(), unreadable, kinds)
}

/// The highest pressure seen during the run.
#[derive(Debug, Clone, PartialEq)]
pub struct PeakPressure {
    pub score: u8,
    pub level: PressureLevel,
    pub offset_ms: u64,
    pub reason: String,
}

/// Everything measured over a run.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub points: Vec<Point>,
    pub kinds: Vec<MemoryKind>,
    /// Largest number of members whose memory could not be read in one sample.
    pub max_unreadable: usize,
    pub pressure: Option<PeakPressure>,
    /// The pressure level at the first reading, when the command had only just started.
    pub initial_level: Option<PressureLevel>,
}

impl Stats {
    pub fn record(&mut self, point: Point, unreadable: usize, kinds: &[MemoryKind]) {
        self.points.push(point);
        self.max_unreadable = self.max_unreadable.max(unreadable);
        for k in kinds {
            if !self.kinds.contains(k) {
                self.kinds.push(*k);
            }
        }
    }

    pub fn record_pressure(&mut self, snapshot: &Snapshot, offset_ms: u64) {
        let p = &snapshot.pressure;
        self.initial_level.get_or_insert(p.level);
        if self
            .pressure
            .as_ref()
            .is_none_or(|peak| p.score > peak.score)
        {
            self.pressure = Some(PeakPressure {
                score: p.score,
                level: p.level,
                offset_ms,
                reason: p.main_reason.message.clone(),
            });
        }
    }

    /// The sample with the most memory (the first one, on a tie).
    pub fn peak(&self) -> Option<Point> {
        self.points
            .iter()
            .copied()
            .reduce(|best, p| if p.bytes > best.bytes { p } else { best })
    }

    /// Median of the samples.
    pub fn typical_bytes(&self) -> u64 {
        let mut v: Vec<u64> = self.points.iter().map(|p| p.bytes).collect();
        percentile(&mut v, 50).unwrap_or(0)
    }

    /// 95th percentile of the samples (nearest rank).
    pub fn p95_bytes(&self) -> u64 {
        let mut v: Vec<u64> = self.points.iter().map(|p| p.bytes).collect();
        percentile(&mut v, 95).unwrap_or(0)
    }

    pub fn max_processes(&self) -> usize {
        self.points.iter().map(|p| p.processes).max().unwrap_or(0)
    }

    pub fn last(&self) -> Option<Point> {
        self.points.last().copied()
    }

    /// True when pressure was already at its highest level when the command started: the
    /// command did not cause it.
    pub fn pressure_preexisting(&self) -> bool {
        match (&self.pressure, self.initial_level) {
            (Some(p), Some(initial)) => p.level > PressureLevel::Calm && initial >= p.level,
            _ => false,
        }
    }

    /// The measurement kind most samples used, for labels; `None` without readable memory.
    pub fn memory_kind(&self) -> Option<MemoryKind> {
        self.kinds.first().copied()
    }
}

/// What to tell the user about pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alert {
    /// Pressure rose to this level (strained or critical) for the first time in this stretch.
    Rose(PressureLevel),
    /// Pressure stayed calm long enough after a warning: the stretch is over.
    Calmed,
}

/// Decides when to warn: once per level as pressure rises, and once when it has been calm again
/// for the event grace period (ADR-018), so a flapping reading does not repeat the warning.
#[derive(Debug, Clone, Default)]
pub struct Alerts {
    warned: Option<PressureLevel>,
    calm_since: Option<u64>,
}

impl Alerts {
    pub fn feed(&mut self, level: PressureLevel, offset_ms: u64) -> Option<Alert> {
        if level > PressureLevel::Calm {
            self.calm_since = None;
            if self.warned.is_none_or(|w| level > w) {
                self.warned = Some(level);
                return Some(Alert::Rose(level));
            }
            return None;
        }
        self.warned?;
        let since = *self.calm_since.get_or_insert(offset_ms);
        if offset_ms.saturating_sub(since) >= CALM_GRACE_MS {
            self.warned = None;
            self.calm_since = None;
            return Some(Alert::Calmed);
        }
        None
    }
}

/// The name a run is remembered under: `--name`, else the program's file name without its
/// folder and a Windows executable extension (`llama-server`, `ollama`).
pub fn run_name(explicit: Option<&str>, program: &str) -> String {
    if let Some(name) = explicit.map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    program_name(program)
}

/// The file name of a program, without its folder and a Windows executable extension.
pub fn program_name(program: &str) -> String {
    let file = program
        .rsplit(['/', '\\'])
        .next()
        .filter(|f| !f.is_empty())
        .unwrap_or(program);
    let lower = file.to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if lower.ends_with(ext) && file.len() > ext.len() {
            return file[..file.len() - ext.len()].to_string();
        }
    }
    file.to_string()
}

/// How the watched command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// It exited with this code (on Windows, the full 32-bit code).
    Exited(i32),
    /// A signal ended it (Unix).
    #[cfg_attr(not(unix), allow(dead_code))]
    Signaled(i32),
}

impl Ending {
    /// canfit's own exit code: the command's code; for a signal, 128 + its number, as shells do.
    /// On Windows the full code is kept (the process exit code is 32-bit there).
    pub fn exit_code(self) -> i32 {
        match self {
            Ending::Exited(code) => code,
            Ending::Signaled(signal) => 128 + signal,
        }
    }

    pub fn code(self) -> Option<i32> {
        match self {
            Ending::Exited(code) => Some(code),
            Ending::Signaled(_) => None,
        }
    }

    pub fn signal(self) -> Option<i32> {
        match self {
            Ending::Exited(_) => None,
            Ending::Signaled(s) => Some(s),
        }
    }
}

/// Plain name of a Unix signal for the summary.
pub fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".to_string(),
        2 => "SIGINT (Ctrl-C)".to_string(),
        3 => "SIGQUIT".to_string(),
        6 => "SIGABRT".to_string(),
        9 => "SIGKILL".to_string(),
        11 => "SIGSEGV".to_string(),
        15 => "SIGTERM".to_string(),
        n => format!("signal {n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ProcessMemory, Provenance};

    const MIB: u64 = 1024 * 1024;

    fn proc(pid: u32, ppid: u32, name: &str, mib: Option<u64>) -> Process {
        Process {
            pid,
            ppid: Some(ppid),
            name: name.to_string(),
            exe: None,
            user: None,
            memory: mib.map(|m| ProcessMemory {
                bytes: m * MIB,
                kind: MemoryKind::Footprint,
                provenance: Provenance::Measured,
            }),
            app_bundle: None,
        }
    }

    fn pids(members: &[&Process]) -> Vec<u32> {
        members.iter().map(|p| p.pid).collect()
    }

    #[test]
    fn tree_follows_children_and_grandchildren() {
        let ps = vec![
            proc(1, 0, "launchd", Some(10)),
            proc(50, 1, "zsh", Some(5)),
            proc(100, 50, "canfit", Some(8)),
            proc(200, 100, "llama-server", Some(4000)),
            proc(201, 200, "worker", Some(100)),
            proc(202, 201, "helper", Some(50)),
            proc(300, 50, "other", Some(999)),
        ];
        let mut tree = Tree::new(200, None);
        assert_eq!(pids(&tree.members(&ps, &[])), [200, 201, 202]);
        let (bytes, n, unreadable, kinds) = measure(&tree.members(&ps, &[]));
        assert_eq!((bytes, n, unreadable), (4150 * MIB, 3, 0));
        assert_eq!(kinds, [MemoryKind::Footprint]);
    }

    #[test]
    fn reparented_grandchildren_keep_counting() {
        let mut tree = Tree::new(200, None);
        let first = vec![
            proc(200, 100, "sh", Some(1)),
            proc(201, 200, "server", Some(500)),
        ];
        assert_eq!(pids(&tree.members(&first, &[])), [200, 201]);
        // The middle process exits and its child is adopted by launchd/init.
        let later = vec![
            proc(1, 0, "launchd", Some(10)),
            proc(201, 1, "server", Some(600)),
        ];
        assert_eq!(pids(&tree.members(&later, &[])), [201]);
        // A recycled pid under another name is not ours.
        let recycled = vec![proc(201, 1, "unrelated", Some(600))];
        assert!(tree.members(&recycled, &[]).is_empty());
    }

    #[test]
    fn adopted_orphans_and_job_members_count_but_never_canfit() {
        // Linux: canfit (pid 100) is the subreaper; an orphan it adopted belongs to the tree.
        let ps = vec![
            proc(100, 50, "canfit", Some(8)),
            proc(200, 100, "make", Some(10)),
            proc(400, 100, "daemonized", Some(70)),
            proc(500, 4, "job-member", Some(30)),
            proc(600, 1, "stranger", Some(30)),
        ];
        let mut tree = Tree::new(200, Some(100));
        assert_eq!(pids(&tree.members(&ps, &[500, 100])), [200, 400, 500]);
    }

    #[test]
    fn unreadable_members_are_counted_apart() {
        let ps = vec![proc(200, 1, "a", Some(10)), proc(201, 200, "b", None)];
        let mut tree = Tree::new(200, None);
        let (bytes, n, unreadable, _) = measure(&tree.members(&ps, &[]));
        assert_eq!((bytes, n, unreadable), (10 * MIB, 2, 1));
    }

    #[test]
    fn stats_track_peak_typical_and_pressure() {
        let mut s = Stats::default();
        for (i, mib) in [100, 300, 200, 300, 150].iter().enumerate() {
            s.record(
                Point {
                    offset_ms: i as u64 * 1000,
                    bytes: mib * MIB,
                    processes: i + 1,
                },
                0,
                &[MemoryKind::Pss],
            );
        }
        let peak = s.peak().unwrap();
        assert_eq!(
            (peak.bytes, peak.offset_ms),
            (300 * MIB, 1000),
            "first peak"
        );
        assert_eq!(s.typical_bytes(), 200 * MIB);
        assert_eq!(s.p95_bytes(), 300 * MIB);
        assert_eq!(s.max_processes(), 5);
        assert_eq!(s.memory_kind(), Some(MemoryKind::Pss));
        assert_eq!(Stats::default().peak(), None);
    }

    #[test]
    fn alerts_warn_once_per_level_and_once_when_calm() {
        use PressureLevel::*;
        let mut a = Alerts::default();
        assert_eq!(a.feed(Calm, 0), None);
        assert_eq!(a.feed(Strained, 1000), Some(Alert::Rose(Strained)));
        assert_eq!(a.feed(Strained, 2000), None);
        assert_eq!(a.feed(Critical, 3000), Some(Alert::Rose(Critical)));
        assert_eq!(a.feed(Strained, 4000), None, "falling back is not news");
        assert_eq!(a.feed(Calm, 5000), None);
        assert_eq!(a.feed(Critical, 6000), None, "same stretch, already warned");
        assert_eq!(a.feed(Calm, 7000), None);
        assert_eq!(a.feed(Calm, 7000 + CALM_GRACE_MS), Some(Alert::Calmed));
        assert_eq!(a.feed(Calm, 9 * CALM_GRACE_MS), None);
        assert_eq!(
            a.feed(Strained, 10 * CALM_GRACE_MS),
            Some(Alert::Rose(Strained))
        );
    }

    #[test]
    fn names_come_from_the_flag_or_the_program() {
        assert_eq!(run_name(Some("my server"), "llama-server"), "my server");
        assert_eq!(
            run_name(Some("  "), "/opt/bin/llama-server"),
            "llama-server"
        );
        assert_eq!(run_name(None, r"C:\llama\llama-server.exe"), "llama-server");
        assert_eq!(run_name(None, "ollama"), "ollama");
        assert_eq!(program_name("./run.cmd"), "run");
        assert_eq!(program_name(".exe"), ".exe");
    }

    #[test]
    fn endings_map_to_exit_codes() {
        assert_eq!(Ending::Exited(0).exit_code(), 0);
        assert_eq!(Ending::Exited(7).exit_code(), 7);
        assert_eq!(Ending::Signaled(2).exit_code(), 130);
        assert_eq!(Ending::Signaled(9).exit_code(), 137);
        assert_eq!(Ending::Exited(-1_073_741_510).exit_code(), -1_073_741_510);
        assert_eq!(Ending::Signaled(15).signal(), Some(15));
        assert_eq!(signal_name(2), "SIGINT (Ctrl-C)");
    }
}
