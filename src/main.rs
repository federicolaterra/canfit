//! canfit — the `top` for people running AI locally.
//!
//! On macOS, Windows and Linux: `canfit` opens the live monitor (a one-shot snapshot when piped)
//! and records pressure events while it runs, `canfit model.gguf` checks whether a GGUF model
//! fits, `canfit models DIR` lists which models in a folder fit, `canfit free` / `--make-room`
//! say what to close and close it after confirmation, `canfit history` shows past pressure
//! events (text or `--json`), `canfit watch -- CMD` runs a command and measures its real memory,
//! and `canfit update` updates canfit or says how. See `docs/open-source/usage.md`.

mod advisor;
mod cli;
mod core;
mod estimate;
mod history;
mod platform;
mod ui;
mod watch;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
