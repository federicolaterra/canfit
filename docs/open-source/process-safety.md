# Process safety

canfit suggests what to close to free memory and, only with your confirmation, closes it. It is
designed to be careful: it never closes anything on its own, never suggests system processes, and
never forces an app to quit without asking twice.

> [!NOTE]
> Suggestions and closing work on **macOS, Windows and Linux** (`canfit free`, `canfit model.gguf
> --make-room`, `x` in the live monitor).

## Classes

Every application (an app with all its helper processes, shown as one row) gets one class:

| Class | Examples | Behaviour |
|---|---|---|
| ● **Safe** | browsers (Chrome, Edge, Safari, Firefox, Arc…), chat and media apps (Slack, Discord, Spotify, Teams…) | Suggested first: they restore tabs and state when reopened |
| ▲ **Caution** | model servers (Ollama, LM Studio, `llama-server`), Docker and virtual machines, editors and IDEs, terminal apps, local databases, Safari's web content processes, Notepad and other Windows document apps, WebView2, **every app canfit does not know** | Suggested with a warning: you may lose unsaved work |
| ✗ **Never** | see below | Never suggested and never closed |

**Never** always includes:

- the kernel and macOS itself: `kernel_task`, `launchd`, `WindowServer`, `loginwindow`, Dock,
  Finder, SystemUIServer, Control Center, Spotlight and everything that ships with macOS under
  `/System/Library`, `/usr/bin`, `/usr/libexec`, `/usr/sbin`, `/bin` and `/sbin`;
- the kernel and Windows itself: `System`, `Registry`, `Memory Compression`, `smss.exe`,
  `csrss.exe`, `wininit.exe`, `services.exe`, `lsass.exe`, `winlogon.exe`, `dwm.exe`,
  `svchost.exe` and the other session processes, **`explorer.exe`** (the taskbar, Start and the
  desktop — Windows restarts it, so closing it frees little and disrupts your session), the memory
  of WSL and Docker's Linux VM (`vmmem`, `vmmemWSL`), and everything else under the Windows folder
  (normally `C:\Windows`) except Notepad, Paint, WordPad and Snipping Tool;
- the kernel and Linux itself: PID 1 (`systemd` or `init`), kernel threads (not even listed), D-Bus,
  `systemd-*` daemons, PipeWire and PulseAudio, the display server and compositor (`Xorg`,
  `Xwayland`, `gnome-shell`, KWin and `plasmashell`, Sway, Hyprland, and other window managers),
  session and login managers (`gnome-session`, GDM, SDDM, LightDM), desktop helpers (`gsd-*`,
  `gvfs*`, `xdg-*` portals, `at-spi*`, keyrings and agents), and everything under
  `/usr/lib/systemd`, `/usr/libexec`, `/usr/lib/xorg`, `/sbin` and `/usr/sbin` except terminal apps
  such as GNOME Terminal;
- processes owned by root or by another user (system daemons, Windows services running as
  `SYSTEM`, `LOCAL SERVICE` or `NETWORK SERVICE`, Linux service accounts such as `messagebus`,
  `gdm` or `ollama`) — canfit could not close them without admin
  rights anyway;
- **canfit itself and every process it runs inside**: its shell, the terminal app, or the editor
  or app hosting that terminal (for example VS Code's integrated terminal makes VS Code "never");
- shells and terminal sessions (`zsh`, `bash`, `fish`, `login`, `tmux`, `screen`, `ssh`…; on
  Windows `cmd.exe`, `powershell.exe`, `pwsh.exe`, `wsl.exe` and the console hosts `conhost.exe`
  and `OpenConsole.exe`, which run every console window);
- everything in your **protected list**.

An app takes the most restrictive class of its processes: if any helper of an app is "never", the
whole app is. Unknown apps are **caution**, never safe. When canfit can't tell who owns a process,
or has no rules for the operating system, it suggests nothing.

The rules are embedded in the binary (`src/advisor/rules/macos.toml`,
`src/advisor/rules/windows.toml` and `src/advisor/rules/linux.toml` in the source); they are listed in order and the first match
wins.

On Windows an app is the program you started plus the helpers it started from its own install
folder (for example Docker Desktop and `com.docker.backend.exe`); it is named after the program's
file without `.exe` ("chrome", "Docker Desktop"). A program started from a terminal stays its own
app.

On Linux an app is, in this order: every process of one **systemd service** (`ollama.service` is
"ollama", with its model runners), of one **Flatpak** (named by its app id, such as
"org.mozilla.firefox") or of one **Snap** ("firefox"); otherwise a program plus the helpers it
started that run the same executable or one from its own install folder (Chrome's zygote,
renderers and crash handler in `/opt/google/chrome` are one "chrome" row). Shared folders such as
`/usr/bin` never collect other programs, so a model server started from a shell stays its own app,
and processes with nothing to fold are grouped by name (every `bash` is one row).

## Protected list

Add apps or processes canfit must never suggest or close to `config.toml` in canfit's config
directory — on macOS `~/Library/Application Support/canfit/config.toml`, on Windows
`%APPDATA%\canfit\config\config.toml`, on Linux `$XDG_CONFIG_HOME/canfit/config.toml` (normally
`~/.config/canfit/config.toml`):

```toml
[processes]
protected = ["Slack", "llama-server", "/Applications/Docker.app"]
```

Entries match an app name, a process name or an app bundle path, ignoring case. Set the
`CANFIT_CONFIG` environment variable to use another file. If the file exists but can't be read or
contains an unknown setting under `[processes]`, canfit stops with exit code `3` instead of
guessing what you protected.

## What to close and how much it frees

The "what to close" list shows safe apps first, then caution apps, each sorted by memory, largest
first. Apps using less than 64 MB are left out. Each app's memory is **measured** (its physical
footprint on macOS, the value Activity Monitor shows; its private working set on Windows, as in
Task Manager's Details tab); what closing it frees is an **estimate** of about that much — less if
part of it is compressed or swapped.

To make room for a model, an app or several of them together (`--make-room` or
`canfit free TARGET…`), canfit picks the smallest set that frees what is missing. The targets
themselves — and shared processes that may hold their memory — are never suggested:

1. safe apps alone when they are enough — as few as possible, and the last one is the smallest
   that still covers the rest, so a huge app is not closed when a smaller one will do;
2. otherwise as few caution apps as possible, then as few safe apps as possible;
3. if even every suggestable app is not enough, it says so and closes nothing.

Closing apps can't help when the model doesn't fit in the memory the GPU may use (Apple Silicon's
GPU limit); canfit says so instead of suggesting anything.

## Closing it yourself

Every suggestion also says how to close it **by hand** — useful for background processes canfit
lists but you would rather stop your own way (programs started from a shell, model servers,
services). The commands are shown as text only, in the "What to close" and make-room cards
(`yourself:` — the safest one), in the closing prompt and the monitor's close dialog (all of them),
and in `--json` (`manual_commands`, each with `forced: true|false`). **canfit never runs them.**
Never-class apps get no command, since they are never suggested.

They are listed safest first:

| Kind of process | Commands |
|---|---|
| A tool with its own stop command | Docker Desktop: `docker desktop stop` (Docker Desktop 4.37 or later; else quit it from its menu). Ollama: `ollama ps`, then `ollama stop MODEL` — unloads a model and frees its memory without stopping the server. LM Studio: `lms unload --all` |
| A systemd service (Linux; from the process's cgroup) | `sudo systemctl stop UNIT.service`, or `systemctl --user stop UNIT.service` for a user service — and **no** `kill`: systemd would restart a killed process |
| A Homebrew service (macOS; a program under Homebrew's `Cellar` or `opt` started by launchd) | `brew services stop NAME`, if you started it that way, then the plain commands below |
| A macOS app | `osascript -e 'quit app "NAME"'` — the same as ⌘Q; the app may ask you to save |
| Any process, macOS and Linux | `kill PID…` (graceful, to the app's main processes), then `kill -9 PID…` (every process of the app) only as a last resort |
| Any process, Windows | `taskkill /PID PID…` (asks its windows to close), then `taskkill /F /T /PID PID…` (or `Stop-Process -Id PID -Force` in PowerShell) only as a last resort |

Forced commands lose unsaved work, like canfit's own forced close. Long pid lists are shortened in
the cards (`… (24 more processes; --json lists them all)`). Pids change when an app restarts, so
copy a command right before running it.

canfit can't yet tell that a macOS process belongs to a launchd agent other than a Homebrew
service, or that a Windows program is a service; Windows services run as `SYSTEM` or another
account and are never suggested anyway.

## Closing rules

- **Nothing is closed without your confirmation.** canfit never closes anything automatically.
  It offers to close only when it runs in a terminal (both input and output) and without
  `--json`; in scripts, pipes and `--json` mode it only reports suggestions.
- **Full list first.** Before asking, canfit lists **every process** that will be affected, with
  its pid, grouped by app, followed by the commands to close them yourself instead. Only an explicit `y` or `yes` counts as yes; anything else, including
  just pressing Enter, cancels.
- **The same in the live monitor.** `x` closes only the selected app, through the same steps: a
  dialog lists every process (scroll it with the arrows when it is long), only the `y` key is a
  yes — any other key, `Esc` or `Ctrl-C` cancels — and forcing asks again in a second dialog.
  `x` on a never-class app only shows why canfit won't close it.
- **Graceful first.** canfit asks each app's main process to quit (helpers are left to their
  app, which closes them itself) and waits up to 10 seconds: `SIGTERM` on macOS and Linux; on
  Windows it closes the app's windows (`WM_CLOSE`), as Task Manager's *End task* does, so apps
  can ask you to save.
- **Forced only after a second confirmation.** If anything is still running, canfit lists it again
  and asks whether to force it (`SIGKILL` on macOS and Linux, `TerminateProcess` on Windows).
  Forcing loses
  unsaved work in those processes.
- **Never-class apps are refused** even if they somehow reached the closing step.
- **No reused pids.** Right before signalling, canfit checks that each process still exists and
  still runs the same program; a pid that now belongs to something else is skipped.
- **No admin rights.** canfit only closes processes your user account could close anyway.

### Apps with unsaved work

An app with unsaved work may **block or ignore a graceful quit**: some apps catch `SIGTERM` and
wait, ask to save, or do nothing. canfit then reports them as still running; check their windows
and save, or confirm the forced close if you accept losing that work. Other apps quit immediately
on `SIGTERM` without asking to save — that is why editors, IDEs and unknown apps are **caution**.
A future release will ask macOS apps to quit the way the Dock's Quit command does.

On Windows a program **without a window** — a console program such as `llama-server.exe` running
in a terminal, or an app that only lives in the notification area — can't be asked to quit:
canfit says so ("it has no window to close") and the only way to end it is the forced close after
the second confirmation. Store apps hosted by `ApplicationFrameHost.exe` can't be closed from
canfit.
