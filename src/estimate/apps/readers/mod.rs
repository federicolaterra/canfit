//! Config readers: each finds an app's own settings files and reads the memory the app is
//! configured to use. Readers only read files: they never write, never run the app's tools and
//! never connect anywhere. Every parser is a pure function of the file's text.

pub mod docker;
pub mod json;
pub mod jvm;
pub mod vm;

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Env, Os, ReadError, Reading};

/// Settings files larger than this are not settings files: canfit refuses to read them.
const MAX_SETTINGS_BYTES: u64 = 4 * 1024 * 1024;

/// A list of paths per OS, as written in a profile.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerOs {
    #[serde(default)]
    pub macos: Vec<String>,
    #[serde(default)]
    pub windows: Vec<String>,
    #[serde(default)]
    pub linux: Vec<String>,
}

impl PerOs {
    pub fn for_os(&self, os: Os) -> &[String] {
        match os {
            Os::Macos => &self.macos,
            Os::Windows => &self.windows,
            Os::Linux => &self.linux,
        }
    }
}

/// One entry of a profile's `readers` list. The `kind` names the reader.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReaderSpec {
    /// Docker Desktop's VM memory (and, on Windows with WSL 2, the `.wslconfig` limit).
    DockerDesktop {},
    /// UTM virtual machines (macOS).
    Utm {},
    /// Parallels Desktop virtual machines (macOS).
    Parallels {},
    /// VMware Fusion (macOS) and Workstation (Windows, Linux) virtual machines.
    Vmware {},
    /// VirtualBox virtual machines.
    Virtualbox {},
    /// A JetBrains IDE (or Android Studio): `-Xmx` from the user's `.vmoptions`, else the
    /// shipped one, plus an estimated non-heap overhead.
    Jetbrains {
        /// Install folder or `.app` names, matched as a prefix ignoring case.
        install_names: Vec<String>,
        /// Settings folder prefixes, e.g. `IntelliJIdea` and `IdeaIC` for `IntelliJIdea2024.1`.
        data_dirs: Vec<String>,
        /// The launcher name the `.vmoptions` file is named after, e.g. `idea`.
        launcher: String,
        /// The settings folder's vendor (`JetBrains`, `Google` for Android Studio).
        #[serde(default = "jetbrains_vendor")]
        vendor: String,
    },
    /// Any JVM app: `-Xmx` from the first of `paths` that exists (`.vmoptions` or `.ini`).
    JvmOptions {
        #[serde(default)]
        paths: PerOs,
    },
    /// A number in a JSON settings file: `key` is a dotted path, matched ignoring case.
    Json {
        key: String,
        /// `bytes`, `KiB`, `MiB` or `GiB`.
        unit: String,
        /// What the number is, e.g. "VM memory".
        label: String,
        #[serde(default)]
        paths: PerOs,
    },
}

fn jetbrains_vendor() -> String {
    "JetBrains".to_string()
}

impl ReaderSpec {
    /// The reader's name, as written in profiles and JSON.
    #[cfg(test)]
    pub fn kind(&self) -> &'static str {
        match self {
            ReaderSpec::DockerDesktop {} => "docker_desktop",
            ReaderSpec::Utm {} => "utm",
            ReaderSpec::Parallels {} => "parallels",
            ReaderSpec::Vmware {} => "vmware",
            ReaderSpec::Virtualbox {} => "virtualbox",
            ReaderSpec::Jetbrains { .. } => "jetbrains",
            ReaderSpec::JvmOptions { .. } => "jvm_options",
            ReaderSpec::Json { .. } => "json",
        }
    }

    /// Checks what can be checked without the files (units, empty lists).
    pub fn validate(&self) -> Result<(), String> {
        match self {
            ReaderSpec::Json { unit, key, .. } => {
                unit_factor(unit).ok_or_else(|| format!("unknown unit \"{unit}\""))?;
                if key.trim().is_empty() {
                    return Err("the json reader needs a key".to_string());
                }
                Ok(())
            }
            ReaderSpec::Jetbrains {
                install_names,
                data_dirs,
                launcher,
                ..
            } => {
                if install_names.is_empty() || data_dirs.is_empty() || launcher.is_empty() {
                    return Err(
                        "the jetbrains reader needs install_names, data_dirs and launcher"
                            .to_string(),
                    );
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Runs the reader for the app called `app`.
    pub fn read(&self, app: &str, env: &Env) -> Result<Reading, ReadError> {
        match self {
            ReaderSpec::DockerDesktop {} => docker::read(env),
            ReaderSpec::Utm {} => vm::read_utm(env),
            ReaderSpec::Parallels {} => vm::read_parallels(env),
            ReaderSpec::Vmware {} => vm::read_vmware(env),
            ReaderSpec::Virtualbox {} => vm::read_virtualbox(env),
            ReaderSpec::Jetbrains {
                install_names,
                data_dirs,
                launcher,
                vendor,
            } => jvm::read_jetbrains(
                app,
                &jvm::JetBrainsSpec {
                    install_names,
                    data_dirs,
                    launcher,
                    vendor,
                },
                env,
            ),
            ReaderSpec::JvmOptions { paths } => jvm::read_options_files(app, paths, env),
            ReaderSpec::Json {
                key,
                unit,
                label,
                paths,
            } => json::read(key, unit, label, paths, env),
        }
    }
}

/// Bytes per unit of a `json` reader.
pub fn unit_factor(unit: &str) -> Option<u64> {
    match unit.to_ascii_lowercase().as_str() {
        "bytes" | "b" => Some(1),
        "kib" | "kb" => Some(1024),
        "mib" | "mb" => Some(1024 * 1024),
        "gib" | "gb" => Some(1024 * 1024 * 1024),
        _ => None,
    }
}

/// Reads a settings file: `Ok(None)` when it does not exist, an error when it exists but can't
/// be read (or is far too big to be a settings file).
pub fn read_text(path: &Path) -> Result<Option<String>, ReadError> {
    let unreadable = |detail: String| ReadError::Unreadable {
        path: path.to_path_buf(),
        detail,
    };
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e.to_string())),
        Ok(meta) if meta.is_dir() => return Err(unreadable("it is a folder".to_string())),
        Ok(meta) if meta.len() > MAX_SETTINGS_BYTES => {
            return Err(unreadable(format!(
                "the file is {} bytes, too big for a settings file",
                meta.len()
            )));
        }
        Ok(_) => {}
    }
    let bytes = std::fs::read(path).map_err(|e| unreadable(e.to_string()))?;
    // Some Windows tools write UTF-16 or a UTF-8 byte-order mark.
    Ok(Some(decode(&bytes)))
}

/// Decodes UTF-8 (with or without a byte-order mark) or UTF-16 with a byte-order mark.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |little: bool| {
        let (pairs, _) = bytes[2..].as_chunks::<2>();
        let units: Vec<u16> = pairs
            .iter()
            .map(|&pair| {
                if little {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, ..] => utf16(true),
        [0xFE, 0xFF, ..] => utf16(false),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The entries of a folder, sorted by name; empty when it can't be listed.
pub fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

/// Whether `path`'s file name ends with `ext` (with the dot), ignoring case.
pub fn has_ext(path: &Path, ext: &str) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|n| n.ends_with(&ext.to_ascii_lowercase()))
}

/// The file name without its extension.
pub fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Shortens a path under the home folder to `~/…` for display.
pub fn display_path(path: &Path, env: &Env) -> String {
    match path.strip_prefix(&env.home) {
        Ok(rest) if env.os != Os::Windows => format!("~/{}", rest.display()),
        Ok(rest) => format!("%USERPROFILE%\\{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fixture home folder under `tests/fixtures/apps/`.
    pub fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("apps")
            .join(name)
    }

    pub const GIB: u64 = 1024 * 1024 * 1024;
    pub const MIB: u64 = 1024 * 1024;

    pub fn env(os: Os, name: &str) -> Env {
        Env::rooted(os, &fixture(name), 32 * GIB)
    }

    #[test]
    fn decodes_utf8_bom_and_utf16() {
        assert_eq!(decode(b"\xEF\xBB\xBFhi"), "hi");
        assert_eq!(decode(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
        assert_eq!(decode(&[0xFE, 0xFF, 0, b'h', 0, b'i']), "hi");
        assert_eq!(decode(b"plain"), "plain");
    }

    #[test]
    fn missing_files_are_none_and_folders_are_errors() {
        let dir = fixture("macos");
        assert_eq!(read_text(&dir.join("no-such-file")), Ok(None));
        assert!(matches!(read_text(&dir), Err(ReadError::Unreadable { .. })));
    }

    #[test]
    fn reader_specs_parse_from_toml_and_validate() {
        #[derive(Deserialize)]
        struct Wrap {
            readers: Vec<ReaderSpec>,
        }
        let w: Wrap = toml::from_str(
            r#"readers = [
                { kind = "docker_desktop" },
                { kind = "json", key = "vm.memoryMiB", unit = "MiB", label = "VM memory",
                  paths = { macos = ["{home}/x.json"] } },
                { kind = "jetbrains", install_names = ["PyCharm"], data_dirs = ["PyCharm"],
                  launcher = "pycharm" },
            ]"#,
        )
        .unwrap();
        assert_eq!(w.readers[0], ReaderSpec::DockerDesktop {});
        assert_eq!(w.readers[1].kind(), "json");
        assert!(w.readers.iter().all(|r| r.validate().is_ok()));
        let bad = ReaderSpec::Json {
            key: "x".to_string(),
            unit: "parsecs".to_string(),
            label: "x".to_string(),
            paths: PerOs::default(),
        };
        assert!(bad.validate().is_err());
        assert!(toml::from_str::<Wrap>(r#"readers = [{ kind = "telepathy" }]"#).is_err());
        assert!(toml::from_str::<Wrap>(r#"readers = [{ kind = "utm", extra = 1 }]"#).is_err());
    }
}
