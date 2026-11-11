# Changelog

All notable changes to canfit are documented in this file. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and canfit follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). From 1.0.0 the command-line commands
and flags, the exit codes, the versioned JSON schemas, the `config.toml` keys and the history
database format are stable: breaking any of them needs a new major version (2.0.0).

## [Unreleased]

## [1.0.0] - 2026-11-11

First public release, on macOS, Windows and Linux: the live monitor, the GGUF fit check, the app
fit check (from an app's settings, from history or from a built-in catalog), several targets
together, `canfit watch -- CMD`, `free` / `--make-room` with confirmed closing, `models`, `history`
and `update`, with `--json` everywhere.

### Added

- Install with one command: a shell installer for macOS and Linux
  (`curl … canfit-installer.sh | sh`) and a PowerShell installer for Windows
  (`irm … canfit-installer.ps1 | iex`), both per user with no admin rights; Homebrew on macOS and
  Linux from the formula in the canfit repository
  (`brew tap federicolaterra/canfit https://github.com/federicolaterra/canfit`, then
  `brew install federicolaterra/canfit/canfit`); Scoop on Windows from the manifest in the
  repository (`scoop install https://raw.githubusercontent.com/federicolaterra/canfit/master/packaging/scoop/canfit.json`);
  an MSI; `.deb` and `.rpm` packages for x86_64 and ARM64; plain archives; and, for Rust
  developers, `cargo install --git https://github.com/federicolaterra/canfit --tag v1.0.0 --locked`.
  The static Linux binary runs on any distribution, e.g. Ubuntu, Debian, Fedora, Arch Linux,
  Alpine. See `docs/open-source/installation.md`.
- Release archives for macOS (Apple Silicon and Intel), Windows x86_64 and Linux x86_64 and ARM64
  (static musl), each with a SHA-256 checksum and a GitHub build provenance attestation.
- `canfit update [--yes] [--json]`: says how this copy of canfit is updated. A copy installed by
  the shell or PowerShell installer updates itself through its companion updater
  (`canfit-update`) after saying it will connect to github.com and asking for confirmation;
  Homebrew, Scoop, MSI, `cargo install` and Linux-package installs get the command to run
  instead. canfit never checks for updates on its own. `--json` prints a `canfit.update`
  document (schema 1) without updating.

- App fit check: `canfit "Docker Desktop"`, `canfit UTM`, `canfit "IntelliJ IDEA"` answer
  whether there is room for an app from the memory its own settings declare — Docker Desktop's VM
  (and the WSL 2 limit in `.wslconfig` on Windows), the RAM of each UTM, Parallels, VMware and
  VirtualBox virtual machine, and the `-Xmx` of JetBrains IDEs and Android Studio plus an estimate
  for memory beyond the heap. Verdicts: fits comfortably (exit `0`), fits but will slow down
  (`1`), at risk (`2`). If the app is already running, only the rest is needed. `canfit "UTM:
  Ubuntu"` checks one virtual machine. See `docs/open-source/apps.md`.
- App profiles in a small TOML format; add or override your own in `apps.toml` next to
  `config.toml` (or `$CANFIT_APPS`).
- App estimates from history: `canfit Slack` uses the memory canfit measured the app use while
  the monitor was open (95th percentile), labelled measured or estimated, with a confidence from
  the evidence ("seen for 3 h over 4 days"). Even a short monitor session gives a low-confidence
  estimate. Settings canfit can't read fall back to history, then the catalog, with a note.
- A seed catalog of about 30 common apps (browsers, chat, music, editors, design, office, AI apps),
  built into canfit, for apps it has never seen: always labelled "estimate from catalog", low
  confidence. Contribution rules in `docs/open-source/apps.md`.
- Unknown app names (no settings, history or catalog value) exit `3` with a sentence suggesting
  close matches.
- Several targets together: `canfit "Docker Desktop" model.gguf`, `canfit Chrome Slack` — any mix
  of models and apps is summed, with each item's source, the total, one verdict and one exit code.
  The Apple Silicon GPU limit applies to the models' part only.
- `--make-room` and `canfit free NAME` work for apps and several targets and never suggest closing
  any of the targets.
- `canfit history --apps` (and `--json`, schema `canfit.learned_apps`) lists what canfit has
  learned about each app.
- `canfit.fit` JSON for apps: `target_kind` (`model`, `app`, `combined`), `verdict_label`,
  `needed.apps_bytes`, and an `apps` entry per app with its source (`config`, `history`,
  `catalog`), confidence, declared and needed memory.
- "Close it yourself": every suggestion shows the safest command to close it by hand for the OS and
  kind of process (`osascript … quit app`, `kill`, `taskkill`, `systemctl [--user] stop`, `brew
  services stop`, `docker desktop stop`, `ollama stop MODEL`, `lms unload --all`), in the cards,
  the closing prompt, the monitor's close dialog and `--json` (`manual_commands`). canfit never
  runs them.
- `canfit watch -- CMD`: runs a command with its own input, output and terminal, measures the real
  memory of it and every process it starts (whole process tree, sampled every second,
  `--interval`), warns on stderr when memory pressure turns strained or critical (closing
  nothing), and prints a summary when it ends: duration, peak and typical memory, peak pressure,
  and, for a llama.cpp command with a GGUF model, canfit's estimate next to the measurement. Ctrl-C
  goes to the command; canfit waits and then summarizes. canfit exits with the command's exit
  code (`128 + signal` after a signal on Unix; `127`/`126` when the command can't start).
  `--quiet`, `--name NAME`, `--json-out FILE` (schema `canfit.watch`, version 1).
- Watched runs are remembered in the history database and used first within history: `canfit
  NAME` answers from the run's peak, labelled "measured by watch on DATE". A model file the
  command maps from disk on macOS or Windows (not in the measured memory there) is added from the
  file. `canfit history --apps` lists watched apps; `method` and `watch` fields in
  `canfit.learned_apps` and `canfit.fit`.
- An issue form for proposing a new app profile or catalog entry, and `good first issue` guidance
  in `CONTRIBUTING.md`.

- History: while the live monitor is open, canfit records pressure events — each stretch of
  strained or critical pressure, with its start, length, worst level, reason and the three apps
  using the most memory at its worst moment — plus a compact snapshot every 30 seconds and the
  memory of the larger apps, in a local SQLite database (`history.sqlite3` in canfit's local data
  directory, or `$CANFIT_DATA_DIR`). Nothing is recorded while the monitor is closed and nothing
  leaves the machine.
- `canfit history [--since 30m|12h|7d|2w|DATE] [--json]`: the timeline of pressure events, opening
  with a one-sentence answer and saying how long canfit was actually watching. JSON schema
  `canfit.history` version 1.
- `h` in the monitor opens a history panel with the last 24 hours of events.
- `canfit models [DIR] [--ctx N] [--cache-type T] [--margin PCT] [--json]`: every GGUF model in a
  folder (split models once, up to four folders deep, vision projectors skipped), smallest first,
  with the memory it needs (estimated) and whether it fits right now. JSON schema `canfit.models`
  version 1.
- `[history]` settings in `config.toml`: `enabled`, `snapshot_interval_secs` (default 30),
  `retention_days` (default 7) and `max_size_mb` (default 64). Older records are deleted, the
  oldest first above the size cap.
- The history database is versioned: newer canfit releases upgrade it in place, and canfit never
  deletes, recreates or writes to a database it does not recognise or that a newer canfit wrote;
  it says so and exits with code 3 (`history`) or keeps monitoring without recording (monitor).
  When a database can't be used, the message names the schema version found and expected and
  points to "Troubleshooting the history database" in `usage.md`.
- `--no-history` (global, every command) and the environment variable `CANFIT_NO_HISTORY=1`:
  this run writes nothing to the history database and creates no database file or folder.
  Existing history is kept and still read (app estimates, `canfit history`, the monitor's `h`
  panel, which shows past events read-only); the monitor's footer says "history off". They can
  only turn recording off: they never turn on what `[history] enabled = false` turned off.
- `canfit watch --no-save`: measures the command as usual (same output, warnings, summary,
  `--json-out` file and exit code) but does not store the run; the summary ends with "Not saved
  (--no-save)." Same as `canfit --no-history watch`.
- `--json` documents with history metadata (`canfit.history`, `canfit.learned_apps`,
  `canfit.watch`) have a `history` field: `"off"` when recording was disabled for the run (by
  `--no-history`, `CANFIT_NO_HISTORY`, `watch --no-save` or `[history] enabled = false`),
  otherwise `"on"`.
- Linux support (x86_64 and ARM64, any distribution): the live monitor, `canfit --json`, the fit
  check, `canfit free` and `--make-room` work natively with no root. Memory comes from
  `/proc/meminfo` (`MemAvailable`; cache is never "used"), swap growth from `/proc/vmstat`, and
  compressed memory from zswap and zram; app memory is each process's proportional set size
  (PSS), or its resident size, labelled, for other users' processes. Kernel threads are not listed.
- Linux pressure-stall information (PSI, Linux 4.20+): a new pressure signal, `memory_stall`
  ("programs spent 35% of the last 10 seconds waiting for memory"), a *Waiting* line in the
  snapshot, and a `stall` object in the JSON snapshot with memory, CPU and I/O readings. Without
  `/proc/pressure` canfit reads the cgroup v2 root's pressure files, and without either it uses
  the other signals.
- Linux app grouping: a systemd service, a Flatpak or a Snap is one row; otherwise a program and
  the helpers it started from its own install folder (Chrome and its renderers are one row). A
  program started from a shell stays its own row.
- Linux process rules: PID 1, D-Bus, `systemd-*`, PipeWire, the display server and compositor
  (Xorg, Xwayland, GNOME Shell, KWin, Sway, Hyprland…), session and login managers, shells,
  root and service accounts, and everything under `/usr/libexec` and `/usr/lib/systemd` (except
  terminal apps) are never suggested. Closing sends `SIGTERM`, then `SIGKILL` only after the
  second confirmation.
- Static musl builds for Linux x86_64 and ARM64 (no shared libraries), built in CI and kept as
  the `canfit-linux-x86_64` and `canfit-linux-aarch64` artifacts.
- Windows 10/11 support: the live monitor, `canfit --json`, the fit check, `canfit free` and
  `--make-room` work natively on Windows x86_64 with no admin rights and no Visual C++
  Redistributable. Memory and pressure come from available memory, commit charge against the
  commit limit, page-file use, hard page faults and Windows' low-memory notification; app memory
  is each process's private working set, as in Task Manager's Details tab.
- Windows process rules: the kernel and session processes, `explorer.exe`, services, shells and
  console hosts, WSL's VM memory and everything else in the Windows folder are never suggested;
  browsers and chat apps are safe; editors, model servers, Docker and unknown apps are caution.
- Closing on Windows asks apps to quit by closing their windows, as Task Manager's *End task*
  does; programs without a window can only be forced, after the second confirmation.
- `CANFIT_ASCII=1` draws ASCII stand-ins for symbols, bars and borders; canfit does this on its
  own in the classic Windows console, whose default fonts lack some symbols (`CANFIT_ASCII=0`
  turns it off).
- JSON snapshot: `memory.commit_bytes`, `memory.commit_limit_bytes` and `hard_faults_per_sec`
  (Windows), and the pressure signals `commit_limit` and `hard_faults`. `schema_version` stays 1.

- `canfit` on a terminal opens a live monitor (macOS): a summary sentence, cards for memory, swap,
  CPU and pressure with bars and graphs of the last few minutes, the apps using the most memory
  with their safe · caution · never class, and a "Why?" panel. The layout adapts from 80×24 to
  full screen.
- Monitor keys: arrows or `j`/`k` to move, `x` to close the selected app through the same
  two-step confirmation as `canfit free`, `?` to explain, `t` to switch theme, `q` or `Ctrl-C` to
  quit; click to select and scroll with the mouse wheel.
- `--interval SECONDS` (0.5–60, default 1) sets the monitor's refresh; the app list is rescanned
  at most every 2 seconds. The monitor uses under 1 % of a CPU core on an M1 Pro.
- `--theme dark|light|high-contrast`; `NO_COLOR` (or `TERM=dumb`) removes colours, keeping
  symbols.
- The fit check, `--make-room` and `canfit free` print their cards in colour when stdout is a
  terminal; piped output is unchanged plain text.

### Changed

- `canfit --help` no longer calls canfit "in early development" and links the documentation.
- The first public release is 1.0.0 (not 0.1.0): its command line, exit codes, JSON schemas,
  configuration keys and history database format are stable under Semantic Versioning.
- Global flags typed before a subcommand now work (`canfit --json history`,
  `canfit --no-history watch -- CMD`); before, the subcommand's name was taken as a target.
- `canfit free`: at the "Type the numbers of the apps to close" prompt, `n`, `no` and `q` (any
  case) close nothing, like Enter, with a plain "Nothing was closed."
- `canfit history --apps` explains both ways an app counts as measured: a run measured by
  `canfit watch` (always measured; confidence high or medium) and the monitor's samples (measured
  with at least 20 samples over 10 minutes, last seen within 30 days).
- The `.deb`, `.rpm` and Scoop package descriptions mention apps as well as models.
- The monitor's footer drops its least needed key hints on narrow terminals instead of running
  into the refresh label.
- Building canfit now compiles SQLite's C source: cross builds need a C compiler for the target
  (see `docs/open-source/development.md`).
- The app list names how memory was measured on this OS ("private working set" on Windows)
  instead of always "physical footprint".
- A failed close now reads "Could not close …" instead of "Could not signal …".
- `canfit` without arguments prints the one-shot snapshot only when stdin or stdout is not a
  terminal (or with `--json`); on a terminal it opens the monitor.
- Repeated process scans reuse each process's executable path, making scans cheaper.

- `canfit free` lists what you could close on macOS: apps classified safe · caution · never,
  safe apps first and then by measured memory, each with a short reason ("you may lose unsaved
  work" for caution), plus a "Why?" paragraph on what is using memory. macOS itself, root and
  other users' processes, shells, the terminal or app canfit runs in, canfit itself and protected
  apps are never suggested. `canfit free --json` prints a `canfit.free` document.
- `canfit model.gguf --make-room` and `canfit free model.gguf` (one shared implementation) pick the
  smallest set of apps to close so the model fits — safe apps first, caution apps only when needed
  — and show the verdict expected afterwards (estimated). `--json` adds a `make_room` object to
  the `canfit.fit` document.
- Confirmed closing on macOS: in a terminal, canfit lists every process it would close, asks, sends
  a graceful quit (SIGTERM), waits up to 10 seconds, and asks a second time before forcing
  (SIGKILL). `--json` and non-terminal runs never prompt or close anything.
- Protected list: `[processes] protected = [...]` in `config.toml` in canfit's config directory
  (or the file named by `CANFIT_CONFIG`). An unreadable config exits `3` instead of being ignored.

- `canfit model.gguf` checks whether a GGUF model fits in the memory available now (macOS). It
  reads the GGUF v2/v3 header without loading weights, sums exact tensor sizes, and estimates the
  KV cache (grouped-query, per-layer and latent attention), compute and runtime overhead, and a
  safety margin. On Apple Silicon it also checks the GPU memory limit and says which limit failed.
  Every number is labelled read from GGUF, estimated or measured.
- Fit-check options: `--ctx` (default 8,192 tokens or the model's own context), `--cache-type
  f16|q8_0|q4_0` and `--margin PERCENT` (default 10).
- Split models (`name-00001-of-0000N.gguf`), folders holding one model and extensionless GGUF
  files (such as Ollama blobs) are accepted; several model targets are summed.
- Fit-check exit codes: `0` fits, `1` tight, `2` doesn't fit; missing, truncated or malformed
  files exit `3` with a one-sentence explanation.
- `canfit model.gguf --json` prints a documented `canfit.fit` document (`schema_version` 1).
- `canfit` prints a one-shot snapshot on macOS (Apple Silicon and Intel): a summary sentence,
  memory, swap and whether it is growing, CPU, the pressure level with its main reason, the GPU
  memory limit on Apple Silicon, and the applications using the most memory. Helpers, renderers
  and XPC services fold into their app. Every value is labelled measured, read from config or
  estimated. No admin rights needed.
- `canfit --json` prints the snapshot as a documented JSON document (`schema_version` 1).
- Unified pressure index (calm · strained · critical, 0–100) with documented macOS signals and
  thresholds.
- Runtime errors (an unreadable file, an unknown app, an unreadable config) exit with code `3`
  and a one-sentence explanation on stderr.

- Command-line shape: `canfit` (monitor), `canfit <target>...` (fit check) with `--ctx`,
  `--make-room` and `--app`, the `free`, `watch`, `history`, `models` and `update` subcommands,
  and a global `--json` flag.
- Usage errors exit with code `64`, keeping `0`–`2` for fit-check answers.
- Project documentation, contribution guidelines, and CI on macOS, Windows and Linux.
- MIT licence.

[Unreleased]: https://github.com/federicolaterra/canfit/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/federicolaterra/canfit/releases/tag/v1.0.0
