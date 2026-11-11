//! Finding the GGUF model a watched command loads, so the summary can put canfit's estimate next
//! to the measured peak (spec §8 estimate validation).
//!
//! Recognized: llama.cpp's `-m` / `--model` (also `--model=PATH`), and any argument that is an
//! existing `.gguf` file; the context from `-c` / `--ctx-size` (`0` = the model's own) and the K
//! cache type from `-ctk` / `--cache-type-k`. Anything else (Ollama model names, LM Studio, `-hf`
//! downloads) is not identified, and the summary says so.

use std::path::{Path, PathBuf};

use crate::estimate::model::CacheType;

/// A model found in the command's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArgs {
    pub path: PathBuf,
    /// The context asked for: `Some(0)` means "the model's own" (llama.cpp's `-c 0`), `None`
    /// that the command did not say.
    pub context: Option<u64>,
    /// The K cache type, when given and one canfit knows.
    pub cache_type: Option<CacheType>,
    /// Whether the weights are mapped from the file (llama.cpp's default), or loaded into the
    /// program's own memory (`--no-mmap`, `--load-mode none|mlock|dio`).
    pub mmap: bool,
}

fn value_of<'a>(args: &'a [String], i: usize, names: &[&str]) -> Option<(&'a str, usize)> {
    let arg = args[i].as_str();
    for name in names {
        if arg == *name {
            return args.get(i + 1).map(|v| (v.as_str(), 2));
        }
        if let Some(v) = arg.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            return Some((v, 1));
        }
    }
    None
}

fn is_gguf(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
}

/// The model in `args` (the command's arguments, without the program). `is_file` tells whether a
/// path is an existing file (injected for tests).
pub fn find_model(args: &[String], is_file: impl Fn(&Path) -> bool) -> Option<ModelArgs> {
    let mut path: Option<PathBuf> = None;
    let mut context = None;
    let mut cache_type = None;
    let mut mmap = true;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--no-mmap" || args[i] == "-nmm" {
            mmap = false;
            i += 1;
            continue;
        }
        if let Some((v, step)) = value_of(args, i, &["--load-mode", "-lm"]) {
            mmap = matches!(v.trim(), "auto" | "mmap" | "mmap+mlock");
            i += step;
            continue;
        }
        if let Some((v, step)) = value_of(args, i, &["--model", "-m"]) {
            if path.is_none() && is_file(Path::new(v)) {
                path = Some(PathBuf::from(v));
            }
            i += step;
            continue;
        }
        if let Some((v, step)) = value_of(args, i, &["--ctx-size", "-c"]) {
            context = v.trim().parse::<u64>().ok().or(context);
            i += step;
            continue;
        }
        if let Some((v, step)) = value_of(args, i, &["--cache-type-k", "-ctk"]) {
            cache_type = match v.trim().to_ascii_lowercase().as_str() {
                "f16" => Some(CacheType::F16),
                "q8_0" => Some(CacheType::Q8_0),
                "q4_0" => Some(CacheType::Q4_0),
                _ => cache_type,
            };
            i += step;
            continue;
        }
        let arg = &args[i];
        if path.is_none() && !arg.starts_with('-') && is_gguf(arg) && is_file(Path::new(arg)) {
            path = Some(PathBuf::from(arg));
        }
        i += 1;
    }
    path.map(|path| ModelArgs {
        path,
        context,
        cache_type,
        mmap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn exists(p: &Path) -> bool {
        p.to_string_lossy().contains("model")
    }

    #[test]
    fn llama_cpp_flags_are_read() {
        let found = find_model(
            &args(&[
                "-m",
                "models/model.gguf",
                "-c",
                "4096",
                "-ctk",
                "q8_0",
                "--port",
                "8080",
            ]),
            exists,
        )
        .unwrap();
        assert_eq!(found.path, PathBuf::from("models/model.gguf"));
        assert_eq!(found.context, Some(4096));
        assert_eq!(found.cache_type, Some(CacheType::Q8_0));
        assert!(found.mmap);

        let found =
            find_model(&args(&["--model=/m/model-q4.gguf", "--ctx-size=0"]), exists).unwrap();
        assert_eq!(found.path, PathBuf::from("/m/model-q4.gguf"));
        assert_eq!(found.context, Some(0));
        assert_eq!(found.cache_type, None);

        for (flags, mmap) in [
            (&["--no-mmap"][..], false),
            (&["-lm", "none"], false),
            (&["--load-mode=mlock"], false),
            (&["--load-mode", "mmap+mlock"], true),
            (&["-lm", "auto"], true),
        ] {
            let mut a = args(&["-m", "model.gguf"]);
            a.extend(args(flags));
            assert_eq!(find_model(&a, exists).unwrap().mmap, mmap, "{flags:?}");
        }
    }

    #[test]
    fn a_bare_gguf_argument_counts_when_it_exists() {
        let found = find_model(&args(&["run", "my-model.GGUF"]), exists).unwrap();
        assert_eq!(found.path, PathBuf::from("my-model.GGUF"));
        assert_eq!(found.context, None);
        assert_eq!(find_model(&args(&["run", "missing.gguf"]), exists), None);
    }

    #[test]
    fn nothing_identifiable() {
        assert_eq!(find_model(&args(&["run", "llama3.2"]), exists), None);
        assert_eq!(find_model(&args(&["-m"]), exists), None);
        assert_eq!(
            find_model(&args(&["-hf", "org/repo", "-c", "abc"]), exists),
            None
        );
        // `-m` pointing at something that is not a file: no model, and its value is skipped.
        assert_eq!(find_model(&args(&["-m", "nothing.gguf"]), exists), None);
    }
}
