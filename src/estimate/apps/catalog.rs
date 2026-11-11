//! The seed catalog (spec §9 source 3, §16; ADR-021): rough average memory for common apps canfit
//! has no settings for and has never seen. Embedded in the binary (`catalog.toml`); values are
//! always labelled "estimate from catalog", low confidence, never measured.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::Os;
use super::profiles::Matchers;
use super::resolve::normalize;

const EMBEDDED: &str = include_str!("catalog.toml");

/// Supported version of the catalog layout.
pub const CATALOG_SCHEMA_VERSION: u32 = 1;

/// Allowed categories.
pub const CATEGORIES: &[&str] = &[
    "browser", "chat", "meeting", "music", "video", "editor", "ide", "design", "office", "notes",
    "mail", "ai", "game", "other",
];

/// Sane bounds for catalog values, in MiB.
pub const MIN_MB: u64 = 50;
pub const MAX_MB: u64 = 16 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    schema_version: u32,
    reviewed: String,
    #[serde(rename = "app", default)]
    apps: Vec<CatalogEntry>,
}

/// One app of the catalog.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// The OSes the app runs on; empty means all three.
    #[serde(default)]
    pub os: Vec<String>,
    pub category: String,
    pub typical_mb: u64,
    pub need_mb: u64,
    /// App group or process names that identify the running app.
    pub processes: Vec<String>,
    pub source: String,
}

impl CatalogEntry {
    pub fn runs_on(&self, os: Os) -> bool {
        self.os.is_empty() || self.os.iter().any(|o| o == os.key())
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.aliases.iter().map(String::as_str))
    }

    /// Matchers that find the running app among app groups.
    pub fn matchers(&self) -> Matchers {
        Matchers {
            bundles: Vec::new(),
            names: self.processes.clone(),
            shared: Vec::new(),
        }
    }

    pub fn need_bytes(&self) -> u64 {
        self.need_mb * 1024 * 1024
    }

    pub fn typical_bytes(&self) -> u64 {
        self.typical_mb * 1024 * 1024
    }
}

/// The catalog and the date its values were last reviewed.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub reviewed: String,
    pub apps: Vec<CatalogEntry>,
}

impl Catalog {
    /// The entry `name` names on `os` (name, alias or process name, ignoring case).
    pub fn find(&self, name: &str, os: Os) -> Option<&CatalogEntry> {
        let wanted = normalize(name);
        let here = || self.apps.iter().filter(|a| a.runs_on(os));
        here()
            .find(|a| a.names().any(|n| normalize(n) == wanted))
            .or_else(|| here().find(|a| a.processes.iter().any(|p| normalize(p) == wanted)))
    }

    /// The entry for an app on another OS only.
    pub fn find_elsewhere(&self, name: &str) -> Option<&CatalogEntry> {
        let wanted = normalize(name);
        self.apps
            .iter()
            .find(|a| a.names().any(|n| normalize(n) == wanted))
    }
}

fn parse(text: &str) -> Result<Catalog, String> {
    let file: CatalogFile = toml::from_str(text).map_err(|e| e.to_string())?;
    if file.schema_version != CATALOG_SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {} (this canfit reads {CATALOG_SCHEMA_VERSION})",
            file.schema_version
        ));
    }
    validate(&file.apps)?;
    Ok(Catalog {
        reviewed: file.reviewed,
        apps: file.apps,
    })
}

fn validate(apps: &[CatalogEntry]) -> Result<(), String> {
    let mut seen: HashSet<String> = HashSet::new();
    for a in apps {
        if a.name.trim().is_empty() {
            return Err("an [[app]] has an empty name".to_string());
        }
        for n in a.names() {
            if !seen.insert(normalize(n)) {
                return Err(format!("\"{n}\" is listed twice"));
            }
        }
        if !CATEGORIES.contains(&a.category.as_str()) {
            return Err(format!(
                "\"{}\": unknown category \"{}\"",
                a.name, a.category
            ));
        }
        for os in &a.os {
            if Os::from_key(os).is_none() {
                return Err(format!("\"{}\": unknown OS \"{os}\"", a.name));
            }
        }
        if !(MIN_MB..=MAX_MB).contains(&a.typical_mb) || !(MIN_MB..=MAX_MB).contains(&a.need_mb) {
            return Err(format!(
                "\"{}\": values must be between {MIN_MB} and {MAX_MB} MiB",
                a.name
            ));
        }
        if a.need_mb < a.typical_mb {
            return Err(format!("\"{}\": need_mb is below typical_mb", a.name));
        }
        if a.processes.iter().all(|p| p.trim().is_empty()) {
            return Err(format!("\"{}\": no process names", a.name));
        }
        if a.source.trim().is_empty() {
            return Err(format!("\"{}\": no source", a.name));
        }
    }
    Ok(())
}

/// The catalog built into canfit.
pub fn embedded() -> Catalog {
    parse(EMBEDDED).expect("the built-in catalog is valid (tested)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::apps::profiles;

    #[test]
    fn the_embedded_catalog_is_valid_and_sized() {
        let c = parse(EMBEDDED).unwrap();
        assert!((20..=40).contains(&c.apps.len()), "{}", c.apps.len());
        assert!(!c.reviewed.is_empty());
        // Every category used at least once is a real one, and the spread covers the brief.
        for cat in ["browser", "chat", "music", "editor", "design"] {
            assert!(c.apps.iter().any(|a| a.category == cat), "{cat}");
        }
    }

    #[test]
    fn catalog_names_do_not_shadow_profiles() {
        // Profiles (declared configuration) come first; a catalog name equal to a profile name
        // would never be used and would confuse contributors.
        let c = embedded();
        for p in profiles::embedded() {
            for n in p.names() {
                assert!(
                    c.find_elsewhere(n).is_none(),
                    "{n} is both a profile and a catalog entry"
                );
            }
        }
    }

    #[test]
    fn lookup_by_name_alias_and_process() {
        let c = embedded();
        assert_eq!(c.find("chrome", Os::Linux).unwrap().name, "Google Chrome");
        assert_eq!(
            c.find("VS  code", Os::Macos).unwrap().name,
            "Visual Studio Code"
        );
        assert_eq!(c.find("slack.exe", Os::Windows).unwrap().name, "Slack");
        assert_eq!(
            c.find("msedge", Os::Windows).unwrap().name,
            "Microsoft Edge"
        );
        assert!(c.find("Safari", Os::Linux).is_none());
        assert_eq!(c.find_elsewhere("Safari").unwrap().name, "Safari");
        assert!(c.find("no such app", Os::Macos).is_none());
    }

    #[test]
    fn invalid_catalogs_are_refused() {
        let entry = |extra: &str| {
            format!(
                "schema_version = 1\nreviewed = \"x\"\n[[app]]\nname = \"A\"\ncategory = \"chat\"\n\
                 typical_mb = 100\nneed_mb = 200\nprocesses = [\"a\"]\nsource = \"s\"\n{extra}"
            )
        };
        assert!(parse(&entry("")).is_ok());
        for (bad, why) in [
            (entry("aliases = [\"a\"]"), "listed twice"),
            (entry("os = [\"beos\"]"), "unknown OS"),
            (entry("color = 1"), "unknown field"),
            (entry("").replace("chat", "toys"), "unknown category"),
            (
                entry("").replace("need_mb = 200", "need_mb = 40"),
                "between",
            ),
            (entry("").replace("need_mb = 200", "need_mb = 99"), "below"),
            (
                entry("").replace("need_mb = 200", "need_mb = 99999"),
                "between",
            ),
            (entry("").replace("[\"a\"]", "[]"), "no process"),
            (
                entry("").replace("schema_version = 1", "schema_version = 2"),
                "schema_version",
            ),
        ] {
            let err = parse(&bad).unwrap_err();
            assert!(err.contains(why), "{why}: {err}");
        }
    }
}
