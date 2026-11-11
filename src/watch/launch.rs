//! Starting the watched command and the per-OS parts of watching it (ADR-023).
//!
//! The child inherits canfit's stdin, stdout and stderr and stays in canfit's process group and
//! console, so it behaves as if run directly: a Ctrl-C typed in the terminal reaches it straight
//! from the terminal.
//!
//! - **Unix:** canfit catches SIGINT, SIGQUIT, SIGTERM and SIGHUP while the child runs, so it can
//!   wait and print the summary. SIGTERM and SIGHUP are always forwarded to the child. SIGINT
//!   and SIGQUIT are forwarded only when canfit's process group is not the terminal's foreground
//!   group (no terminal, or canfit runs in the background): in the foreground, a Ctrl-C typed in
//!   the terminal already reached the child, and forwarding it again would make programs such as
//!   llama-server treat it as a second, forcing Ctrl-C. (The sender of a signal does not tell the
//!   two apart: on macOS a terminal's SIGINT carries the pid of the process that wrote to the
//!   terminal, like a `kill`.)
//! - **Linux:** canfit marks itself a child subreaper, so the command's orphans are adopted by
//!   canfit and stay in the tree; canfit reaps the ones that exit.
//! - **Windows:** the child is put in a job object right after it starts (without limits: canfit
//!   never ends the job), whose process list joins the parent-link tree; a console control
//!   handler keeps canfit alive on Ctrl-C / Ctrl-Break, which the console delivers to the child
//!   itself.
//!
//! Nothing here closes or signals a process on canfit's own initiative.

use std::io;
use std::process::{Child, Command, ExitStatus};

use super::Ending;

/// Why the command could not be started; the exit codes follow shells (`127` not found, `126`
/// found but not runnable).
#[derive(Debug)]
pub enum LaunchError {
    NotFound(String),
    NotExecutable(String),
}

impl LaunchError {
    pub fn exit_code(&self) -> u8 {
        match self {
            LaunchError::NotFound(_) => 127,
            LaunchError::NotExecutable(_) => 126,
        }
    }
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::NotFound(p) => write!(
                f,
                "canfit could not start \"{p}\": no such command or file (check the name and \
                 your PATH)."
            ),
            LaunchError::NotExecutable(detail) => write!(f, "canfit could not start {detail}"),
        }
    }
}

/// The running command.
pub struct Launched {
    child: Child,
    pub pid: u32,
    #[cfg(target_os = "windows")]
    job: Option<windows_job::Job>,
}

/// Starts `command` (program and arguments) with inherited stdio. Installs the signal handling
/// first, so a Ctrl-C right after the start is not lost.
pub fn spawn(command: &[String]) -> Result<Launched, LaunchError> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| LaunchError::NotFound(String::new()))?;
    signals::install();
    #[cfg(target_os = "linux")]
    linux::become_subreaper();
    let child = Command::new(program).args(args).spawn().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            LaunchError::NotFound(program.clone())
        } else {
            LaunchError::NotExecutable(format!("\"{program}\": {e}."))
        }
    })?;
    let pid = child.id();
    signals::set_child(pid);
    #[cfg(target_os = "windows")]
    let job = windows_job::Job::assign(&child);
    #[cfg(target_os = "windows")]
    signals::install_console_handler();
    Ok(Launched {
        child,
        pid,
        #[cfg(target_os = "windows")]
        job,
    })
}

impl Launched {
    /// The command's end, if it has ended.
    pub fn try_wait(&mut self) -> io::Result<Option<Ending>> {
        let ended = self.child.try_wait()?.map(ending);
        if ended.is_some() {
            // The pid is free for reuse from now on: never forward a signal to it again.
            signals::set_child(0);
        }
        Ok(ended)
    }

    /// Process ids the OS itself says belong to the run (the Windows job's members).
    pub fn extra_pids(&self) -> Vec<u32> {
        #[cfg(target_os = "windows")]
        if let Some(job) = &self.job {
            return job.pids();
        }
        Vec::new()
    }

    /// Whether the Windows job object could be set up (always false elsewhere).
    pub fn has_job(&self) -> bool {
        #[cfg(target_os = "windows")]
        return self.job.is_some();
        #[cfg(not(target_os = "windows"))]
        false
    }

    /// Reaps adopted orphans that have exited (Linux; a no-op elsewhere). `pids` are the tree's
    /// members other than the child.
    pub fn reap(&self, pids: &[u32]) {
        #[cfg(target_os = "linux")]
        linux::reap(pids, self.pid);
        #[cfg(not(target_os = "linux"))]
        let _ = pids;
    }
}

/// The process that adopts the command's orphans, when canfit is one (Linux subreaper).
pub fn adopter() -> Option<u32> {
    #[cfg(target_os = "linux")]
    return linux::SUBREAPER
        .load(std::sync::atomic::Ordering::Relaxed)
        .then(std::process::id);
    #[cfg(not(target_os = "linux"))]
    None
}

/// Whether canfit received Ctrl-C or a termination request while the command ran.
pub fn interrupted() -> bool {
    signals::INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed)
}

fn ending(status: ExitStatus) -> Ending {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return Ending::Signaled(signal);
        }
    }
    Ending::Exited(status.code().unwrap_or(1))
}

#[cfg(unix)]
mod signals {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
    static CHILD: AtomicI32 = AtomicI32::new(0);

    pub fn set_child(pid: u32) {
        CHILD.store(i32::try_from(pid).unwrap_or(0), Ordering::SeqCst);
    }

    /// True when canfit's process group is the foreground group of the terminal on stdin,
    /// stderr or stdout: then the terminal's own Ctrl-C reaches the child too.
    fn in_terminal_foreground() -> bool {
        // SAFETY: tcgetpgrp(3) and getpgrp(2) are async-signal-safe and only read.
        unsafe {
            let own = libc::getpgrp();
            [libc::STDIN_FILENO, libc::STDERR_FILENO, libc::STDOUT_FILENO]
                .iter()
                .any(|&fd| libc::tcgetpgrp(fd) == own)
        }
    }

    extern "C" fn on_signal(
        signal: libc::c_int,
        _info: *mut libc::siginfo_t,
        _context: *mut libc::c_void,
    ) {
        INTERRUPTED.store(true, Ordering::SeqCst);
        let child = CHILD.load(Ordering::SeqCst);
        if child <= 0 {
            return;
        }
        let forward = match signal {
            libc::SIGINT | libc::SIGQUIT => !in_terminal_foreground(),
            _ => true,
        };
        if forward {
            // SAFETY: kill(2) is async-signal-safe; `child` is canfit's own child (the pid is
            // cleared as soon as the child has been reaped).
            unsafe {
                libc::kill(child, signal);
            }
        }
    }

    pub fn install() {
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: a zeroed sigaction with a valid handler and an empty mask is well formed;
            // the handler only touches atomics and calls kill(2).
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal
                    as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void)
                    as usize;
                action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod signals {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
    use windows::core::BOOL;

    pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

    pub fn install() {}

    pub fn set_child(_pid: u32) {}

    unsafe extern "system" fn on_control(kind: u32) -> BOOL {
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            // The console delivers the same event to the child; canfit waits for it.
            INTERRUPTED.store(true, Ordering::SeqCst);
            return BOOL(1);
        }
        BOOL(0)
    }

    /// Registered after the child started: a handler routine is not inherited (unlike ignoring
    /// Ctrl-C with a null handler, which would be).
    pub fn install_console_handler() {
        // SAFETY: registering a plain function as a console control handler.
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(on_control), true);
        }
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
mod signals {
    use std::sync::atomic::AtomicBool;

    pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

    pub fn install() {}

    pub fn set_child(_pid: u32) {}
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicBool, Ordering};

    pub static SUBREAPER: AtomicBool = AtomicBool::new(false);

    pub fn become_subreaper() {
        // SAFETY: PR_SET_CHILD_SUBREAPER takes one integer argument.
        let ok = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1 as libc::c_ulong) } == 0;
        SUBREAPER.store(ok, Ordering::Relaxed);
    }

    /// Reaps adopted orphans among `pids` that have exited (never the command itself, which
    /// `Child::try_wait` waits for).
    pub fn reap(pids: &[u32], child: u32) {
        if !SUBREAPER.load(Ordering::Relaxed) {
            return;
        }
        for &pid in pids.iter().filter(|&&p| p != child) {
            let Ok(pid) = i32::try_from(pid) else {
                continue;
            };
            let mut status = 0;
            // SAFETY: waitpid with WNOHANG on a specific pid; fails harmlessly (ECHILD) when the
            // process is not canfit's child.
            unsafe {
                libc::waitpid(pid, &mut status, libc::WNOHANG);
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_job {
    use std::os::windows::io::AsRawHandle as _;
    use std::process::Child;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_PROCESS_ID_LIST,
        JobObjectBasicProcessIdList, QueryInformationJobObject,
    };
    use windows::core::PCWSTR;

    /// Most process ids read from the job at once.
    const MAX_PIDS: usize = 4096;

    /// A job object holding the command and everything it starts. No limits are set, and
    /// closing it does not end its processes.
    pub struct Job(HANDLE);

    impl Job {
        pub fn assign(child: &Child) -> Option<Job> {
            // SAFETY: an anonymous job with default security; the process handle is the child's
            // own, valid while `child` lives.
            unsafe {
                let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
                let process = HANDLE(child.as_raw_handle());
                if AssignProcessToJobObject(job, process).is_err() {
                    let _ = CloseHandle(job);
                    return None;
                }
                Some(Job(job))
            }
        }

        pub fn pids(&self) -> Vec<u32> {
            let header = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList)
                / std::mem::size_of::<usize>();
            let mut buf = vec![0usize; header + MAX_PIDS];
            let bytes = u32::try_from(buf.len() * std::mem::size_of::<usize>()).unwrap_or(0);
            // SAFETY: `buf` is large enough and aligned for the list header and MAX_PIDS ids.
            unsafe {
                if QueryInformationJobObject(
                    Some(self.0),
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr().cast(),
                    bytes,
                    None,
                )
                .is_err()
                {
                    return Vec::new();
                }
                let list = &*(buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST);
                let n = (list.NumberOfProcessIdsInList as usize).min(MAX_PIDS);
                buf[header..header + n]
                    .iter()
                    .filter_map(|&pid| u32::try_from(pid).ok())
                    .collect()
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing our own handle; without JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE this
            // leaves the processes running.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}
