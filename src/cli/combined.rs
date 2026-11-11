//! Several targets together (spec §5, §11; ADR-021): any mix of models and apps, summed. The card
//! lists each item with its source and confidence, then the margin, the total and the limits;
//! the Apple Silicon GPU limit applies to the models' part only.

use std::fmt::Write as _;

use super::advice::{self, MakeRoom};
use super::app;
use super::card::{self, Row};
use super::fit::FitReport;
use crate::core::{PressureLevel, format_bytes};
use crate::estimate::apps::ReadingKind;
use crate::estimate::fit::{LimitCheck, LimitKind, TightCause, Verdict};
use crate::estimate::model::format_parameters;
use crate::ui;

/// Every target's name, in order: models first, then apps.
pub fn names(report: &FitReport) -> Vec<String> {
    report
        .models
        .iter()
        .map(|m| m.facts.name.clone())
        .chain(report.apps.iter().map(|a| a.check.name.clone()))
        .collect()
}

fn models_subject(report: &FitReport) -> String {
    report
        .models
        .iter()
        .map(|m| m.facts.name.as_str())
        .collect::<Vec<_>>()
        .join(" + ")
}

/// The one-sentence answer for several targets.
pub fn summary_sentence(report: &FitReport) -> String {
    let subject = names(report).join(" + ");
    let needed = format_bytes(report.total.total_bytes);
    let check = report.result.deciding();
    let limit = format_bytes(check.limit_bytes);
    let symbol = report.result.verdict.symbol();
    let model_needed = format_bytes(report.gpu_part.map_or(0, |(_, t)| t));
    let models = models_subject(report);
    let it = if report.models.len() > 1 {
        "they need"
    } else {
        "it needs"
    };
    match (report.result.verdict, check.limit, check.tight_cause) {
        (Verdict::Fits, ..) => format!(
            "{symbol} Yes, {subject} fit together: they need about {needed} with the safety \
             margin and {limit} is available now."
        ),
        (Verdict::Tight, LimitKind::AvailableMemory, Some(TightCause::PressureCritical)) => {
            format!(
                "{symbol} {subject} fit together on paper ({needed} of {limit} available), but \
                 memory pressure is already critical, so expect slowdowns."
            )
        }
        (Verdict::Tight, LimitKind::AvailableMemory, _) => format!(
            "{symbol} {subject} fit together, but only just: they need about {needed} with the \
             safety margin and {limit} is available, so the system will compress or swap memory \
             and things will slow down."
        ),
        (Verdict::Tight, LimitKind::GpuMemoryLimit, _) => format!(
            "{symbol} {subject} fit together, but {models} only just fits within the memory the \
             GPU can use: {it} about {model_needed} with the safety margin and the GPU limit is \
             {limit}."
        ),
        (Verdict::DoesntFit, LimitKind::AvailableMemory, _) => format!(
            "{symbol} No, {subject} don't fit together right now: they need about {needed} with \
             the safety margin and only {limit} is available."
        ),
        (Verdict::DoesntFit, LimitKind::GpuMemoryLimit, _) => format!(
            "{symbol} No, {models} doesn't fit in the memory the GPU can use: {it} about \
             {model_needed} with the safety margin and the GPU limit is {limit}. Apps don't count \
             against that limit."
        ),
    }
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

/// The card rows: one or two per item, then margin, total and limits.
pub fn rows(report: &FitReport) -> Vec<Option<Row>> {
    let mut rows = Vec::new();
    for m in &report.models {
        rows.push(Some(Row::new(
            m.facts.name.clone(),
            format_bytes(m.estimate.base_bytes),
            format!(
                "estimated · model · {}, {} params",
                m.facts.quantization,
                format_parameters(m.facts.parameter_count)
            ),
        )));
        rows.push(Some(Row::new(
            "",
            "",
            format!(
                "weights read from GGUF; {} tokens of {} cache",
                crate::estimate::model::group_digits(m.estimate.context_tokens),
                m.estimate.cache_type.label()
            ),
        )));
    }
    for a in &report.apps {
        let c = &a.check;
        let item = &c.reading.items[c.deciding_item];
        let what = match c.reading.kind {
            ReadingKind::VirtualMachines if c.vm.is_none() && c.reading.items.len() > 1 => {
                format!("largest VM ({})", item.name)
            }
            _ => item.detail.clone(),
        };
        rows.push(Some(Row::new(
            c.name.clone(),
            format_bytes(c.need_bytes),
            format!(
                "{} · app · confidence {}",
                app::value_label(c, item.declared_provenance),
                app::confidence_label(c.confidence)
            ),
        )));
        rows.push(Some(Row::new("", "", what)));
        if let Some(r) = c.running.as_ref().filter(|r| r.subtracted) {
            rows.push(Some(Row::new(
                "",
                "",
                format!(
                    "already running: {} in use (measured) of {}",
                    format_bytes(r.held_bytes),
                    format_bytes(c.declared_bytes)
                ),
            )));
        }
    }
    let margin = report
        .models
        .first()
        .map(|m| m.estimate.margin_percent)
        .or_else(|| report.apps.first().map(|a| a.margin_percent))
        .unwrap_or(0);
    rows.push(Some(Row::new(
        "Margin",
        format_bytes(report.total.margin_bytes),
        format!("estimated · {margin}% safety margin"),
    )));
    rows.push(Some(Row::new(
        "Needed",
        format_bytes(report.total.total_bytes),
        format!(
            "estimated · sum of {} items",
            report.models.len() + report.apps.len()
        ),
    )));
    rows.push(None);
    for c in &report.result.checks {
        let (label, part) = match c.limit {
            LimitKind::AvailableMemory => ("Available now", String::new()),
            LimitKind::GpuMemoryLimit => (
                "GPU limit",
                format!(
                    " · models only ({})",
                    format_bytes(report.gpu_part.map_or(0, |(_, t)| t))
                ),
            ),
        };
        rows.push(Some(Row::new(
            label,
            format_bytes(c.limit_bytes),
            format!("{} · {}{part}", c.provenance.label(), check_status(c)),
        )));
    }
    rows
}

/// Notes under the combined card: each app's source and caveats, then the general notes.
pub fn notes(report: &FitReport) -> Vec<String> {
    let mut notes = Vec::new();
    for m in &report.models {
        for note in &m.estimate.notes {
            notes.push(format!("{}: {note}", m.facts.name));
        }
    }
    for a in &report.apps {
        let c = &a.check;
        notes.push(format!("{}: {}", c.name, app::source_sentence(c)));
        for note in &c.notes {
            notes.push(format!("{}: {note}", c.name));
        }
    }
    if report
        .result
        .checks
        .iter()
        .any(|c| c.limit == LimitKind::GpuMemoryLimit)
    {
        notes.push(
            "The GPU limit applies to the models only (llama.cpp with every layer on the GPU, the \
             default on Apple Silicon); apps don't use GPU memory."
                .to_string(),
        );
    }
    if !report.models.is_empty() {
        notes
            .push("Model needs assume canfit's default context unless --ctx is given.".to_string());
    }
    notes.push(
        "Every item's need is added up. Available memory includes file cache the system can \
         reclaim."
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

/// The follow-up lines: apps already running.
fn follow_up(report: &FitReport) -> String {
    let mut out = String::new();
    for a in &report.apps {
        if let Some(r) = a.check.running.as_ref().filter(|r| r.subtracted) {
            let _ = writeln!(
                out,
                "{} is already running ({} in use), so only the rest is counted.",
                a.check.name,
                format_bytes(r.held_bytes)
            );
        }
    }
    out
}

/// The full text output for several targets.
pub fn render_text(
    report: &FitReport,
    make_room: Option<&MakeRoom>,
    theme: Option<ui::theme::Theme>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", card::sentence(&summary_sentence(report), theme));
    out.push_str(&follow_up(report));
    if let Some(m) = make_room {
        let _ = writeln!(out, "{}", advice::make_room_sentence(m, report));
    }
    out.push('\n');
    out.push_str(&card::render_styled(
        &format!("Can {} fit?", names(report).join(" + ")),
        &rows(report),
        theme,
    ));
    if let Some(m) = make_room {
        out.push_str(&advice::render_make_room(m, theme));
    }
    for note in notes(report) {
        out.push_str(&card::wrap(&note, 100, 2));
    }
    if let Some(m) = make_room {
        out.push_str(&advice::make_room_notes(m));
    }
    out
}
