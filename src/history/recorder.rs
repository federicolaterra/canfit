//! The recorder the live monitor feeds with every reading: it detects pressure events, stores a
//! compact snapshot every `snapshot_interval_secs`, and applies the retention policy now and then.
//! No daemon: nothing is recorded while the monitor is closed.

use std::path::Path;

use super::events::{Detector, Reading, Transition};
use super::store::{Retention, Store, StoredEvent};
use super::{HOUR_MS, HistoryError, HistorySettings, MINUTE_MS};
use crate::core::Snapshot;

/// How often the retention policy runs while recording.
pub const PRUNE_EVERY_MS: u64 = 10 * MINUTE_MS;

/// How far back the monitor's history panel looks.
pub const PANEL_WINDOW_MS: u64 = 24 * HOUR_MS;

pub struct Recorder {
    store: Store,
    session: i64,
    snapshot_interval_ms: u64,
    retention: Retention,
    detector: Detector,
    /// Database id of the event in progress.
    open_event: Option<i64>,
    last_snapshot_ms: Option<u64>,
    last_prune_ms: u64,
    /// Events of the last day, oldest first, the open one last: what the history panel shows.
    recent: Vec<StoredEvent>,
}

impl Recorder {
    /// Opens (or creates) the database at `path`, closes events a previous monitor left open,
    /// applies the retention policy and starts a session at `now_ms`.
    pub fn start(
        path: &Path,
        settings: &HistorySettings,
        now_ms: u64,
    ) -> Result<Recorder, HistoryError> {
        let mut store = Store::open(path)?;
        store.close_dangling_events()?;
        let retention = settings.retention();
        store.prune(now_ms, retention)?;
        let snapshot_interval_ms = settings.snapshot_interval_secs * 1000;
        let session = store.begin_session(now_ms, snapshot_interval_ms)?;
        let recent = store.events_since(now_ms.saturating_sub(PANEL_WINDOW_MS))?;
        Ok(Recorder {
            store,
            session,
            snapshot_interval_ms,
            retention,
            detector: Detector::default(),
            open_event: None,
            last_snapshot_ms: None,
            last_prune_ms: now_ms,
            recent,
        })
    }

    /// Recent events for the monitor's history panel, oldest first.
    pub fn recent(&self) -> &[StoredEvent] {
        &self.recent
    }

    /// How long a monitor was recording between `since_ms` and `until_ms`.
    pub fn recorded_ms(&self, since_ms: u64, until_ms: u64) -> Result<u64, HistoryError> {
        self.store.recorded_ms(since_ms, until_ms)
    }

    /// Takes one reading of the monitor.
    pub fn observe(&mut self, snapshot: &Snapshot) -> Result<(), HistoryError> {
        let now = snapshot.timestamp_unix_ms;
        let transition = self.detector.feed(Reading::from_snapshot(snapshot));
        let snapshot_due = self
            .last_snapshot_ms
            .is_none_or(|last| now.saturating_sub(last) >= self.snapshot_interval_ms);
        match transition {
            Transition::None | Transition::Continued => {
                if snapshot_due && let Some(id) = self.open_event {
                    // Keep the stored "last seen" close, in case canfit is killed.
                    let open = self.detector.open().expect("an open event has an episode");
                    self.store.update_event(id, open, None)?;
                    self.refresh_open(id, None);
                }
            }
            Transition::Started => {
                let open = self.detector.open().expect("just started");
                let id = self.store.insert_event(self.session, open)?;
                self.open_event = Some(id);
                self.refresh_open(id, None);
            }
            Transition::Peaked { .. } => {
                if let Some(id) = self.open_event {
                    let open = self.detector.open().expect("an open event has an episode");
                    self.store.update_event(id, open, None)?;
                    self.refresh_open(id, None);
                }
            }
            Transition::Ended { episode, end_ms } => {
                if let Some(id) = self.open_event.take() {
                    self.store
                        .update_event(id, &episode, Some((end_ms, false)))?;
                    self.refresh_open(id, Some((&episode, end_ms, false)));
                }
            }
        }
        if snapshot_due {
            self.store.insert_snapshot(self.session, snapshot)?;
            self.last_snapshot_ms = Some(now);
        }
        if now.saturating_sub(self.last_prune_ms) >= PRUNE_EVERY_MS {
            self.store.prune(now, self.retention)?;
            self.last_prune_ms = now;
            let since = now.saturating_sub(PANEL_WINDOW_MS);
            self.recent
                .retain(|e| e.end_unix_ms.unwrap_or(e.last_seen_unix_ms) >= since);
        }
        Ok(())
    }

    /// The monitor quits: an event still going on is stored as cut short (or as ended, when
    /// pressure had already calmed down).
    pub fn finish(mut self, now_ms: u64) -> Result<(), HistoryError> {
        if let (Some(id), Some((episode, end, cut_short))) =
            (self.open_event.take(), self.detector.finish())
        {
            self.store
                .update_event(id, &episode, Some((end, cut_short)))?;
        }
        self.store.touch_session(self.session, now_ms)
    }

    /// Mirrors the open event (or its end) into the panel's list without a database read.
    fn refresh_open(&mut self, id: i64, ended: Option<(&super::events::Episode, u64, bool)>) {
        let (episode, end) = match ended {
            Some((e, end, cut)) => (e, Some((end, cut))),
            None => match self.detector.open() {
                Some(e) => (e, None),
                None => return,
            },
        };
        let event = StoredEvent {
            id,
            start_unix_ms: episode.start_ms,
            end_unix_ms: end.map(|(t, _)| t),
            last_seen_unix_ms: episode.last_seen_ms,
            cut_short: end.is_some_and(|(_, cut)| cut),
            level: episode.peak_level,
            peak_score: episode.peak_score,
            peak_unix_ms: episode.peak_ms,
            reason_signal: episode.signal.clone(),
            reason: episode.message.clone(),
            culprits: episode.culprits.clone(),
        };
        match self.recent.iter_mut().find(|e| e.id == id) {
            Some(slot) => *slot = event,
            None => self.recent.push(event),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::classify::tests::{desktop, snapshot_with};
    use crate::core::{PressureLevel, PressureSignal};
    use crate::history::tests::temp_dir;

    fn at(ts_s: u64, level: PressureLevel, score: u8) -> Snapshot {
        let mut s = snapshot_with(desktop());
        s.timestamp_unix_ms = ts_s * 1000;
        s.pressure.level = level;
        s.pressure.score = score;
        s.pressure.main_reason.signal = PressureSignal::LowAvailable;
        s
    }

    fn settings() -> HistorySettings {
        HistorySettings {
            snapshot_interval_secs: 10,
            ..HistorySettings::default()
        }
    }

    #[test]
    fn a_monitor_run_stores_snapshots_and_events() {
        let path = temp_dir("recorder-run").join("history.sqlite3");
        let t0 = 1_700_000_000;
        let mut r = Recorder::start(&path, &settings(), t0 * 1000).unwrap();
        // 30 s calm, 20 s strained peaking critical, then calm for a minute: one per second.
        for t in 0..110 {
            let (level, score) = match t {
                30..=39 => (PressureLevel::Strained, 50),
                40..=49 => (PressureLevel::Critical, 80),
                _ => (PressureLevel::Calm, 10),
            };
            r.observe(&at(t0 + t, level, score)).unwrap();
        }
        assert_eq!(r.recent().len(), 1);
        assert!(!r.recent()[0].ongoing());
        r.finish((t0 + 110) * 1000).unwrap();

        let store = Store::open_existing(&path).unwrap().unwrap();
        assert_eq!(store.snapshot_count().unwrap(), 11);
        let events = store.events_since(0).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.start_unix_ms, (t0 + 30) * 1000);
        assert_eq!(e.end_unix_ms, Some((t0 + 50) * 1000));
        assert_eq!(e.level, PressureLevel::Critical);
        assert_eq!(e.peak_unix_ms, (t0 + 40) * 1000);
        assert!(!e.cut_short);
        assert!(!e.culprits.is_empty());
        assert_eq!(e.reason_signal, "low_available");
        assert_eq!(
            store.recorded_ms(0, u64::MAX).unwrap(),
            110 * 1000,
            "the session covers the whole run"
        );
    }

    #[test]
    fn quitting_mid_event_and_a_crash_are_both_recorded() {
        let path = temp_dir("recorder-cut").join("history.sqlite3");
        let t0 = 1_700_000_000;
        let mut r = Recorder::start(&path, &settings(), t0 * 1000).unwrap();
        for t in 0..20 {
            r.observe(&at(t0 + t, PressureLevel::Strained, 45)).unwrap();
        }
        assert!(r.recent()[0].ongoing());
        r.finish((t0 + 20) * 1000).unwrap();

        // A second run that is killed mid-event: no finish.
        let mut r = Recorder::start(&path, &settings(), (t0 + 100) * 1000).unwrap();
        for t in 100..125 {
            r.observe(&at(t0 + t, PressureLevel::Critical, 90)).unwrap();
        }
        assert_eq!(r.recent().len(), 2, "the panel shows the earlier event too");
        drop(r);

        // The next start closes the dangling event at the last time it was stored.
        let r = Recorder::start(&path, &settings(), (t0 + 200) * 1000).unwrap();
        let events = r.recent();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.cut_short && !e.ongoing()));
        assert_eq!(events[0].end_unix_ms, Some((t0 + 19) * 1000));
        // Stored with the last snapshot write (every 10 s): seen until at least 120 s.
        let end = events[1].end_unix_ms.unwrap();
        assert!(
            end >= (t0 + 120) * 1000 && end <= (t0 + 124) * 1000,
            "{end}"
        );
    }

    #[test]
    fn history_survives_restarts() {
        let path = temp_dir("recorder-restart").join("history.sqlite3");
        let t0 = 1_700_000_000;
        for run in 0..3u64 {
            let start = t0 + run * 1000;
            let mut r = Recorder::start(&path, &settings(), start * 1000).unwrap();
            for t in 0..60 {
                let level = if (10..20).contains(&t) {
                    PressureLevel::Strained
                } else {
                    PressureLevel::Calm
                };
                r.observe(&at(start + t, level, 45)).unwrap();
            }
            r.finish((start + 60) * 1000).unwrap();
        }
        let store = Store::open_existing(&path).unwrap().unwrap();
        assert_eq!(store.events_since(0).unwrap().len(), 3);
        assert_eq!(store.snapshot_count().unwrap(), 18);
    }

    #[test]
    fn a_newer_database_is_reported_not_replaced() {
        let path = temp_dir("recorder-newer").join("history.sqlite3");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "PRAGMA application_id = {}; CREATE TABLE future (x); PRAGMA user_version = 99;",
                crate::history::schema::APPLICATION_ID
            ))
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let err = match Recorder::start(&path, &settings(), 0) {
            Err(e) => e,
            Ok(_) => panic!("a newer schema must be refused"),
        };
        assert!(err.to_string().contains("newer canfit"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
