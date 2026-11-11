//! End-to-end checks of `canfit update`. Nothing here touches
//! the network: the updater next to canfit is a stand-in script that only records it was run.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run(exe: &Path, args: &[&str]) -> Output {
    Command::new(exe)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run canfit")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Only the Unix installer test needs a scratch folder (Windows CI denies dead code).
#[cfg(unix)]
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("canfit-cli-update-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_build_tree_copy_says_it_cannot_update_itself() {
    let exe = Path::new(env!("CARGO_BIN_EXE_canfit"));
    let out = run(exe, &["update"]);
    assert_eq!(code(&out), 0, "{out:?}");
    let text = stdout(&out);
    assert!(
        text.starts_with("canfit ") && text.contains("can't tell how it was installed"),
        "{text}"
    );
    assert!(text.contains("releases/latest"), "{text}");

    let out = run(exe, &["update", "--json"]);
    assert_eq!(code(&out), 0, "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(v["schema"], "canfit.update");
    assert_eq!(v["install_method"], "unknown");
    assert_eq!(v["self_update"], false);
    assert_eq!(v["canfit_version"], env!("CARGO_PKG_VERSION"));
}

/// An installer copy: canfit with `canfit-update` beside it. Without a terminal and without
/// `--yes` it must not run the updater; with `--yes` it hands over and reports the result.
#[cfg(unix)]
#[test]
fn the_installer_copy_hands_over_to_the_updater_only_when_confirmed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("installer");
    let exe = dir.join("canfit");
    std::fs::copy(env!("CARGO_BIN_EXE_canfit"), &exe).unwrap();
    let marker = dir.join("updater-ran");
    let updater = dir.join("canfit-update");
    std::fs::write(
        &updater,
        format!("#!/bin/sh\necho updated > '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&updater, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = run(&exe, &["update", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(v["install_method"], "installer");
    assert_eq!(v["self_update"], true);
    assert!(!marker.exists(), "--json must never update");

    let out = run(&exe, &["update"]);
    assert_eq!(code(&out), 3, "{out:?}");
    assert!(
        stdout(&out).contains("github.com"),
        "says what connects first"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("--yes"));
    assert!(!marker.exists(), "no confirmation, no update");

    let out = run(&exe, &["update", "--yes"]);
    assert_eq!(code(&out), 0, "{out:?}");
    assert!(marker.exists(), "the updater ran");
    assert!(stdout(&out).contains("canfit --version"));

    std::fs::write(&updater, "#!/bin/sh\nexit 7\n").unwrap();
    let out = run(&exe, &["update", "--yes"]);
    assert_eq!(code(&out), 3, "a failed update is a runtime error: {out:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
