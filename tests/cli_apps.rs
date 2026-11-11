//! End-to-end checks of app targets (ADR-020, ADR-021): resolution errors, user profiles, fixture
//! homes, the catalog, several targets together, the JSON document and exit codes. They read this
//! machine's memory and processes but only the fixture settings under `tests/fixtures/apps/` and
//! an empty history in a temporary folder, and never close anything.

#[path = "support/gguf_builder.rs"]
mod gguf_builder;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[cfg(any(target_os = "macos", target_os = "linux"))]
const GIB: u64 = 1024 * 1024 * 1024;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("canfit-apps-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs canfit with its config (and so `apps.toml`) in `config_dir`.
fn canfit(args: &[&str], config_dir: &Path, envs: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_canfit"));
    cmd.args(args)
        .env("CANFIT_CONFIG", config_dir.join("config.toml"))
        // Never this machine's history: estimates must not depend on the developer's apps.
        .env("CANFIT_DATA_DIR", config_dir.join("data"))
        .env_remove("CANFIT_APPS")
        .stdin(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("run canfit")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/apps")
        .join(name)
}

/// The documented `canfit.fit` fields for an app (docs/open-source/usage.md).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn assert_app_document(out: &Output) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON output");
    assert_eq!(v["schema"], "canfit.fit");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["target_kind"], "app");
    assert_eq!(v["exit_code"], code(out), "JSON and exit code agree");
    assert!(["fits", "tight", "doesnt_fit"].contains(&v["verdict"].as_str().unwrap()));
    assert!(
        ["fits comfortably", "fits but will slow down", "at risk"]
            .contains(&v["verdict_label"].as_str().unwrap())
    );
    assert!(v["summary"].as_str().unwrap().len() > 20);
    assert_eq!(v["deciding_limit"], "available_memory");
    assert_eq!(
        v["checks"].as_array().unwrap().len(),
        1,
        "no GPU limit for apps"
    );
    for key in [
        "base_bytes",
        "total_bytes",
        "margin_bytes",
        "overhead_bytes",
    ] {
        assert!(v["needed"][key].is_u64(), "needed.{key}");
    }
    assert!(v["models"].as_array().unwrap().is_empty());
    let app = &v["apps"][0];
    assert_eq!(app["source"], "config");
    assert!(["high", "medium", "low"].contains(&app["confidence"].as_str().unwrap()));
    for key in [
        "declared_bytes",
        "need_bytes",
        "deciding_item",
        "margin_percent",
    ] {
        assert!(app[key].is_u64(), "apps[0].{key}");
    }
    assert!(app["notes"].is_array());
    let item = &app["reading"]["items"][0];
    assert!(item["declared_bytes"].is_u64());
    assert!(
        ["read_from_config", "estimated"].contains(&item["declared_provenance"].as_str().unwrap())
    );
    v
}

#[test]
fn unknown_apps_exit_3_with_close_matches() {
    let dir = temp_dir("unknown");
    let out = canfit(&["Dokcer", "--json"], &dir, &[]);
    assert_eq!(code(&out), 3);
    assert!(out.stdout.is_empty());
    let err = stderr(&out);
    assert!(
        err.contains("doesn't know an app called \"Dokcer\""),
        "{err}"
    );
    assert!(err.contains("Did you mean \"Docker Desktop\"?"), "{err}");
    // A reserved word checked as an app.
    let out = canfit(&["--app", "history"], &dir, &[]);
    assert_eq!(code(&out), 3);
    assert!(stderr(&out).contains("\"history\""));
}

#[test]
fn truly_unknown_apps_exit_3() {
    let dir = temp_dir("truly-unknown");
    let out = canfit(&["Qzxwv Studio Pro"], &dir, &[]);
    assert_eq!(code(&out), 3);
    let err = stderr(&out);
    assert!(
        err.contains("no settings, history or catalog value for it"),
        "{err}"
    );
    assert!(
        !dir.join("data").exists(),
        "a check never creates the history"
    );
}

/// An app canfit never saw: the seed catalog's estimate, labelled, low confidence.
#[test]
fn unseen_common_apps_use_the_catalog() {
    let dir = temp_dir("catalog");
    let out = canfit(&["spotify", "--json"], &dir, &[]);
    assert!([0, 1, 2].contains(&code(&out)), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON output");
    assert_eq!(v["target_kind"], "app");
    assert_eq!(v["exit_code"], code(&out));
    let app = &v["apps"][0];
    assert_eq!(app["name"], "Spotify");
    assert_eq!(app["source"], "catalog");
    assert_eq!(app["confidence"], "low");
    assert_eq!(app["catalog"]["provenance"], "estimated");
    assert_eq!(app["catalog"]["category"], "music");
    assert_eq!(
        app["reading"]["items"][0]["declared_provenance"],
        "estimated"
    );
    let out = canfit(&["Spotify"], &dir, &[]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("estimate from catalog"), "{text}");
    assert!(text.contains("confidence low"), "{text}");
}

/// Several targets: each listed with its source, one total, one verdict and exit code.
#[test]
fn a_model_and_apps_are_checked_together() {
    let dir = temp_dir("combined");
    let model = dir.join("tiny.gguf");
    gguf_builder::Gguf::llama("tiny", 2).write(&model);
    let model = model.to_string_lossy().into_owned();
    let out = canfit(&["Spotify", &model, "Slack", "--json"], &dir, &[]);
    assert!([0, 1, 2].contains(&code(&out)), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON output");
    assert_eq!(v["target_kind"], "combined");
    assert_eq!(v["exit_code"], code(&out));
    assert_eq!(v["models"][0]["name"], "tiny");
    let apps = v["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 2);
    assert_eq!(apps[0]["name"], "Spotify");
    assert_eq!(apps[1]["name"], "Slack");
    let app_needs: u64 = apps.iter().map(|a| a["need_bytes"].as_u64().unwrap()).sum();
    assert_eq!(v["needed"]["apps_bytes"], app_needs);
    let model_total = v["models"][0]["estimate"]["total_bytes"].as_u64().unwrap();
    assert!(v["needed"]["total_bytes"].as_u64().unwrap() > model_total + app_needs);
    // On Apple Silicon the GPU check covers the model only.
    if let Some(gpu) = v["checks"].as_array().unwrap().get(1) {
        assert_eq!(gpu["limit"], "gpu_memory_limit");
        let headroom = gpu["headroom_bytes"].as_i64().unwrap();
        let limit = gpu["limit_bytes"].as_i64().unwrap();
        assert_eq!(headroom, limit - model_total as i64);
    }
    let out = canfit(&["Spotify", &model], &dir, &[]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Can tiny + Spotify fit?"), "{text}");
    assert!(text.contains("fit together"), "{text}");

    // Make room for a combination never suggests the targets themselves.
    let out = canfit(&["free", "Spotify", "Slack", &model, "--json"], &dir, &[]);
    assert!([0, 1, 2].contains(&code(&out)), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for a in v["make_room"]["apps"].as_array().unwrap() {
        assert!(
            a["name"] != "Spotify" && a["name"] != "Slack",
            "suggested a target: {a}"
        );
        assert!(a["manual_commands"].is_array());
    }
}

#[test]
fn a_broken_user_profiles_file_is_an_error() {
    let dir = temp_dir("broken-profiles");
    std::fs::write(
        dir.join("apps.toml"),
        "schema_version = 1\n[[app]]\nname = 1\n",
    )
    .unwrap();
    let out = canfit(&["Docker Desktop"], &dir, &[]);
    assert_eq!(code(&out), 3);
    assert!(
        stderr(&out).contains("app profiles file"),
        "{}",
        stderr(&out)
    );
}

/// A user profile with the generic JSON reader, on every OS.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
#[test]
fn user_profiles_read_their_settings() {
    let dir = temp_dir("user-profile");
    let settings = dir.join("example-settings.json");
    std::fs::write(&settings, r#"{"vm": {"memoryMiB": 1}}"#).unwrap();
    let path = settings.to_string_lossy().replace('\\', "/");
    let profile = format!(
        r#"schema_version = 1
[[app]]
name = "Example App"
aliases = ["example"]
readers = [{{ kind = "json", key = "vm.memoryMiB", unit = "MiB", label = "Example VM",
  paths = {{ macos = ["{path}"], windows = ["{path}"], linux = ["{path}"] }} }}]
"#
    );
    std::fs::write(dir.join("apps.toml"), profile).unwrap();
    let out = canfit(&["example", "--json"], &dir, &[]);
    assert_eq!(code(&out), 0, "1 MB fits: {}", stderr(&out));
    let v = assert_app_document(&out);
    assert_eq!(v["apps"][0]["name"], "Example App");
    assert_eq!(
        v["apps"][0]["reading"]["items"][0]["declared_bytes"],
        1024 * 1024
    );
    assert_eq!(v["apps"][0]["confidence"], "high");
    // Unreadable settings: exit 3 with a "could not read settings" sentence.
    std::fs::write(&settings, "{").unwrap();
    let out = canfit(&["Example App"], &dir, &[]);
    assert_eq!(code(&out), 3);
    assert!(
        stderr(&out).contains("could not read Example App's settings"),
        "{}",
        stderr(&out)
    );
    // The text answer starts with a sentence and shows the card.
    std::fs::write(&settings, r#"{"vm": {"memoryMiB": 1}}"#).unwrap();
    let out = canfit(&["Example App"], &dir, &[]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("● Yes, Example App fits comfortably"),
        "{text}"
    );
    assert!(text.contains("Can Example App fit?"), "{text}");
    assert!(text.contains("read from config"), "{text}");
}

/// The built-in readers against a fixture home (HOME can be redirected on Unix only).
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn built_in_readers_with_a_fixture_home() {
    let dir = temp_dir("fixture-home");
    let (home, docker_gib) = if cfg!(target_os = "macos") {
        (fixture("macos"), 10)
    } else {
        (fixture("linux"), 4)
    };
    let envs: [(&str, &Path); 3] = [
        ("HOME", &home),
        ("XDG_CONFIG_HOME", &home.join(".config")),
        ("XDG_DATA_HOME", &home.join(".local/share")),
    ];
    let out = canfit(&["Docker Desktop", "--json"], &dir, &envs);
    assert!([0, 1, 2].contains(&code(&out)), "{}", stderr(&out));
    let v = assert_app_document(&out);
    assert_eq!(
        v["apps"][0]["reading"]["items"][0]["declared_bytes"],
        docker_gib * GIB
    );
    assert_eq!(v["apps"][0]["confidence"], "high");

    let out = canfit(&["IntelliJ IDEA", "--json"], &dir, &envs);
    assert!([0, 1, 2].contains(&code(&out)), "{}", stderr(&out));
    let v = assert_app_document(&out);
    assert_eq!(v["apps"][0]["confidence"], "medium");
    assert_eq!(
        v["apps"][0]["reading"]["items"][0]["declared_bytes"],
        2 * GIB
    );

    let out = canfit(&["VirtualBox", "--json"], &dir, &envs);
    assert!([0, 1, 2].contains(&code(&out)), "{}", stderr(&out));
    let v = assert_app_document(&out);
    assert_eq!(v["apps"][0]["reading"]["kind"], "virtual_machines");

    if cfg!(target_os = "macos") {
        let out = canfit(&["UTM: ubuntu", "--json"], &dir, &envs);
        let v = assert_app_document(&out);
        assert_eq!(v["apps"][0]["name"], "UTM: Ubuntu 24.04");
        assert_eq!(v["apps"][0]["declared_bytes"], 8 * GIB);
        let out = canfit(&["UTM: windows"], &dir, &envs);
        assert_eq!(code(&out), 3);
        assert!(stderr(&out).contains("choose one of"), "{}", stderr(&out));
    } else {
        let out = canfit(&["UTM"], &dir, &envs);
        assert_eq!(code(&out), 3);
        assert!(
            stderr(&out).contains("runs on macOS only"),
            "{}",
            stderr(&out)
        );
    }

    // Make room for an app: the target itself is never suggested.
    let out = canfit(&["free", "Docker Desktop", "--json"], &dir, &envs);
    assert!([0, 1, 2].contains(&code(&out)), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let apps = v["make_room"]["apps"].as_array().unwrap();
    assert!(apps.iter().all(|a| a["name"] != "Docker"));

    // Settings that are missing: "could not read settings", exit 3.
    let empty = fixture("empty");
    let empty_envs: [(&str, &Path); 1] = [("HOME", &empty)];
    let out = canfit(&["Docker Desktop"], &dir, &empty_envs);
    assert_eq!(code(&out), 3);
    assert!(
        stderr(&out).contains("could not read Docker Desktop's settings"),
        "{}",
        stderr(&out)
    );
}
