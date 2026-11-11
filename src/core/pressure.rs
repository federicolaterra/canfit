//! Unified pressure index (spec §7.4, ADR-008).
//!
//! Each OS exposes different raw signals. This shared layer turns them into one 0–100 score, a
//! level (calm / strained / critical) and one main reason in plain language. Every signal is
//! scored on its own; the overall score is the highest signal score, so the main reason always
//! names the signal that drives the level.
//!
//! All thresholds below are **heuristics to calibrate** with real measurements (spec §16). They
//! are recorded in ADR-008 and documented in `docs/open-source/pressure-index.md`; change all
//! three together.

use super::{
    OsPressureLevel, Pressure, PressureLevel, PressureReason, PressureSignal, Provenance,
    SignalScore, StallReading, SwapTrend, format_bytes, format_rate,
};

/// Scores at or above this are **strained**.
pub const STRAINED_AT: u8 = 40;
/// Scores at or above this are **critical**.
pub const CRITICAL_AT: u8 = 70;
/// Signals scoring below this are not worth naming as a reason.
pub const NOTABLE_AT: u8 = 20;

/// Score for the OS's own "warning" pressure level.
pub const OS_WARNING_SCORE: u8 = 60;
/// Score for the OS's own "critical" pressure level.
pub const OS_CRITICAL_SCORE: u8 = 90;

/// Available share of memory at or above which the signal scores 0.
pub const AVAILABLE_CALM_RATIO: f64 = 0.25;
/// Available share of memory at or below which the signal scores `AVAILABLE_MAX_SCORE`.
pub const AVAILABLE_CRITICAL_RATIO: f64 = 0.05;
pub const AVAILABLE_MAX_SCORE: f64 = 90.0;

/// Share of memory held by the compressor at or below which the signal scores 0.
pub const COMPRESSION_CALM_RATIO: f64 = 0.10;
/// Share of memory held by the compressor at which the signal reaches `COMPRESSION_MAX_SCORE`.
pub const COMPRESSION_HIGH_RATIO: f64 = 0.35;
/// Compression alone never makes the level critical: it is the OS coping, not failing.
pub const COMPRESSION_MAX_SCORE: f64 = 65.0;

/// Swap growing at least this fast (bytes per second) counts as "growing".
pub const SWAP_GROWING_BYTES_PER_SEC: f64 = 1024.0 * 1024.0;
/// Swap growing this fast or faster scores `SWAP_MAX_SCORE`.
pub const SWAP_HEAVY_BYTES_PER_SEC: f64 = 32.0 * 1024.0 * 1024.0;
pub const SWAP_MIN_SCORE: f64 = 50.0;
pub const SWAP_MAX_SCORE: f64 = 85.0;

/// Committed share of the commit limit at or below which the signal scores 0 (Windows).
pub const COMMIT_CALM_RATIO: f64 = 0.80;
/// Committed share of the commit limit at which the signal reaches `COMMIT_MAX_SCORE`: Windows
/// starts refusing allocations at the limit.
pub const COMMIT_CRITICAL_RATIO: f64 = 0.97;
pub const COMMIT_MAX_SCORE: f64 = 90.0;

/// Hard page faults per second at or below which the signal scores 0 (Windows). Hard faults
/// also count programs reading their own files (an app starting), so the bar is high.
pub const HARD_FAULTS_CALM_PER_SEC: f64 = 500.0;
/// Hard page faults per second at which the signal reaches `HARD_FAULTS_MAX_SCORE`.
pub const HARD_FAULTS_HEAVY_PER_SEC: f64 = 5000.0;
/// Hard faults alone never make the level critical: they mix paging with ordinary file reads.
pub const HARD_FAULTS_MAX_SCORE: f64 = 65.0;

/// Memory PSI `some avg10` (percent of the last 10 s in which at least one task waited for
/// memory) at or below which the signal scores 0 (Linux). Short reclaim bursts stay below it.
pub const STALL_SOME_CALM_PCT: f64 = 10.0;
/// `some avg10` at which the signal reaches `STALL_SOME_MAX_SCORE`.
pub const STALL_SOME_HEAVY_PCT: f64 = 50.0;
pub const STALL_SOME_MAX_SCORE: f64 = 75.0;
/// Memory PSI `full avg10` (every non-idle task waited at once) at or below which the signal
/// scores 0.
pub const STALL_FULL_CALM_PCT: f64 = 2.0;
/// `full avg10` at which the signal reaches `STALL_FULL_MAX_SCORE`: the machine spends a fifth of
/// its time doing nothing but reclaiming memory, well before systemd-oomd's default 60 % limit.
pub const STALL_FULL_HEAVY_PCT: f64 = 20.0;
pub const STALL_FULL_MAX_SCORE: f64 = 95.0;

/// The raw signals one snapshot provides. Missing signals score 0.
#[derive(Debug, Clone)]
pub struct PressureInputs<'a> {
    /// Human name of the OS used in messages, e.g. "macOS".
    pub os_name: &'a str,
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// Physical memory occupied by the memory compressor.
    pub compressed_bytes: Option<u64>,
    /// `(committed, commit limit)` in bytes (Windows).
    pub commit: Option<(u64, u64)>,
    /// Hard page faults per second (Windows).
    pub hard_faults_per_sec: Option<f64>,
    /// Memory pressure-stall reading (Linux PSI).
    pub memory_stall: Option<StallReading>,
    pub os_level: Option<OsPressureLevel>,
    pub swap_trend: Option<&'a SwapTrend>,
}

/// Computes the unified pressure index. Pure: the same inputs always give the same answer.
pub fn compute(inputs: &PressureInputs<'_>) -> Pressure {
    let total = inputs.total_bytes.max(1) as f64;
    let available_ratio = inputs.available_bytes as f64 / total;

    let mut scored: Vec<(SignalScore, String)> = Vec::with_capacity(4);

    if let Some(level) = inputs.os_level {
        let (score, word) = match level {
            OsPressureLevel::Normal => (0, "normal"),
            OsPressureLevel::Warning => (OS_WARNING_SCORE, "warning"),
            OsPressureLevel::Critical => (OS_CRITICAL_SCORE, "critical"),
        };
        scored.push((
            SignalScore {
                signal: PressureSignal::OsLevel,
                score,
            },
            format!("{} reports memory pressure as {word}", inputs.os_name),
        ));
    }

    let available_score = ramp_down(
        available_ratio,
        AVAILABLE_CALM_RATIO,
        AVAILABLE_CRITICAL_RATIO,
        AVAILABLE_MAX_SCORE,
    );
    scored.push((
        SignalScore {
            signal: PressureSignal::LowAvailable,
            score: available_score,
        },
        format!(
            "only {} of {} memory is available",
            format_bytes(inputs.available_bytes),
            format_bytes(inputs.total_bytes)
        ),
    ));

    if let Some(compressed) = inputs.compressed_bytes {
        let ratio = compressed as f64 / total;
        let score = ramp_up(
            ratio,
            COMPRESSION_CALM_RATIO,
            COMPRESSION_HIGH_RATIO,
            0.0,
            COMPRESSION_MAX_SCORE,
        );
        scored.push((
            SignalScore {
                signal: PressureSignal::Compression,
                score,
            },
            format!(
                "{} is compressing memory to make room ({} of compressed memory)",
                inputs.os_name,
                format_bytes(compressed)
            ),
        ));
    }

    if let Some((committed, limit)) = inputs.commit.filter(|&(_, limit)| limit > 0) {
        let ratio = committed as f64 / limit as f64;
        let score = ramp_up(
            ratio,
            COMMIT_CALM_RATIO,
            COMMIT_CRITICAL_RATIO,
            0.0,
            COMMIT_MAX_SCORE,
        );
        scored.push((
            SignalScore {
                signal: PressureSignal::CommitLimit,
                score,
            },
            format!(
                "{} has promised {} of the {} it can give out (RAM plus page file)",
                inputs.os_name,
                format_bytes(committed),
                format_bytes(limit)
            ),
        ));
    }

    if let Some(rate) = inputs.hard_faults_per_sec {
        let score = ramp_up(
            rate,
            HARD_FAULTS_CALM_PER_SEC,
            HARD_FAULTS_HEAVY_PER_SEC,
            0.0,
            HARD_FAULTS_MAX_SCORE,
        );
        scored.push((
            SignalScore {
                signal: PressureSignal::HardFaults,
                score,
            },
            format!(
                "{} is reading memory back from disk ({rate:.0} hard page faults/s)",
                inputs.os_name
            ),
        ));
    }

    if let Some(stall) = inputs.memory_stall {
        let some = stall.some.avg10;
        let full = stall.full.map_or(0.0, |f| f.avg10);
        let some_score = ramp_up(
            some,
            STALL_SOME_CALM_PCT,
            STALL_SOME_HEAVY_PCT,
            0.0,
            STALL_SOME_MAX_SCORE,
        );
        let full_score = ramp_up(
            full,
            STALL_FULL_CALM_PCT,
            STALL_FULL_HEAVY_PCT,
            0.0,
            STALL_FULL_MAX_SCORE,
        );
        let message = if full_score > some_score {
            format!(
                "every program was stalled waiting for memory {full:.0}% of the last 10 seconds"
            )
        } else {
            format!("programs spent {some:.0}% of the last 10 seconds waiting for memory")
        };
        scored.push((
            SignalScore {
                signal: PressureSignal::MemoryStall,
                score: some_score.max(full_score),
            },
            message,
        ));
    }

    if let Some(trend) = inputs.swap_trend {
        let rate = growth_rate(trend);
        let score = if trend.growing {
            ramp_up(
                rate,
                SWAP_GROWING_BYTES_PER_SEC,
                SWAP_HEAVY_BYTES_PER_SEC,
                SWAP_MIN_SCORE,
                SWAP_MAX_SCORE,
            )
            .max(SWAP_MIN_SCORE as u8)
        } else {
            0
        };
        scored.push((
            SignalScore {
                signal: PressureSignal::SwapGrowing,
                score,
            },
            format!("swap is growing ({})", format_rate(rate)),
        ));
    }

    // Highest score wins; on ties prefer the most concrete, actionable signal.
    let rank = |signal: PressureSignal| match signal {
        PressureSignal::SwapGrowing => 0,
        PressureSignal::LowAvailable => 1,
        PressureSignal::CommitLimit => 2,
        PressureSignal::MemoryStall => 3,
        PressureSignal::HardFaults => 4,
        PressureSignal::Compression => 5,
        PressureSignal::OsLevel => 6,
        PressureSignal::None => 7,
    };
    let mut order: Vec<usize> = (0..scored.len()).collect();
    order.sort_by(|&a, &b| {
        scored[b]
            .0
            .score
            .cmp(&scored[a].0.score)
            .then_with(|| rank(scored[a].0.signal).cmp(&rank(scored[b].0.signal)))
    });

    let top = &scored[order[0]];
    let score = top.0.score.min(100);
    let main_reason = if score < NOTABLE_AT {
        PressureReason {
            signal: PressureSignal::None,
            message: format!(
                "{} of {} memory is available",
                format_bytes(inputs.available_bytes),
                format_bytes(inputs.total_bytes)
            ),
        }
    } else {
        let mut message = top.1.clone();
        // The OS level alone explains little: add the strongest concrete signal behind it.
        if top.0.signal == PressureSignal::OsLevel
            && let Some(next) = order.get(1).map(|&i| &scored[i])
            && next.0.score >= NOTABLE_AT
        {
            message.push_str("; ");
            message.push_str(&next.1);
        }
        PressureReason {
            signal: top.0.signal,
            message,
        }
    };

    Pressure {
        score,
        level: level_for(score),
        main_reason,
        signals: scored.into_iter().map(|(signal, _)| signal).collect(),
        provenance: Provenance::Estimated,
    }
}

/// Maps a 0–100 score to its level.
pub fn level_for(score: u8) -> PressureLevel {
    if score >= CRITICAL_AT {
        PressureLevel::Critical
    } else if score >= STRAINED_AT {
        PressureLevel::Strained
    } else {
        PressureLevel::Calm
    }
}

/// The swap growth rate that counts: the faster of used-swap growth and swap-out traffic.
pub fn growth_rate(trend: &SwapTrend) -> f64 {
    trend
        .used_change_bytes_per_sec
        .max(trend.swap_out_bytes_per_sec.unwrap_or(0.0))
        .max(0.0)
}

/// 0 at `calm` or above, `max` at `critical` or below, linear in between.
fn ramp_down(value: f64, calm: f64, critical: f64, max: f64) -> u8 {
    if value >= calm {
        0
    } else if value <= critical {
        max.round() as u8
    } else {
        (max * (calm - value) / (calm - critical)).round() as u8
    }
}

/// `low_score` at `low` (0 below it), `high_score` at `high` or above, linear in between.
fn ramp_up(value: f64, low: f64, high: f64, low_score: f64, high_score: f64) -> u8 {
    if value <= low {
        0
    } else if value >= high {
        high_score.round() as u8
    } else {
        (low_score + (high_score - low_score) * (value - low) / (high - low)).round() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn inputs(available: u64) -> PressureInputs<'static> {
        PressureInputs {
            os_name: "macOS",
            total_bytes: 16 * GIB,
            available_bytes: available,
            compressed_bytes: Some(GIB / 2),
            commit: None,
            hard_faults_per_sec: None,
            memory_stall: None,
            os_level: Some(OsPressureLevel::Normal),
            swap_trend: None,
        }
    }

    fn trend(rate: f64) -> SwapTrend {
        SwapTrend {
            growing: rate >= SWAP_GROWING_BYTES_PER_SEC,
            used_change_bytes_per_sec: rate,
            swap_out_bytes_per_sec: None,
            window_secs: 1.0,
            provenance: Provenance::Measured,
        }
    }

    #[test]
    fn plenty_of_memory_is_calm_with_a_plain_reason() {
        let p = compute(&inputs(8 * GIB));
        assert_eq!(p.level, PressureLevel::Calm);
        assert_eq!(p.score, 0);
        assert_eq!(p.main_reason.signal, PressureSignal::None);
        assert_eq!(
            p.main_reason.message,
            "8.0 GB of 16.0 GB memory is available"
        );
        assert_eq!(p.provenance, Provenance::Estimated);
    }

    #[test]
    fn low_available_memory_raises_the_level() {
        let p = compute(&inputs(GIB * 16 / 10)); // 10 %
        assert_eq!(p.level, PressureLevel::Strained);
        assert_eq!(p.main_reason.signal, PressureSignal::LowAvailable);
        let p = compute(&inputs(GIB / 2)); // ~3 %
        assert_eq!(p.level, PressureLevel::Critical);
    }

    #[test]
    fn swap_in_use_but_not_growing_is_harmless() {
        let idle = trend(0.0);
        let mut i = inputs(8 * GIB);
        i.swap_trend = Some(&idle);
        let p = compute(&i);
        assert_eq!(p.level, PressureLevel::Calm);
    }

    #[test]
    fn growing_swap_is_strained_then_critical() {
        let slow = trend(2.0 * 1024.0 * 1024.0);
        let mut i = inputs(8 * GIB);
        i.swap_trend = Some(&slow);
        let p = compute(&i);
        assert_eq!(p.level, PressureLevel::Strained);
        assert_eq!(p.main_reason.signal, PressureSignal::SwapGrowing);
        assert!(p.main_reason.message.starts_with("swap is growing"));

        let fast = trend(64.0 * 1024.0 * 1024.0);
        i.swap_trend = Some(&fast);
        assert_eq!(compute(&i).level, PressureLevel::Critical);
    }

    #[test]
    fn compression_alone_never_reaches_critical() {
        let mut i = inputs(8 * GIB);
        i.compressed_bytes = Some(12 * GIB);
        let p = compute(&i);
        assert_eq!(p.level, PressureLevel::Strained);
        assert_eq!(p.main_reason.signal, PressureSignal::Compression);
    }

    #[test]
    fn os_level_reason_names_the_concrete_cause_behind_it() {
        let mut i = inputs(GIB * 16 / 5); // 20 % available scores below the OS warning
        i.os_level = Some(OsPressureLevel::Warning);
        let p = compute(&i);
        assert_eq!(p.main_reason.signal, PressureSignal::OsLevel);
        assert_eq!(
            p.main_reason.message,
            "macOS reports memory pressure as warning; only 3.2 GB of 16.0 GB memory is available"
        );
    }

    #[test]
    fn commit_near_the_limit_raises_the_level() {
        let mut i = inputs(8 * GIB);
        i.os_name = "Windows";
        i.commit = Some((20 * GIB, 32 * GIB)); // 62 %
        assert_eq!(compute(&i).level, PressureLevel::Calm);
        i.commit = Some((29 * GIB, 32 * GIB)); // 91 %
        let p = compute(&i);
        assert_eq!(p.level, PressureLevel::Strained);
        assert_eq!(p.main_reason.signal, PressureSignal::CommitLimit);
        assert!(
            p.main_reason
                .message
                .starts_with("Windows has promised 29.0 GB of the 32.0 GB"),
            "{}",
            p.main_reason.message
        );
        i.commit = Some((32 * GIB, 32 * GIB));
        assert_eq!(compute(&i).level, PressureLevel::Critical);
        // A zero limit is ignored rather than dividing by zero.
        i.commit = Some((GIB, 0));
        assert_eq!(compute(&i).level, PressureLevel::Calm);
    }

    #[test]
    fn hard_faults_strain_but_never_reach_critical_alone() {
        let mut i = inputs(8 * GIB);
        i.os_name = "Windows";
        i.hard_faults_per_sec = Some(200.0);
        assert_eq!(compute(&i).level, PressureLevel::Calm);
        i.hard_faults_per_sec = Some(4000.0);
        let p = compute(&i);
        assert_eq!(p.level, PressureLevel::Strained);
        assert_eq!(p.main_reason.signal, PressureSignal::HardFaults);
        assert!(p.main_reason.message.contains("4000 hard page faults/s"));
        i.hard_faults_per_sec = Some(1e6);
        assert_eq!(compute(&i).level, PressureLevel::Strained);
    }

    fn stall(some: f64, full: f64) -> StallReading {
        let line = |avg10| crate::core::StallLine {
            avg10,
            avg60: 0.0,
            avg300: 0.0,
            total_us: 0,
        };
        StallReading {
            some: line(some),
            full: Some(line(full)),
        }
    }

    #[test]
    fn memory_stall_strains_then_becomes_critical() {
        let mut i = inputs(8 * GIB);
        i.os_name = "Linux";
        i.os_level = None;
        i.compressed_bytes = None;
        i.memory_stall = Some(stall(3.0, 0.5));
        assert_eq!(compute(&i).level, PressureLevel::Calm);
        i.memory_stall = Some(stall(35.0, 1.0));
        let p = compute(&i);
        assert_eq!(p.level, PressureLevel::Strained);
        assert_eq!(p.main_reason.signal, PressureSignal::MemoryStall);
        assert_eq!(
            p.main_reason.message,
            "programs spent 35% of the last 10 seconds waiting for memory"
        );
        i.memory_stall = Some(stall(40.0, 25.0));
        let p = compute(&i);
        assert_eq!(p.level, PressureLevel::Critical);
        assert!(
            p.main_reason
                .message
                .starts_with("every program was stalled")
        );
        // `some` alone reaches critical only when stalls are heavy.
        i.memory_stall = Some(stall(60.0, 0.0));
        assert_eq!(compute(&i).level, PressureLevel::Critical);
    }

    #[test]
    fn levels_follow_the_documented_thresholds() {
        assert_eq!(level_for(STRAINED_AT - 1), PressureLevel::Calm);
        assert_eq!(level_for(STRAINED_AT), PressureLevel::Strained);
        assert_eq!(level_for(CRITICAL_AT), PressureLevel::Critical);
    }
}
