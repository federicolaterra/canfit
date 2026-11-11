//! The generic JSON reader: one number in an app's JSON settings file, for community profiles
//! of apps that store their memory setting that way.

use serde_json::Value;

use super::{PerOs, display_path, read_text, unit_factor};
use crate::core::Provenance;
use crate::estimate::apps::{Confidence, DeclaredItem, Env, ReadError, Reading, ReadingKind};

/// The number at the dotted `key` (each part matched ignoring case).
pub fn lookup(text: &str, key: &str) -> Result<u64, String> {
    let mut value: Value =
        serde_json::from_str(text).map_err(|e| format!("not valid JSON ({e})"))?;
    for part in key.split('.') {
        let Value::Object(object) = value else {
            return Err(format!("no {key} setting"));
        };
        value = object
            .into_iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(part))
            .map(|(_, v)| v)
            .ok_or_else(|| format!("no {key} setting"))?;
    }
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("{key} is not a positive number ({value})"))
}

pub fn read(
    key: &str,
    unit: &str,
    label: &str,
    paths: &PerOs,
    env: &Env,
) -> Result<Reading, ReadError> {
    let factor = unit_factor(unit).ok_or(ReadError::UnsupportedOs)?;
    let templates = paths.for_os(env.os);
    if templates.is_empty() {
        return Err(ReadError::UnsupportedOs);
    }
    for template in templates {
        let path = env.expand(template);
        let Some(text) = read_text(&path)? else {
            continue;
        };
        let number = lookup(&text, key).map_err(|detail| ReadError::Unreadable {
            path: path.clone(),
            detail,
        })?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Ok(Reading {
            reader: "json".to_string(),
            source: format!("settings ({name})"),
            kind: ReadingKind::App,
            items: vec![DeclaredItem {
                name: label.to_string(),
                declared_bytes: number.saturating_mul(factor),
                declared_provenance: Provenance::ReadFromConfig,
                detail: format!("{key} = {number} {unit} in {name}"),
                overhead_bytes: 0,
                overhead_detail: None,
                path: Some(path),
            }],
            confidence: Confidence::High,
            notes: Vec::new(),
        });
    }
    let first = env.expand(&templates[0]);
    Err(ReadError::NotFound(format!(
        "no settings file at {}",
        display_path(&first, env)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::apps::Os;
    use crate::estimate::apps::readers::tests::{GIB, env};

    #[test]
    fn dotted_keys_ignore_case() {
        let text = r#"{"vm": {"MemoryMiB": 2048, "label": "x"}, "n": "512"}"#;
        assert_eq!(lookup(text, "vm.memorymib"), Ok(2048));
        assert_eq!(lookup(text, "n"), Ok(512));
        assert!(lookup(text, "vm.cpus").is_err());
        assert!(lookup(text, "vm.label").is_err());
        assert!(lookup(text, "vm.memorymib.x").is_err());
        assert!(lookup("{", "x").is_err());
    }

    #[test]
    fn reads_the_first_existing_file() {
        let paths = PerOs {
            macos: vec![
                "{home}/missing.json".to_string(),
                "{config}/Example/settings.json".to_string(),
            ],
            ..PerOs::default()
        };
        let r = read(
            "vm.memoryMiB",
            "MiB",
            "Example VM",
            &paths,
            &env(Os::Macos, "macos"),
        )
        .unwrap();
        assert_eq!(r.items[0].declared_bytes, 3 * GIB);
        assert_eq!(r.items[0].name, "Example VM");
        assert!(matches!(
            read("x", "MiB", "x", &paths, &env(Os::Linux, "macos")),
            Err(ReadError::UnsupportedOs)
        ));
    }
}
