//! End-to-end checks of `canfit watch` (ADR-023), on every OS in CI.
//!
//! This test binary has its own harness (`harness = false` in `Cargo.toml`) so it can also be the
//! helper program canfit watches: `cli_watch helper MODE ...` allocates a known amount of memory,
//! starts grandchildren, echoes its input, exits with a given code or ends itself. No real user
//! process is ever touched, and every run points `CANFIT_DATA_DIR` at a temporary folder.

use std::io::Write;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Child;
use std::process::{Command, Output, Stdio};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// The helper program.

fn helper(args: &[String]) -> i32 {
    let arg = |i: usize| -> u64 { args.get(i).and_then(|a| a.parse().ok()).unwrap_or(0) };
    match args.first().map(String::as_str) {
        // alloc MIB HOLD_MS [EXIT]: hold MIB of touched memory, then exit with EXIT.
        Some("alloc") => {
            let memory = touch(arg(1) * MIB);
            std::thread::sleep(Duration::from_millis(arg(2)));
            std::hint::black_box(&memory);
            arg(3) as i32
        }
        // spawn MIB HOLD_MS DEPTH: hold MIB, start a copy one level deeper, wait for it.
        Some("spawn") => {
            let memory = touch(arg(1) * MIB);
            let child = (arg(3) > 0).then(|| {
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "helper",
                        "spawn",
                        &arg(1).to_string(),
                        &arg(2).to_string(),
                        &(arg(3) - 1).to_string(),
                    ])
                    .spawn()
                    .unwrap()
            });
            std::thread::sleep(Duration::from_millis(arg(2)));
            std::hint::black_box(&memory);
            if let Some(mut c) = child {
                c.wait().unwrap();
            }
            0
        }
        // echo: one line from stdin to stdout, and a line on stderr.
        Some("echo") => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            println!("got: {}", line.trim_end());
            eprintln!("err line");
            0
        }
        // sleep MS
        Some("sleep") => {
            std::thread::sleep(Duration::from_millis(arg(1)));
            0
        }
        // selfkill: end itself with SIGKILL (Unix).
        Some("selfkill") => {
            let _ = Command::new("kill")
                .args(["-9", &std::process::id().to_string()])
                .status();
            std::thread::sleep(Duration::from_secs(5));
            1
        }
        other => {
            eprintln!("unknown helper mode {other:?}");
            99
        }
    }
}

/// `bytes` of memory with every page written, so the OS counts it as the process's own.
fn touch(bytes: u64) -> Vec<u8> {
    let mut v = vec![0u8; bytes as usize];
    for i in (0..v.len()).step_by(4096) {
        v[i] = 1;
    }
    v
}

// ---------------------------------------------------------------------------------------------
// Running canfit.

struct Env {
    data: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let data =
            std::env::temp_dir().join(format!("canfit-cli-watch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        Env { data }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_canfit"));
        c.args(args)
            .env("CANFIT_DATA_DIR", &self.data)
            .env("CANFIT_CONFIG", self.data.join("no-config.toml"))
            .env("CANFIT_APPS", self.data.join("no-apps.toml"))
            .env_remove("CANFIT_NO_HISTORY")
            .env("NO_COLOR", "1");
        c
    }

    fn run(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run canfit");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

fn helper_exe() -> String {
    std::env::current_exe().unwrap().display().to_string()
}

fn watch_args<'a>(extra: &[&'a str], exe: &'a str, helper: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["watch"];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", exe, "helper"]);
    args.extend_from_slice(helper);
    args
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Waits until canfit's stderr says it is watching, returning the reader and what it read.
#[cfg(unix)]
fn wait_until_watching(child: &mut Child) -> (BufReader<std::process::ChildStderr>, String) {
    let mut reader = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        seen.push_str(&line);
        if line.contains("Watching") {
            break;
        }
    }
    assert!(seen.contains("Watching"), "{seen}");
    (reader, seen)
}

// ---------------------------------------------------------------------------------------------
// Tests.

fn input_and_output_pass_through() {
    let env = Env::new("io");
    let exe = helper_exe();
    let out = env.run(&watch_args(&[], &exe, &["echo"]), "hello\n");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    // stdout is the command's alone.
    assert_eq!(text(&out.stdout), "got: hello\n");
    let err = text(&out.stderr);
    assert!(err.contains("err line"), "{err}");
    assert!(err.contains("[canfit] Watching"), "{err}");
    assert!(err.contains("canfit watch: cli_watch"), "{err}");
}

fn exit_codes_pass_through() {
    let env = Env::new("exit");
    let exe = helper_exe();
    let out = env.run(
        &watch_args(&["--quiet"], &exe, &["alloc", "1", "200", "7"]),
        "",
    );
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    let err = text(&out.stderr);
    assert!(
        !err.contains("[canfit]"),
        "--quiet prints only the summary: {err}"
    );
    assert!(err.contains("exited with code 7"), "{err}");
    assert!(out.stdout.is_empty());

    let out = env.run(&["watch", "--", "/no/such/command-canfit-test"], "");
    assert_eq!(out.status.code(), Some(127), "{out:?}");
    assert!(text(&out.stderr).contains("no such command"));

    // stdout belongs to the command: --json is refused as a usage error.
    let out = env.run(&["watch", "--json", "--", &exe, "helper", "sleep", "1"], "");
    assert_eq!(out.status.code(), Some(64), "{out:?}");
}

/// Peak memory of a three-process tree (100 MiB each) within the documented tolerance: at most
/// 5 % below and 64 MiB above what the helpers hold (the helper binaries' own memory).
fn tree_peak_is_measured_and_remembered() {
    let env = Env::new("tree");
    let exe = helper_exe();
    let json = env.data.join("watch.json");
    let json_arg = json.display().to_string();
    let out = env.run(
        &watch_args(
            &[
                "--interval",
                "0.5",
                "--name",
                "canfit test tree",
                "--json-out",
                &json_arg,
            ],
            &exe,
            &["spawn", "100", "3000", "2"],
        ),
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(v["schema"], "canfit.watch");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["name"], "canfit test tree");
    assert_eq!(v["ended"]["code"], 0);
    assert_eq!(v["remembered"]["saved"], true, "{v}");
    assert!(v["memory"]["max_processes"].as_u64().unwrap() >= 3, "{v}");
    let peak = v["memory"]["peak_bytes"].as_u64().unwrap();
    let held = 300 * MIB;
    println!(
        "  measured peak {:.1} MiB for {} MiB held",
        peak as f64 / MIB as f64,
        held / MIB
    );
    assert!(
        peak >= held * 95 / 100 && peak <= held + 64 * MIB,
        "peak {} MiB, expected about {} MiB",
        peak / MIB,
        held / MIB
    );
    assert_eq!(v["memory"]["provenance"], "measured");

    // A later fit check by that name uses the measured peak.
    let out = env.run(&["canfit test tree"], "");
    let stdout = text(&out.stdout);
    assert!(matches!(out.status.code(), Some(0..=2)), "{out:?}");
    assert!(stdout.contains("measured by watch on"), "{stdout}");
    let out = env.run(&["canfit test tree", "--json"], "");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let app = &v["apps"][0];
    assert_eq!(app["source"], "history", "{v}");
    assert_eq!(app["history"]["method"], "watch", "{v}");
    assert_eq!(app["history"]["need_bytes"].as_u64(), Some(peak), "{v}");

    let out = env.run(&["history", "--apps"], "");
    assert!(text(&out.stdout).contains("measured by watch"), "{out:?}");
}

#[cfg(unix)]
fn signals_map_to_128_plus_the_number() {
    let env = Env::new("signal");
    let exe = helper_exe();
    let out = env.run(&watch_args(&["-q"], &exe, &["selfkill"]), "");
    assert_eq!(out.status.code(), Some(137), "{out:?}");
    assert!(text(&out.stderr).contains("ended by SIGKILL"), "{out:?}");
}

/// Ctrl-C sent to canfit alone (with `kill`) is forwarded; canfit waits and prints the summary.
#[cfg(unix)]
fn interrupt_is_forwarded_and_summarized() {
    let env = Env::new("interrupt");
    let exe = helper_exe();
    let mut child = env
        .command(&watch_args(&[], &exe, &["sleep", "30000"]))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut reader, mut seen) = wait_until_watching(&mut child);
    let started = Instant::now();
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    reader.read_to_string(&mut seen).unwrap();
    let status = child.wait().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "it did not wait 30 s"
    );
    assert_eq!(status.code(), Some(130), "{seen}");
    assert!(seen.contains("ended by SIGINT"), "{seen}");
    assert!(seen.contains("canfit watch: cli_watch"), "{seen}");

    // A command that handles Ctrl-C itself decides how it ends.
    let mut child = env
        .command(&[
            "watch",
            "--",
            "sh",
            "-c",
            "trap 'echo caught; exit 3' INT; while :; do sleep 0.1; done",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut reader, mut seen) = wait_until_watching(&mut child);
    std::thread::sleep(Duration::from_millis(300));
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    reader.read_to_string(&mut seen).unwrap();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(3), "{seen}");
    assert_eq!(stdout, "caught\n");
    assert!(
        seen.contains("stopped after Ctrl-C (exit code 3)"),
        "{seen}"
    );
}

/// `watch --no-save`, `--no-history watch` and `CANFIT_NO_HISTORY=1` measure and exit as usual
/// but leave an empty data directory empty: no database file, no folder.
fn opt_outs_write_nothing() {
    let exe = helper_exe();
    // (tag, watch flags, flags before `watch`, CANFIT_NO_HISTORY, the summary's last line)
    type Case<'a> = (
        &'a str,
        &'a [&'a str],
        &'a [&'a str],
        Option<&'a str>,
        &'a str,
    );
    let cases: [Case; 4] = [
        (
            "nosave",
            &["--no-save"],
            &[],
            None,
            "Not saved (--no-save).",
        ),
        (
            "flag",
            &[],
            &["--no-history"],
            None,
            "Not saved (--no-history).",
        ),
        (
            "flagafter",
            &["--no-history"],
            &[],
            None,
            "Not saved (--no-history).",
        ),
        (
            "env",
            &[],
            &[],
            Some("1"),
            "Not saved (CANFIT_NO_HISTORY=1).",
        ),
    ];
    for (tag, extra, before, env_value, last_line) in cases {
        let env = Env::new(&format!("optout-{tag}"));
        // The data directory itself does not exist yet: canfit must not create it.
        let data = env.data.join("data");
        let json = env.data.join("watch.json");
        let json_arg = json.display().to_string();
        let mut args: Vec<&str> = before.to_vec();
        let mut flags: Vec<&str> = extra.to_vec();
        flags.extend(["--interval", "0.5", "--json-out", &json_arg]);
        args.extend(watch_args(&flags, &exe, &["alloc", "20", "1200", "5"]));
        let mut command = env.command(&args);
        command.env("CANFIT_DATA_DIR", &data);
        if let Some(value) = env_value {
            command.env("CANFIT_NO_HISTORY", value);
        }
        let out = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(5), "{tag}: {out:?}");
        let err = text(&out.stderr);
        assert!(err.contains("exited with code 5"), "{tag}: {err}");
        assert!(err.contains("Peak"), "{tag}: measured as usual: {err}");
        assert_eq!(
            err.trim_end().lines().last(),
            Some(last_line),
            "{tag}: {err}"
        );
        assert!(!data.exists(), "{tag}: canfit created {}", data.display());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(v["history"], "off", "{tag}: {v}");
        assert_eq!(v["remembered"]["saved"], false, "{tag}: {v}");
        assert_eq!(v["ended"]["canfit_exit_code"], 5, "{tag}: {v}");
        assert!(
            v["memory"]["peak_bytes"].as_u64().unwrap() > 0,
            "{tag}: {v}"
        );
    }

    // An existing data directory with a database: nothing is added to it.
    let env = Env::new("optout-existing");
    let saved = env.run(
        &watch_args(&["--quiet"], &exe, &["alloc", "20", "1200"]),
        "",
    );
    assert_eq!(saved.status.code(), Some(0), "{saved:?}");
    let db = env.data.join("history.sqlite3");
    let before = std::fs::read(&db).unwrap();
    let entries = || std::fs::read_dir(&env.data).unwrap().count();
    let count = entries();
    let out = env.run(
        &watch_args(&["--quiet", "--no-save"], &exe, &["alloc", "20", "1200"]),
        "",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(std::fs::read(&db).unwrap(), before, "byte for byte");
    assert_eq!(entries(), count);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("helper") {
        std::process::exit(helper(&args[1..]));
    }
    if args.iter().any(|a| a == "--list") {
        return;
    }
    // Only Unix adds tests below.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut tests: Vec<(&str, fn())> = vec![
        (
            "input_and_output_pass_through",
            input_and_output_pass_through,
        ),
        ("exit_codes_pass_through", exit_codes_pass_through),
        (
            "tree_peak_is_measured_and_remembered",
            tree_peak_is_measured_and_remembered,
        ),
        ("opt_outs_write_nothing", opt_outs_write_nothing),
    ];
    #[cfg(unix)]
    tests.extend([
        (
            "signals_map_to_128_plus_the_number",
            signals_map_to_128_plus_the_number as fn(),
        ),
        (
            "interrupt_is_forwarded_and_summarized",
            interrupt_is_forwarded_and_summarized as fn(),
        ),
    ]);
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, test) in tests {
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        ran += 1;
        let result = std::panic::catch_unwind(test);
        println!(
            "test {name} ... {}",
            if result.is_ok() { "ok" } else { "FAILED" }
        );
        if result.is_err() {
            failed.push(name);
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        ran - failed.len(),
        failed.len()
    );
    if !failed.is_empty() {
        std::process::exit(1);
    }
}
