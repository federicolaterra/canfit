# Installation

canfit is a single self-contained binary: no runtime, no Docker, no interpreter, no service. Pick
**one** method per machine; the one-line installer is the quickest.

| Platform | One-line install |
|---|---|
| macOS, Linux | `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/federicolaterra/canfit/releases/latest/download/canfit-installer.sh \| sh` |
| Windows | `powershell -ExecutionPolicy Bypass -c "irm https://github.com/federicolaterra/canfit/releases/latest/download/canfit-installer.ps1 \| iex"` |

Then check it works:

```bash
canfit --version
canfit --json
```

Every release ships these files for macOS (Apple Silicon and Intel), Windows x86_64 and Linux
x86_64 and ARM64. The Linux binaries are static musl builds with no shared libraries: they run on
any distribution — e.g. Ubuntu, Debian, Fedora, Arch Linux, Alpine. Every file has a SHA-256 checksum and a signed build provenance attestation (see
[Verify a download](#verify-a-download)).

| Method | macOS | Windows | Linux | Admin rights | Updated by |
|---|---|---|---|---|---|
| [Shell installer](#shell-installer-macos-and-linux) | ✓ | | ✓ | no | `canfit update` |
| [PowerShell installer](#powershell-installer) | | ✓ | | no | `canfit update` |
| [Homebrew](#homebrew-macos-and-linux) (tap in this repository) | ✓ | | ✓ | no | `brew upgrade` |
| [Scoop](#scoop) (manifest in this repository) | | ✓ | | no | `scoop update` |
| [MSI](#msi) | | ✓ | | **yes** (Program Files) | a newer MSI |
| [.deb](#deb-debian-ubuntu) | | | ✓ | **yes** (`sudo`) | a newer .deb |
| [.rpm](#rpm-fedora-rhel-opensuse) | | | ✓ | **yes** (`sudo`) | a newer .rpm |
| [Plain archive](#plain-archive-any-linux-distribution) | ✓ | ✓ | ✓ | no | a newer archive |
| [`cargo install --git`](#cargo-install) | ✓ | ✓ | ✓ | no | `cargo install --git` |
| [From source](#build-from-source) | ✓ | ✓ | ✓ | no | `git pull` + build |

`canfit update` tells you which command updates your copy. It updates by itself only a copy that the
shell or PowerShell installer set up (see [Updating](#updating)).

## macOS

### Shell installer (macOS and Linux)

```bash
# Install the latest release (or replace "latest/download" with "download/v1.0.0" to pin one)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/federicolaterra/canfit/releases/latest/download/canfit-installer.sh | sh

# Verify
canfit --version

# Update (asks before connecting; see "Updating")
canfit update

# Uninstall
rm -f ~/.local/bin/canfit ~/.local/bin/canfit-update ~/.config/canfit/canfit-receipt.json
```

The installer detects your CPU, downloads the matching archive from GitHub, verifies its checksum
and puts `canfit` and its small updater `canfit-update` in `$XDG_BIN_HOME`, else
`$XDG_DATA_HOME/../bin`, else `~/.local/bin` (adjust the `rm` above if you set one of those). It
writes an install receipt to `~/.config/canfit/canfit-receipt.json` (used only by
`canfit update`) and, if that folder is not on your `PATH`, adds a line that sources
`~/.local/bin/env` to your shell profile. On uninstall, remove that line too if no other tool uses
`~/.local/bin`. No `sudo`.

Options: `CANFIT_NO_MODIFY_PATH=1` leaves your shell profile alone; `CANFIT_INSTALL_DIR=/some/dir`
installs elsewhere. To read the script before running it:
`curl -LsSf …/canfit-installer.sh -o canfit-installer.sh`, then `sh canfit-installer.sh`.

### Homebrew (macOS and Linux)

```bash
# Install: add the tap (the canfit repository itself), then install from it
brew tap federicolaterra/canfit https://github.com/federicolaterra/canfit
brew install federicolaterra/canfit/canfit

# Verify
canfit --version

# Update
brew update && brew upgrade canfit

# Uninstall
brew uninstall canfit
brew untap federicolaterra/canfit    # optional: removes the tap
```

The formula is [`Formula/canfit.rb`](../../Formula/canfit.rb) in the canfit repository and installs
the release's own binaries, checked against their SHA-256. Because the repository is not named
`homebrew-…`, `brew tap` needs its URL as the second argument; without the `brew tap` line,
`brew install federicolaterra/canfit/canfit` would look for a repository called `homebrew-canfit`
that does not exist. Installing by the full name `federicolaterra/canfit/canfit` also marks the
formula as trusted, which recent Homebrew versions require for formulae outside the official taps.
No `sudo`.

A new canfit release reaches Homebrew once its formula update is merged into the repository,
usually shortly after the release; until then `brew upgrade` keeps the previous version.

### `cargo install`

See [`cargo install`](#cargo-install) below; it works the same on every platform.

## Windows

canfit runs on Windows 10 and 11 (x86_64) with no Visual C++ Redistributable and no admin rights.

### PowerShell installer

```powershell
# Install the latest release
powershell -ExecutionPolicy Bypass -c "irm https://github.com/federicolaterra/canfit/releases/latest/download/canfit-installer.ps1 | iex"

# Verify (open a new terminal first, so the updated PATH is picked up)
canfit --version

# Update (asks before connecting; see "Updating")
canfit update

# Uninstall
Remove-Item "$env:USERPROFILE\.local\bin\canfit.exe", "$env:USERPROFILE\.local\bin\canfit-update.exe"
Remove-Item "$env:LOCALAPPDATA\canfit\canfit-receipt.json"
```

The installer puts `canfit.exe` and its updater `canfit-update.exe` in
`%USERPROFILE%\.local\bin` (or `%XDG_BIN_HOME%`) and adds that folder to your **user** `PATH`. On
uninstall you can remove it from *Settings → System → About → Advanced system settings →
Environment Variables* if nothing else lives there. No admin rights.

### Scoop

```powershell
# Install: straight from the manifest in the canfit repository (no bucket to add)
scoop install https://raw.githubusercontent.com/federicolaterra/canfit/master/packaging/scoop/canfit.json

# Verify
canfit --version

# Update (Scoop downloads the manifest from the same URL again)
scoop update canfit

# Uninstall
scoop uninstall canfit
```

The manifest is [`packaging/scoop/canfit.json`](../../packaging/scoop/canfit.json) in the canfit
repository; Scoop remembers its URL, so `scoop update` follows new releases. It installs the
release's zip, checked against its SHA-256, per user in `~\scoop`. No admin rights. As with
Homebrew, a new release reaches Scoop once the manifest update is merged.

### MSI

Download `canfit-x86_64-pc-windows-msvc.msi` from the
[latest release](https://github.com/federicolaterra/canfit/releases/latest) and run it, or:

```powershell
# Install (asks for administrator permission: the MSI installs into Program Files)
msiexec /i canfit-x86_64-pc-windows-msvc.msi

# Verify (in a new terminal)
canfit --version

# Update: run the newer MSI the same way; it replaces the installed version.

# Uninstall: Settings → Apps → Installed apps → canfit → Uninstall, or
msiexec /x canfit-x86_64-pc-windows-msvc.msi
```

The MSI installs for all users into `C:\Program Files\canfit\bin` and adds it to the system `PATH`,
so it **needs admin rights**. Without them, use the PowerShell installer or Scoop instead.

### `cargo install`

See [`cargo install`](#cargo-install) below.

## Linux

Every Linux method installs the same static musl binary (x86_64 or ARM64), which runs on any
distribution — e.g. Ubuntu, Debian, Fedora, Arch Linux, Alpine. canfit needs no root to run.
Besides `.deb` and `.rpm` there are no distribution-specific packages: on Arch Linux, Alpine and
other distributions use the shell installer, Homebrew or the plain archive.

### Shell installer

Same as [on macOS](#shell-installer-macos-and-linux):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/federicolaterra/canfit/releases/latest/download/canfit-installer.sh | sh
canfit --version
canfit update
rm -f ~/.local/bin/canfit ~/.local/bin/canfit-update ~/.config/canfit/canfit-receipt.json   # uninstall
```

### Homebrew on Linux

Same as [on macOS](#homebrew-macos-and-linux); it installs the static musl binary (x86_64 or
ARM64):

```bash
brew tap federicolaterra/canfit https://github.com/federicolaterra/canfit
brew install federicolaterra/canfit/canfit
```

### .deb (Debian, Ubuntu)

```bash
# Install (pick amd64 or arm64; replace 1.0.0 with the latest version)
curl -LO https://github.com/federicolaterra/canfit/releases/download/v1.0.0/canfit_1.0.0-1_amd64.deb
sudo apt install ./canfit_1.0.0-1_amd64.deb

# Verify
canfit --version

# Update: download the newer .deb and install it the same way.

# Uninstall
sudo apt remove canfit
```

There is no apt repository yet, so `apt upgrade` does not update canfit. Installs
`/usr/bin/canfit`; needs `sudo`.

### .rpm (Fedora, RHEL, openSUSE)

```bash
# Install (pick x86_64 or aarch64; replace 1.0.0 with the latest version)
curl -LO https://github.com/federicolaterra/canfit/releases/download/v1.0.0/canfit-1.0.0-1.x86_64.rpm
sudo dnf install ./canfit-1.0.0-1.x86_64.rpm      # openSUSE: sudo zypper install ./canfit-…rpm

# Verify
canfit --version

# Update: download the newer .rpm and install it the same way (sudo dnf upgrade ./canfit-…rpm).

# Uninstall
sudo dnf remove canfit
```

There is no rpm repository yet. Installs `/usr/bin/canfit`; needs `sudo`.

### Plain archive (any Linux distribution)

```bash
# Install (pick x86_64 or aarch64): download, verify, put the binary on your PATH
curl -LO https://github.com/federicolaterra/canfit/releases/latest/download/canfit-x86_64-unknown-linux-musl.tar.xz
curl -LO https://github.com/federicolaterra/canfit/releases/latest/download/canfit-x86_64-unknown-linux-musl.tar.xz.sha256
sha256sum -c canfit-x86_64-unknown-linux-musl.tar.xz.sha256
tar -xJf canfit-x86_64-unknown-linux-musl.tar.xz
install -Dm755 canfit-x86_64-unknown-linux-musl/canfit ~/.local/bin/canfit

# Verify
canfit --version

# Update: repeat the steps above with the newer archive.

# Uninstall
rm ~/.local/bin/canfit
```

The archive holds the binary, `README.md`, `LICENSE` and `THIRD_PARTY_NOTICES.md`. The same works on
macOS (`canfit-aarch64-apple-darwin.tar.xz` or `canfit-x86_64-apple-darwin.tar.xz`, checked with
`shasum -a 256 -c`) and on Windows (`canfit-x86_64-pc-windows-msvc.zip`). No root needed.

## `cargo install`

For Rust developers, on every platform. cargo builds canfit from a release tag of the repository:

```bash
# Install (Rust 1.99 or newer, and a C compiler: canfit compiles SQLite's C source)
cargo install --git https://github.com/federicolaterra/canfit --tag v1.0.0 --locked

# Verify
canfit --version

# Update: the same command with the latest release's tag replaces the installed copy
cargo install --git https://github.com/federicolaterra/canfit --tag v1.0.1 --locked

# Uninstall
cargo uninstall canfit
```

Use the tag of the [latest release](https://github.com/federicolaterra/canfit/releases/latest)
(`v1.0.1` above is only an example); without `--tag`, cargo builds the newest commit on `master`.
The C compiler is the one Rust already uses: Xcode Command Line Tools on macOS
(`xcode-select --install`), `build-essential` or `gcc` on Linux, the Visual Studio Build Tools on
Windows. Installs to `~/.cargo/bin`. No admin rights.

## Build from source

Requirements: Rust stable via [rustup](https://rustup.rs/), Git and a C compiler (as for
`cargo install`). The repository's `rust-toolchain.toml` selects the toolchain automatically.

```bash
git clone https://github.com/federicolaterra/canfit.git
cd canfit
cargo build --release --locked
./target/release/canfit --version
```

On Windows, `target\release\canfit.exe` needs no Visual C++ Redistributable: the C runtime is
linked statically through `.cargo/config.toml`. Copy it anywhere and run it.

On Linux, build the fully static binary with musl (it runs on any distribution, glibc or musl):

```bash
sudo apt install musl-tools                     # musl-gcc, for SQLite's C source
rustup target add x86_64-unknown-linux-musl    # or aarch64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/canfit   # "static-pie linked"
```

To update, `git pull` and build again; to uninstall, delete the folder. See
[Development](development.md) for checks and `make` targets.

## Updating

`canfit update` never runs on its own and canfit never checks for updates in the background.

- **Shell or PowerShell installer:** `canfit update` explains that it will connect to github.com,
  asks for confirmation, then hands over to `canfit-update`, which downloads and installs the
  latest release in the same folder. `canfit update --yes` skips the question (for scripts). On
  Windows, canfit exits so its own file can be replaced and the updater finishes in the same
  window.
- **Any other method:** `canfit update` recognises Homebrew, Scoop, the MSI, `cargo install` and
  Linux packages from where canfit is installed, and prints the command to run instead
  (`brew upgrade canfit`, `scoop update canfit`, or
  `cargo install --git https://github.com/federicolaterra/canfit --tag <tag> --locked` with the
  latest release's tag).
  It changes nothing and connects to nothing.
- `canfit update --json` prints how this copy is installed and updated (`canfit.update` schema 1)
  without updating or connecting.

| Exit code | Meaning |
|---|---|
| `0` | Updated, already up to date, you declined, or canfit printed the command to run |
| `3` | The updater failed, or no terminal to confirm and no `--yes` |

## Verify a download

Every release file has a `.sha256` next to it, and `sha256.sum` lists them all:

```bash
shasum -a 256 -c canfit-aarch64-apple-darwin.tar.xz.sha256      # macOS
sha256sum -c canfit-x86_64-unknown-linux-musl.tar.xz.sha256     # Linux
```

```powershell
(Get-FileHash canfit-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash   # Windows: compare with the .sha256 file
```

Each file also has a build provenance attestation made by GitHub Actions. With the
[GitHub CLI](https://cli.github.com/):

```bash
gh attestation verify canfit-aarch64-apple-darwin.tar.xz --repo federicolaterra/canfit
```

The installers, Homebrew and Scoop check the checksums for you.

## Unsigned binaries: macOS Gatekeeper and Windows SmartScreen

canfit 1.0 is **not code-signed or notarised** (Apple Developer ID and Windows code signing are
planned). What that means in practice:

- **macOS.** The shell installer, Homebrew and `cargo install` don't mark files as downloaded, so
  canfit runs with no prompt. If you download an archive **with a browser** and macOS says canfit
  "cannot be opened because the developer cannot be verified", remove the quarantine flag from the
  file you extracted, after verifying its checksum:

  ```bash
  xattr -d com.apple.quarantine ./canfit
  ```

- **Windows.** The PowerShell installer and Scoop are not affected. If you download the MSI
  or zip **with a browser**, SmartScreen may show "Windows protected your PC": choose **More info →
  Run anyway**, or unblock the file first, after verifying its checksum:

  ```powershell
  Unblock-File .\canfit-x86_64-pc-windows-msvc.msi
  ```

## Removing canfit's data

Uninstalling removes the program, not what it recorded. canfit writes only its pressure history,
and only while the live monitor runs; it reads an optional `config.toml`. To delete them too:

| OS | History (`history.sqlite3`) | Config (`config.toml`) |
|---|---|---|
| macOS | `~/Library/Application Support/canfit/` | `~/Library/Application Support/canfit/` |
| Windows | `%LOCALAPPDATA%\canfit\data\` | `%APPDATA%\canfit\config\` |
| Linux | `~/.local/share/canfit/` (`$XDG_DATA_HOME`) | `~/.config/canfit/` (`$XDG_CONFIG_HOME`) |

`$CANFIT_DATA_DIR` and `$CANFIT_CONFIG` override these if you set them.

## Not available yet

More package channels (for example Homebrew core, Snap, Flatpak, Nix, MacPorts) may come after
1.0. Until then, the shell installer, Homebrew or the plain static binary works on every Linux
distribution, including Arch Linux and NixOS (as a downloaded binary).
