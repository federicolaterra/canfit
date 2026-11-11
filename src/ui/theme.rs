//! Colour themes (spec §11): dark, light and high contrast, built from the terminal's own named
//! colours so they follow its palette. Colour is never the only signal: every tone is paired with
//! a symbol (● ▲ ✗). `NO_COLOR` (or `TERM=dumb`) removes every colour and keeps only bold and
//! reverse video.

use ratatui::style::{Color, Modifier, Style};

use crate::advisor::SafetyClass;
use crate::core::PressureLevel;

/// The theme a user picks with `--theme` or cycles with `t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ThemeName {
    /// Bright colours for dark terminal backgrounds (default).
    Dark,
    /// Deeper colours for light terminal backgrounds.
    Light,
    /// Bold, saturated colours and no dimmed text.
    HighContrast,
}

impl ThemeName {
    pub fn label(self) -> &'static str {
        match self {
            ThemeName::Dark => "dark",
            ThemeName::Light => "light",
            ThemeName::HighContrast => "high contrast",
        }
    }

    fn next(self) -> ThemeName {
        match self {
            ThemeName::Dark => ThemeName::Light,
            ThemeName::Light => ThemeName::HighContrast,
            ThemeName::HighContrast => ThemeName::Dark,
        }
    }
}

/// What a value means, independent of colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// ● fine / safe / fits.
    Ok,
    /// ▲ worth watching / caution / tight.
    Warn,
    /// ✗ critical / never / doesn't fit.
    Bad,
}

impl Tone {
    pub fn symbol(self) -> &'static str {
        match self {
            Tone::Ok => "●",
            Tone::Warn => "▲",
            Tone::Bad => "✗",
        }
    }

    pub fn from_level(level: PressureLevel) -> Tone {
        match level {
            PressureLevel::Calm => Tone::Ok,
            PressureLevel::Strained => Tone::Warn,
            PressureLevel::Critical => Tone::Bad,
        }
    }

    pub fn from_class(class: SafetyClass) -> Tone {
        match class {
            SafetyClass::Safe => Tone::Ok,
            SafetyClass::Caution => Tone::Warn,
            SafetyClass::Never => Tone::Bad,
        }
    }

    /// The tone of the first status symbol in `text`, if any.
    pub fn in_text(text: &str) -> Option<(usize, Tone)> {
        text.char_indices().find_map(|(i, c)| match c {
            '●' => Some((i, Tone::Ok)),
            '▲' => Some((i, Tone::Warn)),
            '✗' => Some((i, Tone::Bad)),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: ThemeName,
    /// False under `NO_COLOR`: styles keep only bold and reverse video.
    pub color: bool,
}

/// True when the environment asks for no colour (`NO_COLOR` set and non-empty, or `TERM=dumb`).
pub fn no_color_requested() -> bool {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let dumb = std::env::var_os("TERM").is_some_and(|t| t == "dumb");
    no_color || dumb
}

impl Theme {
    pub fn new(name: ThemeName, color: bool) -> Theme {
        Theme { name, color }
    }

    /// The theme `name`, with colour unless the environment disables it.
    pub fn from_env(name: ThemeName) -> Theme {
        Theme::new(name, !no_color_requested())
    }

    /// The next theme in the cycle (the `t` key).
    pub fn cycled(self) -> Theme {
        Theme {
            name: self.name.next(),
            ..self
        }
    }

    fn fg(self, color: Color) -> Style {
        if self.color {
            Style::new().fg(color)
        } else {
            Style::new()
        }
    }

    fn high_contrast(self) -> bool {
        self.name == ThemeName::HighContrast
    }

    /// Text and bars carrying a tone.
    pub fn tone(self, tone: Tone) -> Style {
        let color = match (self.name, tone) {
            (ThemeName::Light, Tone::Ok) => Color::Green,
            (ThemeName::Light, Tone::Warn) => Color::Yellow,
            (ThemeName::Light, Tone::Bad) => Color::Red,
            (_, Tone::Ok) => Color::LightGreen,
            (_, Tone::Warn) => Color::LightYellow,
            (_, Tone::Bad) => Color::LightRed,
        };
        let style = self.fg(color);
        if self.high_contrast() || !self.color {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    /// Ordinary text.
    pub fn text(self) -> Style {
        if self.high_contrast() && self.color {
            Style::new().fg(Color::White)
        } else {
            Style::new()
        }
    }

    /// Secondary text: provenance labels, hints, empty bar segments.
    pub fn dim(self) -> Style {
        match (self.color, self.name) {
            (false, _) | (true, ThemeName::HighContrast) => Style::new(),
            (true, ThemeName::Dark) => Style::new().fg(Color::DarkGray),
            (true, ThemeName::Light) => Style::new().fg(Color::Gray),
        }
    }

    /// Card borders.
    pub fn border(self) -> Style {
        match (self.color, self.name) {
            (false, _) => Style::new(),
            (true, ThemeName::Dark) => Style::new().fg(Color::Gray),
            (true, ThemeName::Light) => Style::new().fg(Color::DarkGray),
            (true, ThemeName::HighContrast) => Style::new().fg(Color::White),
        }
    }

    /// Card titles and the summary sentence.
    pub fn title(self) -> Style {
        self.text().add_modifier(Modifier::BOLD)
    }

    /// Key names in hints.
    pub fn key(self) -> Style {
        let style = self.fg(Color::Cyan).add_modifier(Modifier::BOLD);
        if self.name == ThemeName::Light && self.color {
            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    /// The selected row of a list.
    pub fn selected(self) -> Style {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_keeps_only_modifiers() {
        for name in [ThemeName::Dark, ThemeName::Light, ThemeName::HighContrast] {
            let t = Theme::new(name, false);
            for style in [
                t.tone(Tone::Ok),
                t.tone(Tone::Warn),
                t.tone(Tone::Bad),
                t.text(),
                t.dim(),
                t.border(),
                t.title(),
                t.key(),
                t.selected(),
            ] {
                assert_eq!(style.fg, None, "{name:?}");
                assert_eq!(style.bg, None, "{name:?}");
            }
        }
    }

    #[test]
    fn tones_have_distinct_colours_and_symbols() {
        let t = Theme::new(ThemeName::Dark, true);
        assert_ne!(t.tone(Tone::Ok), t.tone(Tone::Warn));
        assert_ne!(t.tone(Tone::Warn), t.tone(Tone::Bad));
        assert_eq!(Tone::from_level(PressureLevel::Critical).symbol(), "✗");
        assert_eq!(Tone::from_class(SafetyClass::Caution).symbol(), "▲");
        assert_eq!(Tone::in_text("Avail · ▲ tight"), Some((9, Tone::Warn)));
        assert_eq!(Tone::in_text("plain"), None);
    }

    #[test]
    fn themes_cycle_through_all_three() {
        let t = Theme::new(ThemeName::Dark, true);
        assert_eq!(t.cycled().name, ThemeName::Light);
        assert_eq!(t.cycled().cycled().name, ThemeName::HighContrast);
        assert_eq!(t.cycled().cycled().cycled(), t);
    }
}
