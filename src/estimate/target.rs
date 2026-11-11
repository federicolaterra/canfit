//! Target detection (spec §5, ADR-003): a `.gguf` file, a folder holding one model, or an app
//! name. Split models (`name-00001-of-00003.gguf`) resolve to all their parts.

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// What a command-line target refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A model: its files in order (one, or every part of a split model).
    Model(Vec<PathBuf>),
    /// Anything that is not a path to a model: an application name.
    App(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum TargetError {
    /// A `.gguf` path that does not exist.
    NotFound(PathBuf),
    /// A file that is not a GGUF model.
    NotGguf(PathBuf),
    /// A folder without GGUF files.
    EmptyFolder(PathBuf),
    /// A folder with several models; the fit check needs one.
    SeveralModels {
        dir: PathBuf,
        models: Vec<String>,
    },
    /// A split model with a missing part.
    MissingPart {
        part: PathBuf,
    },
    Io {
        path: PathBuf,
        detail: String,
    },
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetError::NotFound(p) => write!(f, "canfit can't find {}.", p.display()),
            TargetError::NotGguf(p) => write!(
                f,
                "{} is not a GGUF model file. canfit checks .gguf models (other formats arrive later).",
                p.display()
            ),
            TargetError::EmptyFolder(p) => {
                write!(f, "{} contains no .gguf model files.", p.display())
            }
            TargetError::SeveralModels { dir, models } => write!(
                f,
                "{} contains {} models ({}). Pass the model file you want to check.",
                dir.display(),
                models.len(),
                models.join(", ")
            ),
            TargetError::MissingPart { part } => write!(
                f,
                "{} is missing: a split model needs all its parts in the same folder.",
                part.display()
            ),
            TargetError::Io { path, detail } => {
                write!(f, "canfit can't read {}: {detail}", path.display())
            }
        }
    }
}

impl std::error::Error for TargetError {}

/// The parsed name of one part of a split model.
#[derive(Debug, PartialEq, Eq)]
pub struct SplitName<'a> {
    pub prefix: &'a str,
    /// 1-based part number.
    pub part: u32,
    pub count: u32,
}

/// Parses `prefix-00001-of-00003.gguf` (llama.cpp `llama_split_path`).
pub fn split_name(file_name: &str) -> Option<SplitName<'_>> {
    let stem = file_name
        .strip_suffix(".gguf")
        .or_else(|| file_name.strip_suffix(".GGUF"))?;
    let (rest, count) = stem.rsplit_once("-of-")?;
    let (prefix, part) = rest.rsplit_once('-')?;
    let digits = |s: &str| s.len() == 5 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(part) || !digits(count) || prefix.is_empty() {
        return None;
    }
    let (part, count): (u32, u32) = (part.parse().ok()?, count.parse().ok()?);
    (part >= 1 && part <= count).then_some(SplitName {
        prefix,
        part,
        count,
    })
}

pub(crate) fn has_gguf_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
}

fn starts_with_gguf_magic(path: &Path) -> Result<bool, TargetError> {
    let mut magic = [0u8; 4];
    let mut file = fs::File::open(path).map_err(|e| io_error(path, &e))?;
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == b"GGUF"),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(io_error(path, &e)),
    }
}

fn io_error(path: &Path, err: &std::io::Error) -> TargetError {
    TargetError::Io {
        path: path.to_path_buf(),
        detail: err.to_string(),
    }
}

/// Detects what `arg` refers to.
pub fn resolve(arg: &str) -> Result<Target, TargetError> {
    let path = Path::new(arg);
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => resolve_folder(path),
        Ok(_) if has_gguf_extension(path) || starts_with_gguf_magic(path)? => {
            Ok(Target::Model(model_parts(path)?))
        }
        Ok(_) => Err(TargetError::NotGguf(path.to_path_buf())),
        Err(_) if has_gguf_extension(path) => Err(TargetError::NotFound(path.to_path_buf())),
        Err(_) => Ok(Target::App(arg.to_string())),
    }
}

/// All parts of the model `path` belongs to; just `path` unless it is a split part.
pub(crate) fn model_parts(path: &Path) -> Result<Vec<PathBuf>, TargetError> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(split) = split_name(&file_name) else {
        return Ok(vec![path.to_path_buf()]);
    };
    let dir = path.parent().unwrap_or(Path::new(""));
    let ext = &file_name[file_name.len() - 4..];
    (1..=split.count)
        .map(|n| {
            let part = dir.join(format!(
                "{}-{n:05}-of-{:05}.{ext}",
                split.prefix, split.count
            ));
            if part.is_file() {
                Ok(part)
            } else {
                Err(TargetError::MissingPart { part })
            }
        })
        .collect()
}

/// A folder is a target when it holds exactly one model (one file or one split set).
fn resolve_folder(dir: &Path) -> Result<Target, TargetError> {
    let entries = fs::read_dir(dir).map_err(|e| io_error(dir, &e))?;
    let mut models: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error(dir, &e))?;
        let path = entry.path();
        if !path.is_file() || !has_gguf_extension(&path) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let key = match split_name(&name) {
            Some(split) => format!("{} ({} parts)", split.prefix, split.count),
            None => name.clone(),
        };
        if !models.iter().any(|(k, _)| *k == key) {
            models.push((key, path));
        }
    }
    models.sort();
    match models.len() {
        0 => Err(TargetError::EmptyFolder(dir.to_path_buf())),
        1 => Ok(Target::Model(model_parts(&models[0].1)?)),
        _ => Err(TargetError::SeveralModels {
            dir: dir.to_path_buf(),
            models: models.into_iter().map(|(k, _)| k).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("canfit-target-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn split_names_follow_llama_cpp() {
        assert_eq!(
            split_name("qwen-7b-q4_k_m-00002-of-00003.gguf"),
            Some(SplitName {
                prefix: "qwen-7b-q4_k_m",
                part: 2,
                count: 3
            })
        );
        assert_eq!(split_name("model.gguf"), None);
        assert_eq!(split_name("m-00004-of-00003.gguf"), None);
        assert_eq!(split_name("m-1-of-3.gguf"), None);
        assert_eq!(split_name("-00001-of-00003.gguf"), None);
    }

    #[test]
    fn detects_files_folders_and_apps() {
        let dir = temp_dir("detect");
        let model = dir.join("a.gguf");
        touch(&model, b"GGUF");
        assert_eq!(
            resolve(model.to_str().unwrap()),
            Ok(Target::Model(vec![model.clone()]))
        );
        // A folder with one model resolves to it; other files are ignored.
        touch(&dir.join("README.md"), b"hi");
        assert_eq!(
            resolve(dir.to_str().unwrap()),
            Ok(Target::Model(vec![model.clone()]))
        );
        // An extensionless blob (Ollama) is a model when it starts with the magic.
        let blob = dir.join("sha256-abc");
        touch(&blob, b"GGUF....");
        assert_eq!(
            resolve(blob.to_str().unwrap()),
            Ok(Target::Model(vec![blob.clone()]))
        );
        assert_eq!(
            resolve(dir.join("README.md").to_str().unwrap()),
            Err(TargetError::NotGguf(dir.join("README.md")))
        );
        assert_eq!(
            resolve(dir.join("missing.gguf").to_str().unwrap()),
            Err(TargetError::NotFound(dir.join("missing.gguf")))
        );
        assert_eq!(
            resolve("Docker Desktop"),
            Ok(Target::App("Docker Desktop".to_string()))
        );
        touch(&dir.join("b.gguf"), b"GGUF");
        assert!(matches!(
            resolve(dir.to_str().unwrap()),
            Err(TargetError::SeveralModels { .. })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn split_models_resolve_to_every_part() {
        let dir = temp_dir("split");
        let parts: Vec<PathBuf> = (1..=3)
            .map(|n| dir.join(format!("m-{n:05}-of-00003.gguf")))
            .collect();
        for p in &parts {
            touch(p, b"GGUF");
        }
        assert_eq!(
            resolve(parts[1].to_str().unwrap()),
            Ok(Target::Model(parts.clone()))
        );
        assert_eq!(
            resolve(dir.to_str().unwrap()),
            Ok(Target::Model(parts.clone()))
        );
        fs::remove_file(&parts[2]).unwrap();
        assert_eq!(
            resolve(parts[0].to_str().unwrap()),
            Err(TargetError::MissingPart {
                part: parts[2].clone()
            })
        );
        let empty = dir.join("empty");
        fs::create_dir(&empty).unwrap();
        assert_eq!(
            resolve(empty.to_str().unwrap()),
            Err(TargetError::EmptyFolder(empty.clone()))
        );
        let _ = fs::remove_dir_all(dir);
    }
}
