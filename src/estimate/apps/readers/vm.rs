//! Virtual machines: the RAM assigned in each VM's configuration file.
//!
//! - UTM (macOS): `~/Library/Containers/com.utmapp.UTM/Data/Documents/<VM>.utm/config.plist`,
//!   an XML property list; `ConfigurationVersion` 4 (UTM 4 and later) keeps the RAM in MiB at
//!   `System › MemorySize` and the name at `Information › Name`.
//! - Parallels Desktop (macOS): `~/Parallels/<VM>.pvm/config.pvs`, XML; RAM in MiB at
//!   `Hardware › Memory › RAM`, name at `Identification › VmName`.
//! - VMware Fusion / Workstation: `.vmx` text files (`memsize = "4096"` in MiB,
//!   `displayName`), listed in VMware's inventory (`vmInventory` on macOS, `inventory.vmls`
//!   elsewhere) and found in the default folders.
//! - VirtualBox: `.vbox` XML files (`Machine/@name`, `Hardware/Memory/@RAMSize` in MiB), listed
//!   in `VirtualBox.xml` and found in `~/VirtualBox VMs`.

use std::path::{Path, PathBuf};

use super::{display_path, has_ext, list_dir, read_text, stem};
use crate::core::Provenance;
use crate::estimate::apps::xml::{self, Plist};
use crate::estimate::apps::{Confidence, DeclaredItem, Env, Os, ReadError, Reading, ReadingKind};

const MIB: u64 = 1024 * 1024;

/// One VM read from its configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    pub name: Option<String>,
    pub memory_mib: u64,
}

// ---------------------------------------------------------------------------------------------
// Parsers

/// The oldest UTM configuration layout canfit reads.
pub const UTM_CONFIG_VERSION: i64 = 4;

/// Parses a UTM `config.plist`.
pub fn parse_utm(text: &str) -> Result<VmConfig, String> {
    let plist = Plist::parse(text)?;
    let version = plist
        .get("ConfigurationVersion")
        .and_then(Plist::as_i64)
        .unwrap_or(0);
    if version < UTM_CONFIG_VERSION {
        return Err(format!(
            "it uses an older UTM layout (configuration version {version}); open the VM once in \
             a current UTM to upgrade it"
        ));
    }
    let memory = plist
        .path(&["System", "MemorySize"])
        .and_then(Plist::as_i64)
        .filter(|&m| m > 0)
        .ok_or_else(|| "it has no System › MemorySize setting".to_string())?;
    Ok(VmConfig {
        name: plist
            .path(&["Information", "Name"])
            .and_then(Plist::as_str)
            .filter(|n| !n.is_empty())
            .map(str::to_string),
        memory_mib: memory as u64,
    })
}

/// Parses a Parallels `config.pvs`.
pub fn parse_parallels(text: &str) -> Result<VmConfig, String> {
    let root = xml::parse(text)?;
    let memory = root
        .find(&["Hardware", "Memory", "RAM"])
        .map(xml::Element::text)
        .ok_or_else(|| "it has no Hardware › Memory › RAM setting".to_string())?;
    let memory_mib = memory
        .parse::<u64>()
        .ok()
        .filter(|&m| m > 0)
        .ok_or_else(|| format!("RAM \"{memory}\" is not a number of MB"))?;
    Ok(VmConfig {
        name: root
            .find(&["Identification", "VmName"])
            .map(xml::Element::text)
            .filter(|n| !n.is_empty()),
        memory_mib,
    })
}

/// The value of `key` in VMware's `key = "value"` format, ignoring the key's case.
fn vmx_value(text: &str, key: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(key)
            .then(|| v.trim().trim_matches('"').to_string())
    })
}

/// Parses a VMware `.vmx` file.
pub fn parse_vmx(text: &str) -> Result<VmConfig, String> {
    let memory =
        vmx_value(text, "memsize").ok_or_else(|| "it has no memsize setting".to_string())?;
    let memory_mib = memory
        .parse::<u64>()
        .ok()
        .filter(|&m| m > 0)
        .ok_or_else(|| format!("memsize \"{memory}\" is not a number of MB"))?;
    Ok(VmConfig {
        name: vmx_value(text, "displayName").filter(|n| !n.is_empty()),
        memory_mib,
    })
}

/// The `.vmx` paths in a VMware inventory (`vmlistN.config = "…"`).
pub fn parse_vmware_inventory(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let (k, v) = line.split_once('=')?;
            let k = k.trim().to_ascii_lowercase();
            (k.starts_with("vmlist") && k.ends_with(".config"))
                .then(|| v.trim().trim_matches('"').to_string())
                .filter(|p| p.to_ascii_lowercase().ends_with(".vmx"))
                .map(PathBuf::from)
        })
        .collect()
}

/// Parses a VirtualBox `.vbox` file.
pub fn parse_vbox(text: &str) -> Result<VmConfig, String> {
    let root = xml::parse(text)?;
    let machine = root
        .child("Machine")
        .ok_or_else(|| "it has no <Machine> element".to_string())?;
    let memory = machine
        .find(&["Hardware", "Memory"])
        .and_then(|m| m.attr("RAMSize"))
        .ok_or_else(|| "it has no Hardware › Memory RAMSize setting".to_string())?;
    let memory_mib = memory
        .parse::<u64>()
        .ok()
        .filter(|&m| m > 0)
        .ok_or_else(|| format!("RAMSize \"{memory}\" is not a number of MB"))?;
    Ok(VmConfig {
        name: machine
            .attr("name")
            .filter(|n| !n.is_empty())
            .map(str::to_string),
        memory_mib,
    })
}

/// The `.vbox` paths registered in `VirtualBox.xml` (`MachineEntry/@src`).
pub fn parse_virtualbox_registry(text: &str) -> Result<Vec<PathBuf>, String> {
    let root = xml::parse(text)?;
    let Some(registry) = root.find(&["Global", "MachineRegistry"]) else {
        return Ok(Vec::new());
    };
    Ok(registry
        .elements()
        .filter(|e| e.name == "MachineEntry")
        .filter_map(|e| e.attr("src"))
        .map(PathBuf::from)
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Discovery

/// Turns a list of configuration files into a reading. Files that can't be read become notes;
/// the reading fails only when no VM could be read at all.
fn collect(
    reader: &str,
    app: &str,
    files: Vec<PathBuf>,
    parse: fn(&str) -> Result<VmConfig, String>,
    name_of: fn(&Path) -> String,
    env: &Env,
    searched: &str,
) -> Result<Reading, ReadError> {
    let mut items = Vec::new();
    let mut notes = Vec::new();
    let mut last_error = None;
    let mut seen: Vec<PathBuf> = Vec::new();
    for path in files {
        if seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        let text = match read_text(&path) {
            Ok(Some(text)) => text,
            // A registered VM whose files were moved or deleted.
            Ok(None) => continue,
            Err(e) => {
                notes.push(format!("canfit {e}."));
                last_error = Some(e);
                continue;
            }
        };
        match parse(&text) {
            Ok(vm) => {
                let file = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                items.push(DeclaredItem {
                    name: vm.name.unwrap_or_else(|| name_of(&path)),
                    declared_bytes: vm.memory_mib * MIB,
                    declared_provenance: Provenance::ReadFromConfig,
                    detail: format!("{} MB in {file}", vm.memory_mib),
                    overhead_bytes: 0,
                    overhead_detail: None,
                    path: Some(path),
                });
            }
            Err(detail) => {
                notes.push(format!(
                    "canfit could not read settings from {}: {detail}.",
                    display_path(&path, env)
                ));
                last_error = Some(ReadError::Unreadable { path, detail });
            }
        }
    }
    if items.is_empty() {
        return Err(last_error.unwrap_or_else(|| {
            ReadError::NotFound(format!("{app} has no virtual machines in {searched}"))
        }));
    }
    Ok(Reading {
        reader: reader.to_string(),
        source: format!("{app} virtual machines"),
        kind: ReadingKind::VirtualMachines,
        items,
        confidence: Confidence::High,
        notes,
    })
}

/// `config.plist` (or `config.pvs`) inside each bundle folder with `ext` in `dir`.
fn bundles(dir: &Path, ext: &str, config: &str) -> Vec<PathBuf> {
    list_dir(dir)
        .into_iter()
        .filter(|p| p.is_dir() && has_ext(p, ext))
        .map(|p| p.join(config))
        .collect()
}

/// The bundle folder's name without its extension (`Ubuntu.utm/config.plist` → `Ubuntu`).
fn bundle_name(config: &Path) -> String {
    config.parent().map(stem).unwrap_or_default()
}

/// Files with `ext` one level down in each subfolder of `dir` (`dir/<VM>/<VM>.vmx`).
fn nested_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    list_dir(dir)
        .into_iter()
        .filter(|p| p.is_dir())
        .flat_map(|vm| list_dir(&vm).into_iter().filter(|f| has_ext(f, ext)))
        .collect()
}

pub fn read_utm(env: &Env) -> Result<Reading, ReadError> {
    if env.os != Os::Macos {
        return Err(ReadError::UnsupportedOs);
    }
    let dir = env
        .home
        .join("Library/Containers/com.utmapp.UTM/Data/Documents");
    let files = bundles(&dir, ".utm", "config.plist");
    let searched = display_path(&dir, env);
    let mut reading = collect("utm", "UTM", files, parse_utm, bundle_name, env, &searched)?;
    reading.notes.push(
        "canfit reads the VMs in UTM's default folder; VMs opened from elsewhere are not listed."
            .to_string(),
    );
    Ok(reading)
}

pub fn read_parallels(env: &Env) -> Result<Reading, ReadError> {
    if env.os != Os::Macos {
        return Err(ReadError::UnsupportedOs);
    }
    let dir = env.home.join("Parallels");
    let files = bundles(&dir, ".pvm", "config.pvs");
    let searched = display_path(&dir, env);
    collect(
        "parallels",
        "Parallels Desktop",
        files,
        parse_parallels,
        bundle_name,
        env,
        &searched,
    )
}

pub fn read_vmware(env: &Env) -> Result<Reading, ReadError> {
    let (inventory, defaults): (PathBuf, Vec<PathBuf>) = match env.os {
        Os::Macos => (
            env.config.join("VMware Fusion").join("vmInventory"),
            vec![
                env.home.join("Virtual Machines.localized"),
                env.home.join("Virtual Machines"),
            ],
        ),
        Os::Windows => (
            env.config.join("VMware").join("inventory.vmls"),
            vec![env.home.join("Documents").join("Virtual Machines")],
        ),
        Os::Linux => (
            env.home.join(".vmware").join("inventory.vmls"),
            vec![env.home.join("vmware")],
        ),
    };
    let mut files = match read_text(&inventory) {
        Ok(Some(text)) => parse_vmware_inventory(&text),
        _ => Vec::new(),
    };
    for dir in &defaults {
        // macOS keeps each VM in a `.vmwarevm` bundle; elsewhere in a plain folder.
        files.extend(nested_files(dir, ".vmx"));
    }
    let app = if env.os == Os::Macos {
        "VMware Fusion"
    } else {
        "VMware Workstation"
    };
    let searched = display_path(&defaults[0], env);
    collect("vmware", app, files, parse_vmx, stem, env, &searched)
}

pub fn read_virtualbox(env: &Env) -> Result<Reading, ReadError> {
    let registry = match env.os {
        Os::Macos => env.home.join("Library").join("VirtualBox"),
        Os::Windows => env.home.join(".VirtualBox"),
        Os::Linux => env.config.join("VirtualBox"),
    }
    .join("VirtualBox.xml");
    let mut files = Vec::new();
    let mut notes = Vec::new();
    match read_text(&registry) {
        Ok(Some(text)) => match parse_virtualbox_registry(&text) {
            Ok(paths) => {
                let base = registry.parent().unwrap_or(Path::new(""));
                files.extend(paths.into_iter().map(|p| base.join(p)));
            }
            Err(detail) => notes.push(format!(
                "canfit could not read VirtualBox's list of VMs ({}): {detail}.",
                display_path(&registry, env)
            )),
        },
        Ok(None) => {}
        Err(e) => notes.push(format!("canfit {e}.")),
    }
    let default_dir = env.home.join("VirtualBox VMs");
    files.extend(nested_files(&default_dir, ".vbox"));
    let searched = display_path(&default_dir, env);
    let mut reading = collect(
        "virtualbox",
        "VirtualBox",
        files,
        parse_vbox,
        stem,
        env,
        &searched,
    )?;
    notes.append(&mut reading.notes);
    reading.notes = notes;
    Ok(reading)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::apps::readers::tests::{GIB, MIB as M, env, fixture};

    fn names_and_sizes(r: &Reading) -> Vec<(String, u64)> {
        r.items
            .iter()
            .map(|i| (i.name.clone(), i.declared_bytes / M))
            .collect()
    }

    #[test]
    fn utm_v4_plists_and_older_layouts() {
        let text = std::fs::read_to_string(fixture("macos").join(
            "Library/Containers/com.utmapp.UTM/Data/Documents/Ubuntu 24.04.utm/config.plist",
        ))
        .unwrap();
        assert_eq!(
            parse_utm(&text),
            Ok(VmConfig {
                name: Some("Ubuntu 24.04".to_string()),
                memory_mib: 8192
            })
        );
        let old = std::fs::read_to_string(
            fixture("macos-new")
                .join("Library/Containers/com.utmapp.UTM/Data/Documents/Old.utm/config.plist"),
        )
        .unwrap();
        assert!(parse_utm(&old).unwrap_err().contains("older UTM layout"));
        assert!(
            parse_utm(
                "<plist><dict><key>ConfigurationVersion</key><integer>4</integer></dict></plist>"
            )
            .unwrap_err()
            .contains("MemorySize")
        );
        assert!(parse_utm("<plist><dict>").is_err());
    }

    #[test]
    fn utm_lists_every_vm_and_notes_unreadable_ones() {
        let r = read_utm(&env(Os::Macos, "macos")).unwrap();
        assert_eq!(r.kind, ReadingKind::VirtualMachines);
        assert_eq!(
            names_and_sizes(&r),
            [
                ("Kali Linux".to_string(), 4096),
                ("Metasploitable 2".to_string(), 2048),
                ("Ubuntu 24.04".to_string(), 8192)
            ]
        );
        assert_eq!(r.items[2].detail, "8192 MB in config.plist");
        // One VM in the old layout, one malformed: the readable one is listed, the rest noted.
        let r = read_utm(&env(Os::Macos, "macos-new")).unwrap();
        assert_eq!(names_and_sizes(&r), [("Current".to_string(), 3072)]);
        assert!(
            r.notes.iter().any(|n| n.contains("older UTM layout")),
            "{:?}",
            r.notes
        );
        assert!(
            r.notes.iter().any(|n| n.contains("malformed")),
            "{:?}",
            r.notes
        );
        assert!(matches!(
            read_utm(&env(Os::Macos, "empty")),
            Err(ReadError::NotFound(_))
        ));
        assert!(matches!(
            read_utm(&env(Os::Linux, "macos")),
            Err(ReadError::UnsupportedOs)
        ));
    }

    #[test]
    fn parallels_pvs() {
        let r = read_parallels(&env(Os::Macos, "macos")).unwrap();
        assert_eq!(names_and_sizes(&r), [("Windows 11".to_string(), 8192)]);
        assert!(parse_parallels("<ParallelsVirtualMachine/>").is_err());
        assert!(
            parse_parallels(
                "<ParallelsVirtualMachine><Hardware><Memory><RAM>lots</RAM></Memory></Hardware></ParallelsVirtualMachine>"
            )
            .unwrap_err()
            .contains("lots")
        );
    }

    #[test]
    fn vmware_vmx_and_inventory() {
        assert_eq!(
            parse_vmx("displayName = \"Ubuntu\"\nmemsize = \"4096\"\n"),
            Ok(VmConfig {
                name: Some("Ubuntu".to_string()),
                memory_mib: 4096
            })
        );
        assert!(parse_vmx("displayName = \"x\"").is_err());
        assert!(parse_vmx("memsize = \"a lot\"").is_err());
        assert_eq!(
            parse_vmware_inventory(
                ".encoding = \"UTF-8\"\nvmlist1.config = \"/vms/a.vmx\"\nvmlist1.DisplayName = \"a\"\nvmlist2.config = \"C:\\VMs\\b.vmx\"\nvmlist3.config = \"\"\n"
            ),
            [PathBuf::from("/vms/a.vmx"), PathBuf::from("C:\\VMs\\b.vmx")]
        );
        let r = read_vmware(&env(Os::Macos, "macos")).unwrap();
        assert_eq!(names_and_sizes(&r), [("Fedora".to_string(), 4096)]);
        let r = read_vmware(&env(Os::Windows, "windows")).unwrap();
        assert_eq!(names_and_sizes(&r), [("Windows Server".to_string(), 6144)]);
        assert!(r.source.contains("VMware Workstation"));
        let r = read_vmware(&env(Os::Linux, "linux")).unwrap();
        assert_eq!(names_and_sizes(&r), [("Arch".to_string(), 2048)]);
    }

    #[test]
    fn virtualbox_registry_and_default_folder() {
        assert!(parse_vbox("<VirtualBox/>").unwrap_err().contains("Machine"));
        assert!(
            parse_vbox("<VirtualBox><Machine name=\"x\"><Hardware/></Machine></VirtualBox>")
                .unwrap_err()
                .contains("RAMSize")
        );
        // Linux: one VM registered by a relative path, one in the default folder.
        let r = read_virtualbox(&env(Os::Linux, "linux")).unwrap();
        assert_eq!(
            names_and_sizes(&r),
            [
                ("Registered".to_string(), 1024),
                ("Debian".to_string(), 2048)
            ]
        );
        let r = read_virtualbox(&env(Os::Macos, "macos")).unwrap();
        assert_eq!(names_and_sizes(&r), [("Debian".to_string(), 2048)]);
        let r = read_virtualbox(&env(Os::Windows, "windows")).unwrap();
        assert_eq!(names_and_sizes(&r), [("Win10".to_string(), 4096)]);
        // A malformed registry is noted; a malformed .vbox fails when nothing else is readable.
        let err = read_virtualbox(&env(Os::Linux, "broken")).unwrap_err();
        assert!(matches!(err, ReadError::Unreadable { .. }), "{err:?}");
        let total: u64 = r.items.iter().map(|i| i.declared_bytes).sum();
        assert_eq!(total, 4 * GIB);
    }
}
