//! Shared data model used by every other module.
//!
//! The types here are OS-neutral (ADR-004): platform-specific detail, such as macOS compressed
//! memory, appears only as optional fields with generic names. Every value that is not a plain
//! kernel counter carries its provenance: **measured**, **read from config** or **estimated**.
//!
//! Note: this module shadows the built-in `core` crate inside canfit. Refer to it as
//! `crate::core` and to the language crate as `::core`.

pub mod group;
pub mod pressure;
pub mod sampler;

use serde::Serialize;

/// Where a value comes from. Every number canfit shows is labelled with one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Read from the operating system at the time of the snapshot.
    Measured,
    /// Read from a setting (a sysctl the user set, an app's configuration file, ...).
    ReadFromConfig,
    /// Computed from a heuristic; conservative, never presented as a measurement.
    Estimated,
}

impl Provenance {
    pub fn label(self) -> &'static str {
        match self {
            Provenance::Measured => "measured",
            Provenance::ReadFromConfig => "read from config",
            Provenance::Estimated => "estimated",
        }
    }
}

/// One complete, explained picture of the system at a point in time.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// Milliseconds since the Unix epoch when the sample was taken.
    pub timestamp_unix_ms: u64,
    pub platform: PlatformInfo,
    pub memory: MemoryInfo,
    pub swap: SwapInfo,
    pub swap_trend: Option<SwapTrend>,
    pub cpu: CpuInfo,
    /// The operating system's own memory-pressure verdict, when it has one.
    pub os_pressure: Option<OsPressureLevel>,
    /// Hard page faults per second (pages read back from disk), when the OS counts them
    /// (Windows). Measured over the time since the previous sample.
    pub hard_faults_per_sec: Option<f64>,
    /// Share of recent time programs spent waiting for memory, CPU and I/O (Linux PSI), when the
    /// kernel reports it.
    pub stall: Option<StallInfo>,
    pub pressure: Pressure,
    /// Memory the GPU may use, on systems where that is a share of system memory.
    pub gpu_memory_limit: Option<GpuMemoryLimit>,
    /// Application groups, largest memory first.
    pub app_groups: Vec<AppGroup>,
    pub processes: Vec<Process>,
}

impl Snapshot {
    /// How app memory was measured: the kind most processes report, or this OS's preferred kind
    /// when no process memory was readable. Labels the app list ("physical footprint" on macOS,
    /// "private working set" on Windows).
    pub fn process_memory_kind(&self) -> MemoryKind {
        let mut counts: Vec<(MemoryKind, usize)> = Vec::new();
        for kind in self
            .processes
            .iter()
            .filter_map(|p| p.memory.as_ref().map(|m| m.kind))
        {
            match counts.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, n)) => *n += 1,
                None => counts.push((kind, 1)),
            }
        }
        counts.into_iter().max_by_key(|&(_, n)| n).map_or_else(
            || match self.platform.os.as_str() {
                "windows" => MemoryKind::PrivateWorkingSet,
                "linux" => MemoryKind::Pss,
                _ => MemoryKind::Footprint,
            },
            |(kind, _)| kind,
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlatformInfo {
    /// `macos`, `windows` or `linux`.
    pub os: String,
    pub os_version: Option<String>,
    /// CPU architecture of the machine, e.g. `aarch64` or `x86_64`.
    pub arch: String,
    /// True on Apple Silicon, where CPU and GPU share one pool of memory.
    pub unified_memory: bool,
}

/// System memory. All sizes are in bytes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    /// Memory that can be given to a new program without swapping: free memory plus caches the
    /// system drops on its own. Cache is never counted as "used".
    pub available_bytes: u64,
    /// Memory in use by programs and the system (`total - available`).
    pub used_bytes: u64,
    /// Memory not holding anything at all.
    pub free_bytes: u64,
    /// Memory that cannot be paged out (kernel, drivers, GPU allocations on unified memory).
    pub wired_bytes: Option<u64>,
    /// Anonymous memory of applications (Activity Monitor's "App Memory").
    pub app_bytes: Option<u64>,
    /// Reclaimable file cache (Activity Monitor's "Cached Files").
    pub cached_files_bytes: Option<u64>,
    /// Physical memory occupied by the memory compressor.
    pub compressed_bytes: Option<u64>,
    /// How much data the compressor holds before compression.
    pub compressed_original_bytes: Option<u64>,
    /// Memory the system has promised to programs (Windows commit charge): RAM plus page file
    /// that is reserved, whether or not it is touched yet.
    pub commit_bytes: Option<u64>,
    /// The most the system can promise before allocations fail (Windows commit limit: RAM plus
    /// the current page-file size).
    pub commit_limit_bytes: Option<u64>,
    pub provenance: Provenance,
}

/// Swap space. All sizes are in bytes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwapInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    /// Total data written to swap since boot (a monotonic counter), when the OS exposes it.
    pub swapped_out_total_bytes: Option<u64>,
    pub encrypted: Option<bool>,
    pub provenance: Provenance,
}

/// Whether swap is growing over the sampler's window. Swap that is merely in use is harmless.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwapTrend {
    pub growing: bool,
    /// Average change of used swap over the window, in bytes per second (negative when shrinking).
    pub used_change_bytes_per_sec: f64,
    /// Average rate at which data was written to swap, in bytes per second, when known.
    pub swap_out_bytes_per_sec: Option<f64>,
    pub window_secs: f64,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CpuInfo {
    pub logical_cores: usize,
    pub physical_cores: Option<usize>,
    pub brand: Option<String>,
    /// Busy share of all cores, 0–100. `None` until two samples have been taken.
    pub usage_percent: Option<f32>,
    /// 1, 5 and 15 minute load averages, when the OS has them.
    pub load_average: Option<[f64; 3]>,
    pub provenance: Provenance,
}

/// The operating system's own memory-pressure level (macOS `kern.memorystatus_vm_pressure_level`;
/// Windows low-memory resource notification). Linux has no such verdict: its PSI readings are
/// a separate signal ([`StallInfo`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OsPressureLevel {
    Normal,
    Warning,
    Critical,
}

/// One line of a Linux pressure-stall (PSI) file: the share of wall time, in percent, during
/// which tasks waited for the resource, averaged over 10, 60 and 300 seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StallLine {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    /// Total stall time since boot, in microseconds.
    pub total_us: u64,
}

/// One PSI resource. `some`: at least one task waited; `full`: every non-idle task waited at
/// once (the machine did no useful work). CPU has no meaningful `full` line before Linux 5.13.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StallReading {
    pub some: StallLine,
    pub full: Option<StallLine>,
}

/// Pressure-stall information (Linux PSI, kernel 4.20 or later).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StallInfo {
    pub memory: Option<StallReading>,
    pub cpu: Option<StallReading>,
    pub io: Option<StallReading>,
    /// Where the readings came from, e.g. `/proc/pressure` or the cgroup v2 root.
    pub source: String,
    pub provenance: Provenance,
}

/// The GPU-usable share of system memory on unified-memory machines.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GpuMemoryLimit {
    pub bytes: u64,
    /// `read_from_config` when the user set the limit, `estimated` when canfit applies the
    /// platform default.
    pub provenance: Provenance,
    /// Where the value came from, e.g. `iogpu.wired_limit_mb` or the default rule.
    pub source: String,
}

/// How a process's memory was measured. Each OS has a preferred, most accurate kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// macOS physical footprint, the value Activity Monitor shows.
    Footprint,
    /// Linux proportional set size: private memory plus a fair share of shared pages
    /// (`/proc/[pid]/smaps_rollup`).
    Pss,
    /// Resident set size; overcounts shared memory. Used as a fallback.
    Rss,
    /// Windows private working set, the value Task Manager's Details tab shows.
    PrivateWorkingSet,
}

impl MemoryKind {
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Footprint => "physical footprint",
            MemoryKind::Pss => "proportional set size",
            MemoryKind::Rss => "resident size",
            MemoryKind::PrivateWorkingSet => "private working set",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcessMemory {
    pub bytes: u64,
    pub kind: MemoryKind,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Process {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub exe: Option<String>,
    pub user: Option<String>,
    /// `None` when the OS refused to tell (typically another user's process without admin
    /// rights).
    pub memory: Option<ProcessMemory>,
    /// The application the process belongs to, used to group helpers with their app: on macOS
    /// the outermost `.app` bundle; on Windows the executable path of the app's root process
    /// (helpers started by it from its own install folder); on Linux the systemd service, Flatpak
    /// or Snap the process runs in, else the root process's executable (ADR-017).
    pub app_bundle: Option<String>,
}

/// How processes were folded into an application group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupKind {
    /// All processes inside one application bundle (helpers, renderers, XPC services).
    AppBundle,
    /// Processes sharing an executable name.
    Executable,
}

/// One application: the app and all its helper processes, shown as a single row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppGroup {
    pub name: String,
    pub kind: GroupKind,
    pub bundle_path: Option<String>,
    /// Sum of the readable processes' memory.
    pub memory_bytes: u64,
    /// The measurement kinds that make up `memory_bytes` (normally one).
    pub memory_kinds: Vec<MemoryKind>,
    pub process_count: usize,
    /// Processes whose memory could not be read and is not in `memory_bytes`.
    pub unreadable_count: usize,
    pub pids: Vec<u32>,
}

/// Unified pressure index (spec §7.4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pressure {
    /// 0 (idle) to 100 (about to fail).
    pub score: u8,
    pub level: PressureLevel,
    pub main_reason: PressureReason,
    /// Every signal's own score, for explanations.
    pub signals: Vec<SignalScore>,
    /// Pressure scores are always heuristics.
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureLevel {
    Calm,
    Strained,
    Critical,
}

impl PressureLevel {
    pub fn label(self) -> &'static str {
        match self {
            PressureLevel::Calm => "calm",
            PressureLevel::Strained => "strained",
            PressureLevel::Critical => "critical",
        }
    }

    /// Colour-blind-safe symbol paired with the level (spec §11).
    pub fn symbol(self) -> &'static str {
        match self {
            PressureLevel::Calm => "●",
            PressureLevel::Strained => "▲",
            PressureLevel::Critical => "✗",
        }
    }
}

/// Which signal drives the pressure score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureSignal {
    /// Nothing stands out.
    None,
    /// The operating system's own pressure level.
    OsLevel,
    /// Little memory is available.
    LowAvailable,
    /// The OS is compressing a lot of memory.
    Compression,
    /// Swap is growing.
    SwapGrowing,
    /// Committed memory is close to the commit limit (Windows): new allocations may fail.
    CommitLimit,
    /// Many pages are read back from disk every second (Windows hard page faults).
    HardFaults,
    /// Programs spend time waiting for memory (Linux PSI).
    MemoryStall,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PressureReason {
    pub signal: PressureSignal,
    /// One plain-language sentence fragment, e.g. "macOS is compressing 3.1 GB to make room".
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SignalScore {
    pub signal: PressureSignal,
    pub score: u8,
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

/// Formats a byte count the way Activity Monitor does: binary units labelled GB / MB.
pub fn format_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GB", b / GIB)
    } else if b >= MIB {
        format!("{:.0} MB", b / MIB)
    } else {
        format!("{:.0} KB", b / 1024.0)
    }
}

/// Formats a rate in bytes per second.
pub fn format_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= GIB {
        format!("{:.1} GB/s", bytes_per_sec / GIB)
    } else if bytes_per_sec >= MIB {
        format!("{:.1} MB/s", bytes_per_sec / MIB)
    } else {
        format!("{:.0} KB/s", bytes_per_sec / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_use_binary_units_like_activity_monitor() {
        assert_eq!(format_bytes(16 * 1024 * 1024 * 1024), "16.0 GB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
        assert_eq!(format_bytes(200 * 1024 * 1024), "200 MB");
        assert_eq!(format_bytes(4096), "4 KB");
        assert_eq!(format_rate(2.5 * MIB), "2.5 MB/s");
    }

    #[test]
    fn enums_serialize_in_snake_case() {
        let json = serde_json::to_string(&Provenance::ReadFromConfig).unwrap();
        assert_eq!(json, "\"read_from_config\"");
        let json = serde_json::to_string(&MemoryKind::PrivateWorkingSet).unwrap();
        assert_eq!(json, "\"private_working_set\"");
    }
}
