//! `canfit models [DIR]`: every GGUF model in a folder (split models as one), with its size, the
//! estimated memory it needs and whether it fits right now. Same estimate and verdict as the fit
//! check (milestone 03, ADR-011, ADR-012); `--ctx`, `--cache-type`, `--margin` and `--json`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

use super::card::{self, Row};
use super::fit::ModelEntry;
use super::{EXIT_RUNTIME, report};
use crate::core::{PressureLevel, Snapshot, format_bytes};
use crate::estimate::fit::{self, FitResult, LimitCheck, LimitKind, TightCause, Verdict};
use crate::estimate::model::{self, EstimateOptions, group_digits};
use crate::estimate::target::{self, split_name};
use crate::ui::{self, theme::ThemeName};

/// Identifies the models JSON document.
pub const MODELS_SCHEMA: &str = "canfit.models";
/// Bumped on any breaking change to the models JSON layout.
pub const MODELS_SCHEMA_VERSION: u32 = 1;

/// GGUF architecture of multimodal projectors (`mmproj-*.gguf`): companions of a language model,
/// not models to launch on their own.
pub const PROJECTOR_ARCHITECTURE: &str = "clip";

/// How deep the scan goes below the folder (LM Studio keeps `publisher/model/file.gguf`).
pub const MAX_DEPTH: usize = 4;

/// One model found in the folder.
#[derive(Debug, Clone)]
pub struct Found {
    /// The file name without `.gguf` (split models say how many parts), or the path relative to
    /// the folder when two models share a file name.
    pub label: String,
    pub parts: Vec<PathBuf>,
}

/// A `.gguf` file canfit could not use, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    pub path: String,
    pub error: String,
}

/// What the scan of a folder found.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub found: Vec<Found>,
    pub problems: Vec<Problem>,
}

/// Lists the models under `dir`, at most [`MAX_DEPTH`] folders deep. Hidden folders are skipped
/// and folder links are not followed (file links are: Hugging Face's cache links to blobs).
pub fn scan(dir: &Path) -> Result<Scan, String> {
    let meta =
        fs::metadata(dir).map_err(|e| format!("canfit can't open {}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!(
            "{} is not a folder. To check one model, run canfit {}",
            dir.display(),
            dir.display()
        ));
    }
    let mut scan = Scan::default();
    let mut seen_splits: Vec<(PathBuf, String, u32)> = Vec::new();
    let mut pending = vec![(dir.to_path_buf(), 0usize)];
    while let Some((folder, depth)) = pending.pop() {
        let entries = match fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(e) if folder == dir => {
                return Err(format!("canfit can't read {}: {e}", dir.display()));
            }
            Err(e) => {
                scan.problems.push(Problem {
                    path: folder.to_string_lossy().into_owned(),
                    error: format!("canfit can't read this folder: {e}"),
                });
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < MAX_DEPTH && !name.starts_with('.') {
                    pending.push((path, depth + 1));
                }
                continue;
            }
            if !target::has_gguf_extension(&path) || !path.is_file() {
                continue;
            }
            if let Some(split) = split_name(&name) {
                let key = (folder.clone(), split.prefix.to_string(), split.count);
                if seen_splits.contains(&key) {
                    continue;
                }
                seen_splits.push(key);
            }
            let label = label_for(dir, &path, false);
            match target::model_parts(&path) {
                Ok(parts) => scan.found.push(Found { label, parts }),
                Err(e) => scan.problems.push(Problem {
                    path: path.to_string_lossy().into_owned(),
                    error: e.to_string(),
                }),
            }
        }
    }
    // Two files with one name (the same model in two folders): show their paths instead.
    let names: Vec<String> = scan.found.iter().map(|f| f.label.clone()).collect();
    for found in &mut scan.found {
        if names.iter().filter(|n| **n == found.label).count() > 1 {
            found.label = label_for(dir, &found.parts[0], true);
        }
    }
    scan.found.sort_by(|a, b| a.label.cmp(&b.label));
    scan.problems.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(scan)
}

fn label_for(dir: &Path, path: &Path, with_folders: bool) -> String {
    let relative = path.strip_prefix(dir).unwrap_or(path);
    let relative = if with_folders {
        relative
    } else {
        Path::new(relative.file_name().unwrap_or(relative.as_os_str()))
    };
    let text = relative.to_string_lossy().replace('\\', "/");
    let file = relative
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match split_name(&file) {
        Some(split) => {
            let parent = text.strip_suffix(file.as_str()).unwrap_or("");
            format!("{parent}{} ({} parts)", split.prefix, split.count)
        }
        None => text
            .strip_suffix(".gguf")
            .or_else(|| text.strip_suffix(".GGUF"))
            .unwrap_or(&text)
            .to_string(),
    }
}

/// One model with its estimate and its verdict right now.
#[derive(Debug, Clone)]
pub struct Line {
    pub label: String,
    pub entry: ModelEntry,
    pub result: FitResult,
}

/// Everything `canfit models` found, before rendering.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub dir: PathBuf,
    /// Smallest estimate first.
    pub lines: Vec<Line>,
    pub problems: Vec<Problem>,
    pub available_bytes: u64,
    pub gpu_limit_bytes: Option<u64>,
    pub pressure_level: PressureLevel,
}

impl Catalog {
    fn count(&self, verdict: Verdict) -> usize {
        self.lines
            .iter()
            .filter(|l| l.result.verdict == verdict)
            .count()
    }
}

/// Loads and estimates every model found, and judges each one against `snapshot`.
pub fn build(dir: &Path, scan: Scan, options: EstimateOptions, snapshot: &Snapshot) -> Catalog {
    let mut problems = scan.problems;
    let mut lines = Vec::new();
    for found in scan.found {
        match model::load(&found.parts) {
            Ok(facts) if facts.architecture.as_deref() == Some(PROJECTOR_ARCHITECTURE) => {
                problems.push(Problem {
                    path: found.parts[0].to_string_lossy().into_owned(),
                    error: "a vision projector (mmproj), loaded together with its language \
                            model rather than on its own"
                        .to_string(),
                });
            }
            Ok(facts) => {
                let estimate = model::estimate(&facts, options);
                let result = fit::evaluate(estimate.base_bytes, estimate.total_bytes, snapshot);
                lines.push(Line {
                    label: found.label,
                    entry: ModelEntry { facts, estimate },
                    result,
                });
            }
            Err(e) => problems.push(Problem {
                path: found.parts[0].to_string_lossy().into_owned(),
                error: e.to_string(),
            }),
        }
    }
    lines.sort_by(|a, b| {
        (a.entry.estimate.total_bytes, &a.label).cmp(&(b.entry.estimate.total_bytes, &b.label))
    });
    problems.sort_by(|a, b| a.path.cmp(&b.path));
    Catalog {
        dir: dir.to_path_buf(),
        lines,
        problems,
        available_bytes: snapshot.memory.available_bytes,
        gpu_limit_bytes: snapshot.gpu_memory_limit.as_ref().map(|g| g.bytes),
        pressure_level: snapshot.pressure.level,
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "model" } else { "models" }
}

/// How the folder is named in sentences: "the current folder" for `.`, so a sentence never ends
/// in "..".
fn folder_name(dir: &Path) -> String {
    if dir.as_os_str().is_empty() || dir == Path::new(".") {
        "the current folder".to_string()
    } else {
        dir.display().to_string()
    }
}

/// The sentence that opens the output.
pub fn summary_sentence(c: &Catalog) -> String {
    let dir = folder_name(&c.dir);
    let total = c.lines.len();
    let available = format_bytes(c.available_bytes);
    if total == 0 {
        return if c.problems.is_empty() {
            format!("No GGUF models in {dir}.")
        } else {
            format!(
                "✗ canfit found {} .gguf {} in {dir} but could not read any.",
                c.problems.len(),
                if c.problems.len() == 1 {
                    "file"
                } else {
                    "files"
                }
            )
        };
    }
    let fits = c.count(Verdict::Fits);
    let tight = c.count(Verdict::Tight);
    let usable = fits + tight;
    let largest = c
        .lines
        .iter()
        .rev()
        .find(|l| l.result.verdict != Verdict::DoesntFit);
    let tight_note = match tight {
        0 => String::new(),
        n if n == usable => " (only just)".to_string(),
        n => format!(" ({n} only just)"),
    };
    match largest {
        None => {
            let smallest = &c.lines[0];
            let check = smallest.result.deciding();
            let limit = match check.limit {
                LimitKind::AvailableMemory => format!("only {available} is available"),
                LimitKind::GpuMemoryLimit => {
                    format!("the GPU can use only {}", format_bytes(check.limit_bytes))
                }
            };
            let none = if total == 1 {
                "The model".to_string()
            } else {
                format!("None of the {total} models")
            };
            format!(
                "✗ {none} in {dir} {} right now: the smallest needs about {} and {limit}.",
                if total == 1 { "doesn't fit" } else { "fit" },
                format_bytes(smallest.entry.estimate.total_bytes)
            )
        }
        Some(l) if usable == total => {
            let symbol = if fits == total { "●" } else { "▲" };
            let all = if total == 1 {
                "The model".to_string()
            } else {
                format!("All {total} models")
            };
            format!(
                "{symbol} {all} in {dir} {} right now{tight_note}: the largest needs about {} and \
                 {available} is available.",
                if total == 1 { "fits" } else { "fit" },
                format_bytes(l.entry.estimate.total_bytes)
            )
        }
        Some(l) => format!(
            "● {usable} of {total} {} in {dir} fit right now{tight_note}; the largest that fits \
             is {}, which needs about {} of the {available} available.",
            plural(total),
            l.label,
            format_bytes(l.entry.estimate.total_bytes)
        ),
    }
}

fn status(check: &LimitCheck, result: &FitResult) -> String {
    let symbol = result.verdict.symbol();
    let headroom = format_bytes(check.headroom_bytes.unsigned_abs());
    let gpu = check.limit == LimitKind::GpuMemoryLimit;
    match (result.verdict, check.tight_cause, gpu) {
        (Verdict::Fits, ..) => format!("{symbol} fits, {headroom} to spare"),
        (Verdict::Tight, Some(TightCause::PressureCritical), _) => {
            format!("{symbol} tight: pressure is critical")
        }
        (Verdict::Tight, _, false) => format!("{symbol} tight: only without the margin"),
        (Verdict::Tight, _, true) => format!("{symbol} tight for the GPU limit"),
        (Verdict::DoesntFit, _, false) => format!("{symbol} {headroom} short"),
        (Verdict::DoesntFit, _, true) => format!("{symbol} {headroom} over the GPU limit"),
    }
}

/// The full text output: answer, the catalog card and notes.
pub fn render_text(c: &Catalog, theme: Option<ui::theme::Theme>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", card::sentence(&summary_sentence(c), theme));
    if !c.lines.is_empty() {
        let rows: Vec<Option<Row>> = c
            .lines
            .iter()
            .map(|l| {
                Some(Row::new(
                    l.label.clone(),
                    format_bytes(l.entry.estimate.total_bytes),
                    format!(
                        "{} · {} file {}",
                        status(l.result.deciding(), &l.result),
                        l.entry.facts.quantization,
                        format_bytes(l.entry.facts.file_bytes)
                    ),
                ))
            })
            .collect();
        out.push('\n');
        out.push_str(&card::render_styled(
            &format!("Models in {}", folder_name(&c.dir)),
            &rows,
            theme,
        ));
    }
    for note in notes(c) {
        out.push_str(&card::wrap(&note, 100, 2));
    }
    out
}

fn notes(c: &Catalog) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(first) = c.lines.first() {
        let e = &first.entry.estimate;
        let ctx = match e.context_source {
            model::ContextSource::Flag => {
                format!("{} tokens of context", group_digits(e.context_tokens))
            }
            _ => format!(
                "canfit's default context ({} tokens, or less when a model is trained for less)",
                group_digits(model::DEFAULT_CONTEXT)
            ),
        };
        notes.push(format!(
            "The size next to each model is the memory it needs (estimated) with {ctx}, an {} \
             cache and a {}% margin; change them with --ctx, --cache-type and --margin. File \
             sizes are read from disk.",
            e.cache_type.label(),
            e.margin_percent
        ));
        let gpu = c
            .gpu_limit_bytes
            .map(|g| {
                format!(
                    ", and the GPU can use up to {} (estimated)",
                    format_bytes(g)
                )
            })
            .unwrap_or_default();
        notes.push(format!(
            "Each model is checked on its own against the {} available now (measured){gpu}. To \
             check several together, run canfit with their files.",
            format_bytes(c.available_bytes)
        ));
    }
    if c.pressure_level != PressureLevel::Calm {
        notes.push(format!(
            "Memory pressure is {} right now.",
            c.pressure_level.label()
        ));
    }
    for p in &c.problems {
        let path = Path::new(&p.path);
        let shown = path.strip_prefix(&c.dir).unwrap_or(path);
        notes.push(format!("Skipped {}: {}", shown.display(), p.error));
    }
    notes
}

#[derive(Serialize)]
struct ModelJson<'a> {
    label: &'a str,
    verdict: Verdict,
    deciding_limit: LimitKind,
    checks: &'a [LimitCheck],
    #[serde(flatten)]
    entry: &'a ModelEntry,
}

#[derive(Serialize)]
struct Counts {
    fits: usize,
    tight: usize,
    doesnt_fit: usize,
}

#[derive(Serialize)]
struct ModelsDocument<'a> {
    schema: &'static str,
    schema_version: u32,
    canfit_version: &'static str,
    summary: String,
    directory: String,
    memory_available_bytes: u64,
    gpu_memory_limit_bytes: Option<u64>,
    pressure_level: PressureLevel,
    counts: Counts,
    models: Vec<ModelJson<'a>>,
    skipped: &'a [Problem],
}

pub fn to_json(c: &Catalog) -> String {
    let document = ModelsDocument {
        schema: MODELS_SCHEMA,
        schema_version: MODELS_SCHEMA_VERSION,
        canfit_version: env!("CARGO_PKG_VERSION"),
        summary: summary_sentence(c),
        directory: c.dir.to_string_lossy().into_owned(),
        memory_available_bytes: c.available_bytes,
        gpu_memory_limit_bytes: c.gpu_limit_bytes,
        pressure_level: c.pressure_level,
        counts: Counts {
            fits: c.count(Verdict::Fits),
            tight: c.count(Verdict::Tight),
            doesnt_fit: c.count(Verdict::DoesntFit),
        },
        models: c
            .lines
            .iter()
            .map(|l| ModelJson {
                label: &l.label,
                verdict: l.result.verdict,
                deciding_limit: l.result.deciding_limit,
                checks: &l.result.checks,
                entry: &l.entry,
            })
            .collect(),
        skipped: &c.problems,
    };
    serde_json::to_string_pretty(&document).expect("models catalog serializes")
}

/// Runs `canfit models`. Exits 0 when the folder was listed (whatever fits), 3 when it can't be
/// read.
pub fn run(dir: Option<&Path>, options: EstimateOptions, json: bool, theme: ThemeName) -> ExitCode {
    let dir = dir.unwrap_or(Path::new("."));
    let scan = match scan(dir) {
        Ok(scan) => scan,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let snapshot = match report::take_snapshot(false) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let catalog = build(dir, scan, options, &snapshot);
    if json {
        println!("{}", to_json(&catalog));
    } else {
        print!(
            "{}",
            ui::glyphs::text(&render_text(&catalog, ui::stdout_theme(theme)))
        );
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sampler::tests::replay;
    use crate::core::{GpuMemoryLimit, Provenance};
    use crate::estimate::gguf::builder::{Gguf, Kv};
    use crate::history::tests::temp_dir;

    const GB: u64 = 1024 * 1024 * 1024;

    fn options() -> EstimateOptions {
        EstimateOptions {
            overhead: model::OverheadProfile::Uncalibrated,
            ..EstimateOptions::default()
        }
    }

    fn snapshot(available: u64) -> Snapshot {
        let (_, mut s) = replay("calm");
        s.memory.available_bytes = available;
        s.gpu_memory_limit = None;
        s
    }

    /// A folder like a real models folder: loose files, a split model, nested publisher folders,
    /// a hidden folder, a broken split, a bogus file and a non-model file.
    fn models_folder() -> PathBuf {
        let dir = temp_dir("models");
        Gguf::llama("small", 2).write(&dir.join("small-q4.gguf"));
        // The most layers, so the largest estimate (no sparse file: NTFS would allocate it).
        Gguf::llama("big", 24).write(&dir.join("big-q8.gguf"));
        let nested = dir.join("lmstudio-community").join("Qwen");
        fs::create_dir_all(&nested).unwrap();
        Gguf::llama("nested", 4).write(&nested.join("qwen-q4.gguf"));
        for n in 1..=2 {
            Gguf::llama("split", 4).write(&dir.join(format!("split-{n:05}-of-00002.gguf")));
        }
        Gguf::llama("broken", 2).write(&dir.join("broken-00001-of-00003.gguf"));
        let hidden = dir.join(".cache");
        fs::create_dir_all(&hidden).unwrap();
        Gguf::llama("hidden", 2).write(&hidden.join("hidden.gguf"));
        fs::write(dir.join("bogus.gguf"), b"not a model").unwrap();
        fs::write(dir.join("README.md"), b"hi").unwrap();
        dir
    }

    #[test]
    fn scan_finds_models_splits_and_problems() {
        let dir = models_folder();
        let scan = scan(&dir).unwrap();
        let labels: Vec<&str> = scan.found.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(
            labels,
            ["big-q8", "bogus", "qwen-q4", "small-q4", "split (2 parts)"]
        );
        let split = scan
            .found
            .iter()
            .find(|f| f.label.starts_with("split"))
            .unwrap();
        assert_eq!(split.parts.len(), 2);
        assert_eq!(scan.problems.len(), 1);
        assert!(
            scan.problems[0]
                .path
                .ends_with("broken-00001-of-00003.gguf")
        );
        assert!(scan.problems[0].error.contains("is missing"));
    }

    #[test]
    fn duplicate_names_show_their_folders_and_projectors_are_skipped() {
        let dir = temp_dir("models-dup");
        for publisher in ["a", "b"] {
            let folder = dir.join(publisher);
            fs::create_dir_all(&folder).unwrap();
            Gguf::llama("m", 2).write(&folder.join("model-q4.gguf"));
        }
        Gguf::new()
            .kv("general.architecture", Kv::Str("clip"))
            .kv("general.name", Kv::Str("vision"))
            .tensor("v.patch_embd.weight", &[256, 64], 1)
            .write(&dir.join("mmproj-F16.gguf"));
        let scan = scan(&dir).unwrap();
        let labels: Vec<&str> = scan.found.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["a/model-q4", "b/model-q4", "mmproj-F16"]);
        let c = build(&dir, scan, options(), &snapshot(1000 * GB));
        assert_eq!(c.lines.len(), 2);
        assert!(
            c.problems[0].error.contains("vision projector"),
            "{:?}",
            c.problems
        );
    }

    #[test]
    fn catalog_judges_each_model_and_answers_first() {
        let dir = models_folder();
        let scan = scan(&dir).unwrap();
        let probe = build(&dir, scan.clone(), options(), &snapshot(1000 * GB));
        let needs: Vec<u64> = probe
            .lines
            .iter()
            .map(|l| l.entry.estimate.total_bytes)
            .collect();
        assert!(needs.windows(2).all(|w| w[0] <= w[1]), "smallest first");
        assert_eq!(probe.lines.len(), 4);
        assert_eq!(probe.problems.len(), 2, "bogus file and broken split");
        assert!(summary_sentence(&probe).starts_with("● All 4 models in "));

        // Room for everything but the largest model. Without a margin, a model either fits or
        // doesn't, so a few kilobytes separate the two.
        let exact = EstimateOptions {
            margin_percent: 0,
            ..options()
        };
        let probe = build(&dir, scan.clone(), exact, &snapshot(1000 * GB));
        let needs: Vec<u64> = probe
            .lines
            .iter()
            .map(|l| l.entry.estimate.total_bytes)
            .collect();
        assert!(needs[3] > needs[2]);
        assert_eq!(probe.lines[3].label, "big-q8");
        let c = build(&dir, scan.clone(), exact, &snapshot(needs[2]));
        assert_eq!(c.lines.last().unwrap().result.verdict, Verdict::DoesntFit);
        let s = summary_sentence(&c);
        assert!(s.starts_with("● 3 of 4 models in "), "{s}");
        assert!(s.contains("the largest that fits is"), "{s}");
        let text = render_text(&c, None);
        assert!(text.contains("╭─ Models in "), "{text}");
        assert!(text.contains("● fits, "), "{text}");
        assert!(text.contains(" short · Q4_K_M file"), "{text}");
        assert!(text.contains("split (2 parts)"));
        assert!(text.contains("(estimated)") && text.contains("(measured)"));
        assert!(text.contains("Skipped "), "{text}");

        // Nothing fits.
        let c = build(&dir, scan, options(), &snapshot(GB / 1024));
        let s = summary_sentence(&c);
        assert!(s.starts_with("✗ None of the 4 models"), "{s}");
        assert!(s.contains("the smallest needs about"), "{s}");
    }

    #[test]
    fn gpu_limit_and_tight_models_are_named() {
        let dir = temp_dir("models-gpu");
        Gguf::llama("one", 2).write(&dir.join("one.gguf"));
        let scan = scan(&dir).unwrap();
        let mut s = snapshot(1000 * GB);
        s.gpu_memory_limit = Some(GpuMemoryLimit {
            bytes: 1,
            provenance: Provenance::Estimated,
            source: "test".into(),
        });
        let c = build(&dir, scan.clone(), options(), &s);
        assert!(
            summary_sentence(&c).contains("the GPU can use only"),
            "{}",
            summary_sentence(&c)
        );
        assert!(render_text(&c, None).contains("over the GPU limit"));

        let need = &c.lines[0].entry.estimate;
        let c = build(&dir, scan, options(), &snapshot(need.base_bytes));
        assert_eq!(c.lines[0].result.verdict, Verdict::Tight);
        let sentence = summary_sentence(&c);
        assert!(sentence.starts_with("▲ The model in"), "{sentence}");
        assert!(sentence.contains("(only just)"), "{sentence}");
    }

    #[test]
    fn json_lists_models_with_verdicts() {
        let dir = models_folder();
        let c = build(&dir, scan(&dir).unwrap(), options(), &snapshot(1000 * GB));
        let v: serde_json::Value = serde_json::from_str(&to_json(&c)).unwrap();
        assert_eq!(v["schema"], MODELS_SCHEMA);
        assert_eq!(v["schema_version"], MODELS_SCHEMA_VERSION);
        assert_eq!(v["counts"]["fits"], 4);
        assert_eq!(v["models"].as_array().unwrap().len(), 4);
        let m = &v["models"][0];
        assert_eq!(m["verdict"], "fits");
        assert!(m["label"].is_string() && m["name"].is_string());
        assert_eq!(m["estimate"]["overhead_profile"], "uncalibrated");
        assert_eq!(m["checks"][0]["limit"], "available_memory");
        assert_eq!(v["skipped"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn empty_and_missing_folders() {
        let dir = temp_dir("models-empty");
        let c = build(&dir, scan(&dir).unwrap(), options(), &snapshot(GB));
        assert_eq!(
            summary_sentence(&c),
            format!("No GGUF models in {}.", dir.display())
        );
        let here = build(
            Path::new("."),
            Scan {
                found: Vec::new(),
                problems: Vec::new(),
            },
            options(),
            &snapshot(GB),
        );
        assert_eq!(
            summary_sentence(&here),
            "No GGUF models in the current folder."
        );
        assert!(scan(&dir.join("nope")).unwrap_err().contains("can't open"));
        let file = dir.join("f.gguf");
        fs::write(&file, b"x").unwrap();
        assert!(scan(&file).unwrap_err().contains("is not a folder"));
    }
}
