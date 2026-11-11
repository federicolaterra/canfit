//! The history database: writes from the recorder, reads for `canfit history` and the monitor's
//! history panel, and the retention policy (maximum age and size cap).
//!
//! Every method takes the time it should act at, so retention is tested with a fake clock.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(test)]
use rusqlite::OptionalExtension;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

use super::HistoryError;
use super::events::{Culprit, Episode};
use super::schema;
use crate::core::{MemoryKind, PressureLevel, Snapshot};

/// How long a write waits for another canfit holding the database (a second monitor, or
/// `canfit history` reading).
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// App groups smaller than this are not sampled: they would only bloat the database.
pub const APP_SAMPLE_MIN_BYTES: u64 = 100 * 1024 * 1024;
/// At most this many app groups are sampled per stored snapshot.
pub const APP_SAMPLES_PER_SNAPSHOT: usize = 8;
/// App profiles not seen for this long are dropped.
pub const PROFILE_MAX_AGE_MS: u64 = 90 * super::DAY_MS;

/// What the retention policy keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Records older than this go.
    pub max_age_ms: u64,
    /// Above this database size, the oldest records go first.
    pub max_bytes: u64,
}

/// One stored pressure event, as `canfit history` shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredEvent {
    pub id: i64,
    pub start_unix_ms: u64,
    /// `None` while the event is still going on.
    pub end_unix_ms: Option<u64>,
    /// The last time the monitor saw the event.
    pub last_seen_unix_ms: u64,
    /// The monitor stopped before pressure calmed down; the event lasted at least this long.
    pub cut_short: bool,
    pub level: PressureLevel,
    pub peak_score: u8,
    pub peak_unix_ms: u64,
    pub reason_signal: String,
    pub reason: String,
    pub culprits: Vec<Culprit>,
}

impl StoredEvent {
    /// The known duration: up to the end, or up to the last reading while it goes on.
    pub fn duration_ms(&self) -> u64 {
        self.end_unix_ms
            .unwrap_or(self.last_seen_unix_ms)
            .saturating_sub(self.start_unix_ms)
    }

    pub fn ongoing(&self) -> bool {
        self.end_unix_ms.is_none()
    }
}

/// The memory canfit has seen an app use: running totals kept for 90 days, read by history-based
/// app estimates (spec §9, ADR-021) and `canfit history --apps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppProfile {
    pub name: String,
    pub memory_kind: String,
    pub samples: u64,
    pub typical_bytes: u64,
    pub peak_bytes: u64,
    pub peak_ms: u64,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
}

/// What a pruning pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub snapshots: usize,
    pub events: usize,
    pub app_samples: usize,
    /// Rounds of oldest-first deletion the size cap needed.
    pub size_rounds: usize,
}

pub fn level_code(level: PressureLevel) -> i64 {
    match level {
        PressureLevel::Calm => 0,
        PressureLevel::Strained => 1,
        PressureLevel::Critical => 2,
    }
}

pub fn level_from_code(code: i64) -> PressureLevel {
    match code {
        i64::MIN..=0 => PressureLevel::Calm,
        1 => PressureLevel::Strained,
        _ => PressureLevel::Critical,
    }
}

fn kind_label(kind: MemoryKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Clamps an unsigned value into SQLite's signed integers.
fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn uint(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

const PROFILE_COLUMNS: &str = "SELECT name, memory_kind, samples, sum_bytes, peak_bytes, \
     peak_ms, first_seen_ms, last_seen_ms FROM app_profiles";

fn profile_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AppProfile> {
    let samples = uint(r.get(2)?);
    let sum = uint(r.get(3)?);
    Ok(AppProfile {
        name: r.get(0)?,
        memory_kind: r.get(1)?,
        samples,
        typical_bytes: sum.checked_div(samples).unwrap_or(0),
        peak_bytes: uint(r.get(4)?),
        peak_ms: uint(r.get(5)?),
        first_seen_ms: uint(r.get(6)?),
        last_seen_ms: uint(r.get(7)?),
    })
}

/// One run measured by `canfit watch` (ADR-023).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WatchRun {
    /// The name later fit checks find it by (`--name`, or the program's name).
    pub name: String,
    /// The program's file name, without its arguments.
    pub program: String,
    pub started_unix_ms: u64,
    pub duration_ms: u64,
    pub interval_ms: u64,
    pub samples: u64,
    /// How memory was measured (`footprint`, `pss`, `private_working_set`, ...).
    pub memory_kind: String,
    /// Largest sampled memory of the whole process tree.
    pub peak_bytes: u64,
    pub peak_offset_ms: u64,
    /// Median of the samples.
    pub typical_bytes: u64,
    /// 95th percentile of the samples (nearest rank).
    pub p95_bytes: u64,
    pub max_processes: u64,
    pub peak_pressure_score: u8,
    pub peak_pressure_level: PressureLevel,
    /// The command's exit code, when it exited on its own.
    pub exit_code: Option<i32>,
    /// The signal that ended it (Unix).
    pub exit_signal: Option<i32>,
    /// True when canfit received Ctrl-C (or a termination signal) while it ran.
    pub interrupted: bool,
    /// The GGUF model canfit identified in its arguments, and canfit's estimate for it
    /// (without the margin).
    pub model_path: Option<String>,
    pub model_estimate_bytes: Option<u64>,
    /// The model's weights when the command mapped them from the file and the OS's measure
    /// leaves mapped file pages out (macOS footprint, Windows private working set): not in the
    /// peak, but needed to run it (ADR-023).
    pub model_mapped_bytes: Option<u64>,
}

const WATCH_COLUMNS: &str = "SELECT name, program, started_ms, duration_ms, interval_ms, samples, \
     memory_kind, peak_bytes, peak_offset_ms, typical_bytes, p95_bytes, max_processes, \
     peak_pressure_score, peak_pressure_level, exit_code, exit_signal, interrupted, model_path, \
     model_estimate_bytes, model_mapped_bytes FROM watch_runs";

fn watch_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WatchRun> {
    Ok(WatchRun {
        name: r.get(0)?,
        program: r.get(1)?,
        started_unix_ms: uint(r.get(2)?),
        duration_ms: uint(r.get(3)?),
        interval_ms: uint(r.get(4)?),
        samples: uint(r.get(5)?),
        memory_kind: r.get(6)?,
        peak_bytes: uint(r.get(7)?),
        peak_offset_ms: uint(r.get(8)?),
        typical_bytes: uint(r.get(9)?),
        p95_bytes: uint(r.get(10)?),
        max_processes: uint(r.get(11)?),
        peak_pressure_score: u8::try_from(r.get::<_, i64>(12)?.clamp(0, 100)).unwrap_or(0),
        peak_pressure_level: level_from_code(r.get(13)?),
        exit_code: r.get(14)?,
        exit_signal: r.get(15)?,
        interrupted: r.get::<_, i64>(16)? != 0,
        model_path: r.get(17)?,
        model_estimate_bytes: r.get::<_, Option<i64>>(18)?.map(uint),
        model_mapped_bytes: r.get::<_, Option<i64>>(19)?.map(uint),
    })
}

pub struct Store {
    conn: Connection,
    path: PathBuf,
    /// The schema version the file had when it was opened (after any upgrade).
    version: u32,
}

impl Store {
    /// Opens the database at `path`, creating it (and its folder) when missing, and migrates it
    /// to the latest schema. A newer or foreign database is refused untouched. Refused, without
    /// touching the file system, when recording is off for this run ([`super::recording_off`]).
    pub fn open(path: &Path) -> Result<Store, HistoryError> {
        if let Some(reason) = super::recording_off() {
            return Err(HistoryError::RecordingOff { reason });
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| HistoryError::Io {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })?;
        }
        Self::open_with(path, OpenFlags::default())
    }

    /// Opens an existing database (migrating it if older); `None` when there is none yet.
    /// Never creates a file. When recording is off for this run, the file is opened read-only
    /// and never migrated: an older schema is read as it is.
    pub fn open_existing(path: &Path) -> Result<Option<Store>, HistoryError> {
        if !path.exists() {
            return Ok(None);
        }
        if super::recording_off().is_some() {
            return Self::open_read_only(path).map(Some);
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        Self::open_with(path, flags).map(Some)
    }

    /// Opens an existing database for reading only: no migration, no write of any kind.
    pub fn open_read_only(path: &Path) -> Result<Store, HistoryError> {
        let sql = |e: rusqlite::Error| HistoryError::sqlite(path, &e);
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).map_err(sql)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sql)?;
        let version = schema::check_readable(&conn, path)?;
        Ok(Store {
            conn,
            path: path.to_path_buf(),
            version,
        })
    }

    fn open_with(path: &Path, flags: OpenFlags) -> Result<Store, HistoryError> {
        let sql = |e: rusqlite::Error| HistoryError::sqlite(path, &e);
        let mut conn = Connection::open_with_flags(path, flags).map_err(sql)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sql)?;
        let version = schema::migrate(&mut conn, path)?;
        Ok(Store {
            conn,
            path: path.to_path_buf(),
            version,
        })
    }

    fn err(&self, e: rusqlite::Error) -> HistoryError {
        HistoryError::sqlite(&self.path, &e).with_schema(self.version, schema::LATEST_VERSION)
    }

    /// Starts a recording session (one run of the monitor).
    pub fn begin_session(&self, now_ms: u64, interval_ms: u64) -> Result<i64, HistoryError> {
        self.conn
            .execute(
                "INSERT INTO sessions (started_ms, last_seen_ms, interval_ms, canfit_version) \
                 VALUES (?1, ?1, ?2, ?3)",
                params![int(now_ms), int(interval_ms), env!("CARGO_PKG_VERSION")],
            )
            .map_err(|e| self.err(e))?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn touch_session(&self, session: i64, now_ms: u64) -> Result<(), HistoryError> {
        self.conn
            .execute(
                "UPDATE sessions SET last_seen_ms = max(last_seen_ms, ?2) WHERE id = ?1",
                params![session, int(now_ms)],
            )
            .map_err(|e| self.err(e))?;
        Ok(())
    }

    /// Closes events a previous monitor left open (it crashed or was killed): they end at the
    /// last time it saw them and are marked cut short. Returns how many.
    pub fn close_dangling_events(&self) -> Result<usize, HistoryError> {
        self.conn
            .execute(
                "UPDATE events SET end_ms = last_seen_ms, cut_short = 1 WHERE end_ms IS NULL",
                [],
            )
            .map_err(|e| self.err(e))
    }

    /// Stores one compact snapshot and the memory of its largest apps.
    pub fn insert_snapshot(&mut self, session: i64, s: &Snapshot) -> Result<(), HistoryError> {
        let ts = int(s.timestamp_unix_ms);
        let kind = kind_label(s.process_memory_kind());
        let tx = self.conn.transaction().map_err(|e| {
            HistoryError::sqlite(&self.path, &e).with_schema(self.version, schema::LATEST_VERSION)
        })?;
        tx.execute(
            "INSERT INTO snapshots (ts_ms, session_id, memory_total, memory_available, swap_used, \
             compressed, cpu_permille, pressure_score, pressure_level) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                ts,
                session,
                int(s.memory.total_bytes),
                int(s.memory.available_bytes),
                int(s.swap.used_bytes),
                s.memory.compressed_bytes.map(int),
                s.cpu
                    .usage_percent
                    .map(|p| (f64::from(p) * 10.0).round() as i64),
                i64::from(s.pressure.score),
                level_code(s.pressure.level),
            ],
        )
        .map_err(|e| {
            HistoryError::sqlite(&self.path, &e).with_schema(self.version, schema::LATEST_VERSION)
        })?;
        for g in s
            .app_groups
            .iter()
            .filter(|g| g.memory_bytes >= APP_SAMPLE_MIN_BYTES)
            .take(APP_SAMPLES_PER_SNAPSHOT)
        {
            let bytes = int(g.memory_bytes);
            tx.execute(
                "INSERT INTO app_samples (ts_ms, name, memory_bytes, process_count) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![ts, g.name, bytes, int(g.process_count as u64)],
            )
            .map_err(|e| {
                HistoryError::sqlite(&self.path, &e)
                    .with_schema(self.version, schema::LATEST_VERSION)
            })?;
            tx.execute(
                "INSERT INTO app_profiles (name, memory_kind, samples, sum_bytes, peak_bytes, \
                 peak_ms, first_seen_ms, last_seen_ms) VALUES (?1, ?2, 1, ?3, ?3, ?4, ?4, ?4) \
                 ON CONFLICT (name) DO UPDATE SET \
                   memory_kind = excluded.memory_kind, \
                   samples = samples + 1, \
                   sum_bytes = min(sum_bytes + excluded.sum_bytes, 9223372036854775807), \
                   peak_ms = CASE WHEN excluded.peak_bytes > peak_bytes \
                                  THEN excluded.peak_ms ELSE peak_ms END, \
                   peak_bytes = max(peak_bytes, excluded.peak_bytes), \
                   last_seen_ms = max(last_seen_ms, excluded.last_seen_ms)",
                params![g.name, kind, bytes, ts],
            )
            .map_err(|e| {
                HistoryError::sqlite(&self.path, &e)
                    .with_schema(self.version, schema::LATEST_VERSION)
            })?;
        }
        tx.execute(
            "UPDATE sessions SET last_seen_ms = max(last_seen_ms, ?2) WHERE id = ?1",
            params![session, ts],
        )
        .map_err(|e| {
            HistoryError::sqlite(&self.path, &e).with_schema(self.version, schema::LATEST_VERSION)
        })?;
        tx.commit().map_err(|e| {
            HistoryError::sqlite(&self.path, &e).with_schema(self.version, schema::LATEST_VERSION)
        })
    }

    /// Stores a new event and returns its id.
    pub fn insert_event(&self, session: i64, e: &Episode) -> Result<i64, HistoryError> {
        let culprits = serde_json::to_string(&e.culprits).unwrap_or_else(|_| "[]".to_string());
        self.conn
            .execute(
                "INSERT INTO events (session_id, start_ms, end_ms, last_seen_ms, cut_short, \
                 peak_level, peak_score, peak_ms, reason_signal, reason_message, culprits) \
                 VALUES (?1, ?2, NULL, ?3, 0, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session,
                    int(e.start_ms),
                    int(e.last_seen_ms),
                    level_code(e.peak_level),
                    i64::from(e.peak_score),
                    int(e.peak_ms),
                    e.signal,
                    e.message,
                    culprits
                ],
            )
            .map_err(|err| self.err(err))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Updates an event's worst moment and last reading; `end` closes it (`Some((end, cut_short))`).
    pub fn update_event(
        &self,
        id: i64,
        e: &Episode,
        end: Option<(u64, bool)>,
    ) -> Result<(), HistoryError> {
        let culprits = serde_json::to_string(&e.culprits).unwrap_or_else(|_| "[]".to_string());
        self.conn
            .execute(
                "UPDATE events SET last_seen_ms = ?2, peak_level = ?3, peak_score = ?4, \
                 peak_ms = ?5, reason_signal = ?6, reason_message = ?7, culprits = ?8, \
                 end_ms = ?9, cut_short = ?10 WHERE id = ?1",
                params![
                    id,
                    int(e.last_seen_ms),
                    level_code(e.peak_level),
                    i64::from(e.peak_score),
                    int(e.peak_ms),
                    e.signal,
                    e.message,
                    culprits,
                    end.map(|(t, _)| int(t)),
                    end.is_some_and(|(_, cut)| cut),
                ],
            )
            .map_err(|err| self.err(err))?;
        Ok(())
    }

    /// Events that were going on at any time since `since_ms`, oldest first.
    pub fn events_since(&self, since_ms: u64) -> Result<Vec<StoredEvent>, HistoryError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, start_ms, end_ms, last_seen_ms, cut_short, peak_level, peak_score, \
                 peak_ms, reason_signal, reason_message, culprits FROM events \
                 WHERE coalesce(end_ms, last_seen_ms) >= ?1 ORDER BY start_ms, id",
            )
            .map_err(|e| self.err(e))?;
        let rows = stmt
            .query_map(params![int(since_ms)], |r| {
                let culprits: String = r.get(10)?;
                Ok(StoredEvent {
                    id: r.get(0)?,
                    start_unix_ms: uint(r.get(1)?),
                    end_unix_ms: r.get::<_, Option<i64>>(2)?.map(uint),
                    last_seen_unix_ms: uint(r.get(3)?),
                    cut_short: r.get(4)?,
                    level: level_from_code(r.get(5)?),
                    peak_score: u8::try_from(r.get::<_, i64>(6)?.clamp(0, 100)).unwrap_or(0),
                    peak_unix_ms: uint(r.get(7)?),
                    reason_signal: r.get(8)?,
                    reason: r.get(9)?,
                    culprits: serde_json::from_str(&culprits).unwrap_or_default(),
                })
            })
            .map_err(|e| self.err(e))?;
        rows.collect::<Result<_, _>>().map_err(|e| self.err(e))
    }

    /// How long a monitor was recording between `since_ms` and `until_ms`, in milliseconds.
    /// Overlapping sessions (two monitors at once) count once.
    pub fn recorded_ms(&self, since_ms: u64, until_ms: u64) -> Result<u64, HistoryError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT started_ms, last_seen_ms FROM sessions \
                 WHERE last_seen_ms >= ?1 AND started_ms <= ?2 ORDER BY started_ms",
            )
            .map_err(|e| self.err(e))?;
        let spans: Vec<(u64, u64)> = stmt
            .query_map(params![int(since_ms), int(until_ms)], |r| {
                Ok((uint(r.get(0)?), uint(r.get(1)?)))
            })
            .map_err(|e| self.err(e))?
            .collect::<Result<_, _>>()
            .map_err(|e| self.err(e))?;
        let mut total = 0;
        let mut covered_to = since_ms;
        for (start, end) in spans {
            let start = start.max(covered_to);
            let end = end.min(until_ms);
            if end > start {
                total += end - start;
                covered_to = end;
            }
        }
        Ok(total)
    }

    /// The stored profile of one app.
    #[cfg(test)]
    pub fn app_profile(&self, name: &str) -> Result<Option<AppProfile>, HistoryError> {
        self.conn
            .query_row(
                &format!("{PROFILE_COLUMNS} WHERE name = ?1"),
                params![name],
                profile_row,
            )
            .optional()
            .map_err(|e| self.err(e))
    }

    /// Every stored app profile, most recently seen first.
    pub fn app_profiles(&self) -> Result<Vec<AppProfile>, HistoryError> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "{PROFILE_COLUMNS} ORDER BY last_seen_ms DESC, name"
            ))
            .map_err(|e| self.err(e))?;
        stmt.query_map([], profile_row)
            .map_err(|e| self.err(e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| self.err(e))
    }

    /// The stored samples `(time, memory)` of the app groups named `name`, oldest first.
    pub fn app_samples(&self, name: &str) -> Result<Vec<(u64, u64)>, HistoryError> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts_ms, memory_bytes FROM app_samples WHERE name = ?1 ORDER BY ts_ms")
            .map_err(|e| self.err(e))?;
        stmt.query_map(params![name], |r| Ok((uint(r.get(0)?), uint(r.get(1)?))))
            .map_err(|e| self.err(e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| self.err(e))
    }

    /// Stores one watched run. Returns its id.
    pub fn insert_watch_run(&self, run: &WatchRun) -> Result<i64, HistoryError> {
        self.conn
            .execute(
                "INSERT INTO watch_runs (name, program, started_ms, duration_ms, interval_ms, \
                 samples, memory_kind, peak_bytes, peak_offset_ms, typical_bytes, p95_bytes, \
                 max_processes, peak_pressure_score, peak_pressure_level, exit_code, \
                 exit_signal, interrupted, model_path, model_estimate_bytes, \
                 model_mapped_bytes, canfit_version) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                 ?16, ?17, ?18, ?19, ?20, ?21)",
                params![
                    run.name,
                    run.program,
                    int(run.started_unix_ms),
                    int(run.duration_ms),
                    int(run.interval_ms),
                    int(run.samples),
                    run.memory_kind,
                    int(run.peak_bytes),
                    int(run.peak_offset_ms),
                    int(run.typical_bytes),
                    int(run.p95_bytes),
                    int(run.max_processes),
                    i64::from(run.peak_pressure_score),
                    level_code(run.peak_pressure_level),
                    run.exit_code,
                    run.exit_signal,
                    i64::from(run.interrupted),
                    run.model_path,
                    run.model_estimate_bytes.map(int),
                    run.model_mapped_bytes.map(int),
                    env!("CARGO_PKG_VERSION"),
                ],
            )
            .map_err(|e| self.err(e))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every stored watched run, most recent first.
    pub fn watch_runs(&self) -> Result<Vec<WatchRun>, HistoryError> {
        if self.version < 2 {
            // Opened read-only (recording off) before the upgrade that adds watch runs.
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(&format!(
                "{WATCH_COLUMNS} ORDER BY started_ms DESC, id DESC"
            ))
            .map_err(|e| self.err(e))?;
        stmt.query_map([], watch_row)
            .map_err(|e| self.err(e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| self.err(e))
    }

    /// Number of stored snapshots.
    #[cfg(test)]
    pub fn snapshot_count(&self) -> Result<u64, HistoryError> {
        self.conn
            .query_row("SELECT count(*) FROM snapshots", [], |r| r.get::<_, i64>(0))
            .map(uint)
            .map_err(|e| self.err(e))
    }

    /// Bytes the database's pages hold (free pages excluded).
    pub fn used_bytes(&self) -> Result<u64, HistoryError> {
        let pragma = |name: &str| {
            self.conn
                .pragma_query_value(None, name, |r| r.get::<_, i64>(0))
                .map(uint)
                .map_err(|e| self.err(e))
        };
        let pages = pragma("page_count")?.saturating_sub(pragma("freelist_count")?);
        Ok(pages * pragma("page_size")?)
    }

    /// Deletes everything that ended before `cutoff_ms`. Open events are kept.
    fn delete_before(
        &mut self,
        cutoff_ms: u64,
        report: &mut PruneReport,
    ) -> Result<(), HistoryError> {
        let cutoff = int(cutoff_ms);
        let path = self.path.clone();
        let sql = |e: rusqlite::Error| HistoryError::sqlite(&path, &e);
        let tx = self.conn.transaction().map_err(sql)?;
        report.snapshots += tx
            .execute("DELETE FROM snapshots WHERE ts_ms < ?1", params![cutoff])
            .map_err(sql)?;
        report.app_samples += tx
            .execute("DELETE FROM app_samples WHERE ts_ms < ?1", params![cutoff])
            .map_err(sql)?;
        report.events += tx
            .execute(
                "DELETE FROM events WHERE end_ms IS NOT NULL AND end_ms < ?1",
                params![cutoff],
            )
            .map_err(sql)?;
        tx.execute(
            "DELETE FROM sessions WHERE last_seen_ms < ?1 \
             AND id NOT IN (SELECT session_id FROM events)",
            params![cutoff],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    /// The oldest and newest timestamps still stored, if any.
    fn stored_span(&self) -> Result<Option<(u64, u64)>, HistoryError> {
        // One indexed min/max per table: a min over a UNION would scan every row.
        let mut times = Vec::new();
        for query in [
            "SELECT min(ts_ms), max(ts_ms) FROM snapshots",
            "SELECT min(ts_ms), max(ts_ms) FROM app_samples",
            "SELECT min(start_ms), max(start_ms) FROM events WHERE end_ms IS NOT NULL",
        ] {
            let (min, max): (Option<i64>, Option<i64>) = self
                .conn
                .query_row(query, [], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| self.err(e))?;
            times.extend(min.into_iter().chain(max).map(uint));
        }
        Ok(times.iter().min().copied().zip(times.iter().max().copied()))
    }

    /// Applies the retention policy at `now_ms`: drops records older than the maximum age, then,
    /// while the database is over its size cap, the oldest tenth of what is left. Freed pages
    /// go back to the file system.
    pub fn prune(
        &mut self,
        now_ms: u64,
        retention: Retention,
    ) -> Result<PruneReport, HistoryError> {
        let mut report = PruneReport::default();
        self.delete_before(now_ms.saturating_sub(retention.max_age_ms), &mut report)?;
        self.conn
            .execute(
                "DELETE FROM app_profiles WHERE last_seen_ms < ?1",
                params![int(now_ms.saturating_sub(PROFILE_MAX_AGE_MS))],
            )
            .map_err(|e| self.err(e))?;
        self.conn
            .execute(
                "DELETE FROM watch_runs WHERE started_ms + duration_ms < ?1",
                params![int(now_ms.saturating_sub(PROFILE_MAX_AGE_MS))],
            )
            .map_err(|e| self.err(e))?;
        self.vacuum()?;
        while self.used_bytes()? > retention.max_bytes && report.size_rounds < 64 {
            let Some((oldest, newest)) = self.stored_span()? else {
                break;
            };
            let cutoff = oldest + ((newest - oldest) / 10).max(1);
            self.delete_before(cutoff, &mut report)?;
            self.vacuum()?;
            report.size_rounds += 1;
        }
        Ok(report)
    }

    fn vacuum(&self) -> Result<(), HistoryError> {
        // `incremental_vacuum` returns no rows; run it as a batch.
        self.conn
            .execute_batch("PRAGMA incremental_vacuum;")
            .map_err(|e| self.err(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::classify::tests::{desktop, snapshot_with};
    use crate::history::events::tests::reading;
    use crate::history::tests::temp_dir;
    use crate::history::{DAY_MS, HOUR_MS};

    fn store(tag: &str) -> Store {
        Store::open(&temp_dir(tag).join("history.sqlite3")).unwrap()
    }

    /// A desktop with apps above the sampling threshold.
    fn snapshot_at(ts_ms: u64) -> Snapshot {
        let mut s = snapshot_with(desktop());
        s.timestamp_unix_ms = ts_ms;
        s
    }

    fn episode(start_s: u64, level: PressureLevel) -> Episode {
        let r = reading(start_s, level, 80, "Chrome");
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

    #[test]
    fn data_survives_reopening() {
        let dir = temp_dir("store-reopen");
        let path = dir.join("nested").join("history.sqlite3");
        {
            let mut s = Store::open(&path).unwrap();
            let session = s.begin_session(1_000, 30_000).unwrap();
            s.insert_snapshot(session, &snapshot_at(2_000)).unwrap();
            let id = s
                .insert_event(session, &episode(3, PressureLevel::Critical))
                .unwrap();
            let mut e = episode(3, PressureLevel::Critical);
            e.last_seen_ms = 9_000;
            s.update_event(id, &e, Some((8_000, false))).unwrap();
        }
        let s = Store::open_existing(&path).unwrap().expect("exists");
        assert_eq!(s.snapshot_count().unwrap(), 1);
        let events = s.events_since(0).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!((e.start_unix_ms, e.end_unix_ms), (3_000, Some(8_000)));
        assert_eq!(e.duration_ms(), 5_000);
        assert_eq!(e.level, PressureLevel::Critical);
        assert_eq!(e.culprits[0].name, "Chrome");
        assert!(!e.cut_short && !e.ongoing());
        assert_eq!(s.recorded_ms(0, 10_000).unwrap(), 1_000);
    }

    #[test]
    fn open_existing_never_creates_a_file() {
        let path = temp_dir("store-missing").join("history.sqlite3");
        assert!(Store::open_existing(&path).unwrap().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn dangling_events_are_closed_as_cut_short() {
        let s = store("store-dangling");
        let session = s.begin_session(0, 30_000).unwrap();
        let mut e = episode(10, PressureLevel::Strained);
        e.last_seen_ms = 70_000;
        let id = s.insert_event(session, &e).unwrap();
        s.update_event(id, &e, None).unwrap();
        assert!(s.events_since(0).unwrap()[0].ongoing());
        assert_eq!(s.close_dangling_events().unwrap(), 1);
        let e = &s.events_since(0).unwrap()[0];
        assert_eq!(e.end_unix_ms, Some(70_000));
        assert!(e.cut_short);
    }

    #[test]
    fn app_profiles_keep_typical_and_peak() {
        let mut s = store("store-profiles");
        let session = s.begin_session(0, 30_000).unwrap();
        let base = snapshot_at(1_000);
        let name = base
            .app_groups
            .iter()
            .find(|g| g.memory_bytes >= APP_SAMPLE_MIN_BYTES)
            .expect("the fixture has a large app")
            .name
            .clone();
        for (i, gb) in [1u64, 3, 2].into_iter().enumerate() {
            let mut snap = snapshot_at(1_000 * (i as u64 + 1));
            let g = snap.app_groups.iter_mut().find(|g| g.name == name).unwrap();
            g.memory_bytes = gb << 30;
            s.insert_snapshot(session, &snap).unwrap();
        }
        let p = s.app_profile(&name).unwrap().unwrap();
        assert_eq!(p.samples, 3);
        assert_eq!(p.typical_bytes, 2 << 30);
        assert_eq!((p.peak_bytes, p.peak_ms), (3 << 30, 2_000));
        assert_eq!((p.first_seen_ms, p.last_seen_ms), (1_000, 3_000));
        assert!(!p.memory_kind.is_empty());
        assert!(s.app_profile("nobody").unwrap().is_none());
        let all = s.app_profiles().unwrap();
        assert!(all.len() > 1, "the fixture records several apps");
        assert!(all.contains(&p));
        let samples = s.app_samples(&name).unwrap();
        assert_eq!(
            samples,
            vec![(1_000, 1 << 30), (2_000, 3 << 30), (3_000, 2 << 30)]
        );
        assert!(s.app_samples("nobody").unwrap().is_empty());
    }

    #[test]
    fn retention_drops_old_records_with_a_fake_clock() {
        let mut s = store("store-age");
        let session = s.begin_session(0, 30_000).unwrap();
        let start = 1_700_000_000_000u64;
        for h in 0..(10 * 24) {
            s.insert_snapshot(session, &snapshot_at(start + h * HOUR_MS))
                .unwrap();
        }
        let id = s
            .insert_event(session, &episode(start / 1000, PressureLevel::Strained))
            .unwrap();
        s.update_event(
            id,
            &episode(start / 1000, PressureLevel::Strained),
            Some((start + 60_000, false)),
        )
        .unwrap();
        // A recent, still open event survives any age limit.
        let open = episode((start + 9 * DAY_MS) / 1000, PressureLevel::Critical);
        s.insert_event(session, &open).unwrap();

        let now = start + 10 * DAY_MS;
        let retention = Retention {
            max_age_ms: 7 * DAY_MS,
            max_bytes: u64::MAX,
        };
        let report = s.prune(now, retention).unwrap();
        assert_eq!(report.snapshots, 3 * 24);
        assert_eq!(report.events, 1);
        assert_eq!(report.size_rounds, 0);
        assert_eq!(s.snapshot_count().unwrap(), 7 * 24);
        let left = s.events_since(0).unwrap();
        assert_eq!(left.len(), 1);
        assert!(left[0].ongoing());
        // Pruning again at the same time removes nothing.
        assert_eq!(s.prune(now, retention).unwrap(), PruneReport::default());
    }

    #[test]
    fn size_cap_bounds_the_database_in_a_long_run() {
        let dir = temp_dir("store-size");
        let path = dir.join("history.sqlite3");
        let mut s = Store::open(&path).unwrap();
        // Durability is not under test here; skipping fsync keeps the long run fast.
        s.conn
            .execute_batch("PRAGMA synchronous = OFF; PRAGMA journal_mode = MEMORY;")
            .unwrap();
        let session = s.begin_session(0, 30_000).unwrap();
        let cap = 256 * 1024;
        let retention = Retention {
            max_age_ms: 365 * DAY_MS,
            max_bytes: cap,
        };
        let start = 1_700_000_000_000u64;
        let mut largest_file = 0;
        let mut pruned_by_size = 0;
        // A week at one snapshot every 30 s, pruned every ten minutes as the recorder does.
        let steps = 7 * 24 * 120;
        for step in 0..steps {
            let now = start + step * 30_000;
            s.insert_snapshot(session, &snapshot_at(now)).unwrap();
            if step % 120 == 0 {
                let id = s
                    .insert_event(session, &episode(now / 1000, PressureLevel::Strained))
                    .unwrap();
                s.update_event(
                    id,
                    &episode(now / 1000, PressureLevel::Strained),
                    Some((now + 1_000, false)),
                )
                .unwrap();
            }
            if step % 20 == 0 {
                pruned_by_size += s.prune(now, retention).unwrap().size_rounds;
                largest_file = largest_file.max(std::fs::metadata(&path).unwrap().len());
            }
        }
        assert!(pruned_by_size > 0, "the cap was reached");
        // Between two prunes the file grows by at most ten minutes of records.
        assert!(
            largest_file <= cap + 64 * 1024,
            "database grew to {largest_file} bytes with a {cap}-byte cap"
        );
        // Recent history is what survives.
        let kept = s.snapshot_count().unwrap();
        assert!(kept > 100 && kept < steps, "{kept}");
        let newest = s.events_since(0).unwrap();
        assert!(!newest.is_empty());
        assert!(newest[0].start_unix_ms > start);
    }

    #[test]
    fn recorded_time_merges_overlapping_sessions() {
        let s = store("store-recorded");
        let a = s.begin_session(0, 30_000).unwrap();
        s.touch_session(a, 100).unwrap();
        let b = s.begin_session(50, 30_000).unwrap();
        s.touch_session(b, 150).unwrap();
        let c = s.begin_session(300, 30_000).unwrap();
        s.touch_session(c, 400).unwrap();
        assert_eq!(s.recorded_ms(0, 1_000).unwrap(), 150 + 100);
        assert_eq!(s.recorded_ms(120, 350).unwrap(), 30 + 50);
    }

    #[test]
    fn read_only_open_never_writes_or_upgrades() {
        let dir = temp_dir("store-readonly");
        // A missing file is not created.
        let missing = dir.join("missing.sqlite3");
        assert!(Store::open_read_only(&missing).is_err());
        assert!(!missing.exists());

        // A version-1 database is read as it is: events readable, no watch runs, no upgrade.
        let path = dir.join("history.sqlite3");
        {
            let mut conn = Connection::open(&path).unwrap();
            schema::migrate_with(&mut conn, &path, &schema::MIGRATIONS[..1]).unwrap();
            conn.execute(
                "INSERT INTO events (session_id, start_ms, last_seen_ms, end_ms, peak_level, \
                 peak_score, peak_ms, reason_signal, reason_message, culprits) \
                 VALUES (1, 5000, 6000, 6000, 2, 80, 5000, 'low_available', 'low', '[]')",
                [],
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let store = Store::open_read_only(&path).unwrap();
        assert_eq!(store.events_since(0).unwrap().len(), 1);
        assert!(store.watch_runs().unwrap().is_empty());
        assert!(store.begin_session(7000, 30_000).is_err(), "read-only");
        drop(store);
        assert_eq!(std::fs::read(&path).unwrap(), before, "byte for byte");
    }
}
