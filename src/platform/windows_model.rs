//! Pure conversions from Windows readings to the shared model, and the Windows app grouping hint.
//!
//! No system calls: this module compiles and is tested on every OS; the live readings that feed it
//! are in `windows.rs` (ADR-016). Definitions match Task Manager's Performance → Memory page:
//!
//! - Available = `GlobalMemoryStatusEx.ullAvailPhys` (free, zeroed and standby pages: memory a new
//!   program gets without anything being paged out)
//! - In use = total − available
//! - Committed = `GetPerformanceInfo` commit charge and limit (RAM plus the current page files)
//! - Cached = `GetPerformanceInfo.SystemCache`
//! - Swap = the page files (`EnumPageFilesW`)
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashMap;

use crate::core::{MemoryInfo, OsPressureLevel, Process, Provenance, SwapInfo};

/// `GlobalMemoryStatusEx`, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryStatus {
    pub total_phys: u64,
    pub avail_phys: u64,
}

/// `GetPerformanceInfo`, converted from pages to bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerformanceInfo {
    pub commit_total: u64,
    pub commit_limit: u64,
    pub system_cache: u64,
    pub kernel_nonpaged: u64,
}

/// All page files together (`EnumPageFilesW`), in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageFiles {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

/// Builds the shared memory model. `free_bytes` is PDH's "Free & Zero Page List Bytes" when the
/// counter is readable; `compressed_bytes` is the working set of the "Memory Compression" process
/// when Windows lets canfit read it.
pub fn memory_from_readings(
    status: &MemoryStatus,
    perf: &PerformanceInfo,
    free_bytes: Option<u64>,
    compressed_bytes: Option<u64>,
) -> MemoryInfo {
    let total = status.total_phys;
    let available = status.avail_phys.min(total);
    MemoryInfo {
        total_bytes: total,
        available_bytes: available,
        used_bytes: total - available,
        // Without the PDH counter, available minus the standby cache is the closest reading.
        free_bytes: free_bytes
            .unwrap_or_else(|| available.saturating_sub(perf.system_cache))
            .min(available),
        // Non-paged kernel pool: memory Windows can never page out.
        wired_bytes: Some(perf.kernel_nonpaged),
        app_bytes: None,
        cached_files_bytes: Some(perf.system_cache),
        compressed_bytes,
        compressed_original_bytes: None,
        commit_bytes: Some(perf.commit_total),
        commit_limit_bytes: Some(perf.commit_limit),
        provenance: Provenance::Measured,
    }
}

/// Page files are Windows' swap.
pub fn swap_from_page_files(page_files: &PageFiles) -> SwapInfo {
    let used = page_files.used_bytes.min(page_files.total_bytes);
    SwapInfo {
        total_bytes: page_files.total_bytes,
        used_bytes: used,
        free_bytes: page_files.total_bytes - used,
        swapped_out_total_bytes: None,
        encrypted: None,
        provenance: Provenance::Measured,
    }
}

/// Maps the low-memory resource notification (`QueryMemoryResourceNotification`): Windows
/// signals it only when physical memory is nearly exhausted, so it means critical.
pub fn os_pressure_from_low_memory(low: Option<bool>) -> Option<OsPressureLevel> {
    low.map(|low| {
        if low {
            OsPressureLevel::Critical
        } else {
            OsPressureLevel::Normal
        }
    })
}

/// The file name of a Windows (or Unix) path.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// The folder of a path, without the trailing separator.
fn parent_dir(path: &str) -> Option<&str> {
    let cut = path.rfind(['\\', '/'])?;
    Some(&path[..cut]).filter(|d| !d.is_empty())
}

fn same_path(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// True when `path` is strictly inside `dir`.
fn is_inside(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches(['\\', '/']);
    path.len() > dir.len() + 1
        && path.is_char_boundary(dir.len())
        && path[..dir.len()].eq_ignore_ascii_case(dir)
        && matches!(path.as_bytes()[dir.len()], b'\\' | b'/')
}

/// Number of components of a folder path (`C:\Program Files\Docker` has 3).
fn depth(dir: &str) -> usize {
    dir.split(['\\', '/']).filter(|c| !c.is_empty()).count()
}

/// Folders shallower than this (`C:\Program Files`, `C:\Users`) hold many unrelated programs, so
/// a parent there never collects children from its subfolders.
const MIN_APP_DIR_DEPTH: usize = 3;

/// Whether `child` is a helper of `parent`: started by it, after it (Windows reuses process ids
/// and never re-parents orphans), from the parent's own install folder or a subfolder of it.
/// Programs that ship with Windows (`system_root`) never collect children, so Notepad started
/// from Explorer stays its own app.
fn is_helper(
    parent: &Process,
    child: &Process,
    started: &HashMap<u32, u64>,
    system_root: &str,
) -> bool {
    let (Some(parent_exe), Some(child_exe)) = (parent.exe.as_deref(), child.exe.as_deref()) else {
        return false;
    };
    let (Some(parent_start), Some(child_start)) =
        (started.get(&parent.pid), started.get(&child.pid))
    else {
        return false;
    };
    if parent.pid == child.pid || parent_start > child_start {
        return false;
    }
    let (Some(parent_dir), Some(child_dir)) = (parent_dir(parent_exe), parent_dir(child_exe))
    else {
        return false;
    };
    if same_path(parent_dir, system_root) || is_inside(parent_dir, system_root) {
        return false;
    }
    same_path(child_dir, parent_dir)
        || (depth(parent_dir) >= MIN_APP_DIR_DEPTH && is_inside(child_dir, parent_dir))
}

/// Sets each process's `app_bundle` to the executable path of its app's root process: the
/// furthest ancestor reachable through [`is_helper`] links (the process itself when it has no
/// such parent). Processes whose executable is unreadable get no hint and are grouped by name.
/// `started` holds each readable process's creation time.
pub fn assign_apps(processes: &mut [Process], started: &HashMap<u32, u64>, system_root: &str) {
    let index: HashMap<u32, usize> = processes
        .iter()
        .enumerate()
        .map(|(i, p)| (p.pid, i))
        .collect();
    let roots: Vec<Option<String>> = (0..processes.len())
        .map(|i| {
            processes[i].exe.as_ref()?;
            let mut current = i;
            // Bounded walk: a corrupt table cannot loop forever.
            for _ in 0..processes.len() {
                let parent = processes[current]
                    .ppid
                    .filter(|&ppid| ppid != 0)
                    .and_then(|ppid| index.get(&ppid).copied());
                match parent {
                    Some(p)
                        if is_helper(&processes[p], &processes[current], started, system_root) =>
                    {
                        current = p
                    }
                    _ => break,
                }
            }
            processes[current].exe.clone()
        })
        .collect();
    for (process, root) in processes.iter_mut().zip(roots) {
        process.app_bundle = root;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::group::group_processes;
    use crate::core::{GroupKind, MemoryKind, ProcessMemory};

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    const ROOT: &str = r"C:\Windows";

    #[test]
    fn memory_matches_task_manager_definitions() {
        let status = MemoryStatus {
            total_phys: 16 * GIB,
            avail_phys: 9 * GIB,
        };
        let perf = PerformanceInfo {
            commit_total: 12 * GIB,
            commit_limit: 18 * GIB,
            system_cache: 5 * GIB,
            kernel_nonpaged: 300 * MIB,
        };
        let m = memory_from_readings(&status, &perf, Some(2 * GIB), None);
        assert_eq!(m.used_bytes, 7 * GIB);
        assert_eq!(m.available_bytes, 9 * GIB);
        assert_eq!(m.free_bytes, 2 * GIB);
        assert_eq!(m.commit_bytes, Some(12 * GIB));
        assert_eq!(m.commit_limit_bytes, Some(18 * GIB));
        assert_eq!(m.cached_files_bytes, Some(5 * GIB));
        assert_eq!(m.wired_bytes, Some(300 * MIB));
        assert_eq!(m.compressed_bytes, None);
        // Without the PDH counter, free memory is estimated from available minus the cache.
        let m = memory_from_readings(&status, &perf, None, Some(GIB));
        assert_eq!(m.free_bytes, 4 * GIB);
        assert_eq!(m.compressed_bytes, Some(GIB));
    }

    #[test]
    fn readings_are_clamped() {
        let status = MemoryStatus {
            total_phys: 4 * GIB,
            avail_phys: 5 * GIB,
        };
        let m = memory_from_readings(&status, &PerformanceInfo::default(), Some(8 * GIB), None);
        assert_eq!(m.available_bytes, 4 * GIB);
        assert_eq!(m.used_bytes, 0);
        assert_eq!(m.free_bytes, 4 * GIB);
        let s = swap_from_page_files(&PageFiles {
            total_bytes: GIB,
            used_bytes: 2 * GIB,
        });
        assert_eq!((s.used_bytes, s.free_bytes), (GIB, 0));
    }

    #[test]
    fn page_files_are_swap() {
        let s = swap_from_page_files(&PageFiles {
            total_bytes: 4 * GIB,
            used_bytes: GIB,
        });
        assert_eq!(s.total_bytes, 4 * GIB);
        assert_eq!(s.free_bytes, 3 * GIB);
        assert_eq!(s.swapped_out_total_bytes, None);
    }

    #[test]
    fn low_memory_notification_is_critical() {
        assert_eq!(
            os_pressure_from_low_memory(Some(true)),
            Some(OsPressureLevel::Critical)
        );
        assert_eq!(
            os_pressure_from_low_memory(Some(false)),
            Some(OsPressureLevel::Normal)
        );
        assert_eq!(os_pressure_from_low_memory(None), None);
    }

    #[test]
    fn path_helpers_handle_windows_paths() {
        assert_eq!(
            file_name(r"C:\Program Files\Google\chrome.exe"),
            "chrome.exe"
        );
        assert_eq!(file_name("chrome.exe"), "chrome.exe");
        assert_eq!(parent_dir(r"C:\a\b.exe"), Some(r"C:\a"));
        assert_eq!(parent_dir("b.exe"), None);
        assert!(is_inside(
            r"C:\Program Files\Docker\Docker\resources",
            r"C:\program files\docker\docker"
        ));
        assert!(!is_inside(
            r"C:\Program Files\DockerX",
            r"C:\Program Files\Docker"
        ));
        assert!(!is_inside(r"C:\a", r"C:\a"));
        assert_eq!(depth(r"C:\Program Files\Docker"), 3);
    }

    fn row(pid: u32, ppid: u32, exe: &str) -> Process {
        Process {
            pid,
            ppid: Some(ppid),
            name: file_name(exe).to_string(),
            exe: Some(exe.to_string()),
            user: Some(r"PC\fede".to_string()),
            memory: Some(ProcessMemory {
                bytes: 100 * MIB,
                kind: MemoryKind::PrivateWorkingSet,
                provenance: Provenance::Measured,
            }),
            app_bundle: None,
        }
    }

    /// A Windows desktop with a terminal, a model server and apps with helpers.
    fn table() -> (Vec<Process>, HashMap<u32, u64>) {
        let local = r"C:\Users\fede\AppData\Local";
        let processes = vec![
            row(4, 0, "System"),
            row(700, 600, r"C:\Windows\explorer.exe"),
            row(710, 700, r"C:\Windows\System32\notepad.exe"),
            row(720, 700, r"C:\Windows\notepad.exe"),
            row(
                800,
                700,
                r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.21\WindowsTerminal.exe",
            ),
            row(810, 800, r"C:\Program Files\PowerShell\7\pwsh.exe"),
            row(820, 810, r"C:\tools\llama\llama-server.exe"),
            row(
                900,
                700,
                r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            ),
            row(
                901,
                900,
                r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            ),
            row(
                902,
                900,
                r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            ),
            row(
                1000,
                700,
                r"C:\Program Files\Docker\Docker\Docker Desktop.exe",
            ),
            row(
                1001,
                1000,
                r"C:\Program Files\Docker\Docker\resources\com.docker.backend.exe",
            ),
            row(
                1100,
                700,
                &format!(r"{local}\Programs\Ollama\ollama app.exe"),
            ),
            row(1101, 1100, &format!(r"{local}\Programs\Ollama\ollama.exe")),
            row(
                1102,
                1101,
                &format!(
                    r"{local}\Programs\Ollama\lib\ollama\runners\cuda\ollama_llama_server.exe"
                ),
            ),
            // A shallow parent folder never collects children from its subfolders.
            row(1200, 700, r"C:\Program Files\launcher.exe"),
            row(1201, 1200, r"C:\Program Files\Other\other.exe"),
            // The parent id was reused by a process started after this one.
            row(1300, 700, r"C:\Games\Steam\steam.exe"),
            row(1301, 1300, r"C:\Games\Steam\bin\steamwebhelper.exe"),
        ];
        let mut started: HashMap<u32, u64> = processes
            .iter()
            .map(|p| (p.pid, u64::from(p.pid)))
            .collect();
        started.insert(1300, 5000);
        // The kernel's executable is never readable.
        started.remove(&4);
        let mut processes = processes;
        processes[0].exe = None;
        (processes, started)
    }

    fn app_of(processes: &[Process], pid: u32) -> Option<&str> {
        processes
            .iter()
            .find(|p| p.pid == pid)
            .and_then(|p| p.app_bundle.as_deref())
    }

    #[test]
    fn helpers_from_the_install_folder_join_their_app() {
        let (mut processes, started) = table();
        assign_apps(&mut processes, &started, ROOT);
        let chrome = r"C:\Program Files\Google\Chrome\Application\chrome.exe";
        assert_eq!(app_of(&processes, 901), Some(chrome));
        assert_eq!(app_of(&processes, 902), Some(chrome));
        let docker = r"C:\Program Files\Docker\Docker\Docker Desktop.exe";
        assert_eq!(app_of(&processes, 1001), Some(docker));
        // Two levels deep, through a helper.
        assert!(
            app_of(&processes, 1102)
                .unwrap()
                .ends_with(r"Ollama\ollama app.exe")
        );
    }

    #[test]
    fn unrelated_children_stay_their_own_app() {
        let (mut processes, started) = table();
        assign_apps(&mut processes, &started, ROOT);
        // Windows' own programs never collect children: Notepad is not part of Explorer.
        assert_eq!(
            app_of(&processes, 710),
            Some(r"C:\Windows\System32\notepad.exe")
        );
        assert_eq!(app_of(&processes, 720), Some(r"C:\Windows\notepad.exe"));
        // A model server started from a terminal is not part of the terminal.
        assert_eq!(
            app_of(&processes, 820),
            Some(r"C:\tools\llama\llama-server.exe")
        );
        assert_eq!(
            app_of(&processes, 810),
            Some(r"C:\Program Files\PowerShell\7\pwsh.exe")
        );
        assert_eq!(
            app_of(&processes, 1201),
            Some(r"C:\Program Files\Other\other.exe")
        );
        // Reused parent id: the helper started before its "parent".
        assert_eq!(
            app_of(&processes, 1301),
            Some(r"C:\Games\Steam\bin\steamwebhelper.exe")
        );
        // No readable executable: no hint.
        assert_eq!(app_of(&processes, 4), None);
    }

    #[test]
    fn groups_are_named_after_the_root_executable() {
        let (mut processes, started) = table();
        assign_apps(&mut processes, &started, ROOT);
        let groups = group_processes(&processes);
        let find = |name: &str| {
            groups
                .iter()
                .find(|g| g.name == name)
                .unwrap_or_else(|| panic!("{name}"))
        };
        assert_eq!(find("chrome").process_count, 3);
        assert_eq!(find("chrome").kind, GroupKind::AppBundle);
        assert_eq!(find("Docker Desktop").process_count, 2);
        assert_eq!(find("ollama app").process_count, 3);
        assert_eq!(find("llama-server").process_count, 1);
        assert_eq!(find("System").kind, GroupKind::Executable);
    }

    #[test]
    fn cyclic_tables_terminate() {
        let mut processes = vec![
            row(1, 2, r"C:\Apps\Thing\a.exe"),
            row(2, 1, r"C:\Apps\Thing\b.exe"),
        ];
        let started = HashMap::from([(1, 0), (2, 0)]);
        assign_apps(&mut processes, &started, ROOT);
        assert!(processes.iter().all(|p| p.app_bundle.is_some()));
    }
}
