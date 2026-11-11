//! End-to-end checks of `canfit free` and `--make-room`. They read this machine's processes but
//! never close anything: stdin is not a terminal under `cargo test`, so canfit only reports.

#[path = "support/gguf_builder.rs"]
mod builder;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use builder::Gguf;

fn canfit(args: &[&str], config: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_canfit"))
        .args(args)
        .env("CANFIT_CONFIG", config)
        .stdin(Stdio::null())
        .output()
        .expect("run canfit")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("canfit-free-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).expect("JSON output")
}

#[test]
fn malformed_config_stops_canfit_instead_of_ignoring_protection() {
    let dir = temp_dir("bad-config");
    let config = dir.join("config.toml");
    std::fs::write(&config, "[processes]\nprotect = [\"Slack\"]\n").unwrap();
    let out = canfit(&["free", "--json"], &config);
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    if cfg!(any(
        target_os = "macos",
        target_os = "windows",
        target_os = "linux"
    )) {
        assert!(stderr.contains("which apps you protected"), "{stderr}");
    }
    assert!(out.stdout.is_empty());
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
#[test]
fn free_lists_suggestions_and_honours_the_protected_list() {
    let dir = temp_dir("protected");
    let config = dir.join("config.toml");
    let out = canfit(&["free", "--json"], &config);
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out);
    assert_eq!(v["schema"], "canfit.free");
    assert_eq!(v["schema_version"], 1);
    let suggestions = v["suggestions"].as_array().unwrap();
    for s in suggestions {
        assert_ne!(s["class"], "never", "{s}");
        // The test runner and its parents are canfit's ancestors: never suggested.
        assert!(
            !s["pids"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(std::process::id())),
            "{s}"
        );
    }

    // Protect everything that was suggested: none of it may come back.
    let names: Vec<String> = suggestions
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    let list: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
    std::fs::write(
        &config,
        format!("[processes]\nprotected = [{}]\n", list.join(", ")),
    )
    .unwrap();
    let v = json(&canfit(&["free", "--json"], &config));
    for s in v["suggestions"].as_array().unwrap() {
        assert!(
            !names.contains(&s["name"].as_str().unwrap().to_string()),
            "protected app suggested: {s}"
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
#[test]
fn make_room_and_free_target_share_one_answer() {
    let dir = temp_dir("make-room");
    let config = dir.join("config.toml");
    let model = dir.join("m.gguf");
    Gguf::llama("tiny", 2).write(&model);
    let model = model.to_str().unwrap();
    for args in [
        &[model, "--make-room", "--json"][..],
        &["free", model, "--json"],
    ] {
        let out = canfit(args, &config);
        let code = out.status.code().unwrap();
        assert!((0..=2).contains(&code), "{args:?}: {code}");
        let v = json(&out);
        assert_eq!(v["schema"], "canfit.fit");
        assert_eq!(v["exit_code"], code);
        let m = &v["make_room"];
        assert!(m["status"].is_string(), "{v}");
        assert_eq!(m["needed_provenance"], "estimated");
        for app in m["apps"].as_array().unwrap() {
            assert_ne!(app["class"], "never");
        }
    }
    // Without --make-room the document has no make_room field.
    let v = json(&canfit(&[model, "--json"], &config));
    assert!(v.get("make_room").is_none());
}
