//! History-based app estimates (spec §9 source 2, ADR-021): what canfit has measured an app use
//! while the monitor was open.
//!
//! The need is the 95th percentile (nearest rank) of the app's stored samples; the median is its
//! typical use and the profile's all-time peak is context. How far to trust it depends on the
//! evidence: how many samples, how long canfit watched, on how many days, and how recently
//! ([`confidence`]). Everything here is a pure function of the stored rows, so the rules are
//! tested on synthetic histories.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::Serialize;

use super::Confidence;
use super::resolve::normalize;
use crate::core::Provenance;
use crate::history::store::{AppProfile, Store, WatchRun};
use crate::history::{DAY_MS, DEFAULT_SNAPSHOT_INTERVAL_SECS, HOUR_MS, HistoryError, MINUTE_MS};

/// The percentile of the stored samples used as the need.
pub const NEED_PERCENTILE: u32 = 95;

/// Evidence for **high** confidence: at least this many samples ...
pub const HIGH_MIN_SAMPLES: u64 = 120;
/// ... covering at least this much observed time ...
pub const HIGH_MIN_OBSERVED_MS: u64 = HOUR_MS;
/// ... on at least this many different days ...
pub const HIGH_MIN_DAYS: u32 = 2;
/// ... and last seen no longer ago than this.
pub const HIGH_MAX_AGE_MS: u64 = 14 * DAY_MS;

/// Evidence for **medium** confidence (the need is then labelled measured).
pub const MEDIUM_MIN_SAMPLES: u64 = 20;
pub const MEDIUM_MIN_OBSERVED_MS: u64 = 10 * MINUTE_MS;
pub const MEDIUM_MAX_AGE_MS: u64 = 30 * DAY_MS;

/// Gaps longer than this between two samples are breaks (the monitor was closed), not the
/// sampling interval.
const MAX_INTERVAL_MS: u64 = HOUR_MS;

/// A watched run at least this long, with at least [`WATCH_HIGH_MIN_SAMPLES`] samples, is a
/// high-confidence measurement (ADR-023); a shorter one is medium.
pub const WATCH_HIGH_MIN_MS: u64 = 30 * 1000;
pub const WATCH_HIGH_MIN_SAMPLES: u64 = 10;
/// A run that failed (non-zero exit, not interrupted) sooner than this is skipped: the program
/// most likely never got going (a missing model file, a bad flag).
pub const WATCH_FAILED_START_MS: u64 = 30 * 1000;

/// Everything canfit has recorded about apps, read once from the history database.
#[derive(Debug, Clone, Default)]
pub struct AppHistory {
    pub profiles: Vec<AppProfile>,
    /// Stored samples `(time, memory)` per app group name, oldest first.
    pub samples: HashMap<String, Vec<(u64, u64)>>,
    /// Runs measured by `canfit watch`, most recent first (ADR-023).
    pub watch_runs: Vec<WatchRun>,
}

impl AppHistory {
    /// Reads every app profile and its samples. `None` when there is no database yet; never
    /// creates one.
    pub fn load(path: &Path) -> Result<Option<AppHistory>, HistoryError> {
        let Some(store) = Store::open_existing(path)? else {
            return Ok(None);
        };
        let profiles = store.app_profiles()?;
        let mut samples = HashMap::new();
        for p in &profiles {
            samples.insert(p.name.clone(), store.app_samples(&p.name)?);
        }
        let watch_runs = store.watch_runs()?;
        Ok(Some(AppHistory {
            profiles,
            samples,
            watch_runs,
        }))
    }

    /// The recorded app names: watched runs first, then the monitor's app groups, each once
    /// (ignoring case and spacing).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        let mut seen = BTreeSet::new();
        self.watch_runs
            .iter()
            .map(|r| r.name.as_str())
            .chain(self.profiles.iter().map(|p| p.name.as_str()))
            .filter(move |n| seen.insert(normalize(n)))
    }

    /// What canfit learned about the app named like any of `wanted` (ignoring case, spacing and
    /// `.exe` / `.app`), at `now_ms`: a run measured by `canfit watch` first, else the monitor's
    /// samples of the app groups so named (ADR-021, ADR-023). `None` when canfit never saw it.
    pub fn learn(&self, wanted: &[String], now_ms: u64) -> Option<Learned> {
        let wanted: BTreeSet<String> = wanted.iter().map(|w| normalize(w)).collect();
        let runs: Vec<&WatchRun> = self
            .watch_runs
            .iter()
            .filter(|r| wanted.contains(&normalize(&r.name)))
            .collect();
        if let Some(run) = runs.iter().copied().find(|r| usable_run(r)) {
            return Some(from_watch(run, runs.len() as u64));
        }
        let profiles: Vec<&AppProfile> = self
            .profiles
            .iter()
            .filter(|p| wanted.contains(&normalize(&p.name)))
            .collect();
        if profiles.is_empty() {
            return None;
        }
        // Several groups seen at the same moment add up (an app and its helper app).
        let mut by_time: BTreeMap<u64, u64> = BTreeMap::new();
        for p in &profiles {
            for &(ts, bytes) in self.samples.get(&p.name).into_iter().flatten() {
                *by_time.entry(ts).or_default() += bytes;
            }
        }
        let samples: Vec<(u64, u64)> = by_time.into_iter().collect();
        Some(summarize(&profiles, &samples, now_ms))
    }
}

/// What canfit measured an app use, and how much to trust it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Learned {
    /// The app groups the samples come from.
    pub names: Vec<String>,
    /// Samples counted in the running totals (kept 90 days).
    pub samples: u64,
    /// Samples still stored one by one (kept for the history retention, 7 days by default); the
    /// percentile is computed from these.
    pub detailed_samples: u64,
    /// About how long canfit watched the app: samples × the sampling interval.
    pub observed_ms: u64,
    /// Different days (UTC) with stored samples.
    pub days: u32,
    pub first_seen_unix_ms: u64,
    pub last_seen_unix_ms: u64,
    /// Median of the stored samples (the running mean when they were pruned).
    pub typical_bytes: u64,
    /// The 95th percentile of the stored samples (nearest rank).
    pub p95_bytes: Option<u64>,
    /// The largest use ever recorded (90 days).
    pub peak_bytes: u64,
    /// What canfit uses as the need: the 95th percentile, or the peak when only the running
    /// totals are left.
    pub need_bytes: u64,
    pub confidence: Confidence,
    /// `measured` with medium or high confidence, `estimated` with low confidence.
    pub provenance: Provenance,
    /// How the memory was measured (`footprint`, `pss`, ...).
    pub memory_kind: String,
    /// `watch` (a run measured by `canfit watch`) or `monitor` (samples taken while the monitor
    /// was open).
    pub method: Method,
    /// For `method = watch`: the run the need comes from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchInfo>,
}

/// How a history value was measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Sampled while the monitor was open (ADR-021).
    Monitor,
    /// Measured by `canfit watch` over one whole run (ADR-023).
    Watch,
}

/// The watched run a history value comes from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WatchInfo {
    pub program: String,
    pub started_unix_ms: u64,
    pub duration_ms: u64,
    pub peak_bytes: u64,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub interrupted: bool,
    /// The model file's weights the command mapped from disk, which the measured peak leaves
    /// out; added to the need.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapped_model_bytes: Option<u64>,
    /// Watched runs stored under this name (the most recent usable one is used).
    pub runs: u64,
}

/// Whether a watched run can stand for the app: anything except a run that failed early.
pub fn usable_run(run: &WatchRun) -> bool {
    run.samples > 0
        && (run.interrupted || run.exit_code == Some(0) || run.duration_ms >= WATCH_FAILED_START_MS)
}

/// A watched run as a history value: the need is its peak, measured; confidence high when the
/// run was long enough to reach its working size, medium otherwise.
fn from_watch(run: &WatchRun, runs: u64) -> Learned {
    let confidence =
        if run.duration_ms >= WATCH_HIGH_MIN_MS && run.samples >= WATCH_HIGH_MIN_SAMPLES {
            Confidence::High
        } else {
            Confidence::Medium
        };
    Learned {
        names: vec![run.name.clone()],
        samples: run.samples,
        detailed_samples: run.samples,
        observed_ms: run.duration_ms,
        days: 1,
        first_seen_unix_ms: run.started_unix_ms,
        last_seen_unix_ms: run.started_unix_ms + run.duration_ms,
        typical_bytes: run.typical_bytes,
        p95_bytes: Some(run.p95_bytes),
        peak_bytes: run.peak_bytes,
        need_bytes: run
            .peak_bytes
            .saturating_add(run.model_mapped_bytes.unwrap_or(0)),
        confidence,
        provenance: Provenance::Measured,
        memory_kind: run.memory_kind.clone(),
        method: Method::Watch,
        watch: Some(WatchInfo {
            program: run.program.clone(),
            started_unix_ms: run.started_unix_ms,
            duration_ms: run.duration_ms,
            peak_bytes: run.peak_bytes,
            exit_code: run.exit_code,
            exit_signal: run.exit_signal,
            interrupted: run.interrupted,
            mapped_model_bytes: run.model_mapped_bytes,
            runs,
        }),
    }
}

/// Nearest-rank percentile of `values` (sorted in place). `None` when empty.
pub fn percentile(values: &mut [u64], pct: u32) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let n = values.len() as u64;
    let rank = (u64::from(pct.min(100)) * n).div_ceil(100).max(1);
    values.get(usize::try_from(rank - 1).ok()?).copied()
}

/// The sampling interval: the median gap between consecutive samples, ignoring breaks longer
/// than an hour; canfit's default snapshot interval when there are too few samples.
pub fn sampling_interval_ms(times: &[u64]) -> u64 {
    let mut gaps: Vec<u64> = times
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .filter(|&g| g > 0 && g <= MAX_INTERVAL_MS)
        .collect();
    percentile(&mut gaps, 50).unwrap_or(DEFAULT_SNAPSHOT_INTERVAL_SECS * 1000)
}

/// How far to trust a history-based need (ADR-021).
pub fn confidence(samples: u64, observed_ms: u64, days: u32, age_ms: u64) -> Confidence {
    if samples >= HIGH_MIN_SAMPLES
        && observed_ms >= HIGH_MIN_OBSERVED_MS
        && days >= HIGH_MIN_DAYS
        && age_ms <= HIGH_MAX_AGE_MS
    {
        Confidence::High
    } else if samples >= MEDIUM_MIN_SAMPLES
        && observed_ms >= MEDIUM_MIN_OBSERVED_MS
        && age_ms <= MEDIUM_MAX_AGE_MS
    {
        Confidence::Medium
    } else {
        Confidence::Low
    }
}

fn summarize(profiles: &[&AppProfile], samples: &[(u64, u64)], now_ms: u64) -> Learned {
    let total_samples: u64 = profiles.iter().map(|p| p.samples).max().unwrap_or(0);
    let first = profiles.iter().map(|p| p.first_seen_ms).min().unwrap_or(0);
    let last = profiles.iter().map(|p| p.last_seen_ms).max().unwrap_or(0);
    let all_time_peak = profiles.iter().map(|p| p.peak_bytes).max().unwrap_or(0);
    let mut values: Vec<u64> = samples.iter().map(|&(_, b)| b).collect();
    let peak = values.iter().copied().max().unwrap_or(0).max(all_time_peak);
    let p95 = percentile(&mut values, NEED_PERCENTILE);
    let typical = percentile(&mut values, 50)
        .unwrap_or_else(|| profiles.iter().map(|p| p.typical_bytes).max().unwrap_or(0));
    let times: Vec<u64> = samples.iter().map(|&(t, _)| t).collect();
    let interval = sampling_interval_ms(&times);
    // The running totals outlive the detailed samples; count whichever saw more.
    let counted = total_samples.max(samples.len() as u64);
    let days = samples
        .iter()
        .map(|&(t, _)| t / DAY_MS)
        .collect::<BTreeSet<_>>()
        .len() as u32;
    let observed = counted.saturating_mul(interval);
    let age = now_ms.saturating_sub(last);
    let mut confidence = confidence(counted, observed, days, age);
    if p95.is_none() {
        // Only the running totals are left: no distribution, so the peak and low confidence.
        confidence = Confidence::Low;
    }
    let mut names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
    names.sort();
    Learned {
        names,
        samples: counted,
        detailed_samples: samples.len() as u64,
        observed_ms: observed,
        days,
        first_seen_unix_ms: first,
        last_seen_unix_ms: last,
        typical_bytes: typical,
        p95_bytes: p95,
        peak_bytes: peak,
        need_bytes: p95.unwrap_or(peak),
        confidence,
        provenance: if confidence == Confidence::Low {
            Provenance::Estimated
        } else {
            Provenance::Measured
        },
        memory_kind: profiles
            .first()
            .map(|p| p.memory_kind.clone())
            .unwrap_or_default(),
        method: Method::Monitor,
        watch: None,
    }
}

/// A duration in words: "45 s", "4 min", "1 h 12 min", "2 d 3 h".
pub fn span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        3600..86_400 => match (s % 3600) / 60 {
            0 => format!("{} h", s / 3600),
            m => format!("{} h {m} min", s / 3600),
        },
        _ => match (s % 86_400) / 3600 {
            0 => format!("{} d", s / 86_400),
            h => format!("{} d {h} h", s / 86_400),
        },
    }
}

/// How long ago, in days: "today", "yesterday", "5 days ago".
pub fn ago(unix_ms: u64, now_ms: u64) -> String {
    match now_ms.saturating_sub(unix_ms) / DAY_MS {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        n => format!("{n} days ago"),
    }
}

impl Learned {
    /// "seen for 3 h over 4 days, last today"; for a watched run "watched for 12 min, today".
    pub fn evidence(&self, now_ms: u64) -> String {
        if let Some(w) = &self.watch {
            return format!(
                "watched for {}, {}",
                span(w.duration_ms),
                ago(w.started_unix_ms, now_ms)
            );
        }
        let over = match self.days {
            0 | 1 => String::new(),
            n => format!(" over {n} days"),
        };
        format!(
            "seen for {}{over}, last {}",
            span(self.observed_ms),
            ago(self.last_seen_unix_ms, now_ms)
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::sampler::tests::replay;
    use crate::core::{AppGroup, GroupKind, MemoryKind};
    use crate::history::tests::temp_dir;

    const MIB: u64 = 1024 * 1024;
    const T0: u64 = 1_800_000_000_000;

    fn profile(name: &str, samples: &[(u64, u64)]) -> AppProfile {
        AppProfile {
            name: name.to_string(),
            memory_kind: "footprint".to_string(),
            samples: samples.len() as u64,
            typical_bytes: samples.iter().map(|s| s.1).sum::<u64>() / samples.len().max(1) as u64,
            peak_bytes: samples.iter().map(|s| s.1).max().unwrap_or(0),
            peak_ms: 0,
            first_seen_ms: samples.first().map_or(0, |s| s.0),
            last_seen_ms: samples.last().map_or(0, |s| s.0),
        }
    }

    /// `count` samples every 30 s from `start`, rising from 100 MB by 1 MB each.
    pub(crate) fn series(start: u64, count: u64) -> Vec<(u64, u64)> {
        (0..count)
            .map(|i| (start + i * 30_000, (100 + i) * MIB))
            .collect()
    }

    pub(crate) fn history(apps: &[(&str, Vec<(u64, u64)>)]) -> AppHistory {
        AppHistory {
            profiles: apps.iter().map(|(n, s)| profile(n, s)).collect(),
            samples: apps
                .iter()
                .map(|(n, s)| (n.to_string(), s.clone()))
                .collect(),
            watch_runs: Vec::new(),
        }
    }

    #[test]
    fn nearest_rank_percentile() {
        assert_eq!(percentile(&mut [], 95), None);
        assert_eq!(percentile(&mut [7], 95), Some(7));
        let mut v: Vec<u64> = (1..=100).rev().collect();
        assert_eq!(percentile(&mut v, 95), Some(95));
        assert_eq!(percentile(&mut v, 50), Some(50));
        assert_eq!(percentile(&mut v, 100), Some(100));
        // With few samples the 95th percentile is the largest one: conservative.
        assert_eq!(percentile(&mut [1, 9, 5], 95), Some(9));
    }

    #[test]
    fn interval_ignores_breaks() {
        assert_eq!(sampling_interval_ms(&[]), 30_000);
        assert_eq!(sampling_interval_ms(&[0, 60_000, 120_000]), 60_000);
        // A two-day break between monitor sessions is not the interval.
        assert_eq!(
            sampling_interval_ms(&[0, 30_000, 60_000, 2 * DAY_MS, 2 * DAY_MS + 30_000]),
            30_000
        );
    }

    #[test]
    fn confidence_needs_samples_time_days_and_recency() {
        let h = HOUR_MS;
        assert_eq!(confidence(120, h, 2, 0), Confidence::High);
        assert_eq!(confidence(120, h, 1, 0), Confidence::Medium, "one day");
        assert_eq!(confidence(119, h, 2, 0), Confidence::Medium, "few samples");
        assert_eq!(
            confidence(120, h, 2, 15 * DAY_MS),
            Confidence::Medium,
            "old"
        );
        assert_eq!(confidence(20, 10 * MINUTE_MS, 1, 0), Confidence::Medium);
        assert_eq!(confidence(19, h, 3, 0), Confidence::Low);
        assert_eq!(confidence(20, 9 * MINUTE_MS, 1, 0), Confidence::Low);
        assert_eq!(confidence(500, 4 * h, 5, 31 * DAY_MS), Confidence::Low);
    }

    #[test]
    fn a_brief_session_still_gives_a_low_confidence_estimate() {
        let h = history(&[("Slack", series(T0, 6))]);
        let l = h.learn(&["slack".to_string()], T0 + HOUR_MS).unwrap();
        assert_eq!(l.samples, 6);
        assert_eq!(l.observed_ms, 3 * MINUTE_MS);
        assert_eq!(l.need_bytes, 105 * MIB);
        assert_eq!(l.typical_bytes, 102 * MIB);
        assert_eq!(l.confidence, Confidence::Low);
        assert_eq!(l.provenance, Provenance::Estimated);
        assert_eq!(l.evidence(T0 + HOUR_MS), "seen for 3 min, last today");
    }

    #[test]
    fn long_watches_over_several_days_are_high_confidence() {
        let mut samples = series(T0, 100);
        samples.extend(series(T0 + DAY_MS, 100));
        let h = history(&[("Google Chrome", samples)]);
        let now = T0 + DAY_MS + 2 * HOUR_MS;
        let l = h.learn(&["google chrome".to_string()], now).unwrap();
        assert_eq!(l.samples, 200);
        assert_eq!(l.days, 2);
        assert_eq!(l.observed_ms, 200 * 30_000);
        assert_eq!(l.confidence, Confidence::High);
        assert_eq!(l.provenance, Provenance::Measured);
        // Nearest rank 190 of two copies of 100..=199 MB.
        assert_eq!(l.need_bytes, 194 * MIB);
        assert_eq!(l.p95_bytes, Some(194 * MIB));
        assert_eq!(l.peak_bytes, 199 * MIB);
        assert_eq!(
            l.evidence(now),
            "seen for 1 h 40 min over 2 days, last today"
        );
        // Unseen for three weeks: no longer high.
        let l = h
            .learn(&["Google Chrome".to_string()], now + 21 * DAY_MS)
            .unwrap();
        assert_eq!(l.confidence, Confidence::Medium);
        assert!(h.learn(&["Firefox".to_string()], now).is_none());
    }

    #[test]
    fn pruned_samples_fall_back_to_the_peak_with_low_confidence() {
        let mut h = history(&[("Figma", series(T0, 300))]);
        h.samples.clear();
        let l = h.learn(&["Figma".to_string()], T0 + DAY_MS).unwrap();
        assert_eq!(l.p95_bytes, None);
        assert_eq!(l.need_bytes, 399 * MIB);
        assert_eq!(l.confidence, Confidence::Low);
        assert_eq!(l.samples, 300);
    }

    #[test]
    fn groups_seen_together_add_up() {
        let h = history(&[
            ("Docker", vec![(T0, 100 * MIB), (T0 + 30_000, 200 * MIB)]),
            ("com.docker.backend", vec![(T0, 50 * MIB)]),
        ]);
        let l = h
            .learn(
                &["docker".to_string(), "com.docker.backend".to_string()],
                T0,
            )
            .unwrap();
        assert_eq!(l.names, ["Docker", "com.docker.backend"]);
        assert_eq!(l.need_bytes, 200 * MIB);
        assert_eq!(l.detailed_samples, 2);
        // 150 at T0, 200 at T0 + 30 s: the median (nearest rank) is 150.
        assert_eq!(l.typical_bytes, 150 * MIB);
    }

    #[test]
    fn loads_from_a_temporary_database() {
        use crate::history::store::Store;
        let dir = temp_dir("learned");
        let path = dir.join("history.sqlite3");
        assert!(AppHistory::load(&path).unwrap().is_none());
        assert!(!path.exists(), "loading never creates the database");
        let mut store = Store::open(&path).unwrap();
        let session = store.begin_session(T0, 1_000).unwrap();
        let (_, mut snap) = replay("calm");
        for i in 0..30u64 {
            snap.timestamp_unix_ms = T0 + i * 30_000;
            snap.app_groups = vec![AppGroup {
                name: "Slack".to_string(),
                kind: GroupKind::AppBundle,
                bundle_path: None,
                memory_bytes: (400 + i) * MIB,
                memory_kinds: vec![MemoryKind::Footprint],
                process_count: 4,
                unreadable_count: 0,
                pids: vec![1],
            }];
            store.insert_snapshot(session, &snap).unwrap();
        }
        drop(store);
        let h = AppHistory::load(&path).unwrap().unwrap();
        assert_eq!(h.names().collect::<Vec<_>>(), ["Slack"]);
        let l = h.learn(&["SLACK".to_string()], T0 + HOUR_MS).unwrap();
        assert_eq!(l.samples, 30);
        assert_eq!(l.observed_ms, 15 * MINUTE_MS);
        assert_eq!(l.need_bytes, 428 * MIB);
        assert_eq!(l.confidence, Confidence::Medium);
        let _ = std::fs::remove_dir_all(dir);
    }

    pub(crate) fn watch_run(name: &str, started: u64, duration_ms: u64, peak_mib: u64) -> WatchRun {
        WatchRun {
            name: name.to_string(),
            program: "llama-server".to_string(),
            started_unix_ms: started,
            duration_ms,
            interval_ms: 1000,
            samples: duration_ms / 1000,
            memory_kind: "footprint".to_string(),
            peak_bytes: peak_mib * MIB,
            peak_offset_ms: duration_ms / 2,
            typical_bytes: peak_mib * MIB * 9 / 10,
            p95_bytes: peak_mib * MIB,
            max_processes: 2,
            peak_pressure_score: 20,
            peak_pressure_level: crate::core::PressureLevel::Calm,
            exit_code: None,
            exit_signal: Some(2),
            interrupted: true,
            model_path: None,
            model_estimate_bytes: None,
            model_mapped_bytes: None,
        }
    }

    #[test]
    fn a_watched_run_comes_first_within_history() {
        let mut h = history(&[("llama-server", series(T0, 200))]);
        h.watch_runs = vec![
            watch_run("llama-server", T0 + 2 * DAY_MS, 10 * MINUTE_MS, 5000),
            watch_run("llama-server", T0 + DAY_MS, 10 * MINUTE_MS, 9000),
        ];
        let l = h
            .learn(&["Llama-Server".to_string()], T0 + 3 * DAY_MS)
            .unwrap();
        assert_eq!(l.method, Method::Watch);
        assert_eq!(l.need_bytes, 5000 * MIB, "the most recent run's peak");
        assert_eq!(l.confidence, Confidence::High);
        assert_eq!(l.provenance, Provenance::Measured);
        let w = l.watch.as_ref().unwrap();
        assert_eq!((w.runs, w.interrupted), (2, true));
        assert_eq!(l.evidence(T0 + 3 * DAY_MS), "watched for 10 min, yesterday");
        // Names are listed once, watched ones first.
        assert_eq!(h.names().collect::<Vec<_>>(), ["llama-server"]);
        // Without a watched run, the monitor's samples answer as before.
        h.watch_runs.clear();
        assert_eq!(
            h.learn(&["llama-server".to_string()], T0).unwrap().method,
            Method::Monitor
        );
    }

    #[test]
    fn short_and_failed_runs() {
        let mut short = watch_run("tool", T0, 5_000, 300);
        assert!(usable_run(&short));
        let h = AppHistory {
            watch_runs: vec![short.clone()],
            ..AppHistory::default()
        };
        let l = h.learn(&["tool".to_string()], T0).unwrap();
        assert_eq!(l.confidence, Confidence::Medium, "too short for high");
        // A quick failure (bad flag, missing model) is not a measurement of the app ...
        short.interrupted = false;
        short.exit_signal = None;
        short.exit_code = Some(1);
        assert!(!usable_run(&short));
        let ok = watch_run("tool", T0 - DAY_MS, 60_000, 800);
        let h = AppHistory {
            watch_runs: vec![short.clone(), ok],
            ..AppHistory::default()
        };
        // ... so the older, good run answers.
        assert_eq!(
            h.learn(&["tool".to_string()], T0).unwrap().need_bytes,
            800 * MIB
        );
        // A long run that ended with an error still measured the app.
        short.duration_ms = WATCH_FAILED_START_MS;
        assert!(usable_run(&short));
        short.samples = 0;
        assert!(!usable_run(&short));
    }

    #[test]
    fn watched_runs_round_trip_through_the_database() {
        let dir = temp_dir("learned-watch");
        let path = dir.join("history.sqlite3");
        let store = Store::open(&path).unwrap();
        let mut run = watch_run("My Server", T0, 120_000, 4096);
        run.model_path = Some("/m/model.gguf".to_string());
        run.model_estimate_bytes = Some(4000 * MIB);
        run.model_mapped_bytes = Some(3000 * MIB);
        run.peak_pressure_level = crate::core::PressureLevel::Strained;
        store.insert_watch_run(&run).unwrap();
        assert_eq!(store.watch_runs().unwrap(), [run.clone()]);
        drop(store);
        let h = AppHistory::load(&path).unwrap().unwrap();
        let l = h.learn(&["my server".to_string()], T0).unwrap();
        // The mapped model file the footprint left out is part of the need.
        assert_eq!((l.method, l.need_bytes), (Method::Watch, 7096 * MIB));
        assert_eq!(l.peak_bytes, 4096 * MIB);
        // Watched runs are kept like app profiles, 90 days.
        let mut store = Store::open(&path).unwrap();
        let retention = crate::history::store::Retention {
            max_age_ms: 7 * DAY_MS,
            max_bytes: 64 * MIB,
        };
        store.prune(T0 + 30 * DAY_MS, retention).unwrap();
        assert_eq!(store.watch_runs().unwrap().len(), 1);
        store.prune(T0 + 91 * DAY_MS, retention).unwrap();
        assert!(store.watch_runs().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
