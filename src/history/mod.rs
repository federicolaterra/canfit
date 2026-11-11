//! Local history (spec §6.6, ADR-018): while the live monitor runs, canfit stores compact
//! snapshots, pressure events with their culprits, and per-app memory samples in a SQLite
//! database in the user's local data directory. Nothing leaves the machine (ADR-006).
//!
//! - [`schema`]: the versioned schema and its ordered migrations (`PRAGMA user_version`).
//! - [`store`]: reads, writes and the retention policy (age and size cap).
//! - [`events`]: pure pressure-event detection over a sequence of readings.
//! - [`recorder`]: the glue the monitor calls on every reading.

pub mod events;
pub mod recorder;
pub mod schema;
pub mod store;

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Environment variable that points canfit at another data directory (tests, portable setups).
pub const DATA_DIR_ENV: &str = "CANFIT_DATA_DIR";

/// File name of the history database inside the data directory.
pub const DATABASE_FILE: &str = "history.sqlite3";

/// Environment variable that turns recording off for one run, like `--no-history` (for shell
/// aliases and scripts). It can only turn recording off, never on.
pub const NO_HISTORY_ENV: &str = "CANFIT_NO_HISTORY";

/// What turned history recording off for this run of canfit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingOff {
    /// The global `--no-history` flag.
    Flag,
    /// `CANFIT_NO_HISTORY` set to a true value.
    Env,
    /// `canfit watch --no-save`.
    NoSave,
}

impl RecordingOff {
    /// How the user turned it off, as they typed it.
    pub fn switch(self) -> &'static str {
        match self {
            RecordingOff::Flag => "--no-history",
            RecordingOff::Env => "CANFIT_NO_HISTORY=1",
            RecordingOff::NoSave => "--no-save",
        }
    }
}

static RECORDING_OFF: std::sync::OnceLock<RecordingOff> = std::sync::OnceLock::new();

/// Turns history recording off for the rest of this process: [`store::Store::open`] refuses to
/// create or write a database, and existing databases are opened read-only without migrating.
/// The first switch wins; there is no way to turn recording back on.
pub fn turn_recording_off(reason: RecordingOff) {
    let _ = RECORDING_OFF.set(reason);
}

/// Why this run of canfit must not write history, if it must not.
pub fn recording_off() -> Option<RecordingOff> {
    RECORDING_OFF.get().copied()
}

/// Whether a `CANFIT_NO_HISTORY` value turns recording off: anything but empty, `0`, `false`,
/// `no` or `off` (any case) does, so a typo errs on the private side.
pub fn env_turns_recording_off(value: &std::ffi::OsStr) -> bool {
    let value = value.to_string_lossy();
    let value = value.trim().to_ascii_lowercase();
    !matches!(value.as_str(), "" | "0" | "false" | "no" | "off")
}

/// `"on"` or `"off"`: whether this run may record history, as `--json` documents report it.
/// Off when `--no-history`, `CANFIT_NO_HISTORY`, `watch --no-save` or `[history] enabled = false`
/// turned it off.
pub fn recording_label(config_enabled: bool) -> &'static str {
    if config_enabled && recording_off().is_none() {
        "on"
    } else {
        "off"
    }
}

/// Where the troubleshooting advice for an unusable history database lives.
pub const TROUBLESHOOTING_URL: &str = "https://github.com/federicolaterra/canfit/blob/master/docs/open-source/usage.md#troubleshooting-the-history-database";

/// The directory canfit keeps its local data in: `$CANFIT_DATA_DIR`, or the per-OS local data
/// directory — `~/Library/Application Support/canfit` on macOS, `%LOCALAPPDATA%\canfit\data` on
/// Windows, `$XDG_DATA_HOME/canfit` (`~/.local/share/canfit`) on Linux.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "canfit").map(|d| d.data_local_dir().to_path_buf())
}

/// Where the history database lives.
pub fn database_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join(DATABASE_FILE))
}

/// The `[history]` table of `config.toml`.
///
/// ```toml
/// [history]
/// enabled = true
/// snapshot_interval_secs = 30
/// retention_days = 7
/// max_size_mb = 64
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HistorySettings {
    /// Whether the monitor records history at all.
    pub enabled: bool,
    /// Seconds between two stored snapshots (pressure events are detected on every reading).
    pub snapshot_interval_secs: u64,
    /// Days of snapshots, events and app samples kept.
    pub retention_days: u64,
    /// Size cap of the database, in MiB; the oldest records go first when it is exceeded.
    pub max_size_mb: u64,
}

pub const DEFAULT_SNAPSHOT_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_RETENTION_DAYS: u64 = 7;
pub const DEFAULT_MAX_SIZE_MB: u64 = 64;

impl Default for HistorySettings {
    fn default() -> Self {
        HistorySettings {
            enabled: true,
            snapshot_interval_secs: DEFAULT_SNAPSHOT_INTERVAL_SECS,
            retention_days: DEFAULT_RETENTION_DAYS,
            max_size_mb: DEFAULT_MAX_SIZE_MB,
        }
    }
}

impl HistorySettings {
    /// Checks the ranges; the message names the setting and its allowed range.
    pub fn validate(&self) -> Result<(), String> {
        let check = |name: &str, value: u64, min: u64, max: u64| {
            if (min..=max).contains(&value) {
                Ok(())
            } else {
                Err(format!(
                    "history.{name} is {value}; it must be between {min} and {max}"
                ))
            }
        };
        check(
            "snapshot_interval_secs",
            self.snapshot_interval_secs,
            5,
            3600,
        )?;
        check("retention_days", self.retention_days, 1, 365)?;
        check("max_size_mb", self.max_size_mb, 1, 4096)
    }

    pub fn retention(&self) -> store::Retention {
        store::Retention {
            max_age_ms: self.retention_days * DAY_MS,
            max_bytes: self.max_size_mb * 1024 * 1024,
        }
    }
}

pub const MINUTE_MS: u64 = 60 * 1000;
pub const HOUR_MS: u64 = 60 * MINUTE_MS;
pub const DAY_MS: u64 = 24 * HOUR_MS;

/// Why the history database could not be used. Messages are plain language.
#[derive(Debug)]
pub enum HistoryError {
    /// The database directory or file could not be created or opened.
    Io { path: PathBuf, detail: String },
    /// SQLite reported an error.
    Sqlite { path: PathBuf, detail: String },
    /// The file was written by a newer canfit with a schema this version does not know.
    NewerSchema {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    /// The file is not a canfit history database.
    NotCanfit { path: PathBuf },
    /// Recording is off for this run (`--no-history`, `CANFIT_NO_HISTORY`, `watch --no-save`):
    /// nothing may be created or written.
    RecordingOff { reason: RecordingOff },
}

impl HistoryError {
    /// Adds the schema versions to a SQLite error, so a database whose tables don't match its
    /// version (a development build, a hand-edited file) is reported in actionable terms.
    pub(crate) fn with_schema(self, found: u32, expected: u32) -> HistoryError {
        match self {
            HistoryError::Sqlite { path, detail } => HistoryError::Sqlite {
                path,
                detail: format!(
                    "{detail} (the file says schema version {found}; this canfit expects \
                     version {expected})"
                ),
            },
            other => other,
        }
    }

    pub(crate) fn sqlite(path: &Path, err: &rusqlite::Error) -> HistoryError {
        if let rusqlite::Error::SqliteFailure(e, _) = err
            && e.code == rusqlite::ErrorCode::NotADatabase
        {
            return HistoryError::NotCanfit {
                path: path.to_path_buf(),
            };
        }
        HistoryError::Sqlite {
            path: path.to_path_buf(),
            detail: err.to_string(),
        }
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistoryError::Io { path, detail } => write!(
                f,
                "canfit can't open its history database {}: {detail}",
                path.display()
            ),
            HistoryError::Sqlite { path, detail } => write!(
                f,
                "canfit can't use its history database {}: {detail}. canfit left the file \
                 untouched; to start a new history, move it aside (see \"Troubleshooting the \
                 history database\" in {TROUBLESHOOTING_URL}).",
                path.display()
            ),
            HistoryError::NewerSchema {
                path,
                found,
                supported,
            } => write!(
                f,
                "The history database {} was written by a newer canfit (schema {found}; this \
                 canfit reads up to {supported}). Update canfit, or move the file away to start \
                 a new history. canfit left it untouched.",
                path.display()
            ),
            HistoryError::NotCanfit { path } => write!(
                f,
                "{} is not a canfit history database. Move it away to start a new history; \
                 canfit left it untouched.",
                path.display()
            ),
            HistoryError::RecordingOff { reason } => write!(
                f,
                "history recording is off for this run ({})",
                reason.switch()
            ),
        }
    }
}

impl std::error::Error for HistoryError {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh, empty temporary directory; tests never touch the real data directory.
    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "canfit-history-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn settings_have_defaults_and_ranges() {
        let s: HistorySettings = toml::from_str("").unwrap();
        assert_eq!(s, HistorySettings::default());
        assert!(s.validate().is_ok());
        let s: HistorySettings = toml::from_str("retention_days = 30").unwrap();
        assert_eq!(s.retention_days, 30);
        assert_eq!(s.snapshot_interval_secs, DEFAULT_SNAPSHOT_INTERVAL_SECS);
        assert_eq!(s.retention().max_age_ms, 30 * DAY_MS);
        let bad = HistorySettings {
            snapshot_interval_secs: 1,
            ..HistorySettings::default()
        };
        assert!(
            bad.validate()
                .unwrap_err()
                .contains("snapshot_interval_secs")
        );
        assert!(toml::from_str::<HistorySettings>("retain = 3").is_err());
    }

    #[test]
    fn no_history_env_values() {
        use std::ffi::OsStr;
        for on in ["1", "true", "YES", "on", "anything"] {
            assert!(env_turns_recording_off(OsStr::new(on)), "{on}");
        }
        for off in ["", "0", "false", "No", " off "] {
            assert!(!env_turns_recording_off(OsStr::new(off)), "{off:?}");
        }
    }

    #[test]
    fn schema_versions_make_sqlite_errors_actionable() {
        let err = HistoryError::Sqlite {
            path: PathBuf::from("/tmp/h.sqlite3"),
            detail: "table watch_runs has no column named model_mapped_bytes".to_string(),
        }
        .with_schema(2, 3)
        .to_string();
        assert!(err.contains("schema version 2"), "{err}");
        assert!(err.contains("expects version 3"), "{err}");
        assert!(err.contains("move it aside"), "{err}");
        assert!(
            err.contains("usage.md#troubleshooting-the-history-database"),
            "{err}"
        );
    }

    #[test]
    fn data_dir_names_canfit() {
        // Only checks the shape: the environment override is exercised by the CLI tests.
        if std::env::var_os(DATA_DIR_ENV).is_none()
            && let Some(path) = database_path()
        {
            assert!(path.ends_with(DATABASE_FILE));
            assert!(path.to_string_lossy().contains("canfit"));
        }
    }
}
