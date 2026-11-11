//! App fit check from declared configuration (spec §6.3, §9 source 1; ADR-020).
//!
//! An app target names an app canfit has a **profile** for: a small block in the embedded
//! `profiles.toml` (or the user's `apps.toml`) with the app's names, the per-OS matchers that find
//! it among running app groups, and an ordered list of **config readers**. A reader finds the
//! app's own settings file and reads the memory it is configured to use: Docker Desktop's VM
//! memory, the RAM assigned to each virtual machine, a JVM app's `-Xmx`. Readers never write,
//! never run the app's own tools and fail soft: a file they can't read becomes a note.
//!
//! Apps without a profile (or whose settings can't be read) are estimated from what canfit has
//! measured them use while the monitor was open ([`learned`]), then from the seed [`catalog`]
//! (ADR-021). The order is declared configuration → history → catalog → unknown.
//!
//! Everything here is pure or rooted in an [`Env`], so the readers are tested with fixture home
//! folders for every OS on every OS.

pub mod catalog;
pub mod check;
pub mod env;
pub mod learned;
pub mod profiles;
pub mod readers;
pub mod resolve;
mod xml;

use std::fmt;
use std::path::PathBuf;

use serde::Serialize;

use crate::core::Provenance;

pub use env::{Env, Os};

const MIB: u64 = 1024 * 1024;

/// How much to trust an app's declared memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A documented default canfit applied because the setting is absent.
    Low,
    /// Read from the app's settings, plus an estimated part (a JVM's non-heap memory).
    Medium,
    /// Read from the app's settings: the app is configured to use exactly this.
    High,
}

/// One amount of memory an app is configured to use: Docker Desktop's VM, one virtual machine,
/// a JVM app's heap.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeclaredItem {
    /// What it is: "Docker Desktop VM", the virtual machine's name, "Java heap (-Xmx)".
    pub name: String,
    pub declared_bytes: u64,
    /// `read_from_config` when read from a setting, `estimated` when canfit applied the app's
    /// documented default because the setting is absent.
    pub declared_provenance: Provenance,
    /// Where the value came from, e.g. `memoryMiB = 10240 in settings.json`.
    pub detail: String,
    /// Memory the app needs on top of the declared amount (a JVM's code cache, metaspace and
    /// threads). Always estimated.
    pub overhead_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overhead_detail: Option<String>,
    /// The settings file the value was read from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl DeclaredItem {
    /// Declared memory plus overhead.
    pub fn total_bytes(&self) -> u64 {
        self.declared_bytes.saturating_add(self.overhead_bytes)
    }
}

/// How the items of a reading combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingKind {
    /// One app that uses all its items at once (normally one item).
    App,
    /// Virtual machines: each item is one VM, started on its own.
    VirtualMachines,
}

/// What a config reader found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reading {
    /// The reader that produced it (`docker_desktop`, `utm`, `jetbrains`, ...).
    pub reader: String,
    /// The settings it read, in words: "Docker Desktop settings", "UTM virtual machines".
    pub source: String,
    pub kind: ReadingKind,
    pub items: Vec<DeclaredItem>,
    pub confidence: Confidence,
    /// Assumptions and caveats, one sentence each.
    pub notes: Vec<String>,
}

/// Why a reader produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The app's settings are not on this machine (the app is not installed, or was never
    /// configured).
    NotFound(String),
    /// A settings file exists but canfit could not read the memory from it.
    Unreadable { path: PathBuf, detail: String },
    /// The reader does not support this OS.
    UnsupportedOs,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::NotFound(what) => write!(f, "{what}"),
            ReadError::Unreadable { path, detail } => {
                write!(
                    f,
                    "could not read settings from {}: {detail}",
                    path.display()
                )
            }
            ReadError::UnsupportedOs => write!(f, "this app's settings are not read on this OS"),
        }
    }
}

/// Parses a memory size such as `4GB`, `4096MB`, `2g`, `512m`, `1024k` or a plain byte count.
/// `default_unit` applies to a bare number (WSL: bytes; JVM: bytes).
pub fn parse_size(text: &str, default_unit: u64) -> Option<u64> {
    let t = text.trim().trim_matches('"').trim();
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let unit = unit.trim().to_ascii_lowercase();
    let factor: u64 = match unit.as_str() {
        "" => default_unit,
        "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => MIB,
        "g" | "gb" | "gib" => 1024 * MIB,
        "t" | "tb" | "tib" => 1024 * 1024 * MIB,
        _ => return None,
    };
    if number.is_empty() {
        return None;
    }
    if number.contains('.') {
        let value: f64 = number.parse().ok()?;
        let bytes = value * factor as f64;
        (bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
    } else {
        number.parse::<u64>().ok()?.checked_mul(factor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_accept_the_usual_units() {
        assert_eq!(parse_size("4GB", 1), Some(4 * 1024 * MIB));
        assert_eq!(parse_size("4096MB", 1), Some(4096 * MIB));
        assert_eq!(parse_size("2g", 1), Some(2 * 1024 * MIB));
        assert_eq!(parse_size("750m", 1), Some(750 * MIB));
        assert_eq!(parse_size("1024k", 1), Some(MIB));
        assert_eq!(parse_size("1.5GB", 1), Some(1536 * MIB));
        assert_eq!(parse_size(" \"8GB\" ", 1), Some(8 * 1024 * MIB));
        assert_eq!(parse_size("1048576", 1), Some(MIB));
        assert_eq!(parse_size("8", MIB), Some(8 * MIB));
        for bad in ["", "GB", "4 parsecs", "-1GB", "99999999999999999999"] {
            assert_eq!(parse_size(bad, 1), None, "{bad}");
        }
    }
}
