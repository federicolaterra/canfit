//! Per-OS system backends behind one shared interface.
//!
//! Each backend turns native signals into the shared `core` model. Only the backend for the
//! compilation target is built (pure conversion modules, `darwin`, `windows_model` and
//! `linux_model`, build everywhere so their tests run on every OS). Delivery order: macOS,
//! Windows, Linux (ADR-004). Backends only read; interpretation (trends, grouping, the pressure
//! index) happens in `core`.

use std::fmt;

use crate::core::sampler::RawSample;

pub mod darwin;
pub mod linux_model;
pub mod windows_model;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// How hard to ask a process to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseSignal {
    /// Ask the process to quit (SIGTERM on Unix, `WM_CLOSE` to its windows on Windows); it may
    /// save state, ask, refuse or ignore it.
    Graceful,
    /// End the process immediately (SIGKILL on Unix, `TerminateProcess` on Windows); unsaved
    /// work is lost.
    Force,
}

/// Signals processes. Only the advisor's confirmed closing flow uses it (ADR-013).
pub trait ProcessControl {
    /// True while `pid` exists, is not a zombie and still has the executable name `name`
    /// (a changed name means the pid was reused by another program).
    fn is_running(&self, pid: u32, name: &str) -> bool;
    /// Sends one signal to one process.
    fn signal(&mut self, pid: u32, signal: CloseSignal) -> Result<(), String>;
}

/// The process control for this OS, when canfit can close processes on it (macOS, Windows,
/// Linux).
pub fn process_control() -> Option<Box<dyn ProcessControl>> {
    #[cfg(target_os = "macos")]
    return Some(Box::new(macos::MacProcessControl));
    #[cfg(target_os = "windows")]
    return Some(Box::new(windows::WinProcessControl));
    #[cfg(target_os = "linux")]
    return Some(Box::new(linux::LinuxProcessControl::new()));
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    return None;
}

/// The visible top-level windows of `pid` (tests that close their own windowed child).
#[cfg(all(test, target_os = "windows"))]
pub(crate) fn window_count(pid: u32) -> usize {
    windows::top_level_windows(pid).len()
}

/// A source of system readings for one OS.
pub trait SystemSource {
    /// Reads one sample without admin rights. With `with_processes = false` the per-process scan
    /// is skipped: a light sample used only to establish trends.
    fn sample(&mut self, with_processes: bool) -> Result<RawSample, PlatformError>;
}

#[derive(Debug)]
pub enum PlatformError {
    /// No backend exists for this OS yet.
    #[cfg_attr(
        any(target_os = "macos", target_os = "windows", target_os = "linux"),
        allow(dead_code)
    )]
    Unsupported {
        os: &'static str,
        planned: &'static str,
    },
    /// The OS refused or failed a required reading.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
        allow(dead_code)
    )]
    Read { what: &'static str, detail: String },
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlatformError::Unsupported { os, planned } => write!(
                f,
                "canfit can't read {os} systems yet: support is planned for {planned}. \
                 macOS, Windows and Linux are supported today."
            ),
            PlatformError::Read { what, detail } => {
                write!(f, "canfit could not read {what}: {detail}")
            }
        }
    }
}

impl std::error::Error for PlatformError {}

/// The backend for the OS canfit is running on.
pub fn native() -> Result<Box<dyn SystemSource>, PlatformError> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos::MacSource::new()?));
    #[cfg(target_os = "windows")]
    return Ok(Box::new(windows::WinSource::new()?));
    #[cfg(target_os = "linux")]
    return Ok(Box::new(linux::LinuxSource::new()?));
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    return Err(PlatformError::Unsupported {
        os: std::env::consts::OS,
        planned: "no current milestone",
    });
}
