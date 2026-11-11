//! ASCII stand-ins for the symbols canfit draws, for consoles whose font can't show them
//! (ADR-016). The legacy Windows console (conhost) with its default fonts lacks `✗`, partial
//! blocks and rounded corners; Windows Terminal, VS Code, macOS and Linux terminals draw them.
//!
//! The choice is made once per run: `CANFIT_ASCII=1` forces ASCII, `CANFIT_ASCII=0` forces
//! Unicode, and otherwise ASCII is used only when stdout is a Windows console that no modern
//! terminal hosts. Output that is piped or `--json` is never changed.
//!
//! The monitor maps each drawn cell after rendering, so layout code keeps one set of symbols;
//! text printed on stdout goes through [`text`].

use std::borrow::Cow;
use std::sync::OnceLock;

use ratatui::Frame;

/// Environment variable that forces ASCII (`1`) or Unicode (`0`).
pub const ASCII_ENV: &str = "CANFIT_ASCII";

static ASCII: OnceLock<bool> = OnceLock::new();

/// Whether canfit draws ASCII stand-ins on this terminal.
pub fn ascii() -> bool {
    *ASCII.get_or_init(|| {
        // Tests render with the real symbols wherever they run.
        if cfg!(test) {
            return false;
        }
        decide(std::env::var(ASCII_ENV).ok().as_deref(), legacy_console())
    })
}

/// `setting` is the value of [`ASCII_ENV`]; `legacy` says whether stdout is a legacy console.
pub fn decide(setting: Option<&str>, legacy: bool) -> bool {
    match setting.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => legacy,
    }
}

/// True when stdout is a Windows console window not hosted by Windows Terminal, VS Code,
/// ConEmu, WezTerm, mintty or another terminal that announces itself.
#[cfg(windows)]
fn legacy_console() -> bool {
    use std::io::IsTerminal;
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    std::io::stdout().is_terminal()
        && !set("WT_SESSION")
        && !set("TERM_PROGRAM")
        && !set("TERM")
        && !set("ConEmuANSI")
        && !set("WEZTERM_EXECUTABLE")
        && !set("ALACRITTY_LOG")
}

#[cfg(not(windows))]
fn legacy_console() -> bool {
    false
}

/// Whether escape sequences for colour work on stdout. On Windows this also switches the
/// console to virtual-terminal processing (Windows 10 and later); when that fails, canfit prints
/// plain text.
pub fn ansi_supported() -> bool {
    #[cfg(windows)]
    return ratatui::crossterm::ansi_support::supports_ansi();
    #[cfg(not(windows))]
    return true;
}

/// The ASCII stand-in for one drawn symbol, or `None` when it is ASCII already or has no
/// stand-in (app names keep their letters).
pub fn cell(symbol: &str) -> Option<&'static str> {
    let mut chars = symbol.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    Some(match c {
        '●' => "+",
        '▲' => "!",
        '✗' => "x",
        '×' => "x",
        '█' | '▇' => "#",
        '▆' | '▅' => "=",
        '▄' | '▃' => "-",
        '▂' | '▁' => "_",
        '░' => ".",
        '─' | '━' | '═' => "-",
        '│' | '┃' | '║' => "|",
        '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' => {
            "+"
        }
        '↑' => "^",
        '↓' => "v",
        '▸' | '→' => ">",
        '…' => ".",
        '·' | '—' | '–' | '−' => "-",
        '≈' => "~",
        _ => return None,
    })
}

/// Text with ASCII stand-ins when [`ascii`] is on; unchanged otherwise.
pub fn text(s: &str) -> Cow<'_, str> {
    if ascii() {
        to_ascii(s)
    } else {
        Cow::Borrowed(s)
    }
}

/// Replaces every symbol that has a stand-in. A few read better wider than one cell.
pub fn to_ascii(s: &str) -> Cow<'_, str> {
    if s.is_ascii() {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        match c {
            '…' => out.push_str("..."),
            '→' => out.push_str("->"),
            '—' => out.push_str("--"),
            _ => match cell(c.encode_utf8(&mut buf)) {
                Some(stand_in) => out.push_str(stand_in),
                None => out.push(c),
            },
        }
    }
    Cow::Owned(out)
}

/// Applies the stand-ins to a rendered monitor frame when [`ascii`] is on.
pub fn finish(frame: &mut Frame) {
    if ascii() {
        to_ascii_cells(frame);
    }
}

pub(crate) fn to_ascii_cells(frame: &mut Frame) {
    for cell_ref in frame.buffer_mut().content.iter_mut() {
        if let Some(stand_in) = cell(cell_ref.symbol()) {
            cell_ref.set_symbol(stand_in);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_overrides_detection() {
        assert!(decide(Some("1"), false));
        assert!(decide(Some(" YES "), false));
        assert!(!decide(Some("0"), true));
        assert!(decide(None, true));
        assert!(!decide(None, false));
        assert!(decide(Some("maybe"), true));
    }

    #[test]
    fn symbols_map_to_ascii_and_names_keep_their_letters() {
        assert_eq!(
            to_ascii("✗ No, it doesn't fit… → close Chrome · ● ▲"),
            "x No, it doesn't fit... -> close Chrome - + !"
        );
        assert_eq!(to_ascii("Café Zoë"), "Café Zoë");
        assert!(matches!(to_ascii("plain"), Cow::Borrowed(_)));
        assert_eq!(cell("╭"), Some("+"));
        assert_eq!(cell("ab"), None);
        assert_eq!(cell("é"), None);
    }
}
