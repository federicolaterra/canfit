//! End-to-end checks of `canfit models DIR`: the answer, the JSON document and folder errors.

#[path = "support/gguf_builder.rs"]
mod builder;

use std::path::PathBuf;
use std::process::{Command, Output};

use builder::Gguf;

fn canfit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_canfit"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("run canfit")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("canfit-cli-models-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

#[test]
fn lists_models_with_an_answer_first() {
    let dir = temp_dir("list");
    Gguf::llama("tiny", 2).write(&dir.join("tiny-q4.gguf"));
    Gguf::llama("small", 4).write(&dir.join("small-q4.gguf"));
    for n in 1..=2 {
        Gguf::llama("split", 2).write(&dir.join(format!("split-{n:05}-of-00002.gguf")));
    }
    let d = dir.to_str().unwrap();

    let out = canfit(&["models", d]);
    assert_eq!(code(&out), 0);
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap();
    assert!(first.contains("3 models in"), "{text}");
    assert!(text.contains("split (2 parts)"), "{text}");

    let out = canfit(&["models", d, "--json", "--ctx", "2048"]);
    assert_eq!(code(&out), 0);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], "canfit.models");
    assert_eq!(v["schema_version"], 1);
    let models = v["models"].as_array().unwrap();
    assert_eq!(models.len(), 3);
    assert_eq!(models[0]["estimate"]["context_tokens"], 2048);

    // A context far beyond any machine (about 2 TB of KV cache per model) makes nothing fit.
    // A huge context stands in for a huge file: NTFS would allocate a large sparse file for real.
    let out = canfit(&["models", d, "--ctx", "4000000000"]);
    assert_eq!(code(&out), 0, "a listing is not a fit check");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("✗ None of the 3 models in"), "{text}");
    let out = canfit(&["models", d, "--json", "--ctx", "4000000000"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["counts"]["doesnt_fit"], 3);
    assert_eq!(v["models"][0]["verdict"], "doesnt_fit");
}

#[test]
fn folder_errors_exit_3() {
    let dir = temp_dir("errors");
    let out = canfit(&["models", dir.join("missing").to_str().unwrap()]);
    assert_eq!(code(&out), 3);
    assert!(String::from_utf8_lossy(&out.stderr).contains("can't open"));
    let out = canfit(&["models", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 0);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("No GGUF models in"));
}
