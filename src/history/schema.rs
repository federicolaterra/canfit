//! The history database schema and its migrations (ADR-018).
//!
//! The schema version is `PRAGMA user_version`; `PRAGMA application_id` marks the file as
//! canfit's. [`MIGRATIONS`] is ordered: entry `i` upgrades version `i` to `i + 1`, inside one
//! transaction together with the new `user_version`. canfit never recreates, truncates or
//! overwrites a database: a newer schema or a foreign file is refused with a clear message.
//!
//! **Migrations are frozen.** Once a migration has been committed, it is never edited: a user's
//! database may already be at that version, and SQLite would never run the edited text on it (a
//! development build once hit "table watch_runs has no column named model_mapped_bytes" this
//! way). To change the schema, append a new migration, which bumps the version, and add its
//! SHA-256 to [`tests::PINNED`]. The pin test fails if any existing migration changes.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use super::HistoryError;

/// `PRAGMA application_id` of canfit history databases: "cfit" in ASCII.
pub const APPLICATION_ID: i32 = 0x6366_6974;

/// Version 1: recording sessions, compact snapshots, pressure events, app memory samples and
/// per-app profiles.
const V1: &str = "
CREATE TABLE sessions (
    id             INTEGER PRIMARY KEY,
    started_ms     INTEGER NOT NULL,
    last_seen_ms   INTEGER NOT NULL,
    interval_ms    INTEGER NOT NULL,
    canfit_version TEXT    NOT NULL
);
CREATE TABLE snapshots (
    ts_ms             INTEGER NOT NULL,
    session_id        INTEGER NOT NULL,
    memory_total      INTEGER NOT NULL,
    memory_available  INTEGER NOT NULL,
    swap_used         INTEGER NOT NULL,
    compressed        INTEGER,
    cpu_permille      INTEGER,
    pressure_score    INTEGER NOT NULL,
    pressure_level    INTEGER NOT NULL
);
CREATE INDEX snapshots_ts ON snapshots (ts_ms);
CREATE TABLE events (
    id              INTEGER PRIMARY KEY,
    session_id      INTEGER NOT NULL,
    start_ms        INTEGER NOT NULL,
    end_ms          INTEGER,
    last_seen_ms    INTEGER NOT NULL,
    cut_short       INTEGER NOT NULL DEFAULT 0,
    peak_level      INTEGER NOT NULL,
    peak_score      INTEGER NOT NULL,
    peak_ms         INTEGER NOT NULL,
    reason_signal   TEXT    NOT NULL,
    reason_message  TEXT    NOT NULL,
    culprits        TEXT    NOT NULL
);
CREATE INDEX events_start ON events (start_ms);
CREATE TABLE app_samples (
    ts_ms          INTEGER NOT NULL,
    name           TEXT    NOT NULL,
    memory_bytes   INTEGER NOT NULL,
    process_count  INTEGER NOT NULL
);
CREATE INDEX app_samples_ts ON app_samples (ts_ms);
CREATE INDEX app_samples_name ON app_samples (name, ts_ms);
CREATE TABLE app_profiles (
    name           TEXT    PRIMARY KEY,
    memory_kind    TEXT    NOT NULL,
    samples        INTEGER NOT NULL,
    sum_bytes      INTEGER NOT NULL,
    peak_bytes     INTEGER NOT NULL,
    peak_ms        INTEGER NOT NULL,
    first_seen_ms  INTEGER NOT NULL,
    last_seen_ms   INTEGER NOT NULL
) WITHOUT ROWID;
";

/// Version 2: runs measured by `canfit watch` (ADR-023). Kept 90 days, like app profiles; the
/// command line is not stored (it may hold secrets), only the program name.
const V2: &str = "
CREATE TABLE watch_runs (
    id                   INTEGER PRIMARY KEY,
    name                 TEXT    NOT NULL,
    program              TEXT    NOT NULL,
    started_ms           INTEGER NOT NULL,
    duration_ms          INTEGER NOT NULL,
    interval_ms          INTEGER NOT NULL,
    samples              INTEGER NOT NULL,
    memory_kind          TEXT    NOT NULL,
    peak_bytes           INTEGER NOT NULL,
    peak_offset_ms       INTEGER NOT NULL,
    typical_bytes        INTEGER NOT NULL,
    p95_bytes            INTEGER NOT NULL,
    max_processes        INTEGER NOT NULL,
    peak_pressure_score  INTEGER NOT NULL,
    peak_pressure_level  INTEGER NOT NULL,
    exit_code            INTEGER,
    exit_signal          INTEGER,
    interrupted          INTEGER NOT NULL DEFAULT 0,
    model_path           TEXT,
    model_estimate_bytes INTEGER,
    model_mapped_bytes   INTEGER,
    canfit_version       TEXT    NOT NULL
);
CREATE INDEX watch_runs_name ON watch_runs (name, started_ms);
";

/// Every migration, oldest first. The latest schema version is `MIGRATIONS.len()`.
pub const MIGRATIONS: &[&str] = &[V1, V2];

/// The schema version this canfit writes.
pub const LATEST_VERSION: u32 = MIGRATIONS.len() as u32;

#[cfg(test)]
pub fn latest_version() -> u32 {
    LATEST_VERSION
}

/// Brings `conn` up to date with `MIGRATIONS`; returns the version it ends at.
pub fn migrate(conn: &mut Connection, path: &Path) -> Result<u32, HistoryError> {
    migrate_with(conn, path, MIGRATIONS)?;
    Ok(LATEST_VERSION)
}

/// For a read-only open (recording off): checks that the file is a canfit database this canfit
/// can read as it is, without migrating or writing anything. Returns its schema version, which
/// may be older than [`LATEST_VERSION`]: the tables of a later version then read as empty.
pub fn check_readable(conn: &Connection, path: &Path) -> Result<u32, HistoryError> {
    let sql = |e: rusqlite::Error| HistoryError::sqlite(path, &e);
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(sql)?;
    let app_id: i32 = conn
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(sql)?;
    if app_id != APPLICATION_ID {
        return Err(HistoryError::NotCanfit {
            path: path.to_path_buf(),
        });
    }
    if version > LATEST_VERSION {
        return Err(HistoryError::NewerSchema {
            path: path.to_path_buf(),
            found: version,
            supported: LATEST_VERSION,
        });
    }
    Ok(version)
}

/// Brings `conn` to `migrations.len()`. Separate from [`migrate`] so tests can build every
/// earlier version and try upgrades that do not exist yet.
pub fn migrate_with(
    conn: &mut Connection,
    path: &Path,
    migrations: &[&str],
) -> Result<(), HistoryError> {
    let sql = |e: rusqlite::Error| HistoryError::sqlite(path, &e);
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(sql)?;
    let app_id: i32 = conn
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(sql)?;
    let has_tables = conn
        .query_row("SELECT 1 FROM sqlite_schema LIMIT 1", [], |_| Ok(()))
        .optional()
        .map_err(sql)?
        .is_some();

    // Version 0 with tables, or anyone else's application id: not ours, leave it alone.
    let foreign = if version == 0 {
        has_tables || (app_id != 0 && app_id != APPLICATION_ID)
    } else {
        app_id != APPLICATION_ID
    };
    if foreign {
        return Err(HistoryError::NotCanfit {
            path: path.to_path_buf(),
        });
    }
    let target = migrations.len() as u32;
    if version > target {
        return Err(HistoryError::NewerSchema {
            path: path.to_path_buf(),
            found: version,
            supported: target,
        });
    }
    if version == 0 {
        // Must precede the first table: lets the size cap give pages back to the file system.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .map_err(sql)?;
    }
    for (index, migration) in migrations.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction().map_err(sql)?;
        tx.execute_batch(migration).map_err(sql)?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)
            .map_err(sql)?;
        tx.pragma_update(None, "user_version", index as u32 + 1)
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::history::tests::temp_dir;

    /// SHA-256 of every released migration, in order. **Never change an entry**: changing a
    /// pinned migration is a breaking change for existing users, whose databases are already at
    /// that version and will never run the new text (see the module documentation). Append a
    /// new migration and its hash instead.
    pub(crate) const PINNED: &[&str] = &[
        // V1
        "97b38dbefe3045f0d678fc0d8cc73f5317210edf9d0036a413f7fc36081b9ec9",
        // V2
        "082564a314dd11c09339310331ae7db094a8a19952d0375fd9aa20cb86875877",
    ];

    /// SHA-256 (FIPS 180-4), small enough for a test: no dependency for one pin.
    fn sha256(text: &str) -> String {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut data = text.as_bytes().to_vec();
        let bits = (data.len() as u64) * 8;
        data.push(0x80);
        while data.len() % 64 != 56 {
            data.push(0);
        }
        data.extend_from_slice(&bits.to_be_bytes());
        for block in data.chunks(64) {
            let mut w = [0u32; 64];
            for (i, word) in block.chunks(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                *x = x.wrapping_add(y);
            }
        }
        h.iter().map(|x| format!("{x:08x}")).collect()
    }

    #[test]
    fn sha256_matches_known_digests() {
        assert_eq!(
            sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Two blocks.
        assert_eq!(
            sha256("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn released_migrations_are_pinned() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            let version = index + 1;
            let Some(pinned) = PINNED.get(index) else {
                panic!(
                    "migration V{version} is not pinned yet: add \"{}\" to PINNED",
                    sha256(migration)
                );
            };
            assert_eq!(
                sha256(migration),
                *pinned,
                "migration V{version} changed. Released migrations are frozen: revert the edit and \
                 append a new migration instead."
            );
        }
        assert_eq!(
            PINNED.len(),
            MIGRATIONS.len(),
            "PINNED lists a migration that does not exist"
        );
    }

    #[test]
    fn read_only_check_accepts_only_the_current_schema() {
        let dir = temp_dir("schema-readonly");
        let path = dir.join("h.sqlite3");
        let mut conn = Connection::open(&path).unwrap();
        migrate(&mut conn, &path).unwrap();
        assert_eq!(check_readable(&conn, &path).unwrap(), LATEST_VERSION);

        let old = dir.join("old.sqlite3");
        let mut conn = Connection::open(&old).unwrap();
        migrate_with(&mut conn, &old, &MIGRATIONS[..1]).unwrap();
        assert_eq!(check_readable(&conn, &old).unwrap(), 1, "older is readable");
        assert_eq!(version(&conn), 1, "left untouched");

        let newer = dir.join("newer.sqlite3");
        let mut conn = Connection::open(&newer).unwrap();
        let mut next = MIGRATIONS.to_vec();
        next.push("CREATE TABLE later (x INTEGER);");
        migrate_with(&mut conn, &newer, &next).unwrap();
        assert!(matches!(
            check_readable(&conn, &newer),
            Err(HistoryError::NewerSchema { .. })
        ));

        let foreign = dir.join("foreign.sqlite3");
        let conn = Connection::open(&foreign).unwrap();
        conn.execute_batch("CREATE TABLE notes (text TEXT);")
            .unwrap();
        assert!(matches!(
            check_readable(&conn, &foreign),
            Err(HistoryError::NotCanfit { .. })
        ));
    }

    fn version(conn: &Connection) -> u32 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn empty_database_gets_the_latest_schema() {
        let path = temp_dir("schema-empty").join("h.sqlite3");
        let mut conn = Connection::open(&path).unwrap();
        migrate(&mut conn, &path).unwrap();
        assert_eq!(version(&conn), latest_version());
        assert_eq!(
            tables(&conn),
            [
                "app_profiles",
                "app_samples",
                "events",
                "sessions",
                "snapshots",
                "watch_runs"
            ]
        );
        let auto_vacuum: i64 = conn
            .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
            .unwrap();
        assert_eq!(auto_vacuum, 2, "incremental");
        // Opening again is a no-op.
        migrate(&mut conn, &path).unwrap();
        assert_eq!(version(&conn), latest_version());
    }

    #[test]
    fn every_earlier_version_upgrades_and_keeps_its_data() {
        for from in 0..=MIGRATIONS.len() {
            let path = temp_dir("schema-upgrade").join("h.sqlite3");
            {
                let mut conn = Connection::open(&path).unwrap();
                migrate_with(&mut conn, &path, &MIGRATIONS[..from]).unwrap();
                assert_eq!(version(&conn), from as u32);
                if from >= 1 {
                    conn.execute(
                        "INSERT INTO sessions VALUES (7, 1000, 2000, 30000, 'old')",
                        [],
                    )
                    .unwrap();
                }
            }
            let mut conn = Connection::open(&path).unwrap();
            migrate(&mut conn, &path).unwrap();
            assert_eq!(version(&conn), latest_version(), "from version {from}");
            if from >= 1 {
                let kept: String = conn
                    .query_row(
                        "SELECT canfit_version FROM sessions WHERE id = 7",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(kept, "old", "from version {from}");
            }
        }
    }

    #[test]
    fn a_future_migration_upgrades_in_place() {
        // A stand-in for the next schema change: data written by version 1 survives it.
        const NEXT: &str = "ALTER TABLE events ADD COLUMN note TEXT;";
        let path = temp_dir("schema-next").join("h.sqlite3");
        let mut conn = Connection::open(&path).unwrap();
        migrate(&mut conn, &path).unwrap();
        conn.execute(
            "INSERT INTO events (session_id, start_ms, last_seen_ms, peak_level, peak_score, \
             peak_ms, reason_signal, reason_message, culprits) \
             VALUES (1, 5, 6, 2, 80, 5, 'low_available', 'low', '[]')",
            [],
        )
        .unwrap();
        let mut next: Vec<&str> = MIGRATIONS.to_vec();
        next.push(NEXT);
        migrate_with(&mut conn, &path, &next).unwrap();
        assert_eq!(version(&conn), latest_version() + 1);
        let (start, note): (i64, Option<String>) = conn
            .query_row("SELECT start_ms, note FROM events", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((start, note), (5, None));
        // That database is now too new for this canfit, which refuses it untouched.
        let err = migrate(&mut conn, &path).unwrap_err();
        assert!(
            matches!(err, HistoryError::NewerSchema { found, supported, .. }
                if found == latest_version() + 1 && supported == latest_version()),
            "{err}"
        );
        assert!(err.to_string().contains("newer canfit"));
        assert_eq!(version(&conn), latest_version() + 1);
    }

    #[test]
    fn foreign_databases_are_refused_untouched() {
        let dir = temp_dir("schema-foreign");
        // Someone else's SQLite file with a table and no version.
        let path = dir.join("other.sqlite3");
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE notes (text TEXT); INSERT INTO notes VALUES ('keep');")
            .unwrap();
        let err = migrate(&mut conn, &path).unwrap_err();
        assert!(matches!(err, HistoryError::NotCanfit { .. }), "{err}");
        let kept: String = conn
            .query_row("SELECT text FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, "keep");

        // A versioned database with another application id.
        let path = dir.join("versioned.sqlite3");
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA user_version = 3; PRAGMA application_id = 42;")
            .unwrap();
        assert!(matches!(
            migrate(&mut conn, &path),
            Err(HistoryError::NotCanfit { .. })
        ));

        // A file that is not SQLite at all.
        let path = dir.join("text.sqlite3");
        std::fs::write(
            &path,
            "not a database, just some text that is long enough....",
        )
        .unwrap();
        let mut conn = Connection::open(&path).unwrap();
        let err = migrate(&mut conn, &path).unwrap_err();
        assert!(matches!(err, HistoryError::NotCanfit { .. }), "{err}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("not a database")
        );
    }
}
