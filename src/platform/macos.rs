//! macOS backend: `host_statistics64`, `sysctl` (`hw.memsize`, `vm.swapusage`,
//! `kern.memorystatus_vm_pressure_level`, `iogpu.wired_limit_mb`) and per-process physical
//! footprint from `proc_pid_rusage`, the value Activity Monitor shows. Public APIs only; no admin
//! rights and no private frameworks. Conversions to the shared model live in `darwin.rs`.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_int, c_void};
use std::mem::{MaybeUninit, size_of};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::darwin::{self, SwapUsage, VmCounters};
use super::{CloseSignal, PlatformError, ProcessControl, SystemSource};
use crate::core::group::outermost_app_bundle;
use crate::core::sampler::RawSample;
use crate::core::{CpuInfo, MemoryKind, PlatformInfo, Process, ProcessMemory, Provenance};

pub struct MacSource {
    host: libc::host_t,
    total_bytes: u64,
    page_size: u64,
    apple_silicon: bool,
    platform: PlatformInfo,
    system: sysinfo::System,
    cpu_primed: bool,
    physical_cores: Option<usize>,
    users: HashMap<u32, Option<String>>,
    /// Executable paths of processes already seen, so repeated scans (the live monitor) skip
    /// `proc_pidpath`, the costliest call of a scan. See [`PathKey`].
    paths: HashMap<PathKey, Option<String>>,
}

/// Identifies one program run: a reused pid has a different start time, and `exec` in place
/// changes the command name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PathKey {
    pid: c_int,
    start_sec: u64,
    start_usec: u64,
    comm: [libc::c_char; 16],
}

impl PathKey {
    fn new(pid: c_int, bsd: &libc::proc_bsdinfo) -> PathKey {
        PathKey {
            pid,
            start_sec: bsd.pbi_start_tvsec,
            start_usec: bsd.pbi_start_tvusec,
            comm: bsd.pbi_comm,
        }
    }
}

impl MacSource {
    pub fn new() -> Result<Self, PlatformError> {
        let total_bytes = sysctl_int("hw.memsize").map_err(|detail| PlatformError::Read {
            what: "the amount of memory (hw.memsize)",
            detail,
        })?;
        let apple_silicon = sysctl_int("hw.optional.arm64").is_ok_and(|v| v == 1);
        let system = sysinfo::System::new().map_err(|e| PlatformError::Read {
            what: "CPU information",
            detail: e.to_string(),
        })?;
        // SAFETY: returns a send right to the host port; it is kept for the source's lifetime.
        let host = unsafe { mach2::mach_init::mach_host_self() };
        Ok(MacSource {
            host,
            total_bytes,
            page_size: sysctl_int("hw.pagesize").map_err(|detail| PlatformError::Read {
                what: "the memory page size (hw.pagesize)",
                detail,
            })?,
            apple_silicon,
            platform: PlatformInfo {
                os: "macos".to_string(),
                os_version: sysctl_string("kern.osproductversion"),
                // The hardware architecture, also when canfit runs under Rosetta.
                arch: if apple_silicon {
                    "aarch64".to_string()
                } else {
                    std::env::consts::ARCH.to_string()
                },
                unified_memory: apple_silicon,
            },
            system,
            cpu_primed: false,
            physical_cores: sysinfo::System::physical_core_count().ok(),
            users: HashMap::new(),
            paths: HashMap::new(),
        })
    }

    fn vm_counters(&self) -> Result<VmCounters, PlatformError> {
        let mut stats = MaybeUninit::<libc::vm_statistics64>::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: `stats` is large enough for `count` integers; the kernel writes at most that.
        let kr = unsafe {
            libc::host_statistics64(
                self.host,
                libc::HOST_VM_INFO64,
                stats.as_mut_ptr().cast(),
                &mut count,
            )
        };
        if kr != libc::KERN_SUCCESS {
            return Err(PlatformError::Read {
                what: "memory statistics (host_statistics64)",
                detail: format!("kernel returned {kr}"),
            });
        }
        // SAFETY: the call succeeded, and the struct was zero-initialised beyond `count`.
        let s = unsafe { stats.assume_init() };
        let free = u64::from(s.free_count);
        let speculative = u64::from(s.speculative_count);
        Ok(VmCounters {
            page_size: self.page_size,
            free: free.saturating_sub(speculative),
            active: u64::from(s.active_count),
            inactive: u64::from(s.inactive_count),
            speculative,
            wired: u64::from(s.wire_count),
            purgeable: u64::from(s.purgeable_count),
            external: u64::from(s.external_page_count),
            internal: u64::from(s.internal_page_count),
            compressor_occupied: u64::from(s.compressor_page_count),
            compressor_stored: { s.total_uncompressed_pages_in_compressor },
            swapouts: { s.swapouts },
        })
    }

    fn cpu(&mut self) -> CpuInfo {
        self.system.refresh_cpu_usage();
        let usage = self.cpu_primed.then(|| self.system.global_cpu_usage());
        self.cpu_primed = true;
        let cpus = self.system.cpus();
        let load = sysinfo::System::load_average().ok();
        CpuInfo {
            logical_cores: cpus.len(),
            physical_cores: self.physical_cores,
            brand: cpus
                .first()
                .map(|c| c.brand().trim().to_string())
                .filter(|b| !b.is_empty()),
            usage_percent: usage,
            load_average: load.map(|l| [l.one, l.five, l.fifteen]),
            provenance: Provenance::Measured,
        }
    }

    fn processes(&mut self) -> Vec<Process> {
        let mut seen = HashMap::with_capacity(self.paths.len());
        let list = list_pids()
            .into_iter()
            .filter_map(|pid| self.process(pid, &mut seen))
            .collect();
        // Forget processes that have exited.
        self.paths = seen;
        list
    }

    fn process(
        &mut self,
        pid: c_int,
        seen: &mut HashMap<PathKey, Option<String>>,
    ) -> Option<Process> {
        let bsd = pid_info::<libc::proc_bsdinfo>(pid, libc::PROC_PIDTBSDINFO);
        let exe = match &bsd {
            Some(b) => {
                let key = PathKey::new(pid, b);
                let exe = match self.paths.get(&key) {
                    Some(path) => path.clone(),
                    None => pid_path(pid),
                };
                seen.insert(key, exe.clone());
                exe
            }
            None => pid_path(pid),
        };
        let name = process_name(pid, exe.as_deref(), bsd.as_ref())?;
        let user = bsd.as_ref().and_then(|b| self.user_name(b.pbi_uid));
        Some(Process {
            pid: pid as u32,
            ppid: bsd.as_ref().map(|b| b.pbi_ppid),
            name,
            app_bundle: exe
                .as_deref()
                .and_then(outermost_app_bundle)
                .map(str::to_string),
            exe,
            user,
            memory: process_memory(pid),
        })
    }

    fn user_name(&mut self, uid: libc::uid_t) -> Option<String> {
        self.users
            .entry(uid)
            .or_insert_with(|| lookup_user(uid))
            .clone()
    }
}

impl SystemSource for MacSource {
    fn sample(&mut self, with_processes: bool) -> Result<RawSample, PlatformError> {
        let taken_at = Instant::now();
        let timestamp_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let counters = self.vm_counters()?;
        let swap = swap_usage().map_err(|detail| PlatformError::Read {
            what: "swap usage (vm.swapusage)",
            detail,
        })?;
        let os_pressure = sysctl_int("kern.memorystatus_vm_pressure_level")
            .ok()
            .and_then(|v| darwin::os_pressure_from_sysctl(v as i64));
        let gpu_memory_limit = self.apple_silicon.then(|| {
            darwin::gpu_memory_limit(self.total_bytes, sysctl_int("iogpu.wired_limit_mb").ok())
        });
        let cpu = self.cpu();
        let processes = with_processes.then(|| self.processes());
        Ok(RawSample {
            taken_at,
            timestamp_unix_ms,
            os_name: "macOS",
            platform: self.platform.clone(),
            memory: darwin::memory_from_counters(self.total_bytes, &counters),
            swap: darwin::swap_from_usage(&swap, &counters),
            cpu,
            os_pressure,
            hard_faults_per_sec: None,
            stall: None,
            gpu_memory_limit,
            processes,
        })
    }
}

/// The process name canfit shows: the executable's file name, else the kernel's short name.
fn process_name(pid: c_int, exe: Option<&str>, bsd: Option<&libc::proc_bsdinfo>) -> Option<String> {
    exe.and_then(|path| path.rsplit('/').next())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| bsd.map(bsd_name))
        .or_else(|| pid_name(pid))
}

/// Kernel process state of a zombie (`SZOMB` in `<sys/proc.h>`).
const SZOMB: u32 = 5;

/// Signals processes with `kill(2)`; only processes of the current user can be signalled.
pub struct MacProcessControl;

impl ProcessControl for MacProcessControl {
    fn is_running(&self, pid: u32, name: &str) -> bool {
        let Ok(pid) = c_int::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // A zombie has exited: its memory is already free.
        let Some(bsd) = pid_info::<libc::proc_bsdinfo>(pid, libc::PROC_PIDTBSDINFO) else {
            return false;
        };
        if bsd.pbi_status == SZOMB {
            return false;
        }
        process_name(pid, pid_path(pid).as_deref(), Some(&bsd)).as_deref() == Some(name)
    }

    fn signal(&mut self, pid: u32, signal: CloseSignal) -> Result<(), String> {
        let pid = c_int::try_from(pid).map_err(|_| "invalid process id".to_string())?;
        // pid 0 and negative pids address process groups; never send those.
        if pid <= 1 {
            return Err("refusing to signal a process group or launchd".to_string());
        }
        let sig = match signal {
            CloseSignal::Graceful => libc::SIGTERM,
            CloseSignal::Force => libc::SIGKILL,
        };
        // SAFETY: plain kill(2) on a single positive pid.
        if unsafe { libc::kill(pid, sig) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    }
}

/// Physical footprint when the OS allows it; resident size as a labelled fallback; `None` when
/// neither is readable (another user's process without admin rights).
fn process_memory(pid: c_int) -> Option<ProcessMemory> {
    let mut info = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `info` is a valid rusage_info_v4 buffer for flavor RUSAGE_INFO_V4.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    if rc == 0 {
        // SAFETY: the call succeeded and filled the struct.
        let info = unsafe { info.assume_init() };
        return Some(ProcessMemory {
            bytes: info.ri_phys_footprint,
            kind: MemoryKind::Footprint,
            provenance: Provenance::Measured,
        });
    }
    pid_info::<libc::proc_taskinfo>(pid, libc::PROC_PIDTASKINFO).map(|task| ProcessMemory {
        bytes: task.pti_resident_size,
        kind: MemoryKind::Rss,
        provenance: Provenance::Measured,
    })
}

fn list_pids() -> Vec<c_int> {
    // SAFETY: a null buffer asks for the number of processes.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Leave room for processes started between the two calls.
    let mut pids: Vec<c_int> = vec![0; count as usize + 64];
    let bytes = (pids.len() * size_of::<c_int>()) as c_int;
    // SAFETY: `pids` holds `bytes` bytes.
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
    pids.truncate(filled.max(0) as usize);
    pids.retain(|&pid| pid >= 0);
    pids
}

/// `proc_pidinfo` for a fixed-size flavor struct.
fn pid_info<T>(pid: c_int, flavor: c_int) -> Option<T> {
    let mut info = MaybeUninit::<T>::zeroed();
    let size = size_of::<T>() as c_int;
    // SAFETY: `info` is `size` bytes; the kernel writes at most that.
    let written = unsafe { libc::proc_pidinfo(pid, flavor, 0, info.as_mut_ptr().cast(), size) };
    // SAFETY: a full-size write means the struct is initialised.
    (written == size).then(|| unsafe { info.assume_init() })
}

fn pid_path(pid: c_int) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is PROC_PIDPATHINFO_MAXSIZE bytes.
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (len > 0).then(|| String::from_utf8_lossy(&buf[..len as usize]).into_owned())
}

fn pid_name(pid: c_int) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is 256 bytes.
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (len > 0).then(|| String::from_utf8_lossy(&buf[..len as usize]).into_owned())
}

fn bsd_name(info: &libc::proc_bsdinfo) -> String {
    let field = if info.pbi_name[0] != 0 {
        &info.pbi_name[..]
    } else {
        &info.pbi_comm[..]
    };
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn lookup_user(uid: libc::uid_t) -> Option<String> {
    let mut pwd = MaybeUninit::<libc::passwd>::zeroed();
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all buffers are valid for the sizes passed.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: on success `pw_name` points into `buf`, which is still alive.
    let name = unsafe { CStr::from_ptr((*result).pw_name) };
    Some(name.to_string_lossy().into_owned())
}

fn swap_usage() -> Result<SwapUsage, String> {
    let usage: libc::xsw_usage = sysctl_raw("vm.swapusage")?;
    Ok(SwapUsage {
        total_bytes: usage.xsu_total,
        used_bytes: usage.xsu_used,
        free_bytes: usage.xsu_avail,
        encrypted: usage.xsu_encrypted != 0,
    })
}

/// Reads an integer sysctl of 4 or 8 bytes.
fn sysctl_int(name: &str) -> Result<u64, String> {
    let mut buf = [0u8; 8];
    let len = sysctl_into(name, &mut buf)?;
    match len {
        4 => Ok(u64::from(u32::from_ne_bytes(buf[..4].try_into().unwrap()))),
        8 => Ok(u64::from_ne_bytes(buf)),
        n => Err(format!("{name}: unexpected size {n}")),
    }
}

fn sysctl_string(name: &str) -> Option<String> {
    let mut buf = [0u8; 256];
    let len = sysctl_into(name, &mut buf).ok()?;
    let text = CStr::from_bytes_until_nul(&buf[..len]).ok()?;
    Some(text.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
}

/// Reads a fixed-size struct sysctl.
fn sysctl_raw<T>(name: &str) -> Result<T, String> {
    let mut value = MaybeUninit::<T>::zeroed();
    // SAFETY: the slice covers exactly the struct's bytes.
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    let len = sysctl_into(name, bytes)?;
    if len != size_of::<T>() {
        return Err(format!("{name}: unexpected size {len}"));
    }
    // SAFETY: the kernel filled all bytes of a plain-data struct.
    Ok(unsafe { value.assume_init() })
}

fn sysctl_into(name: &str, buf: &mut [u8]) -> Result<usize, String> {
    let cname = CString::new(name).map_err(|e| e.to_string())?;
    let mut len = buf.len();
    // SAFETY: `buf` is `len` bytes; the kernel writes at most `len` and updates it.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(format!("{name}: {}", std::io::Error::last_os_error()));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    //! Live checks against this Mac. They only assert invariants, never exact values.

    use super::*;

    #[test]
    fn live_snapshot_is_consistent() {
        let mut source = MacSource::new().expect("macOS source");
        let sample = source.sample(true).expect("sample");
        let m = &sample.memory;
        assert!(m.total_bytes > 1 << 30);
        assert!(m.used_bytes <= m.total_bytes);
        assert_eq!(m.used_bytes + m.available_bytes, m.total_bytes);
        assert!(sample.os_pressure.is_some());
        assert!(sample.cpu.logical_cores > 0);
        let processes = sample.processes.expect("processes");
        assert!(processes.len() > 10);
        // canfit's own test process is always readable with its physical footprint.
        let me = std::process::id();
        let own = processes.iter().find(|p| p.pid == me).expect("own process");
        assert_eq!(own.memory.as_ref().unwrap().kind, MemoryKind::Footprint);
    }

    #[test]
    fn repeated_scans_reuse_paths_and_agree() {
        let mut source = MacSource::new().expect("macOS source");
        let first = source.sample(true).expect("sample").processes.unwrap();
        assert!(!source.paths.is_empty(), "paths are cached after a scan");
        let second = source.sample(true).expect("sample").processes.unwrap();
        let me = std::process::id();
        let exe = |list: &[Process]| {
            list.iter()
                .find(|p| p.pid == me)
                .and_then(|p| p.exe.clone())
        };
        assert!(exe(&first).is_some());
        assert_eq!(exe(&first), exe(&second));
        // Only live processes stay in the cache.
        assert!(source.paths.len() <= second.len() + 50);
    }

    #[test]
    fn sysctl_strings_and_ints_read() {
        assert!(sysctl_int("hw.memsize").unwrap() > 0);
        assert!(sysctl_string("kern.osproductversion").is_some());
        assert!(sysctl_int("no.such.sysctl").is_err());
    }
}
