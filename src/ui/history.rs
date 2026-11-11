//! The last few minutes of readings, one point per refresh, for the cards' sparklines.

use std::collections::VecDeque;

use crate::core::Snapshot;

/// Points kept per series: five minutes at the default one-second refresh.
pub const HISTORY_POINTS: usize = 300;

/// Sparkline values are integers: percentages are kept in tenths.
pub const PERCENT_SCALE: u64 = 1000;

#[derive(Debug, Clone, Default)]
pub struct History {
    /// Memory used, tenths of a percent of total.
    pub memory: VecDeque<u64>,
    /// Swap used, MiB.
    pub swap: VecDeque<u64>,
    /// CPU busy, tenths of a percent.
    pub cpu: VecDeque<u64>,
    /// Pressure score, 0–100.
    pub pressure: VecDeque<u64>,
}

fn push(series: &mut VecDeque<u64>, value: u64) {
    if series.len() == HISTORY_POINTS {
        series.pop_front();
    }
    series.push_back(value);
}

fn tenths(part: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        (u128::from(part) * u128::from(PERCENT_SCALE) / u128::from(total)) as u64
    }
}

impl History {
    pub fn record(&mut self, s: &Snapshot) {
        push(
            &mut self.memory,
            tenths(s.memory.used_bytes, s.memory.total_bytes),
        );
        push(&mut self.swap, s.swap.used_bytes / (1024 * 1024));
        // The CPU has no reading on the very first sample; skip it rather than drawing a zero.
        if let Some(u) = s.cpu.usage_percent {
            push(&mut self.cpu, (f64::from(u) * 10.0).round().max(0.0) as u64);
        }
        push(&mut self.pressure, u64::from(s.pressure.score));
    }
}

/// The newest `n` points of a series, oldest first.
pub fn tail(series: &VecDeque<u64>, n: usize) -> Vec<u64> {
    let skip = series.len().saturating_sub(n);
    series.iter().skip(skip).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sampler::tests::replay;

    #[test]
    fn series_are_bounded_and_scaled() {
        let (_, mut s) = replay("calm");
        s.cpu.usage_percent = Some(12.34);
        let mut h = History::default();
        for _ in 0..HISTORY_POINTS + 5 {
            h.record(&s);
        }
        assert_eq!(h.memory.len(), HISTORY_POINTS);
        assert_eq!(h.cpu.back(), Some(&123));
        assert_eq!(
            *h.memory.back().unwrap(),
            tenths(s.memory.used_bytes, s.memory.total_bytes)
        );
        assert_eq!(tail(&h.cpu, 3), vec![123, 123, 123]);
        assert_eq!(tail(&VecDeque::from(vec![1, 2, 3]), 10), vec![1, 2, 3]);
    }

    #[test]
    fn missing_cpu_reading_is_not_drawn_as_zero() {
        let (_, mut s) = replay("calm");
        s.cpu.usage_percent = None;
        let mut h = History::default();
        h.record(&s);
        assert!(h.cpu.is_empty());
        assert_eq!(h.pressure.len(), 1);
        assert_eq!(tenths(1, 0), 0);
    }
}
