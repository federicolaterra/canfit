//! The user's protected list, read from `config.toml` in canfit's config directory.
//!
//! ```toml
//! [processes]
//! protected = ["Slack", "llama-server"]
//!
//! [history]
//! retention_days = 7
//! ```
//!
//! Entries match an app (group) name, a process name or a bundle path, ignoring case. Protected
//! apps are treated like the "never" class. A config file that exists but cannot be read is an
//! error, never silently ignored: ignoring it could suggest closing something the user protected.
//! The `[history]` table is described in [`crate::history::HistorySettings`].

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::history::HistorySettings;

/// Environment variable that points canfit at another config file.
pub const CONFIG_ENV: &str = "CANFIT_CONFIG";

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub processes: ProcessesConfig,
    #[serde(default)]
    pub history: HistorySettings,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessesConfig {
    /// Apps and processes canfit must never suggest or close.
    #[serde(default)]
    pub protected: Vec<String>,
}

#[derive(Debug)]
pub struct ConfigError {
    pub path: PathBuf,
    pub detail: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "canfit could not read its config file {}, so it can't tell which apps you \
             protected: {}",
            self.path.display(),
            self.detail.trim()
        )
    }
}

/// The config file path: `$CANFIT_CONFIG`, or `config.toml` in the per-OS config directory
/// (`~/Library/Application Support/canfit` on macOS, `%APPDATA%\canfit\config` on Windows).
pub fn path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CONFIG_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    directories::ProjectDirs::from("", "", "canfit").map(|d| d.config_dir().join("config.toml"))
}

/// Loads the config; a missing file is the default (empty) config.
pub fn load() -> Result<(Config, Option<PathBuf>), ConfigError> {
    let Some(path) = path() else {
        return Ok((Config::default(), None));
    };
    let config = load_from(&path)?;
    Ok((config, Some(path)))
}

pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => {
            return Err(ConfigError {
                path: path.to_path_buf(),
                detail: e.to_string(),
            });
        }
    };
    let config: Config = toml::from_str(&text).map_err(|e| ConfigError {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    config.history.validate().map_err(|detail| ConfigError {
        path: path.to_path_buf(),
        detail,
    })?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, text: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("canfit-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        path
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        let path = temp("missing", None);
        assert_eq!(load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn protected_list_is_read_and_other_tables_are_ignored() {
        let path = temp(
            "ok",
            Some(
                "[processes]\nprotected = [\"Slack\", \"llama-server\"]\n\n[theme]\nname = \"dark\"\n",
            ),
        );
        let config = load_from(&path).unwrap();
        assert_eq!(config.processes.protected, ["Slack", "llama-server"]);
        assert_eq!(config.history, HistorySettings::default());
    }

    #[test]
    fn history_settings_are_read_and_checked() {
        let path = temp(
            "history",
            Some("[history]\nretention_days = 3\nsnapshot_interval_secs = 60\n"),
        );
        let config = load_from(&path).unwrap();
        assert_eq!(config.history.retention_days, 3);
        assert_eq!(config.history.snapshot_interval_secs, 60);
        assert!(config.history.enabled);
        let path = temp("history-bad", Some("[history]\nmax_size_mb = 0\n"));
        assert!(
            load_from(&path)
                .unwrap_err()
                .to_string()
                .contains("max_size_mb")
        );
    }

    #[test]
    fn malformed_or_misspelled_config_is_an_error() {
        for (name, text) in [
            ("syntax", "[processes\nprotected = 1"),
            ("typo", "[processes]\nprotect = [\"Slack\"]"),
            ("type", "[processes]\nprotected = \"Slack\""),
        ] {
            let path = temp(name, Some(text));
            let err = load_from(&path).unwrap_err();
            assert!(
                err.to_string().contains("which apps you protected"),
                "{err}"
            );
        }
    }
}
