//! `canfit update`: says how this copy of canfit is updated and, for a copy installed by the
//! release's shell or PowerShell installer, hands over to the companion updater after an explicit
//! confirmation (ADR-019).
//!
//! canfit itself never connects to the network. The updater (`canfit-update`, cargo-dist's
//! axoupdater) is installed next to canfit only by the installer scripts; it reads the install
//! receipt the installer wrote, asks GitHub for the latest release and, if it is newer, runs the
//! release's installer. Copies installed by a package manager are updated by that package manager,
//! and `canfit update` says which command to run. There are no background checks.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Serialize;

use super::EXIT_RUNTIME;
use super::advice::TerminalConfirm;
use crate::advisor::close::Confirm;

pub const UPDATE_SCHEMA: &str = "canfit.update";
pub const UPDATE_SCHEMA_VERSION: u32 = 1;

/// Where the releases are published; shown when canfit can't tell how it was installed.
const RELEASES_URL: &str = "https://github.com/federicolaterra/canfit/releases/latest";

/// `cargo install` from the repository (canfit is not published as a crate, ADR-028). canfit never
/// connects to find the latest tag, so the command names it as `<tag>` and the answer says where
/// to look.
const CARGO_COMMAND: &str =
    "cargo install --git https://github.com/federicolaterra/canfit --tag <tag> --locked";

/// How this copy of canfit was installed, judged from where its executable lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    /// The release's shell or PowerShell installer, which also installed the updater.
    Installer,
    /// Homebrew, from the tap in the canfit repository.
    Homebrew,
    /// Scoop, from the manifest in the canfit repository.
    Scoop,
    /// The release's MSI, installed under Program Files.
    Msi,
    Cargo,
    /// A Linux package (.deb or .rpm from the release) in /usr/bin.
    SystemPackage,
    Unknown,
}

impl InstallMethod {
    /// The command that updates this copy, when it isn't the updater.
    fn command(&self) -> Option<&'static str> {
        match self {
            InstallMethod::Installer | InstallMethod::Unknown => None,
            InstallMethod::Homebrew => Some("brew upgrade canfit"),
            InstallMethod::Scoop => Some("scoop update canfit"),
            InstallMethod::Msi => None,
            InstallMethod::Cargo => Some(CARGO_COMMAND),
            InstallMethod::SystemPackage => None,
        }
    }

    fn manager(&self) -> &'static str {
        match self {
            InstallMethod::Installer => "the canfit installer",
            InstallMethod::Homebrew => "Homebrew",
            InstallMethod::Scoop => "Scoop",
            InstallMethod::Msi => "the Windows installer (MSI)",
            InstallMethod::Cargo => "cargo install",
            InstallMethod::SystemPackage => "your distribution's package manager",
            InstallMethod::Unknown => "an unknown method",
        }
    }
}

/// The updater's file name next to canfit.
fn updater_name() -> &'static str {
    if cfg!(windows) {
        "canfit-update.exe"
    } else {
        "canfit-update"
    }
}

/// Classifies an install from the (canonical) executable path and whether the updater sits next
/// to it. Pure, so every rule is tested on every OS.
pub fn detect(exe: &Path, updater_present: bool) -> InstallMethod {
    let path = exe
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let has = |part: &str| path.contains(part);
    if has("/cellar/") || has("/homebrew/") || has("/linuxbrew/") {
        InstallMethod::Homebrew
    } else if has("/scoop/") {
        InstallMethod::Scoop
    } else if has("/program files/") || has("/program files (x86)/") {
        InstallMethod::Msi
    } else if updater_present {
        InstallMethod::Installer
    } else if has("/.cargo/bin/") {
        InstallMethod::Cargo
    } else if path.starts_with("/usr/bin/") || path.starts_with("/usr/sbin/") {
        InstallMethod::SystemPackage
    } else {
        InstallMethod::Unknown
    }
}

/// What canfit found out about its own install.
#[derive(Debug)]
pub struct Install {
    pub exe: PathBuf,
    pub method: InstallMethod,
    pub updater: Option<PathBuf>,
}

/// Looks at the running executable. Reads the file system only.
pub fn inspect() -> Result<Install, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("canfit could not find its own executable: {e}."))?;
    // Follow symlinks (Homebrew links /opt/homebrew/bin/canfit into its Cellar).
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let updater = exe
        .parent()
        .map(|dir| dir.join(updater_name()))
        .filter(|p| p.is_file());
    let method = detect(&exe, updater.is_some());
    let updater = updater.filter(|_| method == InstallMethod::Installer);
    Ok(Install {
        exe,
        method,
        updater,
    })
}

/// The one-sentence answer, followed by what to do.
pub fn answer(install: &Install) -> String {
    let version = env!("CARGO_PKG_VERSION");
    match &install.method {
        InstallMethod::Installer => format!(
            "canfit {version} can update itself: the canfit installer set it up in {}.",
            folder(install)
        ),
        InstallMethod::SystemPackage => format!(
            "canfit {version} was installed as a Linux package, so update it the same way: \
             install the new .deb or .rpm from {RELEASES_URL} (`sudo apt install ./canfit_*.deb`, \
             `sudo dnf install ./canfit-*.rpm`)."
        ),
        InstallMethod::Msi => format!(
            "canfit {version} was installed with the Windows installer (MSI), so update it the \
             same way: download the latest canfit-x86_64-pc-windows-msvc.msi from {RELEASES_URL} \
             and run it; it replaces this version."
        ),
        InstallMethod::Unknown => format!(
            "canfit {version} can't tell how it was installed ({}), so it can't update itself. \
             Download the latest release from {RELEASES_URL}, or reinstall with the one-line \
             installer.",
            install.exe.display()
        ),
        InstallMethod::Cargo => format!(
            "canfit {version} was installed with cargo install, so cargo updates it: run \
             `{CARGO_COMMAND}` with the tag of the latest release (for example v{version}) from \
             {RELEASES_URL}."
        ),
        method => format!(
            "canfit {version} was installed with {}, so {} updates it: run `{}`.",
            method.manager(),
            method.manager(),
            method.command().unwrap_or_default()
        ),
    }
}

fn folder(install: &Install) -> String {
    install
        .exe
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default()
}

/// What the updater will do, said before anything connects.
pub fn consent_text() -> String {
    "To update, canfit runs its updater (canfit-update), which connects to github.com to look for \
     a newer release and, if there is one, downloads it and replaces canfit in the same folder. \
     Nothing else is sent. canfit never checks for updates on its own."
        .to_string()
}

#[derive(Serialize)]
struct UpdateDocument {
    schema: &'static str,
    schema_version: u32,
    canfit_version: &'static str,
    summary: String,
    install_method: InstallMethod,
    executable: String,
    self_update: bool,
    update_command: Option<String>,
}

/// The `--json` document: how this copy is updated. It never updates or connects.
pub fn to_json(install: &Install) -> String {
    let update_command = match &install.method {
        InstallMethod::Installer => Some("canfit update --yes".to_string()),
        m => m.command().map(str::to_string),
    };
    let document = UpdateDocument {
        schema: UPDATE_SCHEMA,
        schema_version: UPDATE_SCHEMA_VERSION,
        canfit_version: env!("CARGO_PKG_VERSION"),
        summary: answer(install),
        install_method: install.method.clone(),
        executable: install.exe.display().to_string(),
        self_update: install.method == InstallMethod::Installer,
        update_command,
    };
    serde_json::to_string_pretty(&document).expect("the update document serializes")
}

/// `canfit update [--yes] [--json]`.
pub fn run(yes: bool, json: bool) -> ExitCode {
    let install = match inspect() {
        Ok(install) => install,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    if json {
        println!("{}", to_json(&install));
        return ExitCode::SUCCESS;
    }
    println!("{}", crate::ui::glyphs::text(&answer(&install)));
    let Some(updater) = install.updater.clone() else {
        return ExitCode::SUCCESS;
    };
    println!("\n{}", consent_text());
    if !yes {
        if !super::advice::interactive_terminal() {
            eprintln!(
                "\ncanfit did not update: it needs your confirmation. Run `canfit update` in a \
                 terminal, or `canfit update --yes` to update without asking."
            );
            return ExitCode::from(EXIT_RUNTIME);
        }
        if !TerminalConfirm.confirm("Update canfit now?") {
            println!("\nNothing changed.");
            return ExitCode::SUCCESS;
        }
    }
    run_updater(&updater)
}

#[cfg(not(windows))]
fn run_updater(updater: &Path) -> ExitCode {
    println!();
    match Command::new(updater).status() {
        Ok(status) if status.success() => {
            println!("\nDone. Run `canfit --version` to see the version now installed.");
            ExitCode::SUCCESS
        }
        Ok(status) => {
            eprintln!(
                "\nThe updater stopped ({status}); its message is above. canfit is unchanged \
                 unless it says otherwise."
            );
            ExitCode::from(EXIT_RUNTIME)
        }
        Err(e) => {
            eprintln!("\ncanfit could not start {}: {e}.", updater.display());
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

/// Windows can't replace an executable while it runs, so canfit starts the updater and exits at
/// once, leaving canfit.exe free to be replaced.
#[cfg(windows)]
fn run_updater(updater: &Path) -> ExitCode {
    match Command::new(updater).spawn() {
        Ok(_) => {
            println!(
                "\nThe updater is running in this window; canfit has exited so it can be \
                 replaced. When the updater says it is done, run `canfit --version`."
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\ncanfit could not start {}: {e}.", updater.display());
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method(path: &str, updater: bool) -> InstallMethod {
        detect(Path::new(path), updater)
    }

    #[test]
    fn package_managers_are_recognised_from_the_path() {
        assert_eq!(
            method("/opt/homebrew/Cellar/canfit/1.0.0/bin/canfit", false),
            InstallMethod::Homebrew
        );
        assert_eq!(
            method("/usr/local/Cellar/canfit/1.0.0/bin/canfit", false),
            InstallMethod::Homebrew
        );
        assert_eq!(
            method(
                "/home/linuxbrew/.linuxbrew/Cellar/canfit/1.0.0/bin/canfit",
                false
            ),
            InstallMethod::Homebrew
        );
        assert_eq!(
            method(r"C:\Users\fede\scoop\apps\canfit\current\canfit.exe", false),
            InstallMethod::Scoop
        );
        assert_eq!(
            method(r"C:\Program Files\canfit\bin\canfit.exe", false),
            InstallMethod::Msi
        );
        assert_eq!(
            method("/home/fede/.cargo/bin/canfit", false),
            InstallMethod::Cargo
        );
        assert_eq!(
            method("/usr/bin/canfit", false),
            InstallMethod::SystemPackage
        );
    }

    #[test]
    fn the_updater_next_to_canfit_means_the_installer() {
        assert_eq!(
            method("/home/fede/.local/bin/canfit", true),
            InstallMethod::Installer
        );
        assert_eq!(
            method(r"C:\Users\fede\.local\bin\canfit.exe", true),
            InstallMethod::Installer
        );
        // An installer that used ~/.cargo/bin still self-updates.
        assert_eq!(
            method("/home/fede/.cargo/bin/canfit", true),
            InstallMethod::Installer
        );
        // A package manager wins even if an updater file happens to be there.
        assert_eq!(
            method("/opt/homebrew/Cellar/canfit/1.0.0/bin/canfit", true),
            InstallMethod::Homebrew
        );
        assert_eq!(
            method("/home/fede/.local/bin/canfit", false),
            InstallMethod::Unknown
        );
        assert_eq!(
            method("/Users/fede/dev/canfit/target/release/canfit", false),
            InstallMethod::Unknown
        );
    }

    fn install(path: &str, method: InstallMethod) -> Install {
        Install {
            exe: PathBuf::from(path),
            method,
            updater: None,
        }
    }

    #[test]
    fn answers_name_the_command_to_run() {
        let brew = answer(&install(
            "/opt/homebrew/Cellar/canfit/1.0.0/bin/canfit",
            InstallMethod::Homebrew,
        ));
        assert!(
            brew.starts_with("canfit ") && brew.contains("Homebrew"),
            "{brew}"
        );
        assert!(brew.contains("`brew upgrade canfit`"), "{brew}");
        let scoop = answer(&install(
            r"C:\Users\fede\scoop\apps\canfit\current\canfit.exe",
            InstallMethod::Scoop,
        ));
        assert!(scoop.contains("`scoop update canfit`"), "{scoop}");
        let cargo = answer(&install(
            "/home/fede/.cargo/bin/canfit",
            InstallMethod::Cargo,
        ));
        assert!(
            cargo.contains("cargo install --git https://github.com/federicolaterra/canfit --tag")
                && cargo.contains(RELEASES_URL),
            "{cargo}"
        );
        let msi = answer(&install(
            r"C:\Program Files\canfit\bin\canfit.exe",
            InstallMethod::Msi,
        ));
        assert!(msi.contains(".msi") && msi.contains(RELEASES_URL), "{msi}");
        let distro = answer(&install("/usr/bin/canfit", InstallMethod::SystemPackage));
        assert!(
            distro.contains(".deb") && distro.contains(".rpm") && !distro.contains("AUR"),
            "{distro}"
        );
        let unknown = answer(&install("/tmp/canfit", InstallMethod::Unknown));
        assert!(unknown.contains(RELEASES_URL), "{unknown}");
        let own = answer(&install(
            "/home/fede/.local/bin/canfit",
            InstallMethod::Installer,
        ));
        assert!(own.contains("can update itself"), "{own}");
    }

    #[test]
    fn json_reports_the_method_without_updating() {
        let doc = to_json(&install(
            "/home/fede/.cargo/bin/canfit",
            InstallMethod::Cargo,
        ));
        let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(v["schema"], UPDATE_SCHEMA);
        assert_eq!(v["schema_version"], UPDATE_SCHEMA_VERSION);
        assert_eq!(v["install_method"], "cargo");
        assert_eq!(v["self_update"], false);
        assert_eq!(v["update_command"], CARGO_COMMAND);
        let doc = to_json(&install(
            "/home/fede/.local/bin/canfit",
            InstallMethod::Installer,
        ));
        let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(v["install_method"], "installer");
        assert_eq!(v["self_update"], true);
        let doc = to_json(&install("/tmp/canfit", InstallMethod::Unknown));
        let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert!(v["update_command"].is_null());
    }

    #[test]
    fn consent_says_what_connects_before_anything_does() {
        let text = consent_text();
        assert!(text.contains("github.com"));
        assert!(text.contains("never checks for updates on its own"));
    }
}
