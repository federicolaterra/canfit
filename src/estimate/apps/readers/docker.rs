//! Docker Desktop: the memory of its Linux VM.
//!
//! Docker Desktop keeps its settings in `settings-store.json` (4.35 and later, keys such as
//! `MemoryMiB`) or `settings.json` (earlier versions, keys such as `memoryMiB`):
//!
//! - macOS: `~/Library/Group Containers/group.com.docker/`
//! - Windows: `%APPDATA%\Docker\`
//! - Linux: `~/.docker/desktop/`
//!
//! Keys are matched ignoring case so both generations read the same. On Windows with the WSL 2
//! backend (`wslEngineEnabled`), Docker's own memory setting is unused: the VM is WSL's, limited
//! by `memory=` under `[wsl2]` in `%UserProfile%\.wslconfig`, or by WSL's documented default of
//! half the machine's memory.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{display_path, read_text};
use crate::core::Provenance;
use crate::estimate::apps::{
    Confidence, DeclaredItem, Env, Os, ReadError, Reading, ReadingKind, parse_size,
};

const MIB: u64 = 1024 * 1024;

/// What a Docker Desktop settings file says about memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerSettings {
    /// `memoryMiB` / `MemoryMiB`, when present.
    pub memory_mib: Option<u64>,
    /// `wslEngineEnabled` / `WslEngineEnabled`, when present.
    pub wsl_engine: Option<bool>,
    /// The name of the memory key as written in the file.
    pub memory_key: Option<String>,
}

/// The value of `key` in a JSON object, ignoring the key's case.
fn get_ci<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Option<(&'a String, &'a Value)> {
    object.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))
}

/// Parses a Docker Desktop `settings.json` or `settings-store.json`.
pub fn parse_settings(text: &str) -> Result<DockerSettings, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON ({e})"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "the settings are not a JSON object".to_string())?;
    let memory = get_ci(object, "memoryMiB");
    let memory_mib = match memory {
        None => None,
        Some((_, v)) => Some(
            v.as_u64()
                .filter(|&m| m > 0)
                .ok_or_else(|| format!("memoryMiB is not a positive number ({v})"))?,
        ),
    };
    let wsl_engine = get_ci(object, "wslEngineEnabled").and_then(|(_, v)| v.as_bool());
    Ok(DockerSettings {
        memory_mib,
        wsl_engine,
        memory_key: memory.map(|(k, _)| k.clone()),
    })
}

/// The `memory=` value of the `[wsl2]` section of a `.wslconfig`, in bytes. `Ok(None)` when the
/// file does not set it.
pub fn parse_wslconfig(text: &str) -> Result<Option<u64>, String> {
    let mut in_wsl2 = false;
    let mut memory = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_wsl2 = section.trim().eq_ignore_ascii_case("wsl2");
            continue;
        }
        if !in_wsl2 {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("memory") {
            // Strip a trailing comment.
            let value = value.split(['#', ';']).next().unwrap_or("").trim();
            // WSL reads a size without a unit as bytes.
            let bytes = parse_size(value, 1)
                .filter(|&b| b > 0)
                .ok_or_else(|| format!("memory={value} under [wsl2] is not a size"))?;
            memory = Some(bytes);
        }
    }
    Ok(memory)
}

fn settings_dir(env: &Env) -> PathBuf {
    match env.os {
        Os::Macos => env
            .home
            .join("Library")
            .join("Group Containers")
            .join("group.com.docker"),
        Os::Windows => env.config.join("Docker"),
        Os::Linux => env.home.join(".docker").join("desktop"),
    }
}

/// The newest settings file that exists.
fn settings_file(env: &Env) -> Result<Option<(PathBuf, String)>, ReadError> {
    let dir = settings_dir(env);
    for name in ["settings-store.json", "settings.json"] {
        let path = dir.join(name);
        if let Some(text) = read_text(&path)? {
            return Ok(Some((path, text)));
        }
    }
    Ok(None)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Reads Docker Desktop's VM memory.
pub fn read(env: &Env) -> Result<Reading, ReadError> {
    let Some((path, text)) = settings_file(env)? else {
        return Err(ReadError::NotFound(format!(
            "Docker Desktop's settings are not in {} (is it installed and started once?)",
            display_path(&settings_dir(env), env)
        )));
    };
    let settings = parse_settings(&text).map_err(|detail| ReadError::Unreadable {
        path: path.clone(),
        detail,
    })?;
    let name = file_name(&path);
    let mut notes = Vec::new();
    if env.os == Os::Windows && settings.wsl_engine == Some(true) {
        return read_wsl(env, &name, notes);
    }
    let Some(mib) = settings.memory_mib else {
        return Err(ReadError::Unreadable {
            path,
            detail: "it has no memoryMiB setting".to_string(),
        });
    };
    if env.os == Os::Windows && settings.wsl_engine.is_none() {
        notes.push(
            "Docker Desktop's settings don't say whether the WSL 2 backend is on; canfit used \
             the Hyper-V memory setting."
                .to_string(),
        );
    }
    let key = settings
        .memory_key
        .unwrap_or_else(|| "memoryMiB".to_string());
    Ok(Reading {
        reader: "docker_desktop".to_string(),
        source: format!("Docker Desktop settings ({name})"),
        kind: ReadingKind::App,
        items: vec![DeclaredItem {
            name: "Docker Desktop VM".to_string(),
            declared_bytes: mib * MIB,
            declared_provenance: Provenance::ReadFromConfig,
            detail: format!("{key} = {mib} in {name}"),
            overhead_bytes: 0,
            overhead_detail: None,
            path: Some(path),
        }],
        confidence: Confidence::High,
        notes,
    })
}

/// Windows with the WSL 2 backend: the VM is WSL's, limited by `.wslconfig`.
fn read_wsl(env: &Env, settings_name: &str, mut notes: Vec<String>) -> Result<Reading, ReadError> {
    let path = env.home.join(".wslconfig");
    notes.push(format!(
        "Docker Desktop uses the WSL 2 backend ({settings_name}), so its memory is WSL's limit, \
         shared with every WSL distribution."
    ));
    let configured = match read_text(&path)? {
        Some(text) => parse_wslconfig(&text).map_err(|detail| ReadError::Unreadable {
            path: path.clone(),
            detail,
        })?,
        None => None,
    };
    let (bytes, provenance, detail, confidence) = match configured {
        Some(bytes) => (
            bytes,
            Provenance::ReadFromConfig,
            "memory under [wsl2] in .wslconfig".to_string(),
            Confidence::High,
        ),
        None => {
            notes.push(
                ".wslconfig sets no memory limit, so canfit assumed WSL's documented default: \
                 half of the machine's memory."
                    .to_string(),
            );
            (
                env.total_memory_bytes / 2,
                Provenance::Estimated,
                "WSL default: 50% of memory".to_string(),
                Confidence::Low,
            )
        }
    };
    Ok(Reading {
        reader: "docker_desktop".to_string(),
        source: "WSL 2 settings (.wslconfig)".to_string(),
        kind: ReadingKind::App,
        items: vec![DeclaredItem {
            name: "WSL 2 VM (Docker Desktop)".to_string(),
            declared_bytes: bytes,
            declared_provenance: provenance,
            detail,
            overhead_bytes: 0,
            overhead_detail: None,
            path: configured.map(|_| path),
        }],
        confidence,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::apps::readers::tests::{GIB, env};

    #[test]
    fn both_settings_generations_parse() {
        let old = parse_settings(r#"{"memoryMiB": 10240, "cpus": 7, "wslEngineEnabled": false}"#)
            .unwrap();
        assert_eq!(old.memory_mib, Some(10240));
        assert_eq!(old.wsl_engine, Some(false));
        let new = parse_settings(r#"{"MemoryMiB": 4096, "WslEngineEnabled": true}"#).unwrap();
        assert_eq!(new.memory_mib, Some(4096));
        assert_eq!(new.memory_key.as_deref(), Some("MemoryMiB"));
        assert_eq!(new.wsl_engine, Some(true));
        assert_eq!(parse_settings("{}").unwrap().memory_mib, None);
        for bad in [
            "",
            "[1]",
            "{\"memoryMiB\": \"lots\"}",
            "{\"memoryMiB\": 0}",
            "{nope",
        ] {
            assert!(parse_settings(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn wslconfig_memory() {
        let text = "# comment\n[wsl2]\nprocessors=4\nmemory = 6GB # six\nswap=0\n[experimental]\nmemory=1GB\n";
        assert_eq!(parse_wslconfig(text), Ok(Some(6 * GIB)));
        assert_eq!(parse_wslconfig("[wsl2]\nprocessors=2\n"), Ok(None));
        assert_eq!(parse_wslconfig("[boot]\nmemory=2GB\n"), Ok(None));
        assert_eq!(
            parse_wslconfig("[WSL2]\nMemory=2147483648\n"),
            Ok(Some(2 * GIB))
        );
        assert!(parse_wslconfig("[wsl2]\nmemory=plenty\n").is_err());
    }

    #[test]
    fn reads_macos_settings_json() {
        let r = read(&env(Os::Macos, "macos")).unwrap();
        assert_eq!(r.items[0].declared_bytes, 10 * GIB);
        assert_eq!(r.items[0].declared_provenance, Provenance::ReadFromConfig);
        assert_eq!(r.items[0].detail, "memoryMiB = 10240 in settings.json");
        assert_eq!(r.confidence, Confidence::High);
    }

    #[test]
    fn prefers_settings_store_and_reads_linux() {
        let r = read(&env(Os::Macos, "macos-new")).unwrap();
        assert_eq!(r.items[0].declared_bytes, 6 * GIB);
        assert_eq!(r.items[0].detail, "MemoryMiB = 6144 in settings-store.json");
        let r = read(&env(Os::Linux, "linux")).unwrap();
        assert_eq!(r.items[0].declared_bytes, 4 * GIB);
    }

    #[test]
    fn windows_wsl_uses_wslconfig_or_the_default() {
        let r = read(&env(Os::Windows, "windows")).unwrap();
        assert_eq!(r.items[0].declared_bytes, 6 * GIB);
        assert_eq!(r.items[0].declared_provenance, Provenance::ReadFromConfig);
        assert!(r.notes[0].contains("WSL 2 backend"));
        let r = read(&env(Os::Windows, "windows-wsl-default")).unwrap();
        assert_eq!(r.items[0].declared_bytes, 16 * GIB, "half of 32 GB");
        assert_eq!(r.items[0].declared_provenance, Provenance::Estimated);
        assert_eq!(r.confidence, Confidence::Low);
        let r = read(&env(Os::Windows, "windows-hyperv")).unwrap();
        assert_eq!(r.items[0].declared_bytes, 2 * GIB);
        assert_eq!(r.items[0].name, "Docker Desktop VM");
    }

    #[test]
    fn missing_and_broken_settings_fail_softly() {
        assert!(matches!(
            read(&env(Os::Macos, "empty")),
            Err(ReadError::NotFound(_))
        ));
        let err = read(&env(Os::Macos, "broken")).unwrap_err();
        assert!(matches!(err, ReadError::Unreadable { .. }), "{err:?}");
        assert!(err.to_string().contains("not valid JSON"), "{err}");
        let err = read(&env(Os::Linux, "broken")).unwrap_err();
        assert!(err.to_string().contains("no memoryMiB"), "{err}");
        let err = read(&env(Os::Windows, "broken")).unwrap_err();
        assert!(err.to_string().contains("memory=lots"), "{err}");
    }
}
