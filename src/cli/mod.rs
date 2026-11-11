//! Command-line interface: argument parsing, target detection, `--json` and exit codes.
//!
//! The shape follows ADR-003: no `fit` subcommand. `canfit` alone opens the live monitor,
//! `canfit <target>...` runs the fit check, and a few reserved words are subcommands.

mod advice;
mod app;
pub(crate) mod card;
mod combined;
mod fit;
pub(crate) mod history;
mod models;
pub(crate) mod report;
mod update;
mod watch;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::estimate::model::{CacheType, DEFAULT_MARGIN_PERCENT, EstimateOptions};
use crate::ui::{self, theme::ThemeName};

/// Exit code for invalid command-line usage (`EX_USAGE` from sysexits.h).
///
/// clap exits with 2 on usage errors by default, which would collide with the fit-check
/// meaning of 2 ("doesn't fit"). See ADR-003.
pub const EXIT_USAGE: u8 = 64;

/// Exit code for runtime errors, such as an OS canfit cannot read yet (ADR-009).
///
/// Codes 0–2 are fit answers and 64 is a usage error; anything canfit could not complete is 3.
pub const EXIT_RUNTIME: u8 = 3;

/// The top for people running AI locally: can this model or app fit right now, and what
/// should I close?
#[derive(Debug, Parser)]
#[command(
    name = "canfit",
    version,
    about,
    args_conflicts_with_subcommands = true,
    after_help = "Exit codes: 0 fits, 1 fits but tight (for apps: will slow down), 2 doesn't fit (for apps: at risk), 3 canfit could not complete the request (unreadable file, unknown app, unreadable app settings, unsupported OS), 64 usage error.\n\n`canfit` opens the live monitor (a one-shot snapshot when piped) and records pressure events while it runs, `canfit model.gguf` checks a model, `canfit \"Docker Desktop\"` checks an app from the memory its settings declare (`canfit \"UTM: Ubuntu\"` for one virtual machine), `canfit Slack` estimates an app from what the monitor measured it use (or canfit's catalog), `canfit \"Docker Desktop\" model.gguf` checks several things together, `canfit models DIR` lists which models fit, `canfit free` says what to close, `canfit history` shows past pressure events (`--apps`: what canfit learned about apps), `canfit watch -- CMD` runs a command and measures its real memory (it exits with the command's own exit code; 126/127 when the command can't start) and `canfit update` says how to update.\n\n`--no-history` (or CANFIT_NO_HISTORY=1) records nothing in this run; past history is kept and still read.\n\nDocumentation: https://github.com/federicolaterra/canfit"
)]
pub struct Cli {
    #[command(flatten)]
    pub check: CheckArgs,

    /// Print machine-readable JSON instead of the human-readable view.
    #[arg(long, global = true)]
    pub json: bool,

    /// Refresh interval of the live monitor, in seconds (0.5 to 60).
    #[arg(long, value_name = "SECONDS", default_value = "1", value_parser = parse_interval,
          conflicts_with_all = ["targets", "apps"])]
    pub interval: Duration,

    /// Write nothing to the history database in this run: no snapshots, pressure events, app
    /// samples or watch runs, and no database file or folder is created. Past history is kept
    /// and still read where a command needs it (app estimates, `canfit history`). Same as
    /// CANFIT_NO_HISTORY=1.
    #[arg(long, global = true)]
    pub no_history: bool,

    /// Colour theme of the monitor and the cards. `NO_COLOR` turns colours off in every theme.
    #[arg(long, global = true, value_enum, value_name = "THEME", default_value_t = ThemeName::Dark)]
    pub theme: ThemeName,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Arguments of the default action: the fit check.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Things to check: a .gguf file, a folder of models, or an app name ("Docker Desktop",
    /// "UTM: Ubuntu" for one virtual machine, "Slack"). Several targets are summed. With no
    /// target, canfit opens the live monitor.
    #[arg(value_name = "TARGET")]
    pub targets: Vec<String>,

    /// Treat NAME as an app, even when it matches a subcommand (e.g. `--app history`).
    #[arg(long = "app", value_name = "NAME")]
    pub apps: Vec<String>,

    #[command(flatten)]
    pub estimate: EstimateArgs,

    /// If the targets don't fit, suggest what to close to make room (and, on a terminal, offer
    /// to close it after confirmation). Same as `canfit free TARGET`.
    #[arg(long)]
    pub make_room: bool,
}

impl CheckArgs {
    fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.apps.is_empty()
    }
}

/// Options of the model estimate, shared by the fit check and `free`.
#[derive(Debug, Args)]
pub struct EstimateArgs {
    /// Context length, in tokens, used to estimate the model's KV cache. Default: 8192, or the
    /// model's own context length if smaller.
    #[arg(long, value_name = "TOKENS", value_parser = clap::value_parser!(u32).range(1..))]
    pub ctx: Option<u32>,

    /// KV cache type, as llama.cpp's --cache-type-k/-v.
    #[arg(long, value_enum, value_name = "TYPE", default_value_t = CacheType::F16)]
    pub cache_type: CacheType,

    /// Safety margin added to the estimate, in percent.
    #[arg(long, value_name = "PERCENT", default_value_t = DEFAULT_MARGIN_PERCENT,
          value_parser = clap::value_parser!(u32).range(0..=100))]
    pub margin: u32,
}

impl EstimateArgs {
    fn options(&self) -> EstimateOptions {
        EstimateOptions {
            context: self.ctx.map(u64::from),
            cache_type: self.cache_type,
            margin_percent: self.margin,
            ..EstimateOptions::default()
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// What to close: without a target, the apps safest to close, largest first; with targets,
    /// what to close to make room for them (same as `canfit TARGET --make-room`). On a terminal,
    /// canfit offers to close them after confirmation.
    Free {
        #[arg(value_name = "TARGET")]
        targets: Vec<String>,

        /// Treat NAME as an app target.
        #[arg(long = "app", value_name = "NAME")]
        apps: Vec<String>,

        #[command(flatten)]
        estimate: EstimateArgs,
    },
    /// Launch a command and measure the real memory of it and everything it starts. The command
    /// keeps its own input and output; canfit writes to stderr only, prints a summary when it
    /// ends, remembers the peak for `canfit NAME` and exits with the command's exit code.
    Watch {
        /// The name the run is remembered under (default: the program's name), so `canfit NAME`
        /// uses the measured peak.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Seconds between two samples (0.5 to 60).
        #[arg(long, value_name = "SECONDS", default_value = "1", value_parser = parse_interval)]
        interval: Duration,

        /// Print only the summary at the end: no start line, status lines or pressure warnings.
        #[arg(long, short = 'q')]
        quiet: bool,

        /// Also write the summary as JSON (`canfit.watch`) to FILE; stdout belongs to the
        /// command.
        #[arg(long, value_name = "FILE")]
        json_out: Option<PathBuf>,

        /// Measure the command as usual but don't store the run, so later fit checks don't
        /// learn from it. Same as `canfit --no-history watch`.
        #[arg(long)]
        no_save: bool,

        /// The command to launch, after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Show the timeline of past pressure events, recorded while the monitor runs.
    History {
        /// How far back to look: a duration (30m, 12h, 7d, 2w) or a local date or time
        /// (2026-11-09, "2026-11-09 14:30"). Default: 24h.
        #[arg(long, value_name = "WHEN", value_parser = history::parse_since_now,
              conflicts_with = "apps")]
        since: Option<history::Since>,

        /// Show what canfit has learned about apps instead: the memory each app was measured
        /// using while the monitor was open, used by `canfit APP`.
        #[arg(long)]
        apps: bool,
    },
    /// List the GGUF models in a folder (default: the current folder) and which of them fit
    /// right now.
    Models {
        #[arg(value_name = "DIR")]
        dir: Option<PathBuf>,

        #[command(flatten)]
        estimate: EstimateArgs,
    },
    /// Update canfit to the latest release, after asking. Copies installed by a package manager
    /// (Homebrew, Scoop, MSI, cargo, .deb/.rpm) are updated by it; canfit says how.
    Update {
        /// Update without asking for confirmation (the installer's copy only).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

/// Parses the process arguments and runs the requested command.
pub fn run() -> ExitCode {
    let cli = match Cli::try_parse_from(hoist_leading_globals(std::env::args_os())) {
        Ok(cli) => cli,
        Err(err) => return usage_error(&err),
    };
    turn_history_off_if_asked(&cli);
    match &cli.command {
        None if cli.check.is_empty() => {
            if !cli.json && ui::monitor_possible() {
                ui::run_monitor(cli.interval, cli.theme)
            } else {
                snapshot(cli.json)
            }
        }
        None => fit::run(
            &cli.check.targets,
            &cli.check.apps,
            cli.check.make_room,
            cli.check.estimate.options(),
            cli.json,
            cli.theme,
        ),
        Some(Command::Free {
            targets,
            apps,
            estimate,
        }) => {
            if targets.is_empty() && apps.is_empty() {
                advice::run_free(cli.json, cli.theme)
            } else {
                fit::run(targets, apps, true, estimate.options(), cli.json, cli.theme)
            }
        }
        Some(Command::History { apps: true, .. }) => history::run_apps(cli.json, cli.theme),
        Some(Command::History { since, .. }) => history::run(since.as_ref(), cli.json, cli.theme),
        Some(Command::Models { dir, estimate }) => {
            models::run(dir.as_deref(), estimate.options(), cli.json, cli.theme)
        }
        Some(Command::Update { yes }) => update::run(*yes, cli.json),
        Some(Command::Watch {
            name,
            interval,
            quiet,
            json_out,
            no_save: _,
            command,
        }) => {
            if cli.json {
                eprintln!(
                    "canfit watch writes its JSON summary with --json-out FILE: stdout belongs to \
                     the command it runs."
                );
                return ExitCode::from(EXIT_USAGE);
            }
            watch::run(watch::Options {
                command: command.clone(),
                name: name.clone(),
                interval: *interval,
                quiet: *quiet,
                json_out: json_out.clone(),
                theme: cli.theme,
            })
        }
    }
}

/// Global switches that may come before a subcommand (`canfit --no-history watch -- CMD`).
const LEADING_GLOBALS: &[&str] = &["--no-history", "--json"];

/// Moves global switches typed before a subcommand to just after its name. The fit check's
/// targets conflict with subcommands (`args_conflicts_with_subcommands`, so `canfit m.gguf free`
/// checks two targets), which would otherwise turn `canfit --no-history watch` into a check of an
/// app called "watch". Anything else is passed through untouched.
fn hoist_leading_globals<I>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    use clap::CommandFactory;
    let args: Vec<std::ffi::OsString> = args.into_iter().collect();
    let leading = args
        .iter()
        .skip(1)
        .take_while(|a| LEADING_GLOBALS.iter().any(|g| a.as_os_str() == *g))
        .count();
    let Some(next) = args.get(1 + leading).and_then(|a| a.to_str()) else {
        return args;
    };
    if leading == 0
        || !Cli::command()
            .get_subcommands()
            .any(|c| c.get_name() == next)
    {
        return args;
    }
    let mut out = Vec::with_capacity(args.len());
    out.push(args[0].clone());
    out.push(args[1 + leading].clone());
    out.extend(args[1..=leading].iter().cloned());
    out.extend(args[2 + leading..].iter().cloned());
    out
}

/// `--no-history`, `CANFIT_NO_HISTORY` and `watch --no-save` turn history recording off for the
/// whole run (one code path: the history store refuses writes). They can only turn it off: none
/// of them turns on what `config.toml` turned off.
fn turn_history_off_if_asked(cli: &Cli) {
    use crate::history::{self as store, RecordingOff};
    let no_save = matches!(cli.command, Some(Command::Watch { no_save: true, .. }));
    let env =
        std::env::var_os(store::NO_HISTORY_ENV).is_some_and(|v| store::env_turns_recording_off(&v));
    let reason = if cli.no_history {
        Some(RecordingOff::Flag)
    } else if no_save {
        Some(RecordingOff::NoSave)
    } else if env {
        Some(RecordingOff::Env)
    } else {
        None
    };
    if let Some(reason) = reason {
        store::turn_recording_off(reason);
    }
}

/// Parses `--interval`: decimal seconds between 0.5 and 60.
fn parse_interval(text: &str) -> Result<Duration, String> {
    let secs: f64 = text
        .trim()
        .parse()
        .map_err(|_| format!("\"{text}\" is not a number of seconds"))?;
    if !(0.5..=60.0).contains(&secs) {
        return Err("the interval must be between 0.5 and 60 seconds".to_string());
    }
    Ok(Duration::from_secs_f64(secs))
}

/// `canfit --json`, or `canfit` when stdin or stdout is not a terminal: one explained snapshot.
fn snapshot(json: bool) -> ExitCode {
    match report::take_snapshot(true) {
        Ok(snapshot) => {
            if json {
                println!("{}", report::to_json(&snapshot));
            } else {
                print!(
                    "{}",
                    crate::ui::glyphs::text(&report::render_text(&snapshot))
                );
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn usage_error(err: &clap::Error) -> ExitCode {
    // `--help` and `--version` are reported as errors by clap but are successful requests.
    let _ = err.print();
    if err.use_stderr() {
        ExitCode::from(EXIT_USAGE)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(hoist_leading_globals(
            std::iter::once("canfit")
                .chain(args.iter().copied())
                .map(std::ffi::OsString::from),
        ))
        .expect("arguments should parse")
    }

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_arguments_opens_the_monitor() {
        let cli = parse(&[]);
        assert!(cli.command.is_none());
        assert!(cli.check.is_empty());
    }

    #[test]
    fn targets_run_the_fit_check() {
        let cli = parse(&[
            "model.gguf",
            "Docker Desktop",
            "--ctx",
            "32768",
            "--make-room",
        ]);
        assert!(cli.command.is_none());
        assert_eq!(cli.check.targets, ["model.gguf", "Docker Desktop"]);
        assert_eq!(cli.check.estimate.ctx, Some(32768));
        assert_eq!(cli.check.estimate.cache_type, CacheType::F16);
        assert_eq!(cli.check.estimate.margin, DEFAULT_MARGIN_PERCENT);
        assert!(cli.check.make_room);
    }

    #[test]
    fn estimate_flags_are_parsed_and_bounded() {
        let cli = parse(&["m.gguf", "--cache-type", "q8_0", "--margin", "0"]);
        assert_eq!(cli.check.estimate.cache_type, CacheType::Q8_0);
        let options = cli.check.estimate.options();
        assert_eq!(options.margin_percent, 0);
        assert_eq!(options.context, None);
        for bad in [
            &["m.gguf", "--ctx", "0"][..],
            &["m.gguf", "--margin", "101"],
            &["m.gguf", "--cache-type", "q5_1"],
        ] {
            let err = Cli::try_parse_from(std::iter::once("canfit").chain(bad.iter().copied()))
                .unwrap_err();
            assert!(err.use_stderr(), "{bad:?}");
        }
    }

    #[test]
    fn reserved_words_are_subcommands() {
        assert!(matches!(
            parse(&["history"]).command,
            Some(Command::History {
                since: None,
                apps: false
            })
        ));
        assert!(matches!(
            parse(&["history", "--apps", "--json"]).command,
            Some(Command::History { apps: true, .. })
        ));
        assert!(Cli::try_parse_from(["canfit", "history", "--apps", "--since", "2d"]).is_err());
        assert!(matches!(
            parse(&["update"]).command,
            Some(Command::Update { yes: false })
        ));
        assert!(matches!(
            parse(&["update", "--yes"]).command,
            Some(Command::Update { yes: true })
        ));
        assert!(matches!(
            parse(&["models", "/tmp/models"]).command,
            Some(Command::Models { dir: Some(_), .. })
        ));
    }

    #[test]
    fn history_and_models_take_their_options() {
        match parse(&["history", "--since", "7d", "--json"]).command {
            Some(Command::History { since: Some(s), .. }) => {
                assert_eq!(s.label, "in the last 7 days");
            }
            other => panic!("expected history, got {other:?}"),
        }
        let err = Cli::try_parse_from(["canfit", "history", "--since", "soon"]).unwrap_err();
        assert!(err.use_stderr());
        match parse(&["models", "--ctx", "32768", "--cache-type", "q8_0"]).command {
            Some(Command::Models { dir, estimate }) => {
                assert!(dir.is_none());
                assert_eq!(estimate.options().context, Some(32768));
                assert_eq!(estimate.cache_type, CacheType::Q8_0);
            }
            other => panic!("expected models, got {other:?}"),
        }
    }

    #[test]
    fn free_takes_optional_targets_and_estimate_flags() {
        match parse(&["free"]).command {
            Some(Command::Free { targets, apps, .. }) => {
                assert!(targets.is_empty() && apps.is_empty());
            }
            other => panic!("expected free, got {other:?}"),
        }
        match parse(&["free", "m.gguf", "--ctx", "4096", "--json"]).command {
            Some(Command::Free {
                targets, estimate, ..
            }) => {
                assert_eq!(targets, ["m.gguf"]);
                assert_eq!(estimate.options().context, Some(4096));
            }
            other => panic!("expected free, got {other:?}"),
        }
    }

    #[test]
    fn app_flag_escapes_reserved_words() {
        let cli = parse(&["--app", "history"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.check.apps, ["history"]);
    }

    #[test]
    fn watch_takes_the_command_after_double_dash() {
        let cli = parse(&["watch", "--", "llama-server", "-m", "m.gguf"]);
        match cli.command {
            Some(Command::Watch {
                command,
                name,
                interval,
                quiet,
                json_out,
                ..
            }) => {
                assert_eq!(command, ["llama-server", "-m", "m.gguf"]);
                assert_eq!(
                    (name, interval, quiet, json_out),
                    (None, Duration::from_secs(1), false, None)
                );
            }
            other => panic!("expected watch, got {other:?}"),
        }
        match parse(&[
            "watch",
            "--name",
            "my llm",
            "--interval",
            "0.5",
            "-q",
            "--json-out",
            "w.json",
            "--",
            "ollama",
            "serve",
        ])
        .command
        {
            Some(Command::Watch {
                command,
                name,
                interval,
                quiet,
                json_out,
                ..
            }) => {
                assert_eq!(command, ["ollama", "serve"]);
                assert_eq!(name.as_deref(), Some("my llm"));
                assert_eq!(interval, Duration::from_millis(500));
                assert!(quiet);
                assert_eq!(json_out, Some(PathBuf::from("w.json")));
            }
            other => panic!("expected watch, got {other:?}"),
        }
        assert!(Cli::try_parse_from(["canfit", "watch"]).is_err());
        assert!(Cli::try_parse_from(["canfit", "watch", "--interval", "0.1", "--", "x"]).is_err());
    }

    #[test]
    fn history_opt_out_flags_parse() {
        assert!(!parse(&[]).no_history);
        assert!(parse(&["--no-history"]).no_history);
        // Global: accepted before or after a subcommand and with targets.
        assert!(parse(&["--no-history", "history"]).no_history);
        assert!(parse(&["history", "--apps", "--no-history"]).no_history);
        assert!(parse(&["m.gguf", "--no-history"]).no_history);
        let cli = parse(&["--no-history", "watch", "--", "x"]);
        assert!(cli.no_history);
        assert!(matches!(
            cli.command,
            Some(Command::Watch { no_save: false, .. })
        ));
        assert!(matches!(
            parse(&["watch", "--no-save", "--", "x", "--no-save"]).command,
            Some(Command::Watch { no_save: true, ref command, .. })
                if command == &["x", "--no-save"]
        ));
        assert!(parse(&["--json", "--no-history", "history"]).json);
        // A target first keeps the old meaning: subcommand names after it are targets.
        let cli = parse(&["--no-history", "m.gguf", "free"]);
        assert!(cli.command.is_none() && cli.no_history);
        assert_eq!(cli.check.targets, ["m.gguf", "free"]);
        // --no-save belongs to watch only.
        assert!(Cli::try_parse_from(["canfit", "--no-save"]).is_err());
        assert!(Cli::try_parse_from(["canfit", "history", "--no-save"]).is_err());
    }

    #[test]
    fn json_is_accepted_everywhere() {
        assert!(parse(&["--json"]).json);
        assert!(parse(&["model.gguf", "--json"]).json);
        assert!(parse(&["history", "--json"]).json);
    }

    #[test]
    fn monitor_interval_and_theme() {
        let cli = parse(&[]);
        assert_eq!(cli.interval, Duration::from_secs(1));
        assert_eq!(cli.theme, ThemeName::Dark);
        let cli = parse(&["--interval", "2.5", "--theme", "high-contrast"]);
        assert_eq!(cli.interval, Duration::from_millis(2500));
        assert_eq!(cli.theme, ThemeName::HighContrast);
        // The theme also styles the fit check and `free`.
        assert_eq!(
            parse(&["m.gguf", "--theme", "light"]).theme,
            ThemeName::Light
        );
        assert_eq!(parse(&["free", "--theme", "light"]).theme, ThemeName::Light);
        for bad in [
            &["--interval", "0.1"][..],
            &["--interval", "61"],
            &["--interval", "soon"],
            &["m.gguf", "--interval", "2"],
            &["--theme", "pink"],
        ] {
            let err = Cli::try_parse_from(std::iter::once("canfit").chain(bad.iter().copied()))
                .unwrap_err();
            assert!(err.use_stderr(), "{bad:?}");
        }
    }

    #[test]
    fn usage_errors_do_not_use_fit_exit_codes() {
        let err = Cli::try_parse_from(["canfit", "--no-such-flag"]).unwrap_err();
        assert!(err.use_stderr());
        assert!(![0, 1, 2].contains(&EXIT_USAGE));
        assert!(![0, 1, 2, EXIT_USAGE].contains(&EXIT_RUNTIME));
    }
}
