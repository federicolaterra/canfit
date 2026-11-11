# Apps

`canfit "Docker Desktop"`, `canfit Slack` and `canfit Chrome "Docker Desktop" model.gguf` answer
*is there room for this right now?* for apps, alone or together with models. canfit works out
each app's need from the best source it has, in this order:

1. **Declared configuration** — the memory the app is *configured to use*: Docker Desktop's VM
   size, the RAM you gave a virtual machine, a Java app's maximum heap ([below](#apps-canfit-knows)).
2. **History** — what canfit **measured** the app use on this machine: first a run measured by
   `canfit watch` ([Measured by watch](#measured-by-watch)), else what the monitor saw while it
   was open ([History](#estimates-from-history)).
3. **Seed catalog** — a rough average for about 30 common apps canfit ships with, always labelled
   *estimate from catalog* ([The seed catalog](#the-seed-catalog)).
4. **Unknown** — none of the above: canfit says so and exits with code `3`. It never guesses.

If the app is already running, canfit subtracts what it holds now and compares the rest with the
memory available now. The card names the source and the confidence of every number.

```text
✗ No, Docker Desktop is at risk right now: it is set to use up to 11.0 GB with the safety margin and only 4.2 GB is available.

╭─ Can Docker Desktop fit? ───────────────────────────────────────────────────────────╮
│  Docker Desktop VM  10.0 GB  read from config · memoryMiB = 10240 in settings.json  │
│  Margin              1.0 GB  estimated · 10% safety margin                          │
│  Needed             11.0 GB  estimated · confidence high                            │
│                                                                                     │
│  Available now       4.2 GB  measured · ✗ 6.8 GB short                              │
╰─────────────────────────────────────────────────────────────────────────────────────╯
  Read from Docker Desktop settings (settings.json).
  The memory an app is configured to use is a maximum: it may use less. Available memory includes
  file cache the system can reclaim.
```

## Apps canfit knows

| App | Where the memory comes from | OS |
|---|---|---|
| Docker Desktop | VM memory (`memoryMiB` / `MemoryMiB`) in `settings-store.json` or `settings.json`; on Windows with the WSL 2 backend, `memory=` under `[wsl2]` in `%UserProfile%\.wslconfig`, else WSL's default (half of RAM, estimated) | macOS, Windows, Linux |
| UTM | Each VM's `config.plist` (`System › MemorySize`) in UTM's default folder | macOS |
| Parallels Desktop | Each VM's `config.pvs` (`Hardware › Memory › RAM`) in `~/Parallels` | macOS |
| VMware Fusion / Workstation | Each VM's `.vmx` (`memsize`), from VMware's inventory and default folders | macOS / Windows, Linux |
| VirtualBox | Each VM's `.vbox` (`RAMSize`), from `VirtualBox.xml` and `~/VirtualBox VMs` | macOS, Windows, Linux |
| IntelliJ IDEA, PyCharm, WebStorm, PhpStorm, CLion, GoLand, Rider, RubyMine, DataGrip, RustRover, Android Studio | `-Xmx` from your `.vmoptions`, else the one the IDE ships, plus an estimate for memory beyond the heap | macOS, Windows, Linux |

Names are matched ignoring case, with aliases (`canfit docker`, `canfit intellij`). A name canfit
doesn't know at all exits with code `3` and suggests close matches. If an app above has a profile
but its settings can't be read, canfit falls back to history, then the catalog, and says why in a
note; with neither, it exits `3` with the "could not read settings" sentence. To check an app whose name is a
subcommand, use `--app`: `canfit --app history`.

Settings files the readers look at:

| App | macOS | Windows | Linux |
|---|---|---|---|
| Docker Desktop | `~/Library/Group Containers/group.com.docker/` | `%APPDATA%\Docker\` (+ `%UserProfile%\.wslconfig`) | `~/.docker/desktop/` |
| UTM | `~/Library/Containers/com.utmapp.UTM/Data/Documents/*.utm/config.plist` | — | — |
| Parallels Desktop | `~/Parallels/*.pvm/config.pvs` | — | — |
| VMware | `~/Library/Application Support/VMware Fusion/vmInventory`, `~/Virtual Machines.localized/` | `%APPDATA%\VMware\inventory.vmls`, `Documents\Virtual Machines\` | `~/.vmware/inventory.vmls`, `~/vmware/` |
| VirtualBox | `~/Library/VirtualBox/VirtualBox.xml`, `~/VirtualBox VMs/` | `%UserProfile%\.VirtualBox\VirtualBox.xml`, `%UserProfile%\VirtualBox VMs\` | `~/.config/VirtualBox/VirtualBox.xml`, `~/VirtualBox VMs/` |
| JetBrains IDEs | `/Applications/<IDE>.app/Contents/Resources/product-info.json`, `~/Library/Application Support/JetBrains/<IDE><version>/` | `Program Files\JetBrains\<IDE> <version>\`, `%APPDATA%\JetBrains\<IDE><version>\` | `/opt/<ide>*`, Toolbox apps, `~/.config/JetBrains/<IDE><version>/` |

canfit only **reads** these files: it never changes them, never runs the app or its command-line
tools, and never connects anywhere. A file it can't read becomes a "could not read settings" note;
if nothing can be read, the check exits with code `3`.

## Virtual machines

`canfit UTM` lists every VM with its RAM and its own verdict; the answer and the exit code are for
the **largest** VM. To check one VM, name it after a colon:

```bash
canfit "UTM: Ubuntu 24.04"
canfit "UTM: ubuntu"          # a unique beginning of the name is enough
canfit "VirtualBox: Debian"
```

## What the labels mean

- **read from config**: the number is in the app's settings (Docker's VM size, a VM's RAM,
  `-Xmx`). It is what the app is allowed to use — a maximum; it may use less.
- **estimated**: canfit's own conservative figure — the 10 % safety margin (`--margin`), the
  memory a Java app needs beyond its heap (code cache + 1 GB for class data, threads and native
  memory), or a documented default when the setting is absent (WSL: half of RAM; a JetBrains IDE
  without `-Xmx`: 2 GB).
- **measured**: memory available now, and what the app already holds if it is running.
- **measured by watch on DATE**: the peak of a run measured by `canfit watch` (plus, for a model
  server that maps its model file, the model's weights read from the file).
- **measured by canfit**: from the monitor's history with enough evidence (medium or high confidence): the 95th
  percentile of what canfit measured the app use.
- **estimated from little history**: from history with low confidence (seen briefly, long ago, or
  only the running totals left).
- **estimate from catalog**: the seed catalog's rough average. Never a measurement.

Every app check also has a **confidence**:

| Source | `high` | `medium` | `low` |
|---|---|---|---|
| Configuration | read from settings | read from settings plus an estimated part (a Java app's overhead) | a default canfit assumed because the setting is absent |
| Watch | a run of ≥ 30 s with ≥ 10 samples | a shorter run | — |
| History | ≥ 120 samples, ≥ 1 h watched, on ≥ 2 days, last seen ≤ 14 days ago | ≥ 20 samples, ≥ 10 min watched, last seen ≤ 30 days ago | anything less |
| Catalog | — | — | always |

## Estimates from history

While the monitor (`canfit` with no arguments) is open, canfit records every 30 seconds the memory
of the 8 largest apps above 100 MB ([History](usage.md#history)). `canfit APP` then uses:

- **need**: the **95th percentile** of the stored samples (nearest rank) — what the app reaches in
  a busy moment, not its single worst spike;
- **typical**: the median, shown as context, with the largest use ever recorded;
- **evidence**: how many samples, about how long canfit watched (samples × the sampling interval),
  on how many days and how recently: "seen for 3 h over 4 days, last today".

Even one short monitor session gives an estimate, with low confidence and a note saying how little
was seen. Samples are kept for the history retention (7 days by default); the running totals for
90 days. When only the totals are left, the need is the largest use recorded, with low confidence.
Because only large apps are sampled, history leans high — the conservative direction.

History is looked up by the app's group name as the monitor shows it ("Google Chrome", "Slack")
and by the names and process names of its catalog entry or profile, so `canfit chrome` finds
"Google Chrome". An app only history knows can be checked by the name the monitor shows.

See what canfit has learned:

```bash
canfit history --apps           # every app canfit measured, largest need first
canfit history --apps --json    # canfit.learned_apps, schema version 1
```

In `canfit.learned_apps` and in the `history` object of `canfit.fit`, `method` is `monitor` or
`watch`; a watched value also has a `watch` object (`program`, `started_unix_ms`, `duration_ms`,
`peak_bytes`, `exit_code`, `exit_signal`, `interrupted`, `mapped_model_bytes`, `runs`).
`canfit.learned_apps` also has a top-level `history` field: `"off"` when recording was disabled for
the run (`--no-history`, `CANFIT_NO_HISTORY`, `[history] enabled = false`), otherwise `"on"`. With
recording off, app checks still read what canfit learned before ([Turning history
off](usage.md#turning-history-off)); `canfit watch --no-save` measures a command without adding it.

## Measured by watch

`canfit watch -- COMMAND` runs a command and measures the whole process tree it starts until it
ends ([Watch](usage.md#watch)). The run is remembered under `--name NAME`, or the
program's file name:

```bash
canfit watch --name "LM agent" -- ./run-agent.sh
canfit "LM agent"                # need = the run's peak, "measured by watch on 2026-11-10"
canfit "LM agent" model.gguf     # ...summed with other targets as usual
```

- **need**: the peak of the most recent usable run (a run that failed within 30 seconds without
  Ctrl-C, or measured nothing, is not saved). When the command mapped a GGUF model from its file
  (llama.cpp's default) on macOS or Windows, whose measures leave mapped file pages out, the
  model's weights are added as a separate **Model file** row, read from the file.
- **confidence**: high for a run of 30 seconds and 10 samples or more, else medium — always
  *measured*. A short run gets a note: it may not have reached its full size.
- **order**: a watched run comes before the monitor's samples of an app with the same name;
  declared configuration still comes first.
- A brief spike between two samples (1 s by default) is not in the peak.

Watch the run you want canfit to plan for: a typical session, with the context and the load you
normally use.

An illustrative example:

```text
canfit has learned how much memory 3 apps use; 2 with enough evidence to count as measured.

╭─ What canfit learned about apps ──────────────────────────────────────────────────────────╮
│  Google Chrome   3.6 GB  measured · typical 2.9 GB · peak 4.1 GB · confidence high        │
│                          seen for 6 h 20 min over 3 days, last today                      │
│  Slack           930 MB  measured · typical 880 MB · peak 990 MB · confidence medium      │
│                          seen for 40 min, last yesterday                                  │
│  Figma           1.4 GB  estimated · typical 1.2 GB · peak 1.4 GB · confidence low        │
│                          seen for 2 min, last 5 days ago                                  │
╰───────────────────────────────────────────────────────────────────────────────────────────╯
```

## The seed catalog

[`src/estimate/apps/catalog.toml`](../../src/estimate/apps/catalog.toml) holds rounded averages for
about 30 common apps: browsers (Chrome, Firefox, Safari, Edge, Brave, Arc), chat and meetings
(Slack, Discord, Teams, WhatsApp, Telegram, Zoom), music and video (Spotify, Music, VLC), editors
and IDEs (VS Code, Cursor, Zed, Sublime Text, Xcode), design (Figma, Photoshop, Blender, GIMP),
notes, office and mail (Notion, Obsidian, Word, Excel, Outlook) and AI apps (LM Studio, ChatGPT,
Claude). It is built into the binary: canfit never downloads it.

Catalog values are used only for an app canfit has no settings for and has never seen, always
with low confidence and the label *estimate from catalog*. They describe a moderate workload (a
browser with about a dozen tabs, an editor with one project open) and are **not measurements of
your machine**: the first monitor session that sees the app replaces them with real numbers.

```toml
[[app]]
name = "Slack"
aliases = []                         # other names people type
os = ["macos", "windows", "linux"]   # optional; default: all three
category = "chat"                    # browser, chat, meeting, music, video, editor, ide, design,
                                     # office, notes, mail, ai, game, other
typical_mb = 600                     # typical use of the whole app (all its processes), MiB
need_mb = 1200                       # what canfit checks: a busy but normal session, MiB
processes = ["Slack", "slack", "slack.exe"]   # app group or process names on each OS
source = "maintainer estimate, 2026-11"
```

### Where the numbers come from

The first values are rounded starting points chosen by canfit's maintainers (November 2026), not
measurements; each entry says so in `source`. Contributions should replace them with measured
values:

1. Use the app normally with the monitor open for at least an hour on two different days, so
   `canfit history --apps` reports it with **high** confidence.
2. Take `need_mb` from its first number (the 95th percentile) and `typical_mb` from "typical",
   rounded to 50 MB (100 MB above 1 GB).
3. Say in `source` what you measured: OS, memory kind (physical footprint on macOS, private working
   set on Windows, PSS on Linux), workload, and the date — for example
   `"measured on macOS 15, footprint, 12 tabs, 2026-12"`. Measurements from two or more machines
   or OSes are better; use the larger.

### Review rules

A test validates the file on every OS (`cargo test catalog`): the schema, unique names and aliases
ignoring case, known categories and OSes, values between 50 MB and 16 GB, `need_mb ≥ typical_mb`,
at least one process name and a `source`. Reviewers also check that:

- the app is common and has no profile (apps that declare their memory get a profile instead);
- the numbers come from measurements described in `source`, or the change says why not;
- a change of more than half to an existing value is explained;
- names and process names are the ones the monitor shows on each OS.

Open a pull request that changes only `catalog.toml` (plus its test, if the counts change), titled
`feat: add <App> to the app catalog` or `fix: update <App> catalog values`.

## Several things together

Give several targets, in any mix of models and apps; their needs are summed and there is one
verdict and one exit code:

```bash
canfit "Docker Desktop" qwen3-14b-q4_k_m.gguf
canfit Chrome Slack "IntelliJ IDEA"
canfit Chrome "Docker Desktop" --make-room
```

An illustrative example:

```text
✗ No, qwen3-14b + Docker Desktop don't fit together right now: they need about 18.2 GB with the safety margin and only 9.3 GB is available.

╭─ Can qwen3-14b + Docker Desktop fit? ──────────────────────────────────────────────╮
│  qwen3-14b         6.5 GB  estimated · model · Q4_K_M, 14.8B params               │
│                            weights read from GGUF; 8,192 tokens of f16 cache       │
│  Docker Desktop   10.0 GB  read from config · app · confidence high                │
│                            memoryMiB = 10240 in settings.json                      │
│  Margin            1.7 GB  estimated · 10% safety margin                           │
│  Needed           18.2 GB  estimated · sum of 2 items                              │
│                                                                                    │
│  Available now     9.3 GB  measured · ✗ 8.9 GB short                               │
│  GPU limit        21.3 GB  estimated · ● 14.1 GB to spare · models only (7.2 GB)   │
╰────────────────────────────────────────────────────────────────────────────────────╯
```

Models are listed first, then apps. Each line names its source and confidence. Apps that are
already running count only what they don't hold yet. The Apple Silicon GPU limit applies to the
models' part only: apps don't use GPU memory. A VM app counts its largest VM unless you name one.
`--make-room` / `canfit free A B …` finds the smallest safe-first set of apps to close and never
suggests any of the targets (or what may hold their memory) or a never-class process.


## Already running

If the app is running, its memory is already part of "used" memory, so it only needs the rest:

```text
Docker Desktop is already running: 3.1 GB in use of 4.0 GB.
```

canfit finds the running app by its app bundle or process names. For virtual machines it can't
tell which VM a running hypervisor process belongs to, so nothing is subtracted and a note says
the need may be overstated. On macOS, Docker Desktop and UTM may run their VM inside a shared
system process (`com.apple.Virtualization.VirtualMachine`) that canfit can't attribute; that is
noted too.

## Verdicts and exit codes

The same rules as for models ([Memory estimation](estimation.md)), with words for apps:

| Verdict | When | Exit code |
|---|---|---|
| ● **fits comfortably** | the need plus the margin fits in the memory available now | `0` |
| ▲ **fits but will slow down** | it fits only without the margin, or memory pressure is already critical: the system will compress or swap | `1` |
| ✗ **at risk** | even the need without the margin is more than is available | `2` |

Only available memory is checked: the Apple Silicon GPU limit applies to models, not apps.
`canfit "Docker Desktop" --make-room` (or `canfit free "Docker Desktop"`) says what to close; it
never suggests the app being checked. Several targets together have one verdict, worded
fits / tight / doesn't fit when a model is among them ([above](#several-things-together)).

## Add a profile

Profiles are small TOML blocks. canfit's own are in
[`src/estimate/apps/profiles.toml`](../../src/estimate/apps/profiles.toml); yours go in
`apps.toml` next to canfit's `config.toml` (macOS `~/Library/Application Support/canfit/`, Windows
`%APPDATA%\canfit\config\`, Linux `~/.config/canfit/`), or in the file named by `$CANFIT_APPS`.
A profile with the same name as a built-in one replaces it.

```toml
schema_version = 1

[[app]]
name = "Example VM Manager"
aliases = ["example"]
os = ["macos", "linux"]          # optional; default: all three
readers = [
  { kind = "json", key = "vm.memoryMiB", unit = "MiB", label = "Example VM",
    paths = { macos = ["{config}/Example/settings.json"], linux = ["{config}/example/settings.json"] } },
]
macos = { bundles = ["Example VM Manager"] }
linux = { names = ["example-vm"] }
```

| Key | Meaning |
|---|---|
| `name`, `aliases` | Names people type; matched ignoring case |
| `os` | OSes the app runs on (`macos`, `windows`, `linux`) |
| `readers` | Tried in order; the first that reads a value wins |
| `macos` / `windows` / `linux` | How to find the running app: `bundles` (macOS `.app` names), `names` (process names, `.exe` optional), `shared` (shared processes that may hold its memory; only noted) |

Reader kinds:

| `kind` | Reads | Options |
|---|---|---|
| `json` | A number in a JSON file | `key` (dotted path, case ignored), `unit` (`bytes`, `KiB`, `MiB`, `GiB`), `label`, `paths` |
| `jvm_options` | `-Xmx` (and `-XX:ReservedCodeCacheSize`) from a `.vmoptions` or `.ini` file, plus the Java overhead | `paths` |
| `jetbrains` | A JetBrains-style IDE | `install_names`, `data_dirs`, `launcher`, `vendor` (default `JetBrains`) |
| `docker_desktop`, `utm`, `parallels`, `vmware`, `virtualbox` | The apps above | none |

`paths` lists files per OS; use `{home}`, `{config}` (macOS `~/Library/Application Support`,
Windows `%APPDATA%`, Linux `~/.config`) and `{data}` (macOS `~/Library/Application Support`,
Windows `%LOCALAPPDATA%`, Linux `~/.local/share`), with `/` as separator on every OS. A profile
file that doesn't parse stops canfit with exit code `3` rather than being ignored.

To contribute a profile to canfit itself, add the block to `profiles.toml`, a fixture settings
file under `tests/fixtures/apps/`, and a test; see [Development](development.md). An app whose
settings need a new reader is a good first issue to discuss.

## Not in 1.0

- An online or community catalog download: the catalog ships inside canfit and is updated with
  canfit releases.
- History records only while the monitor is open; there is no background recorder yet.
- UTM VMs opened from outside UTM's default folder are not listed.
