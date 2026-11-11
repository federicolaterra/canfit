//! The advisor on the command line: `canfit free` (the "what to close" list),
//! `canfit TARGET --make-room` / `canfit free TARGET` (one shared implementation) and the
//! confirmed closing prompt.
//!
//! Closing is offered only when both stdin and stdout are terminals and `--json` is off;
//! otherwise canfit only reports suggestions (ADR-013).

use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

use serde::Serialize;

use super::card::{self, Row};
use super::fit::FitReport;
use super::{EXIT_RUNTIME, report};
use crate::advisor::close::{self, CloseError, CloseOptions, CloseReport, Confirm};
use crate::advisor::plan::{self, Suggestion};
use crate::advisor::{self, Advice, SafetyClass, config};
use crate::core::{PressureLevel, Provenance, Snapshot, format_bytes};
use crate::estimate::fit::{self as verdict, LimitKind, TightCause, Verdict};
use crate::platform;
use crate::ui::{self, theme::Theme, theme::ThemeName};

/// Identifies the `canfit free` JSON document.
pub const FREE_SCHEMA: &str = "canfit.free";
pub const FREE_SCHEMA_VERSION: u32 = 1;

/// How many apps the text list shows (JSON lists all of them).
const LIST_LIMIT: usize = 10;

/// Paragraph width of explanations.
const WRAP_WIDTH: usize = 100;

/// Classifies the snapshot with the user's protected list. A config file that can't be read is
/// an error: canfit can't know what the user protected.
pub fn load(snapshot: &Snapshot) -> Result<Advice, String> {
    let (config, _) = config::load().map_err(|e| e.to_string())?;
    Ok(advisor::advise(
        snapshot,
        std::process::id(),
        config.processes.protected,
    ))
}

/// Whether canfit may ask to close anything: never under `--json` or without a terminal on both
/// ends, never on an OS where it can't close processes.
pub fn interactive(json: bool) -> bool {
    may_prompt(
        json,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        platform::process_control().is_some(),
    )
}

/// Whether both stdin and stdout are terminals, so canfit may ask a question.
pub fn interactive_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn may_prompt(json: bool, stdin_tty: bool, stdout_tty: bool, can_close: bool) -> bool {
    !json && stdin_tty && stdout_tty && can_close
}

// ---------------------------------------------------------------------------------------------
// Make room

/// What the make-room step concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MakeRoomStatus {
    /// It already fits: nothing to close.
    NotNeeded,
    /// Closing `apps` frees enough.
    Plan,
    /// It fits on paper but pressure is critical: closing `apps` would ease it.
    EasePressure,
    /// Even closing every suggestable app is not enough; `apps` lists them all.
    NotEnough,
    /// The GPU memory limit is the problem; closing apps can't raise it.
    GpuLimit,
}

#[derive(Debug, Clone, Serialize)]
pub struct MakeRoom {
    pub status: MakeRoomStatus,
    /// Memory to free so the targets fit with the margin: `needed − available`.
    pub needed_bytes: u64,
    pub needed_provenance: Provenance,
    /// What closing `apps` should free: their measured memory, an estimate of what is freed.
    pub freed_bytes: u64,
    pub freed_provenance: Provenance,
    pub apps: Vec<Suggestion>,
    /// The verdict expected after closing `apps` (estimated); absent when nothing would change.
    pub projected_verdict: Option<Verdict>,
    pub projected_available_bytes: Option<u64>,
    /// The "Why?" paragraph.
    pub why: String,
}

impl MakeRoom {
    /// Whether closing `apps` is worth offering.
    pub fn closable(&self) -> bool {
        matches!(
            self.status,
            MakeRoomStatus::Plan | MakeRoomStatus::EasePressure
        ) && !self.apps.is_empty()
    }
}

/// Works out what to close for the fit check `report`, using one shared implementation for
/// `--make-room` and `canfit free TARGET`.
pub fn make_room(report: &FitReport, snapshot: &Snapshot, advice: &Advice) -> MakeRoom {
    let base = report.total.base_bytes;
    let total = report.total.total_bytes;
    let available = snapshot.memory.available_bytes;
    let needed = total.saturating_sub(available);
    let gpu_blocks = report
        .result
        .checks
        .iter()
        .any(|c| c.limit == LimitKind::GpuMemoryLimit && c.verdict == Verdict::DoesntFit);
    let mut out = MakeRoom {
        status: MakeRoomStatus::NotNeeded,
        needed_bytes: needed,
        needed_provenance: Provenance::Estimated,
        freed_bytes: 0,
        freed_provenance: Provenance::Estimated,
        apps: Vec::new(),
        projected_verdict: None,
        projected_available_bytes: None,
        why: advice.why.clone(),
    };
    if gpu_blocks {
        out.status = MakeRoomStatus::GpuLimit;
        return out;
    }
    let pressure_only = report
        .result
        .checks
        .iter()
        .any(|c| c.tight_cause == Some(TightCause::PressureCritical));
    if needed == 0 {
        if pressure_only && let Some(first) = advice.suggestions.first() {
            out.status = MakeRoomStatus::EasePressure;
            out.freed_bytes = first.memory_bytes;
            out.apps = vec![first.clone()];
        }
        return out;
    }
    let plan = plan::make_room(&advice.suggestions, needed);
    out.status = if plan.enough {
        MakeRoomStatus::Plan
    } else {
        MakeRoomStatus::NotEnough
    };
    out.freed_bytes = plan.freed_bytes;
    out.apps = plan.chosen;
    if out.status == MakeRoomStatus::Plan {
        let mut after = snapshot.clone();
        after.memory.available_bytes = available
            .saturating_add(out.freed_bytes)
            .min(snapshot.memory.total_bytes);
        let projected = verdict::evaluate_parts(base, total, report.gpu_part, &after);
        out.projected_verdict = Some(projected.verdict);
        out.projected_available_bytes = Some(after.memory.available_bytes);
    }
    out
}

fn names(apps: &[Suggestion]) -> String {
    let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn apps_word(n: usize) -> &'static str {
    if n == 1 { "app" } else { "apps" }
}

/// The sentence that follows the fit answer.
pub fn make_room_sentence(m: &MakeRoom, report: &FitReport) -> String {
    let freed = format_bytes(m.freed_bytes);
    match m.status {
        MakeRoomStatus::NotNeeded => "Nothing needs to be closed.".to_string(),
        MakeRoomStatus::Plan => {
            let then = match m.projected_verdict {
                Some(Verdict::Fits) => "it should fit",
                Some(Verdict::Tight) => "it should fit, but only just",
                _ => "it may still not fit",
            };
            format!(
                "To make room, close {}: that frees about {freed} (estimated), and then {then}.",
                names(&m.apps)
            )
        }
        MakeRoomStatus::EasePressure => format!(
            "Closing {} would ease the pressure: it frees about {freed} (estimated).",
            names(&m.apps)
        ),
        MakeRoomStatus::NotEnough if m.apps.is_empty() => {
            "canfit has nothing it can suggest closing to make room.".to_string()
        }
        MakeRoomStatus::NotEnough => format!(
            "Closing every app canfit can suggest frees only about {freed} (estimated), still \
             {} short of the {} needed.",
            format_bytes(m.needed_bytes.saturating_sub(m.freed_bytes)),
            format_bytes(m.needed_bytes)
        ),
        MakeRoomStatus::GpuLimit => {
            let limit = report
                .result
                .checks
                .iter()
                .find(|c| c.limit == LimitKind::GpuMemoryLimit)
                .map_or(0, |c| c.limit_bytes);
            format!(
                "Closing apps won't help: the GPU can use at most {}, less than needed. Try a \
                 smaller quantization or a shorter context (--ctx).",
                format_bytes(limit)
            )
        }
    }
}

fn suggestion_rows(apps: &[Suggestion]) -> Vec<Option<Row>> {
    let mut rows = Vec::new();
    for (i, a) in apps.iter().enumerate() {
        let processes = match a.process_count {
            1 => "1 process".to_string(),
            n => format!("{n} processes"),
        };
        rows.push(Some(Row::new(
            format!("{} {} {}", i + 1, a.class.symbol(), a.name),
            format_bytes(a.memory_bytes),
            format!("{} · measured · {processes}", a.class.label()),
        )));
        rows.push(Some(Row::new("", "", a.reason.clone())));
        if let Some(c) = a.manual_commands.iter().find(|c| !c.forced) {
            rows.push(Some(Row::new("", "", format!("yourself: {}", c.short()))));
        }
    }
    rows
}

/// The make-room card, or nothing when there is nothing to list.
pub fn render_make_room(m: &MakeRoom, theme: Option<Theme>) -> String {
    if m.apps.is_empty() {
        return String::new();
    }
    let mut rows = suggestion_rows(&m.apps);
    rows.push(None);
    rows.push(Some(Row::new(
        "Frees",
        format_bytes(m.freed_bytes),
        format!(
            "estimated · measured memory of {} {}",
            m.apps.len(),
            apps_word(m.apps.len())
        ),
    )));
    if let (Some(v), Some(after)) = (m.projected_verdict, m.projected_available_bytes) {
        let word = match v {
            Verdict::Fits => "fits",
            Verdict::Tight => "tight",
            Verdict::DoesntFit => "doesn't fit",
        };
        rows.push(Some(Row::new(
            "Available after",
            format_bytes(after),
            format!("estimated · {} {word}", v.symbol()),
        )));
    }
    let title = match m.status {
        MakeRoomStatus::NotEnough => "Everything canfit could close",
        _ => "Close to make room",
    };
    card::render_styled(title, &rows, theme)
}

/// Notes printed under the make-room card.
pub fn make_room_notes(m: &MakeRoom) -> String {
    let mut out = String::new();
    if !m.apps.is_empty() {
        out.push_str(&card::wrap(CLOSING_NOTE, WRAP_WIDTH, 2));
    }
    out.push_str(&card::wrap(&format!("Why? {}", m.why), WRAP_WIDTH, 2));
    out
}

const CLOSING_NOTE: &str = "Each app's memory is measured; closing it frees about that much \
    (estimated), less if part of it is compressed or swapped. ● safe apps restore their state; \
    ▲ caution apps may lose unsaved work. canfit never suggests system processes, your terminal, \
    itself or apps you protected. \"yourself:\" is the safest command to close an app by hand \
    (canfit never runs it; --json lists every command, forced ones last).";

// ---------------------------------------------------------------------------------------------
// canfit free (no target)

#[derive(Serialize)]
struct FreeDocument<'a> {
    schema: &'static str,
    schema_version: u32,
    canfit_version: &'static str,
    summary: String,
    why: &'a str,
    pressure_level: PressureLevel,
    memory_available_bytes: u64,
    memory_total_bytes: u64,
    suggestions: &'a [Suggestion],
    /// The largest apps canfit will not suggest, with the reason.
    not_suggested: Vec<NotSuggested<'a>>,
}

#[derive(Serialize)]
struct NotSuggested<'a> {
    name: &'a str,
    memory_bytes: u64,
    memory_provenance: Provenance,
    reason: &'a str,
}

/// How many never-class apps are named as "not suggested".
const NOT_SUGGESTED_LIMIT: usize = 5;

fn not_suggested(advice: &Advice) -> Vec<NotSuggested<'_>> {
    advice
        .groups
        .iter()
        .filter(|g| {
            g.class == SafetyClass::Never && g.group.memory_bytes >= plan::MIN_SUGGESTION_BYTES
        })
        .take(NOT_SUGGESTED_LIMIT)
        .map(|g| NotSuggested {
            name: &g.group.name,
            memory_bytes: g.group.memory_bytes,
            memory_provenance: Provenance::Measured,
            reason: &g.reason,
        })
        .collect()
}

/// The answer sentence of `canfit free`.
pub fn free_summary(snapshot: &Snapshot, advice: &Advice) -> String {
    let level = match snapshot.pressure.level {
        PressureLevel::Calm => "Memory is calm",
        PressureLevel::Strained => "Memory is under pressure",
        PressureLevel::Critical => "Memory is critical",
    };
    let available = format_bytes(snapshot.memory.available_bytes);
    match advice.suggestions.first() {
        None => format!(
            "{level} ({available} available) and canfit has nothing it can suggest closing: the \
             biggest apps are system processes, protected, or hosting canfit."
        ),
        Some(s) => {
            let how = match s.class {
                SafetyClass::Safe => "safe to close",
                _ => "closable with care",
            };
            format!(
                "{level} ({available} available). The best app to close is {}: it frees about {} \
                 and is {how}.",
                s.name,
                format_bytes(s.memory_bytes)
            )
        }
    }
}

pub fn free_to_json(snapshot: &Snapshot, advice: &Advice) -> String {
    let document = FreeDocument {
        schema: FREE_SCHEMA,
        schema_version: FREE_SCHEMA_VERSION,
        canfit_version: env!("CARGO_PKG_VERSION"),
        summary: free_summary(snapshot, advice),
        why: &advice.why,
        pressure_level: snapshot.pressure.level,
        memory_available_bytes: snapshot.memory.available_bytes,
        memory_total_bytes: snapshot.memory.total_bytes,
        suggestions: &advice.suggestions,
        not_suggested: not_suggested(advice),
    };
    serde_json::to_string_pretty(&document).expect("free document serializes")
}

pub fn free_to_text(snapshot: &Snapshot, advice: &Advice, theme: Option<Theme>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}\n",
        card::sentence(&free_summary(snapshot, advice), theme)
    );
    let shown = &advice.suggestions[..advice.suggestions.len().min(LIST_LIMIT)];
    if !shown.is_empty() {
        out.push_str(&card::render_styled(
            "What to close",
            &suggestion_rows(shown),
            theme,
        ));
        if advice.suggestions.len() > shown.len() {
            let _ = writeln!(
                out,
                "  {} more smaller {} not shown; --json lists all of them.",
                advice.suggestions.len() - shown.len(),
                apps_word(advice.suggestions.len() - shown.len())
            );
        }
        out.push_str(&card::wrap(CLOSING_NOTE, WRAP_WIDTH, 2));
    }
    let skipped: Vec<String> = not_suggested(advice)
        .iter()
        .take(3)
        .map(|n| {
            format!(
                "{} ({}: {})",
                n.name,
                format_bytes(n.memory_bytes),
                n.reason
            )
        })
        .collect();
    if !skipped.is_empty() {
        out.push_str(&card::wrap(
            &format!("Not suggested: {}.", skipped.join("; ")),
            WRAP_WIDTH,
            2,
        ));
    }
    out.push_str(&card::wrap(&format!("Why? {}", advice.why), WRAP_WIDTH, 2));
    out
}

/// `canfit free`: the "what to close" list, then an optional confirmed close.
pub fn run_free(json: bool, theme: ThemeName) -> ExitCode {
    let snapshot = match report::take_snapshot(true) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let advice = match load(&snapshot) {
        Ok(a) => a,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    if json {
        println!("{}", free_to_json(&snapshot, &advice));
        return ExitCode::SUCCESS;
    }
    print!(
        "{}",
        ui::glyphs::text(&free_to_text(&snapshot, &advice, ui::stdout_theme(theme)))
    );
    if interactive(false) && !advice.suggestions.is_empty() {
        let shown = advice.suggestions.len().min(LIST_LIMIT);
        let mut ui = TerminalConfirm;
        let answer = ui.ask(&format!(
            "Type the numbers of the apps to close (for example 1 3), or press Enter (or n) to \
             close nothing [1-{shown}]:"
        ));
        match parse_selection(&answer, shown) {
            Ok(picked) if picked.is_empty() => println!("Nothing was closed."),
            Ok(picked) => {
                let apps: Vec<Suggestion> = picked
                    .into_iter()
                    .map(|i| advice.suggestions[i].clone())
                    .collect();
                if let Some(closed) = offer_close(&apps, &snapshot) {
                    print_after_close(&closed);
                }
            }
            Err(msg) => println!("{msg} Nothing was closed."),
        }
    }
    ExitCode::SUCCESS
}

/// Parses "1 3", "1,3" or "" into zero-based indexes below `max`. "n", "no" and "q" (any case)
/// mean "close nothing", like an empty answer.
fn parse_selection(answer: &str, max: usize) -> Result<Vec<usize>, String> {
    if matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "n" | "no" | "q"
    ) {
        return Ok(Vec::new());
    }
    let mut picked = Vec::new();
    for part in answer.split(|c: char| c == ',' || c.is_whitespace()) {
        if part.is_empty() {
            continue;
        }
        match part.parse::<usize>() {
            Ok(n) if (1..=max).contains(&n) => {
                if !picked.contains(&(n - 1)) {
                    picked.push(n - 1);
                }
            }
            _ => return Err(format!("\"{part}\" is not one of the listed numbers.")),
        }
    }
    Ok(picked)
}

// ---------------------------------------------------------------------------------------------
// Closing

/// Asks on the terminal. Anything but "y" or "yes" is a no, including end of input.
pub struct TerminalConfirm;

impl TerminalConfirm {
    fn ask(&mut self, question: &str) -> String {
        print!("\n{} ", ui::glyphs::text(question));
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => String::new(),
            Ok(_) => line.trim().to_string(),
        }
    }
}

impl Confirm for TerminalConfirm {
    fn show(&mut self, text: &str) {
        print!("\n{}", ui::glyphs::text(text));
    }

    fn confirm(&mut self, question: &str) -> bool {
        is_yes(&self.ask(&format!("{question} [y/N]")))
    }
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Runs the confirmed closing flow for `apps` and prints what happened. Returns the report when
/// anything was signalled.
pub fn offer_close(apps: &[Suggestion], snapshot: &Snapshot) -> Option<CloseReport> {
    let mut control = platform::process_control()?;
    let mut ui = TerminalConfirm;
    match close::close_apps(
        apps,
        &snapshot.processes,
        control.as_mut(),
        &mut ui,
        CloseOptions::default(),
    ) {
        Ok(report) if report.cancelled => {
            println!("Nothing was closed.");
            None
        }
        Ok(report) => {
            print!("{}", describe_close(&report));
            Some(report)
        }
        Err(CloseError::Empty) => {
            println!("Those apps have already quit.");
            None
        }
        Err(CloseError::NeverClass(name)) => {
            println!("canfit never closes {name}.");
            None
        }
    }
}

fn describe_close(r: &CloseReport) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "\nClosed {} of {} processes",
        r.closed.len(),
        r.targets.len()
    );
    if !r.forced.is_empty() {
        let _ = write!(out, " ({} forced)", r.forced.len());
    }
    let _ = writeln!(out, ".");
    if !r.still_running.is_empty() {
        let _ = write!(out, "Still running:\n{}", close::describe(&r.still_running));
        if r.force_declined {
            let _ = writeln!(
                out,
                "They may be waiting for you to save or confirm; check their windows."
            );
        }
    }
    for (t, err) in &r.errors {
        let _ = writeln!(out, "Could not close {} (pid {}): {err}", t.name, t.pid);
    }
    out
}

fn print_after_close(_closed: &CloseReport) {
    match report::take_snapshot(false) {
        Ok(after) => println!(
            "Available memory now: {} (measured).",
            format_bytes(after.memory.available_bytes)
        ),
        Err(err) => eprintln!("{err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::classify::tests::{GB, MB, desktop, snapshot_with};
    use crate::cli::fit::{self, ModelEntry};
    use crate::core::GpuMemoryLimit;
    use crate::estimate::gguf::builder::Gguf;
    use crate::estimate::model::{self, EstimateOptions};

    fn advice_for(s: &Snapshot) -> Advice {
        advisor::advise(s, crate::advisor::classify::tests::CANFIT_PID, vec![])
    }

    fn model_entry() -> ModelEntry {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "canfit-advice-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.gguf");
        Gguf::llama("tiny", 2).write(&path);
        let facts = model::load(&[path]).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        let estimate = model::estimate(
            &facts,
            EstimateOptions {
                overhead: model::OverheadProfile::Uncalibrated,
                ..EstimateOptions::default()
            },
        );
        ModelEntry { facts, estimate }
    }

    /// The desktop table with `available` bytes free and a GPU limit.
    fn desktop_snapshot(available: u64, gpu: u64) -> Snapshot {
        let mut s = snapshot_with(desktop());
        s.memory.available_bytes = available;
        s.gpu_memory_limit = Some(GpuMemoryLimit {
            bytes: gpu,
            provenance: Provenance::Estimated,
            source: "test".to_string(),
        });
        s
    }

    #[test]
    fn prompts_only_on_a_terminal_without_json() {
        assert!(may_prompt(false, true, true, true));
        assert!(!may_prompt(true, true, true, true));
        assert!(!may_prompt(false, false, true, true));
        assert!(!may_prompt(false, true, false, true));
        assert!(!may_prompt(false, true, true, false));
    }

    #[test]
    fn only_an_explicit_yes_is_yes() {
        for yes in ["y", "Y", "yes", "YES"] {
            assert!(is_yes(yes), "{yes}");
        }
        for no in ["", "n", "no", "ye", "yes please", "1"] {
            assert!(!is_yes(no), "{no}");
        }
    }

    #[test]
    fn selection_accepts_numbers_and_rejects_anything_else() {
        assert_eq!(parse_selection("", 5), Ok(vec![]));
        assert_eq!(parse_selection("1 3", 5), Ok(vec![0, 2]));
        assert_eq!(parse_selection("3,1, 3", 5), Ok(vec![2, 0]));
        assert!(parse_selection("0", 5).is_err());
        assert!(parse_selection("6", 5).is_err());
        assert!(parse_selection("all", 5).is_err());
        assert!(parse_selection("y", 5).is_err());
        // "n", "no" and "q" close nothing, plainly; other words stay errors (also nothing).
        for nothing in ["n", "N", "no", "No", " NO ", "q", "Q"] {
            assert_eq!(parse_selection(nothing, 5), Ok(vec![]), "{nothing:?}");
        }
        for invalid in ["nope", "quit", "n 1", "yes"] {
            assert!(parse_selection(invalid, 5).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn make_room_plans_the_smallest_safe_set_and_projects_the_verdict() {
        let entry = model_entry();
        let need = entry.estimate.total_bytes;
        // Short by 500 MB: Slack (1000 MB, safe) is the smallest single safe app that covers it.
        let s = desktop_snapshot(need - 500 * MB, 64 * GB);
        let report = fit::build(vec![entry], &s);
        assert_eq!(report.result.verdict, Verdict::DoesntFit);
        let m = make_room(&report, &s, &advice_for(&s));
        assert_eq!(m.status, MakeRoomStatus::Plan);
        assert_eq!(m.needed_bytes, 500 * MB);
        let names: Vec<&str> = m.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Slack"]);
        assert_eq!(m.projected_verdict, Some(Verdict::Fits));
        assert!(m.closable());
        let sentence = make_room_sentence(&m, &report);
        assert!(
            sentence.starts_with("To make room, close Slack: that frees about 1000 MB (estimated)"),
            "{sentence}"
        );
        let text = fit::render_text(&report, Some(&m), None);
        assert!(text.contains("Close to make room"), "{text}");
        assert!(text.contains("Why? Memory is"), "{text}");
        let v: serde_json::Value = serde_json::from_str(&fit::to_json(&report, Some(&m))).unwrap();
        assert_eq!(v["make_room"]["status"], "plan");
        assert_eq!(v["make_room"]["apps"][0]["name"], "Slack");
        assert_eq!(v["make_room"]["apps"][0]["class"], "safe");
        assert_eq!(v["make_room"]["freed_provenance"], "estimated");
        assert_eq!(v["make_room"]["projected_verdict"], "fits");
    }

    #[test]
    fn never_class_apps_never_reach_make_room() {
        let entry = model_entry();
        let need = entry.estimate.total_bytes;
        let s = desktop_snapshot(need, 256 * GB);
        let mut report = fit::build(vec![entry], &s);
        // Pretend the model needs 100 GB: more than everything suggestable.
        report.total.base_bytes = 90 * GB;
        report.total.total_bytes = 100 * GB;
        report.result = crate::estimate::fit::evaluate(90 * GB, 100 * GB, &s);
        let advice = advice_for(&s);
        let m = make_room(&report, &s, &advice);
        assert_eq!(m.status, MakeRoomStatus::NotEnough);
        assert!(!m.closable());
        let never: Vec<&str> = advice
            .groups
            .iter()
            .filter(|g| g.class == SafetyClass::Never)
            .map(|g| g.group.name.as_str())
            .collect();
        assert!(never.contains(&"Terminal") && never.contains(&"WindowServer"));
        assert!(m.apps.iter().all(|a| !never.contains(&a.name.as_str())));
        assert!(
            make_room_sentence(&m, &report).starts_with("Closing every app canfit can suggest")
        );
    }

    #[test]
    fn gpu_limit_and_fitting_models_need_no_closing() {
        let entry = model_entry();
        let need = entry.estimate.total_bytes;
        let s = desktop_snapshot(need - 500 * MB, need / 4);
        let report = fit::build(vec![entry.clone()], &s);
        let m = make_room(&report, &s, &advice_for(&s));
        assert_eq!(m.status, MakeRoomStatus::GpuLimit);
        assert!(m.apps.is_empty() && !m.closable());
        assert!(make_room_sentence(&m, &report).starts_with("Closing apps won't help"));

        let s = desktop_snapshot(need * 2, need * 2);
        let report = fit::build(vec![entry], &s);
        let m = make_room(&report, &s, &advice_for(&s));
        assert_eq!(m.status, MakeRoomStatus::NotNeeded);
        assert_eq!(
            make_room_sentence(&m, &report),
            "Nothing needs to be closed."
        );
    }

    #[test]
    fn free_list_answers_first_and_documents_json() {
        let s = desktop_snapshot(4 * GB, 10 * GB);
        let advice = advice_for(&s);
        let text = free_to_text(&s, &advice, None);
        assert!(
            text.starts_with(
                "Memory is calm (4.0 GB available). The best app to close is Google Chrome"
            ),
            "{text}"
        );
        assert!(text.contains("What to close"));
        assert!(!text.contains("WindowServer  "), "{text}");
        assert!(
            text.contains("Not suggested: WindowServer (900 MB: belongs to another user"),
            "{text}"
        );
        let v: serde_json::Value = serde_json::from_str(&free_to_json(&s, &advice)).unwrap();
        assert_eq!(v["schema"], FREE_SCHEMA);
        assert_eq!(v["schema_version"], FREE_SCHEMA_VERSION);
        assert_eq!(v["suggestions"][0]["name"], "Google Chrome");
        assert_eq!(v["suggestions"][0]["memory_provenance"], "measured");
        let commands = v["suggestions"][0]["manual_commands"].as_array().unwrap();
        assert!(!commands.is_empty());
        assert_eq!(commands.last().unwrap()["forced"], true);
        assert!(text.contains("yourself: "), "{text}");
        assert_eq!(v["not_suggested"][0]["name"], "WindowServer");
        assert!(
            v["suggestions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["class"] != "never")
        );
    }
}
