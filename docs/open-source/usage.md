# Usage

> [!NOTE]
> canfit 1.0 does all of this on macOS, Windows and Linux: the live monitor (see
> [Live monitor](#live-monitor)) or a one-shot snapshot when piped (see
> [Snapshot](#snapshot)); `canfit model.gguf` (see [Fit check](#fit-check));
> `canfit "Docker Desktop"` and `canfit Slack` for apps, from their settings, from what canfit
> measured them use, or from its catalog, alone or together with models (see
> [App fit check](#app-fit-check) and [Apps](apps.md)); `canfit free` / `--make-room` (see
> [What to close](#what-to-close)); `canfit models DIR` (see
> [Models catalog](#models-catalog)); `canfit history` (see [History](#history));
> `canfit watch -- CMD` (see [Watch](#watch)); and `canfit update` (see
> [Update](#update)).

## Commands

The name is the question, so there is no `fit` subcommand: the check is the default action.

| Command | What it does |
|---|---|
| `canfit` | Live monitor, like `top`, starting with a one-sentence summary |
| `canfit model.gguf` | Can this model fit in the memory available right now? |
| `canfit model.gguf --ctx 32768` | The same check with a 32k-token context |
| `canfit model.gguf --make-room` | If it doesn't fit, suggest what to close |
| `canfit free [model.gguf]` | What should I close (to make room)? |
| `canfit history [--since 7d]` | Timeline of pressure events and their culprits |
| `canfit history --apps` | What canfit has learned about each app's memory |
| `canfit models ~/models` | Which GGUF files in a folder fit right now |
| `canfit update [--yes]` | Update canfit, or say which package manager updates it |
| `canfit "Docker Desktop"` | Is there room for this app, from the memory its settings declare? |
| `canfit "UTM: Ubuntu"` | The same for one virtual machine of a VM app |
| `canfit Slack` | The same for an app without settings: from history, else the seed catalog |
| `canfit "Docker Desktop" model.gguf` | Can both fit together? Targets are summed |
| `canfit watch -- llama-server -m m.gguf` | Launch a command, measure the real memory of it and everything it starts, and remember the peak |
| `canfit watch --no-save -- CMD` | The same, without remembering the run |
| `canfit --no-history` | Any command, recording nothing in this run ([Turning history off](#turning-history-off)) |

### Targets

A target is detected automatically:

- a path ending in `.gguf`, or an existing file that starts with the GGUF signature (such as an
  Ollama blob), is a model; any part of a split model (`name-00001-of-00003.gguf`) stands for all
  its parts;
- a folder holding exactly one model (one file or one split set) is that model;
- anything else is an app name (quote names with spaces), matched ignoring case against the apps
  canfit has a profile for, then the apps it has measured, then its seed catalog
  ([Apps](apps.md)); `APP: VM` names one virtual machine. A folder without models that is named
  like a known app is the app. A name none of them knows exits with code `3` and suggests close
  matches. Several targets — models, apps or both — are summed into one answer.

`free`, `watch`, `history`, `models`, `update` and `help` are reserved words. To check an app with
one of those names, use `--app`: `canfit --app history`.

### Options

| Option | Meaning |
|---|---|
| `--ctx <TOKENS>` | Context length used to estimate the model's KV cache (default 8,192, or the model's own context if smaller); also for `models` |
| `--cache-type <TYPE>` | KV cache type: `f16` (default), `q8_0` or `q4_0`, as llama.cpp's `--cache-type-k/-v` |
| `--margin <PERCENT>` | Safety margin added to the estimate, `0`–`100` (default `10`) |
| `--make-room` | Also say what to close so the targets fit, and offer to close it after confirmation; same as `canfit free TARGET` (macOS, Windows) |
| `--app <NAME>` | Treat `NAME` as an app, even when it is a subcommand or a file name |
| `--since <WHEN>` | `history` only: how far back to look — a duration (`30m`, `12h`, `7d`, `2w`) or a local date or time (`2026-11-09`, `"2026-11-09 14:30"`); default `24h` |
| `--apps` | `history` only: list what canfit has learned about apps instead of pressure events |
| `--interval <SECONDS>` | Refresh interval of the live monitor, `0.5`–`60` (default `1`); not accepted with targets. `watch` takes its own: seconds between samples, same range and default |
| `--name <NAME>`, `-q`/`--quiet`, `--json-out <FILE>`, `--no-save` | `watch` only: see [Watch](#watch) |
| `--no-history` | Write nothing to the history database in this run; past history is still read. Accepted by every command, before or after the subcommand. Same as `CANFIT_NO_HISTORY=1`. See [Turning history off](#turning-history-off) |
| `--theme <THEME>` | `dark` (default), `light` or `high-contrast`, for the monitor and the coloured cards |
| `--json` | Machine-readable output; accepted by every command |
| `-h`, `--help` / `-V`, `--version` | Help and version |

## Exit codes

| Exit code | Meaning |
|---|---|
| `0` | Fits |
| `1` | Fits, but tight / will slow down (swap, compression) |
| `2` | Doesn't fit (for apps: at risk) |
| `3` | canfit could not complete the request: a missing, unreadable, truncated or malformed model file, an unknown or ambiguous app name (no settings, history or catalog value), app settings it could not read with no history or catalog to fall back on, an unreadable `apps.toml`, a failed or unconfirmed `canfit update`, an unreadable config file, a folder `models` can't read, a history database it must not touch (newer or not canfit's), or an OS it can't read yet |
| `64` | Invalid command-line usage |

```bash
canfit model.gguf && llama-server -m model.gguf
```

Codes `0`–`2` are reserved for fit answers so that a typo or a runtime error never looks like
"doesn't fit". Errors are explained in one sentence on stderr, also with `--json`.
`canfit history` and `canfit models` are listings, not fit checks: they exit `0` whenever they
could answer (whatever fits), `3` on errors and `64` on usage errors. `canfit watch` exits with
the watched command's own exit code (see [Watch](#watch)).

## Results

For models the answer has three levels — **fits** · **tight** · **doesn't fit** — and for apps
**fits comfortably** · **fits but will slow down** · **at risk**. Every number is labelled as
measured, read from config, or estimated; see [Memory estimation](estimation.md).

## Live monitor

`canfit` with no arguments, on a terminal, opens a live monitor (macOS, Windows and Linux). It
starts with one sentence saying what is going on, then four cards — memory, swap, CPU and pressure — each with a
bar and a graph of the last few minutes, the apps using the most memory (an app and all its
helpers are one row), and a "Why?" panel explaining the pressure and the selected app.

```text
 ✗ Memory is critical: macOS reports memory pressure as critical; only 931 MB of
16.0 GB memory is available. Google Chrome is using 9.8 GB.
╭─ ✗ Memory ───────╮╭─ ▲ Swap ─────────╮╭─ ● CPU ──────────╮╭─ ✗ Pressure ─────╮
│931 MB available  ││growing 64.0 MB/s ││23% of 10 cores   ││critical · 90/100 │
│████████████░  94%││███████████░░  81%││███░░░░░░░░░░  23%││████████████░  90%│
│▄▄▄▄▄▄▄▄▅▅▅▅▅▅▅▅▅▅││▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆▆││▁▁▁▁▁▁▂▂▂▂▂▂▃▃▃▃▃▃││▃▄▄▄▄▄▄▅▅▅▅▅▆▆▆▆▆▆│
╰──────── measured ╯╰──────── measured ╯╰──────── measured ╯╰─────── estimated ╯
╭─ Apps · largest first ───────────────────────────────────────────────────────╮
│▸ ● Google Chrome                                9.8 GB   2 processes  safe   │
│  ▲ Ollama                                       4.0 GB     1 process  caution│
│  ▲ Visual Studio Code                           1.5 GB     1 process  caution│
│  ● Slack                                       1000 MB     1 process  safe   │
│  ✗ WindowServer                                 900 MB     1 process  never  │
│  ▲ Some Tool                                    400 MB     1 process  caution│
│  ● Spotify                                      300 MB     1 process  safe   │
│  ✗ Terminal                                     300 MB     1 process  never  │
│  ✗ Finder                                       190 MB     1 process  never  │
╰ 1/15 ───────────────────────────────────────── measured · physical footprint ╯
╭─ Why? ───────────────────────────────────────────────────────────────────────╮
│● Google Chrome: safe, a browser: it can reopen your tabs. Press x to close it│
│(canfit asks first).                                                          │
│Memory is critical because macOS reports memory pressure as critical; only 931│
╰────────────────────────────────────────────────────────────────────── ? more ╯
 ↑↓ move  x close  ? explain  t theme  q quit                  every 1 s · dark
```

*Captured from a render test with fake apps (`docs/assets/`), not from a real machine.*

Every card says whether its values are **measured** or **estimated**. Each status colour is paired
with a symbol — ● calm / safe, ▲ strained / caution, ✗ critical / never — so nothing depends on
colour alone. Cards sit in one row from 80 columns and wrap into two rows on narrower terminals;
from 110 columns the "Why?" panel moves beside the app list, and tall terminals get taller graphs.

### Keys

| Key | Action |
|---|---|
| `↑` `↓` or `k` `j` | Move the selection in the app list |
| `PgUp` `PgDn`, `g` `G` (`Home` `End`) | Jump by a page, to the first or the last app |
| `x` | Close the selected app — canfit shows every process it would close and asks first |
| `?` | Explain: the full "Why?" paragraph, the selected app's class and reason, symbols and keys |
| `h` | History: pressure events of the last 24 hours, as `canfit history` shows them |
| `t` | Switch theme: dark → light → high contrast |
| `Esc` | Close the explanation, the history panel or the last message |
| `q`, `Ctrl-C` | Quit |
| mouse | Click an app to select it; the wheel scrolls the list |

`x` runs the same confirmed closing as `canfit free`: a dialog lists every process that will be
asked to quit and waits for `y` (any other key is a no), then canfit asks the app to quit
gracefully, waits up to 10 seconds, and asks a second time before forcing anything still running.
Apps marked ✗ — the operating system itself, other users' processes, your terminal and anything
canfit runs inside, canfit itself and protected apps — can't be closed: `x` only explains why.
Closing works on macOS, Windows and Linux. Because the monitor uses the mouse, selecting text with it
needs a modifier key held down; which one depends on the terminal (often `Option` on macOS,
`Shift` on Linux and in Windows Terminal).

On Windows the monitor runs in Windows Terminal, VS Code's terminal and the classic console
window (conhost). The classic console's default fonts can't draw some symbols, so there canfit
uses ASCII stand-ins: `+` safe or calm, `!` caution or strained, `x` never or critical, `#`
for bars and `+-|` for borders. Set `CANFIT_ASCII=1` to use them in any terminal, or
`CANFIT_ASCII=0` to keep the Unicode symbols in a console whose font has them. Piped output and
`--json` are never changed.

`k` moves up, as in `less` and `vim`; closing is on `x` so that a navigation key can never start a
close.

### Refresh and resource use

Memory, swap, CPU and pressure refresh every second (`--interval` changes it). The app list is
rescanned at most every 2 seconds, because reading every process is the costly part. On an M1 Pro
the monitor used under 1 % of one core (0.7–0.9 %, measured). There are no animations, and the
screen is redrawn only after a reading or a key press.

### Themes and colour

`--theme dark` (default) suits dark terminal backgrounds, `--theme light` light ones, and
`--theme high-contrast` uses bold, saturated colours without dimmed text. Themes use your
terminal's own palette. Setting `NO_COLOR` (to any non-empty value) or `TERM=dumb` removes every
colour in all themes; symbols, bold and the reversed selection remain.

The fit check and `canfit free` print their cards with the same card component, in colour, when
stdout is a terminal. Piped or redirected output, and `NO_COLOR`, keep the plain text shown below.

When stdin or stdout is not a terminal (a pipe, a script, a file), or with `--json`, `canfit`
prints a one-shot snapshot instead of opening the monitor.

The terminal is always restored on exit, on errors and on a crash. If canfit is killed with a
signal (for example `kill -9`), run `reset` to restore the terminal.

## Fit check

`canfit model.gguf` reads the model's header (never the weights), estimates the memory it needs
and compares that with the memory available now. On Apple Silicon it also checks the share of
memory the GPU may use. The answer comes first, then a card with every number labelled:

```text
✗ No, qwen3-8b-Q3_K_M doesn't fit right now: it needs about 5.9 GB and only 4.1 GB is available.

╭─ Can qwen3-8b-Q3_K_M fit? ───────────────────────────────────────╮
│  qwen3-8b-Q3_K_M           Q3_K · 8.2B params · 36 layers        │
│  Weights           3.8 GB  read from GGUF (exact)                │
│  KV cache          1.1 GB  estimated · 8,192 tokens, f16 cache   │
│  Overhead          399 MB  estimated · compute buffers, runtime  │
│  Margin            548 MB  estimated · 10% safety margin         │
│  Needed            5.9 GB  estimated                             │
│                                                                  │
│  Available now     4.1 GB  measured · ✗ 1.8 GB short             │
│  GPU limit        10.7 GB  estimated · ● 4.8 GB to spare         │
╰──────────────────────────────────────────────────────────────────╯
  Context 8,192 tokens is canfit's default; the model supports 40,960. Change it with --ctx.
  Assumes llama.cpp with every layer on the GPU, the default on Apple Silicon.
  Available memory includes file cache the system can reclaim. Needed memory is an estimate: real use varies with the runtime and its settings.
```

Several model targets are summed (`canfit a.gguf b.gguf`). How the estimate and the verdict are
computed — and what "tight" means exactly — is in [Memory estimation](estimation.md). The check
takes about 0.3 s and needs no admin rights.

### `canfit model.gguf --json`

The fit check prints one JSON document. Sizes are integers in bytes; enums are `snake_case`.

| Field | Meaning |
|---|---|
| `schema` | Always `"canfit.fit"` |
| `schema_version` | `1`. Increases on any breaking change; new fields may be added without a bump |
| `canfit_version` | Version of canfit that wrote the document |
| `summary` | The one-sentence answer, as printed first in the text output |
| `verdict` | `fits`, `tight` or `doesnt_fit` |
| `exit_code` | The process exit code for this verdict: `0`, `1` or `2` |
| `deciding_limit` | The limit that decided the verdict: `available_memory` or `gpu_memory_limit` |
| `needed` | Sum over all models: `weights_bytes`, `kv_cache_bytes`, `overhead_bytes`, `margin_bytes`, `base_bytes` (without margin), `total_bytes` (with margin), `provenance` (always `estimated`) |
| `checks` | One entry per limit: `limit`, `limit_bytes`, `provenance`, `verdict`, `tight_cause` (`within_margin`, `pressure_critical` or `null`), `headroom_bytes` (limit − total; negative when short) |
| `pressure_level` | Current pressure level: `calm`, `strained` or `critical` |
| `memory_total_bytes` | Installed memory |
| `models` | One entry per model, see below |

Each entry of `models` holds the facts read from the file — `name`, `path`, `parts` (every file of
a split model), `gguf_version`, `architecture`, `parameter_count`, `quantization`,
`bits_per_weight`, `weights_bytes`, `tensor_count`, `file_bytes`, `block_count`,
`context_length`, `embedding_length`, `head_count`, `head_count_kv`, `key_length`,
`value_length`, `expert_count`, `expert_used_count`, `sliding_window`, `kv_lora_rank` (`null`
when the file does not say) — and its `estimate`: the same byte components as `needed`, plus
`context_tokens`, `context_source` (`flag`, `default` or `model`), `cache_type`,
`margin_percent`, `overhead_profile` (`apple_silicon` or `uncalibrated`) and `notes`
(plain-language caveats).

```bash
canfit model.gguf --json | jq '.verdict, .needed.total_bytes'
```

## App fit check

`canfit "Docker Desktop"` reads the memory the app is configured to use from its own settings;
`canfit Slack` uses what canfit measured Slack use while the monitor was open (the 95th
percentile, with a confidence that grows with the evidence), or, for an app canfit has never seen,
the seed catalog's rough estimate (low confidence). canfit subtracts what the app already holds
if it is running, adds the safety margin and compares the result with the memory available now.
Sources, confidence rules, the catalog and how to contribute to it are in [Apps](apps.md).

```text
✗ No, UTM's largest VM (Ubuntu 24.04) is at risk right now: it is set to use up to 8.8 GB with the safety margin and only 4.2 GB is available.
Of UTM's 3 virtual machines, Metasploitable 2 fits comfortably; Kali Linux fits but will slow down; Ubuntu 24.04 is at risk. Check one with canfit "UTM: NAME".

╭─ Can UTM fit? ───────────────────────────────────────────────────────────╮
│  Kali Linux        4.0 GB  read from config · ▲ fits but will slow down  │
│  Metasploitable 2  2.0 GB  read from config · ● fits comfortably         │
│  Ubuntu 24.04      8.0 GB  read from config · ✗ at risk                  │
│  Margin            819 MB  estimated · 10% safety margin                 │
│  Needed            8.8 GB  estimated · largest VM · confidence high      │
│                                                                          │
│  Available now     4.2 GB  measured · ✗ 4.6 GB short                     │
╰──────────────────────────────────────────────────────────────────────────╯
```

The verdict is **fits comfortably** (exit `0`), **fits but will slow down** (`1`) or **at risk**
(`2`), with the same bands and `--margin` as models; only available memory is checked (the Apple
Silicon GPU limit applies to models only). `--make-room` and `canfit free NAME` never suggest
closing the app itself.

### Several targets

`canfit "Docker Desktop" model.gguf`, `canfit Chrome Slack` and any other mix are summed: the card
lists each item with its source and confidence (models first, then apps), the margin and the total,
and there is **one verdict and one exit code**. Available memory is checked against the total; the
Apple Silicon GPU limit only against the models' part. With a model among the targets the verdict
reads fits / tight / doesn't fit, otherwise fits comfortably / will slow down / at risk. An error in
any target (an unknown app, an unreadable model) exits `3`. `--make-room` never suggests closing
any of the targets.

### `canfit "Docker Desktop" --json`

The same `canfit.fit` document (schema version `1`, new fields only), with:

| Field | Meaning |
|---|---|
| `target_kind` | `app` for one app; `combined` for several targets with at least one app; `model` for models only |
| `verdict_label` | `fits comfortably`, `fits but will slow down` or `at risk` for apps only (with a model among the targets, or for models: `fits`, `tight`, `doesn't fit`) |
| `needed` | `base_bytes` = everything still needed without margin, `margin_bytes`, `total_bytes`; `apps_bytes` = the apps' part of `base_bytes`; for one app, `overhead_bytes` = estimated part (a Java app's memory beyond the heap), otherwise the models' overhead; `weights_bytes` and `kv_cache_bytes` are the models'; `provenance` is `estimated` unless the whole number was read from config (or measured) |
| `checks` | `available_memory` against the total; with a model on Apple Silicon also `gpu_memory_limit`, against the models' part only |
| `models` | The model targets (empty for apps only) |
| `apps` | One entry per app target, in command-line order, see below |

Each entry of `apps` holds:

| Field | Meaning |
|---|---|
| `name` | What was checked: `Docker Desktop`, `UTM: Ubuntu 24.04` |
| `profile`, `vm` | The profile used and, for `APP: VM`, the VM's configured name |
| `source` | `config` (the app's settings), `history` (what canfit measured) or `catalog` (the seed catalog) |
| `confidence` | `high`, `medium` or `low`; the rules per source are in [Apps](apps.md#what-the-labels-mean) |
| `reading` | `reader` (`docker_desktop`, `utm`, `jetbrains`, …), `source` (in words), `kind` (`app` or `virtual_machines`), `confidence`, `notes`, and `items`: `name`, `declared_bytes`, `declared_provenance` (`read_from_config` or `estimated`), `detail`, `overhead_bytes` (estimated), `overhead_detail`, `path` |
| `deciding_item` | Index of the item the verdict is for: the VM named, the largest VM, or the app |
| `declared_bytes` | Declared memory plus overhead of that item |
| `running` | Present when the app is running: `held_bytes` (`provenance` `measured`), `groups`, `pids`, `subtracted` (`false` for VMs, which canfit can't attribute) |
| `need_bytes` | `declared_bytes` minus what the running app holds when subtracted, never below 0 |
| `margin_percent` | The safety margin used |
| `vm_verdicts` | For a VM app checked as a whole: each VM's `name`, `needed_bytes` (with margin) and `verdict` |
| `notes` | Plain-language caveats, including settings canfit could not read |
| `history` | With `source: "history"`: `names` (app groups), `samples`, `detailed_samples`, `observed_ms`, `days`, `first_seen_unix_ms`, `last_seen_unix_ms`, `typical_bytes`, `p95_bytes`, `peak_bytes`, `need_bytes`, `confidence`, `provenance` (`measured` or `estimated`), `memory_kind` |
| `catalog` | With `source: "catalog"`: `category`, `typical_bytes`, `need_bytes`, `provenance` (always `estimated`), `source`, `reviewed` |

```bash
canfit "Docker Desktop" --json | jq '.verdict_label, .apps[0].need_bytes'
```

## What to close

`canfit free` lists the apps you could close, safe ones first, and explains in one paragraph what
is using memory. Never-class apps — the operating system itself, other users' processes, your terminal and
canfit itself, protected apps — are never listed; the biggest of them are named under "Not
suggested" so you know why they are missing. How apps are classified is in
[Process safety](process-safety.md).

Each suggestion also says how to close it yourself (`yourself: osascript -e 'quit app "Slack"'`,
`kill 4321`, `taskkill /PID 4321`, `systemctl --user stop app.service`, `ollama stop MODEL`, …):
the safest command for this OS and kind of process. The closing prompt and the monitor's close
dialog list them too, and `--json` has every command in `manual_commands`, forced ones last.
canfit never runs them ([Closing it yourself](process-safety.md#closing-it-yourself)).

`canfit model.gguf --make-room` and `canfit free model.gguf` are the same command: the fit check,
followed by the smallest set of apps to close so the model fits, and the verdict expected after
closing them (estimated). Both take `--ctx`, `--cache-type`, `--margin` and `--json`.

```text
✗ No, Qwen2.5 Coder 3B Instruct doesn't fit right now: it needs about 7.8 GB and only 3.6 GB is available.
To make room, close Google Chrome, Slack and com.apple.Virtualization.VirtualMachine: that frees about 4.7 GB (estimated), and then it should fit.

╭─ Can Qwen2.5 Coder 3B Instruct fit? ─╮
│  ...                                  │
╰───────────────────────────────────────╯
╭─ Close to make room ─────────────────────────────────────────────────────────────────────────────────────╮
│  1 ● Google Chrome                 2.1 GB  safe · measured · 25 processes                                │
│                                            a browser: it can reopen your tabs                            │
│  2 ● Slack                         831 MB  safe · measured · 7 processes                                 │
│                                            a chat or media app: it syncs and reopens where you left it   │
│  3 ▲ com.apple.Virtualization.Vi…  1.8 GB  caution · measured · 1 process                                │
│                                            a virtual machine: closing it stops it like pulling the plug  │
│                                                                                                          │
│  Frees                             4.7 GB  estimated · measured memory of 3 apps                         │
│  Available after                   8.2 GB  estimated · ● fits                                            │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────╯
  Why? Memory is calm: macOS is compressing memory to make room (3.4 GB of compressed memory). ...
```

When canfit runs in a terminal (and without `--json`), it then offers to close what it listed:

- `canfit free` asks for the numbers of the apps to close (`1 3`); Enter closes nothing.
- `--make-room` / `canfit free model.gguf` offer to close the apps in the plan.

Either way canfit first lists every process that will be asked to quit and waits for an explicit
`y`. It sends a graceful quit request, waits up to 10 seconds, and asks a **second** time before
forcing anything still running. Apps with unsaved work may refuse or ignore the graceful quit;
canfit then lists them so you can save first. Details: [Process safety](process-safety.md).

Exit codes: `canfit free` exits `0` (or `3` on errors). `--make-room` and `canfit free model.gguf`
exit with the fit-check code (`0`/`1`/`2`) for the memory available now — or, if you closed apps,
for the memory available after closing them.

The protected list lives in `config.toml` in canfit's config directory
(`~/Library/Application Support/canfit/config.toml` on macOS,
`%APPDATA%\canfit\config\config.toml` on Windows, `~/.config/canfit/config.toml` (or
`$XDG_CONFIG_HOME/canfit/config.toml`) on Linux, or the file named by `CANFIT_CONFIG`):

```toml
[processes]
protected = ["Slack", "llama-server"]
```

### `--make-room --json`

The `canfit.fit` document gains a `make_room` object (absent without `--make-room`):

| Field | Meaning |
|---|---|
| `status` | `not_needed` (it already fits), `plan` (closing `apps` frees enough), `ease_pressure` (it fits but pressure is critical; closing `apps` would ease it), `not_enough` (even every suggestable app is not enough; `apps` lists them all), `gpu_limit` (the GPU limit is the problem; closing apps can't help) |
| `needed_bytes` | Memory to free: needed (with margin) − available; `needed_provenance` is `estimated` |
| `freed_bytes` | Sum of the listed apps' memory; `freed_provenance` is `estimated` |
| `apps` | The apps to close, as in `suggestions` below |
| `projected_verdict` | `fits`, `tight` or `doesnt_fit` after closing `apps` (estimated); `null` when not computed |
| `projected_available_bytes` | Available memory expected after closing `apps` (estimated) |
| `why` | The "Why?" paragraph |

### `canfit free --json`

| Field | Meaning |
|---|---|
| `schema` | Always `"canfit.free"` |
| `schema_version` | `1`. Increases on any breaking change; new fields may be added without a bump |
| `canfit_version` | Version of canfit that wrote the document |
| `summary` | The one-sentence answer |
| `why` | The "Why?" paragraph |
| `pressure_level` | `calm`, `strained` or `critical` |
| `memory_available_bytes`, `memory_total_bytes` | Measured memory |
| `suggestions` | Every app canfit may suggest, safe first then by memory: `name`, `class` (`safe` or `caution`), `reason`, `memory_bytes`, `memory_provenance` (`measured`), `process_count`, `unreadable_count`, `pids`, `manual_commands` |
| `manual_commands` | How to close the app yourself, safest first: `command` (ready to copy), `note`, `forced` (`true` for last-resort commands that lose unsaved work). canfit never runs them; see [Process safety](process-safety.md#closing-it-yourself) |
| `not_suggested` | Up to five of the largest never-class apps: `name`, `memory_bytes`, `memory_provenance`, `reason` |

`--json` never prompts and never closes anything.

```bash
canfit free --json | jq -r '.suggestions[] | "\(.class)\t\(.name)\t\(.memory_bytes)"'
```

## History

While the live monitor is open, canfit records what happens to memory in a small local database:

- **pressure events** — every stretch of *strained* or *critical* pressure, with when it started,
  how long it lasted, its worst level and score, the reason, and the three apps using the most
  memory at the worst moment. An event ends once pressure has been calm for 30 seconds, so a short
  dip does not split it;
- a **compact snapshot** every 30 seconds (memory, swap, compressed memory, CPU, pressure);
- the memory of the larger apps (100 MB or more, up to eight per snapshot) and a per-app profile
  of typical and peak use, which `canfit APP` uses as its estimate (see
  [Apps](apps.md#estimates-from-history)) and `canfit history --apps` lists.

Nothing is recorded while the monitor is closed: there is no background service. Press `h` in the
monitor for the last 24 hours, or run `canfit history` (an example):

```text
✗ 2 pressure events in the last 24 hours. The worst was critical for 4 min, today 14:02, when Google Chrome was using 6.0 GB.

╭─ Pressure events in the last 24 hours ─────────────────────────────────────────╮
│  yesterday 22:10  12 min  ▲ strained  Google Chrome 5.1 GB, Docker 3.0 GB      │
│  today 14:02       4 min  ✗ critical  Google Chrome 6.0 GB, Docker 3.0 GB      │
╰────────────────────────────────────────────────────────────────────────────────╯
  canfit watched for 3 h of this period: history is recorded only while the monitor is open.
  Levels come from canfit's pressure index (estimated); app sizes are the memory each app used at
  the worst moment (measured).
  History file: /Users/you/Library/Application Support/canfit/history.sqlite3
```

`--since` widens or narrows the period (`canfit history --since 7d`, `--since 2026-11-09`). An
event still going on says "so far"; one whose monitor was closed (or killed) before pressure calmed
down shows its known length followed by `+`. canfit reports how long it was actually watching, so
a quiet history and an unwatched one look different.

### Where history is stored

| OS | Database |
|---|---|
| macOS | `~/Library/Application Support/canfit/history.sqlite3` |
| Windows | `%LOCALAPPDATA%\canfit\data\history.sqlite3` |
| Linux | `$XDG_DATA_HOME/canfit/history.sqlite3` (default `~/.local/share/canfit/history.sqlite3`) |

The monitor writes to it while it runs, and `canfit watch` adds one row per watched run (the name,
the program's file name — never its arguments — and the measurements). `CANFIT_DATA_DIR` points
canfit at another folder. The database holds app names, memory sizes and times; it never leaves
the machine. To delete your history, quit the monitor and delete the file.

### Turning history off

For **one run**, add `--no-history` to any command, or set the environment variable
`CANFIT_NO_HISTORY=1` (handy in a shell alias or a script):

```bash
canfit --no-history                      # monitor without recording; the footer says "history off"
canfit --no-history watch -- ./bench.sh  # measure without saving (same as watch --no-save)
CANFIT_NO_HISTORY=1 canfit Slack         # an app check that can't write anything
alias canfit='canfit --no-history'       # never record from this shell
```

That run writes **nothing** to the history database — no snapshots, pressure events, app samples,
app profiles or watch runs — and does not create the database file or its folder if they don't
exist. Existing history is not deleted and is still **read** where a command needs it: app
estimates from history (`canfit Slack`), `canfit history`, `canfit history --apps`, and the
monitor's `h` panel, which shows past events read-only. An older database is read as it is, not
upgraded (upgrading is a write); a later run with recording on upgrades it.

`CANFIT_NO_HISTORY` turns recording off for any value except empty, `0`, `false`, `no` and `off`.
The flag and the variable only ever turn recording **off**: neither turns it on when
`config.toml` says `[history] enabled = false`. To turn history off **for good**, use that setting
(see [Settings](#settings)).

`--json` documents that carry history metadata (`canfit history --json`, `canfit history --apps
--json` and `watch --json-out`) have a `history` field: `"off"` when recording was disabled for the
run (by `--no-history`, `CANFIT_NO_HISTORY`, `watch --no-save` or `enabled = false`), otherwise
`"on"`.

### Settings

In canfit's `config.toml` (see [Process safety](process-safety.md) for where it lives):

```toml
[history]
enabled = true                # false: the monitor records nothing and watch saves no run
snapshot_interval_secs = 30   # 5–3600
retention_days = 7            # 1–365: older records are deleted
max_size_mb = 64              # 1–4096: above this, the oldest records go first
```

Retention runs when the monitor starts and every 10 minutes while it runs; deleted space is given
back to the file system. A week of history at the defaults takes a few megabytes. Watched runs and
per-app totals are kept 90 days.

The database file is versioned. A newer canfit upgrades it in place, keeping your history. canfit
never deletes or recreates a database it can't use: if the file was written by a newer canfit, or
is not a canfit database, `canfit history` exits with code `3` and says so, and the monitor runs
without recording and says why in its footer and history panel.

### Troubleshooting the history database

If canfit says it "can't use its history database", the message names the file, the schema
version the file has and the version this canfit expects. canfit never deletes, repairs or
rewrites the file. To start a fresh history, quit every running canfit (the monitor, `watch`) and
move the file aside — keep it if you want to look at it later:

```bash
# macOS
mv ~/Library/Application\ Support/canfit/history.sqlite3 ~/canfit-history-old.sqlite3
# Linux
mv ~/.local/share/canfit/history.sqlite3 ~/canfit-history-old.sqlite3
# Windows (PowerShell)
Move-Item "$env:LOCALAPPDATA\canfit\data\history.sqlite3" "$HOME\canfit-history-old.sqlite3"
```

The next run with recording on creates a new database. If the file was written by a **newer**
canfit, update canfit instead (`canfit update`) to keep your history. Until then, `--no-history`
keeps canfit from trying to write to it.

### `canfit history --json`

| Field | Meaning |
|---|---|
| `schema` | Always `"canfit.history"` |
| `schema_version` | `1`. Increases on any breaking change to this layout |
| `canfit_version` | Version of canfit that wrote the document |
| `summary` | The opening sentence |
| `since_unix_ms`, `until_unix_ms` | The period covered |
| `history` | `"off"` when recording was disabled for this run ([Turning history off](#turning-history-off)), otherwise `"on"` |
| `database` | Path of the history database, or `null` when there is none yet |
| `recorded_ms` | How long a monitor was recording during the period |
| `events` | Oldest first: `start_unix_ms`, `end_unix_ms` (`null` while `ongoing`), `duration_ms`, `ongoing`, `cut_short` (the monitor stopped before it ended), `level` (`strained` or `critical`, the worst reached), `peak_score`, `peak_unix_ms`, `level_provenance` (always `estimated`), `reason` (`signal`, `message`), `culprits` (`name`, `memory_bytes`, `provenance`: `measured`) |

```bash
canfit history --since 7d --json | jq -r '.events[] | "\(.level)\t\(.duration_ms)\t\(.culprits[0].name)"'
```

## Watch

`canfit watch -- COMMAND [ARGS...]` starts the command and measures the real memory of it and of
everything it starts — children, grandchildren, and processes left behind when their parent
exits — until the command ends (macOS, Windows and Linux). Then it prints a summary, remembers
the run, and exits with the command's own exit code.

```bash
canfit watch -- llama-server -m qwen2.5-coder-3b.gguf -c 8192 --port 8080
canfit watch --name "my agent" -- python agent.py
canfit qwen2.5   # later: uses the measured peak ("measured by watch on 2026-11-10")
```

**It behaves like the command run directly.** The command keeps canfit's terminal, input, output
and exit code; canfit writes only to **stderr**, and only whole lines starting with `[canfit]`. It
never redraws a status line, so it can't garble the command's output or a progress bar:

- one line when it starts;
- a warning when memory pressure becomes strained or critical — the level, the main reason, the
  command's memory and the two largest other apps — once per level, and one line when pressure has
  been calm again for 30 seconds. canfit never closes or signals anything because of it: run
  `canfit free` in another terminal to see what is safe to close;
- a "still watching" line every minute (time, current and peak memory, processes, pressure);
- the summary when the command ends.

`--quiet` (`-q`) leaves only the summary. Colours follow stderr (off when it is not a terminal, or
with `NO_COLOR`).

**Ctrl-C** goes to the command as usual — the terminal delivers it to both — and canfit waits for
the command to finish, then prints the summary. A Ctrl-C (SIGINT) or SIGQUIT sent to canfit alone
with `kill` while it is not the terminal's foreground job (a script, a background job) is
forwarded to the command; SIGTERM and SIGHUP sent to canfit are always forwarded. On Windows the
console delivers Ctrl-C / Ctrl-Break to the command itself and canfit waits.

**The summary** starts with one sentence ("llama-server ran for 12 min and used at most 2.7 GB
(measured); memory pressure stayed calm."), then a card:

| Row | Meaning |
|---|---|
| Peak | Largest sampled memory of the whole process tree (measured): physical footprint on macOS, PSS on Linux, private working set on Windows; when it happened and how many processes |
| Typical | Median of the samples |
| Ran for | Duration, sampling interval, and how the command ended (exit code, Ctrl-C, or a signal) |
| Pressure | The highest pressure level and score (estimated) and when; the main reason follows as a note |
| Model file, canfit estimate, with margin | When the command loads a GGUF model canfit can identify (see below) |

and notes: whether the run was saved, processes the command left running (canfit leaves them
alone), processes whose memory could not be read.

**Sampling.** Every second by default (`--interval 0.5`–`60`). A spike shorter than the interval
can be missed: on the Mac a llama-server load measured 381 MB where macOS's lifetime peak said
382 MB. On the helper tree used by the tests (three processes holding 100 MiB each), the measured
peak is within 5 % below and 64 MiB above what they hold (303 MiB for 300 MiB on the Mac).

**Models.** When the command's arguments name a GGUF file — llama.cpp's `-m` / `--model`, or any
argument that is an existing `.gguf` file — the summary puts canfit's estimate for it next to the
measured peak, using the command's `-c` / `--ctx-size` (`0` = the model's own; canfit's default
8,192 when absent, with a note) and `-ctk` / `--cache-type-k`. llama.cpp maps the weights from the
file (mmap) unless `--no-mmap` or `--load-mode none|mlock|dio` is given. The physical footprint
(macOS) and the private working set (Windows) leave mapped file pages out, so there canfit shows
the weights as a separate **Model file** row (read from the file) and adds them to the remembered
need; Linux PSS already counts them. Ollama model names, LM Studio and `-hf` downloads are not
identified.

**Remembered.** A run is saved in the history database under `--name NAME`, or the program's file
name (`llama-server`; `.exe` dropped), so `canfit NAME` — alone or with other targets — answers
from it: the need is the run's peak (plus mapped model weights, see above), labelled **measured by
watch on DATE**, confidence **high** for a run of 30 s and 10 samples or more, else **medium**. A
watched run comes before the monitor's samples of an app with the same name, and the most recent
usable run wins; a run that failed within 30 seconds (a non-zero exit or a signal, without Ctrl-C) is not saved,
nor is one with no measurement. Only the program's file name is stored, never its arguments (they
may hold keys or paths). With `[history] enabled = false` nothing is saved.

**Not saving one run.** `canfit watch --no-save -- COMMAND` runs and measures the command exactly
as usual — same output, warnings, summary, `--json-out` file and exit code — but does not store
the run, so later fit checks don't learn from it. The summary's last line is "Not saved
(--no-save)." It is the same as `canfit --no-history watch -- COMMAND` (that summary ends with
"Not saved (--no-history).") and as `CANFIT_NO_HISTORY=1`.
`canfit history --apps` lists watched apps with "measured by watch".

**Exit codes.** canfit exits with the command's code. When a signal ended the command (Unix),
canfit exits with 128 + the signal number, as shells do (`130` after Ctrl-C, `137` after SIGKILL).
On Windows the command's 32-bit exit code is passed on unchanged (Ctrl-C usually ends a console
program with `0xC000013A`). Before the command starts: `127` when it can't be found, `126` when it
can't be run, `3` when canfit can't measure this OS (the command is not started), `64` for usage
errors (including `--json`: stdout belongs to the command).

### `--json-out FILE`

Writes the summary as a `canfit.watch` document (stdout belongs to the command, so `--json` is
refused):

| Field | Meaning |
|---|---|
| `schema` | Always `"canfit.watch"` |
| `schema_version` | `1`. Increases on any breaking change to this layout |
| `canfit_version` | Version of canfit that wrote the document |
| `summary` | The opening sentence |
| `history` | `"off"` when recording was disabled for this run (`--no-save`, `--no-history`, `CANFIT_NO_HISTORY`, `enabled = false`), otherwise `"on"` |
| `name`, `program`, `pid` | The name the run is remembered under, the program's file name, the command's process id |
| `started_unix_ms`, `duration_ms`, `interval_ms`, `samples` | When, how long, how often, how many samples |
| `memory` | Absent without a measurement. `kind` (`footprint`, `pss`, `rss`, `private_working_set`), `provenance` (`measured`), `peak_bytes`, `peak_offset_ms`, `typical_bytes` (median), `p95_bytes`, `last_bytes`, `max_processes`, `max_unreadable_processes` |
| `pressure` | `peak_level`, `peak_score` (estimated), `peak_offset_ms`, `peak_reason`, `warnings` (how many times pressure rose) |
| `model` | Only when a model was identified: `path`, `context_tokens`, `context_from_command`, `mmap`, `mapped_bytes` (weights left out of the measurement, when added), `estimate` (as in `canfit.fit`), `difference_percent` ((peak + mapped) vs the estimate without margin) |
| `ended` | `code` (`null` after a signal), `signal` (Unix), `interrupted` (canfit received Ctrl-C or a termination request), `canfit_exit_code` |
| `still_running` | Processes the command started that were still running when it ended |
| `remembered` | `saved`, `name`, `reason` (why not; `"Not saved (--no-save)."` and similar when recording was off for the run) |
| `notes` | The notes under the card |

## Models catalog

`canfit models DIR` (default: the current folder) lists every GGUF model in a folder and up to
four folders below it — the layout LM Studio and the Hugging Face cache use — with the memory each
one needs and whether it fits right now, smallest first:

```text
$ canfit models .cache/huggingface/hub
● 2 of 3 models in .cache/huggingface/hub fit right now; the largest that fits is bge-small-en-v1.5-f16, which needs about 364 MB of the 3.9 GB available.

╭─ Models in .cache/huggingface/hub ────────────────────────────────────────╮
│  stories260K            285 MB  ● fits, 3.7 GB to spare · F32 file 1 MB   │
│  bge-small-en-v1.5-f16  364 MB  ● fits, 3.6 GB to spare · F16 file 64 MB  │
│  Qwen3.5-4B-UD-Q4_K_XL  4.6 GB  ✗ 647 MB short · Q4_K_M file 2.8 GB       │
╰───────────────────────────────────────────────────────────────────────────╯
```

Real output on a 16 GB Mac; the vision projector in the same folder is reported under the card
as skipped.

Each model is checked on its own, with the same estimate and verdict as `canfit model.gguf`
(`--ctx`, `--cache-type` and `--margin` apply to all of them). Split models count once, labelled
with their number of parts. Models are named by file; when two files share a name, by their path
in the folder. Hidden folders and folder links are skipped; file links are followed. Vision
projectors (`mmproj-*.gguf`) are listed as skipped, since they load together with their language
model; unreadable or incomplete files are listed as skipped with the reason. Ollama's blobs have no
`.gguf` name and are not listed yet: check one with `canfit <blob path>`.

### `canfit models --json`

| Field | Meaning |
|---|---|
| `schema` | Always `"canfit.models"` |
| `schema_version` | `1`. Increases on any breaking change to this layout |
| `summary`, `directory` | The opening sentence and the folder listed |
| `memory_available_bytes`, `gpu_memory_limit_bytes`, `pressure_level` | What every model was compared with |
| `counts` | `fits`, `tight`, `doesnt_fit` |
| `models` | Smallest estimate first: `label`, `verdict`, `deciding_limit`, `checks`, plus every field of a model in the fit check's `models` (`name`, `path`, `parts`, `quantization`, `file_bytes`, `estimate`, …) |
| `skipped` | `path` and `error` of every file not listed |

## Snapshot

When stdin or stdout is not a terminal, `canfit` with no arguments takes one snapshot and prints a
summary sentence, a few supporting lines and the applications using the most memory (for example
`canfit | less` or `canfit > state.txt`):

```text
All calm: 4.2 GB of 16.0 GB memory is available.

  Memory    11.8 GB used of 16.0 GB, 4.2 GB available  [measured]
            apps 7.6 GB · wired 2.6 GB · compressed 1.6 GB · cached files 3.2 GB
  Swap      3.9 GB used of 5.0 GB, not growing (old swap is harmless)  [measured]
  CPU       22% busy across 10 cores, load 3.10  [measured]
  Pressure  ● calm, 0/100 (OS level: normal)  [estimated]
  GPU       up to 10.7 GB of memory usable by the GPU (macOS default: about 2/3 of memory up to 36 GB)  [estimated]

  Top apps by memory
  Google Chrome                       2.6 GB  26 processes
  Claude                              1.5 GB  16 processes
  ...
```

A snapshot takes two readings 0.25 s apart, so CPU usage and the swap trend are real measurements;
the whole command finishes in about 0.3 s and needs no admin rights. Sizes use binary units
labelled GB and MB, as in Activity Monitor.

It works on macOS, Windows and Linux. On Linux the snapshot adds a *Waiting* line — the share of
the last 10 seconds in which programs were stalled waiting for memory, CPU and I/O (the kernel's
pressure-stall information, PSI; shown when the kernel provides it) — and app memory is each
process's proportional set size (PSS: its private memory plus a fair share of shared pages). Other
users' processes, whose PSS Linux keeps private, show their resident size instead and are marked
"partly resident size". Kernel threads are not listed. On Windows the snapshot adds a *Committed* line — memory Windows has promised to programs
against its commit limit (RAM plus page file) — and swap is the page file. Windows has no load
average.

### `canfit --json`

`canfit --json` prints the same snapshot as one JSON document. All sizes are integers in bytes,
times are milliseconds since the Unix epoch, and enum values are `snake_case` strings.

| Field | Meaning |
|---|---|
| `schema` | Always `"canfit.snapshot"` |
| `schema_version` | `1`. Increases on any breaking change to this layout; new fields may be added without a bump |
| `canfit_version` | Version of canfit that wrote the document |
| `timestamp_unix_ms` | When the snapshot was taken |
| `platform` | `os`, `os_version`, hardware `arch`, `unified_memory` (true on Apple Silicon) |
| `memory` | `total_bytes`, `available_bytes`, `used_bytes`, `free_bytes`; optional detail `wired_bytes` (Windows: non-paged kernel pool), `app_bytes` (Linux: `AnonPages`), `cached_files_bytes` (Linux: `free`'s buff/cache), `compressed_bytes` (physical; on Windows only when readable, usually as administrator; on Linux zswap plus zram), `compressed_original_bytes` (data held by the compressor before compression, including data swapped out), `commit_bytes` and `commit_limit_bytes` (Windows commit charge and limit); `provenance` |
| `swap` | `total_bytes`, `used_bytes`, `free_bytes`, `swapped_out_total_bytes` (since boot; `null` on Windows), `encrypted`, `provenance`. On Windows: the page files |
| `swap_trend` | `growing`, `used_change_bytes_per_sec`, `swap_out_bytes_per_sec`, `window_secs`, `provenance`; `null` without two readings |
| `cpu` | `logical_cores`, `physical_cores`, `brand`, `usage_percent` (0–100), `load_average` (1, 5, 15 min), `provenance` |
| `os_pressure` | The OS's own level: `normal`, `warning`, `critical`, or `null`. Windows reports only `normal` or `critical` (its low-memory notification) |
| `hard_faults_per_sec` | Windows: pages read back from disk per second since the previous reading (PDH `Pages Input/sec`); `null` elsewhere or on the first reading |
| `stall` | Linux PSI: `memory`, `cpu`, `io`, each `{ some, full }` with `avg10`, `avg60`, `avg300` (percent of time stalled) and `total_us` (`full` is `null` for CPU on kernels before 5.13); `source` (`/proc/pressure` or the cgroup v2 root); `provenance`. `null` on macOS, Windows and Linux without PSI |
| `pressure` | `score` (0–100), `level` (`calm`, `strained`, `critical`), `main_reason` (`signal`, `message`), per-signal `signals`, `provenance` (always `estimated`) — see [Pressure index](pressure-index.md) |
| `gpu_memory_limit` | On Apple Silicon: `bytes`, `provenance` (`read_from_config` or `estimated`), `source`; otherwise `null` |
| `app_groups` | Applications, largest first: `name`, `kind` (`app_bundle` — a macOS bundle, a Windows or Linux app with helpers, a Linux service, Flatpak or Snap — or `executable`), `bundle_path`, `memory_bytes`, `memory_kinds`, `process_count`, `unreadable_count`, `pids` |
| `processes` | Every process: `pid`, `ppid`, `name`, `exe`, `user`, `app_bundle`, `memory` (`bytes`, `kind`, `provenance`, or `null` when it can't be read without admin rights) |

`provenance` is one of `measured`, `read_from_config` or `estimated`. A process's memory `kind` is
`footprint` (macOS physical footprint, as in Activity Monitor), `rss` (resident size, a fallback),
`pss` (Linux) or `private_working_set` (Windows).

```bash
canfit --json | jq '.pressure.level, .memory.available_bytes'
```

## Update

`canfit update` answers how this copy of canfit is updated. It never runs on its own, and canfit
never checks for updates in the background.

- Installed with the **shell or PowerShell installer**: canfit says it will connect to github.com,
  asks `Update canfit now? [y/N]`, and only on `y` runs its companion updater `canfit-update`,
  which downloads the latest release and replaces canfit in the same folder. `--yes` (`-y`) skips
  the question; without a terminal and without `--yes` nothing happens and the exit code is `3`.
- Installed with **Homebrew, Scoop, the MSI, `cargo install` or a .deb/.rpm**: canfit prints the
  command that updates it (`brew upgrade canfit`, `scoop update canfit`, or
  `cargo install --git https://github.com/federicolaterra/canfit --tag <tag> --locked` with the
  latest release's tag) and changes nothing. It recognises the method from where its executable lives.
- Anything else (for example a binary you copied by hand): canfit says it can't tell and points to
  the latest release.

```text
$ canfit update
canfit 1.0.0 was installed with Homebrew, so Homebrew updates it: run `brew upgrade canfit`.
```

Exit codes: `0` when updated, declined or answered; `3` when the updater failed or could not ask.
See [Installation](installation.md#updating) for each method.

### `canfit update --json`

Reports, without updating or connecting:

```json
{
  "schema": "canfit.update",
  "schema_version": 1,
  "canfit_version": "1.0.0",
  "summary": "canfit 1.0.0 can update itself: the canfit installer set it up in /home/me/.local/bin.",
  "install_method": "installer",
  "executable": "/home/me/.local/bin/canfit",
  "self_update": true,
  "update_command": "canfit update --yes"
}
```

`install_method` is one of `installer`, `homebrew`, `scoop`, `msi`, `cargo`,
`system_package` or `unknown`; `update_command` is `null` when there is no single command (MSI,
Linux packages, unknown).
