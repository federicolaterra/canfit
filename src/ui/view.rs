//! Drawing the monitor: summary sentence, metric cards with bars and sparklines, the app list,
//! the "Why?" panel, the footer and the overlays. Layout adapts from 80×24 to full screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Sparkline, Wrap};

use super::app::{App, Overlay};
use super::history::{HISTORY_POINTS, PERCENT_SCALE, tail};
use super::theme::{Theme, Tone};
use crate::cli::report::summary_sentence;
use crate::core::pressure::growth_rate;
use crate::core::{Provenance, format_bytes, format_rate};

/// Below this width the four cards wrap into two rows.
const ONE_ROW_CARDS_WIDTH: u16 = 80;
/// From this width the "Why?" panel sits beside the app list.
const SIDE_PANEL_WIDTH: u16 = 110;
/// From this height cards get taller sparklines.
const TALL_HEIGHT: u16 = 40;

/// Where each part of the screen goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    pub summary: Rect,
    pub cards: [Rect; 4],
    /// The app list card, border included.
    pub list: Rect,
    pub why: Option<Rect>,
    pub footer: Rect,
}

impl Areas {
    /// The app list's rows, inside the border.
    pub fn list_rows(&self) -> Rect {
        self.list.inner(ratatui::layout::Margin::new(1, 1))
    }
}

/// Lines `text` takes when word-wrapped at `width` columns.
pub fn wrapped_height(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let mut lines = 1u16;
    let mut used = 0usize;
    for word in text.split_whitespace() {
        let len = word.chars().count();
        if used > 0 && used + 1 + len > width {
            lines += 1;
            used = len;
        } else {
            used += if used > 0 { 1 + len } else { len };
        }
    }
    lines
}

fn headline(app: &App) -> String {
    format!(
        "{} {}",
        app.snapshot.pressure.level.symbol(),
        summary_sentence(&app.snapshot)
    )
}

pub fn layout(area: Rect, app: &App) -> Areas {
    let summary_h = wrapped_height(&format!(" {}", headline(app)), area.width).clamp(1, 3);
    let card_h = if area.height >= TALL_HEIGHT { 7 } else { 5 };
    let one_row = area.width >= ONE_ROW_CARDS_WIDTH;
    let cards_h = if one_row { card_h } else { card_h * 2 };
    let [summary, cards, body, footer] = Layout::vertical([
        Constraint::Length(summary_h),
        Constraint::Length(cards_h),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    let cards = if one_row {
        Layout::horizontal([Constraint::Ratio(1, 4); 4]).areas(cards)
    } else {
        let [top, bottom] = Layout::vertical([Constraint::Length(card_h); 2]).areas(cards);
        let [a, b] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(top);
        let [c, d] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(bottom);
        [a, b, c, d]
    };

    let (list, why) = if area.width >= SIDE_PANEL_WIDTH {
        let [list, why] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(body);
        (list, Some(why))
    } else if body.height >= 14 {
        let [list, why] = Layout::vertical([Constraint::Min(0), Constraint::Length(5)]).areas(body);
        (list, Some(why))
    } else {
        (body, None)
    };
    Areas {
        summary,
        cards,
        list,
        why,
        footer,
    }
}

/// A card with rounded borders and a `─ title ─` heading, the building block of every screen.
pub fn card_block<'a>(title: Vec<Span<'a>>, theme: Theme) -> Block<'a> {
    let mut spans = vec![Span::styled("─ ", theme.border())];
    spans.extend(title);
    spans.push(Span::raw(" "));
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border())
        .title(Line::from(spans))
}

/// A bottom-right label such as the provenance of a card's values.
fn footnote(text: impl Into<String>, theme: Theme) -> Line<'static> {
    Line::from(Span::styled(format!(" {} ", text.into()), theme.dim())).right_aligned()
}

fn tone_title(symbol_tone: Tone, title: &str, theme: Theme) -> Vec<Span<'static>> {
    vec![
        Span::styled(symbol_tone.symbol(), theme.tone(symbol_tone)),
        Span::raw(" "),
        Span::styled(title.to_string(), theme.title()),
    ]
}

/// A horizontal bar with a percentage, e.g. `██████░░░░  62%`.
fn bar(fraction: f64, width: u16, tone: Tone, theme: Theme) -> Line<'static> {
    let fraction = fraction.clamp(0.0, 1.0);
    let label = format!(" {:>3.0}%", fraction * 100.0);
    let bar_w = usize::from(width).saturating_sub(label.len());
    let filled = ((fraction * bar_w as f64).round() as usize).min(bar_w);
    Line::from(vec![
        Span::styled("█".repeat(filled), theme.tone(tone)),
        Span::styled("░".repeat(bar_w - filled), theme.dim()),
        Span::styled(label, theme.text()),
    ])
}

struct Metric<'a> {
    title: &'a str,
    tone: Tone,
    value: String,
    fraction: f64,
    series: Vec<u64>,
    max: u64,
    provenance: Provenance,
}

fn draw_metric(frame: &mut Frame, area: Rect, m: Metric<'_>, theme: Theme) {
    let block = card_block(tone_title(m.tone, m.title, theme), theme)
        .title_bottom(footnote(m.provenance.label(), theme));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let [value, gauge, spark] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(Span::styled(m.value, theme.text())), value);
    frame.render_widget(bar(m.fraction, gauge.width, m.tone, theme), gauge);
    if spark.height > 0 {
        let data = tail_of(&m.series, usize::from(spark.width));
        frame.render_widget(
            Sparkline::default()
                .data(data)
                .max(m.max.max(1))
                .style(theme.tone(m.tone)),
            spark,
        );
    }
}

fn tail_of(series: &[u64], n: usize) -> Vec<u64> {
    series[series.len().saturating_sub(n)..].to_vec()
}

fn fraction(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    }
}

fn draw_cards(frame: &mut Frame, cards: [Rect; 4], app: &App) {
    let s = &app.snapshot;
    let h = &app.history;
    let theme = app.theme;
    let width = usize::from(cards[0].width);

    let level_tone = Tone::from_level(s.pressure.level);
    draw_metric(
        frame,
        cards[0],
        Metric {
            title: "Memory",
            tone: level_tone,
            value: format!("{} available", format_bytes(s.memory.available_bytes)),
            fraction: fraction(s.memory.used_bytes, s.memory.total_bytes),
            series: tail(&h.memory, width),
            max: PERCENT_SCALE,
            provenance: s.memory.provenance,
        },
        theme,
    );

    let growing = s.swap_trend.as_ref().filter(|t| t.growing);
    let swap_value = match (s.swap.total_bytes, growing, &s.swap_trend) {
        (0, ..) => "no swap in use".to_string(),
        (_, Some(t), _) => format!("growing {}", format_rate(growth_rate(t))),
        (_, None, Some(_)) => format!("{} · steady", format_bytes(s.swap.used_bytes)),
        (_, None, None) => format!("{} used", format_bytes(s.swap.used_bytes)),
    };
    draw_metric(
        frame,
        cards[1],
        Metric {
            title: "Swap",
            tone: if growing.is_some() {
                Tone::Warn
            } else {
                Tone::Ok
            },
            value: swap_value,
            fraction: fraction(s.swap.used_bytes, s.swap.total_bytes),
            series: tail(&h.swap, width),
            max: s.swap.total_bytes / (1024 * 1024),
            provenance: s.swap.provenance,
        },
        theme,
    );

    let cpu = s.cpu.usage_percent.map(f64::from);
    let cpu_tone = match cpu {
        Some(u) if u >= 90.0 => Tone::Bad,
        Some(u) if u >= 70.0 => Tone::Warn,
        _ => Tone::Ok,
    };
    draw_metric(
        frame,
        cards[2],
        Metric {
            title: "CPU",
            tone: cpu_tone,
            value: match cpu {
                Some(u) => format!("{u:.0}% of {} cores", s.cpu.logical_cores),
                None => "measuring…".to_string(),
            },
            fraction: cpu.unwrap_or(0.0) / 100.0,
            series: tail(&h.cpu, width),
            max: PERCENT_SCALE,
            provenance: s.cpu.provenance,
        },
        theme,
    );

    let p = &s.pressure;
    draw_metric(
        frame,
        cards[3],
        Metric {
            title: "Pressure",
            tone: level_tone,
            value: format!("{} · {}/100", p.level.label(), p.score),
            fraction: f64::from(p.score) / 100.0,
            series: tail(&h.pressure, width),
            max: 100,
            provenance: p.provenance,
        },
        theme,
    );
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        format!("{text:<width$}")
    } else if width == 0 {
        String::new()
    } else {
        let mut s: String = text.chars().take(width - 1).collect();
        s.push('…');
        s
    }
}

fn draw_list(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;
    let count = app.row_count();
    let position = if count == 0 {
        String::new()
    } else {
        format!("{}/{count}", app.selected + 1)
    };
    let block = card_block(
        vec![
            Span::styled("Apps", theme.title()),
            Span::styled(" · largest first", theme.dim()),
        ],
        theme,
    )
    .title_bottom(Line::from(Span::styled(
        format!(" {position} "),
        theme.dim(),
    )))
    .title_bottom(footnote(
        format!("measured · {}", app.snapshot.process_memory_kind().label()),
        theme,
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if count == 0 {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "canfit could not read any app's memory.",
                theme.dim(),
            )),
            inner,
        );
        return;
    }

    let w = usize::from(inner.width);
    let show_processes = w >= 60;
    let fixed = 2 + 2 + 9 + 9 + if show_processes { 14 } else { 0 };
    let name_w = w.saturating_sub(fixed).max(6);
    let lines: Vec<Line> = app
        .rows()
        .enumerate()
        .skip(app.offset)
        .take(usize::from(inner.height))
        .map(|(i, g)| {
            let tone = Tone::from_class(g.class);
            let selected = i == app.selected;
            let muted = if g.class == crate::advisor::SafetyClass::Never {
                theme.dim()
            } else {
                theme.text()
            };
            let mut spans = vec![
                Span::raw(if selected { "▸ " } else { "  " }),
                Span::styled(tone.symbol(), theme.tone(tone)),
                Span::raw(" "),
                Span::styled(truncate(&g.group.name, name_w), muted),
                Span::styled(
                    format!("{:>9}", format_bytes(g.group.memory_bytes)),
                    theme.text(),
                ),
            ];
            if show_processes {
                spans.push(Span::styled(
                    format!(
                        "{:>14}",
                        plural(g.group.process_count, "process", "processes")
                    ),
                    theme.dim(),
                ));
            }
            spans.push(Span::styled(format!("  {:<7}", g.class.label()), muted));
            let line = Line::from(spans);
            if selected {
                line.style(theme.selected())
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn selected_line(app: &App) -> Option<Line<'static>> {
    let theme = app.theme;
    let g = app.selected_group()?;
    let tone = Tone::from_class(g.class);
    let action = match g.class {
        crate::advisor::SafetyClass::Never => String::new(),
        _ if app.can_close => " Press x to close it (canfit asks first).".to_string(),
        _ => String::new(),
    };
    Some(Line::from(vec![
        Span::styled(tone.symbol(), theme.tone(tone)),
        Span::styled(format!(" {}", g.group.name), theme.title()),
        Span::styled(
            format!(": {}, {}.{action}", g.class.label(), g.reason),
            theme.text(),
        ),
    ]))
}

fn draw_why(frame: &mut Frame, area: Rect, app: &App, compact: bool) {
    let theme = app.theme;
    let mut block = card_block(vec![Span::styled("Why?", theme.title())], theme);
    if compact {
        block = block.title_bottom(footnote("? more", theme));
    }
    let mut lines = Vec::new();
    if let Some(line) = selected_line(app) {
        lines.push(line);
        if !compact {
            lines.push(Line::raw(""));
        }
    }
    lines.push(Line::styled(app.advice.why.clone(), theme.text()));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

fn format_interval(app: &App) -> String {
    let secs = app.interval.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("{secs:.0} s")
    } else {
        format!("{secs} s")
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;
    // A message takes the whole line; the "Why?" panel repeats anything cut here.
    if let Some(n) = &app.notice {
        let tone = if n.warning { Tone::Warn } else { Tone::Ok };
        let room = usize::from(area.width).saturating_sub(3);
        let text = truncate(&n.text, room);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(tone.symbol(), theme.tone(tone)),
                Span::styled(format!(" {}", text.trim_end()), theme.text()),
            ])),
            area,
        );
        return;
    }
    let right = format!(
        "{}every {} · {} ",
        if app.history_off {
            "history off · "
        } else {
            ""
        },
        format_interval(app),
        theme.name.label()
    );
    let mut hints: Vec<(&str, &str)> = vec![("↑↓", "move")];
    if app.can_close {
        hints.push(("x", "close"));
    }
    hints.extend([
        ("?", "explain"),
        ("h", "history"),
        ("t", "theme"),
        ("q", "quit"),
    ]);
    // On narrow terminals the least needed hints go first; the `?` panel lists every key.
    let width = |hints: &[(&str, &str)]| -> usize {
        1 + hints
            .iter()
            .map(|(k, w)| k.chars().count() + w.chars().count() + 3)
            .sum::<usize>()
    };
    for drop in ["theme", "history", "explain"] {
        if width(&hints) + right.chars().count() <= usize::from(area.width) {
            break;
        }
        hints.retain(|(_, w)| *w != drop);
    }
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in hints {
        spans.push(Span::styled(key, theme.key()));
        spans.push(Span::styled(format!(" {what}  "), theme.dim()));
    }
    let left = Line::from(spans);
    let right_w = right.chars().count() as u16;
    let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).areas(area);
    frame.render_widget(Paragraph::new(left), l);
    frame.render_widget(
        Paragraph::new(Span::styled(right, theme.dim())).right_aligned(),
        r,
    );
}

/// A centred rectangle at most `width` × `height` inside `area`.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn draw_explain(frame: &mut Frame, app: &App) {
    let theme = app.theme;
    let area = centered(
        frame.area(),
        frame.area().width.saturating_sub(4).min(92),
        frame.area().height.saturating_sub(2),
    );
    let block = card_block(vec![Span::styled("Explain", theme.title())], theme)
        .title_bottom(footnote("? or Esc to close", theme));
    let readings = app.history.memory.len().min(HISTORY_POINTS);
    let mut lines = vec![
        Line::styled(format!("Why? {}", app.advice.why), theme.text()),
        Line::raw(""),
    ];
    if let Some(line) = selected_line(app) {
        lines.push(line);
        lines.push(Line::raw(""));
    }
    let legend = |tone: Tone, text: &'static str| {
        vec![
            Span::styled(tone.symbol(), theme.tone(tone)),
            Span::styled(format!(" {text}   "), theme.text()),
        ]
    };
    let mut symbols = vec![Span::styled("Symbols: ", theme.title())];
    symbols.extend(legend(Tone::Ok, "calm · safe"));
    symbols.extend(legend(Tone::Warn, "strained · caution"));
    symbols.extend(legend(Tone::Bad, "critical · never closed"));
    lines.push(Line::from(symbols));
    lines.push(Line::styled(
        "Colours only repeat the symbols. Memory, swap, CPU and app sizes are measured; the \
         pressure index and the memory freed by closing an app are estimated.",
        theme.text(),
    ));
    lines.push(Line::styled(
        format!(
            "Graphs: the last {}, one every {} (up to {HISTORY_POINTS}).",
            plural(readings, "reading", "readings"),
            format_interval(app)
        ),
        theme.text(),
    ));
    lines.push(Line::styled(
        "Keys: ↑↓ or j/k move · g/G first/last · x close the selected app (canfit lists every \
         process and asks; forcing asks again) · h pressure history · t theme · q quit · click \
         or scroll the list.",
        theme.text(),
    ));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

/// A line whose text from its first status symbol on takes that symbol's tone.
fn toned_line(text: &str, theme: Theme) -> Line<'static> {
    match Tone::in_text(text) {
        Some((i, tone)) => Line::from(vec![
            Span::styled(text[..i].to_string(), theme.text()),
            Span::styled(text[i..].to_string(), theme.tone(tone)),
        ]),
        None => Line::styled(text.to_string(), theme.text()),
    }
}

/// The history panel: pressure events of the last day, as `canfit history` prints them.
fn draw_history(frame: &mut Frame, app: &App) {
    let theme = app.theme;
    let t = &app.timeline;
    let area = centered(
        frame.area(),
        frame.area().width.saturating_sub(4).min(100),
        frame.area().height.saturating_sub(2),
    );
    let block = card_block(
        vec![Span::styled("History · last 24 hours", theme.title())],
        theme,
    )
    .title_bottom(footnote("h or Esc to close", theme));
    let inner_w = area.width.saturating_sub(2);
    let mut lines = vec![toned_line(&t.summary, theme), Line::raw("")];
    // Newest events last; keep the newest when they don't all fit.
    let fixed = wrapped_height(&t.summary, inner_w) + wrapped_height(&t.status, inner_w) + 4;
    let room = usize::from(area.height.saturating_sub(fixed));
    // The "earlier" line takes one of the rows.
    let hidden = if t.rows.len() > room {
        t.rows.len() - room.saturating_sub(1)
    } else {
        0
    };
    if hidden > 0 {
        lines.push(Line::styled(
            format!(
                "… {} earlier (canfit history)",
                plural(hidden, "event", "events")
            ),
            theme.dim(),
        ));
    }
    for row in t.rows.iter().skip(hidden) {
        lines.push(toned_line(&truncate(row, usize::from(inner_w)), theme));
    }
    if !t.rows.is_empty() {
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(t.status.clone(), theme.dim()));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

/// Draws the whole monitor.
pub fn draw(frame: &mut Frame, app: &App) {
    let areas = layout(frame.area(), app);
    let theme = app.theme;
    let tone = Tone::from_level(app.snapshot.pressure.level);
    let sentence = summary_sentence(&app.snapshot);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(app.snapshot.pressure.level.symbol(), theme.tone(tone)),
            Span::styled(format!(" {sentence}"), theme.title()),
        ]))
        .wrap(Wrap { trim: false }),
        areas.summary,
    );
    draw_cards(frame, areas.cards, app);
    draw_list(frame, areas.list, app);
    if let Some(why) = areas.why {
        draw_why(frame, why, app, why.height < 8);
    }
    draw_footer(frame, areas.footer, app);
    match app.overlay {
        Overlay::Explain => draw_explain(frame, app),
        Overlay::History => draw_history(frame, app),
        Overlay::None => {}
    }
}

/// The confirmation dialog of the closing flow, drawn over the monitor.
pub fn draw_dialog(frame: &mut Frame, theme: Theme, body: &[String], question: &str, scroll: u16) {
    let full = frame.area();
    let width = full.width.saturating_sub(4).min(80);
    let inner_w = width.saturating_sub(4);
    let question_h = wrapped_height(question, inner_w);
    let wanted = body.len() as u16 + question_h + 5;
    let area = centered(full, width, wanted.min(full.height.saturating_sub(2)));
    let block = card_block(vec![Span::styled("Close apps", theme.title())], theme)
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let [list, gap, ask, keys] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(question_h),
        Constraint::Length(1),
    ])
    .areas(inner);
    let _ = gap;
    let max_scroll = (body.len() as u16).saturating_sub(list.height);
    let scroll = scroll.min(max_scroll);
    let lines: Vec<Line> = body
        .iter()
        .map(|l| Line::styled(l.clone(), theme.text()))
        .collect();
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), list);
    let tone = if question.contains("Force") {
        Tone::Bad
    } else {
        Tone::Warn
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(tone.symbol(), theme.tone(tone)),
            Span::styled(format!(" {question}"), theme.title()),
        ]))
        .wrap(Wrap { trim: true }),
        ask,
    );
    let mut spans = vec![
        Span::styled("y", theme.key()),
        Span::styled(" yes   ", theme.dim()),
        Span::styled("any other key", theme.key()),
        Span::styled(" no", theme.dim()),
    ];
    let hidden = max_scroll.saturating_sub(scroll);
    if max_scroll > 0 {
        spans.push(Span::styled("   ↑↓", theme.key()));
        spans.push(Span::styled(
            format!(" scroll ({hidden} more below)"),
            theme.dim(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), keys);
}

/// A one-line "please wait" box while apps are asked to quit.
pub fn draw_busy(frame: &mut Frame, theme: Theme, text: &str) {
    let full = frame.area();
    let width = full.width.saturating_sub(4).min(80);
    let h = wrapped_height(text, width.saturating_sub(4)) + 2;
    let area = centered(full, width, h);
    let block = card_block(vec![Span::styled("Closing", theme.title())], theme)
        .padding(ratatui::widgets::Padding::horizontal(1));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Span::styled(text.to_string(), theme.text()))
            .wrap(Wrap { trim: true })
            .block(block),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor;
    use crate::advisor::classify::tests::{CANFIT_PID, GB, desktop};
    use crate::core::group::group_processes;
    use crate::core::sampler::tests::replay;
    use crate::ui::app::tests::sample_app;
    use crate::ui::app::{Action, App, Notice};
    use crate::ui::theme::ThemeName;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    /// A fake history: a slow wave, so sparklines have a visible, fixed shape.
    fn with_history(mut app: App) -> App {
        app.snapshot.cpu.usage_percent = Some(23.0);
        let s = app.snapshot.clone();
        for i in 0..120u64 {
            let mut s = s.clone();
            let wave = (i % 20) as f64 / 20.0;
            s.cpu.usage_percent = Some((10.0 + 40.0 * wave) as f32);
            s.pressure.score = (s.pressure.score as f64 * (0.5 + wave / 2.0)) as u8;
            s.memory.used_bytes =
                s.memory.total_bytes / 2 + (s.memory.total_bytes as f64 * wave / 4.0) as u64;
            app.history.record(&s);
        }
        app
    }

    /// A critical machine: the "critical" fixture with the desktop's apps.
    fn critical_app() -> App {
        let (_, mut s) = replay("critical");
        let mut processes = desktop();
        // Chrome's renderer has grown.
        processes[11].memory.as_mut().unwrap().bytes = 9 * GB;
        s.app_groups = group_processes(&processes);
        s.processes = processes;
        let advice = advisor::advise(&s, CANFIT_PID, vec![]);
        with_history(App::new(
            s,
            advice,
            Theme::new(ThemeName::Dark, true),
            Duration::from_secs(1),
            true,
        ))
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn ascii_stand_ins_cover_every_drawn_symbol() {
        let app = critical_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
                draw_dialog(f, app.theme, &["a … list".to_string()], "Close?", 0);
                crate::ui::glyphs::to_ascii_cells(f);
            })
            .unwrap();
        for cell in terminal.backend().buffer().content.iter() {
            assert!(
                cell.symbol().is_ascii(),
                "{:?} left in the frame",
                cell.symbol()
            );
        }
    }

    #[test]
    fn calm_monitor_at_80x24() {
        insta::assert_snapshot!(render(&with_history(sample_app()), 80, 24));
    }

    #[test]
    fn critical_monitor_at_80x24() {
        insta::assert_snapshot!(render(&critical_app(), 80, 24));
    }

    #[test]
    fn critical_monitor_full_screen() {
        insta::assert_snapshot!(render(&critical_app(), 140, 44));
    }

    #[test]
    fn narrow_terminal_reflows_cards() {
        insta::assert_snapshot!(render(&critical_app(), 64, 30));
    }

    #[test]
    fn explain_overlay_at_80x24() {
        let mut app = critical_app();
        app.apply(Action::Explain, Rect::default());
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn history_panel_over_the_monitor() {
        let mut app = critical_app();
        app.timeline = crate::ui::app::Timeline {
            summary: "✗ 2 pressure events in the last 24 hours. The worst was critical for 4 \
                      min, today 14:02, when Google Chrome was using 6.0 GB."
                .into(),
            rows: vec![
                "yesterday 22:10  12 min  ▲ strained  Google Chrome 5.1 GB, Docker 3.0 GB".into(),
                "today 14:02       4 min  ✗ critical  Google Chrome 6.0 GB, Docker 3.0 GB".into(),
            ],
            status: "Recording to /data/history.sqlite3 (canfit history shows more).".into(),
        };
        app.apply(Action::History, Rect::default());
        insta::assert_snapshot!(render(&app, 80, 24));
        // On a short terminal the newest events stay visible.
        app.timeline.rows = (0..40)
            .map(|i| format!("event {i:02}  ▲ strained"))
            .collect();
        let screen = render(&app, 80, 24);
        assert!(screen.contains("event 39"), "{screen}");
        assert!(!screen.contains("event 00"), "{screen}");
        assert!(screen.contains("earlier (canfit history)"), "{screen}");
    }

    #[test]
    fn footer_says_when_history_is_off() {
        let mut app = critical_app();
        let footer = render(&app, 100, 24).lines().nth(23).unwrap().to_string();
        assert!(!footer.contains("history off"), "{footer}");
        app.history_off = true;
        let footer = render(&app, 100, 24).lines().nth(23).unwrap().to_string();
        assert!(footer.contains("history off · every"), "{footer}");
        // Narrow terminals drop key hints first, never the indicator.
        let footer = render(&app, 80, 24).lines().nth(23).unwrap().to_string();
        assert!(footer.contains("history off"), "{footer}");
    }

    #[test]
    fn notice_replaces_the_key_hints() {
        let mut app = critical_app();
        app.notice = Some(Notice {
            text: "canfit never closes Terminal: canfit is running inside it.".into(),
            warning: true,
        });
        let screen = render(&app, 80, 24);
        let footer = screen.lines().nth(23).unwrap();
        assert!(
            footer.contains("▲ canfit never closes Terminal"),
            "{footer}"
        );
        assert!(!footer.contains("explain"));
    }

    #[test]
    fn close_dialog_over_the_monitor() {
        let app = critical_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let body: Vec<String> = "1 app (2 processes) will be asked to quit:\n  Google Chrome (2 processes):\n    Google Chrome (pid 300)\n    Google Chrome Helper (Renderer) (pid 301)"
            .lines()
            .map(str::to_string)
            .collect();
        terminal
            .draw(|f| {
                draw(f, &app);
                draw_dialog(f, app.theme, &body, "Ask them to quit now?", 0);
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn every_size_from_80x24_up_keeps_the_answer_and_the_list() {
        let app = critical_app();
        for (w, h) in [(80, 24), (100, 30), (120, 40), (200, 60)] {
            let screen = render(&app, w, h);
            let first = screen.lines().next().unwrap();
            assert!(first.contains("✗ Memory is critical"), "{w}x{h}: {first}");
            assert!(screen.contains("Apps · largest first"), "{w}x{h}");
            assert!(screen.contains("Google Chrome"), "{w}x{h}");
            assert!(screen.contains("measured"), "{w}x{h}");
            assert!(screen.contains("estimated"), "{w}x{h}");
            let areas = layout(Rect::new(0, 0, w, h), &app);
            assert!(areas.list_rows().height >= 5, "{w}x{h}: {areas:?}");
        }
    }

    #[test]
    fn wrapping_counts_lines() {
        assert_eq!(wrapped_height("one two three", 7), 2);
        assert_eq!(wrapped_height("", 10), 1);
        assert_eq!(truncate("Google Chrome", 6), "Googl…");
        assert_eq!(truncate("Slack", 7), "Slack  ");
    }
}
