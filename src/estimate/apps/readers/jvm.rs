//! JVM apps: the maximum heap (`-Xmx`) plus an estimated non-heap overhead.
//!
//! JetBrains IDEs ship `product-info.json` next to their launcher (macOS
//! `<App>.app/Contents/Resources/`, Windows and Linux the install folder). It names the settings
//! folder (`dataDirectoryName`, e.g. `IntelliJIdea2024.1`) and the shipped `.vmoptions` file
//! (`launch[].vmOptionsFilePath`). The user's own options live in the vendor's settings folder,
//! in a file with the same name:
//!
//! - macOS: `~/Library/Application Support/JetBrains/<dataDirectoryName>/idea.vmoptions`
//! - Windows: `%APPDATA%\JetBrains\<dataDirectoryName>\idea64.exe.vmoptions`
//! - Linux: `~/.config/JetBrains/<dataDirectoryName>/idea64.vmoptions`
//!
//! The user's `-Xmx` wins over the shipped one. The JVM needs memory beyond the heap — the code
//! cache (`-XX:ReservedCodeCacheSize`), class metadata, thread stacks, GC structures and native
//! buffers — which canfit estimates as the code cache plus [`NON_HEAP_EXTRA_BYTES`].

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{PerOs, display_path, has_ext, list_dir, read_text};
use crate::core::{Provenance, format_bytes};
use crate::estimate::apps::{
    Confidence, DeclaredItem, Env, Os, ReadError, Reading, ReadingKind, parse_size,
};

const MIB: u64 = 1024 * 1024;

/// Non-heap memory on top of the code cache: class metadata, thread stacks, GC structures and
/// native buffers of a large desktop JVM app such as an IDE (ADR-020). Conservative.
pub const NON_HEAP_EXTRA_BYTES: u64 = 1024 * MIB;

/// Heap assumed when no options file sets `-Xmx` (current JetBrains IDEs ship 2 GB).
pub const DEFAULT_HEAP_BYTES: u64 = 2048 * MIB;

/// Code cache assumed when no options file sets it (JetBrains IDEs ship 512 MB).
pub const DEFAULT_CODE_CACHE_BYTES: u64 = 512 * MIB;

/// The memory options of one `.vmoptions` or `.ini` file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JvmOptions {
    pub max_heap_bytes: Option<u64>,
    pub code_cache_bytes: Option<u64>,
}

impl JvmOptions {
    /// Options in `self` win over `base` (the user's file over the shipped one).
    fn over(self, base: JvmOptions) -> JvmOptions {
        JvmOptions {
            max_heap_bytes: self.max_heap_bytes.or(base.max_heap_bytes),
            code_cache_bytes: self.code_cache_bytes.or(base.code_cache_bytes),
        }
    }
}

/// Parses JVM options, one per line; the last occurrence wins, as in the JVM.
pub fn parse_options(text: &str) -> Result<JvmOptions, String> {
    let mut options = JvmOptions::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("-Xmx") {
            // The JVM reads a size without a unit as bytes.
            options.max_heap_bytes = Some(
                parse_size(value, 1)
                    .filter(|&b| b > 0)
                    .ok_or_else(|| format!("-Xmx{value} is not a size"))?,
            );
        } else if let Some(value) = line.strip_prefix("-XX:ReservedCodeCacheSize=") {
            options.code_cache_bytes = Some(
                parse_size(value, 1)
                    .ok_or_else(|| format!("-XX:ReservedCodeCacheSize={value} is not a size"))?,
            );
        }
    }
    Ok(options)
}

/// What `product-info.json` says about an installed IDE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductInfo {
    pub name: Option<String>,
    pub version: Option<String>,
    pub data_directory_name: String,
    /// The shipped options file, relative to the folder holding `product-info.json`.
    pub vm_options_file: Option<String>,
}

/// Parses `product-info.json`, picking the launch entry for `os`.
pub fn parse_product_info(text: &str, os: Os) -> Result<ProductInfo, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON ({e})"))?;
    let data_directory_name = value
        .get("dataDirectoryName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "it has no dataDirectoryName".to_string())?
        .to_string();
    let os_name = match os {
        Os::Macos => "macos",
        Os::Windows => "windows",
        Os::Linux => "linux",
    };
    let launches = value
        .get("launch")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let launch = launches
        .iter()
        .find(|l| {
            l.get("os")
                .and_then(Value::as_str)
                .is_some_and(|o| o.eq_ignore_ascii_case(os_name))
        })
        .or_else(|| launches.first());
    Ok(ProductInfo {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        version: value
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        data_directory_name,
        vm_options_file: launch
            .and_then(|l| l.get("vmOptionsFilePath"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Numbers in a version string, for ordering (`IntelliJIdea2024.1` → [2024, 1]).
fn version_key(s: &str) -> Vec<u64> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect()
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// The profile's description of a JetBrains IDE.
pub struct JetBrainsSpec<'a> {
    pub install_names: &'a [String],
    pub data_dirs: &'a [String],
    pub launcher: &'a str,
    pub vendor: &'a str,
}

impl JetBrainsSpec<'_> {
    /// Whether a settings folder name belongs to this IDE (`IntelliJIdea2024.1`).
    fn owns_data_dir(&self, name: &str) -> bool {
        self.data_dirs.iter().any(|prefix| {
            starts_with_ci(name, prefix)
                && name[prefix.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
        })
    }

    fn matches_install(&self, folder: &str) -> bool {
        self.install_names
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.launcher))
            .any(|n| starts_with_ci(folder, n))
    }
}

/// One installed copy found on disk.
#[derive(Debug, Clone)]
struct Install {
    info: ProductInfo,
    /// The folder holding `product-info.json`.
    dir: PathBuf,
}

/// Every installed copy of the IDE in the app folders, newest first.
fn find_installs(spec: &JetBrainsSpec<'_>, env: &Env) -> Vec<Install> {
    let mut found: Vec<Install> = Vec::new();
    for root in &env.app_dirs {
        for entry in list_dir(root) {
            let name = entry
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !entry.is_dir() || !spec.matches_install(&name) {
                continue;
            }
            // macOS bundle, a plain install folder, or Toolbox's channel/build folders below.
            let mut candidates = vec![entry.join("Contents").join("Resources"), entry.clone()];
            for child in list_dir(&entry).into_iter().filter(|p| p.is_dir()) {
                candidates.extend(list_dir(&child).into_iter().filter(|p| p.is_dir()));
                candidates.push(child);
            }
            for dir in candidates {
                let file = dir.join("product-info.json");
                if found.iter().any(|f| f.dir == dir) {
                    continue;
                }
                let Ok(Some(text)) = read_text(&file) else {
                    continue;
                };
                let Ok(info) = parse_product_info(&text, env.os) else {
                    continue;
                };
                if spec.owns_data_dir(&info.data_directory_name) {
                    found.push(Install { info, dir });
                }
            }
        }
    }
    found.sort_by(|a, b| {
        version_key(&b.info.data_directory_name).cmp(&version_key(&a.info.data_directory_name))
    });
    found
}

/// The newest settings folder of the IDE, when canfit finds no install.
fn newest_settings_dir(spec: &JetBrainsSpec<'_>, vendor_dir: &Path) -> Option<PathBuf> {
    list_dir(vendor_dir)
        .into_iter()
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| spec.owns_data_dir(&n.to_string_lossy()))
        })
        .max_by_key(|p| version_key(&p.file_name().unwrap_or_default().to_string_lossy()))
}

/// The user's options file in `dir`: `name` when given, else any `<launcher>*.vmoptions`.
fn user_options_file(dir: &Path, name: Option<&str>, launcher: &str) -> Option<PathBuf> {
    if let Some(name) = name {
        return Some(dir.join(name));
    }
    list_dir(dir).into_iter().find(|p| {
        has_ext(p, ".vmoptions")
            && p.file_name()
                .is_some_and(|n| starts_with_ci(&n.to_string_lossy(), launcher))
    })
}

fn read_options(path: &Path) -> Result<Option<JvmOptions>, ReadError> {
    match read_text(path)? {
        None => Ok(None),
        Some(text) => parse_options(&text)
            .map(Some)
            .map_err(|detail| ReadError::Unreadable {
                path: path.to_path_buf(),
                detail,
            }),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Builds the reading from the user's and the shipped options.
fn jvm_reading(
    reader: &str,
    source: String,
    user: Option<(PathBuf, JvmOptions)>,
    shipped: Option<(PathBuf, JvmOptions)>,
    mut notes: Vec<String>,
) -> Reading {
    let user_options = user.as_ref().map(|(_, o)| *o).unwrap_or_default();
    let shipped_options = shipped.as_ref().map(|(_, o)| *o).unwrap_or_default();
    let effective = user_options.over(shipped_options);
    let heap_from = if user_options.max_heap_bytes.is_some() {
        user.as_ref().map(|(p, _)| p.clone())
    } else if shipped_options.max_heap_bytes.is_some() {
        shipped.as_ref().map(|(p, _)| p.clone())
    } else {
        None
    };
    let (heap, provenance, detail, confidence) = match (effective.max_heap_bytes, &heap_from) {
        (Some(heap), Some(path)) => (
            heap,
            Provenance::ReadFromConfig,
            format!("-Xmx in {}", file_name(path)),
            Confidence::Medium,
        ),
        _ => {
            notes.push(format!(
                "No options file sets -Xmx, so canfit assumed {} (estimated).",
                format_bytes(DEFAULT_HEAP_BYTES)
            ));
            (
                DEFAULT_HEAP_BYTES,
                Provenance::Estimated,
                "assumed default heap".to_string(),
                Confidence::Low,
            )
        }
    };
    let code_cache = effective
        .code_cache_bytes
        .unwrap_or(DEFAULT_CODE_CACHE_BYTES);
    let overhead = code_cache + NON_HEAP_EXTRA_BYTES;
    notes.push(
        "Java apps use memory beyond the heap (code cache, class data, threads); canfit adds an \
         estimate for it."
            .to_string(),
    );
    Reading {
        reader: reader.to_string(),
        source,
        kind: ReadingKind::App,
        items: vec![DeclaredItem {
            name: "Java heap (-Xmx)".to_string(),
            declared_bytes: heap,
            declared_provenance: provenance,
            detail,
            overhead_bytes: overhead,
            overhead_detail: Some(format!(
                "code cache {} + {} class data, threads and native memory",
                format_bytes(code_cache),
                format_bytes(NON_HEAP_EXTRA_BYTES)
            )),
            path: heap_from,
        }],
        confidence,
        notes,
    }
}

pub fn read_jetbrains(
    app: &str,
    spec: &JetBrainsSpec<'_>,
    env: &Env,
) -> Result<Reading, ReadError> {
    let vendor_dir = env.config.join(spec.vendor);
    let installs = find_installs(spec, env);
    let mut notes = Vec::new();
    let (settings_dir, user_name, shipped_path, version) = match installs.first() {
        Some(install) => {
            let shipped = install
                .info
                .vm_options_file
                .as_deref()
                .map(|rel| install.dir.join(rel));
            let name = shipped.as_deref().map(file_name);
            (
                Some(vendor_dir.join(&install.info.data_directory_name)),
                name,
                shipped,
                install.info.version.clone(),
            )
        }
        None => (newest_settings_dir(spec, &vendor_dir), None, None, None),
    };
    if installs.len() > 1 {
        notes.push(format!(
            "Several versions of {app} are installed; canfit read the newest ({}).",
            installs[0].info.data_directory_name
        ));
    }
    let Some(settings_dir) = settings_dir else {
        return Err(ReadError::NotFound(format!(
            "canfit found neither an installed {app} nor its settings in {}",
            display_path(&vendor_dir, env)
        )));
    };
    let user = match user_options_file(&settings_dir, user_name.as_deref(), spec.launcher) {
        Some(path) => read_options(&path)?.map(|o| (path, o)),
        None => None,
    };
    let shipped = match shipped_path {
        Some(path) => read_options(&path)?.map(|o| (path, o)),
        None => None,
    };
    let what = match &version {
        Some(v) => format!("{app} {v} options"),
        None => format!("{app} options"),
    };
    let files: Vec<String> = user
        .iter()
        .map(|(p, _)| format!("yours: {}", file_name(p)))
        .chain(
            shipped
                .iter()
                .map(|(p, _)| format!("shipped: {}", file_name(p))),
        )
        .collect();
    let source = if files.is_empty() {
        what
    } else {
        format!("{what} ({})", files.join(", "))
    };
    Ok(jvm_reading("jetbrains", source, user, shipped, notes))
}

/// A generic JVM app: the first options file of `paths` that exists.
pub fn read_options_files(app: &str, paths: &PerOs, env: &Env) -> Result<Reading, ReadError> {
    let templates = paths.for_os(env.os);
    if templates.is_empty() {
        return Err(ReadError::UnsupportedOs);
    }
    for template in templates {
        let path = env.expand(template);
        if let Some(options) = read_options(&path)? {
            let source = format!("{app} options ({})", file_name(&path));
            return Ok(jvm_reading(
                "jvm_options",
                source,
                Some((path, options)),
                None,
                Vec::new(),
            ));
        }
    }
    Err(ReadError::NotFound(format!(
        "no options file at {}",
        display_path(&env.expand(&templates[0]), env)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::apps::readers::tests::{GIB, MIB as M, env};

    fn idea() -> (Vec<String>, Vec<String>) {
        (
            vec!["IntelliJ IDEA".to_string(), "intellij-idea".to_string()],
            vec!["IntelliJIdea".to_string(), "IdeaIC".to_string()],
        )
    }

    fn read_idea(e: &Env) -> Result<Reading, ReadError> {
        let (install_names, data_dirs) = idea();
        read_jetbrains(
            "IntelliJ IDEA",
            &JetBrainsSpec {
                install_names: &install_names,
                data_dirs: &data_dirs,
                launcher: "idea",
                vendor: "JetBrains",
            },
            e,
        )
    }

    #[test]
    fn options_last_value_wins() {
        let o = parse_options(
            "-Xms128m\n-Xmx750m\n# -Xmx99g\n-XX:ReservedCodeCacheSize=512m\n-Xmx2g\n",
        )
        .unwrap();
        assert_eq!(o.max_heap_bytes, Some(2 * GIB));
        assert_eq!(o.code_cache_bytes, Some(512 * M));
        assert_eq!(parse_options("-ea\n").unwrap(), JvmOptions::default());
        assert_eq!(
            parse_options("-Xmx1073741824").unwrap().max_heap_bytes,
            Some(GIB)
        );
        assert!(parse_options("-Xmxlots").is_err());
    }

    #[test]
    fn product_info_picks_this_os_launch() {
        let text = r#"{"name":"IntelliJ IDEA","version":"2024.1","dataDirectoryName":"IntelliJIdea2024.1",
            "launch":[{"os":"Windows","vmOptionsFilePath":"bin/idea64.exe.vmoptions"},
                      {"os":"Linux","vmOptionsFilePath":"bin/idea64.vmoptions"}]}"#;
        let info = parse_product_info(text, Os::Linux).unwrap();
        assert_eq!(info.data_directory_name, "IntelliJIdea2024.1");
        assert_eq!(
            info.vm_options_file.as_deref(),
            Some("bin/idea64.vmoptions")
        );
        assert!(parse_product_info("{}", Os::Linux).is_err());
        assert!(parse_product_info("nope", Os::Linux).is_err());
    }

    #[test]
    fn macos_user_options_win_over_the_shipped_ones() {
        let r = read_idea(&env(Os::Macos, "macos")).unwrap();
        let item = &r.items[0];
        assert_eq!(
            item.declared_bytes,
            2048 * M,
            "user -Xmx2048m over shipped 750m"
        );
        assert_eq!(item.declared_provenance, Provenance::ReadFromConfig);
        assert_eq!(item.detail, "-Xmx in idea.vmoptions");
        assert!(
            item.path
                .as_ref()
                .unwrap()
                .ends_with("IntelliJIdea2020.3/idea.vmoptions")
        );
        assert_eq!(item.overhead_bytes, 512 * M + NON_HEAP_EXTRA_BYTES);
        assert_eq!(r.confidence, Confidence::Medium);
        assert_eq!(
            r.source,
            "IntelliJ IDEA 2020.3.3 options (yours: idea.vmoptions, shipped: idea.vmoptions)"
        );
        assert!(
            r.notes.iter().all(|n| !n.contains("Several versions")),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn windows_and_linux_layouts() {
        let r = read_idea(&env(Os::Windows, "windows")).unwrap();
        assert_eq!(
            r.items[0].declared_bytes,
            4 * GIB,
            "user idea64.exe.vmoptions"
        );
        // Linux Toolbox install: no user file, the shipped -Xmx2048m applies.
        let r = read_idea(&env(Os::Linux, "linux")).unwrap();
        assert_eq!(r.items[0].declared_bytes, 2 * GIB);
        assert!(
            r.items[0]
                .path
                .as_ref()
                .unwrap()
                .ends_with("bin/idea64.vmoptions")
        );
        assert_eq!(r.items[0].overhead_bytes, 512 * M + NON_HEAP_EXTRA_BYTES);
    }

    #[test]
    fn settings_without_an_install_and_defaults() {
        // macos-new: no install, settings folders 2023.3 and 2024.2; the newest is read.
        let r = read_idea(&env(Os::Macos, "macos-new")).unwrap();
        assert_eq!(r.items[0].declared_bytes, 3 * GIB);
        // broken (Linux): a settings folder without -Xmx → the assumed default, low confidence.
        let r = read_idea(&env(Os::Linux, "broken")).unwrap();
        assert_eq!(r.items[0].declared_bytes, DEFAULT_HEAP_BYTES);
        assert_eq!(r.items[0].declared_provenance, Provenance::Estimated);
        assert_eq!(r.confidence, Confidence::Low);
        // broken (macOS): a malformed -Xmx is an error, not a guess.
        assert!(matches!(
            read_idea(&env(Os::Macos, "broken")),
            Err(ReadError::Unreadable { .. })
        ));
        assert!(matches!(
            read_idea(&env(Os::Macos, "empty")),
            Err(ReadError::NotFound(_))
        ));
    }

    #[test]
    fn generic_options_files() {
        let paths = PerOs {
            linux: vec!["{home}/dbeaver/dbeaver.ini".to_string()],
            ..PerOs::default()
        };
        let r = read_options_files("DBeaver", &paths, &env(Os::Linux, "linux")).unwrap();
        assert_eq!(r.items[0].declared_bytes, GIB);
        assert_eq!(r.reader, "jvm_options");
        assert!(matches!(
            read_options_files("DBeaver", &paths, &env(Os::Macos, "linux")),
            Err(ReadError::UnsupportedOs)
        ));
    }
}
