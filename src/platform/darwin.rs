//! Pure conversions from macOS kernel counters to the shared model.
//!
//! This module has no system calls, so it compiles and is tested on every OS; the live readings
//! that feed it are in `macos.rs`. Formulas match Activity Monitor's Memory tab:
//!
//! - App Memory = anonymous (internal) pages − purgeable pages
//! - Memory Used = App Memory + Wired + Compressed (physical pages held by the compressor)
//! - Cached Files = file-backed (external) pages + purgeable pages
//! - Available = total − Memory Used (free memory plus caches macOS drops on its own)
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use serde::Deserialize;

use crate::core::{GpuMemoryLimit, MemoryInfo, OsPressureLevel, Provenance, SwapInfo};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Page counts from `host_statistics64(HOST_VM_INFO64)`, named like `vm_stat`'s output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct VmCounters {
    pub page_size: u64,
    /// "Pages free" as `vm_stat` prints it: the kernel's `free_count − speculative_count`.
    pub free: u64,
    pub active: u64,
    pub inactive: u64,
    pub speculative: u64,
    pub wired: u64,
    pub purgeable: u64,
    /// "File-backed pages".
    pub external: u64,
    /// "Anonymous pages".
    pub internal: u64,
    /// "Pages occupied by compressor" (physical).
    pub compressor_occupied: u64,
    /// "Pages stored in compressor" (logical, before compression; includes swapped-out data).
    pub compressor_stored: u64,
    /// "Swapouts" (cumulative pages written to swap since boot).
    pub swapouts: u64,
}

/// `vm.swapusage` (`struct xsw_usage`), in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct SwapUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub encrypted: bool,
}

/// Builds the shared memory model from raw counters and `hw.memsize`.
pub fn memory_from_counters(total_bytes: u64, c: &VmCounters) -> MemoryInfo {
    let ps = c.page_size;
    let app = c.internal.saturating_sub(c.purgeable) * ps;
    let wired = c.wired * ps;
    let compressed = c.compressor_occupied * ps;
    let used = (app + wired + compressed).min(total_bytes);
    MemoryInfo {
        total_bytes,
        available_bytes: total_bytes - used,
        used_bytes: used,
        free_bytes: c.free * ps,
        wired_bytes: Some(wired),
        app_bytes: Some(app),
        cached_files_bytes: Some((c.external + c.purgeable) * ps),
        compressed_bytes: Some(compressed),
        compressed_original_bytes: Some(c.compressor_stored * ps),
        commit_bytes: None,
        commit_limit_bytes: None,
        provenance: Provenance::Measured,
    }
}

pub fn swap_from_usage(usage: &SwapUsage, c: &VmCounters) -> SwapInfo {
    SwapInfo {
        total_bytes: usage.total_bytes,
        used_bytes: usage.used_bytes,
        free_bytes: usage.free_bytes,
        swapped_out_total_bytes: Some(c.swapouts * c.page_size),
        encrypted: Some(usage.encrypted),
        provenance: Provenance::Measured,
    }
}

/// Maps `kern.memorystatus_vm_pressure_level` (1 normal, 2 warning, 4 critical).
pub fn os_pressure_from_sysctl(value: i64) -> Option<OsPressureLevel> {
    match value {
        1 => Some(OsPressureLevel::Normal),
        2 => Some(OsPressureLevel::Warning),
        4 => Some(OsPressureLevel::Critical),
        _ => None,
    }
}

/// Machines with more memory than this get the larger default GPU share.
pub const GPU_LARGE_MACHINE_BYTES: u64 = 36 * GIB;

/// GPU-usable memory on Apple Silicon (spec §3.1): `iogpu.wired_limit_mb` when the user set it
/// (non-zero), otherwise the macOS default of ~75 % of memory above 36 GB and ~2/3 at or below.
pub fn gpu_memory_limit(total_bytes: u64, wired_limit_mb: Option<u64>) -> GpuMemoryLimit {
    match wired_limit_mb {
        Some(mb) if mb > 0 => GpuMemoryLimit {
            bytes: (mb * MIB).min(total_bytes),
            provenance: Provenance::ReadFromConfig,
            source: "iogpu.wired_limit_mb".to_string(),
        },
        _ if total_bytes > GPU_LARGE_MACHINE_BYTES => GpuMemoryLimit {
            bytes: total_bytes / 4 * 3,
            provenance: Provenance::Estimated,
            source: "macOS default: about 3/4 of memory above 36 GB".to_string(),
        },
        _ => GpuMemoryLimit {
            bytes: total_bytes / 3 * 2,
            provenance: Provenance::Estimated,
            source: "macOS default: about 2/3 of memory up to 36 GB".to_string(),
        },
    }
}

#[cfg(test)]
pub use fixtures::{Fixture, raw_sample_from_fixture};

#[cfg(test)]
mod fixtures {
    //! Recorded and synthesized macOS readings (`tests/fixtures/macos/*.json`).

    use std::time::Instant;

    use serde::Deserialize;

    use super::*;
    use crate::core::sampler::RawSample;
    use crate::core::{CpuInfo, PlatformInfo};

    #[derive(Debug, Deserialize)]
    pub struct Fixture {
        pub total_bytes: u64,
        pub kernel_pressure_level: i64,
        pub iogpu_wired_limit_mb: Option<u64>,
        pub samples: Vec<FixtureSample>,
        pub expected: Expected,
    }

    #[derive(Debug, Deserialize)]
    pub struct FixtureSample {
        pub elapsed_ms: u64,
        pub vm: VmCounters,
        pub swap: SwapUsage,
    }

    #[derive(Debug, Deserialize)]
    pub struct Expected {
        pub used_bytes: Option<u64>,
        pub available_bytes: Option<u64>,
    }

    pub fn raw_sample_from_fixture(
        fixture: &Fixture,
        sample: &FixtureSample,
        taken_at: Instant,
    ) -> RawSample {
        RawSample {
            taken_at,
            timestamp_unix_ms: sample.elapsed_ms,
            os_name: "macOS",
            platform: PlatformInfo {
                os: "macos".to_string(),
                os_version: None,
                arch: "aarch64".to_string(),
                unified_memory: true,
            },
            memory: memory_from_counters(fixture.total_bytes, &sample.vm),
            swap: swap_from_usage(&sample.swap, &sample.vm),
            cpu: CpuInfo {
                logical_cores: 10,
                physical_cores: Some(10),
                brand: None,
                usage_percent: None,
                load_average: None,
                provenance: Provenance::Measured,
            },
            os_pressure: os_pressure_from_sysctl(fixture.kernel_pressure_level),
            hard_faults_per_sec: None,
            stall: None,
            gpu_memory_limit: Some(gpu_memory_limit(
                fixture.total_bytes,
                fixture.iogpu_wired_limit_mb,
            )),
            processes: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_matches_activity_monitor_formulas() {
        // `vm_stat` recorded on a 16 GB Apple Silicon Mac (see tests/fixtures/macos/calm.json).
        let c = VmCounters {
            page_size: 16384,
            free: 4076,
            active: 388_175,
            inactive: 348_484,
            speculative: 39006,
            wired: 168_455,
            purgeable: 7312,
            external: 209_404,
            internal: 566_261,
            compressor_occupied: 47910,
            compressor_stored: 832_053,
            swapouts: 1_024_121,
        };
        let m = memory_from_counters(17_179_869_184, &c);
        assert_eq!(m.app_bytes, Some((566_261 - 7312) * 16384));
        assert_eq!(m.wired_bytes, Some(168_455 * 16384));
        assert_eq!(m.compressed_bytes, Some(47910 * 16384));
        assert_eq!(m.used_bytes, (558_949 + 168_455 + 47910) * 16384);
        assert_eq!(m.available_bytes, m.total_bytes - m.used_bytes);
        assert_eq!(m.cached_files_bytes, Some((209_404 + 7312) * 16384));
        assert_eq!(m.free_bytes, 4076 * 16384);
    }

    #[test]
    fn used_never_exceeds_total() {
        let c = VmCounters {
            page_size: 4096,
            internal: 10_000,
            wired: 10_000,
            ..Default::default()
        };
        let m = memory_from_counters(4096 * 100, &c);
        assert_eq!(m.used_bytes, m.total_bytes);
        assert_eq!(m.available_bytes, 0);
    }

    #[test]
    fn kernel_pressure_levels_map_to_the_shared_enum() {
        assert_eq!(os_pressure_from_sysctl(1), Some(OsPressureLevel::Normal));
        assert_eq!(os_pressure_from_sysctl(2), Some(OsPressureLevel::Warning));
        assert_eq!(os_pressure_from_sysctl(4), Some(OsPressureLevel::Critical));
        assert_eq!(os_pressure_from_sysctl(0), None);
    }

    #[test]
    fn gpu_limit_prefers_the_user_setting() {
        let limit = gpu_memory_limit(64 * GIB, Some(57344));
        assert_eq!(limit.bytes, 56 * GIB);
        assert_eq!(limit.provenance, Provenance::ReadFromConfig);
    }

    #[test]
    fn gpu_limit_defaults_depend_on_machine_size() {
        let small = gpu_memory_limit(16 * GIB, Some(0));
        assert_eq!(small.provenance, Provenance::Estimated);
        assert_eq!(small.bytes, 16 * GIB / 3 * 2);
        assert_eq!(gpu_memory_limit(36 * GIB, None).bytes, 24 * GIB);
        assert_eq!(gpu_memory_limit(64 * GIB, None).bytes, 48 * GIB);
    }
}
