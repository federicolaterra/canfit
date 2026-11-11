//! `canfit <target>...`: the fit check for GGUF models and for apps with declared memory
//! (ADR-020) — a card like spec §11, `--json` (`canfit.fit` schema) and exit codes 0 fits /
//! 1 tight / 2 doesn't fit.

use std::fmt::Write as _;
use std::process::ExitCode;

use serde::Serialize;

use super::advice::{self, MakeRoom};
use super::app::{self, AppEntry, Sources};
use super::card::{self, Row};
use super::{EXIT_RUNTIME, report};
use crate::core::{PressureLevel, Provenance, Snapshot, format_bytes};
use crate::estimate::apps::profiles;
use crate::estimate::apps::resolve::{self, Resolution};
use crate::estimate::fit::{self, FitResult, LimitCheck, LimitKind, TightCause, Verdict};
use crate::estimate::model::{
    self, ContextSource, Estimate, EstimateOptions, ModelFacts, format_parameters, group_digits,
};
use crate::estimate::target::{self, Target, TargetError};
use crate::ui::{self, theme::ThemeName};

/// Identifies the fit-check JSON document.
pub const FIT_SCHEMA: &str = "canfit.fit";
/// Bumped on any breaking change to the fit-check JSON layout.
pub const FIT_SCHEMA_VERSION: u32 = 1;

/// One model and its estimate.
#[derive(Debug, Clone, Serialize)]
pub struct ModelEntry {
    #[serde(flatten)]
    pub facts: ModelFacts,
    pub estimate: Estimate,
}

/// Everything the fit check found, before rendering: models, apps, or both (summed).
#[derive(Debug, Clone)]
pub struct FitReport {
    pub models: Vec<ModelEntry>,
    /// The app targets (ADR-020, ADR-021).
    pub apps: Vec<AppEntry>,
    /// Sum of every target's need.
    pub total: Totals,
    /// The models' part `(without, with margin)`: the only part checked against the GPU memory
    /// limit. `None` without models.
    pub gpu_part: Option<(u64, u64)>,
    pub result: FitResult,
    pub snapshot_memory_total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Totals {
    pub weights_bytes: u64,
    pub kv_cache_bytes: u64,
    pub overhead_bytes: u64,
    /// The apps' part of `base_bytes`: their needs, any estimated overhead included.
    pub apps_bytes: u64,
    pub margin_bytes: u64,
    pub base_bytes: u64,
    pub total_bytes: u64,
    pub provenance: Provenance,
}

impl Totals {
    fn new() -> Self {
        Totals {
            weights_bytes: 0,
            kv_cache_bytes: 0,
            overhead_bytes: 0,
            apps_bytes: 0,
            margin_bytes: 0,
            base_bytes: 0,
            total_bytes: 0,
            provenance: Provenance::Estimated,
        }
    }

    fn add(&mut self, e: &Estimate) {
        self.weights_bytes += e.weights_bytes;
        self.kv_cache_bytes += e.kv_cache_bytes;
        self.overhead_bytes += e.overhead_bytes;
        self.margin_bytes += e.margin_bytes;
        self.base_bytes += e.base_bytes;
        self.total_bytes += e.total_bytes;
    }
}

/// What the command line asked for, before the system is measured.
struct Request {
    models: Vec<ModelEntry>,
    /// App names, in command-line order.
    apps: Vec<String>,
    /// What apps are estimated from; loaded only when an app is checked.
    sources: Sources,
    margin_percent: u32,
}

impl Request {
    /// Measures what the request needs against `snapshot`.
    fn assemble(&self, snapshot: &Snapshot) -> Result<FitReport, String> {
        let mut checks = Vec::new();
        for name in &self.apps {
            checks.push(app::check_target(name, &self.sources, snapshot)?);
        }
        Ok(assemble(
            self.models.clone(),
            checks,
            self.margin_percent,
            snapshot,
        ))
    }
}

/// The report for any mix of models and app checks: models alone, one app alone, or several
/// targets summed (spec §5).
pub fn assemble(
    models: Vec<ModelEntry>,
    mut checks: Vec<crate::estimate::apps::check::AppCheck>,
    margin_percent: u32,
    snapshot: &Snapshot,
) -> FitReport {
    if checks.is_empty() {
        return build(models, snapshot);
    }
    if models.is_empty() && checks.len() == 1 {
        return app::build(checks.remove(0), margin_percent, snapshot);
    }
    let apps = checks
        .into_iter()
        .map(|c| app::entry(c, margin_percent, snapshot, false))
        .collect();
    build_combined(models, apps, snapshot)
}

/// Several targets together: every need is summed against available memory; only the models'
/// part is checked against the GPU memory limit (ADR-021).
pub fn build_combined(
    models: Vec<ModelEntry>,
    apps: Vec<AppEntry>,
    snapshot: &Snapshot,
) -> FitReport {
    let mut total = Totals::new();
    for m in &models {
        total.add(&m.estimate);
    }
    let gpu_part = (!models.is_empty()).then_some((total.base_bytes, total.total_bytes));
    for a in &apps {
        let margin = app::margin_of(a);
        total.apps_bytes += a.check.need_bytes;
        total.base_bytes += a.check.need_bytes;
        total.margin_bytes += margin;
        total.total_bytes += a.check.need_bytes + margin;
    }
    let result = fit::evaluate_parts(total.base_bytes, total.total_bytes, gpu_part, snapshot);
    FitReport {
        models,
        apps,
        total,
        gpu_part,
        result,
        snapshot_memory_total: snapshot.memory.total_bytes,
    }
}

impl FitReport {
    /// Several targets, at least one of them an app.
    pub fn is_combined(&self) -> bool {
        !self.apps.is_empty() && self.models.len() + self.apps.len() > 1
    }
}

/// Runs the fit check and returns the process exit code. With `make_room` (also
/// `canfit free TARGET`) it adds what to close and, on a terminal, offers to close it.
pub fn run(
    targets: &[String],
    apps: &[String],
    make_room: bool,
    options: EstimateOptions,
    json: bool,
    theme: ThemeName,
) -> ExitCode {
    let request = match prepare(targets, apps, options) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let has_app = !request.apps.is_empty();
    // Make room and apps need the per-process scan; the plain model check does not.
    let snapshot = match report::take_snapshot(make_room || has_app) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let report = match request.assemble(&snapshot) {
        Ok(report) => report,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let plan = if make_room {
        match advice::load(&snapshot) {
            Ok(mut advice) => {
                // Never suggest closing the app being checked (or what may hold its memory).
                let excluded: Vec<&String> = report
                    .apps
                    .iter()
                    .flat_map(|a| &a.excluded_groups)
                    .collect();
                advice.suggestions.retain(|s| !excluded.contains(&&s.name));
                Some(advice::make_room(&report, &snapshot, &advice))
            }
            Err(err) => {
                eprintln!("{err}");
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    } else {
        None
    };
    if json {
        println!("{}", to_json(&report, plan.as_ref()));
        return ExitCode::from(report.result.verdict.exit_code());
    }
    print!(
        "{}",
        ui::glyphs::text(&render_text(
            &report,
            plan.as_ref(),
            ui::stdout_theme(theme)
        ))
    );
    if let Some(plan) = plan.filter(|p| p.closable())
        && advice::interactive(false)
        && advice::offer_close(&plan.apps, &snapshot).is_some()
    {
        // Answer again with the memory available now.
        let after = report::take_snapshot(has_app)
            .map_err(|e| e.to_string())
            .and_then(|after| request.assemble(&after));
        match after {
            Ok(report) => {
                println!("\nNow: {}", ui::glyphs::text(&summary_sentence(&report)));
                return ExitCode::from(report.result.verdict.exit_code());
            }
            Err(err) => {
                eprintln!("{err}");
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }
    ExitCode::from(report.result.verdict.exit_code())
}

/// Resolves the targets and estimates every model. Errors are plain-language messages.
fn prepare(
    targets: &[String],
    apps: &[String],
    options: EstimateOptions,
) -> Result<Request, String> {
    let mut app_names: Vec<String> = apps.to_vec();
    let mut models = Vec::new();
    for arg in targets {
        match target::resolve(arg) {
            Ok(Target::Model(parts)) => {
                let facts = model::load(&parts).map_err(|e| e.to_string())?;
                let estimate = model::estimate(&facts, options);
                models.push(ModelEntry { facts, estimate });
            }
            Ok(Target::App(name)) => app_names.push(name),
            // A folder without models that is named like a known app (`canfit UTM` in a folder
            // holding a `UTM` folder) is the app.
            Err(err @ TargetError::EmptyFolder(_)) => {
                let known = profiles::load().map_err(|e| e.to_string())?;
                let os = crate::estimate::apps::Os::current();
                let is_app = os.is_some_and(|os| {
                    matches!(
                        resolve::resolve(arg, &known, os, &[]),
                        Resolution::Profile { .. }
                    )
                });
                if !is_app {
                    return Err(err.to_string());
                }
                app_names.push(arg.clone());
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    let sources = if app_names.is_empty() {
        Sources::default()
    } else {
        Sources::load()?
    };
    Ok(Request {
        models,
        apps: app_names,
        sources,
        margin_percent: options.margin_percent,
    })
}

pub fn build(models: Vec<ModelEntry>, snapshot: &Snapshot) -> FitReport {
    let mut total = Totals::new();
    for m in &models {
        total.add(&m.estimate);
    }
    let result = fit::evaluate(total.base_bytes, total.total_bytes, snapshot);
    FitReport {
        models,
        apps: Vec::new(),
        gpu_part: Some((total.base_bytes, total.total_bytes)),
        total,
        result,
        snapshot_memory_total: snapshot.memory.total_bytes,
    }
}

fn subject(report: &FitReport) -> String {
    report
        .models
        .iter()
        .map(|m| m.facts.name.as_str())
        .chain(report.apps.iter().map(|a| a.check.name.as_str()))
        .collect::<Vec<_>>()
        .join(" + ")
}

/// The one-sentence answer that opens the output.
pub fn summary_sentence(report: &FitReport) -> String {
    if report.is_combined() {
        return super::combined::summary_sentence(report);
    }
    if !report.apps.is_empty() {
        return app::summary_sentence(report);
    }
    let name = subject(report);
    let plural = report.models.len() > 1;
    let (fits, doesnt, it) = if plural {
        ("fit together", "don't fit together", "they need")
    } else {
        ("fits", "doesn't fit", "it needs")
    };
    let needed = format_bytes(report.total.total_bytes);
    let check = report.result.deciding();
    let limit = format_bytes(check.limit_bytes);
    let symbol = report.result.verdict.symbol();
    match (report.result.verdict, check.limit, check.tight_cause) {
        (Verdict::Fits, ..) => format!(
            "{symbol} Yes, {name} {fits}: {it} about {needed} and {limit} is available now."
        ),
        (Verdict::Tight, LimitKind::AvailableMemory, Some(TightCause::PressureCritical)) => {
            format!(
                "{symbol} {name} {fits} on paper ({needed} of {limit} available), but memory pressure \
             is already critical, so expect slowdowns."
            )
        }
        (Verdict::Tight, LimitKind::AvailableMemory, _) => format!(
            "{symbol} {name} {fits}, but only just: {it} about {needed} with the safety margin \
             and {limit} is available, so the system may compress or swap memory."
        ),
        (Verdict::Tight, LimitKind::GpuMemoryLimit, _) => format!(
            "{symbol} {name} {fits}, but only just within the memory the GPU can use: {it} \
             about {needed} with the safety margin and the GPU limit is {limit}."
        ),
        (Verdict::DoesntFit, LimitKind::AvailableMemory, _) => format!(
            "{symbol} No, {name} {doesnt} right now: {it} about {needed} and only {limit} is \
             available."
        ),
        (Verdict::DoesntFit, LimitKind::GpuMemoryLimit, _) => format!(
            "{symbol} No, {name} {doesnt} in the memory the GPU can use: {it} about {needed} \
             and the GPU limit is {limit}."
        ),
    }
}

fn row(label: impl Into<String>, size: u64, text: impl Into<String>) -> Row {
    Row::new(label, format_bytes(size), text)
}

fn describe_model(m: &ModelFacts) -> String {
    let mut parts = vec![
        m.quantization.clone(),
        format!("{} params", format_parameters(m.parameter_count)),
    ];
    if let Some(layers) = m.block_count {
        parts.push(format!("{layers} layers"));
    }
    if let (Some(experts), Some(used)) = (m.expert_count, m.expert_used_count) {
        parts.push(format!("MoE {used} of {experts} experts"));
    }
    parts.join(" · ")
}

fn check_status(c: &LimitCheck) -> String {
    let symbol = c.verdict.symbol();
    let headroom = format_bytes(c.headroom_bytes.unsigned_abs());
    match (c.verdict, c.tight_cause) {
        (Verdict::Fits, _) => format!("{symbol} {headroom} to spare"),
        (Verdict::Tight, Some(TightCause::PressureCritical)) => {
            format!("{symbol} pressure is critical")
        }
        (Verdict::Tight, _) => format!("{symbol} only without the margin"),
        (Verdict::DoesntFit, _) => format!("{symbol} {headroom} short"),
    }
}

fn kv_text(e: &Estimate) -> String {
    format!(
        "estimated · {} tokens, {} cache",
        group_digits(e.context_tokens),
        e.cache_type.label()
    )
}

/// The full text output: answer, card and notes, plus what to close when `make_room` is set.
/// With `theme`, the answer and the cards are printed in colour with the monitor's card widget.
pub fn render_text(
    report: &FitReport,
    make_room: Option<&MakeRoom>,
    theme: Option<ui::theme::Theme>,
) -> String {
    if report.is_combined() {
        return super::combined::render_text(report, make_room, theme);
    }
    if !report.apps.is_empty() {
        return render_app_text(report, make_room, theme);
    }
    let mut rows: Vec<Option<Row>> = Vec::new();
    let single = report.models.len() == 1;
    for m in &report.models {
        rows.push(Some(Row {
            label: m.facts.name.clone(),
            size: if single {
                String::new()
            } else {
                format_bytes(m.estimate.total_bytes)
            },
            text: describe_model(&m.facts),
        }));
        if !single {
            rows.push(Some(Row {
                label: String::new(),
                size: String::new(),
                text: format!(
                    "estimated · {} tokens, {} cache",
                    group_digits(m.estimate.context_tokens),
                    m.estimate.cache_type.label()
                ),
            }));
        }
    }
    let t = &report.total;
    rows.push(Some(row(
        "Weights",
        t.weights_bytes,
        "read from GGUF (exact)",
    )));
    rows.push(Some(row(
        "KV cache",
        t.kv_cache_bytes,
        if single {
            kv_text(&report.models[0].estimate)
        } else {
            "estimated".to_string()
        },
    )));
    rows.push(Some(row(
        "Overhead",
        t.overhead_bytes,
        "estimated · compute buffers, runtime",
    )));
    let margin = report
        .models
        .first()
        .map_or(0, |m| m.estimate.margin_percent);
    rows.push(Some(row(
        "Margin",
        t.margin_bytes,
        format!("estimated · {margin}% safety margin"),
    )));
    rows.push(Some(row("Needed", t.total_bytes, "estimated")));
    rows.push(None);
    for c in &report.result.checks {
        let label = match c.limit {
            LimitKind::AvailableMemory => "Available now",
            LimitKind::GpuMemoryLimit => "GPU limit",
        };
        rows.push(Some(row(
            label,
            c.limit_bytes,
            format!("{} · {}", c.provenance.label(), check_status(c)),
        )));
    }

    let mut out = String::new();
    let _ = writeln!(out, "{}", card::sentence(&summary_sentence(report), theme));
    if let Some(m) = make_room {
        let _ = writeln!(out, "{}", advice::make_room_sentence(m, report));
    }
    out.push('\n');
    out.push_str(&card::render_styled(
        &format!("Can {} fit?", subject(report)),
        &rows,
        theme,
    ));
    if let Some(m) = make_room {
        out.push_str(&advice::render_make_room(m, theme));
    }

    for note in notes(report) {
        let _ = writeln!(out, "  {note}");
    }
    if let Some(m) = make_room {
        out.push_str(&advice::make_room_notes(m));
    }
    out
}

/// The app variant of [`render_text`].
fn render_app_text(
    report: &FitReport,
    make_room: Option<&MakeRoom>,
    theme: Option<ui::theme::Theme>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", card::sentence(&summary_sentence(report), theme));
    out.push_str(&app::follow_up_text(report));
    if let Some(m) = make_room {
        let _ = writeln!(out, "{}", advice::make_room_sentence(m, report));
    }
    out.push('\n');
    out.push_str(&card::render_styled(
        &app::title(report),
        &app::rows(report),
        theme,
    ));
    if let Some(m) = make_room {
        out.push_str(&advice::render_make_room(m, theme));
    }
    for note in app::notes(report) {
        out.push_str(&card::wrap(&note, 100, 2));
    }
    if let Some(m) = make_room {
        out.push_str(&advice::make_room_notes(m));
    }
    out
}

/// Plain-language assumptions and caveats printed under the card.
fn notes(report: &FitReport) -> Vec<String> {
    let mut notes = Vec::new();
    for m in &report.models {
        let e = &m.estimate;
        let prefix = if report.models.len() > 1 {
            format!("{}: ", m.facts.name)
        } else {
            String::new()
        };
        match e.context_source {
            ContextSource::Default => {
                let trained = m
                    .facts
                    .context_length
                    .map(|c| format!("; the model supports {}", group_digits(c)))
                    .unwrap_or_default();
                notes.push(format!(
                    "{prefix}context {} tokens is canfit's default{trained}. Change it with --ctx.",
                    group_digits(e.context_tokens)
                ));
            }
            ContextSource::Model => notes.push(format!(
                "{prefix}context {} tokens, the model's full context.",
                group_digits(e.context_tokens)
            )),
            ContextSource::Flag => {}
        }
        if let (Some(experts), Some(used)) = (m.facts.expert_count, m.facts.expert_used_count) {
            notes.push(format!(
                "{prefix}mixture of experts: all {experts} experts stay in memory, though {used} \
                 are active per token."
            ));
        }
        for note in &e.notes {
            notes.push(format!("{prefix}{note}"));
        }
    }
    if report
        .result
        .checks
        .iter()
        .any(|c| c.limit == LimitKind::GpuMemoryLimit)
    {
        notes.push(
            "Assumes llama.cpp with every layer on the GPU, the default on Apple Silicon."
                .to_string(),
        );
    }
    // Notes without a model prefix start a sentence.
    for note in &mut notes {
        if let Some(first) = note.chars().next().filter(char::is_ascii_lowercase) {
            note.replace_range(..1, &first.to_ascii_uppercase().to_string());
        }
    }
    notes.push(
        "Available memory includes file cache the system can reclaim. Needed memory is an \
         estimate: real use varies with the runtime and its settings."
            .to_string(),
    );
    if report.result.pressure_level != PressureLevel::Calm {
        notes.push(format!(
            "Memory pressure is {} right now.",
            report.result.pressure_level.label()
        ));
    }
    notes
}

#[derive(Serialize)]
struct FitDocument<'a> {
    schema: &'static str,
    schema_version: u32,
    canfit_version: &'static str,
    summary: String,
    /// `model` (models only), `app` (one app) or `combined` (several targets with at least one
    /// app). Additive in schema 1 (ADR-020, ADR-021).
    target_kind: &'static str,
    verdict: Verdict,
    /// The verdict in words: for apps "fits comfortably", "fits but will slow down", "at risk".
    verdict_label: &'static str,
    exit_code: u8,
    deciding_limit: LimitKind,
    needed: &'a Totals,
    checks: &'a [LimitCheck],
    pressure_level: PressureLevel,
    memory_total_bytes: u64,
    models: &'a [ModelEntry],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    apps: &'a [AppEntry],
    #[serde(skip_serializing_if = "Option::is_none")]
    make_room: Option<&'a MakeRoom>,
}

pub fn to_json(report: &FitReport, make_room: Option<&MakeRoom>) -> String {
    let document = FitDocument {
        schema: FIT_SCHEMA,
        schema_version: FIT_SCHEMA_VERSION,
        canfit_version: env!("CARGO_PKG_VERSION"),
        summary: summary_sentence(report),
        target_kind: if report.apps.is_empty() {
            "model"
        } else if report.is_combined() {
            "combined"
        } else {
            "app"
        },
        verdict: report.result.verdict,
        verdict_label: if !report.models.is_empty() || report.apps.is_empty() {
            match report.result.verdict {
                Verdict::Fits => "fits",
                Verdict::Tight => "tight",
                Verdict::DoesntFit => "doesn't fit",
            }
        } else {
            app::verdict_label(report.result.verdict)
        },
        exit_code: report.result.verdict.exit_code(),
        deciding_limit: report.result.deciding_limit,
        needed: &report.total,
        checks: &report.result.checks,
        pressure_level: report.result.pressure_level,
        memory_total_bytes: report.snapshot_memory_total,
        models: &report.models,
        apps: &report.apps,
        make_room,
    };
    serde_json::to_string_pretty(&document).expect("fit report serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::GpuMemoryLimit;
    use crate::core::sampler::tests::replay;
    use crate::estimate::gguf::builder::Gguf;

    const GB: u64 = 1024 * 1024 * 1024;

    fn entry(name: &'static str) -> ModelEntry {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "canfit-fit-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.gguf");
        Gguf::llama(name, 2).write(&path);
        let facts = model::load(&[path]).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        // A fixed profile keeps the numbers identical on every OS.
        let options = EstimateOptions {
            overhead: model::OverheadProfile::Uncalibrated,
            ..EstimateOptions::default()
        };
        let estimate = model::estimate(&facts, options);
        ModelEntry { facts, estimate }
    }

    fn snapshot(available: u64, gpu: Option<u64>) -> Snapshot {
        let (_, mut s) = replay("calm");
        s.memory.available_bytes = available;
        s.gpu_memory_limit = gpu.map(|bytes| GpuMemoryLimit {
            bytes,
            provenance: Provenance::Estimated,
            source: "test".to_string(),
        });
        s
    }

    #[test]
    fn fitting_model_answers_first_and_labels_every_value() {
        let report = build(vec![entry("tiny")], &snapshot(8 * GB, Some(10 * GB)));
        assert_eq!(report.result.verdict, Verdict::Fits);
        let text = render_text(&report, None, None);
        assert!(
            text.starts_with("● Yes, tiny fits: it needs about 1.1 GB"),
            "{text}"
        );
        assert!(text.contains("╭─ Can tiny fit? "));
        assert!(text.contains("read from GGUF"));
        assert!(text.contains("estimated · 4,096 tokens, f16 cache"));
        assert!(text.contains("Available now"));
        assert!(text.contains("GPU limit"));
        assert!(text.contains("  Context 4,096 tokens, the model's full context."));
        // The box is rectangular.
        let widths: Vec<usize> = text
            .lines()
            .filter(|l| l.starts_with('│') || l.starts_with('╭') || l.starts_with('╰'))
            .map(|l| l.chars().count())
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{text}");
    }

    #[test]
    fn several_models_are_summed() {
        let one = entry("a");
        let report = build(vec![one.clone(), entry("b")], &snapshot(GB, None));
        assert_eq!(report.total.total_bytes, 2 * one.estimate.total_bytes);
        assert_eq!(report.result.verdict, Verdict::DoesntFit);
        let sentence = summary_sentence(&report);
        assert!(
            sentence.starts_with("✗ No, a + b don't fit together right now"),
            "{sentence}"
        );
    }

    #[test]
    fn gpu_limit_failure_is_named() {
        let report = build(vec![entry("tiny")], &snapshot(8 * GB, Some(GB)));
        assert_eq!(report.result.deciding_limit, LimitKind::GpuMemoryLimit);
        assert!(summary_sentence(&report).contains("in the memory the GPU can use"));
    }

    #[test]
    fn json_document_has_schema_and_exit_code() {
        let report = build(vec![entry("tiny")], &snapshot(8 * GB, Some(10 * GB)));
        let v: serde_json::Value = serde_json::from_str(&to_json(&report, None)).unwrap();
        assert_eq!(v["schema"], FIT_SCHEMA);
        assert_eq!(v["schema_version"], FIT_SCHEMA_VERSION);
        assert_eq!(v["verdict"], "fits");
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["deciding_limit"], "available_memory");
        assert_eq!(v["needed"]["provenance"], "estimated");
        assert_eq!(v["checks"][1]["limit"], "gpu_memory_limit");
        assert_eq!(v["models"][0]["name"], "tiny");
        assert_eq!(v["models"][0]["quantization"], "Q4_K_M");
        assert_eq!(v["models"][0]["estimate"]["context_source"], "model");
        assert_eq!(v["models"][0]["estimate"]["cache_type"], "f16");
        assert!(v["models"][0]["parts"].is_array());
    }

    fn docker(s: &Snapshot) -> crate::estimate::apps::check::AppCheck {
        use crate::estimate::apps::Os;
        use crate::estimate::apps::check;
        use crate::estimate::apps::readers::tests::env;
        let profiles = profiles::embedded();
        let docker = profiles
            .iter()
            .find(|p| p.name == "Docker Desktop")
            .unwrap();
        check::check(docker, None, &env(Os::Macos, "macos"), s).unwrap()
    }

    #[test]
    fn a_model_and_an_app_are_summed_with_the_gpu_limit_on_the_model_only() {
        let model = entry("tiny");
        let m_base = model.estimate.base_bytes;
        let m_total = model.estimate.total_bytes;
        // Docker's 10 GiB VM + the model: more than the 8 GB GPU limit, but the model alone fits.
        let s = snapshot(16 * GB, Some(8 * GB));
        let report = assemble(vec![model], vec![docker(&s)], 10, &s);
        assert!(report.is_combined());
        assert_eq!(report.total.apps_bytes, 10 * GB);
        assert_eq!(report.total.base_bytes, m_base + 10 * GB);
        assert_eq!(report.total.total_bytes, m_total + 11 * GB);
        assert_eq!(report.gpu_part, Some((m_base, m_total)));
        assert_eq!(report.result.verdict, Verdict::Fits);
        assert_eq!(report.result.checks.len(), 2);
        let text = render_text(&report, None, None);
        assert!(
            text.starts_with("● Yes, tiny + Docker Desktop fit together: they need about"),
            "{text}"
        );
        assert!(
            text.contains("╭─ Can tiny + Docker Desktop fit? "),
            "{text}"
        );
        assert!(
            text.contains("read from config · app · confidence high"),
            "{text}"
        );
        assert!(text.contains("estimated · model · Q4_K_M"), "{text}");
        assert!(text.contains("models only"), "{text}");
        assert!(
            text.contains("Docker Desktop: Read from Docker Desktop settings"),
            "{text}"
        );
        let v: serde_json::Value = serde_json::from_str(&to_json(&report, None)).unwrap();
        assert_eq!(v["target_kind"], "combined");
        assert_eq!(v["verdict_label"], "fits");
        assert_eq!(v["models"][0]["name"], "tiny");
        assert_eq!(v["apps"][0]["source"], "config");
        assert_eq!(v["needed"]["apps_bytes"], 10 * GB);
        assert_eq!(v["exit_code"], 0);
        // Short on memory: one verdict for everything, exit 2.
        let s = snapshot(6 * GB, Some(8 * GB));
        let report = assemble(vec![entry("tiny")], vec![docker(&s)], 10, &s);
        assert_eq!(report.result.verdict, Verdict::DoesntFit);
        assert_eq!(report.result.verdict.exit_code(), 2);
        assert!(
            summary_sentence(&report).starts_with("✗ No, tiny + Docker Desktop don't fit together"),
            "{}",
            summary_sentence(&report)
        );
        // A GPU limit below the model alone decides, and says apps don't count.
        let s = snapshot(64 * GB, Some(GB / 2));
        let report = assemble(vec![entry("tiny")], vec![docker(&s)], 10, &s);
        assert_eq!(report.result.deciding_limit, LimitKind::GpuMemoryLimit);
        assert!(
            summary_sentence(&report).contains("Apps don't count against that limit"),
            "{}",
            summary_sentence(&report)
        );
    }

    #[test]
    fn several_apps_without_models_have_no_gpu_check() {
        let s = snapshot(32 * GB, Some(8 * GB));
        let report = assemble(Vec::new(), vec![docker(&s), docker(&s)], 10, &s);
        assert!(report.is_combined());
        assert_eq!(report.gpu_part, None);
        assert_eq!(report.result.checks.len(), 1);
        assert_eq!(report.total.total_bytes, 22 * GB);
        let v: serde_json::Value = serde_json::from_str(&to_json(&report, None)).unwrap();
        assert_eq!(v["verdict_label"], "fits comfortably");
        // One app alone keeps its own card.
        let one = assemble(Vec::new(), vec![docker(&s)], 10, &s);
        assert!(!one.is_combined());
        assert!(render_text(&one, None, None).contains("╭─ Can Docker Desktop fit? "));
    }

    #[test]
    fn app_reports_render_answer_card_and_json() {
        use crate::estimate::apps::Os;
        use crate::estimate::apps::check;
        use crate::estimate::apps::readers::tests::env;
        let profiles = profiles::embedded();
        let docker = profiles
            .iter()
            .find(|p| p.name == "Docker Desktop")
            .unwrap();
        let mut s = snapshot(16 * GB, Some(12 * GB));
        s.platform.os = "macos".to_string();
        let c = check::check(docker, None, &env(Os::Macos, "macos"), &s).unwrap();
        let report = app::build(c, 10, &s);
        assert_eq!(report.total.total_bytes, 11 * GB);
        assert_eq!(report.result.verdict, Verdict::Fits);
        assert_eq!(report.result.checks.len(), 1, "no GPU limit for apps");
        let text = render_text(&report, None, None);
        assert!(
            text.starts_with(
                "● Yes, Docker Desktop fits comfortably: it is set to use up to 11.0 GB"
            ),
            "{text}"
        );
        assert!(text.contains("╭─ Can Docker Desktop fit? "), "{text}");
        assert!(
            text.contains("read from config · memoryMiB = 10240 in settings.json"),
            "{text}"
        );
        assert!(text.contains("confidence high"), "{text}");
        let v: serde_json::Value = serde_json::from_str(&to_json(&report, None)).unwrap();
        assert_eq!(v["schema"], FIT_SCHEMA);
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["target_kind"], "app");
        assert_eq!(v["verdict_label"], "fits comfortably");
        assert_eq!(v["apps"][0]["source"], "config");
        assert_eq!(v["apps"][0]["confidence"], "high");
        assert_eq!(
            v["apps"][0]["reading"]["items"][0]["declared_provenance"],
            "read_from_config"
        );
        assert_eq!(v["apps"][0]["need_bytes"], 10 * GB);
        assert!(v["models"].as_array().unwrap().is_empty());
        // Short on memory: at risk, exit 2.
        let s = snapshot(4 * GB, None);
        let c = check::check(docker, None, &env(Os::Macos, "macos"), &s).unwrap();
        let report = app::build(c, 10, &s);
        assert_eq!(report.result.verdict.exit_code(), 2);
        assert!(summary_sentence(&report).starts_with("✗ No, Docker Desktop is at risk right now"));
        // Between the estimate and the margin: fits but will slow down.
        let s = snapshot(10 * GB + GB / 2, None);
        let c = check::check(docker, None, &env(Os::Macos, "macos"), &s).unwrap();
        let report = app::build(c, 10, &s);
        assert_eq!(report.result.verdict, Verdict::Tight);
        assert!(summary_sentence(&report).contains("fits but will slow down"));
    }

    #[test]
    fn vm_apps_list_each_vm_with_its_verdict() {
        use crate::estimate::apps::Os;
        use crate::estimate::apps::check;
        use crate::estimate::apps::readers::tests::env;
        let profiles = profiles::embedded();
        let utm = profiles.iter().find(|p| p.name == "UTM").unwrap();
        let s = snapshot(6 * GB, None);
        let c = check::check(utm, None, &env(Os::Macos, "macos"), &s).unwrap();
        let report = app::build(c, 10, &s);
        assert_eq!(report.result.verdict, Verdict::DoesntFit);
        let text = render_text(&report, None, None);
        assert!(
            text.starts_with("✗ No, UTM's largest VM (Ubuntu 24.04) is at risk right now"),
            "{text}"
        );
        assert!(
            text.contains("Of UTM's 3 virtual machines, Kali Linux and Metasploitable 2 fit comfortably; Ubuntu 24.04 is at risk."),
            "{text}"
        );
        assert!(text.contains("● fits comfortably"), "{text}");
        let v: serde_json::Value = serde_json::from_str(&to_json(&report, None)).unwrap();
        assert_eq!(v["apps"][0]["vm_verdicts"][2]["verdict"], "doesnt_fit");
        assert_eq!(v["apps"][0]["reading"]["kind"], "virtual_machines");
    }
}
