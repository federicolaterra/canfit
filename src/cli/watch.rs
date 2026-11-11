//! `canfit watch -- CMD` on the command line (ADR-023): runs the command with its own stdin,
//! stdout and stderr, samples its whole process tree, writes canfit's own lines to stderr only
//! (whole lines, never a redrawn status line, so the child's output is never garbled), prints a
//! summary card when the command ends, remembers the run in the history database and exits with
//! the command's exit code.

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::card::{self, Row};
use super::{EXIT_RUNTIME, app};
use crate::advisor::config;
use crate::core::sampler::Sampler;
use crate::core::{MemoryKind, PressureLevel, Snapshot, format_bytes};
use crate::estimate::apps::learned::span;
use crate::estimate::model::{self, Estimate, EstimateOptions};
use crate::estimate::target;
use crate::history::store::{Store, WatchRun};
use crate::platform::{self, SystemSource};
use crate::ui::{self, glyphs, theme::ThemeName};
use crate::watch::launch::{self, Launched};
use crate::watch::{self, Alert, Alerts, Ending, Point, Stats, Tree};

/// `canfit.watch` JSON document (`--json-out`), schema version 1.
pub const WATCH_SCHEMA: &str = "canfit.watch";
pub const WATCH_SCHEMA_VERSION: u32 = 1;

/// How often a "still watching" line is printed (unless `--quiet`).
const STATUS_EVERY: Duration = Duration::from_secs(60);
/// How often canfit checks whether the command has ended between two samples.
const POLL: Duration = Duration::from_millis(25);
/// Other apps named in a pressure warning.
const CULPRITS: usize = 2;

/// What `canfit watch` was asked to do.
#[derive(Debug, Clone)]
pub struct Options {
    pub command: Vec<String>,
    pub name: Option<String>,
    pub interval: Duration,
    pub quiet: bool,
    pub json_out: Option<PathBuf>,
    pub theme: ThemeName,
}

/// The model found in the command's arguments, with canfit's estimate.
#[derive(Debug, Clone, Serialize)]
pub struct ModelComparison {
    pub path: PathBuf,
    /// The context the estimate uses, and whether the command set it.
    pub context_tokens: u64,
    pub context_from_command: bool,
    pub estimate: Estimate,
    /// Whether the command maps the weights from the file (llama.cpp's default).
    pub mmap: bool,
    /// The weights the measured memory leaves out: mapped from the file, on an OS whose measure
    /// does not count mapped file pages (macOS footprint, Windows private working set). Added to
    /// the remembered need (ADR-023).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapped_bytes: Option<u64>,
    /// (Measured peak + mapped weights) − estimate without the margin, as a percentage of the
    /// estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difference_percent: Option<f64>,
}

/// Where the run was saved, or why not.
#[derive(Debug, Clone, Serialize)]
pub struct Remembered {
    pub saved: bool,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `"on"` or `"off"`: whether this run could record history at all (off with
    /// `--no-save`, `--no-history`, `CANFIT_NO_HISTORY` or `[history] enabled = false`).
    #[serde(skip)]
    pub history: &'static str,
    /// The run-only switch that turned recording off, if one did: the summary ends with
    /// "Not saved (SWITCH)."
    #[serde(skip)]
    pub switched_off: Option<crate::history::RecordingOff>,
}

#[derive(Debug, Clone, Serialize)]
struct MemoryDoc {
    kind: Option<String>,
    provenance: &'static str,
    peak_bytes: u64,
    peak_offset_ms: u64,
    typical_bytes: u64,
    p95_bytes: u64,
    last_bytes: u64,
    max_processes: usize,
    max_unreadable_processes: usize,
}

#[derive(Debug, Clone, Serialize)]
struct PressureDoc {
    peak_level: PressureLevel,
    peak_score: u8,
    peak_offset_ms: u64,
    peak_reason: String,
    warnings: u32,
}

#[derive(Debug, Clone, Serialize)]
struct EndingDoc {
    code: Option<i32>,
    signal: Option<i32>,
    interrupted: bool,
    /// What canfit itself exits with.
    canfit_exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
struct WatchDocument<'a> {
    schema: &'static str,
    schema_version: u32,
    canfit_version: &'static str,
    summary: String,
    /// `"off"` when history recording was disabled for this run.
    history: &'static str,
    name: &'a str,
    program: &'a str,
    pid: u32,
    started_unix_ms: u64,
    duration_ms: u64,
    interval_ms: u64,
    samples: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<MemoryDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pressure: Option<PressureDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a ModelComparison>,
    ended: EndingDoc,
    /// Processes the command started that were still running when it ended.
    still_running: usize,
    remembered: &'a Remembered,
    notes: &'a [String],
}

/// Everything known when the command has ended.
struct Outcome {
    name: String,
    program: String,
    pid: u32,
    started_unix_ms: u64,
    duration_ms: u64,
    interval_ms: u64,
    stats: Stats,
    ending: Ending,
    interrupted: bool,
    warnings: u32,
    still_running: usize,
    model: Option<ModelComparison>,
    /// Sampling problems and other caveats, one sentence each.
    notes: Vec<String>,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// One whole line on stderr, written at once.
fn say(line: &str) {
    let text = format!("[canfit] {}\n", glyphs::text(line));
    let mut err = io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
}

/// canfit's exit code for an ending: the command's own; a Windows code beyond 0–255 is passed to
/// the OS whole.
fn exit_with(code: i32) -> ExitCode {
    match u8::try_from(code) {
        Ok(c) => ExitCode::from(c),
        Err(_) => {
            let _ = io::stderr().flush();
            let _ = io::stdout().flush();
            std::process::exit(code)
        }
    }
}

/// Finds the model in the command's arguments and estimates it, before the command starts.
fn model_comparison(command: &[String]) -> Option<ModelComparison> {
    let found = watch::args::find_model(command.get(1..).unwrap_or_default(), Path::is_file)?;
    let parts = target::model_parts(&found.path).ok()?;
    let facts = model::load(&parts).ok()?;
    let context = match found.context {
        Some(0) => facts.context_length,
        other => other,
    };
    let options = EstimateOptions {
        context,
        cache_type: found.cache_type.unwrap_or_default(),
        ..EstimateOptions::default()
    };
    let estimate = model::estimate(&facts, options);
    Some(ModelComparison {
        path: found.path,
        context_tokens: estimate.context_tokens,
        context_from_command: found.context.is_some(),
        estimate,
        mmap: found.mmap,
        mapped_bytes: None,
        difference_percent: None,
    })
}

/// The warning printed when pressure rises: the level, the main reason, the watched tree and the
/// largest other apps. canfit closes nothing.
fn warning(
    level: PressureLevel,
    already: bool,
    snapshot: &Snapshot,
    tree_bytes: u64,
    members: &[u32],
) -> String {
    let others: Vec<String> = snapshot
        .app_groups
        .iter()
        .filter(|g| !g.pids.iter().any(|p| members.contains(p)))
        .take(CULPRITS)
        .map(|g| format!("{} {}", g.name, format_bytes(g.memory_bytes)))
        .collect();
    let mut largest = format!("this command {}", format_bytes(tree_bytes));
    for o in others {
        let _ = write!(largest, ", {o}");
    }
    let state = if already {
        format!("was already {} when the command started", level.label())
    } else {
        format!("is {}", level.label())
    };
    format!(
        "{} Memory pressure {state} ({}/100): {}. Using the most: {largest}. canfit closes \
         nothing; run `canfit free` in another terminal to see what is safe to close.",
        level.symbol(),
        snapshot.pressure.score,
        snapshot.pressure.main_reason.message
    )
}

/// Runs `canfit watch`.
pub fn run(options: Options) -> ExitCode {
    let program = options.command.first().cloned().unwrap_or_default();
    let name = watch::run_name(options.name.as_deref(), &program);
    let mut source = match platform::native() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("{err} canfit did not start the command.");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let mut model = model_comparison(&options.command);

    let started = Instant::now();
    let started_unix_ms = now_unix_ms();
    let mut launched = match launch::spawn(&options.command) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(err.exit_code());
        }
    };
    if !options.quiet {
        say(&format!(
            "Watching {} (pid {}): measuring the memory of it and everything it starts every {}. \
             Ctrl-C stops it; canfit then prints a summary.",
            program_display(&program),
            launched.pid,
            interval_text(options.interval)
        ));
    }

    let (stats, ending, warnings, mut notes, tree) =
        watch_loop(&mut launched, source.as_mut(), &options, &name, started);
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // What the command left running (not measured any more, never touched).
    let still_running = source
        .sample(true)
        .map(|raw| {
            let mut tree = tree;
            let processes = raw.processes.unwrap_or_default();
            tree.members(&processes, &launched.extra_pids())
                .iter()
                .filter(|p| p.pid != launched.pid)
                .count()
        })
        .unwrap_or(0);

    if let (Some(m), Some(peak)) = (model.as_mut(), stats.peak()) {
        let leaves_out_mapped = matches!(
            stats.memory_kind(),
            Some(MemoryKind::Footprint | MemoryKind::PrivateWorkingSet)
        );
        if m.mmap && leaves_out_mapped {
            m.mapped_bytes = Some(m.estimate.weights_bytes);
        }
        let base = m.estimate.base_bytes as f64;
        let measured = peak.bytes.saturating_add(m.mapped_bytes.unwrap_or(0)) as f64;
        if base > 0.0 {
            m.difference_percent = Some((measured - base) / base * 100.0);
        }
    }
    if launched.has_job() || cfg!(not(target_os = "windows")) {
        // Nothing to add: the tree is tracked by job and parent links.
    } else {
        notes.push(
            "canfit could not put the command in a job object, so it followed its processes by \
             parent links only."
                .to_string(),
        );
    }

    let outcome = Outcome {
        name,
        program: watch::program_name(&program),
        pid: launched.pid,
        started_unix_ms,
        duration_ms,
        interval_ms: u64::try_from(options.interval.as_millis()).unwrap_or(1000),
        stats,
        ending,
        interrupted: launch::interrupted(),
        warnings,
        still_running,
        model,
        notes,
    };
    let remembered = remember(&outcome);
    let summary = summary_sentence(&outcome);
    let theme = ui::stderr_theme(options.theme);
    eprint!(
        "\n{}",
        glyphs::text(&render(&outcome, &remembered, &summary, theme))
    );
    if let Some(path) = &options.json_out
        && let Err(e) = std::fs::write(path, to_json(&outcome, &remembered, &summary))
    {
        eprintln!(
            "canfit could not write the summary to {}: {e}",
            path.display()
        );
    }
    exit_with(outcome.ending.exit_code())
}

fn program_display(program: &str) -> String {
    watch::program_name(program)
}

fn interval_text(interval: Duration) -> String {
    let secs = interval.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("{} s", secs as u64)
    } else {
        format!("{secs:.1} s")
    }
}

/// Samples until the command ends. Returns the statistics, the ending, the number of warnings,
/// notes about sampling problems and the tree (for the final look).
fn watch_loop(
    launched: &mut Launched,
    source: &mut dyn SystemSource,
    options: &Options,
    name: &str,
    started: Instant,
) -> (Stats, Ending, u32, Vec<String>, Tree) {
    let mut tree = Tree::new(launched.pid, launch::adopter());
    let mut sampler = Sampler::new(64, Duration::from_secs(10));
    let mut stats = Stats::default();
    let mut alerts = Alerts::default();
    let mut warnings = 0u32;
    let mut notes = Vec::new();
    let mut read_error: Option<String> = None;
    let mut last_status = Instant::now();
    loop {
        let tick = Instant::now();
        if let Some(ending) = wait_step(launched, &mut notes) {
            return (stats, ending, warnings, notes, tree);
        }
        match source.sample(true) {
            Ok(raw) => {
                let offset_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let snapshot = sampler.record(raw);
                let extra = launched.extra_pids();
                let members = tree.members(&snapshot.processes, &extra);
                let (bytes, count, unreadable, kinds) = watch::measure(&members);
                let pids: Vec<u32> = members.iter().map(|p| p.pid).collect();
                if count > 0 {
                    stats.record(
                        Point {
                            offset_ms,
                            bytes,
                            processes: count,
                        },
                        unreadable,
                        &kinds,
                    );
                }
                let first = stats.initial_level.is_none();
                stats.record_pressure(&snapshot, offset_ms);
                match alerts.feed(snapshot.pressure.level, offset_ms) {
                    Some(Alert::Rose(level)) => {
                        warnings += 1;
                        // With --quiet, warnings are only counted for the summary.
                        if !options.quiet {
                            say(&warning(level, first, &snapshot, bytes, &pids));
                        }
                    }
                    Some(Alert::Calmed) if !options.quiet => {
                        say("● Memory pressure is calm again.");
                    }
                    _ => {}
                }
                launched.reap(&pids);
                if !options.quiet && last_status.elapsed() >= STATUS_EVERY {
                    last_status = Instant::now();
                    let peak = stats.peak().map_or(0, |p| p.bytes);
                    say(&format!(
                        "{name} · {} · now {}, peak {} (measured) · {} · pressure {}",
                        span(offset_ms),
                        format_bytes(bytes),
                        format_bytes(peak),
                        plural(count, "process", "processes"),
                        snapshot.pressure.level.label()
                    ));
                }
            }
            Err(err) => {
                let text = err.to_string();
                if read_error.as_deref() != Some(text.as_str()) {
                    if !options.quiet {
                        say(&format!("{text} canfit keeps the command running."));
                    }
                    notes.push(format!("Some samples failed: {text}"));
                    read_error = Some(text);
                }
            }
        }
        // Wait for the next tick, noticing the end of the command promptly.
        while tick.elapsed() < options.interval {
            if let Some(ending) = wait_step(launched, &mut notes) {
                return (stats, ending, warnings, notes, tree);
            }
            std::thread::sleep(POLL.min(options.interval.saturating_sub(tick.elapsed())));
        }
    }
}

/// One non-blocking check for the command's end.
fn wait_step(launched: &mut Launched, notes: &mut Vec<String>) -> Option<Ending> {
    match launched.try_wait() {
        Ok(ending) => ending,
        Err(e) => {
            notes.push(format!("canfit could not wait for the command: {e}"));
            Some(Ending::Exited(1))
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn ending_text(o: &Outcome) -> String {
    match o.ending {
        Ending::Exited(code) if o.interrupted => {
            format!("stopped after Ctrl-C (exit code {code})")
        }
        Ending::Exited(0) => "exited normally (code 0)".to_string(),
        Ending::Exited(code) => format!("exited with code {code}"),
        Ending::Signaled(s) => format!("ended by {}", watch::signal_name(s)),
    }
}

/// The plain-language answer that opens the summary.
pub fn summary_sentence_parts(
    name: &str,
    duration_ms: u64,
    peak: Option<u64>,
    pressure: Option<(PressureLevel, u64)>,
    preexisting: bool,
) -> String {
    let Some(peak) = peak else {
        return format!(
            "● {name} ended after {}, before canfit could measure it, so nothing was saved.",
            span(duration_ms)
        );
    };
    let (symbol, tail) = match pressure {
        None | Some((PressureLevel::Calm, _)) => ("●", "memory pressure stayed calm".to_string()),
        Some((level, _)) if preexisting => (
            level.symbol(),
            format!(
                "memory pressure was already {} when it started",
                level.label()
            ),
        ),
        Some((level, at)) => (
            level.symbol(),
            format!("memory pressure reached {} {} in", level.label(), span(at)),
        ),
    };
    format!(
        "{symbol} {name} ran for {} and used at most {} (measured); {tail}.",
        span(duration_ms),
        format_bytes(peak)
    )
}

fn summary_sentence(o: &Outcome) -> String {
    summary_sentence_parts(
        &o.name,
        o.duration_ms,
        o.stats.peak().map(|p| p.bytes).filter(|&b| b > 0),
        o.stats.pressure.as_ref().map(|p| (p.level, p.offset_ms)),
        o.stats.pressure_preexisting(),
    )
}

/// The rows of the summary card.
fn rows(o: &Outcome) -> Vec<Option<Row>> {
    let mut rows = Vec::new();
    let kind = o
        .stats
        .memory_kind()
        .map_or("memory", |k| k.label())
        .to_string();
    if let Some(peak) = o.stats.peak() {
        rows.push(Some(Row::new(
            "Peak",
            format_bytes(peak.bytes),
            format!(
                "measured · {kind} · {} in · {}",
                span(peak.offset_ms),
                plural(peak.processes, "process", "processes")
            ),
        )));
        rows.push(Some(Row::new(
            "Typical",
            format_bytes(o.stats.typical_bytes()),
            format!(
                "measured · median of {}",
                plural(o.stats.points.len(), "sample", "samples")
            ),
        )));
    }
    rows.push(Some(Row::new(
        "Ran for",
        span(o.duration_ms),
        format!(
            "sampled every {} · {}",
            interval_text(Duration::from_millis(o.interval_ms)),
            ending_text(o)
        ),
    )));
    if let Some(p) = &o.stats.pressure {
        let text = if p.level == PressureLevel::Calm {
            format!("estimated · {}/100 at most", p.score)
        } else if o.stats.pressure_preexisting() {
            format!("estimated · {}/100 · already when it started", p.score)
        } else {
            format!("estimated · {}/100 · {} in", p.score, span(p.offset_ms))
        };
        rows.push(Some(Row::new(
            "Pressure",
            format!("{} {}", p.level.symbol(), p.level.label()),
            text,
        )));
    }
    if let Some(m) = &o.model {
        let file = short_name(&m.path);
        let measured = if m.mapped_bytes.is_some() {
            "peak + model file"
        } else {
            "measured peak"
        };
        let comparison = match m.difference_percent {
            Some(d) if d.abs() < 0.5 => format!(" · {measured} matches"),
            Some(d) if d < 0.0 => format!(" · {measured} {:.0}% below", -d),
            Some(d) => format!(" · {measured} {d:.0}% above"),
            None => String::new(),
        };
        rows.push(None);
        if let Some(mapped) = m.mapped_bytes {
            rows.push(Some(Row::new(
                "Model file",
                format_bytes(mapped),
                format!("read from file · weights mapped from {file}, not in the peak"),
            )));
        }
        rows.push(Some(Row::new(
            "canfit estimate",
            format_bytes(m.estimate.base_bytes),
            format!(
                "estimated · {file}, {} tokens{comparison}",
                model::group_digits(m.context_tokens)
            ),
        )));
        rows.push(Some(Row::new(
            "with margin",
            format_bytes(m.estimate.total_bytes),
            format!(
                "estimated · +{}% margin, what a fit check compares",
                m.estimate.margin_percent
            ),
        )));
    }
    rows
}

/// The notes under the card.
fn notes(o: &Outcome, remembered: &Remembered) -> Vec<String> {
    let mut notes = Vec::new();
    if remembered.saved {
        notes.push(format!(
            "Saved: `canfit {}` now uses this peak (measured by watch on {}).",
            shell_word(&remembered.name),
            app::local_date(o.started_unix_ms)
        ));
    } else if let Some(reason) = remembered
        .reason
        .as_ref()
        .filter(|_| remembered.switched_off.is_none())
    {
        notes.push(format!("Not saved: {reason}"));
    }
    if let Some(p) = o
        .stats
        .pressure
        .as_ref()
        .filter(|p| p.level > PressureLevel::Calm)
    {
        let reason = &p.reason;
        notes.push(format!(
            "At the highest pressure, the main reason was: {reason}."
        ));
    }
    if o.still_running > 0 {
        notes.push(format!(
            "{} it started {} still running; canfit leaves {} alone.",
            plural(o.still_running, "process", "processes"),
            if o.still_running == 1 { "is" } else { "are" },
            if o.still_running == 1 { "it" } else { "them" }
        ));
    }
    if o.stats.max_unreadable > 0 {
        notes.push(format!(
            "canfit could not read the memory of {} in the tree (another user's, or protected), \
             so the peak leaves {} out.",
            plural(o.stats.max_unreadable, "process", "processes"),
            if o.stats.max_unreadable == 1 {
                "it"
            } else {
                "them"
            }
        ));
    }
    if let Some(m) = &o.model {
        if !m.context_from_command {
            notes.push(format!(
                "The command sets no context size, so canfit's estimate assumes {} tokens; the \
                 program may use another.",
                model::group_digits(m.context_tokens)
            ));
        }
        let kind = o.stats.memory_kind().map_or("memory", |k| k.label());
        if let Some(mapped) = m.mapped_bytes {
            notes.push(format!(
                "The command maps the model file into memory (mmap) and the {kind} leaves those \
                 pages out, so canfit adds the model's weights ({}, read from the file) to the \
                 peak when it remembers this run.",
                format_bytes(mapped)
            ));
        } else if m.mmap {
            notes.push(format!(
                "The command maps the model file into memory (mmap); the {kind} counts the parts \
                 of it that were in memory."
            ));
        }
    }
    notes.push(format!(
        "Sampled every {}: a spike shorter than that may be missed.",
        interval_text(Duration::from_millis(o.interval_ms))
    ));
    notes.extend(o.notes.iter().cloned());
    if let Some(off) = remembered.switched_off {
        // Last, so the user's own choice is the last thing the summary says.
        notes.push(not_saved_line(off));
    }
    notes
}

/// The summary's last line when the user turned recording off for this run.
fn not_saved_line(off: crate::history::RecordingOff) -> String {
    format!("Not saved ({}).", off.switch())
}

/// A file name for a card row, shortened in the middle when long (Ollama blobs are 71
/// characters).
fn short_name(path: &Path) -> String {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |f| f.to_string_lossy().into_owned(),
    );
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= 40 {
        return name;
    }
    let head: String = chars[..24].iter().collect();
    let tail: String = chars[chars.len() - 12..].iter().collect();
    format!("{head}…{tail}")
}

/// Quotes a name for a shell command when it needs it.
fn shell_word(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        name.to_string()
    } else {
        format!("\"{name}\"")
    }
}

fn render(
    o: &Outcome,
    remembered: &Remembered,
    summary: &str,
    theme: Option<ui::theme::Theme>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", card::sentence(summary, theme));
    // Always: even without a measurement, the card says how long it ran and how it ended.
    {
        out.push('\n');
        out.push_str(&card::render_styled(
            &format!("canfit watch: {}", o.name),
            &rows(o),
            theme,
        ));
    }
    out.push('\n');
    for note in notes(o, remembered) {
        let _ = writeln!(out, "{}", card::wrap(&note, 96, 0).trim_end());
    }
    out
}

fn to_json(o: &Outcome, remembered: &Remembered, summary: &str) -> String {
    let notes = notes(o, remembered);
    let doc = WatchDocument {
        schema: WATCH_SCHEMA,
        schema_version: WATCH_SCHEMA_VERSION,
        canfit_version: env!("CARGO_PKG_VERSION"),
        summary: summary.to_string(),
        history: remembered.history,
        name: &o.name,
        program: &o.program,
        pid: o.pid,
        started_unix_ms: o.started_unix_ms,
        duration_ms: o.duration_ms,
        interval_ms: o.interval_ms,
        samples: o.stats.points.len(),
        memory: o.stats.peak().map(|peak| MemoryDoc {
            kind: o.stats.memory_kind().map(|k| kind_key(k).to_string()),
            provenance: "measured",
            peak_bytes: peak.bytes,
            peak_offset_ms: peak.offset_ms,
            typical_bytes: o.stats.typical_bytes(),
            p95_bytes: o.stats.p95_bytes(),
            last_bytes: o.stats.last().map_or(0, |p| p.bytes),
            max_processes: o.stats.max_processes(),
            max_unreadable_processes: o.stats.max_unreadable,
        }),
        pressure: o.stats.pressure.as_ref().map(|p| PressureDoc {
            peak_level: p.level,
            peak_score: p.score,
            peak_offset_ms: p.offset_ms,
            peak_reason: p.reason.clone(),
            warnings: o.warnings,
        }),
        model: o.model.as_ref(),
        ended: EndingDoc {
            code: o.ending.code(),
            signal: o.ending.signal(),
            interrupted: o.interrupted,
            canfit_exit_code: o.ending.exit_code(),
        },
        still_running: o.still_running,
        remembered,
        notes: &notes,
    };
    let mut text = serde_json::to_string_pretty(&doc).expect("watch summary serializes");
    text.push('\n');
    text
}

fn kind_key(kind: crate::core::MemoryKind) -> &'static str {
    use crate::core::MemoryKind;
    match kind {
        MemoryKind::Footprint => "footprint",
        MemoryKind::Pss => "pss",
        MemoryKind::Rss => "rss",
        MemoryKind::PrivateWorkingSet => "private_working_set",
    }
}

/// Stores the run in the history database (ADR-023), unless history is off or the run has no
/// samples.
fn remember(o: &Outcome) -> Remembered {
    let not = |reason: String| Remembered {
        saved: false,
        name: o.name.clone(),
        reason: Some(reason),
        history: "on",
        switched_off: None,
    };
    if let Some(off) = crate::history::recording_off() {
        return Remembered {
            reason: Some(not_saved_line(off)),
            history: "off",
            switched_off: Some(off),
            ..not(String::new())
        };
    }
    let Some(peak) = o.stats.peak().filter(|p| p.bytes > 0) else {
        return not("canfit could not measure any memory while the command ran.".to_string());
    };
    let settings = match config::load() {
        Ok((c, _)) => c.history,
        Err(e) => return not(format!("{e}")),
    };
    if !settings.enabled {
        return Remembered {
            history: "off",
            ..not("history is off ([history] enabled = false in canfit's config.toml).".into())
        };
    }
    let Some(path) = crate::history::database_path() else {
        return not("canfit found no data directory on this system.".to_string());
    };
    let pressure = o.stats.pressure.clone();
    let run = WatchRun {
        name: o.name.clone(),
        program: o.program.clone(),
        started_unix_ms: o.started_unix_ms,
        duration_ms: o.duration_ms,
        interval_ms: o.interval_ms,
        samples: o.stats.points.len() as u64,
        memory_kind: o
            .stats
            .memory_kind()
            .map_or("unknown", kind_key)
            .to_string(),
        peak_bytes: peak.bytes,
        peak_offset_ms: peak.offset_ms,
        typical_bytes: o.stats.typical_bytes(),
        p95_bytes: o.stats.p95_bytes(),
        max_processes: o.stats.max_processes() as u64,
        peak_pressure_score: pressure.as_ref().map_or(0, |p| p.score),
        peak_pressure_level: pressure.as_ref().map_or(PressureLevel::Calm, |p| p.level),
        exit_code: o.ending.code(),
        exit_signal: o.ending.signal(),
        interrupted: o.interrupted,
        model_path: o.model.as_ref().map(|m| m.path.display().to_string()),
        model_estimate_bytes: o.model.as_ref().map(|m| m.estimate.base_bytes),
        model_mapped_bytes: o.model.as_ref().and_then(|m| m.mapped_bytes),
    };
    if !crate::estimate::apps::learned::usable_run(&run) {
        return not(format!(
            "the command failed after {} ({}), so it probably never got going.",
            span(o.duration_ms),
            ending_text(o)
        ));
    }
    match Store::open(&path).and_then(|s| s.insert_watch_run(&run)) {
        Ok(_) => Remembered {
            saved: true,
            name: o.name.clone(),
            reason: None,
            history: "on",
            switched_off: None,
        },
        Err(e) => not(format!("{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn outcome(points: &[(u64, u64)], ending: Ending) -> Outcome {
        let mut stats = Stats::default();
        for &(t, mib) in points {
            stats.record(
                Point {
                    offset_ms: t,
                    bytes: mib * MIB,
                    processes: 2,
                },
                0,
                &[crate::core::MemoryKind::Footprint],
            );
        }
        Outcome {
            name: "llama-server".to_string(),
            program: "llama-server".to_string(),
            pid: 42,
            started_unix_ms: 1_800_000_000_000,
            duration_ms: 125_000,
            interval_ms: 1000,
            stats,
            ending,
            interrupted: false,
            warnings: 0,
            still_running: 0,
            model: None,
            notes: Vec::new(),
        }
    }

    #[test]
    fn summary_opens_with_an_answer() {
        assert_eq!(
            summary_sentence_parts("x", 2 * 60_000 + 5_000, Some(4096 * MIB), None, false),
            "● x ran for 2 min and used at most 4.0 GB (measured); memory pressure stayed calm."
        );
        assert_eq!(
            summary_sentence_parts(
                "x",
                10_000,
                Some(512 * MIB),
                Some((PressureLevel::Critical, 4_000)),
                false
            ),
            "✗ x ran for 10 s and used at most 512 MB (measured); memory pressure reached \
             critical 4 s in."
        );
        assert!(summary_sentence_parts("x", 100, None, None, false).contains("nothing was saved"));
        assert!(
            summary_sentence_parts(
                "x",
                10_000,
                Some(MIB),
                Some((PressureLevel::Strained, 0)),
                true
            )
            .ends_with("memory pressure was already strained when it started.")
        );
    }

    #[test]
    fn card_and_json_carry_the_measurements() {
        let o = outcome(&[(0, 100), (1000, 300), (2000, 200)], Ending::Signaled(2));
        let remembered = Remembered {
            saved: true,
            name: "llama-server".to_string(),
            reason: None,
            history: "on",
            switched_off: None,
        };
        let text = render(&o, &remembered, &summary_sentence(&o), None);
        assert!(text.starts_with("● llama-server ran for 2 min"), "{text}");
        // A command that ended before the first sample still says how it ended.
        let quick = outcome(&[], Ending::Signaled(9));
        let not_saved = Remembered {
            saved: false,
            name: "llama-server".to_string(),
            reason: Some("nothing measured.".to_string()),
            history: "on",
            switched_off: None,
        };
        let short = render(&quick, &not_saved, &summary_sentence(&quick), None);
        assert!(short.contains("before canfit could measure it"), "{short}");
        assert!(short.contains("canfit watch: llama-server"), "{short}");
        assert!(short.contains("ended by SIGKILL"), "{short}");
        assert!(!short.contains("Peak"), "{short}");
        assert!(text.contains("Peak"), "{text}");
        assert!(text.contains("300 MB"), "{text}");
        assert!(text.contains("ended by SIGINT (Ctrl-C)"), "{text}");
        assert!(
            text.contains("Saved: `canfit llama-server` now uses this peak"),
            "{text}"
        );

        let v: serde_json::Value = serde_json::from_str(&to_json(&o, &remembered, "s")).unwrap();
        assert_eq!(v["schema"], "canfit.watch");
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["memory"]["peak_bytes"], 300 * MIB);
        assert_eq!(v["memory"]["typical_bytes"], 200 * MIB);
        assert_eq!(v["memory"]["kind"], "footprint");
        assert_eq!(v["memory"]["provenance"], "measured");
        assert_eq!(v["ended"]["signal"], 2);
        assert_eq!(v["ended"]["canfit_exit_code"], 130);
        assert_eq!(v["remembered"]["saved"], true);
        assert_eq!(v["history"], "on");
        assert!(v.get("model").is_none());
    }

    #[test]
    fn no_save_is_the_last_line_and_history_is_off() {
        use crate::history::RecordingOff;
        let o = outcome(&[(0, 100), (1000, 300)], Ending::Exited(0));
        let remembered = Remembered {
            saved: false,
            name: "llama-server".to_string(),
            reason: Some(not_saved_line(RecordingOff::NoSave)),
            history: "off",
            switched_off: Some(RecordingOff::NoSave),
        };
        let text = render(&o, &remembered, &summary_sentence(&o), None);
        assert_eq!(
            text.trim_end().lines().last(),
            Some("Not saved (--no-save)."),
            "{text}"
        );
        assert!(!text.contains("Not saved:"), "{text}");
        let v: serde_json::Value = serde_json::from_str(&to_json(&o, &remembered, "s")).unwrap();
        assert_eq!(v["history"], "off");
        assert_eq!(v["remembered"]["saved"], false);
        assert_eq!(v["remembered"]["reason"], "Not saved (--no-save).");
        assert_eq!(
            not_saved_line(RecordingOff::Flag),
            "Not saved (--no-history)."
        );
    }

    #[test]
    fn long_file_names_are_shortened() {
        assert_eq!(short_name(Path::new("/m/model.gguf")), "model.gguf");
        assert_eq!(
            short_name(Path::new(
                "/b/sha256-4a188102020e9c9530b687fd6400f775c45e90a0d7baafe65bd0a36963fbb7ba"
            )),
            "sha256-4a188102020e9c953…a36963fbb7ba"
        );
    }

    #[test]
    fn names_are_quoted_for_the_shell_when_needed() {
        assert_eq!(shell_word("llama-server"), "llama-server");
        assert_eq!(shell_word("my server"), "\"my server\"");
    }
}
