# Architecture

canfit is a single Rust binary with no runtime dependencies. It is one crate organized in
modules:

```text
src/
├── cli/        # commands, target detection, --json, exit codes
├── core/       # shared data model: snapshots, processes, app groups, pressure levels
├── platform/   # per-OS backends: macOS, Windows, Linux
├── estimate/   # GGUF header reader, memory estimate, app profiles, config readers, history estimates and catalog, target detection, fit verdict
├── advisor/    # process classification, "what to close", explanations
├── history/    # local SQLite snapshots, pressure events and watched runs
├── ui/         # terminal UI: cards, themes, adaptive layout
└── watch/      # canfit watch: process-tree tracking, run statistics, launching the command
```

> [!NOTE]
> `cli`, `core`, `estimate` (GGUF reader, memory estimate, fit verdict, app profiles and config
> readers in `estimate::apps`), `advisor` (process
> classification, "what to close", make room, explanations, confirmed closing), `ui` (live monitor,
> themes, coloured cards, ASCII fallback, history panel), `history` (SQLite store, pressure-event
> detection, retention, watched runs), `watch` and the macOS, Windows and Linux backends in
> `platform` contain code today.

App checks (`estimate::apps`) are data-driven: `profiles.toml` describes each app (names,
running-app matchers, an ordered list of config readers), the readers are pure parsers rooted in an
`Env` (the user's folders for one OS), and `check` turns a reading and a snapshot into the memory
the app still needs. Apps without readable settings fall back to `learned` (the 95th percentile of
the samples the monitor recorded, with confidence rules) and then to the embedded seed
`catalog.toml`. Several targets are summed in `cli::fit` / `cli::combined`, with the GPU limit
applied to the models' part only (`estimate::fit::evaluate_parts`). `advisor::manual` builds the
"close it yourself" commands. See [Apps](apps.md).

The terminal UI keeps state and drawing pure: `ui::app` holds the monitor state and keymap,
`ui::view` lays out and draws the cards, and both are tested with ratatui's `TestBackend` and
`insta` snapshots. Only `ui::mod` touches the terminal (raw mode, alternate screen, mouse, restore
on exit and panic). Closing from the monitor goes through `advisor::close` with a dialog that
implements the same `Confirm` trait as the command-line prompt.

`canfit watch` (`cli::watch`) launches the command with inherited stdio (`watch::launch`) and
samples the system through the same `platform` backend and `core::sampler` as the monitor. The
watched process tree (`watch::Tree`) is the child plus every process reachable by parent links,
plus processes already counted that are still alive under the same name (reparented
grandchildren), plus — on Linux, where canfit is a child subreaper — orphans canfit adopted, and —
on Windows — the members of a job object the child is put in right after it starts. `watch::Stats`
keeps the samples (peak, median, 95th percentile, peak pressure) and `watch::Alerts` decides when
to warn. On Unix canfit catches SIGINT/SIGQUIT/SIGTERM/SIGHUP while the child runs, forwards
SIGTERM/SIGHUP, and forwards SIGINT/SIGQUIT only when it is not the terminal's foreground job (in
the foreground, the terminal already signalled the child). The run is stored in the history
table `watch_runs`, which `estimate::apps::learned` reads first.

## Stack

| Area | Choice |
|---|---|
| Language | Rust (stable) |
| Command line | `clap` |
| Terminal UI | `ratatui` + `crossterm`; `insta` for render snapshot tests |
| System data | `sysinfo` (CPU) + OS-specific code (`libc`, `mach2` on macOS; `windows` on Windows; `/proc` and `/sys` with `std::fs`, plus `libc` for signals, on Linux) |
| JSON | `serde`, `serde_json` |
| History | SQLite via `rusqlite` with SQLite compiled in (`bundled`); local times via `chrono` |
| Paths and config | `directories`, TOML via `toml` + `serde` |
| NVIDIA GPU | NVML, loaded at runtime if present (later) |

## Per-OS backends

Each operating system has its own backend that turns native signals into one shared model. Only
the backend for the current OS is compiled in; the pure conversion code next to each backend
(`platform::darwin`, `platform::windows_model`, `platform::linux_model`) builds and is tested on
every OS. The Linux reader takes a root folder, so the tests run it against fixture trees that
mirror `/proc`, `/sys` and `/etc` (`tests/fixtures/linux/`).

| OS | Memory | Pressure | Per-process memory |
|---|---|---|---|
| macOS | `host_statistics64`, `vm.swapusage` | kernel memory-pressure level | physical footprint (as in Activity Monitor) |
| Windows | `GlobalMemoryStatusEx`, `GetPerformanceInfo` (commit charge), page files | commit vs limit, PDH hard page faults, low-memory notification | private working set (as in Task Manager's Details tab) |
| Linux | `/proc/meminfo`, `/proc/vmstat`, zram `mm_stat` | PSI (`/proc/pressure/*`, else the cgroup v2 root's `*.pressure`) | PSS from `smaps_rollup` (resident size for other users' processes) |

Backends are built in the order macOS → Windows → Linux. Each backend implements one small
`SystemSource` trait that only *reads*: memory, swap, CPU, the OS's own pressure level, the GPU
memory limit and processes. A short ring buffer of samples (the sampler) provides trends such as
swap growth. The shared layer turns their raw signals into
the [pressure index](pressure-index.md), groups processes into applications, and feeds the
[estimation](estimation.md) and [process safety](process-safety.md) logic.

### Processes and applications

Per-process memory is measured the way each OS does it best and labelled with its kind: physical
footprint on macOS (falling back to resident size, labelled as such, if footprint is refused);
private working set on Windows; PSS on Linux (resident size, labelled, for other users'
processes, whose PSS the kernel keeps private). Processes whose memory can't be read without admin
rights (other users' processes on macOS and Windows) are counted, never guessed.

Processes fold into applications: on macOS, everything inside one `.app` bundle — helpers,
renderers, XPC services — is one row. On Windows, a program and the helpers it started from its
own install folder are one row, named after the program. On Linux, a systemd service, a Flatpak
or a Snap is one row; otherwise a program and its helpers running the same executable or one from
its install folder. Processes without such a hint are grouped by executable name, and a model
server started from a terminal stays its own row. Shared WebKit processes, whose app public APIs
don't reveal, form one "Safari and other web views (WebKit)" group.

### Apple Silicon GPU memory

On Apple Silicon the GPU can use only part of unified memory. canfit reads `iogpu.wired_limit_mb`
when it is set (labelled *read from config*), otherwise applies the macOS default — about 3/4 of
memory above 36 GB, about 2/3 at or below — labelled *estimated*.

## History

While the monitor runs, `history::recorder` feeds every reading to a pure event detector
(`history::events`: an event is a stretch of strained or critical pressure, ended by 30 seconds of
calm) and writes a compact snapshot every 30 seconds to a SQLite database (`history::store`).
The schema is versioned with `PRAGMA user_version` and marked as canfit's with
`PRAGMA application_id`; ordered migrations upgrade older files in place, and canfit refuses —
without touching them — files written by a newer canfit or by another program
(`history::schema`). Retention deletes records past their age limit and, above the size cap, the
oldest first, then returns the space with incremental vacuum. There is no daemon: nothing is
recorded while the monitor is closed. `canfit watch` adds one row per watched run to the same
database (schema version 2), kept 90 days. See [Usage](usage.md#history) for paths and
settings.

## Data and privacy

canfit sends no telemetry and contains no network code. The only network access is `canfit
update` on a copy installed by the shell or PowerShell installer: after you confirm, canfit runs
the separate updater program `canfit-update`, which asks GitHub for the latest release and
installs it. Other installs are told which package-manager command to run. History and settings stay in your user data directories: the history
database under the local data directory (`~/Library/Application Support/canfit` on macOS,
`%LOCALAPPDATA%\canfit\data` on Windows, `~/.local/share/canfit` on Linux); delete the file to
delete your history.
