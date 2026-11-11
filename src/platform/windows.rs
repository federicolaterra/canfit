//! Windows backend (ADR-016): `GlobalMemoryStatusEx`, `GetPerformanceInfo` (commit charge and
//! limit), `EnumPageFilesW` (page files as swap), PDH counters (pages read from disk per second,
//! free and zeroed memory), the low-memory resource notification, and per-process **private
//! working set** (`GetProcessMemoryInfo`), the value Task Manager's Details tab shows. Documented
//! APIs only; no admin rights. Conversions to the shared model live in `windows_model.rs`.
//!
//! Closing: the graceful request posts `WM_CLOSE` to the process's visible top-level windows, as
//! Task Manager's "End task" does; the forced close is `TerminateProcess`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HLOCAL, HWND, LPARAM, LocalFree, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, PSID, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Memory::{
    CreateMemoryResourceNotification, LowMemoryResourceNotification,
    QueryMemoryResourceNotification,
};
use windows::Win32::System::Performance::{
    PDH_FMT, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW,
};
use windows::Win32::System::ProcessStatus::{
    ENUM_PAGE_FILE_INFORMATION, EnumPageFilesW, GetPerformanceInfo, GetProcessMemoryInfo,
    PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, PROCESS_VM_READ,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
    WM_CLOSE,
};
use windows::core::{BOOL, PCWSTR, PWSTR, w};

use super::windows_model::{
    self, MemoryStatus, PageFiles, PerformanceInfo, assign_apps, file_name,
};
use super::{CloseSignal, PlatformError, ProcessControl, SystemSource};
use crate::advisor::rules::windows_system_root;
use crate::core::sampler::RawSample;
use crate::core::{CpuInfo, MemoryKind, PlatformInfo, Process, ProcessMemory, Provenance};

/// Process ids Windows reserves: the idle process and the kernel ("System").
const IDLE_PID: u32 = 0;
const SYSTEM_PID: u32 = 4;

/// Closes a kernel handle when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned open by the API that created it and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<Handle> {
    // SAFETY: plain OpenProcess; the handle is owned by the guard.
    unsafe { OpenProcess(access, false, pid) }.ok().map(Handle)
}

/// A PDH query with the counters canfit reads. Rate counters need two collections, so the first
/// sample has no rate.
struct Pdh {
    query: PDH_HQUERY,
    pages_input: Option<PDH_HCOUNTER>,
    free_and_zero: Option<PDH_HCOUNTER>,
}

impl Pdh {
    fn open() -> Option<Pdh> {
        let mut query = PDH_HQUERY::default();
        // SAFETY: `query` receives the new query handle.
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
            return None;
        }
        // English names work on every display language.
        let add = |path: PCWSTR| {
            let mut counter = PDH_HCOUNTER::default();
            // SAFETY: valid query, static wide string, `counter` receives the handle.
            (unsafe { PdhAddEnglishCounterW(query, path, 0, &mut counter) } == 0).then_some(counter)
        };
        let pdh = Pdh {
            query,
            pages_input: add(w!("\\Memory\\Pages Input/sec")),
            free_and_zero: add(w!("\\Memory\\Free & Zero Page List Bytes")),
        };
        Some(pdh)
    }

    fn collect(&self) -> bool {
        // SAFETY: valid query handle.
        unsafe { PdhCollectQueryData(self.query) == 0 }
    }

    fn value(counter: Option<PDH_HCOUNTER>, format: PDH_FMT) -> Option<PDH_FMT_COUNTERVALUE> {
        let counter = counter?;
        let mut value = PDH_FMT_COUNTERVALUE::default();
        // SAFETY: valid counter handle and output struct.
        let status = unsafe { PdhGetFormattedCounterValue(counter, format, None, &mut value) };
        (status == 0 && value.CStatus == 0).then_some(value)
    }

    fn pages_input_per_sec(&self) -> Option<f64> {
        // SAFETY: PDH_FMT_DOUBLE fills the `doubleValue` member.
        Self::value(self.pages_input, PDH_FMT_DOUBLE)
            .map(|v| unsafe { v.Anonymous.doubleValue })
            .filter(|v| v.is_finite() && *v >= 0.0)
    }

    fn free_and_zero_bytes(&self) -> Option<u64> {
        // SAFETY: PDH_FMT_LARGE fills the `largeValue` member.
        Self::value(self.free_and_zero, PDH_FMT_LARGE)
            .and_then(|v| u64::try_from(unsafe { v.Anonymous.largeValue }).ok())
    }
}

impl Drop for Pdh {
    fn drop(&mut self) {
        // SAFETY: the query was opened by PdhOpenQueryW and is closed once.
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// What canfit remembers about one program run, so repeated scans skip the costly calls.
#[derive(Debug, Clone)]
struct Known {
    exe: Option<String>,
    user: Option<String>,
}

pub struct WinSource {
    platform: PlatformInfo,
    system: sysinfo::System,
    cpu_primed: bool,
    physical_cores: Option<usize>,
    page_size: u64,
    system_root: String,
    pdh: Option<Pdh>,
    low_memory: Option<Handle>,
    /// Keyed by pid and creation time: a reused pid is a different program.
    known: HashMap<(u32, u64), Known>,
    /// Account names by SID string.
    accounts: HashMap<String, Option<String>>,
    /// The compressed store's size from the last process scan, when Windows lets canfit read it.
    compressed: Option<u64>,
}

impl WinSource {
    pub fn new() -> Result<Self, PlatformError> {
        let system = sysinfo::System::new().map_err(|e| PlatformError::Read {
            what: "CPU information",
            detail: e.to_string(),
        })?;
        let perf = performance_info()?;
        let mut pdh = Pdh::open();
        if let Some(pdh) = pdh.as_mut() {
            // Start the rate counters, so the next sample has a rate.
            pdh.collect();
        }
        // SAFETY: plain call; the handle is owned by the guard.
        let low_memory = unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) }
            .ok()
            .map(Handle);
        Ok(WinSource {
            platform: PlatformInfo {
                os: "windows".to_string(),
                os_version: sysinfo::System::long_os_version().ok(),
                arch: std::env::consts::ARCH.to_string(),
                unified_memory: false,
            },
            system,
            cpu_primed: false,
            physical_cores: sysinfo::System::physical_core_count().ok(),
            page_size: perf.PageSize as u64,
            system_root: windows_system_root(),
            pdh,
            low_memory,
            known: HashMap::new(),
            accounts: HashMap::new(),
            compressed: None,
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
            // Windows has no load average.
            load_average: None,
            provenance: Provenance::Measured,
        }
    }

    fn low_memory(&self) -> Option<bool> {
        let handle = self.low_memory.as_ref()?;
        let mut state = BOOL::default();
        // SAFETY: valid notification handle and output BOOL.
        unsafe { QueryMemoryResourceNotification(handle.0, &mut state) }.ok()?;
        Some(state.as_bool())
    }

    fn page_files(&self) -> Result<PageFiles, PlatformError> {
        unsafe extern "system" fn add(
            context: *mut c_void,
            info: *mut ENUM_PAGE_FILE_INFORMATION,
            _name: PCWSTR,
        ) -> BOOL {
            // SAFETY: `context` is the `(u64, u64)` passed below; `info` is valid for the call.
            unsafe {
                let totals = &mut *context.cast::<(u64, u64)>();
                totals.0 += (*info).TotalSize as u64;
                totals.1 += (*info).TotalInUse as u64;
            }
            BOOL::from(true)
        }
        let mut pages: (u64, u64) = (0, 0);
        // SAFETY: the callback only touches `pages`, which outlives the call.
        unsafe { EnumPageFilesW(Some(add), (&mut pages as *mut (u64, u64)).cast()) }.map_err(
            |e| PlatformError::Read {
                what: "page file usage (EnumPageFilesW)",
                detail: e.to_string(),
            },
        )?;
        Ok(PageFiles {
            total_bytes: pages.0 * self.page_size,
            used_bytes: pages.1 * self.page_size,
        })
    }

    fn processes(&mut self) -> Vec<Process> {
        let entries = process_entries();
        let mut started: HashMap<u32, u64> = HashMap::with_capacity(entries.len());
        let mut seen: HashMap<(u32, u64), Known> = HashMap::with_capacity(entries.len());
        let mut list = Vec::with_capacity(entries.len());
        let mut compressed = None;
        for entry in entries {
            if entry.pid == IDLE_PID {
                continue;
            }
            let handle = open_process(
                entry.pid,
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            )
            .or_else(|| open_process(entry.pid, PROCESS_QUERY_LIMITED_INFORMATION));
            let (known, memory) = match &handle {
                Some(h) => {
                    let start = creation_time(h);
                    let known = match start.and_then(|s| self.known.get(&(entry.pid, s))) {
                        Some(known) => known.clone(),
                        None => Known {
                            exe: image_path(h),
                            user: self.process_user(h),
                        },
                    };
                    if let Some(start) = start {
                        started.insert(entry.pid, start);
                        seen.insert((entry.pid, start), known.clone());
                    }
                    (known, process_memory(h))
                }
                None => (
                    Known {
                        exe: None,
                        user: None,
                    },
                    None,
                ),
            };
            let name = known
                .exe
                .as_deref()
                .map(file_name)
                .filter(|n| !n.is_empty())
                .map_or_else(|| entry.name.clone(), str::to_string);
            if entry.name == "Memory Compression" {
                compressed = handle.as_ref().and_then(working_set);
            }
            list.push(Process {
                pid: entry.pid,
                ppid: Some(entry.ppid),
                name,
                exe: known.exe,
                user: known.user,
                memory,
                app_bundle: None,
            });
        }
        // Forget programs that have exited.
        self.known = seen;
        self.compressed = compressed;
        assign_apps(&mut list, &started, &self.system_root);
        list
    }

    /// The account owning the process, as `DOMAIN\name` (the SID string when the name can't be
    /// looked up). `None` when Windows refuses to tell.
    fn process_user(&mut self, process: &Handle) -> Option<String> {
        let mut token = HANDLE::default();
        // SAFETY: valid process handle; `token` receives a handle owned by the guard below.
        unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) }.ok()?;
        let token = Handle(token);
        let mut buffer = vec![0u64; 64]; // 512 bytes, 8-byte aligned for TOKEN_USER
        let mut needed = 0u32;
        // SAFETY: `buffer` is `len * 8` bytes; Windows writes at most that.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                (buffer.len() * size_of::<u64>()) as u32,
                &mut needed,
            )
        }
        .ok()?;
        // SAFETY: the call succeeded, so the buffer starts with a TOKEN_USER.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let key = sid_string(sid)?;
        if let Some(name) = self.accounts.get(&key) {
            return name.clone();
        }
        let name = account_name(sid).or_else(|| Some(key.clone()));
        self.accounts.insert(key, name.clone());
        name
    }
}

impl SystemSource for WinSource {
    fn sample(&mut self, with_processes: bool) -> Result<RawSample, PlatformError> {
        let taken_at = Instant::now();
        let timestamp_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let status = memory_status()?;
        let perf = performance_info()?;
        let page = perf.PageSize as u64;
        let perf = PerformanceInfo {
            commit_total: perf.CommitTotal as u64 * page,
            commit_limit: perf.CommitLimit as u64 * page,
            system_cache: perf.SystemCache as u64 * page,
            kernel_nonpaged: perf.KernelNonpaged as u64 * page,
        };
        let page_files = self.page_files()?;
        let (hard_faults_per_sec, free_bytes) = match self.pdh.as_mut() {
            Some(pdh) => {
                if pdh.collect() {
                    (pdh.pages_input_per_sec(), pdh.free_and_zero_bytes())
                } else {
                    (None, None)
                }
            }
            _ => (None, None),
        };
        let cpu = self.cpu();
        let processes = with_processes.then(|| self.processes());
        Ok(RawSample {
            taken_at,
            timestamp_unix_ms,
            os_name: "Windows",
            platform: self.platform.clone(),
            memory: windows_model::memory_from_readings(
                &MemoryStatus {
                    total_phys: status.ullTotalPhys,
                    avail_phys: status.ullAvailPhys,
                },
                &perf,
                free_bytes,
                self.compressed,
            ),
            swap: windows_model::swap_from_page_files(&page_files),
            cpu,
            os_pressure: windows_model::os_pressure_from_low_memory(self.low_memory()),
            hard_faults_per_sec,
            stall: None,
            // Discrete GPUs have their own memory; DXGI readings come in a later milestone.
            gpu_memory_limit: None,
            processes,
        })
    }
}

fn memory_status() -> Result<MEMORYSTATUSEX, PlatformError> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a MEMORYSTATUSEX with its length set.
    unsafe { GlobalMemoryStatusEx(&mut status) }.map_err(|e| PlatformError::Read {
        what: "memory status (GlobalMemoryStatusEx)",
        detail: e.to_string(),
    })?;
    Ok(status)
}

fn performance_info() -> Result<PERFORMANCE_INFORMATION, PlatformError> {
    let mut info = PERFORMANCE_INFORMATION::default();
    let size = size_of::<PERFORMANCE_INFORMATION>() as u32;
    // SAFETY: `info` is `size` bytes.
    unsafe { GetPerformanceInfo(&mut info, size) }.map_err(|e| PlatformError::Read {
        what: "commit charge (GetPerformanceInfo)",
        detail: e.to_string(),
    })?;
    Ok(info)
}

/// One row of the toolhelp process list.
struct Entry {
    pid: u32,
    ppid: u32,
    name: String,
}

fn process_entries() -> Vec<Entry> {
    // SAFETY: plain call; the snapshot handle is owned by the guard.
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let snapshot = Handle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    // SAFETY: `entry` has its size set; the snapshot handle is valid.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok();
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        out.push(Entry {
            pid: entry.th32ProcessID,
            ppid: entry.th32ParentProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..len]),
        });
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) }.is_ok();
    }
    out
}

fn creation_time(process: &Handle) -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: valid handle and output structs.
    unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) }
        .ok()?;
    Some(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
}

fn image_path(process: &Handle) -> Option<String> {
    let mut buffer = vec![0u16; 32_768];
    let mut len = buffer.len() as u32;
    // SAFETY: `buffer` holds `len` UTF-16 units; Windows updates `len`.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&buffer[..len as usize])).filter(|p| !p.is_empty())
}

/// Private working set when Windows supports it (Windows 10 1809 and later); the full working
/// set, which counts shared pages, as a labelled fallback.
fn process_memory(process: &Handle) -> Option<ProcessMemory> {
    let mut ex2 = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
        ..Default::default()
    };
    // SAFETY: `ex2` starts with the PROCESS_MEMORY_COUNTERS layout and `cb` is its full size.
    let ok = unsafe {
        GetProcessMemoryInfo(
            process.0,
            (&mut ex2 as *mut PROCESS_MEMORY_COUNTERS_EX2).cast::<PROCESS_MEMORY_COUNTERS>(),
            ex2.cb,
        )
    }
    .is_ok();
    if ok {
        return Some(ProcessMemory {
            bytes: ex2.PrivateWorkingSetSize as u64,
            kind: MemoryKind::PrivateWorkingSet,
            provenance: Provenance::Measured,
        });
    }
    working_set(process).map(|bytes| ProcessMemory {
        bytes,
        kind: MemoryKind::Rss,
        provenance: Provenance::Measured,
    })
}

fn working_set(process: &Handle) -> Option<u64> {
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: `counters` is `cb` bytes.
    unsafe { GetProcessMemoryInfo(process.0, &mut counters, counters.cb) }.ok()?;
    Some(counters.WorkingSetSize as u64)
}

fn sid_string(sid: PSID) -> Option<String> {
    let mut text = PWSTR::null();
    // SAFETY: `sid` is valid; Windows allocates `text`, freed below with LocalFree.
    unsafe { ConvertSidToStringSidW(sid, &mut text) }.ok()?;
    // SAFETY: on success `text` is a NUL-terminated wide string.
    let value = unsafe { text.to_string() }.ok();
    // SAFETY: `text` was allocated by LocalAlloc inside ConvertSidToStringSidW.
    unsafe {
        LocalFree(Some(HLOCAL(text.0.cast())));
    }
    value
}

fn account_name(sid: PSID) -> Option<String> {
    let mut name = vec![0u16; 256];
    let mut domain = vec![0u16; 256];
    let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
    let mut kind = SID_NAME_USE::default();
    // SAFETY: both buffers hold the lengths passed; Windows updates the lengths.
    unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut kind,
        )
    }
    .ok()?;
    let name = String::from_utf16_lossy(&name[..name_len as usize]);
    let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
    Some(if domain.is_empty() {
        name
    } else {
        format!("{domain}\\{name}")
    })
}

/// The visible top-level windows that belong to `pid` and have no owner window (the windows
/// Task Manager's "End task" closes).
pub(crate) fn top_level_windows(pid: u32) -> Vec<HWND> {
    struct Search {
        pid: u32,
        found: Vec<HWND>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Search` passed below, alive for the whole enumeration.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut owner_pid = 0u32;
        // SAFETY: plain window queries on a handle Windows passed in.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut owner_pid));
            if owner_pid == search.pid
                && IsWindowVisible(hwnd).as_bool()
                && GetWindow(hwnd, GW_OWNER).is_err()
            {
                search.found.push(hwnd);
            }
        }
        BOOL::from(true)
    }
    let mut search = Search {
        pid,
        found: Vec::new(),
    };
    // SAFETY: the callback only touches `search`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
    search.found
}

/// Closes processes of the current user: `WM_CLOSE` to their windows, then `TerminateProcess`.
pub struct WinProcessControl;

fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl ProcessControl for WinProcessControl {
    fn is_running(&self, pid: u32, name: &str) -> bool {
        if pid == IDLE_PID || pid == SYSTEM_PID {
            return false;
        }
        if let Some(h) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE)
        {
            // SAFETY: valid handle with SYNCHRONIZE access; a zero timeout only polls.
            if unsafe { WaitForSingleObject(h.0, 0) } != WAIT_TIMEOUT {
                return false; // exited; the object lives on only while handles are open
            }
            if let Some(path) = image_path(&h) {
                return same_name(file_name(&path), name);
            }
        }
        // Windows refused a handle: check the process list instead.
        process_entries()
            .iter()
            .any(|e| e.pid == pid && same_name(&e.name, name))
    }

    fn signal(&mut self, pid: u32, signal: CloseSignal) -> Result<(), String> {
        if pid == IDLE_PID || pid == SYSTEM_PID {
            return Err("refusing to close the Windows kernel".to_string());
        }
        match signal {
            CloseSignal::Graceful => {
                let windows = top_level_windows(pid);
                if windows.is_empty() {
                    return Err(
                        "it has no window to close, so only a forced close can end it".to_string(),
                    );
                }
                for hwnd in windows {
                    // SAFETY: posting a message to a window handle from EnumWindows; a window
                    // that closed meanwhile only makes the call fail.
                    unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            CloseSignal::Force => {
                let h = open_process(pid, PROCESS_TERMINATE)
                    .ok_or_else(|| std::io::Error::last_os_error().to_string())?;
                // SAFETY: valid handle with PROCESS_TERMINATE access.
                unsafe { TerminateProcess(h.0, 1) }.map_err(|e| e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Live checks against this Windows machine. They only assert invariants, never exact values.

    use super::*;

    #[test]
    fn live_snapshot_is_consistent() {
        let mut source = WinSource::new().expect("Windows source");
        source.sample(false).expect("first sample");
        let sample = source.sample(true).expect("sample");
        let m = &sample.memory;
        assert!(m.total_bytes > 1 << 30);
        assert_eq!(m.used_bytes + m.available_bytes, m.total_bytes);
        assert!(m.commit_bytes.unwrap() <= m.commit_limit_bytes.unwrap());
        assert!(sample.swap.used_bytes <= sample.swap.total_bytes);
        assert!(sample.cpu.logical_cores > 0);
        assert!(sample.os_pressure.is_some());
        let processes = sample.processes.expect("processes");
        assert!(processes.len() > 10);
        // The test's own process is readable with its private working set and its user.
        let me = std::process::id();
        let own = processes.iter().find(|p| p.pid == me).expect("own process");
        assert_eq!(
            own.memory.as_ref().unwrap().kind,
            MemoryKind::PrivateWorkingSet
        );
        assert!(own.user.is_some());
        assert!(own.exe.is_some());
        assert!(processes.iter().any(|p| p.pid == SYSTEM_PID));
    }

    #[test]
    fn repeated_scans_reuse_cached_paths() {
        let mut source = WinSource::new().expect("Windows source");
        let first = source.sample(true).expect("sample").processes.unwrap();
        assert!(!source.known.is_empty());
        let second = source.sample(true).expect("sample").processes.unwrap();
        let me = std::process::id();
        let exe = |list: &[Process]| {
            list.iter()
                .find(|p| p.pid == me)
                .and_then(|p| p.exe.clone())
        };
        assert!(exe(&first).is_some());
        assert_eq!(exe(&first), exe(&second));
    }

    #[test]
    fn kernel_pids_are_never_signalled() {
        let mut control = WinProcessControl;
        assert!(control.signal(IDLE_PID, CloseSignal::Force).is_err());
        assert!(control.signal(SYSTEM_PID, CloseSignal::Graceful).is_err());
        assert!(!control.is_running(SYSTEM_PID, "System"));
    }
}
