//! App profiles: the embedded `profiles.toml` plus the user's `apps.toml` (ADR-020).
//!
//! The user file lives next to `config.toml` (or at `$CANFIT_APPS`) and has the same layout. A
//! user profile whose name matches an embedded one replaces it; others are added. A user file
//! that exists but can't be read is an error, never silently ignored.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::Os;
use super::readers::ReaderSpec;

const EMBEDDED: &str = include_str!("profiles.toml");

/// Supported version of the profiles file layout.
pub const PROFILES_SCHEMA_VERSION: u32 = 1;

/// Environment variable that points canfit at another user profiles file.
pub const APPS_ENV: &str = "CANFIT_APPS";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    schema_version: u32,
    #[serde(rename = "app", default)]
    apps: Vec<Profile>,
}

/// How canfit finds the running app on one OS.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matchers {
    /// macOS `.app` bundle names (without `.app`).
    #[serde(default)]
    pub bundles: Vec<String>,
    /// Process or app group names (`.exe` optional).
    #[serde(default)]
    pub names: Vec<String>,
    /// Shared processes that may hold the app's memory but can't be attributed to it.
    #[serde(default)]
    pub shared: Vec<String>,
}

/// One app canfit knows how to read the settings of.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// The OSes the app runs on; empty means all three.
    #[serde(default)]
    pub os: Vec<String>,
    pub readers: Vec<ReaderSpec>,
    #[serde(default)]
    pub macos: Matchers,
    #[serde(default)]
    pub windows: Matchers,
    #[serde(default)]
    pub linux: Matchers,
}

impl Profile {
    pub fn runs_on(&self, os: Os) -> bool {
        self.os.is_empty() || self.os.iter().any(|o| o == os.key())
    }

    pub fn matchers(&self, os: Os) -> &Matchers {
        match os {
            Os::Macos => &self.macos,
            Os::Windows => &self.windows,
            Os::Linux => &self.linux,
        }
    }

    /// The profile's name and aliases.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.aliases.iter().map(String::as_str))
    }

    fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("an [[app]] has an empty name".to_string());
        }
        if self.readers.is_empty() {
            return Err(format!("\"{}\" has no readers", self.name));
        }
        for os in &self.os {
            if Os::from_key(os).is_none() {
                return Err(format!(
                    "\"{}\": unknown OS \"{os}\" (use macos, windows or linux)",
                    self.name
                ));
            }
        }
        for reader in &self.readers {
            reader
                .validate()
                .map_err(|e| format!("\"{}\": {e}", self.name))?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProfilesError {
    pub path: Option<PathBuf>,
    pub detail: String,
}

impl fmt::Display for ProfilesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(
                f,
                "canfit could not read your app profiles file {}: {}",
                path.display(),
                self.detail.trim()
            ),
            None => write!(
                f,
                "canfit's built-in app profiles are invalid: {}",
                self.detail
            ),
        }
    }
}

fn parse(text: &str) -> Result<Vec<Profile>, String> {
    let file: ProfileFile = toml::from_str(text).map_err(|e| e.to_string())?;
    if file.schema_version != PROFILES_SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {} (this canfit reads {PROFILES_SCHEMA_VERSION})",
            file.schema_version
        ));
    }
    for profile in &file.apps {
        profile.validate()?;
    }
    Ok(file.apps)
}

/// The profiles built into canfit.
pub fn embedded() -> Vec<Profile> {
    parse(EMBEDDED).expect("embedded profiles parse (covered by tests)")
}

/// Merges user profiles over `base`: same name (ignoring case) replaces, others are added.
pub fn merge(mut base: Vec<Profile>, user: Vec<Profile>) -> Vec<Profile> {
    for profile in user {
        match base
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&profile.name))
        {
            Some(existing) => *existing = profile,
            None => base.push(profile),
        }
    }
    base
}

/// The user profiles file: `$CANFIT_APPS`, else `apps.toml` next to the config file.
pub fn user_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(APPS_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    crate::advisor::config::path().and_then(|p| p.parent().map(|d| d.join("apps.toml")))
}

/// Reads the user's profiles from `path`; a missing file is none.
pub fn load_user(path: &Path) -> Result<Vec<Profile>, ProfilesError> {
    let error = |detail: String| ProfilesError {
        path: Some(path.to_path_buf()),
        detail,
    };
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(error),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(error(e.to_string())),
    }
}

/// Every profile: embedded, then the user's.
pub fn load() -> Result<Vec<Profile>, ProfilesError> {
    let user = match user_path() {
        Some(path) => load_user(&path)?,
        None => Vec::new(),
    };
    Ok(merge(embedded(), user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_profiles_parse_and_are_unique() {
        let profiles = embedded();
        assert!(profiles.len() >= 15);
        for p in &profiles {
            assert!(!p.readers.is_empty(), "{}", p.name);
            let others = profiles
                .iter()
                .filter(|q| q.name.eq_ignore_ascii_case(&p.name));
            assert_eq!(others.count(), 1, "{}", p.name);
        }
        for name in [
            "Docker Desktop",
            "UTM",
            "IntelliJ IDEA",
            "VirtualBox",
            "Android Studio",
        ] {
            assert!(profiles.iter().any(|p| p.name == name), "{name}");
        }
        // Every OS-specific alias stays unique on each OS.
        for os in [Os::Macos, Os::Windows, Os::Linux] {
            let mut seen: Vec<String> = Vec::new();
            for p in profiles.iter().filter(|p| p.runs_on(os)) {
                for n in p.names() {
                    let n = n.to_ascii_lowercase();
                    assert!(!seen.contains(&n), "{n} twice on {os:?}");
                    seen.push(n);
                }
            }
        }
    }

    #[test]
    fn user_profiles_replace_and_add() {
        let user = parse(
            r#"schema_version = 1
[[app]]
name = "docker desktop"
readers = [{ kind = "json", key = "memoryMiB", unit = "MiB", label = "VM", paths = { linux = ["{home}/d.json"] } }]

[[app]]
name = "Example IDE"
aliases = ["ex"]
os = ["linux"]
readers = [{ kind = "jvm_options", paths = { linux = ["{home}/ex.vmoptions"] } }]
linux = { names = ["example"] }
"#,
        )
        .unwrap();
        let merged = merge(embedded(), user);
        let docker = merged
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case("Docker Desktop"))
            .unwrap();
        assert_eq!(docker.readers[0].kind(), "json");
        let ex = merged.iter().find(|p| p.name == "Example IDE").unwrap();
        assert!(ex.runs_on(Os::Linux) && !ex.runs_on(Os::Macos));
        assert_eq!(ex.matchers(Os::Linux).names, ["example"]);
    }

    #[test]
    fn invalid_user_files_are_errors() {
        for bad in [
            "schema_version = 2\n",
            "[[app]]\nname = \"x\"\nreaders = []\n",
            "schema_version = 1\n[[app]]\nname = \"x\"\nreaders = []\n",
            "schema_version = 1\n[[app]]\nname = \"x\"\nreaders = [{ kind = \"utm\" }]\nos = [\"beos\"]\n",
            "schema_version = 1\n[[app]]\nname = \"x\"\nreaders = [{ kind = \"utm\" }]\ncolour = 1\n",
            "schema_version = 1\n[[app]]\nname = \"x\"\nreaders = [{ kind = \"json\", key = \"k\", unit = \"furlongs\", label = \"l\" }]\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let dir = std::env::temp_dir().join(format!("canfit-profiles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("apps.toml");
        assert_eq!(load_user(&path), Ok(Vec::new()));
        std::fs::write(&path, "schema_version = [").unwrap();
        let err = load_user(&path).unwrap_err();
        assert!(err.to_string().contains("app profiles file"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
