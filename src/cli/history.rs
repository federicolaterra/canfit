//! `canfit history`: the timeline of pressure events the monitor recorded, with when, how long,
//! how bad and which apps were using the most memory (`--since`, `--json`). The same lines fill
//! the monitor's history panel.

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::Serialize;

use super::EXIT_RUNTIME;
use super::card::{self, Row};
use crate::core::{PressureLevel, Provenance, format_bytes};
use crate::estimate::apps::Confidence;
use crate::estimate::apps::learned::{AppHistory, Learned, NEED_PERCENTILE};
use crate::history::store::{Store, StoredEvent};
use crate::history::{self, DAY_MS, HOUR_MS, HistoryError, MINUTE_MS};
use crate::ui::{self, theme::ThemeName};

/// Identifies the history JSON document.
pub const HISTORY_SCHEMA: &str = "canfit.history";
/// Bumped on any breaking change to the history JSON layout.
pub const HISTORY_SCHEMA_VERSION: u32 = 1;

/// The period `canfit history` covers without `--since`.
pub const DEFAULT_SINCE: &str = "24h";

/// The start of the period to show, and how to name it in a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Since {
    pub unix_ms: u64,
    /// "in the last 24 hours", "since Mon 9 Nov 10:00".
    pub label: String,
}

/// Parses `--since`: a duration back from now (`30m`, `12h`, `7d`, `2w`) or a local date or
/// time (`2026-11-09`, `2026-11-09 14:30`, `2026-11-09T14:30`).
pub fn parse_since<Tz: TimeZone>(text: &str, now: &DateTime<Tz>) -> Result<Since, String>
where
    Tz::Offset: std::fmt::Display,
{
    let text = text.trim();
    let now_ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
    let digits_end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits_end);
    if !number.is_empty() && !unit.contains('-') {
        let n: u64 = number
            .parse()
            .map_err(|_| format!("\"{text}\" is too large"))?;
        let (unit_ms, one, many) = match unit.trim() {
            "m" | "min" | "mins" | "minute" | "minutes" => (MINUTE_MS, "minute", "minutes"),
            "h" | "hour" | "hours" => (HOUR_MS, "hour", "hours"),
            "d" | "day" | "days" => (DAY_MS, "day", "days"),
            "w" | "week" | "weeks" => (7 * DAY_MS, "week", "weeks"),
            _ => return Err(since_help(text)),
        };
        if n == 0 {
            return Err("--since needs a period longer than zero".to_string());
        }
        let label = if n == 1 {
            format!("in the last {one}")
        } else {
            format!("in the last {n} {many}")
        };
        return Ok(Since {
            unix_ms: now_ms.saturating_sub(n.saturating_mul(unit_ms)),
            label,
        });
    }
    let naive = NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .or_else(|| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").ok())
        .or_else(|| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M").ok())
        .ok_or_else(|| since_help(text))?;
    let start = now
        .timezone()
        .from_local_datetime(&naive)
        .earliest()
        .ok_or_else(|| format!("{text} does not exist in the local time zone"))?;
    let unix_ms = u64::try_from(start.timestamp_millis()).unwrap_or(0);
    Ok(Since {
        unix_ms,
        label: format!("since {}", when(unix_ms, now)),
    })
}

fn since_help(text: &str) -> String {
    format!(
        "\"{text}\" is not a period: use a duration such as 30m, 12h, 7d or 2w, or a local date \
         such as 2026-11-09 or 2026-11-09 14:30"
    )
}

/// `--since` as clap parses it, against the local clock.
pub fn parse_since_now(text: &str) -> Result<Since, String> {
    parse_since(text, &Local::now())
}

/// A time as people say it: "today 14:02", "yesterday 09:15", "Mon 9 Nov 14:02".
pub fn when<Tz: TimeZone>(unix_ms: u64, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(t) = now
        .timezone()
        .timestamp_millis_opt(i64::try_from(unix_ms).unwrap_or(i64::MAX))
        .single()
    else {
        return "?".to_string();
    };
    let days = now
        .date_naive()
        .signed_duration_since(t.date_naive())
        .num_days();
    match days {
        0 => format!("today {}", t.format("%H:%M")),
        1 => format!("yesterday {}", t.format("%H:%M")),
        _ => t.format("%a %-d %b %H:%M").to_string(),
    }
}

/// A duration in words: "45 s", "4 min", "1 h 12 min", "2 d 3 h".
pub fn duration(ms: u64) -> String {
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

/// How long an event lasted, with "so far" or "+" when the end is unknown.
fn event_duration(e: &StoredEvent) -> String {
    let d = duration(e.duration_ms());
    if e.ongoing() {
        format!("{d} so far")
    } else if e.cut_short {
        format!("{d}+")
    } else {
        d
    }
}

fn culprit_text(e: &StoredEvent) -> String {
    if e.culprits.is_empty() {
        return "no app stood out".to_string();
    }
    e.culprits
        .iter()
        .map(|c| format!("{} {}", c.name, format_bytes(c.memory_bytes)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The worst event: highest level, then score, then the longest.
fn worst(events: &[StoredEvent]) -> Option<&StoredEvent> {
    events
        .iter()
        .max_by_key(|e| (e.level, e.peak_score, e.duration_ms()))
}

/// The sentence that opens the output.
pub fn summary_sentence<Tz: TimeZone>(
    events: &[StoredEvent],
    period: &str,
    recorded_ms: u64,
    now: &DateTime<Tz>,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(w) = worst(events) else {
        if recorded_ms == 0 {
            return format!(
                "No pressure events recorded {period}: canfit was not watching. History is \
                 recorded while the monitor (canfit) is open."
            );
        }
        return format!(
            "{} No pressure events {period}; canfit watched for {} of that time.",
            PressureLevel::Calm.symbol(),
            duration(recorded_ms)
        );
    };
    let count = match events.len() {
        1 => "1 pressure event".to_string(),
        n => format!("{n} pressure events"),
    };
    let mut sentence = format!("{} {count} {period}.", w.level.symbol());
    let whose = w
        .culprits
        .first()
        .map(|c| {
            format!(
                ", when {} was using {}",
                c.name,
                format_bytes(c.memory_bytes)
            )
        })
        .unwrap_or_default();
    let lead = if events.len() == 1 { "It" } else { "The worst" };
    if w.ongoing() {
        let _ = write!(
            sentence,
            " {lead} is still going on: {} for {} so far{whose}.",
            w.level.label(),
            duration(w.duration_ms())
        );
    } else {
        let _ = write!(
            sentence,
            " {lead} was {} for {}, {}{whose}.",
            w.level.label(),
            event_duration(w),
            when(w.start_unix_ms, now)
        );
    }
    sentence
}

/// One timeline row per event: when, how long, level and culprits. Used by the text output and
/// the monitor's panel.
pub fn rows<Tz: TimeZone>(events: &[StoredEvent], now: &DateTime<Tz>) -> Vec<Row>
where
    Tz::Offset: std::fmt::Display,
{
    events
        .iter()
        .map(|e| {
            Row::new(
                when(e.start_unix_ms, now),
                event_duration(e),
                format!(
                    "{} {:<8}  {}",
                    e.level.symbol(),
                    e.level.label(),
                    culprit_text(e)
                ),
            )
        })
        .collect()
}

/// The notes printed under the timeline.
fn notes(events: &[StoredEvent], recorded_ms: u64, path: Option<&Path>) -> Vec<String> {
    // Without events the opening sentence already says how long canfit watched.
    let mut notes = vec![if events.is_empty() {
        "History is recorded only while the monitor is open.".to_string()
    } else {
        format!(
            "canfit watched for {} of this period: history is recorded only while the monitor \
             is open.",
            duration(recorded_ms)
        )
    }];
    if !events.is_empty() {
        notes.push(
            "Levels come from canfit's pressure index (estimated); app sizes are the memory each \
             app used at the worst moment (measured)."
                .to_string(),
        );
    }
    if events.iter().any(|e| e.cut_short) {
        notes.push(
            "A + after a duration: the monitor closed before pressure calmed down, so the event \
             lasted at least that long."
                .to_string(),
        );
    }
    if let Some(path) = path {
        notes.push(format!("History file: {}", path.display()));
    }
    notes
}

/// `"on"` or `"off"` for `--json`: off with `--no-history`, `CANFIT_NO_HISTORY` or
/// `[history] enabled = false` (an unreadable config counts as the default, on).
fn recording() -> &'static str {
    let enabled = crate::advisor::config::load().map_or(true, |(c, _)| c.history.enabled);
    history::recording_label(enabled)
}

/// Reads the events of the period from `path`. No database yet is an empty history.
pub fn load(
    path: &Path,
    since_ms: u64,
    until_ms: u64,
) -> Result<(Vec<StoredEvent>, u64, bool), HistoryError> {
    match Store::open_existing(path)? {
        None => Ok((Vec::new(), 0, false)),
        Some(store) => Ok((
            store.events_since(since_ms)?,
            store.recorded_ms(since_ms, until_ms)?,
            true,
        )),
    }
}

/// Identifies the `canfit history --apps` JSON document.
pub const LEARNED_SCHEMA: &str = "canfit.learned_apps";
pub const LEARNED_SCHEMA_VERSION: u32 = 1;

/// One app canfit learned, as `canfit history --apps` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct LearnedApp {
    pub name: String,
    #[serde(flatten)]
    pub learned: Learned,
}

/// What canfit learned about every recorded app, largest need first.
pub fn learned_apps(history: &AppHistory, now_ms: u64) -> Vec<LearnedApp> {
    let mut apps: Vec<LearnedApp> = history
        .names()
        .filter_map(|name| {
            history
                .learn(&[name.to_string()], now_ms)
                .map(|learned| LearnedApp {
                    name: name.to_string(),
                    learned,
                })
        })
        .collect();
    apps.sort_by(|a, b| {
        b.learned
            .need_bytes
            .cmp(&a.learned.need_bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    apps
}

/// The sentence that opens `canfit history --apps`.
pub fn learned_sentence(apps: &[LearnedApp]) -> String {
    let confident = apps
        .iter()
        .filter(|a| a.learned.confidence != Confidence::Low)
        .count();
    match apps {
        [] => "canfit hasn't learned any app's memory yet: it learns while the monitor (canfit) is open, so keep it open while you use your apps, or measure one run with canfit watch -- CMD."
            .to_string(),
        [one] => format!(
            "canfit has learned how much memory 1 app uses: {} (confidence {}).",
            one.name,
            confidence_word(one.learned.confidence)
        ),
        _ => format!(
            "canfit has learned how much memory {} apps use; {confident} with enough evidence to count as measured.",
            apps.len()
        ),
    }
}

fn confidence_word(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
}

pub fn learned_rows(apps: &[LearnedApp], now_ms: u64) -> Vec<Option<Row>> {
    let mut rows = Vec::new();
    for a in apps {
        let l = &a.learned;
        rows.push(Some(Row::new(
            a.name.clone(),
            format_bytes(l.need_bytes),
            format!(
                "{} · typical {} · peak {} · confidence {}",
                match (l.watch.is_some(), l.provenance) {
                    (true, _) => "measured by watch",
                    (false, Provenance::Measured) => "measured",
                    (false, _) => "estimated",
                },
                format_bytes(l.typical_bytes),
                format_bytes(l.peak_bytes),
                confidence_word(l.confidence)
            ),
        )));
        rows.push(Some(Row::new("", "", l.evidence(now_ms))));
    }
    rows
}

#[derive(Serialize)]
struct LearnedDocument<'a> {
    schema: &'static str,
    schema_version: u32,
    canfit_version: &'static str,
    summary: String,
    /// `"on"` or `"off"`: whether this run of canfit could record history.
    history: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a Path>,
    /// The percentile used as each app's need.
    need_percentile: u32,
    apps: &'a [LearnedApp],
}

pub fn learned_to_json(apps: &[LearnedApp], path: Option<&Path>) -> String {
    serde_json::to_string_pretty(&LearnedDocument {
        schema: LEARNED_SCHEMA,
        schema_version: LEARNED_SCHEMA_VERSION,
        canfit_version: env!("CARGO_PKG_VERSION"),
        summary: learned_sentence(apps),
        history: recording(),
        path,
        need_percentile: NEED_PERCENTILE,
        apps,
    })
    .expect("learned apps serialize")
}

/// The footnote under `canfit history --apps`: how each source counts as measured, matching the
/// confidence rules in `estimate::apps::learned` and `docs/open-source/apps.md`.
fn footnote() -> String {
    use crate::estimate::apps::learned::{
        MEDIUM_MAX_AGE_MS, MEDIUM_MIN_OBSERVED_MS, MEDIUM_MIN_SAMPLES, WATCH_HIGH_MIN_MS,
        WATCH_HIGH_MIN_SAMPLES,
    };
    format!(
        "The first number is what canfit APP checks. A run measured by canfit watch counts with \
         its peak and is always measured: confidence high for a run of {} and {WATCH_HIGH_MIN_SAMPLES} \
         samples or more, medium for a shorter one. For an app the monitor sampled, it is the \
         {NEED_PERCENTILE}th percentile of the samples: measured with medium or high confidence \
         (at least {MEDIUM_MIN_SAMPLES} samples over {}, last seen within {}), otherwise an \
         estimate.",
        duration(WATCH_HIGH_MIN_MS),
        duration(MEDIUM_MIN_OBSERVED_MS),
        duration(MEDIUM_MAX_AGE_MS)
    )
}

pub fn learned_to_text(
    apps: &[LearnedApp],
    now_ms: u64,
    path: Option<&Path>,
    theme: Option<ui::theme::Theme>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", card::sentence(&learned_sentence(apps), theme));
    if !apps.is_empty() {
        out.push('\n');
        out.push_str(&card::render_styled(
            "What canfit learned about apps",
            &learned_rows(apps, now_ms),
            theme,
        ));
        for note in [
            footnote(),
            "The monitor samples only apps among the 8 largest and above 100 MB; canfit watch \
             measures a command's whole process tree."
                .to_string(),
        ] {
            out.push_str(&card::wrap(&note, 100, 2));
        }
    }
    if let Some(path) = path {
        out.push_str(&card::wrap(
            &format!("History file: {}", path.display()),
            100,
            2,
        ));
    }
    out
}

/// `canfit history --apps`: what canfit learned about apps (ADR-021).
pub fn run_apps(json: bool, theme: ThemeName) -> ExitCode {
    let now_ms = u64::try_from(Local::now().timestamp_millis()).unwrap_or(0);
    let Some(path) = history::database_path() else {
        eprintln!("canfit can't find a data directory for its history on this system.");
        return ExitCode::from(EXIT_RUNTIME);
    };
    let found = match AppHistory::load(&path) {
        Ok(h) => h,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let exists = found.is_some();
    let apps = found.map_or_else(Vec::new, |h| learned_apps(&h, now_ms));
    let shown = exists.then_some(path.as_path());
    if json {
        println!("{}", learned_to_json(&apps, shown));
    } else {
        print!(
            "{}",
            ui::glyphs::text(&learned_to_text(
                &apps,
                now_ms,
                shown,
                ui::stdout_theme(theme)
            ))
        );
    }
    ExitCode::SUCCESS
}

pub fn run(since: Option<&Since>, json: bool, theme: ThemeName) -> ExitCode {
    let now = Local::now();
    let since = match since {
        Some(s) => s.clone(),
        None => parse_since(DEFAULT_SINCE, &now).expect("the default period parses"),
    };
    let now_ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
    let Some(path) = history::database_path() else {
        eprintln!("canfit can't find a data directory for its history on this system.");
        return ExitCode::from(EXIT_RUNTIME);
    };
    let (events, recorded_ms, exists) = match load(&path, since.unix_ms, now_ms) {
        Ok(found) => found,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let summary = summary_sentence(&events, &since.label, recorded_ms, &now);
    if json {
        println!(
            "{}",
            to_json(
                &summary,
                &since,
                now_ms,
                exists.then_some(path.as_path()),
                recorded_ms,
                &events
            )
        );
        return ExitCode::SUCCESS;
    }
    print!(
        "{}",
        ui::glyphs::text(&render_text(
            &summary,
            &since,
            &events,
            recorded_ms,
            exists.then_some(path.as_path()),
            &now,
            ui::stdout_theme(theme)
        ))
    );
    ExitCode::SUCCESS
}

pub fn render_text<Tz: TimeZone>(
    summary: &str,
    since: &Since,
    events: &[StoredEvent],
    recorded_ms: u64,
    path: Option<&Path>,
    now: &DateTime<Tz>,
    theme: Option<ui::theme::Theme>,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let mut out = String::new();
    let _ = writeln!(out, "{}", card::sentence(summary, theme));
    if path.is_none() {
        let _ = writeln!(
            out,
            "\nOpen the monitor (run canfit with no arguments) to start recording."
        );
        return out;
    }
    if !events.is_empty() {
        out.push('\n');
        let rows: Vec<Option<Row>> = rows(events, now).into_iter().map(Some).collect();
        out.push_str(&card::render_styled(
            &format!("Pressure events {}", since.label),
            &rows,
            theme,
        ));
    }
    for note in notes(events, recorded_ms, path) {
        out.push_str(&card::wrap(&note, 100, 2));
    }
    out
}

#[derive(Serialize)]
struct CulpritJson<'a> {
    name: &'a str,
    memory_bytes: u64,
    provenance: Provenance,
}

#[derive(Serialize)]
struct ReasonJson<'a> {
    signal: &'a str,
    message: &'a str,
}

#[derive(Serialize)]
struct EventJson<'a> {
    start_unix_ms: u64,
    end_unix_ms: Option<u64>,
    duration_ms: u64,
    ongoing: bool,
    cut_short: bool,
    level: PressureLevel,
    peak_score: u8,
    peak_unix_ms: u64,
    level_provenance: Provenance,
    reason: ReasonJson<'a>,
    culprits: Vec<CulpritJson<'a>>,
}

#[derive(Serialize)]
struct HistoryDocument<'a> {
    schema: &'static str,
    schema_version: u32,
    canfit_version: &'static str,
    summary: &'a str,
    /// `"on"` or `"off"`: whether this run of canfit could record history.
    history: &'static str,
    since_unix_ms: u64,
    until_unix_ms: u64,
    database: Option<String>,
    recorded_ms: u64,
    events: Vec<EventJson<'a>>,
}

pub fn to_json(
    summary: &str,
    since: &Since,
    until_ms: u64,
    path: Option<&Path>,
    recorded_ms: u64,
    events: &[StoredEvent],
) -> String {
    let document = HistoryDocument {
        schema: HISTORY_SCHEMA,
        schema_version: HISTORY_SCHEMA_VERSION,
        canfit_version: env!("CARGO_PKG_VERSION"),
        summary,
        history: recording(),
        since_unix_ms: since.unix_ms,
        until_unix_ms: until_ms,
        database: path.map(|p| p.to_string_lossy().into_owned()),
        recorded_ms,
        events: events
            .iter()
            .map(|e| EventJson {
                start_unix_ms: e.start_unix_ms,
                end_unix_ms: e.end_unix_ms,
                duration_ms: e.duration_ms(),
                ongoing: e.ongoing(),
                cut_short: e.cut_short,
                level: e.level,
                peak_score: e.peak_score,
                peak_unix_ms: e.peak_unix_ms,
                level_provenance: Provenance::Estimated,
                reason: ReasonJson {
                    signal: &e.reason_signal,
                    message: &e.reason,
                },
                culprits: e
                    .culprits
                    .iter()
                    .map(|c| CulpritJson {
                        name: &c.name,
                        memory_bytes: c.memory_bytes,
                        provenance: Provenance::Measured,
                    })
                    .collect(),
            })
            .collect(),
    };
    serde_json::to_string_pretty(&document).expect("history serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::events::Culprit;
    use chrono::Utc;

    const GB: u64 = 1024 * 1024 * 1024;

    fn now() -> DateTime<Utc> {
        // Monday 9 November 2026, 15:00 UTC.
        Utc.with_ymd_and_hms(2026, 11, 9, 15, 0, 0).unwrap()
    }

    fn ms(t: DateTime<Utc>) -> u64 {
        t.timestamp_millis() as u64
    }

    fn event(start: DateTime<Utc>, minutes: u64, level: PressureLevel) -> StoredEvent {
        let start_ms = ms(start);
        StoredEvent {
            id: 1,
            start_unix_ms: start_ms,
            end_unix_ms: Some(start_ms + minutes * MINUTE_MS),
            last_seen_unix_ms: start_ms + minutes * MINUTE_MS,
            cut_short: false,
            level,
            peak_score: if level == PressureLevel::Critical {
                85
            } else {
                50
            },
            peak_unix_ms: start_ms,
            reason_signal: "low_available".into(),
            reason: "only 1.0 GB of 16.0 GB memory is available".into(),
            culprits: vec![
                Culprit {
                    name: "Google Chrome".into(),
                    memory_bytes: 6 * GB,
                },
                Culprit {
                    name: "Docker".into(),
                    memory_bytes: 3 * GB,
                },
            ],
        }
    }

    #[test]
    fn since_accepts_durations_and_local_dates() {
        let now = now();
        let s = parse_since("24h", &now).unwrap();
        assert_eq!(s.unix_ms, ms(now) - DAY_MS);
        assert_eq!(s.label, "in the last 24 hours");
        assert_eq!(parse_since("1d", &now).unwrap().label, "in the last day");
        assert_eq!(
            parse_since("2w", &now).unwrap().unix_ms,
            ms(now) - 14 * DAY_MS
        );
        assert_eq!(
            parse_since("30m", &now).unwrap().label,
            "in the last 30 minutes"
        );
        let s = parse_since("2026-11-08", &now).unwrap();
        assert_eq!(
            s.unix_ms,
            ms(Utc.with_ymd_and_hms(2026, 11, 8, 0, 0, 0).unwrap())
        );
        assert_eq!(s.label, "since yesterday 00:00");
        let s = parse_since("2026-11-01 14:30", &now).unwrap();
        assert_eq!(s.label, "since Sun 1 Nov 14:30");
        assert_eq!(
            parse_since("2026-11-01T14:30", &now).unwrap().unix_ms,
            s.unix_ms
        );
        for bad in [
            "",
            "soon",
            "0h",
            "5y",
            "2026-13-01",
            "99999999999999999999d",
        ] {
            assert!(parse_since(bad, &now).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(45_000), "45 s");
        assert_eq!(duration(4 * MINUTE_MS + 59_000), "4 min");
        assert_eq!(duration(HOUR_MS), "1 h");
        assert_eq!(duration(HOUR_MS + 12 * MINUTE_MS), "1 h 12 min");
        assert_eq!(duration(2 * DAY_MS + 3 * HOUR_MS), "2 d 3 h");
    }

    #[test]
    fn the_answer_names_the_worst_event_and_its_culprit() {
        let now = now();
        let events = vec![
            event(
                Utc.with_ymd_and_hms(2026, 11, 8, 22, 10, 0).unwrap(),
                12,
                PressureLevel::Strained,
            ),
            event(
                Utc.with_ymd_and_hms(2026, 11, 9, 14, 2, 0).unwrap(),
                4,
                PressureLevel::Critical,
            ),
        ];
        let s = summary_sentence(&events, "in the last 24 hours", HOUR_MS, &now);
        assert_eq!(
            s,
            "✗ 2 pressure events in the last 24 hours. The worst was critical for 4 min, today \
             14:02, when Google Chrome was using 6.0 GB."
        );
        let text = render_text(
            &s,
            &parse_since("24h", &now).unwrap(),
            &events,
            3 * HOUR_MS,
            Some(Path::new("/data/history.sqlite3")),
            &now,
            None,
        );
        assert!(text.starts_with("✗ 2 pressure events"), "{text}");
        assert!(
            text.contains("╭─ Pressure events in the last 24 hours "),
            "{text}"
        );
        assert!(
            text.contains(
                "yesterday 22:10  12 min  ▲ strained  Google Chrome 6.0 GB, Docker 3.0 GB"
            ),
            "{text}"
        );
        assert!(
            text.contains("today 14:02       4 min  ✗ critical"),
            "{text}"
        );
        assert!(
            text.contains("canfit watched for 3 h of this period"),
            "{text}"
        );
        assert!(text.contains("(estimated)") && text.contains("(measured)"));
        assert!(text.contains("History file: /data/history.sqlite3"));
    }

    #[test]
    fn ongoing_and_cut_short_events_are_honest_about_their_end() {
        let now = now();
        let mut ongoing = event(
            Utc.with_ymd_and_hms(2026, 11, 9, 14, 50, 0).unwrap(),
            10,
            PressureLevel::Strained,
        );
        ongoing.end_unix_ms = None;
        let s = summary_sentence(
            std::slice::from_ref(&ongoing),
            "in the last 24 hours",
            HOUR_MS,
            &now,
        );
        assert!(
            s.contains("It is still going on: strained for 10 min so far"),
            "{s}"
        );
        let mut cut = event(
            Utc.with_ymd_and_hms(2026, 11, 9, 9, 0, 0).unwrap(),
            3,
            PressureLevel::Strained,
        );
        cut.cut_short = true;
        let rows = rows(&[ongoing, cut.clone()], &now);
        assert_eq!(rows[0].size, "10 min so far");
        assert_eq!(rows[1].size, "3 min+");
        assert!(
            notes(&[cut], HOUR_MS, None)
                .iter()
                .any(|n| n.starts_with("A + after"))
        );
    }

    #[test]
    fn quiet_and_unwatched_periods_say_so() {
        let now = now();
        let s = summary_sentence(&[], "in the last 24 hours", 2 * HOUR_MS, &now);
        assert_eq!(
            s,
            "● No pressure events in the last 24 hours; canfit watched for 2 h of that time."
        );
        let s = summary_sentence(&[], "in the last 24 hours", 0, &now);
        assert!(s.starts_with(
            "No pressure events recorded in the last 24 hours: canfit was not watching"
        ));
        let text = render_text(
            &s,
            &parse_since("24h", &now).unwrap(),
            &[],
            0,
            None,
            &now,
            None,
        );
        assert!(text.contains("Open the monitor"));
    }

    #[test]
    fn json_document_labels_values() {
        let now = now();
        let mut e = event(
            Utc.with_ymd_and_hms(2026, 11, 9, 14, 2, 0).unwrap(),
            4,
            PressureLevel::Critical,
        );
        e.cut_short = true;
        let since = parse_since("24h", &now).unwrap();
        let json = to_json(
            "sentence",
            &since,
            ms(now),
            Some(Path::new("/x.sqlite3")),
            60_000,
            &[e],
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema"], HISTORY_SCHEMA);
        assert_eq!(v["schema_version"], HISTORY_SCHEMA_VERSION);
        assert_eq!(v["since_unix_ms"], ms(now) - DAY_MS);
        assert_eq!(v["database"], "/x.sqlite3");
        assert_eq!(v["recorded_ms"], 60_000);
        let e = &v["events"][0];
        assert_eq!(e["level"], "critical");
        assert_eq!(e["level_provenance"], "estimated");
        assert_eq!(e["duration_ms"], 4 * MINUTE_MS);
        assert_eq!(e["ongoing"], false);
        assert_eq!(e["cut_short"], true);
        assert_eq!(e["reason"]["signal"], "low_available");
        assert_eq!(e["culprits"][0]["name"], "Google Chrome");
        assert_eq!(e["culprits"][0]["provenance"], "measured");
    }

    #[test]
    fn a_missing_database_is_an_empty_history() {
        let path = crate::history::tests::temp_dir("cli-history").join("none.sqlite3");
        let (events, recorded, exists) = load(&path, 0, 1).unwrap();
        assert!(events.is_empty() && recorded == 0 && !exists);
        assert!(!path.exists());
    }

    #[test]
    fn apps_footnote_describes_watch_runs_and_monitor_samples() {
        let note = footnote();
        assert!(
            note.contains("canfit watch counts with its peak and is always measured"),
            "{note}"
        );
        assert!(
            note.contains("high for a run of 30 s and 10 samples"),
            "{note}"
        );
        assert!(note.contains("medium for a shorter one"), "{note}");
        assert!(note.contains("app the monitor sampled"), "{note}");
        assert!(
            note.contains("at least 20 samples over 10 min, last seen within 30 d"),
            "{note}"
        );
    }
}
