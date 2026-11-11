//! Temporary one-shot output of `canfit` until the live TUI arrives (milestone 05): a summary
//! sentence, a few supporting lines and the top applications. Also the `--json` document.

use std::fmt::Write as _;
use std::thread;
use std::time::Duration;

use serde::Serialize;

use crate::core::pressure::growth_rate;
use crate::core::sampler::Sampler;
use crate::core::{
    MemoryKind, OsPressureLevel, PressureLevel, PressureSignal, Provenance, Snapshot, format_bytes,
    format_rate,
};
use crate::platform::{self, PlatformError};

/// Identifies the JSON document type.
pub const SNAPSHOT_SCHEMA: &str = "canfit.snapshot";
/// Bumped on any breaking change to the JSON layout (documented in `docs/open-source/usage.md`).
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Time between the light first sample and the full one. Long enough for a CPU reading
/// (`sysinfo` needs 200 ms) and a first swap trend, short enough to stay well under one second.
pub const ONE_SHOT_WINDOW: Duration = Duration::from_millis(250);

/// How many applications the text summary lists.
const TOP_APPS: usize = 8;

/// Takes one explained snapshot: a light sample, a short wait, then a second sample, with the
/// per-process scan when `with_processes` is set.
pub fn take_snapshot(with_processes: bool) -> Result<Snapshot, PlatformError> {
    let mut source = platform::native()?;
    let mut sampler = Sampler::new(4, Duration::from_secs(10));
    sampler.record(source.sample(false)?);
    thread::sleep(ONE_SHOT_WINDOW);
    Ok(sampler.record(source.sample(with_processes)?))
}

#[derive(Serialize)]
struct SnapshotDocument<'a> {
    schema: &'static str,
    schema_version: u32,
    canfit_version: &'static str,
    #[serde(flatten)]
    snapshot: &'a Snapshot,
}

pub fn to_json(snapshot: &Snapshot) -> String {
    let document = SnapshotDocument {
        schema: SNAPSHOT_SCHEMA,
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        canfit_version: env!("CARGO_PKG_VERSION"),
        snapshot,
    };
    serde_json::to_string_pretty(&document).expect("snapshot serializes")
}

/// The one-sentence answer that opens the output.
pub fn summary_sentence(snapshot: &Snapshot) -> String {
    let pressure = &snapshot.pressure;
    let reason = &pressure.main_reason.message;
    let mut sentence = match (pressure.level, pressure.main_reason.signal) {
        (PressureLevel::Calm, PressureSignal::None) => format!("All calm: {reason}."),
        (PressureLevel::Calm, _) => format!("Mostly calm, but {reason}."),
        (PressureLevel::Strained, _) => format!("Memory under pressure: {reason}."),
        (PressureLevel::Critical, _) => format!("Memory is critical: {reason}."),
    };
    if pressure.level != PressureLevel::Calm
        && let Some(top) = snapshot.app_groups.first().filter(|g| g.memory_bytes > 0)
    {
        let _ = write!(
            sentence,
            " {} is using {}.",
            top.name,
            format_bytes(top.memory_bytes)
        );
    }
    sentence
}

/// The full text summary.
pub fn render_text(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}\n", summary_sentence(snapshot));

    let m = &snapshot.memory;
    let mut detail = Vec::new();
    for (label, value) in [
        ("apps", m.app_bytes),
        ("wired", m.wired_bytes),
        ("compressed", m.compressed_bytes),
        ("cached files", m.cached_files_bytes),
    ] {
        if let Some(bytes) = value {
            detail.push(format!("{label} {}", format_bytes(bytes)));
        }
    }
    row(
        &mut out,
        "Memory",
        format!(
            "{} used of {}, {} available",
            format_bytes(m.used_bytes),
            format_bytes(m.total_bytes),
            format_bytes(m.available_bytes)
        ),
        m.provenance,
    );
    if !detail.is_empty() {
        let _ = writeln!(out, "  {:<10}{}", "", detail.join(" · "));
    }
    if let (Some(commit), Some(limit)) = (m.commit_bytes, m.commit_limit_bytes) {
        row(
            &mut out,
            "Committed",
            format!(
                "{} of a {} limit (RAM plus page file)",
                format_bytes(commit),
                format_bytes(limit)
            ),
            m.provenance,
        );
    }

    let s = &snapshot.swap;
    let trend = match &snapshot.swap_trend {
        None => "trend not measured yet".to_string(),
        Some(t) if t.growing => format!("growing at {}", format_rate(growth_rate(t))),
        Some(_) if s.used_bytes > 0 => "not growing (old swap is harmless)".to_string(),
        Some(_) => "not growing".to_string(),
    };
    let swap_kind = if snapshot.platform.os == "windows" {
        " page file"
    } else {
        ""
    };
    row(
        &mut out,
        "Swap",
        format!(
            "{} used of {}{swap_kind}, {trend}",
            format_bytes(s.used_bytes),
            format_bytes(s.total_bytes)
        ),
        s.provenance,
    );

    let c = &snapshot.cpu;
    let usage = c
        .usage_percent
        .map_or("usage not measured".to_string(), |u| {
            format!("{u:.0}% busy")
        });
    let mut cpu = format!("{usage} across {} cores", c.logical_cores);
    if let Some([one, _, _]) = c.load_average {
        let _ = write!(cpu, ", load {one:.2}");
    }
    row(&mut out, "CPU", cpu, c.provenance);

    let p = &snapshot.pressure;
    let mut pressure = format!("{} {}, {}/100", p.level.symbol(), p.level.label(), p.score);
    if let Some(level) = snapshot.os_pressure {
        let word = match level {
            OsPressureLevel::Normal => "normal",
            OsPressureLevel::Warning => "warning",
            OsPressureLevel::Critical => "critical",
        };
        let _ = write!(pressure, " (OS level: {word})");
    }
    row(&mut out, "Pressure", pressure, p.provenance);

    if let Some(stall) = &snapshot.stall {
        let parts: Vec<String> = [
            ("memory", stall.memory),
            ("CPU", stall.cpu),
            ("I/O", stall.io),
        ]
        .into_iter()
        .filter_map(|(label, r)| r.map(|r| format!("{label} {:.0}%", r.some.avg10)))
        .collect();
        if !parts.is_empty() {
            row(
                &mut out,
                "Waiting",
                format!(
                    "{} of the last 10 s (programs stalled, PSI)",
                    parts.join(" · ")
                ),
                stall.provenance,
            );
        }
    }

    if let Some(gpu) = &snapshot.gpu_memory_limit {
        row(
            &mut out,
            "GPU",
            format!(
                "up to {} of memory usable by the GPU ({})",
                format_bytes(gpu.bytes),
                gpu.source
            ),
            gpu.provenance,
        );
    }

    let groups: Vec<_> = snapshot
        .app_groups
        .iter()
        .filter(|g| g.memory_bytes > 0)
        .take(TOP_APPS)
        .collect();
    if !groups.is_empty() {
        let _ = writeln!(out, "\n  Top apps by memory");
        let width = groups
            .iter()
            .map(|g| g.name.chars().count())
            .max()
            .unwrap_or(0);
        let width = width.min(40);
        for g in groups {
            let name: String = g.name.chars().take(width).collect();
            let processes = match g.process_count {
                1 => "1 process".to_string(),
                n => format!("{n} processes"),
            };
            let fallback = if g.memory_kinds.contains(&MemoryKind::Rss) {
                "  (partly resident size)"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "  {name:<width$}  {:>8}  {processes}{fallback}",
                format_bytes(g.memory_bytes)
            );
        }
    }

    let unreadable = snapshot
        .processes
        .iter()
        .filter(|p| p.memory.is_none())
        .count();
    let _ = writeln!(
        out,
        "\n  App memory is each process's {}, measured.",
        snapshot.process_memory_kind().label()
    );
    if unreadable > 0 {
        let _ = writeln!(
            out,
            "  {unreadable} of {} processes belong to other users and can't be read without admin rights.",
            snapshot.processes.len()
        );
    }
    out
}

fn row(out: &mut String, label: &str, text: String, provenance: Provenance) {
    let _ = writeln!(out, "  {label:<10}{text}  [{}]", provenance.label());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sampler::tests::replay;
    use crate::core::{AppGroup, GroupKind};

    fn chrome(bytes: u64) -> AppGroup {
        AppGroup {
            name: "Google Chrome".to_string(),
            kind: GroupKind::AppBundle,
            bundle_path: Some("/Applications/Google Chrome.app".to_string()),
            memory_bytes: bytes,
            memory_kinds: vec![MemoryKind::Footprint],
            process_count: 28,
            unreadable_count: 0,
            pids: vec![1],
        }
    }

    #[test]
    fn calm_output_starts_with_an_answer() {
        let (_, snapshot) = replay("calm");
        let text = render_text(&snapshot);
        assert!(
            text.starts_with("All calm: 4.2 GB of 16.0 GB memory is available."),
            "{text}"
        );
        assert!(text.contains("not growing (old swap is harmless)"));
        assert!(text.contains("[estimated]"));
        assert!(text.contains("[measured]"));
    }

    #[test]
    fn pressure_names_the_biggest_app() {
        let (_, mut snapshot) = replay("critical");
        snapshot.app_groups = vec![chrome(9 * 1024 * 1024 * 1024)];
        let sentence = summary_sentence(&snapshot);
        assert!(
            sentence.starts_with("Memory is critical: macOS reports memory pressure as critical")
        );
        assert!(
            sentence.ends_with("Google Chrome is using 9.0 GB."),
            "{sentence}"
        );
        assert!(render_text(&snapshot).contains("Google Chrome"));
    }

    #[test]
    fn json_document_has_a_schema_version() {
        let (_, snapshot) = replay("swapping");
        let value: serde_json::Value = serde_json::from_str(&to_json(&snapshot)).unwrap();
        assert_eq!(value["schema"], SNAPSHOT_SCHEMA);
        assert_eq!(value["schema_version"], SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(value["pressure"]["level"], "strained");
        assert_eq!(value["pressure"]["main_reason"]["signal"], "swap_growing");
        assert_eq!(value["memory"]["provenance"], "measured");
        assert_eq!(value["gpu_memory_limit"]["provenance"], "estimated");
        assert_eq!(value["swap_trend"]["growing"], true);
        assert!(value["app_groups"].is_array());
        assert!(value["stall"].is_null());
    }

    #[test]
    fn linux_output_shows_stalls_and_critical_reason() {
        let snapshot = crate::platform::linux_model::tests::snapshot("high_pressure");
        let text = render_text(&snapshot);
        assert!(
            text.starts_with("Memory is critical: every program was stalled waiting for memory"),
            "{text}"
        );
        assert!(
            text.contains("Waiting   memory 64% · CPU 20% · I/O 70% of the last 10 s"),
            "{text}"
        );
        assert!(text.contains("llama-server is using"), "{text}");
        let value: serde_json::Value = serde_json::from_str(&to_json(&snapshot)).unwrap();
        assert_eq!(value["platform"]["os"], "linux");
        assert_eq!(value["stall"]["memory"]["full"]["avg10"], 30.0);
        assert_eq!(value["stall"]["source"], "/proc/pressure");
        assert_eq!(value["pressure"]["main_reason"]["signal"], "memory_stall");
    }
}
