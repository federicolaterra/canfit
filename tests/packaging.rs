//! The in-repository package manifests (Homebrew `Formula/canfit.rb`, Scoop
//! `packaging/scoop/canfit.json`) must be exactly what `packaging/render.sh` makes from
//! `packaging/release.env` and the templates in `packaging/templates/` (ADR-029).
//!
//! The rendering is a plain placeholder substitution, so it is repeated here to check it on every
//! OS without a shell; CI also runs `packaging/render.sh --check` on Linux.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const KEYS: [&str; 6] = [
    "VERSION",
    "SHA256_MACOS_AARCH64",
    "SHA256_MACOS_X86_64",
    "SHA256_LINUX_AARCH64",
    "SHA256_LINUX_X86_64",
    "SHA256_WINDOWS_ZIP",
];

const RELEASES: &str = "https://github.com/federicolaterra/canfit/releases/download";

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", full.display()))
        // A Windows checkout may turn LF into CRLF; the manifests are compared as text.
        .replace("\r\n", "\n")
}

fn source() -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in read("packaging/release.env").lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("release.env: not KEY=value: {line}"));
        assert!(KEYS.contains(&key), "release.env: unknown key {key}");
        values.insert(key.to_string(), value.to_string());
    }
    for key in KEYS {
        assert!(values.contains_key(key), "release.env: {key} is missing");
    }
    values
}

fn render(template: &str, values: &BTreeMap<String, String>) -> String {
    let mut text = read(template);
    for (key, value) in values {
        text = text.replace(&format!("{{{{{key}}}}}"), value);
    }
    assert!(!text.contains("{{"), "{template}: unknown placeholder left");
    text
}

fn version_triple(version: &str) -> (u64, u64, u64) {
    let core = version.split(['-', '+']).next().unwrap();
    let parts: Vec<u64> = core
        .split('.')
        .map(|p| {
            p.parse()
                .unwrap_or_else(|_| panic!("not a version: {version}"))
        })
        .collect();
    assert_eq!(parts.len(), 3, "not a version like 1.0.0: {version}");
    (parts[0], parts[1], parts[2])
}

#[test]
fn release_env_holds_a_version_and_five_checksums() {
    let values = source();
    let version = &values["VERSION"];
    // The manifests point at a published release, never at a version newer than this crate.
    assert!(
        version_triple(version) <= version_triple(env!("CARGO_PKG_VERSION")),
        "release.env version {version} is newer than Cargo.toml {}",
        env!("CARGO_PKG_VERSION")
    );
    for (key, value) in values.iter().filter(|(k, _)| k.starts_with("SHA256_")) {
        assert!(
            value.len() == 64
                && value
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{key} is not a lowercase SHA-256: {value}"
        );
    }
}

#[test]
fn committed_manifests_match_what_release_env_renders_to() {
    let values = source();
    for (template, output) in [
        ("packaging/templates/canfit.rb", "Formula/canfit.rb"),
        (
            "packaging/templates/canfit.json",
            "packaging/scoop/canfit.json",
        ),
    ] {
        assert!(
            render(template, &values) == read(output),
            "{output} is not what packaging/release.env renders to: run packaging/render.sh"
        );
    }
}

#[test]
fn homebrew_formula_covers_four_targets_with_their_checksums() {
    let values = source();
    let formula = read("Formula/canfit.rb");
    assert!(formula.contains("class Canfit < Formula"));
    assert!(formula.contains("bin.install \"canfit\""));
    let version = &values["VERSION"];
    for (target, key) in [
        ("aarch64-apple-darwin", "SHA256_MACOS_AARCH64"),
        ("x86_64-apple-darwin", "SHA256_MACOS_X86_64"),
        ("aarch64-unknown-linux-musl", "SHA256_LINUX_AARCH64"),
        ("x86_64-unknown-linux-musl", "SHA256_LINUX_X86_64"),
    ] {
        let url = format!("url \"{RELEASES}/v{version}/canfit-{target}.tar.xz\"");
        let sha = format!("sha256 \"{}\"", values[key]);
        let at = formula
            .find(&url)
            .unwrap_or_else(|| panic!("formula has no {url}"));
        // Each URL is followed by its own checksum (the next line).
        let next = formula[at..].lines().nth(1).unwrap_or_default().trim();
        assert_eq!(next, sha, "checksum after {target}");
    }
}

#[test]
fn scoop_manifest_is_valid_and_bucket_ready() {
    let values = source();
    let manifest: serde_json::Value =
        serde_json::from_str(&read("packaging/scoop/canfit.json")).expect("valid JSON");
    let version = &values["VERSION"];
    assert_eq!(manifest["version"], version.as_str());
    assert_eq!(manifest["license"], "MIT");
    assert_eq!(manifest["bin"], "canfit.exe");
    let x64 = &manifest["architecture"]["64bit"];
    assert_eq!(
        x64["url"],
        format!("{RELEASES}/v{version}/canfit-x86_64-pc-windows-msvc.zip").as_str()
    );
    assert_eq!(x64["hash"], values["SHA256_WINDOWS_ZIP"].as_str());
    // checkver + autoupdate let the manifest move to a bucket unchanged.
    assert_eq!(manifest["checkver"], "github");
    assert_eq!(
        manifest["homepage"],
        "https://github.com/federicolaterra/canfit"
    );
    let auto = &manifest["autoupdate"]["architecture"]["64bit"];
    assert_eq!(
        auto["url"],
        format!("{RELEASES}/v$version/canfit-x86_64-pc-windows-msvc.zip").as_str()
    );
    assert_eq!(auto["hash"]["url"], "$url.sha256");
}
