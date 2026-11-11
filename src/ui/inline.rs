//! Cards printed inline (no alternate screen) when stdout is a terminal: the fit check and the
//! advisor cards are drawn with the monitor's own card widget into a buffer, then printed with
//! ANSI colours. Plain text output (pipes, files, `NO_COLOR`) is unchanged.

use std::fmt::Write as _;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph, Widget};

use super::theme::{Theme, Tone};
use super::view::card_block;

/// Styles one card line: the text from its first status symbol on takes that symbol's tone.
fn styled_line(line: &str, theme: Theme) -> Line<'static> {
    match Tone::in_text(line) {
        Some((i, tone)) => Line::from(vec![
            Span::styled(line[..i].to_string(), theme.text()),
            Span::styled(line[i..].to_string(), theme.tone(tone)),
        ]),
        None => Line::styled(line.to_string(), theme.text()),
    }
}

/// Renders a card whose rows are already laid out (see `cli::card`), at its natural width.
pub fn card(title: &str, lines: &[String], theme: Theme) -> String {
    let content_w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let title_w = title.chars().count() + 6;
    // Same geometry as the plain card: two spaces inside each border, the longest line touching
    // the right padding.
    let width = (content_w + 4).max(title_w) + 2;
    let height = lines.len() + 2;
    let area = Rect::new(0, 0, width as u16, height as u16);
    let mut buffer = Buffer::empty(area);
    let block = card_block(vec![Span::styled(title.to_string(), theme.title())], theme)
        .padding(Padding::horizontal(2));
    let text: Vec<Line> = lines.iter().map(|l| styled_line(l, theme)).collect();
    Paragraph::new(text).block(block).render(area, &mut buffer);
    to_ansi(&buffer)
}

/// A sentence whose leading status symbol takes its tone, printed in bold.
pub fn sentence(text: &str, theme: Theme) -> String {
    let mut buffer_line = String::new();
    let bold = theme.title();
    match Tone::in_text(text) {
        Some((0, tone)) => {
            let symbol_len = tone.symbol().len();
            let _ = write!(
                buffer_line,
                "{}{}{}{}{RESET}",
                sgr(theme.tone(tone)),
                &text[..symbol_len],
                sgr(bold),
                &text[symbol_len..]
            );
        }
        _ => {
            let _ = write!(buffer_line, "{}{text}{RESET}", sgr(bold));
        }
    }
    buffer_line
}

const RESET: &str = "\x1b[0m";

fn color_code(color: Color) -> Option<String> {
    let named = |n: u8| Some(n.to_string());
    match color {
        Color::Reset => None,
        Color::Black => named(30),
        Color::Red => named(31),
        Color::Green => named(32),
        Color::Yellow => named(33),
        Color::Blue => named(34),
        Color::Magenta => named(35),
        Color::Cyan => named(36),
        Color::Gray => named(37),
        Color::DarkGray => named(90),
        Color::LightRed => named(91),
        Color::LightGreen => named(92),
        Color::LightYellow => named(93),
        Color::LightBlue => named(94),
        Color::LightMagenta => named(95),
        Color::LightCyan => named(96),
        Color::White => named(97),
        Color::Indexed(i) => Some(format!("38;5;{i}")),
        Color::Rgb(r, g, b) => Some(format!("38;2;{r};{g};{b}")),
    }
}

/// The SGR sequence that sets exactly `style` (starting from a reset).
fn sgr(style: Style) -> String {
    let mut codes = vec!["0".to_string()];
    let m = style.add_modifier;
    if m.contains(Modifier::BOLD) {
        codes.push("1".into());
    }
    if m.contains(Modifier::DIM) {
        codes.push("2".into());
    }
    if m.contains(Modifier::REVERSED) {
        codes.push("7".into());
    }
    if let Some(fg) = style.fg.and_then(color_code) {
        codes.push(fg);
    }
    format!("\x1b[{}m", codes.join(";"))
}

/// Converts a rendered buffer into text with ANSI SGR sequences, one line per row.
pub fn to_ansi(buffer: &Buffer) -> String {
    let mut out = String::new();
    let area = buffer.area;
    for y in area.top()..area.bottom() {
        let mut current = Style::default();
        // Cells covered by the previous wide character hold nothing to print.
        let mut covered = 0usize;
        for x in area.left()..area.right() {
            let cell = &buffer[(x, y)];
            if covered > 0 {
                covered -= 1;
                continue;
            }
            covered = Span::raw(cell.symbol()).width().saturating_sub(1);
            let style = Style::new().fg(cell.fg).add_modifier(cell.modifier);
            if style != current {
                out.push_str(&sgr(style));
                current = style;
            }
            out.push_str(cell.symbol());
        }
        if current != Style::default() {
            out.push_str(RESET);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::ThemeName;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn styled_card_has_the_plain_card_geometry() {
        let lines = vec![
            "Needed          9.4 GB  estimated".to_string(),
            "Available now  10.1 GB  measured · ● 0.7 GB to spare".to_string(),
        ];
        let theme = Theme::new(ThemeName::Dark, true);
        let text = card("Can qwen fit?", &lines, theme);
        let plain = strip_ansi(&text);
        let rows = crate::cli::card::render_lines("Can qwen fit?", &lines);
        assert_eq!(plain, rows, "styled:\n{plain}\nplain:\n{rows}");
        // The symbol and what follows it are green (92 = light green).
        assert!(text.contains("\x1b[0;92m● 0.7 GB to spare"), "{text:?}");
    }

    #[test]
    fn sentences_colour_their_leading_symbol() {
        let theme = Theme::new(ThemeName::Light, true);
        let s = sentence("✗ No, it doesn't fit.", theme);
        assert!(s.starts_with("\x1b[0;31m✗\x1b[0;1m No"), "{s:?}");
        assert!(s.ends_with(RESET));
        assert_eq!(strip_ansi(&s), "✗ No, it doesn't fit.");
    }

    #[test]
    fn no_color_theme_emits_no_colour_codes() {
        let theme = Theme::new(ThemeName::Dark, false);
        let text = card("T", &["a ▲ b".to_string()], theme);
        for code in ["31", "32", "33", "90", "91", "92", "93", "97"] {
            assert!(!text.contains(&format!(";{code}m")), "{text:?}");
        }
    }
}
