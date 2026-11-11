//! End-to-end checks of `canfit <target>`: exit codes, the JSON document and error handling.
//! Models are tiny synthetic GGUF files written by the shared builder.

#[path = "support/gguf_builder.rs"]
mod builder;

use std::path::PathBuf;
use std::process::{Command, Output};

use builder::Gguf;

fn canfit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_canfit"))
        .args(args)
        .output()
        .expect("run canfit")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("canfit-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

#[test]
fn usage_errors_exit_64() {
    for args in [
        &["m.gguf", "--ctx", "0"][..],
        &["m.gguf", "--cache-type", "f32"],
        &["m.gguf", "--margin", "500"],
        &["--no-such-flag"],
    ] {
        assert_eq!(code(&canfit(args)), 64, "{args:?}");
    }
}

#[test]
fn unreadable_targets_exit_3_with_an_explanation() {
    let dir = temp_dir("errors");
    let missing = dir.join("missing.gguf");
    let bogus = dir.join("bogus.gguf");
    std::fs::write(&bogus, b"not a model at all").unwrap();
    let truncated = dir.join("truncated.gguf");
    let bytes = Gguf::llama("t", 2).build();
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();

    for (target, needle) in [
        (missing.to_str().unwrap(), "can't find"),
        (bogus.to_str().unwrap(), "not a GGUF file"),
        (truncated.to_str().unwrap(), "truncated"),
        ("No Such App 42", "doesn't know an app called"),
    ] {
        let out = canfit(&[target]);
        assert_eq!(code(&out), 3, "{target}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(needle), "{target}: {stderr}");
        assert!(out.stdout.is_empty(), "{target}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Exit code and JSON agree, and the document has every documented field.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
#[test]
fn small_model_fit_check_and_json_schema() {
    let dir = temp_dir("fit");
    let model = dir.join("tiny.gguf");
    Gguf::llama("tiny", 2).write(&model);
    let path = model.to_str().unwrap();

    let text = canfit(&[path]);
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        stdout.starts_with("● Yes") || stdout.starts_with("▲ "),
        "{stdout}"
    );
    assert!(matches!(code(&text), 0 | 1), "{stdout}");

    let out = canfit(&[path, "--json", "--ctx", "2048", "--cache-type", "q8_0"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(doc["schema"], "canfit.fit");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["exit_code"].as_i64(), Some(i64::from(code(&out))));
    let verdict = doc["verdict"].as_str().unwrap();
    let expected = match verdict {
        "fits" => 0,
        "tight" => 1,
        "doesnt_fit" => 2,
        other => panic!("unknown verdict {other}"),
    };
    assert_eq!(code(&out), expected);
    for key in [
        "canfit_version",
        "summary",
        "deciding_limit",
        "pressure_level",
        "memory_total_bytes",
    ] {
        assert!(!doc[key].is_null(), "missing {key}");
    }
    for key in [
        "weights_bytes",
        "kv_cache_bytes",
        "overhead_bytes",
        "margin_bytes",
        "base_bytes",
        "total_bytes",
    ] {
        assert!(doc["needed"][key].is_u64(), "needed.{key}");
    }
    assert_eq!(doc["needed"]["provenance"], "estimated");
    let check = &doc["checks"][0];
    assert_eq!(check["limit"], "available_memory");
    assert_eq!(check["provenance"], "measured");
    assert!(check["headroom_bytes"].is_i64());
    let m = &doc["models"][0];
    assert_eq!(m["name"], "tiny");
    assert_eq!(m["quantization"], "Q4_K_M");
    assert_eq!(m["block_count"], 2);
    assert_eq!(m["estimate"]["context_tokens"], 2048);
    assert_eq!(m["estimate"]["context_source"], "flag");
    assert_eq!(m["estimate"]["cache_type"], "q8_0");
    assert_eq!(m["estimate"]["margin_percent"], 10);
    let _ = std::fs::remove_dir_all(dir);
}

/// A model far larger than any machine: exit 2, so `canfit m.gguf && run` never runs.
/// macOS and Linux only: NTFS allocates an extended file instead of leaving it sparse, so the
/// 576 GiB file can't be created on Windows test machines.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn huge_model_does_not_fit() {
    let dir = temp_dir("huge");
    let model = dir.join("huge.gguf");
    // 2^40 Q4_K elements = 576 GiB of weights in a sparse file.
    Gguf::llama("huge", 1)
        .tensor("blk.0.ffn.weight", &[1 << 20, 1 << 20], 12)
        .write_sparse(&model);
    let out = canfit(&[model.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(code(&out), 2, "{stdout}");
    assert!(stdout.starts_with("✗ No, huge doesn't fit"), "{stdout}");
    let _ = std::fs::remove_dir_all(dir);
}
