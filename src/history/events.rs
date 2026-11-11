//! Pressure-event detection. Pure: fed one reading at a time, it says when an event starts, gets
//! worse and ends, so it is tested on synthetic sequences.
//!
//! An event is one stretch of **strained** or **critical** pressure. It starts on the first
//! non-calm reading and ends once pressure has been calm for [`CALM_GRACE_MS`]; a short calm dip
//! inside that window does not split the event. The event keeps its worst moment (peak level,
//! then score) with the reason and the apps using the most memory at that moment.

use serde::{Deserialize, Serialize};

use crate::core::{PressureLevel, Snapshot};

/// Calm time needed before an event counts as over.
pub const CALM_GRACE_MS: u64 = 30_000;

/// Apps named as culprits of an event.
pub const CULPRITS: usize = 3;

/// An app using a lot of memory at the worst moment of an event (measured).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Culprit {
    pub name: String,
    pub memory_bytes: u64,
}

/// What the detector needs from one reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub ts_ms: u64,
    pub level: PressureLevel,
    pub score: u8,
    /// The signal driving the score, in snake case (`low_available`, ...).
    pub signal: String,
    pub message: String,
    pub culprits: Vec<Culprit>,
}

impl Reading {
    pub fn from_snapshot(s: &Snapshot) -> Reading {
        let signal = serde_json::to_value(s.pressure.main_reason.signal)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        Reading {
            ts_ms: s.timestamp_unix_ms,
            level: s.pressure.level,
            score: s.pressure.score,
            signal,
            message: s.pressure.main_reason.message.clone(),
            culprits: s
                .app_groups
                .iter()
                .filter(|g| g.memory_bytes > 0)
                .take(CULPRITS)
                .map(|g| Culprit {
                    name: g.name.clone(),
                    memory_bytes: g.memory_bytes,
                })
                .collect(),
        }
    }
}

/// An event in progress (or just ended).
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub start_ms: u64,
    /// The last reading the monitor took during the event.
    pub last_seen_ms: u64,
    pub peak_level: PressureLevel,
    pub peak_score: u8,
    pub peak_ms: u64,
    pub signal: String,
    pub message: String,
    pub culprits: Vec<Culprit>,
}

impl Episode {
    fn new(r: Reading) -> Episode {
        Episode {
            start_ms: r.ts_ms,
            last_seen_ms: r.ts_ms,
            peak_level: r.level,
            peak_score: r.score,
            peak_ms: r.ts_ms,
            signal: r.signal,
            message: r.message,
            culprits: r.culprits,
        }
    }
}

/// What one reading changed.
#[derive(Debug, Clone, PartialEq)]
pub enum Transition {
    /// Nothing to store.
    None,
    /// A new event began with this reading.
    Started,
    /// The event reached a new worst moment; `level_up` when its level rose.
    Peaked { level_up: bool },
    /// The event goes on, no worse than before (calm dips inside the grace window included).
    Continued,
    /// The event ended at `end_ms`, the first calm reading of the calm stretch.
    Ended { episode: Episode, end_ms: u64 },
}

#[derive(Debug, Clone)]
pub struct Detector {
    open: Option<Episode>,
    calm_since: Option<u64>,
    grace_ms: u64,
}

impl Default for Detector {
    fn default() -> Self {
        Detector::new(CALM_GRACE_MS)
    }
}

impl Detector {
    pub fn new(grace_ms: u64) -> Detector {
        Detector {
            open: None,
            calm_since: None,
            grace_ms,
        }
    }

    /// The event in progress, if any.
    pub fn open(&self) -> Option<&Episode> {
        self.open.as_ref()
    }

    pub fn feed(&mut self, r: Reading) -> Transition {
        if r.level == PressureLevel::Calm {
            let Some(open) = &mut self.open else {
                return Transition::None;
            };
            open.last_seen_ms = r.ts_ms;
            let since = *self.calm_since.get_or_insert(r.ts_ms);
            if r.ts_ms.saturating_sub(since) < self.grace_ms {
                return Transition::Continued;
            }
            let episode = self.open.take().expect("checked above");
            self.calm_since = None;
            return Transition::Ended {
                episode,
                end_ms: since,
            };
        }
        self.calm_since = None;
        let Some(open) = &mut self.open else {
            self.open = Some(Episode::new(r));
            return Transition::Started;
        };
        open.last_seen_ms = r.ts_ms;
        if (r.level, r.score) > (open.peak_level, open.peak_score) {
            let level_up = r.level > open.peak_level;
            open.peak_level = r.level;
            open.peak_score = r.score;
            open.peak_ms = r.ts_ms;
            open.signal = r.signal;
            open.message = r.message;
            open.culprits = r.culprits;
            Transition::Peaked { level_up }
        } else {
            Transition::Continued
        }
    }

    /// The monitor stops watching. Returns the open event with its end and whether it was cut
    /// short (pressure had not calmed down yet).
    pub fn finish(&mut self) -> Option<(Episode, u64, bool)> {
        let episode = self.open.take()?;
        let calm_since = self.calm_since.take();
        Some(match calm_since {
            Some(since) => (episode, since, false),
            None => {
                let end = episode.last_seen_ms;
                (episode, end, true)
            }
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use PressureLevel::{Calm, Critical, Strained};

    pub(crate) fn reading(ts_s: u64, level: PressureLevel, score: u8, top: &str) -> Reading {
        Reading {
            ts_ms: ts_s * 1000,
            level,
            score,
            signal: "low_available".into(),
            message: format!("score {score}"),
            culprits: vec![Culprit {
                name: top.into(),
                memory_bytes: u64::from(score) << 20,
            }],
        }
    }

    /// Feeds `(second, level, score)` readings and collects the ended events.
    fn run(levels: &[(u64, PressureLevel, u8)]) -> (Vec<(Episode, u64)>, Detector) {
        let mut d = Detector::default();
        let mut ended = Vec::new();
        for &(t, level, score) in levels {
            if let Transition::Ended { episode, end_ms } = d.feed(reading(t, level, score, "app")) {
                ended.push((episode, end_ms));
            }
        }
        (ended, d)
    }

    #[test]
    fn calm_produces_nothing() {
        let mut d = Detector::default();
        for t in 0..100 {
            assert_eq!(d.feed(reading(t, Calm, 5, "a")), Transition::None);
        }
        assert!(d.finish().is_none());
    }

    #[test]
    fn one_event_with_its_worst_moment() {
        let mut d = Detector::default();
        assert_eq!(d.feed(reading(10, Strained, 45, "a")), Transition::Started);
        assert_eq!(
            d.feed(reading(11, Strained, 50, "b")),
            Transition::Peaked { level_up: false }
        );
        assert_eq!(
            d.feed(reading(12, Critical, 80, "chrome")),
            Transition::Peaked { level_up: true }
        );
        assert_eq!(
            d.feed(reading(13, Strained, 60, "c")),
            Transition::Continued
        );
        // Calm from 20 s; the event ends once it has been calm for the grace window.
        assert_eq!(d.feed(reading(20, Calm, 10, "d")), Transition::Continued);
        assert_eq!(d.feed(reading(49, Calm, 10, "d")), Transition::Continued);
        match d.feed(reading(50, Calm, 10, "d")) {
            Transition::Ended { episode, end_ms } => {
                assert_eq!(episode.start_ms, 10_000);
                assert_eq!(end_ms, 20_000);
                assert_eq!(episode.peak_level, Critical);
                assert_eq!(episode.peak_score, 80);
                assert_eq!(episode.peak_ms, 12_000);
                assert_eq!(episode.culprits[0].name, "chrome");
                assert_eq!(episode.message, "score 80");
            }
            other => panic!("expected the end, got {other:?}"),
        }
        assert!(d.open().is_none());
    }

    #[test]
    fn short_calm_dips_do_not_split_an_event() {
        let mut seq = vec![(0, Strained, 45), (5, Calm, 10), (20, Strained, 50)];
        seq.extend((21..=60).map(|t| (t, Calm, 10)));
        seq.extend([(100, Critical, 75), (110, Calm, 5), (140, Calm, 5)]);
        let (ended, d) = run(&seq);
        assert_eq!(ended.len(), 2);
        assert_eq!((ended[0].0.start_ms, ended[0].1), (0, 21_000));
        assert_eq!(ended[0].0.peak_score, 50);
        assert_eq!((ended[1].0.start_ms, ended[1].1), (100_000, 110_000));
        assert_eq!(ended[1].0.peak_level, Critical);
        assert!(d.open().is_none());
    }

    #[test]
    fn flapping_pressure_is_one_event() {
        // Strained and calm every other second for ten minutes.
        let seq: Vec<_> = (0..600)
            .map(|t| {
                if t % 2 == 0 {
                    (t, Strained, 41)
                } else {
                    (t, Calm, 39)
                }
            })
            .collect();
        let (ended, mut d) = run(&seq);
        assert!(ended.is_empty());
        let (episode, end, cut_short) = d.finish().unwrap();
        assert_eq!(episode.start_ms, 0);
        // The last reading was a calm dip that had not lasted the grace window yet: the event is
        // complete and ended at that dip.
        assert_eq!((end, cut_short), (599_000, false));
    }

    #[test]
    fn stopping_mid_event_cuts_it_short() {
        let (_, mut d) = run(&[(0, Strained, 45), (1, Critical, 90), (2, Critical, 85)]);
        let (episode, end, cut_short) = d.finish().unwrap();
        assert_eq!((episode.start_ms, end, cut_short), (0, 2_000, true));
        assert_eq!(episode.peak_score, 90);
        assert!(d.finish().is_none());
    }

    #[test]
    fn readings_come_from_snapshots() {
        let (_, s) = crate::core::sampler::tests::replay("critical");
        let r = Reading::from_snapshot(&s);
        assert_eq!(r.level, s.pressure.level);
        assert_eq!(r.ts_ms, s.timestamp_unix_ms);
        assert!(!r.signal.is_empty() && !r.signal.contains('"'));
        assert!(r.culprits.len() <= CULPRITS);
        assert!(
            r.culprits
                .windows(2)
                .all(|w| w[0].memory_bytes >= w[1].memory_bytes)
        );
    }
}
