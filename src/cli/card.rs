//! Cards with rounded borders (spec §11), shared by the fit check and the advisor: plain text,
//! or the monitor's card widget in colour when stdout is a terminal.

use std::fmt::Write as _;

use crate::ui::inline;
use crate::ui::theme::Theme;

/// One card line: a label, a right-aligned size and a description.
pub struct Row {
    pub label: String,
    pub size: String,
    pub text: String,
}

impl Row {
    pub fn new(label: impl Into<String>, size: impl Into<String>, text: impl Into<String>) -> Row {
        Row {
            label: label.into(),
            size: size.into(),
            text: text.into(),
        }
    }
}

/// Lays the rows out as text lines: label, right-aligned size, description.
pub fn lines(rows: &[Option<Row>]) -> Vec<String> {
    let label_w = rows
        .iter()
        .flatten()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(13, 32);
    let size_w = rows
        .iter()
        .flatten()
        .map(|r| r.size.chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|r| match r {
            None => String::new(),
            Some(r) => {
                let label: String = if r.label.chars().count() > label_w {
                    let mut short: String = r.label.chars().take(label_w - 1).collect();
                    short.push('…');
                    short
                } else {
                    r.label.clone()
                };
                format!("{label:<label_w$}  {:>size_w$}  {}", r.size, r.text)
                    .trim_end()
                    .to_string()
            }
        })
        .collect()
}

/// Renders a card; `None` rows are blank separator lines.
pub fn render(title: &str, rows: &[Option<Row>]) -> String {
    render_lines(title, &lines(rows))
}

/// Renders a card in colour with the monitor's card widget when `theme` is set (stdout is a
/// terminal and colour is allowed), otherwise as plain text.
pub fn render_styled(title: &str, rows: &[Option<Row>], theme: Option<Theme>) -> String {
    match theme {
        Some(theme) => inline::card(title, &lines(rows), theme),
        None => render(title, rows),
    }
}

/// Renders already laid-out lines inside a plain-text card.
pub fn render_lines(title: &str, lines: &[String]) -> String {
    let title = format!(" {title} ");
    let inner = lines
        .iter()
        .map(|l| l.chars().count() + 4)
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 4);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "╭─{title}{}╮",
        "─".repeat(inner - 1 - title.chars().count())
    );
    for line in lines {
        let pad = inner - 2 - line.chars().count();
        let _ = writeln!(out, "│  {line}{}│", " ".repeat(pad));
    }
    let _ = writeln!(out, "╰{}╯", "─".repeat(inner));
    out
}

/// A sentence in colour (leading status symbol in its tone) when `theme` is set.
pub fn sentence(text: &str, theme: Option<Theme>) -> String {
    match theme {
        Some(theme) => inline::sentence(text, theme),
        None => text.to_string(),
    }
}

/// Wraps a paragraph at `width` columns, indenting every line by `indent` spaces.
pub fn wrap(text: &str, width: usize, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && indent + line.chars().count() + 1 + word.chars().count() > width {
            let _ = writeln!(out, "{pad}{line}");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        let _ = writeln!(out, "{pad}{line}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_are_rectangular() {
        let text = render(
            "Title",
            &[
                Some(Row::new("A", "1.0 GB", "measured")),
                None,
                Some(Row::new("Longer label", "", "é unicode text")),
                Some(Row::new(
                    "A label much longer than thirty-two characters",
                    "",
                    "x",
                )),
            ],
        );
        let widths: Vec<usize> = text.lines().map(|l| l.chars().count()).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{text}");
        assert!(text.starts_with("╭─ Title ─"));
        assert!(
            text.contains("A label much longer than thirty…  "),
            "{text}"
        );
    }

    #[test]
    fn wrap_keeps_words_and_indent() {
        let text = wrap("one two three four five", 12, 2);
        assert_eq!(text, "  one two\n  three four\n  five\n");
        assert_eq!(wrap("", 10, 2), "");
    }
}
