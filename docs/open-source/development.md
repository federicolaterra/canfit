# Development

## Prerequisites

- Rust via [rustup](https://rustup.rs/). `rust-toolchain.toml` pins the toolchain to the minimum
  supported Rust version declared in `Cargo.toml` (`rust-version`), with `rustfmt` and `clippy`;
  bump both together.
- `make` (macOS and Linux). On Windows, run the cargo commands shown below directly.

## Everyday commands

| Command | Does |
|---|---|
| `make check` | Format check, clippy with warnings denied, tests — the same gates as CI |
| `make fmt` | Format all sources |
| `make build` | Optimized binary in `target/release/canfit` |
| `make run ARGS="model.gguf --json"` | Run a debug build with arguments |

`make check` runs:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

`--locked` requires `Cargo.lock` to be up to date; after changing dependencies, run `cargo build`
once and commit the updated lockfile.

### Render snapshots

The live monitor is tested by drawing it into ratatui's `TestBackend` at fixed sizes (80×24 and
larger) from fake snapshots and comparing the text with [`insta`](https://insta.rs) snapshots in
`src/ui/snapshots/`. After an intended UI change, a failing test writes a `.snap.new` file next to
the old one; review and accept it with `cargo insta review` (from `cargo install cargo-insta`), or
rerun with `INSTA_UPDATE=always cargo test ui::` and inspect the diff before committing. The text
captures in `docs/assets/` are copied from these snapshots.

## Continuous integration

GitHub Actions runs the three gates on macOS, Windows and Linux for every push and pull request
(`.github/workflows/ci.yml`). On Windows it also builds the release `canfit.exe`, fails if the
binary depends on the Visual C++ runtime, runs `canfit --json`, `canfit` and `canfit free`, and
keeps the binary as a downloadable workflow artifact (`canfit-windows-x86_64`) for 14 days. The
release workflows are described under [Releases](#releases).

A second job builds the static musl binaries for Linux x86_64 (`ubuntu-latest`) and ARM64
(`ubuntu-24.04-arm`): it runs the whole test suite against musl, fails if `file` does not report a
static binary or `ldd` lists any library, runs `canfit --json`, `canfit` and `canfit free`, and
keeps `canfit-linux-x86_64` and `canfit-linux-aarch64` as artifacts for 14 days.

On Windows, CI sets `RUSTFLAGS` to `-D warnings -C target-feature=+crt-static`: a `RUSTFLAGS`
variable replaces the static C runtime flag in `.cargo/config.toml`, so keep the flag when you
set your own.

From macOS you can type-check and lint the Windows code without a Windows machine:

```bash
rustup target add x86_64-pc-windows-msvc
cargo clippy --target x86_64-pc-windows-msvc --all-targets --all-features -- -D warnings
```

Building, linking and running the tests still need Windows.

Linux code can be linted the same way (`--target x86_64-unknown-linux-gnu`), and the static musl
binaries link with the Rust toolchain's own `rust-lld`:

```bash
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
```

The history database compiles SQLite's C source into canfit (`rusqlite` with `bundled`), so every
cross build and cross lint above also needs a C compiler and C headers for the target; Apple's
`clang` alone is not enough. Without one, the `libsqlite3-sys` build script fails with
"failed to find tool". Point `CC_<target>` (and `AR_<target>`) at a cross compiler — for example
`zig cc -target x86_64-linux-musl`, or `clang-cl` with the Windows SDK headers from `xwin` — or
rely on CI, which builds every target natively (the musl jobs install `musl-tools` and set
`CC_<target>=musl-gcc`). Debug and test builds compile SQLite with optimisations
(`[profile.dev.package.libsqlite3-sys]`) so the history tests stay fast.

The `/proc` parsers and the Linux grouping are tested on every OS against fixtures; live readings
and closing need a Linux machine or VM.

App config readers (`src/estimate/apps/readers/`) are tested on every OS against fixture home
folders in `tests/fixtures/apps/<case>/` (`macos`, `macos-new`, `windows`, `windows-wsl-default`,
`windows-hyperv`, `linux`, `empty`, `broken`), each laid out like that OS's home folder; an `Env`
rooted at the fixture applies the OS's folder layout. To add a profile, add its block to
`src/estimate/apps/profiles.toml`, a settings file to the matching fixture home, and a reader test.
Check the real app's file location and keys on an installed copy and say which version you
checked.

## Releases

Releases are built by [cargo-dist](https://github.com/axodotdev/cargo-dist) 0.34. Its settings live
in `dist-workspace.toml`; `dist generate` writes `.github/workflows/release.yml` and
`wix/main.wxs` from them. Never edit those two files by hand: change `dist-workspace.toml` and run
`dist generate`, and the release workflow refuses to run when they are stale.

```bash
cargo install cargo-dist@0.34.0 --locked --root target/tools   # or any folder on your PATH
export PATH="$PWD/target/tools/bin:$PATH"
dist plan                                    # what a tag would build
dist build --artifacts=all --target aarch64-apple-darwin      # build this machine's artifacts into target/distrib/
```

Pushing a tag `vX.Y.Z` that matches `Cargo.toml` runs the release workflow: it builds the archives
for macOS (Apple Silicon, Intel), Windows x86_64 and Linux x86_64/ARM64 (static musl, built on
native runners with `musl-tools`), the MSI, the shell and PowerShell installers, the `canfit-update`
updater, a SHA-256 file for each and build provenance attestations, then creates the GitHub Release.
It needs no secret other than the workflow's own `GITHUB_TOKEN`. Pull requests only run
`dist plan`.

When it succeeds, `.github/workflows/linux-packages.yml` downloads the two musl archives from the
release, checks their checksums, wraps the binaries into `.deb` (`cargo deb --no-build`) and `.rpm`
(`cargo generate-rpm`) using the metadata in `Cargo.toml`, installs and runs the x86_64 `.deb` on
the runner, and attaches the packages and their `.sha256` files to the release. It can be rerun by
hand for a tag.

The Homebrew formula (`Formula/canfit.rb`) and the Scoop manifest (`packaging/scoop/canfit.json`)
live in this repository and are rendered by `packaging/render.sh` from `packaging/release.env`
(version and checksums) and the templates in `packaging/templates/`. When the release workflow
succeeds, `.github/workflows/package-manifests.yml` checks the released archives against their
`.sha256` files, records the release in `packaging/release.env`, re-renders both manifests and
opens a pull request against `master`; pull requests opened by GitHub Actions don't start CI, so
close and reopen it before merging. Until it is merged, Homebrew and Scoop install the previous
version. `tests/packaging.rs` (and `packaging/render.sh --check` in CI) fails if the manifests
are edited by hand. See [`packaging/README.md`](../../packaging/README.md).

## Testing on real systems

Anything that reads memory, pressure or processes must be verified on a real machine or a virtual
machine. Containers report the container's limits, not the host's, so they are not valid evidence.
Note the OS version, machine and RAM when you report measurements.

### Smoke test

`.github/workflows/smoke.yml` (Actions → Smoke → Run workflow, on demand) builds the release
binary for all five targets and runs [`.github/smoke/smoke.sh`](../../.github/smoke/smoke.sh)
against it: about a hundred end-to-end checks of exit codes, JSON output, fit checks on synthetic
GGUF files (`.github/smoke/mkgguf.py`), apps, `watch`, `history`, the history opt-outs
(`--no-history`, `CANFIT_NO_HISTORY=1` and `watch --no-save` must leave an empty data directory
empty) and `--version`. It never closes anything. Run it locally before a release:

```bash
cargo build --release
bash .github/smoke/smoke.sh target/release/canfit "$(mktemp -d)"
```

It complements CI and the manual VM test plan; it does not replace checks that need a real
desktop (monitor keys, closing real apps, installers).

## Code layout

See [Architecture](architecture.md). Inside canfit the `core` module shadows Rust's built-in `core`
crate: write `crate::core::...` for canfit's module and `::core::...` for the language crate.

## Contributing

See [`CONTRIBUTING.md`](../../.github/CONTRIBUTING.md) and add user-visible changes under
`Unreleased` in the [changelog](CHANGELOG.md).
