//! A short ring buffer of samples, so trends such as "swap is growing" are available.
//!
//! Backends produce [`RawSample`]s; the sampler remembers the last few and turns each one into a
//! complete [`Snapshot`] with a swap trend, app groups and the unified pressure index.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::pressure::{self, PressureInputs, SWAP_GROWING_BYTES_PER_SEC};
use super::{
    CpuInfo, GpuMemoryLimit, MemoryInfo, OsPressureLevel, PlatformInfo, Process, Provenance,
    Snapshot, StallInfo, SwapInfo, SwapTrend, group,
};

/// What a platform backend reads in one pass, before shared interpretation.
#[derive(Debug, Clone)]
pub struct RawSample {
    /// Monotonic time of the sample, used for rates.
    pub taken_at: Instant,
    pub timestamp_unix_ms: u64,
    /// Human name of the OS for messages, e.g. "macOS".
    pub os_name: &'static str,
    pub platform: PlatformInfo,
    pub memory: MemoryInfo,
    pub swap: SwapInfo,
    pub cpu: CpuInfo,
    pub os_pressure: Option<OsPressureLevel>,
    /// Hard page faults per second since the previous sample (Windows).
    pub hard_faults_per_sec: Option<f64>,
    /// Pressure-stall readings (Linux PSI).
    pub stall: Option<StallInfo>,
    pub gpu_memory_limit: Option<GpuMemoryLimit>,
    /// `None` for a light sample taken only to establish a trend.
    pub processes: Option<Vec<Process>>,
}

#[derive(Debug, Clone, Copy)]
struct TrendPoint {
    at: Instant,
    swap_used_bytes: u64,
    swapped_out_total_bytes: Option<u64>,
}

/// Samples shorter apart than this give no meaningful rate.
const MIN_TREND_SPAN: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub struct Sampler {
    points: VecDeque<TrendPoint>,
    capacity: usize,
    window: Duration,
}

impl Sampler {
    /// Keeps at most `capacity` samples and computes trends over the last `window`.
    pub fn new(capacity: usize, window: Duration) -> Self {
        Sampler {
            points: VecDeque::with_capacity(capacity.max(2)),
            capacity: capacity.max(2),
            window,
        }
    }

    /// Records a sample and returns the explained snapshot.
    pub fn record(&mut self, sample: RawSample) -> Snapshot {
        if self.points.len() == self.capacity {
            self.points.pop_front();
        }
        self.points.push_back(TrendPoint {
            at: sample.taken_at,
            swap_used_bytes: sample.swap.used_bytes,
            swapped_out_total_bytes: sample.swap.swapped_out_total_bytes,
        });
        let swap_trend = self.swap_trend();

        let pressure = pressure::compute(&PressureInputs {
            os_name: sample.os_name,
            total_bytes: sample.memory.total_bytes,
            available_bytes: sample.memory.available_bytes,
            compressed_bytes: sample.memory.compressed_bytes,
            commit: sample
                .memory
                .commit_bytes
                .zip(sample.memory.commit_limit_bytes),
            hard_faults_per_sec: sample.hard_faults_per_sec,
            memory_stall: sample.stall.as_ref().and_then(|s| s.memory),
            os_level: sample.os_pressure,
            swap_trend: swap_trend.as_ref(),
        });

        let processes = sample.processes.unwrap_or_default();
        let app_groups = group::group_processes(&processes);

        Snapshot {
            timestamp_unix_ms: sample.timestamp_unix_ms,
            platform: sample.platform,
            memory: sample.memory,
            swap: sample.swap,
            swap_trend,
            cpu: sample.cpu,
            os_pressure: sample.os_pressure,
            hard_faults_per_sec: sample.hard_faults_per_sec,
            stall: sample.stall,
            pressure,
            gpu_memory_limit: sample.gpu_memory_limit,
            app_groups,
            processes,
        }
    }

    /// Swap change between the oldest sample inside the window and the latest one.
    pub fn swap_trend(&self) -> Option<SwapTrend> {
        let latest = *self.points.back()?;
        let oldest = self
            .points
            .iter()
            .find(|p| latest.at.duration_since(p.at) <= self.window)?;
        let span = latest.at.duration_since(oldest.at);
        if span < MIN_TREND_SPAN {
            return None;
        }
        let secs = span.as_secs_f64();
        let used_change = (latest.swap_used_bytes as f64 - oldest.swap_used_bytes as f64) / secs;
        let swap_out = match (
            latest.swapped_out_total_bytes,
            oldest.swapped_out_total_bytes,
        ) {
            (Some(new), Some(old)) => Some(new.saturating_sub(old) as f64 / secs),
            _ => None,
        };
        // Swap-out traffic counts while used swap holds steady (the OS churning pages out and
        // back in), but never while swap is shrinking: when a large process exits, its swap is
        // freed even though the cumulative swap-out counter still ticks.
        let shrinking = used_change <= -SWAP_GROWING_BYTES_PER_SEC;
        let growing = used_change >= SWAP_GROWING_BYTES_PER_SEC
            || (!shrinking && swap_out.is_some_and(|rate| rate >= SWAP_GROWING_BYTES_PER_SEC));
        Some(SwapTrend {
            growing,
            used_change_bytes_per_sec: used_change,
            swap_out_bytes_per_sec: swap_out,
            window_secs: secs,
            provenance: Provenance::Measured,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::{PressureLevel, PressureSignal};
    use crate::platform::darwin::{self, Fixture};

    fn fixture(name: &str) -> Fixture {
        let path = format!(
            "{}/tests/fixtures/macos/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// Replays a recorded or synthesized fixture through the macOS conversion, the sampler and
    /// the pressure index, and returns the last snapshot.
    pub(crate) fn replay(name: &str) -> (Fixture, Snapshot) {
        let fixture = fixture(name);
        let start = Instant::now();
        let mut sampler = Sampler::new(8, Duration::from_secs(10));
        let mut last = None;
        for sample in &fixture.samples {
            let raw = darwin::raw_sample_from_fixture(
                &fixture,
                sample,
                start + Duration::from_millis(sample.elapsed_ms),
            );
            last = Some(sampler.record(raw));
        }
        (fixture, last.expect("fixture has samples"))
    }

    fn check(name: &str, level: PressureLevel, signal: PressureSignal) -> Snapshot {
        let (fixture, snapshot) = replay(name);
        assert_eq!(
            snapshot.pressure.level, level,
            "{name}: {:?}",
            snapshot.pressure
        );
        assert_eq!(
            snapshot.pressure.main_reason.signal, signal,
            "{name}: {:?}",
            snapshot.pressure
        );
        if let Some(expected) = fixture.expected.used_bytes {
            assert_eq!(snapshot.memory.used_bytes, expected, "{name}: used");
        }
        if let Some(expected) = fixture.expected.available_bytes {
            assert_eq!(
                snapshot.memory.available_bytes, expected,
                "{name}: available"
            );
        }
        snapshot
    }

    #[test]
    fn calm_fixture_recorded_on_a_real_mac() {
        let s = check("calm", PressureLevel::Calm, PressureSignal::None);
        // Old swap in use is harmless while it is not growing.
        assert!(s.swap.used_bytes > 4 * 1024 * 1024 * 1024);
        assert!(!s.swap_trend.as_ref().unwrap().growing);
    }

    #[test]
    fn compressing_fixture_is_strained_by_compression() {
        let s = check(
            "compressing",
            PressureLevel::Strained,
            PressureSignal::Compression,
        );
        assert!(s.pressure.main_reason.message.contains("compressing"));
    }

    #[test]
    fn swapping_fixture_is_strained_by_growing_swap() {
        let s = check(
            "swapping",
            PressureLevel::Strained,
            PressureSignal::SwapGrowing,
        );
        assert!(s.swap_trend.as_ref().unwrap().growing);
    }

    #[test]
    fn critical_fixture_is_critical() {
        let s = check("critical", PressureLevel::Critical, PressureSignal::OsLevel);
        assert!(s.pressure.main_reason.message.contains("critical"));
    }

    #[test]
    fn shrinking_swap_is_not_growing_even_with_swap_out_traffic() {
        // Recorded under load on 2026-11-09: a 7 GB process exited, used swap fell by 3.5 GB in
        // one second while the swap-out counter still advanced.
        let fixture = fixture("calm");
        let start = Instant::now();
        let mut sampler = Sampler::new(4, Duration::from_secs(10));
        let mut first = darwin::raw_sample_from_fixture(&fixture, &fixture.samples[0], start);
        first.swap.used_bytes = 9_720 * 1024 * 1024;
        first.swap.swapped_out_total_bytes = Some(0);
        let mut second = darwin::raw_sample_from_fixture(
            &fixture,
            &fixture.samples[0],
            start + Duration::from_secs(1),
        );
        second.swap.used_bytes = 6_250 * 1024 * 1024;
        second.swap.swapped_out_total_bytes = Some(105 * 1024 * 1024);
        sampler.record(first);
        let snapshot = sampler.record(second);
        let trend = snapshot.swap_trend.unwrap();
        assert!(!trend.growing);
        assert!(trend.used_change_bytes_per_sec < 0.0);
        assert_eq!(snapshot.pressure.level, PressureLevel::Calm);
    }

    #[test]
    fn a_single_sample_has_no_trend() {
        let mut sampler = Sampler::new(4, Duration::from_secs(10));
        let fixture = fixture("calm");
        let raw = darwin::raw_sample_from_fixture(&fixture, &fixture.samples[0], Instant::now());
        let snapshot = sampler.record(raw);
        assert!(snapshot.swap_trend.is_none());
    }

    #[test]
    fn the_ring_buffer_keeps_only_recent_samples() {
        let fixture = fixture("calm");
        let start = Instant::now();
        let mut sampler = Sampler::new(3, Duration::from_secs(60));
        for i in 0..10 {
            let raw = darwin::raw_sample_from_fixture(
                &fixture,
                &fixture.samples[0],
                start + Duration::from_secs(i),
            );
            sampler.record(raw);
        }
        assert_eq!(sampler.points.len(), 3);
        assert_eq!(sampler.swap_trend().unwrap().window_secs, 2.0);
    }
}
