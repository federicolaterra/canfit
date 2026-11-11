//! The fit verdict: compares an estimate with the memory available now and, on unified-memory
//! machines, with the GPU memory limit (ADR-012).

use serde::Serialize;

use crate::core::{PressureLevel, Provenance, Snapshot};

/// The answer to "can I launch it right now?". Exit codes follow ADR-003.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Fits,
    Tight,
    DoesntFit,
}

impl Verdict {
    pub fn exit_code(self) -> u8 {
        match self {
            Verdict::Fits => 0,
            Verdict::Tight => 1,
            Verdict::DoesntFit => 2,
        }
    }

    /// Colour-blind-safe symbol (spec §11).
    pub fn symbol(self) -> &'static str {
        match self {
            Verdict::Fits => "●",
            Verdict::Tight => "▲",
            Verdict::DoesntFit => "✗",
        }
    }
}

/// Which memory limit a check is against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    /// Memory available now (free plus reclaimable cache).
    AvailableMemory,
    /// The share of unified memory the GPU may use (Apple Silicon).
    GpuMemoryLimit,
}

/// Why a check is tight rather than a plain fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TightCause {
    /// The estimate fits only without the safety margin.
    WithinMargin,
    /// It fits, but memory pressure is already critical.
    PressureCritical,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LimitCheck {
    pub limit: LimitKind,
    pub limit_bytes: u64,
    pub provenance: Provenance,
    pub verdict: Verdict,
    pub tight_cause: Option<TightCause>,
    /// `limit − total needed (with margin)`; negative when short.
    pub headroom_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FitResult {
    /// The worst verdict over all checks.
    pub verdict: Verdict,
    /// The check that decided the verdict.
    pub deciding_limit: LimitKind,
    pub checks: Vec<LimitCheck>,
    pub pressure_level: PressureLevel,
}

impl FitResult {
    pub fn deciding(&self) -> &LimitCheck {
        self.checks
            .iter()
            .find(|c| c.limit == self.deciding_limit)
            .expect("the deciding limit is one of the checks")
    }
}

/// Bands one limit: fits with the margin, fits only without it, or doesn't fit.
fn band(base: u64, total: u64, limit: u64) -> Verdict {
    if total <= limit {
        Verdict::Fits
    } else if base <= limit {
        Verdict::Tight
    } else {
        Verdict::DoesntFit
    }
}

fn check(
    limit: LimitKind,
    limit_bytes: u64,
    provenance: Provenance,
    base: u64,
    total: u64,
) -> LimitCheck {
    let verdict = band(base, total, limit_bytes);
    LimitCheck {
        limit,
        limit_bytes,
        provenance,
        verdict,
        tight_cause: (verdict == Verdict::Tight).then_some(TightCause::WithinMargin),
        headroom_bytes: i64::try_from(i128::from(limit_bytes) - i128::from(total))
            .unwrap_or(i64::MIN),
    }
}

/// Judges whether `base` bytes (estimate without margin) / `total` bytes (with margin) fit on the
/// system described by `snapshot`.
pub fn evaluate(base: u64, total: u64, snapshot: &Snapshot) -> FitResult {
    evaluate_parts(base, total, Some((base, total)), snapshot)
}

/// Like [`evaluate`], against available memory only: apps (ADR-020) don't allocate GPU-wired
/// memory, so the Apple Silicon GPU limit does not apply to them.
pub fn evaluate_memory(base: u64, total: u64, snapshot: &Snapshot) -> FitResult {
    evaluate_parts(base, total, None, snapshot)
}

/// Judges several targets together (spec §5): everything (`base` / `total`) against available
/// memory, and only the models' part (`gpu_part`, without / with margin) against the GPU memory
/// limit — apps don't allocate GPU-wired memory (ADR-021).
pub fn evaluate_parts(
    base: u64,
    total: u64,
    gpu_part: Option<(u64, u64)>,
    snapshot: &Snapshot,
) -> FitResult {
    let mut memory = check(
        LimitKind::AvailableMemory,
        snapshot.memory.available_bytes,
        snapshot.memory.provenance,
        base,
        total,
    );
    if memory.verdict == Verdict::Fits && snapshot.pressure.level == PressureLevel::Critical {
        memory.verdict = Verdict::Tight;
        memory.tight_cause = Some(TightCause::PressureCritical);
    }
    let mut checks = vec![memory];
    if let (Some(gpu), Some((gpu_base, gpu_total))) = (snapshot.gpu_memory_limit.as_ref(), gpu_part)
    {
        checks.push(check(
            LimitKind::GpuMemoryLimit,
            gpu.bytes,
            gpu.provenance,
            gpu_base,
            gpu_total,
        ));
    }
    // The first check with the worst verdict decides (available memory wins ties).
    let worst = checks
        .iter()
        .map(|c| c.verdict)
        .max()
        .unwrap_or(Verdict::Fits);
    let deciding_limit = checks
        .iter()
        .find(|c| c.verdict == worst)
        .map_or(LimitKind::AvailableMemory, |c| c.limit);
    FitResult {
        verdict: worst,
        deciding_limit,
        checks,
        pressure_level: snapshot.pressure.level,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::GpuMemoryLimit;
    use crate::core::sampler::tests::replay;

    const GB: u64 = 1024 * 1024 * 1024;

    fn snapshot(available: u64, gpu: Option<u64>, level: PressureLevel) -> Snapshot {
        let (_, mut s) = replay("calm");
        s.memory.available_bytes = available;
        s.gpu_memory_limit = gpu.map(|bytes| GpuMemoryLimit {
            bytes,
            provenance: Provenance::Estimated,
            source: "test".to_string(),
        });
        s.pressure.level = level;
        s
    }

    #[test]
    fn three_bands_against_available_memory() {
        let s = snapshot(10 * GB, None, PressureLevel::Calm);
        let r = evaluate(8 * GB, 9 * GB, &s);
        assert_eq!(r.verdict, Verdict::Fits);
        assert_eq!(r.checks[0].headroom_bytes, GB as i64);
        let r = evaluate(9 * GB, 11 * GB, &s);
        assert_eq!(r.verdict, Verdict::Tight);
        assert_eq!(r.checks[0].tight_cause, Some(TightCause::WithinMargin));
        let r = evaluate(11 * GB, 12 * GB, &s);
        assert_eq!(r.verdict, Verdict::DoesntFit);
        assert_eq!(r.checks[0].headroom_bytes, -2 * GB as i64);
        assert_eq!(r.verdict.exit_code(), 2);
    }

    #[test]
    fn critical_pressure_makes_a_fit_tight() {
        let s = snapshot(10 * GB, None, PressureLevel::Critical);
        let r = evaluate(2 * GB, 3 * GB, &s);
        assert_eq!(r.verdict, Verdict::Tight);
        assert_eq!(r.checks[0].tight_cause, Some(TightCause::PressureCritical));
        let strained = snapshot(10 * GB, None, PressureLevel::Strained);
        assert_eq!(evaluate(2 * GB, 3 * GB, &strained).verdict, Verdict::Fits);
    }

    #[test]
    fn the_gpu_limit_applies_to_the_model_part_only() {
        let s = snapshot(30 * GB, Some(10 * GB), PressureLevel::Calm);
        // 8 GB of model + 12 GB of apps: fits in memory, and the model fits the GPU limit.
        let r = evaluate_parts(20 * GB, 22 * GB, Some((8 * GB, 9 * GB)), &s);
        assert_eq!(r.verdict, Verdict::Fits);
        assert_eq!(r.checks[1].headroom_bytes, GB as i64);
        // The same total judged as one model would break the GPU limit.
        assert_eq!(evaluate(20 * GB, 22 * GB, &s).verdict, Verdict::DoesntFit);
        // No model part: no GPU check.
        assert_eq!(evaluate_parts(20 * GB, 22 * GB, None, &s).checks.len(), 1);
        let r = evaluate_parts(20 * GB, 22 * GB, Some((11 * GB, 12 * GB)), &s);
        assert_eq!(r.deciding_limit, LimitKind::GpuMemoryLimit);
    }

    #[test]
    fn gpu_limit_is_a_second_limit() {
        let s = snapshot(20 * GB, Some(10 * GB), PressureLevel::Calm);
        let r = evaluate(11 * GB, 12 * GB, &s);
        assert_eq!(r.verdict, Verdict::DoesntFit);
        assert_eq!(r.deciding_limit, LimitKind::GpuMemoryLimit);
        assert_eq!(r.checks[0].verdict, Verdict::Fits);
        assert_eq!(r.deciding().limit_bytes, 10 * GB);
        // Both short: available memory is named first.
        let s = snapshot(5 * GB, Some(10 * GB), PressureLevel::Calm);
        assert_eq!(
            evaluate(11 * GB, 12 * GB, &s).deciding_limit,
            LimitKind::AvailableMemory
        );
    }

    #[test]
    fn apps_ignore_the_gpu_limit() {
        let s = snapshot(20 * GB, Some(10 * GB), PressureLevel::Calm);
        let r = evaluate_memory(11 * GB, 12 * GB, &s);
        assert_eq!(r.verdict, Verdict::Fits);
        assert_eq!(r.checks.len(), 1);
        let critical = snapshot(20 * GB, None, PressureLevel::Critical);
        assert_eq!(evaluate_memory(GB, GB, &critical).verdict, Verdict::Tight);
    }
}
