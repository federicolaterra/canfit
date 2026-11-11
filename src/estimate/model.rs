//! Model facts read from GGUF headers, and the memory estimate of spec §8:
//! `total ≈ weights + KV cache + overhead + margin`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::gguf::{self, GgmlType, GgufError, GgufFile, Value};

const GIB: u64 = 1024 * 1024 * 1024;

/// Context used when `--ctx` is not given, capped by the model's own context length (ADR-011).
pub const DEFAULT_CONTEXT: u64 = 8192;
/// Default safety margin on top of the estimate, in percent.
pub const DEFAULT_MARGIN_PERCENT: u32 = 10;

/// How the runtime overhead (compute buffers, runtime, graph) is estimated (ADR-011).
///
/// `overhead = fixed + context_tokens × embedding_length × bytes_per_embedding + share × weights`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverheadProfile {
    /// llama.cpp with Metal on Apple Silicon, calibrated against llama-server's own buffer
    /// accounting (see `docs/open-source/estimation.md`).
    AppleSilicon,
    /// Backends not measured yet (CPU, CUDA, Vulkan): the spec's starting heuristic,
    /// 1 GiB + 7.6 % of the weights, which runs high on purpose.
    Uncalibrated,
}

impl OverheadProfile {
    /// The profile for the machine canfit runs on.
    pub fn for_host() -> OverheadProfile {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            OverheadProfile::AppleSilicon
        } else {
            OverheadProfile::Uncalibrated
        }
    }

    /// `(fixed bytes, bytes per token per embedding dimension, share of weights)`.
    pub fn parameters(self) -> (u64, u64, f64) {
        match self {
            OverheadProfile::AppleSilicon => (256 * 1024 * 1024, 2, 0.02),
            OverheadProfile::Uncalibrated => (GIB, 0, 0.076),
        }
    }

    /// Estimated overhead for a model with `weights` bytes and `embedding` dimensions at `ctx`.
    pub fn overhead(self, weights: u64, embedding: Option<u64>, ctx: u64) -> u64 {
        let (fixed, per_token_dim, share) = self.parameters();
        // Without an embedding length, assume a large one (8192) rather than none.
        let context = ctx
            .saturating_mul(embedding.unwrap_or(8192))
            .saturating_mul(per_token_dim);
        fixed
            .saturating_add(context)
            .saturating_add((weights as f64 * share).round() as u64)
    }
}

/// KV cache element type, as llama.cpp's `--cache-type-k/-v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum CacheType {
    #[default]
    F16,
    #[value(name = "q8_0")]
    Q8_0,
    #[value(name = "q4_0")]
    Q4_0,
}

impl CacheType {
    pub fn bytes_per_element(self) -> f64 {
        match self {
            CacheType::F16 => GgmlType::F16.bytes_per_element(),
            CacheType::Q8_0 => GgmlType::Q8_0.bytes_per_element(),
            CacheType::Q4_0 => GgmlType::Q4_0.bytes_per_element(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CacheType::F16 => "f16",
            CacheType::Q8_0 => "q8_0",
            CacheType::Q4_0 => "q4_0",
        }
    }
}

/// Why a model could not be loaded.
#[derive(Debug)]
pub struct ModelError {
    pub path: PathBuf,
    pub kind: ModelErrorKind,
}

#[derive(Debug)]
pub enum ModelErrorKind {
    Gguf(GgufError),
    /// The parts of a split model disagree with each other.
    Split(String),
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.kind {
            ModelErrorKind::Gguf(GgufError::Io(err)) => {
                write!(f, "canfit can't read {path}: {err}")
            }
            ModelErrorKind::Gguf(err) => write!(f, "canfit can't use {path}: {err}"),
            ModelErrorKind::Split(detail) => write!(f, "canfit can't use {path}: {detail}"),
        }
    }
}

impl std::error::Error for ModelError {}

/// What canfit knows about one model (all parts of a split model together).
#[derive(Debug, Clone, Serialize)]
pub struct ModelFacts {
    /// `general.name`, or the file name.
    pub name: String,
    #[serde(serialize_with = "lossy_path")]
    pub path: PathBuf,
    /// Every file of the model; more than one for split models.
    #[serde(serialize_with = "lossy_paths")]
    pub parts: Vec<PathBuf>,
    pub gguf_version: u32,
    pub architecture: Option<String>,
    /// Sum of all tensors' elements (exact; every expert of a MoE model counts).
    pub parameter_count: u64,
    /// The file type llama.cpp records (`Q4_K_M`, ...), or the dominant tensor type.
    pub quantization: String,
    pub bits_per_weight: f64,
    /// Exact size of all tensor data, in bytes.
    pub weights_bytes: u64,
    pub tensor_count: usize,
    /// Size of the model's file(s) on disk.
    pub file_bytes: u64,
    pub block_count: Option<u64>,
    /// Context length the model was trained for.
    pub context_length: Option<u64>,
    pub embedding_length: Option<u64>,
    pub head_count: Option<u64>,
    /// KV heads; the largest value when it differs per layer.
    pub head_count_kv: Option<u64>,
    pub key_length: Option<u64>,
    pub value_length: Option<u64>,
    pub expert_count: Option<u64>,
    pub expert_used_count: Option<u64>,
    pub sliding_window: Option<u64>,
    /// MLA latent KV size (DeepSeek-style attention), when present.
    pub kv_lora_rank: Option<u64>,
    /// KV cache bytes per token of context with an f16 cache, summed over layers. `None` when
    /// the file has no attention metadata.
    #[serde(skip)]
    kv_elements_per_token: Option<u64>,
}

fn lossy_path<S: serde::Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&path.to_string_lossy())
}

fn lossy_paths<S: serde::Serializer>(paths: &[PathBuf], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(paths.iter().map(|p| p.to_string_lossy()))
}

/// Loads a model from its file(s). `parts` is in order: part 1 first.
pub fn load(parts: &[PathBuf]) -> Result<ModelFacts, ModelError> {
    let first_path = parts
        .first()
        .expect("a model has at least one file")
        .clone();
    let mut files = Vec::with_capacity(parts.len());
    for path in parts {
        let file = gguf::read_file(path).map_err(|err| ModelError {
            path: path.clone(),
            kind: ModelErrorKind::Gguf(err),
        })?;
        files.push(file);
    }
    let split_error = |detail: String| ModelError {
        path: first_path.clone(),
        kind: ModelErrorKind::Split(detail),
    };
    if let Some(count) = files[0].get_u64("split.count")
        && count != parts.len() as u64
    {
        return Err(split_error(format!(
            "it says the model has {count} parts, but {} were found",
            parts.len()
        )));
    }
    for (i, file) in files.iter().enumerate() {
        if let Some(no) = file.get_u64("split.no")
            && no != i as u64
        {
            return Err(split_error(format!(
                "{} is part {} of the model, expected part {}",
                parts[i].display(),
                no + 1,
                i + 1
            )));
        }
    }
    let tensor_count: usize = files.iter().map(|f| f.tensors.len()).sum();
    if let Some(expected) = files[0].get_u64("split.tensors.count")
        && expected != tensor_count as u64
    {
        return Err(split_error(format!(
            "the parts hold {tensor_count} tensors, the model declares {expected}"
        )));
    }
    Ok(facts(&first_path, parts, &files))
}

fn facts(path: &Path, parts: &[PathBuf], files: &[GgufFile]) -> ModelFacts {
    let head = &files[0];
    let architecture = head.get_str("general.architecture").map(str::to_string);
    let arch = architecture.clone().unwrap_or_default();
    let key = |suffix: &str| format!("{arch}.{suffix}");
    let num = |suffix: &str| head.get_u64(&key(suffix));

    let weights_bytes: u64 = files.iter().map(GgufFile::tensor_bytes).sum();
    let parameter_count: u64 = files.iter().map(GgufFile::tensor_elements).sum();
    let bits_per_weight = if parameter_count == 0 {
        0.0
    } else {
        weights_bytes as f64 * 8.0 / parameter_count as f64
    };

    let block_count = num("block_count");
    let embedding_length = num("embedding_length");
    let layers = block_count.unwrap_or(0);
    let heads = per_layer(head.get(&key("attention.head_count")), layers);
    let kv_heads =
        per_layer(head.get(&key("attention.head_count_kv")), layers).or_else(|| heads.clone());
    let key_length = num("attention.key_length");
    let value_length = num("attention.value_length");
    let kv_lora_rank = num("attention.kv_lora_rank");

    let kv_elements_per_token = match (&heads, &kv_heads) {
        _ if layers == 0 => None,
        (_, _) if kv_lora_rank.is_some() => {
            // MLA caches one compressed latent plus the RoPE part per layer and token.
            let rope = num("rope.dimension_count").unwrap_or(0);
            Some(layers * (kv_lora_rank.unwrap_or(0) + rope))
        }
        (Some(heads), Some(kv_heads)) => {
            let mut total = 0u64;
            for (&h, &kv) in heads.iter().zip(kv_heads) {
                if kv == 0 || h == 0 {
                    continue;
                }
                let head_dim = embedding_length.unwrap_or(0) / h;
                let k = key_length.unwrap_or(head_dim);
                let v = value_length.unwrap_or(head_dim);
                total = total.saturating_add(kv.saturating_mul(k.saturating_add(v)));
            }
            Some(total)
        }
        _ => None,
    };

    let name = model_name(head, path, &arch);

    ModelFacts {
        name,
        path: path.to_path_buf(),
        parts: parts.to_vec(),
        gguf_version: head.version,
        quantization: quantization(head, files),
        architecture,
        parameter_count,
        bits_per_weight,
        weights_bytes,
        tensor_count: files.iter().map(|f| f.tensors.len()).sum(),
        file_bytes: files.iter().map(|f| f.file_size).sum(),
        block_count,
        context_length: num("context_length"),
        embedding_length,
        head_count: heads.as_ref().and_then(|h| h.iter().copied().max()),
        head_count_kv: kv_heads.as_ref().and_then(|h| h.iter().copied().max()),
        key_length,
        value_length,
        expert_count: num("expert_count").filter(|&n| n > 0),
        expert_used_count: num("expert_used_count").filter(|&n| n > 0),
        sliding_window: num("attention.sliding_window").filter(|&n| n > 0),
        kv_lora_rank,
        kv_elements_per_token,
    }
}

/// A per-layer value that may be stored as one number or as an array with one entry per layer.
fn per_layer(value: Option<&Value>, layers: u64) -> Option<Vec<u64>> {
    let layers = usize::try_from(layers).ok()?;
    match value? {
        Value::Array(_) => {
            let items = value?.as_array()?;
            let values: Option<Vec<u64>> = items.iter().map(Value::as_u64).collect();
            values.filter(|v| v.len() == layers)
        }
        other => other.as_u64().map(|n| vec![n; layers]),
    }
}

/// File name without the `.gguf` extension and split suffix.
fn display_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name
        .strip_suffix(".gguf")
        .or_else(|| name.strip_suffix(".GGUF"))
        .unwrap_or(&name);
    match super::target::split_name(&name) {
        Some(split) => split.prefix.to_string(),
        None => stem.to_string(),
    }
}

/// llama.cpp `llama_ftype` names.
fn file_type_name(id: u64) -> Option<&'static str> {
    Some(match id {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        36 => "TQ1_0",
        37 => "TQ2_0",
        38 => "MXFP4_MOE",
        _ => return None,
    })
}

/// Some converters write a content hash into `general.name`; such names help nobody.
fn looks_like_hash(name: &str) -> bool {
    let core = name.strip_prefix("sha256-").unwrap_or(name);
    core.len() >= 24 && core.chars().all(|c| c.is_ascii_hexdigit())
}

/// `general.name`, else the file name, else `basename size` from the metadata.
fn model_name(head: &GgufFile, path: &Path, arch: &str) -> String {
    if let Some(name) = head
        .get_str("general.name")
        .map(str::trim)
        .filter(|n| !n.is_empty() && !looks_like_hash(n))
    {
        return name.to_string();
    }
    let stem = display_stem(path);
    if !stem.is_empty() && !looks_like_hash(&stem) {
        return stem;
    }
    let base = head
        .get_str("general.basename")
        .filter(|b| !b.trim().is_empty())
        .unwrap_or(if arch.is_empty() { "model" } else { arch });
    match head.get_str("general.size_label") {
        Some(size) => format!("{base} {size}"),
        None => base.to_string(),
    }
}

/// The ggml type holding the most bytes.
fn dominant_type(files: &[GgufFile]) -> Option<GgmlType> {
    let mut by_type: Vec<(GgmlType, u64)> = Vec::new();
    for t in files.iter().flat_map(|f| &f.tensors) {
        match by_type.iter_mut().find(|(ty, _)| *ty == t.ggml_type) {
            Some((_, bytes)) => *bytes += t.bytes,
            None => by_type.push((t.ggml_type, t.bytes)),
        }
    }
    by_type
        .into_iter()
        .max_by_key(|&(_, bytes)| bytes)
        .map(|(ty, _)| ty)
}

/// The recorded file type (`Q4_K_M`), unless the tensors contradict it — some converters write
/// a wrong `general.file_type` — in which case the dominant tensor type (`Q3_K`).
fn quantization(head: &GgufFile, files: &[GgufFile]) -> String {
    let dominant = dominant_type(files).map(GgmlType::name);
    let recorded = head.get_u64("general.file_type").and_then(file_type_name);
    match (recorded, dominant) {
        (Some(ft), Some(dom))
            if ft.starts_with(dom) || (ft.starts_with("IQ") && dom.starts_with("IQ")) =>
        {
            ft.to_string()
        }
        (Some(ft), None) => ft.to_string(),
        (_, Some(dom)) => dom.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

/// Options of one estimate.
#[derive(Debug, Clone, Copy)]
pub struct EstimateOptions {
    /// `--ctx`; `None` uses [`DEFAULT_CONTEXT`] capped by the model's context length.
    pub context: Option<u64>,
    pub cache_type: CacheType,
    pub margin_percent: u32,
    pub overhead: OverheadProfile,
}

impl Default for EstimateOptions {
    fn default() -> Self {
        EstimateOptions {
            context: None,
            cache_type: CacheType::F16,
            margin_percent: DEFAULT_MARGIN_PERCENT,
            overhead: OverheadProfile::for_host(),
        }
    }
}

/// Where the context length came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    /// `--ctx`.
    Flag,
    /// canfit's default, [`DEFAULT_CONTEXT`].
    Default,
    /// The model's context length, smaller than the default.
    Model,
}

/// The memory a model needs, by component. All sizes in bytes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Estimate {
    /// Exact, read from the GGUF file.
    pub weights_bytes: u64,
    /// Computed from file metadata, the context and the cache type.
    pub kv_cache_bytes: u64,
    /// Compute buffers and runtime; a heuristic.
    pub overhead_bytes: u64,
    /// Safety margin on top of the other three.
    pub margin_bytes: u64,
    /// weights + KV cache + overhead, without the margin.
    pub base_bytes: u64,
    /// base + margin: what canfit compares with available memory.
    pub total_bytes: u64,
    pub context_tokens: u64,
    pub context_source: ContextSource,
    pub cache_type: CacheType,
    pub margin_percent: u32,
    pub overhead_profile: OverheadProfile,
    /// Plain-language caveats about this estimate.
    pub notes: Vec<String>,
}

/// Estimates the memory needed to run `model` (spec §8).
pub fn estimate(model: &ModelFacts, options: EstimateOptions) -> Estimate {
    let mut notes = Vec::new();
    let (context_tokens, context_source) = match (options.context, model.context_length) {
        (Some(ctx), trained) => {
            if let Some(trained) = trained.filter(|&t| ctx > t) {
                notes.push(format!(
                    "{} tokens is more than the {} the model was trained for.",
                    group_digits(ctx),
                    group_digits(trained)
                ));
            }
            (ctx, ContextSource::Flag)
        }
        (None, Some(trained)) if trained < DEFAULT_CONTEXT => (trained, ContextSource::Model),
        (None, _) => (DEFAULT_CONTEXT, ContextSource::Default),
    };

    let kv_cache_bytes = match model.kv_elements_per_token {
        Some(per_token) => {
            let elements = per_token as f64 * context_tokens as f64;
            (elements * options.cache_type.bytes_per_element()).ceil() as u64
        }
        None => {
            notes.push(
                "The file has no attention metadata, so the context cache is not included."
                    .to_string(),
            );
            0
        }
    };
    if model.sliding_window.is_some() && model.kv_lora_rank.is_none() {
        notes.push(
            "This model uses sliding-window attention; canfit sizes a full cache for every layer, \
             so long-context estimates run high."
                .to_string(),
        );
    }

    let overhead_bytes =
        options
            .overhead
            .overhead(model.weights_bytes, model.embedding_length, context_tokens);
    let base_bytes = model.weights_bytes + kv_cache_bytes + overhead_bytes;
    let margin_bytes = base_bytes * u64::from(options.margin_percent) / 100;
    Estimate {
        weights_bytes: model.weights_bytes,
        kv_cache_bytes,
        overhead_bytes,
        margin_bytes,
        base_bytes,
        total_bytes: base_bytes + margin_bytes,
        context_tokens,
        context_source,
        cache_type: options.cache_type,
        margin_percent: options.margin_percent,
        overhead_profile: options.overhead,
        notes,
    }
}

/// `8192` → `8,192`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `8_190_735_360` → `8.2B`.
pub fn format_parameters(n: u64) -> String {
    let n = n as f64;
    if n >= 1e12 {
        format!("{:.1}T", n / 1e12)
    } else if n >= 1e9 {
        format!("{:.1}B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.0}M", n / 1e6)
    } else {
        format!("{:.0}K", n / 1e3)
    }
}

#[cfg(test)]
mod tests {
    use super::super::gguf::builder::{Gguf, Kv};
    use super::*;

    fn write_model(dir: &Path, name: &str, gguf: &Gguf) -> PathBuf {
        let path = dir.join(name);
        gguf.write(&path);
        path
    }

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("canfit-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_model_facts() {
        let dir = temp_dir("facts");
        let path = write_model(&dir, "tiny.gguf", &Gguf::llama("Tiny", 4));
        let m = load(&[path]).unwrap();
        assert_eq!(m.name, "Tiny");
        assert_eq!(m.architecture.as_deref(), Some("llama"));
        assert_eq!(m.quantization, "Q4_K_M");
        assert_eq!(m.block_count, Some(4));
        assert_eq!(m.context_length, Some(4096));
        assert_eq!(m.head_count, Some(8));
        assert_eq!(m.head_count_kv, Some(2));
        assert_eq!(m.parameter_count, 256 * 64 + 4 * 256 * 256);
        // Q4_K: 144 bytes per 256 elements.
        assert_eq!(m.weights_bytes, m.parameter_count / 256 * 144);
        assert!((m.bits_per_weight - 4.5).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn kv_cache_follows_the_spec_formula() {
        let dir = temp_dir("kv");
        let path = write_model(&dir, "tiny.gguf", &Gguf::llama("Tiny", 4));
        let m = load(&[path]).unwrap();
        // head_dim = 256 / 8 = 32; 2 × 4 layers × ctx × 2 kv heads × 32 × 2 bytes.
        let e = estimate(&m, EstimateOptions::default());
        assert_eq!(e.context_tokens, 4096);
        assert_eq!(e.context_source, ContextSource::Model);
        assert_eq!(e.kv_cache_bytes, 2 * 4 * 4096 * 2 * 32 * 2);
        let q8 = estimate(
            &m,
            EstimateOptions {
                context: Some(1000),
                cache_type: CacheType::Q8_0,
                ..EstimateOptions::default()
            },
        );
        assert_eq!(q8.context_source, ContextSource::Flag);
        assert_eq!(q8.kv_cache_bytes, (2 * 4 * 1000 * 2 * 32) as u64 * 34 / 32);
        let long = estimate(
            &m,
            EstimateOptions {
                context: Some(8192),
                ..EstimateOptions::default()
            },
        );
        assert_eq!(long.notes.len(), 1, "{:?}", long.notes);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn total_is_weights_kv_overhead_and_margin() {
        let dir = temp_dir("total");
        let path = write_model(&dir, "tiny.gguf", &Gguf::llama("Tiny", 2));
        let m = load(&[path]).unwrap();
        let uncalibrated = EstimateOptions {
            overhead: OverheadProfile::Uncalibrated,
            ..EstimateOptions::default()
        };
        let e = estimate(&m, uncalibrated);
        let overhead = GIB + (m.weights_bytes as f64 * 0.076).round() as u64;
        assert_eq!(e.overhead_bytes, overhead);
        let apple = estimate(
            &m,
            EstimateOptions {
                overhead: OverheadProfile::AppleSilicon,
                ..uncalibrated
            },
        );
        // 256 MiB + 4096 tokens × 256 dims × 2 B + 2 % of the weights.
        assert_eq!(
            apple.overhead_bytes,
            256 * 1024 * 1024 + 4096 * 256 * 2 + (m.weights_bytes as f64 * 0.02).round() as u64
        );
        assert_eq!(e.base_bytes, e.weights_bytes + e.kv_cache_bytes + overhead);
        assert_eq!(e.margin_bytes, e.base_bytes / 10);
        assert_eq!(e.total_bytes, e.base_bytes + e.margin_bytes);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn per_layer_kv_heads_and_explicit_key_lengths() {
        // Hybrid model: layers with 0 KV heads (recurrent/conv) have no KV cache.
        let g = Gguf::new()
            .kv("general.architecture", Kv::Str("lfm2"))
            .kv("lfm2.block_count", Kv::U32(4))
            .kv("lfm2.embedding_length", Kv::U32(512))
            .kv("lfm2.attention.head_count", Kv::U32(8))
            .kv("lfm2.attention.head_count_kv", Kv::ArrI32(vec![0, 2, 0, 2]))
            .kv("lfm2.attention.key_length", Kv::U32(128))
            .kv("lfm2.attention.value_length", Kv::U32(64))
            .kv("lfm2.expert_count", Kv::U32(32))
            .kv("lfm2.expert_used_count", Kv::U32(4))
            .tensor("t", &[256], 0);
        let dir = temp_dir("hybrid");
        let m = load(&[write_model(&dir, "h.gguf", &g)]).unwrap();
        assert_eq!(m.expert_count, Some(32));
        assert_eq!(m.head_count_kv, Some(2));
        let e = estimate(
            &m,
            EstimateOptions {
                context: Some(100),
                ..EstimateOptions::default()
            },
        );
        assert_eq!(e.kv_cache_bytes, 100 * 2 * 2 * (128 + 64) * 2);
        // No general.name and no file_type: fall back to the file stem and dominant tensor type.
        assert_eq!(m.name, "h");
        assert_eq!(m.quantization, "F32");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hashed_names_and_wrong_file_types_are_not_trusted() {
        let dir = temp_dir("names");
        let g = Gguf::new()
            .kv("general.architecture", Kv::Str("qwen3"))
            .kv(
                "general.name",
                Kv::Str("B968826D9C46Dd6066D109Eabc6255188De91218"),
            )
            .kv("general.size_label", Kv::Str("8.2B"))
            .kv("general.file_type", Kv::U32(32))
            .tensor("t", &[256, 4], 11);
        let named = load(&[write_model(&dir, "qwen3-8b-Q3_K_M.gguf", &g)]).unwrap();
        assert_eq!(named.name, "qwen3-8b-Q3_K_M");
        assert_eq!(named.quantization, "Q3_K");
        let blob = load(&[write_model(
            &dir,
            "sha256-4a188102020e9c9530b687fd6400f775",
            &g,
        )])
        .unwrap();
        assert_eq!(blob.name, "qwen3 8.2B");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mla_and_missing_attention_metadata() {
        let mla = Gguf::new()
            .kv("general.architecture", Kv::Str("deepseek2"))
            .kv("deepseek2.block_count", Kv::U32(2))
            .kv("deepseek2.attention.head_count", Kv::U32(16))
            .kv("deepseek2.attention.kv_lora_rank", Kv::U32(512))
            .kv("deepseek2.rope.dimension_count", Kv::U32(64))
            .tensor("t", &[32], 1);
        let none = Gguf::new()
            .kv("general.architecture", Kv::Str("mamba"))
            .kv("mamba.block_count", Kv::U32(2))
            .tensor("t", &[32], 1);
        let dir = temp_dir("mla");
        let m = load(&[write_model(&dir, "m.gguf", &mla)]).unwrap();
        let opts = EstimateOptions {
            context: Some(10),
            ..EstimateOptions::default()
        };
        assert_eq!(estimate(&m, opts).kv_cache_bytes, 2 * (512 + 64) * 10 * 2);
        let n = load(&[write_model(&dir, "n.gguf", &none)]).unwrap();
        let e = estimate(&n, opts);
        assert_eq!(e.kv_cache_bytes, 0);
        assert_eq!(e.notes.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn split_parts_are_summed_and_checked() {
        let dir = temp_dir("split");
        let p1 = write_model(
            &dir,
            "m-00001-of-00002.gguf",
            &Gguf::llama("M", 1)
                .kv("split.no", Kv::U32(0))
                .kv("split.count", Kv::U32(2))
                .kv("split.tensors.count", Kv::I32(3)),
        );
        let p2 = write_model(
            &dir,
            "m-00002-of-00002.gguf",
            &Gguf::new()
                .kv("split.no", Kv::U32(1))
                .kv("split.count", Kv::U32(2))
                .tensor("output.weight", &[256, 64], 12),
        );
        let m = load(&[p1.clone(), p2.clone()]).unwrap();
        assert_eq!(m.tensor_count, 3);
        assert_eq!(m.parts.len(), 2);
        assert_eq!(m.parameter_count, 2 * 256 * 64 + 256 * 256);
        let err = load(std::slice::from_ref(&p1)).unwrap_err();
        assert!(err.to_string().contains("2 parts"), "{err}");
        let err = load(&[p2, p1]).unwrap_err();
        assert!(matches!(err.kind, ModelErrorKind::Split(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn numbers_are_formatted_for_people() {
        assert_eq!(group_digits(8192), "8,192");
        assert_eq!(group_digits(131_072), "131,072");
        assert_eq!(group_digits(999), "999");
        assert_eq!(format_parameters(8_190_735_360), "8.2B");
        assert_eq!(format_parameters(494_000_000), "494M");
    }
}
