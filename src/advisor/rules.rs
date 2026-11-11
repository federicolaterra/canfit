//! The per-OS classification rules embedded in the binary (ADR-013).
//!
//! Rules are TOML so that they read as data, not code, and can be reviewed in one place:
//! `src/advisor/rules/<os>.toml`. They are parsed when the advisor runs; a malformed embedded
//! file is a bug caught by the tests.

use serde::Deserialize;

use super::SafetyClass;

const MACOS_RULES: &str = include_str!("rules/macos.toml");
const WINDOWS_RULES: &str = include_str!("rules/windows.toml");
const LINUX_RULES: &str = include_str!("rules/linux.toml");

/// Placeholder in Windows `path_prefixes` for the Windows folder.
const SYSTEM_ROOT_TOKEN: &str = "%SystemRoot%";

/// Supported version of the rules file layout.
const RULES_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    pub schema_version: u32,
    #[serde(rename = "rule")]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub class: SafetyClass,
    pub reason: String,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub name_prefixes: Vec<String>,
    #[serde(default)]
    pub bundles: Vec<String>,
    #[serde(default)]
    pub path_prefixes: Vec<String>,
}

/// What a rule is matched against: one process.
pub struct Subject<'a> {
    pub name: &'a str,
    pub bundle_name: Option<&'a str>,
    pub exe: Option<&'a str>,
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn starts_with(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && eq(&s[..prefix.len()], prefix)
}

impl Rule {
    pub fn matches(&self, s: &Subject<'_>) -> bool {
        self.names.iter().any(|n| eq(n, s.name))
            || self.name_prefixes.iter().any(|p| starts_with(s.name, p))
            || s.bundle_name
                .is_some_and(|b| self.bundles.iter().any(|n| eq(n, b)))
            || s.exe
                .is_some_and(|e| self.path_prefixes.iter().any(|p| starts_with(e, p)))
    }
}

impl RuleSet {
    /// Replaces [`SYSTEM_ROOT_TOKEN`] in path prefixes with `system_root`.
    fn expand_system_root(mut self, system_root: &str) -> RuleSet {
        let root = system_root.trim_end_matches(['\\', '/']);
        for rule in &mut self.rules {
            for prefix in &mut rule.path_prefixes {
                *prefix = prefix.replace(SYSTEM_ROOT_TOKEN, root);
            }
        }
        self
    }

    fn parse(text: &str) -> Result<RuleSet, String> {
        let set: RuleSet = toml::from_str(text).map_err(|e| e.to_string())?;
        if set.schema_version != RULES_SCHEMA_VERSION {
            return Err(format!(
                "unsupported rules schema_version {}",
                set.schema_version
            ));
        }
        Ok(set)
    }

    /// The first rule matching the process, in file order.
    pub fn first_match(&self, s: &Subject<'_>) -> Option<&Rule> {
        self.rules.iter().find(|r| r.matches(s))
    }
}

/// The embedded rules for an OS (`Snapshot.platform.os`), or `None` when canfit has none yet.
/// Without rules the advisor suggests nothing (fail closed).
pub fn for_os(os: &str) -> Option<RuleSet> {
    let text = match os {
        "macos" => MACOS_RULES,
        "windows" => WINDOWS_RULES,
        "linux" => LINUX_RULES,
        _ => return None,
    };
    let set = RuleSet::parse(text).expect("embedded rules parse (covered by tests)");
    Some(set.expand_system_root(&windows_system_root()))
}

/// The Windows folder (`%SystemRoot%`), normally `C:\Windows`.
pub fn windows_system_root() -> String {
    std::env::var("SystemRoot")
        .ok()
        .filter(|root| !root.trim().is_empty())
        .unwrap_or_else(|| r"C:\Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject<'a>(name: &'a str, bundle: Option<&'a str>, exe: Option<&'a str>) -> Subject<'a> {
        Subject {
            name,
            bundle_name: bundle,
            exe,
        }
    }

    #[test]
    fn embedded_macos_rules_parse_and_have_every_class() {
        let set = for_os("macos").expect("macOS rules");
        for class in [SafetyClass::Safe, SafetyClass::Caution, SafetyClass::Never] {
            assert!(set.rules.iter().any(|r| r.class == class), "{class:?}");
        }
        assert!(set.rules.iter().all(|r| !r.reason.is_empty()));
        assert!(for_os("freebsd").is_none());
    }

    #[test]
    fn embedded_windows_rules_parse_and_expand_the_system_root() {
        let set = for_os("windows").expect("Windows rules");
        for class in [SafetyClass::Safe, SafetyClass::Caution, SafetyClass::Never] {
            assert!(set.rules.iter().any(|r| r.class == class), "{class:?}");
        }
        let prefixes: Vec<&String> = set.rules.iter().flat_map(|r| &r.path_prefixes).collect();
        assert!(prefixes.iter().all(|p| !p.contains(SYSTEM_ROOT_TOKEN)));
        let root = format!("{}\\", windows_system_root());
        assert!(prefixes.iter().any(|p| **p == root), "{prefixes:?}");
        let custom = RuleSet::parse(WINDOWS_RULES)
            .unwrap()
            .expand_system_root(r"D:\WINNT\");
        assert!(
            custom
                .rules
                .iter()
                .any(|r| r.path_prefixes.iter().any(|p| p == r"D:\WINNT\"))
        );
    }

    #[test]
    fn required_windows_never_processes_are_never() {
        let set = for_os("windows").unwrap();
        for name in [
            "System",
            "smss.exe",
            "csrss.exe",
            "wininit.exe",
            "services.exe",
            "lsass.exe",
            "winlogon.exe",
            "dwm.exe",
            "explorer.exe",
            "svchost.exe",
            "conhost.exe",
            "OpenConsole.exe",
            "cmd.exe",
            "powershell.exe",
            "pwsh.exe",
            "canfit.exe",
            "vmmemWSL",
        ] {
            let rule = set.first_match(&subject(name, None, None)).expect(name);
            assert_eq!(rule.class, SafetyClass::Never, "{name}");
        }
        let system = subject(
            "SomeHelper.exe",
            None,
            Some(r"C:\Windows\System32\SomeHelper.exe"),
        );
        assert_eq!(set.first_match(&system).unwrap().class, SafetyClass::Never);
    }

    #[test]
    fn windows_apps_and_exceptions_get_their_class() {
        let set = for_os("windows").unwrap();
        let class = |s: Subject<'_>| set.first_match(&s).map(|r| r.class);
        assert_eq!(
            class(subject("chrome.exe", Some("chrome"), None)),
            Some(SafetyClass::Safe)
        );
        assert_eq!(
            class(subject("CHROME.EXE", None, None)),
            Some(SafetyClass::Safe)
        );
        // Notepad lives in the Windows folder but is an exception listed before the path rule.
        assert_eq!(
            class(subject(
                "notepad.exe",
                Some("notepad"),
                Some(r"C:\Windows\System32\notepad.exe")
            )),
            Some(SafetyClass::Caution)
        );
        // Docker's helpers are matched through the app's root executable.
        assert_eq!(
            class(subject(
                "com.docker.dev-envs.exe",
                Some("Docker Desktop"),
                Some(r"C:\Program Files\Docker\Docker\resources\com.docker.dev-envs.exe")
            )),
            Some(SafetyClass::Caution)
        );
        assert_eq!(
            class(subject(
                "llama-server.exe",
                None,
                Some(r"C:\tools\llama-server.exe")
            )),
            Some(SafetyClass::Caution)
        );
        assert_eq!(
            class(subject("WindowsTerminal.exe", None, None)),
            Some(SafetyClass::Caution)
        );
        assert_eq!(class(subject("SomeTool.exe", Some("SomeTool"), None)), None);
    }

    #[test]
    fn required_never_processes_are_never() {
        let set = for_os("macos").unwrap();
        for name in [
            "launchd",
            "kernel_task",
            "WindowServer",
            "loginwindow",
            "Dock",
            "Finder",
            "canfit",
            "zsh",
        ] {
            let rule = set.first_match(&subject(name, None, None)).expect(name);
            assert_eq!(rule.class, SafetyClass::Never, "{name}");
        }
        let daemon = subject(
            "chronod",
            None,
            Some("/System/Library/PrivateFrameworks/ChronoCore.framework/Support/chronod"),
        );
        assert_eq!(set.first_match(&daemon).unwrap().class, SafetyClass::Never);
        let libexec = subject("trustd", None, Some("/usr/libexec/trustd"));
        assert_eq!(set.first_match(&libexec).unwrap().class, SafetyClass::Never);
    }

    #[test]
    fn apps_and_exceptions_get_their_class() {
        let set = for_os("macos").unwrap();
        let class = |s: Subject<'_>| set.first_match(&s).map(|r| r.class);
        assert_eq!(
            class(subject("Google Chrome Helper", Some("Google Chrome"), None)),
            Some(SafetyClass::Safe)
        );
        assert_eq!(
            class(subject("Electron", Some("visual studio code"), None)),
            Some(SafetyClass::Caution)
        );
        // Homebrew binaries under /usr/local are not "part of macOS".
        assert_eq!(
            class(subject(
                "llama-server",
                None,
                Some("/usr/local/bin/llama-server")
            )),
            Some(SafetyClass::Caution)
        );
        assert_eq!(
            class(subject("node", None, Some("/usr/local/bin/node"))),
            None
        );
        // System-provided WebKit content processes are an explicit exception.
        assert_eq!(
            class(subject(
                "com.apple.WebKit.WebContent",
                None,
                Some("/System/Library/Frameworks/WebKit.framework/x/com.apple.WebKit.WebContent")
            )),
            Some(SafetyClass::Caution)
        );
        assert_eq!(class(subject("Unknown App", Some("Unknown"), None)), None);
    }

    #[test]
    fn embedded_linux_rules_parse_and_have_every_class() {
        let set = for_os("linux").expect("Linux rules");
        for class in [SafetyClass::Safe, SafetyClass::Caution, SafetyClass::Never] {
            assert!(set.rules.iter().any(|r| r.class == class), "{class:?}");
        }
        assert!(set.rules.iter().all(|r| !r.reason.is_empty()));
    }

    #[test]
    fn required_linux_never_processes_are_never() {
        let set = for_os("linux").unwrap();
        for name in [
            "systemd",
            "init",
            "kthreadd",
            "systemd-journald",
            "dbus-broker",
            "Xorg",
            "Xwayland",
            "gnome-shell",
            "kwin_wayland",
            "plasmashell",
            "sway",
            "Hyprland",
            "gdm-wayland-session",
            "sddm",
            "gnome-session-binary",
            "pipewire",
            "gsd-power",
            "bash",
            "zsh",
            "fish",
            "sshd",
            "tmux",
            "canfit",
        ] {
            let rule = set.first_match(&subject(name, None, None)).expect(name);
            assert_eq!(rule.class, SafetyClass::Never, "{name}");
        }
        let libexec = subject(
            "evolution-source-registry",
            None,
            Some("/usr/libexec/evolution-source-registry"),
        );
        assert_eq!(set.first_match(&libexec).unwrap().class, SafetyClass::Never);
    }

    #[test]
    fn linux_apps_and_exceptions_get_their_class() {
        let set = for_os("linux").unwrap();
        let class = |s: Subject<'_>| set.first_match(&s).map(|r| r.class);
        assert_eq!(
            class(subject("chrome_crashpad_handler", Some("chrome"), None)),
            Some(SafetyClass::Safe)
        );
        assert_eq!(
            class(subject(
                "bwrap",
                Some("org.mozilla.firefox"),
                Some("/usr/bin/bwrap")
            )),
            Some(SafetyClass::Safe)
        );
        // GNOME Terminal lives in /usr/libexec but is an exception listed before the path rule.
        assert_eq!(
            class(subject(
                "gnome-terminal-server",
                Some("gnome-terminal-server"),
                Some("/usr/libexec/gnome-terminal-server")
            )),
            Some(SafetyClass::Caution)
        );
        assert_eq!(
            class(subject("ollama_llama_server", Some("ollama"), None)),
            Some(SafetyClass::Caution)
        );
        assert_eq!(
            class(subject(
                "llama-server",
                None,
                Some("/home/fede/llama.cpp/build/bin/llama-server")
            )),
            Some(SafetyClass::Caution)
        );
        assert_eq!(
            class(subject("code", Some("code"), None)),
            Some(SafetyClass::Caution)
        );
        assert_eq!(
            class(subject("qemu-system-riscv64", None, None)),
            Some(SafetyClass::Caution)
        );
        assert_eq!(
            class(subject("some-tool", None, Some("/usr/bin/some-tool"))),
            None
        );
    }

    #[test]
    fn matching_ignores_case_and_handles_unicode() {
        assert!(starts_with("COM.APPLE.WebKit.x", "com.apple.webkit."));
        assert!(!starts_with("é", "e"));
        assert!(!starts_with("ab", "abc"));
    }

    #[test]
    fn malformed_or_unknown_rules_are_rejected() {
        assert!(RuleSet::parse("schema_version = 2\nrule = []").is_err());
        assert!(
            RuleSet::parse(
                "schema_version = 1\n[[rule]]\nclass = \"maybe\"\nreason = \"x\"\nnames = []"
            )
            .is_err()
        );
        assert!(
            RuleSet::parse(
                "schema_version = 1\n[[rule]]\nclass = \"safe\"\nreason = \"x\"\nnmes = []"
            )
            .is_err()
        );
    }
}
