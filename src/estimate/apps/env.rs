//! Where config readers look: the user's folders for one OS. Built from the running system, or
//! rooted in a fixture folder so every OS's layout is tested on every OS.

use std::path::PathBuf;

/// The OS whose settings layout a reader follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Macos,
    Windows,
    Linux,
}

impl Os {
    /// The OS canfit runs on, when it is one of the three.
    pub fn current() -> Option<Os> {
        match std::env::consts::OS {
            "macos" => Some(Os::Macos),
            "windows" => Some(Os::Windows),
            "linux" => Some(Os::Linux),
            _ => None,
        }
    }

    /// The key used in profiles (`macos`, `windows`, `linux`) and in `Snapshot.platform.os`.
    pub fn key(self) -> &'static str {
        match self {
            Os::Macos => "macos",
            Os::Windows => "windows",
            Os::Linux => "linux",
        }
    }

    pub fn from_key(key: &str) -> Option<Os> {
        match key {
            "macos" => Some(Os::Macos),
            "windows" => Some(Os::Windows),
            "linux" => Some(Os::Linux),
            _ => None,
        }
    }
}

/// The folders readers resolve paths against.
#[derive(Debug, Clone, PartialEq)]
pub struct Env {
    pub os: Os,
    /// The user's home folder (`%USERPROFILE%` on Windows).
    pub home: PathBuf,
    /// Per-user settings: macOS `~/Library/Application Support`, Windows `%APPDATA%`, Linux
    /// `$XDG_CONFIG_HOME` (`~/.config`).
    pub config: PathBuf,
    /// Per-user local data: macOS `~/Library/Application Support`, Windows `%LOCALAPPDATA%`,
    /// Linux `$XDG_DATA_HOME` (`~/.local/share`).
    pub data: PathBuf,
    /// Folders that hold installed apps: macOS `/Applications` and `~/Applications`, Windows
    /// `Program Files` (and its `JetBrains` folder) and `%LOCALAPPDATA%\Programs`, Linux `/opt` and
    /// JetBrains Toolbox.
    pub app_dirs: Vec<PathBuf>,
    /// Total physical memory, for settings whose default is a share of RAM (WSL).
    pub total_memory_bytes: u64,
}

impl Env {
    /// The running system's folders.
    pub fn system(total_memory_bytes: u64) -> Option<Env> {
        let os = Os::current()?;
        let dirs = directories::BaseDirs::new()?;
        let home = dirs.home_dir().to_path_buf();
        let config = dirs.config_dir().to_path_buf();
        let data = dirs.data_local_dir().to_path_buf();
        let app_dirs = match os {
            Os::Macos => vec![PathBuf::from("/Applications"), home.join("Applications")],
            Os::Windows => {
                let mut roots: Vec<PathBuf> = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
                    .iter()
                    .filter_map(std::env::var_os)
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .collect();
                roots.dedup();
                // JetBrains installs into `Program Files\JetBrains\<IDE> <version>`.
                let mut dirs: Vec<PathBuf> = roots.iter().map(|r| r.join("JetBrains")).collect();
                dirs.extend(roots);
                dirs.push(data.join("Programs"));
                dirs
            }
            Os::Linux => vec![
                PathBuf::from("/opt"),
                data.join("JetBrains").join("Toolbox").join("apps"),
            ],
        };
        Some(Env {
            os,
            home,
            config,
            data,
            app_dirs,
            total_memory_bytes,
        })
    }

    /// The default layout of `os` with the home folder at `root` (tests and fixtures).
    #[cfg(test)]
    pub fn rooted(os: Os, root: &std::path::Path, total_memory_bytes: u64) -> Env {
        let home = root.to_path_buf();
        let (config, data, app_dirs) = match os {
            Os::Macos => {
                let support = home.join("Library").join("Application Support");
                (support.clone(), support, vec![home.join("Applications")])
            }
            Os::Windows => {
                let local = home.join("AppData").join("Local");
                (
                    home.join("AppData").join("Roaming"),
                    local.clone(),
                    vec![
                        home.join("Program Files").join("JetBrains"),
                        home.join("Program Files"),
                        local.join("Programs"),
                    ],
                )
            }
            Os::Linux => {
                let data = home.join(".local").join("share");
                (
                    home.join(".config"),
                    data.clone(),
                    vec![
                        home.join("opt"),
                        data.join("JetBrains").join("Toolbox").join("apps"),
                    ],
                )
            }
        };
        Env {
            os,
            home,
            config,
            data,
            app_dirs,
            total_memory_bytes,
        }
    }

    /// Expands `{home}`, `{config}` and `{data}` in a profile path and splits it on `/`, so one
    /// spelling works on every OS.
    pub fn expand(&self, template: &str) -> PathBuf {
        let mut path = PathBuf::new();
        for (i, part) in template.split('/').enumerate() {
            match part {
                "{home}" if i == 0 => path.push(&self.home),
                "{config}" if i == 0 => path.push(&self.config),
                "{data}" if i == 0 => path.push(&self.data),
                "" if i == 0 => path.push(std::path::MAIN_SEPARATOR_STR),
                // A Windows drive (`C:`): keep the path absolute, not drive-relative.
                drive if i == 0 && drive.len() == 2 && drive.ends_with(':') => {
                    path.push(format!("{drive}{}", std::path::MAIN_SEPARATOR));
                }
                "" => {}
                other => path.push(other),
            }
        }
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rooted_layouts_follow_each_os() {
        let root = Path::new("root");
        let mac = Env::rooted(Os::Macos, root, 0);
        assert_eq!(mac.config, root.join("Library/Application Support"));
        let win = Env::rooted(Os::Windows, root, 0);
        assert_eq!(win.config, root.join("AppData").join("Roaming"));
        assert_eq!(win.data, root.join("AppData").join("Local"));
        let linux = Env::rooted(Os::Linux, root, 0);
        assert_eq!(linux.config, root.join(".config"));
        assert_eq!(
            linux.expand("{config}/JetBrains/x.vmoptions"),
            root.join(".config").join("JetBrains").join("x.vmoptions")
        );
        assert_eq!(linux.expand("{home}/.wslconfig"), root.join(".wslconfig"));
        // `/etc/x.json` has no drive on Windows: rooted, but not absolute there.
        #[cfg(unix)]
        assert!(linux.expand("/etc/x.json").is_absolute());
        #[cfg(windows)]
        assert!(win.expand("C:/Users/x/a.json").is_absolute());
    }

    #[test]
    fn os_keys_round_trip() {
        for os in [Os::Macos, Os::Windows, Os::Linux] {
            assert_eq!(Os::from_key(os.key()), Some(os));
        }
        assert_eq!(Os::from_key("freebsd"), None);
    }
}
