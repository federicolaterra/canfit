//! Linux readings from `/proc` and `/sys`, conversions to the shared model, and the Linux app
//! grouping hint (ADR-017).
//!
//! Only `std::fs`: no system calls of its own, so this module compiles and is tested on every OS
//! against fixture trees under `tests/fixtures/linux/` that mirror `/proc`, `/sys` and `/etc`.
//! The live backend in `linux.rs` adds CPU usage, the page size and process control. Definitions
//! match `free -b` and `htop`:
//!
//! - Available = `MemAvailable` (free memory plus caches the kernel can drop without swapping)
//! - Used = total − available (`free`'s "used" is total − free − buff/cache: it hides the
//!   shared memory and unreclaimable slab that `MemAvailable` already accounts for)
//! - Cached files = `Buffers + Cached + SReclaimable` (`free`'s "buff/cache")
//! - Compressed = zswap (`Zswap`/`Zswapped`, Linux 5.19+) plus zram devices (`mm_stat`)
//! - Per-process memory = PSS from `/proc/[pid]/smaps_rollup` (Linux 4.14+); `VmRSS` from
//!   `/proc/[pid]/status`, labelled `rss`, when the kernel refuses (other users' processes)
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use crate::core::{
    MemoryInfo, MemoryKind, Process, ProcessMemory, Provenance, StallInfo, StallLine, StallReading,
    SwapInfo,
};

/// `PF_KTHREAD` in `/proc/[pid]/stat` field 9: the task is a kernel thread.
pub const PF_KTHREAD: u64 = 0x0020_0000;
/// `kthreadd`, the parent of every kernel thread.
pub const KTHREADD_PID: u32 = 2;

// --- parsers ------------------------------------------------------------------------------------

/// `/proc/meminfo`, values in bytes (the file reports kB; counts such as `HugePages_Total` are
/// kept as they are).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meminfo(HashMap<String, u64>);

impl Meminfo {
    pub fn get(&self, key: &str) -> Option<u64> {
        self.0.get(key).copied()
    }
}

pub fn parse_meminfo(text: &str) -> Meminfo {
    let mut map = HashMap::new();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let Some(Ok(value)) = parts.next().map(str::parse::<u64>) else {
            continue;
        };
        let bytes = match parts.next() {
            Some(unit) if unit.eq_ignore_ascii_case("kB") => value.saturating_mul(1024),
            _ => value,
        };
        map.insert(key.trim().to_string(), bytes);
    }
    Meminfo(map)
}

fn parse_stall_line(rest: &str) -> Option<StallLine> {
    let mut line = StallLine {
        avg10: f64::NAN,
        avg60: f64::NAN,
        avg300: f64::NAN,
        total_us: 0,
    };
    let mut total = None;
    for field in rest.split_whitespace() {
        let (key, value) = field.split_once('=')?;
        match key {
            "avg10" => line.avg10 = value.parse().ok()?,
            "avg60" => line.avg60 = value.parse().ok()?,
            "avg300" => line.avg300 = value.parse().ok()?,
            "total" => total = Some(value.parse().ok()?),
            _ => {}
        }
    }
    line.total_us = total?;
    let valid = |v: f64| v.is_finite() && (0.0..=100.0).contains(&v);
    (valid(line.avg10) && valid(line.avg60) && valid(line.avg300)).then_some(line)
}

/// One PSI file (`/proc/pressure/memory` or a cgroup's `memory.pressure`):
/// `some avg10=0.00 avg60=0.00 avg300=0.00 total=0` and, except for old CPU files, a `full` line.
pub fn parse_psi(text: &str) -> Option<StallReading> {
    let mut some = None;
    let mut full = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("some ") {
            some = parse_stall_line(rest);
        } else if let Some(rest) = line.strip_prefix("full ") {
            full = parse_stall_line(rest);
        }
    }
    Some(StallReading { some: some?, full })
}

/// A counter from `/proc/vmstat` (`pswpout 1234`).
pub fn parse_vmstat_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

/// `/proc/loadavg`: the 1, 5 and 15 minute load averages.
pub fn parse_loadavg(text: &str) -> Option<[f64; 3]> {
    let mut parts = text.split_whitespace().map(str::parse::<f64>);
    let mut next = || parts.next()?.ok().filter(|v| v.is_finite() && *v >= 0.0);
    Some([next()?, next()?, next()?])
}

/// The fields canfit uses from `/proc/[pid]/stat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStat {
    /// The kernel's short name (at most 15 bytes).
    pub comm: String,
    /// `R`, `S`, `D`, `Z` (zombie), `X` (dead), ...
    pub state: char,
    pub ppid: u32,
    pub flags: u64,
    /// Start time after boot, in clock ticks: with the pid it identifies one program run.
    pub start_ticks: u64,
}

impl ProcStat {
    pub fn is_kernel_thread(&self) -> bool {
        self.flags & PF_KTHREAD != 0
    }

    pub fn has_exited(&self) -> bool {
        matches!(self.state, 'Z' | 'X' | 'x')
    }
}

/// `/proc/[pid]/stat`. The name sits in parentheses and may itself contain spaces and `)`, so the
/// fields are split after the **last** `)`.
pub fn parse_stat(text: &str) -> Option<ProcStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let comm = text[open + 1..close].to_string();
    // Fields after the name, numbered from 3 (state) as in proc(5).
    let rest: Vec<&str> = text[close + 1..].split_whitespace().collect();
    let field = |n: usize| rest.get(n - 3).copied();
    Some(ProcStat {
        comm,
        state: field(3)?.chars().next()?,
        ppid: field(4)?.parse().ok()?,
        flags: field(9)?.parse().ok()?,
        start_ticks: field(22)?.parse().ok()?,
    })
}

/// The fields canfit uses from `/proc/[pid]/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcStatus {
    /// Real user id.
    pub uid: Option<u32>,
    /// Resident set size in bytes; absent for kernel threads.
    pub rss_bytes: Option<u64>,
}

pub fn parse_status(text: &str) -> ProcStatus {
    let mut status = ProcStatus::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            status.uid = rest.split_whitespace().next().and_then(|v| v.parse().ok());
        } else if let Some(rest) = line.strip_prefix("VmRSS:") {
            status.rss_bytes = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|kb| kb * 1024);
        }
    }
    status
}

/// The `Pss:` line of `/proc/[pid]/smaps_rollup`, in bytes.
pub fn parse_pss(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("Pss:")?;
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    })
}

/// The process's cgroup path from `/proc/[pid]/cgroup`: the unified (v2) `0::` line, else the
/// systemd hierarchy of a cgroup v1 system (`1:name=systemd:/...`).
pub fn parse_cgroup(text: &str) -> Option<&str> {
    let mut v1 = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Some(path.trim()).filter(|p| !p.is_empty());
        }
        let mut parts = line.splitn(3, ':');
        let (_, controllers, path) = (parts.next(), parts.next(), parts.next());
        if controllers == Some("name=systemd") {
            v1 = path.map(str::trim).filter(|p| !p.is_empty());
        }
    }
    v1
}

/// Decodes systemd's `\xNN` escapes in a unit name (`foo\x2dbar.service`).
fn unescape_unit(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(at) = rest.find("\\x") {
        out.push_str(&rest[..at]);
        let hex = rest.get(at + 2..at + 4);
        match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(byte) if byte.is_ascii() => {
                out.push(byte as char);
                rest = &rest[at + 4..];
            }
            _ => {
                out.push_str("\\x");
                rest = &rest[at + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The application a cgroup path stands for, as an app hint (ADR-017):
///
/// - a Flatpak app (`app-flatpak-<id>-<n>.scope`) → `/var/lib/flatpak/app/<id>`;
/// - a Snap (`snap.<name>.<app>-<id>.scope`) → `/snap/<name>`;
/// - a systemd service (`ollama.service`) → the cgroup path with the unit name decoded, except
///   the user manager (`user@1000.service`), desktop app launches (`app-*`, grouped by process
///   tree because a terminal's shells may live in the terminal's unit), D-Bus activations and the
///   desktop session's own services (`session.slice`: `gnome-shell`, `pipewire`), which keep
///   their process names.
///
/// Everything else (session and app scopes, `init.scope`) has no hint.
pub fn cgroup_app(path: &str) -> Option<String> {
    let (parent, leaf) = path.rsplit_once('/')?;
    if let Some(rest) = leaf
        .strip_prefix("app-flatpak-")
        .and_then(|r| r.strip_suffix(".scope"))
    {
        let id = match rest.rsplit_once('-') {
            Some((id, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => id,
            _ => rest,
        };
        return (!id.is_empty()).then(|| format!("/var/lib/flatpak/app/{id}"));
    }
    if let Some(rest) = leaf
        .strip_prefix("snap.")
        .and_then(|r| r.strip_suffix(".scope"))
    {
        let name = rest.split('.').next().filter(|n| !n.is_empty())?;
        return Some(format!("/snap/{name}"));
    }
    if leaf.ends_with(".service")
        && !parent.ends_with("/session.slice")
        && !leaf.starts_with("user@")
        && !leaf.starts_with("app-")
        && !leaf.starts_with("dbus-")
    {
        return Some(format!("{parent}/{}", unescape_unit(leaf)));
    }
    None
}

/// `/etc/passwd`: user names by uid.
pub fn parse_passwd(text: &str) -> HashMap<u32, String> {
    let mut users = HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(':');
        let (Some(name), _, Some(uid)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if let Ok(uid) = uid.parse::<u32>()
            && !name.is_empty()
        {
            users.entry(uid).or_insert_with(|| name.to_string());
        }
    }
    users
}

/// The user name for `uid`: from `/etc/passwd`, `root` for uid 0, else the number (users from
/// LDAP or other directories that canfit does not query).
pub fn user_name(users: &HashMap<u32, String>, uid: u32) -> String {
    match users.get(&uid) {
        Some(name) => name.clone(),
        None if uid == 0 => "root".to_string(),
        None => uid.to_string(),
    }
}

/// A zram device's `mm_stat`: `(orig_data_size, mem_used_total)` in bytes.
pub fn parse_zram_mm_stat(text: &str) -> Option<(u64, u64)> {
    let fields: Vec<u64> = text
        .split_whitespace()
        .take(3)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    (fields.len() == 3).then(|| (fields[0], fields[2]))
}

/// An executable path from the `/proc/[pid]/exe` link, without the ` (deleted)` mark the kernel
/// adds when the file was replaced (a package upgrade while the program runs).
pub fn clean_exe(link: &str) -> String {
    link.strip_suffix(" (deleted)").unwrap_or(link).to_string()
}

/// The process name canfit shows: the executable's file name; else the first argument's file
/// name, unless the program rewrote it into a status line (`postgres: checkpointer`); else the
/// kernel's short name.
pub fn process_name(exe: Option<&str>, cmdline: Option<&str>, comm: &str) -> String {
    let file_name = |path: &str| path.rsplit('/').next().unwrap_or(path).to_string();
    if let Some(name) = exe.map(file_name).filter(|n| !n.is_empty()) {
        return name;
    }
    let argv0 = cmdline
        .and_then(|c| c.split('\0').next())
        .filter(|a| !a.is_empty() && !a.contains([' ', ':']));
    match argv0.map(file_name).filter(|n| !n.is_empty()) {
        Some(name) => name,
        None => comm.to_string(),
    }
}

// --- conversions --------------------------------------------------------------------------------

/// Builds the shared memory model. `zram` is the sum of the zram devices' `(original, used)`.
pub fn memory_from_meminfo(m: &Meminfo, zram: Option<(u64, u64)>) -> Result<MemoryInfo, String> {
    let total = m
        .get("MemTotal")
        .filter(|&t| t > 0)
        .ok_or("MemTotal is missing from /proc/meminfo")?;
    let free = m.get("MemFree").unwrap_or(0).min(total);
    let buffers = m.get("Buffers").unwrap_or(0);
    let cached = m.get("Cached").unwrap_or(0);
    let reclaimable = m.get("SReclaimable").unwrap_or(0);
    let (available, provenance) = match m.get("MemAvailable") {
        Some(available) => (available, Provenance::Measured),
        // Kernels before 3.14: the kernel's own estimate is missing, so canfit makes a
        // conservative one (shared memory in the page cache cannot be dropped).
        None => (
            (free + buffers + cached + reclaimable).saturating_sub(m.get("Shmem").unwrap_or(0)),
            Provenance::Estimated,
        ),
    };
    let available = available.min(total);
    let zswap = m.get("Zswap").zip(m.get("Zswapped"));
    let compressed = match (zswap, zram) {
        (None, None) => None,
        (a, b) => {
            let (a_used, a_orig) = a.unwrap_or((0, 0));
            let (b_orig, b_used) = b.unwrap_or((0, 0));
            Some((a_used + b_used, a_orig + b_orig))
        }
    };
    Ok(MemoryInfo {
        total_bytes: total,
        available_bytes: available,
        used_bytes: total - available,
        free_bytes: free,
        wired_bytes: None,
        app_bytes: m.get("AnonPages"),
        cached_files_bytes: Some(buffers + cached + reclaimable),
        compressed_bytes: compressed.map(|(used, _)| used),
        compressed_original_bytes: compressed.map(|(_, orig)| orig),
        commit_bytes: None,
        commit_limit_bytes: None,
        provenance,
    })
}

/// Swap from `/proc/meminfo`; `swapped_out_pages` is `/proc/vmstat`'s `pswpout`.
pub fn swap_from_meminfo(m: &Meminfo, swapped_out_pages: Option<u64>, page_size: u64) -> SwapInfo {
    let total = m.get("SwapTotal").unwrap_or(0);
    let free = m.get("SwapFree").unwrap_or(0).min(total);
    SwapInfo {
        total_bytes: total,
        used_bytes: total - free,
        free_bytes: free,
        swapped_out_total_bytes: swapped_out_pages.map(|p| p.saturating_mul(page_size)),
        encrypted: None,
        provenance: Provenance::Measured,
    }
}

// --- app grouping -------------------------------------------------------------------------------

fn parent_dir(path: &str) -> Option<&str> {
    let cut = path.rfind('/')?;
    Some(&path[..cut]).filter(|d| !d.is_empty())
}

fn depth(dir: &str) -> usize {
    dir.split('/').filter(|c| !c.is_empty()).count()
}

/// Folders shared by many unrelated programs: a parent there never collects other programs.
fn is_shared_dir(dir: &str) -> bool {
    let last = dir.rsplit('/').next().unwrap_or(dir);
    depth(dir) <= 1
        || matches!(
            last,
            "bin" | "sbin" | "libexec" | "lib" | "lib64" | "lib32" | "share" | "games"
        )
}

/// Whether `child` is a helper of `parent` (ADR-017): started by it, after it, running the same
/// executable, or one from the parent's own install folder (`/opt/google/chrome`) or a subfolder
/// of it. Shared folders such as `/usr/bin` never collect children, so a model server started
/// from a shell stays its own app.
fn is_helper(parent: &Process, child: &Process, started: &HashMap<u32, u64>) -> bool {
    let (Some(parent_exe), Some(child_exe)) = (parent.exe.as_deref(), child.exe.as_deref()) else {
        return false;
    };
    match (started.get(&parent.pid), started.get(&child.pid)) {
        (Some(p), Some(c)) if p <= c => {}
        _ => return false,
    }
    if parent.pid == child.pid {
        return false;
    }
    if parent_exe == child_exe {
        return true;
    }
    let (Some(parent_dir), Some(child_dir)) = (parent_dir(parent_exe), parent_dir(child_exe))
    else {
        return false;
    };
    if is_shared_dir(parent_dir) {
        return false;
    }
    let deep_enough = depth(parent_dir) >= 3 || parent_dir.starts_with("/opt/");
    child_dir == parent_dir
        || (deep_enough
            && child_dir.len() > parent_dir.len() + 1
            && child_dir.starts_with(parent_dir)
            && child_dir.as_bytes()[parent_dir.len()] == b'/')
}

/// Sets each process's `app_bundle`: its cgroup hint ([`cgroup_app`]) when it has one, else the
/// executable of its app's root process, the furthest ancestor reachable through [`is_helper`]
/// links without crossing into a process that has a cgroup hint. A root that collects no helper
/// gives no hint, so lone processes are grouped by name (every `bash` is one row). `started`
/// holds each process's start time; `hints` the cgroup hints by pid.
pub fn assign_apps(
    processes: &mut [Process],
    started: &HashMap<u32, u64>,
    hints: &HashMap<u32, String>,
) {
    let index: HashMap<u32, usize> = processes
        .iter()
        .enumerate()
        .map(|(i, p)| (p.pid, i))
        .collect();
    let roots: Vec<Option<usize>> = (0..processes.len())
        .map(|i| {
            if hints.contains_key(&processes[i].pid) {
                return None;
            }
            processes[i].exe.as_ref()?;
            let mut current = i;
            // Bounded walk: a corrupt table cannot loop forever.
            for _ in 0..processes.len() {
                let parent = processes[current]
                    .ppid
                    .filter(|&ppid| ppid != 0)
                    .and_then(|ppid| index.get(&ppid).copied())
                    .filter(|&p| !hints.contains_key(&processes[p].pid));
                match parent {
                    Some(p) if is_helper(&processes[p], &processes[current], started) => {
                        current = p
                    }
                    _ => break,
                }
            }
            Some(current)
        })
        .collect();
    let mut members: HashMap<usize, usize> = HashMap::new();
    for root in roots.iter().flatten() {
        *members.entry(*root).or_default() += 1;
    }
    let hinted: Vec<Option<String>> = processes
        .iter()
        .zip(&roots)
        .map(|(p, root)| match (hints.get(&p.pid), root) {
            (Some(hint), _) => Some(hint.clone()),
            (None, Some(root)) if members[root] > 1 => processes[*root].exe.clone(),
            _ => None,
        })
        .collect();
    for (process, hint) in processes.iter_mut().zip(hinted) {
        process.app_bundle = hint;
    }
}

// --- reading ------------------------------------------------------------------------------------

/// Reads `/proc`, `/sys` and `/etc` under a root: `/` on a live system, a fixture folder in
/// tests. Fixture folders store each `/proc/[pid]/exe` link as a small text file holding the
/// target, so they check out the same way on every OS.
#[derive(Debug, Clone)]
pub struct ProcFs {
    root: PathBuf,
    links_as_files: bool,
}

/// What canfit remembers about one program run, so repeated scans skip the link and the command
/// line.
#[derive(Debug, Clone)]
pub struct Known {
    pub exe: Option<String>,
    pub name: String,
}

/// Everything a process scan produces.
#[derive(Debug, Default)]
pub struct Scan {
    pub processes: Vec<Process>,
    /// Program runs seen in this scan; becomes the next scan's cache.
    pub known: HashMap<(u32, u64), Known>,
}

impl ProcFs {
    pub fn system() -> ProcFs {
        ProcFs {
            root: PathBuf::from("/"),
            links_as_files: false,
        }
    }

    #[cfg(test)]
    pub fn fixture(dir: impl Into<PathBuf>) -> ProcFs {
        ProcFs {
            root: dir.into(),
            links_as_files: true,
        }
    }

    fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }

    /// Reads a text file; invalid UTF-8 (command lines) is replaced, never an error.
    pub fn read(&self, absolute: &str) -> io::Result<String> {
        std::fs::read(self.path(absolute)).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    pub fn read_link(&self, absolute: &str) -> io::Result<String> {
        let path = self.path(absolute);
        if self.links_as_files {
            return self.read(absolute).map(|t| t.trim_end().to_string());
        }
        std::fs::read_link(path).map(|p| p.to_string_lossy().into_owned())
    }

    pub fn meminfo(&self) -> io::Result<Meminfo> {
        self.read("/proc/meminfo").map(|t| parse_meminfo(&t))
    }

    pub fn swapped_out_pages(&self) -> Option<u64> {
        parse_vmstat_field(&self.read("/proc/vmstat").ok()?, "pswpout")
    }

    pub fn load_average(&self) -> Option<[f64; 3]> {
        parse_loadavg(&self.read("/proc/loadavg").ok()?)
    }

    pub fn users(&self) -> HashMap<u32, String> {
        self.read("/etc/passwd")
            .map(|t| parse_passwd(&t))
            .unwrap_or_default()
    }

    /// The zram devices' combined `(original, used)` bytes, when any exists.
    pub fn zram(&self) -> Option<(u64, u64)> {
        let entries = std::fs::read_dir(self.path("/sys/block")).ok()?;
        let mut total: Option<(u64, u64)> = None;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("zram") {
                continue;
            }
            if let Some((orig, used)) = self
                .read(&format!("/sys/block/{name}/mm_stat"))
                .ok()
                .and_then(|t| parse_zram_mm_stat(&t))
            {
                let (o, u) = total.unwrap_or((0, 0));
                total = Some((o + orig, u + used));
            }
        }
        total
    }

    /// Pressure-stall readings: `/proc/pressure/*` (Linux 4.20+), else the cgroup v2 root's
    /// `*.pressure` files. Missing files (older kernels, `CONFIG_PSI` off) and refused reads
    /// (`psi=0` on the kernel command line) give `None`.
    pub fn stall(&self) -> Option<StallInfo> {
        for (dir, suffix, source) in [
            ("/proc/pressure/", "", "/proc/pressure"),
            (
                "/sys/fs/cgroup/",
                ".pressure",
                "cgroup v2 root (/sys/fs/cgroup)",
            ),
        ] {
            let read = |resource: &str| {
                self.read(&format!("{dir}{resource}{suffix}"))
                    .ok()
                    .and_then(|t| parse_psi(&t))
            };
            let (memory, cpu, io) = (read("memory"), read("cpu"), read("io"));
            if memory.is_some() || cpu.is_some() || io.is_some() {
                return Some(StallInfo {
                    memory,
                    cpu,
                    io,
                    source: source.to_string(),
                    provenance: Provenance::Measured,
                });
            }
        }
        None
    }

    /// Process ids listed in `/proc`.
    pub fn pids(&self) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir(self.path("/proc")) else {
            return Vec::new();
        };
        let mut pids: Vec<u32> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        pids.sort_unstable();
        pids
    }

    pub fn stat(&self, pid: u32) -> Option<ProcStat> {
        parse_stat(&self.read(&format!("/proc/{pid}/stat")).ok()?)
    }

    /// The executable path, when the kernel lets canfit read the link (own processes).
    pub fn exe(&self, pid: u32) -> Option<String> {
        self.read_link(&format!("/proc/{pid}/exe"))
            .ok()
            .filter(|l| !l.is_empty())
            .map(|l| clean_exe(&l))
    }

    /// The executable path and display name of a program run.
    pub fn identify(&self, pid: u32, stat: &ProcStat) -> Known {
        let exe = self.exe(pid);
        let cmdline = self.read(&format!("/proc/{pid}/cmdline")).ok();
        Known {
            name: process_name(exe.as_deref(), cmdline.as_deref(), &stat.comm),
            exe,
        }
    }

    /// Proportional set size when readable, else the resident size (labelled).
    pub fn process_memory(&self, pid: u32, status: &ProcStatus) -> Option<ProcessMemory> {
        if let Some(pss) = self
            .read(&format!("/proc/{pid}/smaps_rollup"))
            .ok()
            .and_then(|t| parse_pss(&t))
        {
            return Some(ProcessMemory {
                bytes: pss,
                kind: MemoryKind::Pss,
                provenance: Provenance::Measured,
            });
        }
        status.rss_bytes.map(|bytes| ProcessMemory {
            bytes,
            kind: MemoryKind::Rss,
            provenance: Provenance::Measured,
        })
    }

    /// Reads every user-space process, groups them into apps and returns the new cache. Kernel
    /// threads and exited processes are left out: they hold no memory of their own and can never
    /// be closed.
    pub fn scan(&self, users: &HashMap<u32, String>, cache: &HashMap<(u32, u64), Known>) -> Scan {
        let pids = self.pids();
        let mut scan = Scan {
            processes: Vec::with_capacity(pids.len()),
            known: HashMap::with_capacity(pids.len()),
        };
        let mut started = HashMap::with_capacity(pids.len());
        let mut hints = HashMap::new();
        for pid in pids {
            // A process that exits during the scan simply disappears from it.
            let Some(stat) = self.stat(pid) else {
                continue;
            };
            if pid == KTHREADD_PID || stat.is_kernel_thread() || stat.has_exited() {
                continue;
            }
            let status = self
                .read(&format!("/proc/{pid}/status"))
                .map(|t| parse_status(&t))
                .unwrap_or_default();
            let key = (pid, stat.start_ticks);
            let known = match cache.get(&key) {
                Some(known) => known.clone(),
                None => self.identify(pid, &stat),
            };
            if let Some(hint) = self
                .read(&format!("/proc/{pid}/cgroup"))
                .ok()
                .as_deref()
                .and_then(parse_cgroup)
                .and_then(cgroup_app)
            {
                hints.insert(pid, hint);
            }
            started.insert(pid, stat.start_ticks);
            scan.processes.push(Process {
                pid,
                ppid: Some(stat.ppid),
                name: known.name.clone(),
                exe: known.exe.clone(),
                user: status.uid.map(|uid| user_name(users, uid)),
                memory: self.process_memory(pid, &status),
                app_bundle: None,
            });
            scan.known.insert(key, known);
        }
        assign_apps(&mut scan.processes, &started, &hints);
        scan
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::advisor::{self, SafetyClass};
    use crate::core::sampler::{RawSample, Sampler};
    use crate::core::{
        CpuInfo, GroupKind, PlatformInfo, PressureLevel, PressureSignal, Snapshot,
        group::group_processes,
    };
    use std::time::{Duration, Instant};

    const GIB: u64 = 1024 * 1024 * 1024;
    const KIB: u64 = 1024;
    /// canfit's pid in the `normal` fixture: `canfit` started from bash in GNOME Terminal.
    pub const FIXTURE_CANFIT_PID: u32 = 4100;

    pub fn fixture(name: &str) -> ProcFs {
        ProcFs::fixture(format!(
            "{}/tests/fixtures/linux/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
    }

    /// Reads a fixture tree the way the live backend reads `/`, with a fixed CPU.
    pub fn raw_sample(fs: &ProcFs, taken_at: Instant) -> RawSample {
        let meminfo = fs.meminfo().expect("fixture meminfo");
        let scan = fs.scan(&fs.users(), &HashMap::new());
        RawSample {
            taken_at,
            timestamp_unix_ms: 0,
            os_name: "Linux",
            platform: PlatformInfo {
                os: "linux".to_string(),
                os_version: None,
                arch: "x86_64".to_string(),
                unified_memory: false,
            },
            memory: memory_from_meminfo(&meminfo, fs.zram()).expect("memory"),
            swap: swap_from_meminfo(&meminfo, fs.swapped_out_pages(), 4096),
            cpu: CpuInfo {
                logical_cores: 8,
                physical_cores: Some(4),
                brand: None,
                usage_percent: None,
                load_average: fs.load_average(),
                provenance: Provenance::Measured,
            },
            os_pressure: None,
            hard_faults_per_sec: None,
            stall: fs.stall(),
            gpu_memory_limit: None,
            processes: Some(scan.processes),
        }
    }

    pub fn snapshot(name: &str) -> Snapshot {
        let fs = fixture(name);
        let mut sampler = Sampler::new(4, Duration::from_secs(10));
        sampler.record(raw_sample(&fs, Instant::now()))
    }

    // --- parsers --------------------------------------------------------------------------------

    #[test]
    fn meminfo_is_parsed_in_bytes() {
        let m = parse_meminfo(
            "MemTotal:       16303428 kB\nMemAvailable:    9000000 kB\nHugePages_Total:       0\nbroken line\nX: y kB\n",
        );
        assert_eq!(m.get("MemTotal"), Some(16_303_428 * KIB));
        assert_eq!(m.get("HugePages_Total"), Some(0));
        assert_eq!(m.get("X"), None);
    }

    #[test]
    fn psi_files_are_parsed_with_and_without_full() {
        let r = parse_psi(
            "some avg10=1.53 avg60=0.87 avg300=0.25 total=4893412\nfull avg10=0.40 avg60=0.10 avg300=0.02 total=1000\n",
        )
        .unwrap();
        assert_eq!(r.some.avg10, 1.53);
        assert_eq!(r.some.total_us, 4_893_412);
        assert_eq!(r.full.unwrap().avg10, 0.40);
        // CPU before Linux 5.13 has no `full` line.
        let cpu = parse_psi("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").unwrap();
        assert!(cpu.full.is_none());
        assert!(parse_psi("").is_none());
        assert!(parse_psi("some avg10=x avg60=0 avg300=0 total=0").is_none());
        assert!(parse_psi("some avg10=150.0 avg60=0 avg300=0 total=0").is_none());
    }

    #[test]
    fn stat_names_with_spaces_and_parentheses_are_handled() {
        let text = "4242 (Web Content (x)) S 4100 4242 4242 0 -1 4194560 100 0 0 0 5 2 0 0 20 0 30 0 123456 1000000 2000 18446744073709551615";
        let s = parse_stat(text).unwrap();
        assert_eq!(s.comm, "Web Content (x)");
        assert_eq!(s.state, 'S');
        assert_eq!(s.ppid, 4100);
        assert_eq!(s.flags, 4_194_560);
        assert_eq!(s.start_ticks, 123_456);
        assert!(!s.is_kernel_thread());
        let kthread =
            "77 (kworker/0:1-events) I 2 0 0 0 -1 69238880 0 0 0 0 0 0 0 0 20 0 1 0 50 0 0";
        assert!(parse_stat(kthread).unwrap().is_kernel_thread());
        assert!(parse_stat("1 (init").is_none());
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn status_cgroup_passwd_and_small_files_parse() {
        let s = parse_status("Name:\tbash\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t    5120 kB\n");
        assert_eq!(s.uid, Some(1000));
        assert_eq!(s.rss_bytes, Some(5120 * KIB));
        assert_eq!(
            parse_status("Name:\tkthreadd\nUid:\t0\t0\t0\t0\n").rss_bytes,
            None
        );
        assert_eq!(
            parse_pss("55d0-56 ---p 0 00:00 0 [rollup]\nRss: 10 kB\nPss: 7 kB\n"),
            Some(7 * KIB)
        );
        assert_eq!(
            parse_cgroup("0::/system.slice/ollama.service\n"),
            Some("/system.slice/ollama.service")
        );
        assert_eq!(
            parse_cgroup("12:memory:/x\n1:name=systemd:/system.slice/sshd.service\n"),
            Some("/system.slice/sshd.service")
        );
        assert_eq!(parse_cgroup("0::/\n").filter(|p| *p != "/"), None);
        let users = parse_passwd(
            "# comment\nroot:x:0:0::/root:/bin/sh\nfede:x:1000:1000::/home/fede:/bin/bash\nbad\n",
        );
        assert_eq!(user_name(&users, 1000), "fede");
        assert_eq!(user_name(&HashMap::new(), 0), "root");
        assert_eq!(user_name(&users, 1234), "1234");
        assert_eq!(
            parse_loadavg("0.52 0.58 0.59 1/389 12345\n"),
            Some([0.52, 0.58, 0.59])
        );
        assert_eq!(
            parse_vmstat_field("pswpin 3\npswpout 42\n", "pswpout"),
            Some(42)
        );
        assert_eq!(
            parse_zram_mm_stat("4096 1024 2048 0 2048 0 0 0 0\n"),
            Some((4096, 2048))
        );
        assert_eq!(clean_exe("/usr/bin/foo (deleted)"), "/usr/bin/foo");
    }

    #[test]
    fn process_names_prefer_the_executable() {
        assert_eq!(
            process_name(Some("/opt/google/chrome/chrome"), None, "chrome"),
            "chrome"
        );
        assert_eq!(
            process_name(
                None,
                Some("/usr/lib/postgresql/16/bin/postgres\0-D\0/x"),
                "postgres"
            ),
            "postgres"
        );
        assert_eq!(
            process_name(None, Some("postgres: checkpointer \0"), "postgres"),
            "postgres"
        );
        assert_eq!(process_name(None, Some(""), "kworker/0:1"), "kworker/0:1");
    }

    #[test]
    fn cgroup_paths_become_app_hints() {
        let user = "/user.slice/user-1000.slice/user@1000.service";
        for (path, hint) in [
            (
                "/system.slice/ollama.service",
                Some("/system.slice/ollama.service"),
            ),
            (
                "/system.slice/system-getty.slice/getty@tty1.service",
                Some("/system.slice/system-getty.slice/getty@tty1.service"),
            ),
            (
                "/system.slice/foo\\x2dbar.service",
                Some("/system.slice/foo-bar.service"),
            ),
            (
                &format!("{user}/app.slice/app-flatpak-org.mozilla.firefox-12345.scope"),
                Some("/var/lib/flatpak/app/org.mozilla.firefox"),
            ),
            (
                &format!("{user}/app.slice/snap.firefox.firefox-1a2b.scope"),
                Some("/snap/firefox"),
            ),
            (&format!("{user}/init.scope"), None),
            (
                &format!("{user}/session.slice/org.gnome.Shell@wayland.service"),
                None,
            ),
            ("/user.slice/user-1000.slice/user@1000.service", None),
            (
                &format!("{user}/app.slice/app-gnome-google\\x2dchrome-4321.scope"),
                None,
            ),
            (
                &format!("{user}/app.slice/app-org.kde.konsole@a1b2.service"),
                None,
            ),
            (
                &format!("{user}/app.slice/dbus-:1.2-org.gnome.Shell.Screencast@0.service"),
                None,
            ),
            ("/user.slice/user-1000.slice/session-2.scope", None),
            ("/init.scope", None),
            ("/", None),
        ] {
            assert_eq!(cgroup_app(path).as_deref(), hint, "{path}");
        }
    }

    #[test]
    fn meminfo_converts_like_free() {
        let m = parse_meminfo(
            "MemTotal: 16000000 kB\nMemFree: 2000000 kB\nMemAvailable: 9000000 kB\nBuffers: 100000 kB\nCached: 6000000 kB\nSReclaimable: 400000 kB\nShmem: 500000 kB\nAnonPages: 5000000 kB\nSwapTotal: 4000000 kB\nSwapFree: 3000000 kB\n",
        );
        let mem = memory_from_meminfo(&m, None).unwrap();
        assert_eq!(mem.available_bytes, 9_000_000 * KIB);
        assert_eq!(mem.used_bytes, 7_000_000 * KIB);
        assert_eq!(mem.free_bytes, 2_000_000 * KIB);
        assert_eq!(mem.cached_files_bytes, Some(6_500_000 * KIB));
        assert_eq!(mem.app_bytes, Some(5_000_000 * KIB));
        assert_eq!(mem.compressed_bytes, None);
        assert_eq!(mem.provenance, Provenance::Measured);
        let swap = swap_from_meminfo(&m, Some(10), 4096);
        assert_eq!(swap.used_bytes, 1_000_000 * KIB);
        assert_eq!(swap.swapped_out_total_bytes, Some(40960));

        // An old kernel without MemAvailable: a labelled, conservative estimate.
        let old = parse_meminfo(
            "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 10 kB\nCached: 300 kB\nSReclaimable: 40 kB\nShmem: 50 kB\n",
        );
        let mem = memory_from_meminfo(&old, Some((800 * KIB, 200 * KIB))).unwrap();
        assert_eq!(mem.available_bytes, 400 * KIB);
        assert_eq!(mem.provenance, Provenance::Estimated);
        assert_eq!(mem.compressed_bytes, Some(200 * KIB));
        assert_eq!(mem.compressed_original_bytes, Some(800 * KIB));
        assert!(memory_from_meminfo(&parse_meminfo(""), None).is_err());
    }

    // --- grouping -------------------------------------------------------------------------------

    fn proc(pid: u32, ppid: u32, exe: Option<&str>) -> Process {
        Process {
            pid,
            ppid: Some(ppid),
            name: exe.map_or("x".to_string(), |e| process_name(Some(e), None, "x")),
            exe: exe.map(str::to_string),
            user: Some("fede".to_string()),
            memory: Some(ProcessMemory {
                bytes: 100,
                kind: MemoryKind::Pss,
                provenance: Provenance::Measured,
            }),
            app_bundle: None,
        }
    }

    #[test]
    fn helpers_fold_into_their_app_but_shell_children_do_not() {
        let mut processes = vec![
            proc(10, 1, Some("/usr/libexec/gnome-terminal-server")),
            proc(11, 10, Some("/usr/bin/bash")),
            proc(12, 11, Some("/home/fede/llama.cpp/build/bin/llama-server")),
            proc(13, 11, Some("/usr/bin/python3")),
            proc(14, 13, Some("/usr/bin/python3")),
            proc(20, 1, Some("/opt/google/chrome/chrome")),
            proc(21, 20, Some("/opt/google/chrome/chrome")),
            proc(22, 20, Some("/opt/google/chrome/chrome_crashpad_handler")),
            proc(23, 21, Some("/opt/google/chrome/nacl/nacl_helper")),
            proc(30, 1, Some("/opt/foo/foo")),
            proc(31, 30, Some("/opt/foo/helper")),
            proc(40, 1, Some("/usr/local/bin/ollama")),
            proc(
                41,
                40,
                Some("/tmp/ollama123/runners/cuda/ollama_llama_server"),
            ),
            proc(50, 1, None),
        ];
        // The child started before the parent: a reused pid, not a helper.
        processes.push(proc(32, 30, Some("/opt/foo/other")));
        let mut started: HashMap<u32, u64> = processes.iter().map(|p| (p.pid, 100)).collect();
        started.insert(32, 50);
        let mut hints = HashMap::new();
        hints.insert(40, "/system.slice/ollama.service".to_string());
        hints.insert(41, "/system.slice/ollama.service".to_string());
        assign_apps(&mut processes, &started, &hints);
        let hint = |pid: u32| {
            processes
                .iter()
                .find(|p| p.pid == pid)
                .unwrap()
                .app_bundle
                .clone()
        };
        // Lone processes get no hint and are grouped by name.
        assert_eq!(hint(11), None);
        assert_eq!(hint(12), None);
        assert_eq!(hint(13).as_deref(), Some("/usr/bin/python3"));
        assert_eq!(hint(14).as_deref(), Some("/usr/bin/python3"));
        for pid in [20, 21, 22, 23] {
            assert_eq!(
                hint(pid).as_deref(),
                Some("/opt/google/chrome/chrome"),
                "{pid}"
            );
        }
        assert_eq!(hint(31).as_deref(), Some("/opt/foo/foo"));
        assert_eq!(hint(32), None);
        assert_eq!(hint(41).as_deref(), Some("/system.slice/ollama.service"));
        assert_eq!(hint(50), None);

        let groups = group_processes(&processes);
        let chrome = groups.iter().find(|g| g.name == "chrome").unwrap();
        assert_eq!(chrome.process_count, 4);
        assert_eq!(chrome.kind, GroupKind::AppBundle);
        let ollama = groups.iter().find(|g| g.name == "ollama").unwrap();
        assert_eq!(ollama.process_count, 2);
    }

    // --- fixtures -------------------------------------------------------------------------------

    #[test]
    fn normal_fixture_reads_like_free_and_psi() {
        let s = snapshot("normal");
        assert_eq!(s.memory.total_bytes, 16_303_428 * KIB);
        assert_eq!(s.memory.available_bytes, 10_841_604 * KIB);
        assert_eq!(s.memory.used_bytes, (16_303_428 - 10_841_604) * KIB);
        assert_eq!(s.swap.total_bytes, 8 * GIB);
        let stall = s.stall.as_ref().expect("PSI");
        assert_eq!(stall.source, "/proc/pressure");
        assert_eq!(stall.memory.unwrap().some.avg10, 0.0);
        assert!(stall.cpu.unwrap().full.is_none());
        assert_eq!(s.cpu.load_average, Some([0.52, 0.58, 0.59]));
        assert_eq!(s.pressure.level, PressureLevel::Calm);
        assert_eq!(s.pressure.main_reason.signal, PressureSignal::None);
        assert!(
            s.pressure
                .signals
                .iter()
                .any(|sig| sig.signal == PressureSignal::MemoryStall)
        );
        // zram swap: compressed memory comes from mm_stat.
        assert_eq!(s.memory.compressed_bytes, Some(52_428_800));
    }

    #[test]
    fn normal_fixture_groups_apps_and_skips_kernel_threads() {
        let s = snapshot("normal");
        let pids: Vec<u32> = s.processes.iter().map(|p| p.pid).collect();
        assert!(!pids.contains(&2), "kthreadd listed");
        assert!(!pids.contains(&77), "kernel worker listed");
        assert!(!pids.contains(&4999), "zombie listed");
        let group = |name: &str| {
            s.app_groups
                .iter()
                .find(|g| g.name == name)
                .unwrap_or_else(|| panic!("no group {name}: {:?}", s.app_groups))
        };
        // Chrome with its zygote, renderers and crashpad = one row.
        let chrome = group("chrome");
        assert_eq!(chrome.process_count, 5);
        assert_eq!(chrome.memory_kinds, [MemoryKind::Pss]);
        // The Ollama service: the server and its runner (another user: resident size).
        let ollama = group("ollama");
        assert_eq!(ollama.process_count, 2);
        assert!(ollama.memory_kinds.contains(&MemoryKind::Rss));
        // Firefox from Flatpak, grouped by its app id.
        assert_eq!(group("org.mozilla.firefox").process_count, 3);
        // A model server started from the terminal stays its own app.
        assert_eq!(group("llama-server").process_count, 1);
        assert_eq!(s.app_groups[0].name, "llama-server");
        assert_eq!(s.process_memory_kind(), MemoryKind::Pss);
        let own = s
            .processes
            .iter()
            .find(|p| p.pid == FIXTURE_CANFIT_PID)
            .unwrap();
        assert_eq!(own.user.as_deref(), Some("fede"));
        assert_eq!(own.name, "canfit");
    }

    #[test]
    fn normal_fixture_never_class_covers_the_session() {
        let s = snapshot("normal");
        let advice = advisor::advise(&s, FIXTURE_CANFIT_PID, Vec::new());
        let class = |name: &str| {
            advice
                .groups
                .iter()
                .find(|g| g.group.name == name)
                .unwrap_or_else(|| panic!("no group {name}"))
                .class
        };
        for name in [
            "systemd",
            "gnome-shell",
            "Xwayland",
            "gnome-terminal-server",
            "bash",
            "canfit",
            "sshd",
            "ollama",
            "pipewire",
            "dbus-broker",
        ] {
            assert_eq!(class(name), SafetyClass::Never, "{name}");
        }
        assert_eq!(class("chrome"), SafetyClass::Safe);
        assert_eq!(class("org.mozilla.firefox"), SafetyClass::Safe);
        assert_eq!(class("code"), SafetyClass::Caution);
        assert_eq!(class("llama-server"), SafetyClass::Caution);
        let names: Vec<&str> = advice.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.first(), Some(&"chrome"), "{names:?}");
        assert!(!names.contains(&"bash"));
    }

    #[test]
    fn missing_psi_falls_back_to_the_other_signals() {
        let s = snapshot("missing_psi");
        assert!(s.stall.is_none());
        assert!(
            s.pressure
                .signals
                .iter()
                .all(|sig| sig.signal != PressureSignal::MemoryStall)
        );
        assert_eq!(s.pressure.level, PressureLevel::Calm);
        assert!(s.memory.compressed_bytes.is_none());
    }

    #[test]
    fn cgroup_root_pressure_is_used_when_proc_pressure_is_missing() {
        let s = snapshot("cgroup_psi");
        let stall = s.stall.expect("cgroup PSI");
        assert!(stall.source.starts_with("cgroup v2 root"));
        assert_eq!(stall.memory.unwrap().some.avg10, 12.5);
    }

    #[test]
    fn unreadable_smaps_falls_back_to_labelled_resident_size() {
        let s = snapshot("missing_smaps");
        assert!(!s.processes.is_empty());
        for p in &s.processes {
            let m = p.memory.as_ref().expect("RSS");
            assert_eq!(m.kind, MemoryKind::Rss, "{}", p.name);
            assert_eq!(m.provenance, Provenance::Measured);
        }
        assert_eq!(s.process_memory_kind(), MemoryKind::Rss);
    }

    #[test]
    fn high_pressure_fixture_is_critical_from_memory_stalls() {
        let s = snapshot("high_pressure");
        assert_eq!(s.pressure.level, PressureLevel::Critical);
        assert_eq!(s.pressure.main_reason.signal, PressureSignal::MemoryStall);
        assert!(
            s.pressure
                .main_reason
                .message
                .contains("stalled waiting for memory"),
            "{}",
            s.pressure.main_reason.message
        );
        // The available-memory signal agrees: under 5 % is left.
        assert!(s.memory.available_bytes * 20 < s.memory.total_bytes);
    }

    #[test]
    fn swap_growth_is_measured_from_vmstat() {
        let fs = fixture("high_pressure");
        let start = Instant::now();
        let mut sampler = Sampler::new(4, Duration::from_secs(10));
        let first = raw_sample(&fs, start);
        let mut second = raw_sample(&fs, start + Duration::from_secs(2));
        second.swap.used_bytes += 64 * 1024 * 1024;
        second.swap.swapped_out_total_bytes =
            Some(first.swap.swapped_out_total_bytes.unwrap() + 64 * 1024 * 1024);
        sampler.record(first);
        let s = sampler.record(second);
        assert!(s.swap_trend.unwrap().growing);
    }
}
