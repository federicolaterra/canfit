//! End-to-end checks of `canfit history`: an empty history, a recorded one, `--since`, `--json`
//! and a database written by a newer canfit. Every test points `CANFIT_DATA_DIR` at a temporary
//! folder, never at the real data directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn canfit(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_canfit"))
        .args(args)
        .env("CANFIT_DATA_DIR", data_dir)
        .env_remove("CANFIT_NO_HISTORY")
        .env("NO_COLOR", "1")
        .output()
        .expect("run canfit")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("canfit-cli-history-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Writes a version-1 history database with one session and one event, as the monitor would.
/// The statements spell out the documented schema version 1, so a change to it shows up here.
fn seed(dir: &Path, start_ms: i64) -> PathBuf {
    let path = dir.join("history.sqlite3");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (id INTEGER PRIMARY KEY, started_ms INTEGER NOT NULL, \
           last_seen_ms INTEGER NOT NULL, interval_ms INTEGER NOT NULL, \
           canfit_version TEXT NOT NULL);
         CREATE TABLE events (id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL, \
           start_ms INTEGER NOT NULL, end_ms INTEGER, last_seen_ms INTEGER NOT NULL, \
           cut_short INTEGER NOT NULL DEFAULT 0, peak_level INTEGER NOT NULL, \
           peak_score INTEGER NOT NULL, peak_ms INTEGER NOT NULL, reason_signal TEXT NOT NULL, \
           reason_message TEXT NOT NULL, culprits TEXT NOT NULL);
         PRAGMA application_id = 1667656052;
         PRAGMA user_version = 1;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions VALUES (1, ?1, ?2, 30000, '0.0.0')",
        [start_ms - 60_000, start_ms + 600_000],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events (session_id, start_ms, end_ms, last_seen_ms, cut_short, peak_level, \
         peak_score, peak_ms, reason_signal, reason_message, culprits) \
         VALUES (1, ?1, ?2, ?2, 0, 2, 85, ?1, 'low_available', \
         'only 900 MB of 16.0 GB memory is available', \
         '[{\"name\":\"Google Chrome\",\"memory_bytes\":6442450944}]')",
        [start_ms, start_ms + 240_000],
    )
    .unwrap();
    path
}

#[test]
fn no_history_yet_is_an_answer_and_creates_nothing() {
    let dir = temp_dir("empty");
    let out = canfit(&dir, &["history"]);
    assert_eq!(code(&out), 0);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("No pressure events recorded in the last 24 hours"),
        "{text}"
    );
    assert!(text.contains("Open the monitor"));
    assert!(!dir.join("history.sqlite3").exists());

    let out = canfit(&dir, &["history", "--json"]);
    assert_eq!(code(&out), 0);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], "canfit.history");
    assert_eq!(v["schema_version"], 1);
    assert!(v["database"].is_null());
    assert_eq!(v["events"].as_array().unwrap().len(), 0);
}

#[test]
fn recorded_events_are_listed_and_filtered_by_since() {
    let dir = temp_dir("recorded");
    let start = now_ms() - 2 * 3_600_000;
    let path = seed(&dir, start);

    let out = canfit(&dir, &["history"]);
    assert_eq!(code(&out), 0);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("✗ 1 pressure event in the last 24 hours. It was critical for 4 min"),
        "{text}"
    );
    assert!(text.contains("Google Chrome 6.0 GB"), "{text}");
    assert!(text.contains(&path.display().to_string()), "{text}");

    let out = canfit(&dir, &["history", "--json", "--since", "7d"]);
    assert_eq!(code(&out), 0);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let e = &v["events"][0];
    assert_eq!(e["level"], "critical");
    assert_eq!(e["duration_ms"], 240_000);
    assert_eq!(e["culprits"][0]["name"], "Google Chrome");
    assert_eq!(v["recorded_ms"], 660_000);

    // An hour ago is after the event ended.
    let out = canfit(&dir, &["history", "--json", "--since", "1h"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["events"].as_array().unwrap().len(), 0);
}

#[test]
fn a_newer_database_exits_3_and_stays_untouched() {
    let dir = temp_dir("newer");
    let path = dir.join("history.sqlite3");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA application_id = 1667656052; CREATE TABLE future (x); \
             PRAGMA user_version = 42;",
        )
        .unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let out = canfit(&dir, &["history"]);
    assert_eq!(code(&out), 3);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("written by a newer canfit (schema 42"),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// With recording off, past history is read as it is: an older schema is not upgraded and the
/// file is not changed by a single byte; an empty data directory stays empty.
#[test]
fn no_history_reads_without_writing() {
    let dir = temp_dir("readonly");
    let data = dir.join("data");
    let run = |args: &[&str], env: Option<&str>| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_canfit"));
        c.args(args)
            .env("CANFIT_DATA_DIR", &data)
            .env("CANFIT_CONFIG", dir.join("no-config.toml"))
            .env_remove("CANFIT_NO_HISTORY")
            .env("NO_COLOR", "1");
        if let Some(v) = env {
            c.env("CANFIT_NO_HISTORY", v);
        }
        c.output().expect("run canfit")
    };
    for (args, env) in [
        (&["--no-history", "history"][..], None),
        (&["history", "--apps", "--no-history"][..], None),
        (&["history", "--json"][..], Some("1")),
        (&["--no-history", "--json"][..], None),
        (&["--no-history", "spotify"][..], None),
    ] {
        let out = run(args, env);
        assert!([0, 1, 2].contains(&code(&out)), "{args:?}: {out:?}");
        assert!(!data.exists(), "{args:?} created the data directory");
    }
    let out = run(&["history", "--json"], Some("1"));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["history"], "off");
    let out = run(&["history", "--json"], Some("0"));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["history"], "on", "CANFIT_NO_HISTORY=0 changes nothing");
    let out = run(&["history", "--apps", "--json", "--no-history"], None);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["history"], "off");

    // A version-1 database (older than this canfit's schema) is read, not upgraded.
    std::fs::create_dir_all(&data).unwrap();
    let start = now_ms() - 2 * 3_600_000;
    let path = seed(&data, start);
    let before = std::fs::read(&path).unwrap();
    let out = run(&["--no-history", "history"], None);
    assert_eq!(code(&out), 0, "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("✗ 1 pressure event"), "{text}");
    let out = run(&["--no-history", "spotify", "--json"], None);
    assert!([0, 1, 2].contains(&code(&out)), "{out:?}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "byte for byte");
    let entries: Vec<_> = std::fs::read_dir(&data).unwrap().collect();
    assert_eq!(entries.len(), 1, "no journal or side files left");

    // Without the flag, the same database is upgraded as before.
    let out = run(&["history"], None);
    assert_eq!(code(&out), 0, "{out:?}");
    assert_ne!(
        std::fs::read(&path).unwrap(),
        before,
        "upgraded to the latest schema"
    );
}

#[test]
fn bad_since_is_a_usage_error() {
    let dir = temp_dir("usage");
    assert_eq!(code(&canfit(&dir, &["history", "--since", "soon"])), 64);
    assert_eq!(code(&canfit(&dir, &["history", "--since", "0d"])), 64);
}

/// Adds version-1 app tables with `count` samples of `name`, every 30 s, to the seeded history.
fn seed_app(path: &Path, name: &str, start_ms: i64, count: i64, mb: i64) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_samples (ts_ms INTEGER NOT NULL, name TEXT NOT NULL, \
           memory_bytes INTEGER NOT NULL, process_count INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS app_profiles (name TEXT PRIMARY KEY, \
           memory_kind TEXT NOT NULL, samples INTEGER NOT NULL, sum_bytes INTEGER NOT NULL, \
           peak_bytes INTEGER NOT NULL, peak_ms INTEGER NOT NULL, \
           first_seen_ms INTEGER NOT NULL, last_seen_ms INTEGER NOT NULL) WITHOUT ROWID;",
    )
    .unwrap();
    let mib = 1024 * 1024;
    let mut sum = 0;
    for i in 0..count {
        // Rises by 1 MB per sample.
        let bytes = (mb + i) * mib;
        sum += bytes;
        conn.execute(
            "INSERT INTO app_samples VALUES (?1, ?2, ?3, 3)",
            rusqlite::params![start_ms + i * 30_000, name, bytes],
        )
        .unwrap();
    }
    let last = start_ms + (count - 1) * 30_000;
    conn.execute(
        "INSERT INTO app_profiles VALUES (?1, 'footprint', ?2, ?3, ?4, ?5, ?6, ?5)",
        rusqlite::params![name, count, sum, (mb + count - 1) * mib, last, start_ms],
    )
    .unwrap();
}

#[test]
fn learned_apps_are_listed_and_used_by_app_checks() {
    let dir = temp_dir("apps");
    let out = canfit(&dir, &["history", "--apps"]);
    assert_eq!(code(&out), 0);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("canfit hasn't learned any app"), "{text}");
    assert!(!dir.join("history.sqlite3").exists());

    let start = now_ms() - 3 * 3_600_000;
    let path = seed(&dir, start);
    // A short session: 10 samples (5 minutes) of an app the catalog doesn't know.
    seed_app(&path, "Qzx Tool", start, 10, 300);
    // A longer one of an app the catalog knows: history wins over the catalog.
    seed_app(&path, "Spotify", start, 40, 200);

    let out = canfit(&dir, &["history", "--apps"]);
    assert_eq!(code(&out), 0);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("canfit has learned how much memory 2 apps use; 1 with enough evidence"),
        "{text}"
    );
    assert!(text.contains("What canfit learned about apps"), "{text}");
    assert!(text.contains("seen for 5 min, last today"), "{text}");

    let out = canfit(&dir, &["history", "--apps", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], "canfit.learned_apps");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["need_percentile"], 95);
    let apps = v["apps"].as_array().unwrap();
    assert_eq!(apps[0]["name"], "Qzx Tool", "largest need first");
    assert_eq!(apps[0]["confidence"], "low");
    assert_eq!(apps[0]["provenance"], "estimated");
    assert_eq!(apps[1]["name"], "Spotify");
    assert_eq!(apps[1]["confidence"], "medium");
    assert_eq!(apps[1]["provenance"], "measured");
    assert_eq!(apps[1]["samples"], 40);
    assert_eq!(apps[1]["observed_ms"], 40 * 30_000);

    // `canfit APP` uses it, labelled, with its confidence.
    let check = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_canfit"))
            .args(args)
            .env("CANFIT_DATA_DIR", &dir)
            .env("CANFIT_CONFIG", dir.join("config.toml"))
            .env_remove("CANFIT_APPS")
            .env("NO_COLOR", "1")
            .output()
            .expect("run canfit")
    };
    let out = check(&["spotify", "--json"]);
    assert!([0, 1, 2].contains(&code(&out)));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let app = &v["apps"][0];
    assert_eq!(app["source"], "history");
    assert_eq!(app["confidence"], "medium");
    assert_eq!(app["history"]["samples"], 40);
    let out = check(&["Qzx Tool"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("estimated from little history"), "{text}");
    assert!(text.contains("confidence low"), "{text}");
    assert!(text.contains("seen for 5 min"), "{text}");
}
