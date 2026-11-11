//! Linux backend (ADR-017): `/proc/meminfo`, `/proc/vmstat`, PSI (`/proc/pressure/*`, else the
//! cgroup v2 root's `*.pressure` files), zram `mm_stat`, and per-process PSS from
//! `/proc/[pid]/smaps_rollup` (resident size, labelled, when the kernel refuses). Processes are
//! grouped by systemd service, Flatpak or Snap, else by process tree and install folder. No root
//! needed; readings and grouping live in `linux_model.rs`, which is tested on every OS.
//!
//! Closing: SIGTERM, then SIGKILL after the second confirmation, to user-space processes only.

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::linux_model::{self, KTHREADD_PID, Known, ProcFs};
use super::{CloseSignal, PlatformError, ProcessControl, SystemSource};
use crate::core::sampler::RawSample;
use crate::core::{CpuInfo, PlatformInfo, Provenance};

pub struct LinuxSource {
    fs: ProcFs,
    platform: PlatformInfo,
    system: sysinfo::System,
    cpu_primed: bool,
    physical_cores: Option<usize>,
    page_size: u64,
    /// User names by uid, from `/etc/passwd`.
    users: HashMap<u32, String>,
    /// Executable paths and names by pid and start time: a reused pid is a different program.
    known: HashMap<(u32, u64), Known>,
}

fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).ok().filter(|&s| s > 0).unwrap_or(4096)
}

impl LinuxSource {
    pub fn new() -> Result<Self, PlatformError> {
        let fs = ProcFs::system();
        // Fail early with a clear message when /proc is not mounted.
        fs.meminfo().map_err(|e| PlatformError::Read {
            what: "memory information (/proc/meminfo)",
            detail: e.to_string(),
        })?;
        let system = sysinfo::System::new().map_err(|e| PlatformError::Read {
            what: "CPU information",
            detail: e.to_string(),
        })?;
        Ok(LinuxSource {
            users: fs.users(),
            fs,
            platform: PlatformInfo {
                os: "linux".to_string(),
                os_version: sysinfo::System::long_os_version().ok(),
                arch: std::env::consts::ARCH.to_string(),
                unified_memory: false,
            },
            system,
            cpu_primed: false,
            physical_cores: sysinfo::System::physical_core_count().ok(),
            page_size: page_size(),
            known: HashMap::new(),
        })
    }

    fn cpu(&mut self) -> CpuInfo {
        self.system.refresh_cpu_usage();
        let usage = self.cpu_primed.then(|| self.system.global_cpu_usage());
        self.cpu_primed = true;
        let cpus = self.system.cpus();
        CpuInfo {
            logical_cores: cpus.len(),
            physical_cores: self.physical_cores,
            brand: cpus
                .first()
                .map(|c| c.brand().trim().to_string())
                .filter(|b| !b.is_empty()),
            usage_percent: usage,
            load_average: self.fs.load_average(),
            provenance: Provenance::Measured,
        }
    }
}

impl SystemSource for LinuxSource {
    fn sample(&mut self, with_processes: bool) -> Result<RawSample, PlatformError> {
        let taken_at = Instant::now();
        let timestamp_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let meminfo = self.fs.meminfo().map_err(|e| PlatformError::Read {
            what: "memory information (/proc/meminfo)",
            detail: e.to_string(),
        })?;
        let memory =
            linux_model::memory_from_meminfo(&meminfo, self.fs.zram()).map_err(|detail| {
                PlatformError::Read {
                    what: "memory information (/proc/meminfo)",
                    detail,
                }
            })?;
        let swap =
            linux_model::swap_from_meminfo(&meminfo, self.fs.swapped_out_pages(), self.page_size);
        let stall = self.fs.stall();
        let cpu = self.cpu();
        let processes = with_processes.then(|| {
            let scan = self.fs.scan(&self.users, &self.known);
            // Forget programs that have exited.
            self.known = scan.known;
            scan.processes
        });
        Ok(RawSample {
            taken_at,
            timestamp_unix_ms,
            os_name: "Linux",
            platform: self.platform.clone(),
            memory,
            swap,
            cpu,
            // Linux has no pressure verdict of its own; PSI is scored as its own signal.
            os_pressure: None,
            hard_faults_per_sec: None,
            stall,
            // Discrete GPUs have their own memory; NVML and AMD sysfs come in a later milestone.
            gpu_memory_limit: None,
            processes,
        })
    }
}

/// Closes processes of the current user with SIGTERM, then SIGKILL.
pub struct LinuxProcessControl {
    fs: ProcFs,
}

impl LinuxProcessControl {
    pub fn new() -> Self {
        LinuxProcessControl {
            fs: ProcFs::system(),
        }
    }
}

impl ProcessControl for LinuxProcessControl {
    fn is_running(&self, pid: u32, name: &str) -> bool {
        if pid == 0 {
            return false;
        }
        // A zombie has exited: its memory is already free.
        let Some(stat) = self.fs.stat(pid) else {
            return false;
        };
        if stat.has_exited() || stat.is_kernel_thread() {
            return false;
        }
        self.fs.identify(pid, &stat).name == name
    }

    fn signal(&mut self, pid: u32, signal: CloseSignal) -> Result<(), String> {
        let pid_t = libc::pid_t::try_from(pid).map_err(|_| "invalid process id".to_string())?;
        // pid 0 and negative pids address process groups; pid 1 is init.
        if pid_t <= 1 || pid == KTHREADD_PID {
            return Err("refusing to signal a process group, init or kthreadd".to_string());
        }
        if self.fs.stat(pid).is_some_and(|s| s.is_kernel_thread()) {
            return Err("refusing to signal a kernel thread".to_string());
        }
        let sig = match signal {
            CloseSignal::Graceful => libc::SIGTERM,
            CloseSignal::Force => libc::SIGKILL,
        };
        // SAFETY: plain kill(2) on a single positive pid.
        if unsafe { libc::kill(pid_t, sig) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    //! Live checks against this Linux machine (or VM). They only assert invariants.

    use super::*;
    use crate::core::MemoryKind;

    #[test]
    fn live_snapshot_is_consistent() {
        let mut source = LinuxSource::new().expect("Linux source");
        source.sample(false).expect("first sample");
        let sample = source.sample(true).expect("sample");
        let m = &sample.memory;
        assert!(m.total_bytes > 256 << 20);
        assert_eq!(m.used_bytes + m.available_bytes, m.total_bytes);
        assert!(sample.swap.used_bytes <= sample.swap.total_bytes);
        assert!(sample.swap.swapped_out_total_bytes.is_some());
        assert!(sample.cpu.logical_cores > 0);
        assert!(sample.cpu.load_average.is_some());
        let processes = sample.processes.expect("processes");
        assert!(processes.len() > 1);
        // The test's own process: readable PSS, its user and executable.
        let me = std::process::id();
        let own = processes.iter().find(|p| p.pid == me).expect("own process");
        assert_eq!(own.memory.as_ref().unwrap().kind, MemoryKind::Pss);
        assert!(own.user.is_some());
        assert!(own.exe.is_some());
        // Kernel threads are never listed.
        assert!(processes.iter().all(|p| p.pid != KTHREADD_PID));
    }

    #[test]
    fn repeated_scans_reuse_cached_paths() {
        let mut source = LinuxSource::new().expect("Linux source");
        let first = source.sample(true).expect("sample").processes.unwrap();
        assert!(!source.known.is_empty());
        let second = source.sample(true).expect("sample").processes.unwrap();
        let me = std::process::id();
        let exe = |list: &[crate::core::Process]| {
            list.iter()
                .find(|p| p.pid == me)
                .and_then(|p| p.exe.clone())
        };
        assert!(exe(&first).is_some());
        assert_eq!(exe(&first), exe(&second));
    }

    #[test]
    fn init_and_kernel_threads_are_never_signalled() {
        let mut control = LinuxProcessControl::new();
        assert!(control.signal(0, CloseSignal::Force).is_err());
        assert!(control.signal(1, CloseSignal::Graceful).is_err());
        assert!(control.signal(KTHREADD_PID, CloseSignal::Force).is_err());
        assert!(!control.is_running(KTHREADD_PID, "kthreadd"));
        assert!(!control.is_running(0, "swapper"));
    }
}
