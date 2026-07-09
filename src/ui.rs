use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span as RSpan, Text};
use ratatui::widgets::{Block as UiBlock, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::app::{App, Mode};
use crate::doc::{Block, Document, SpanStyle};

fn span_style(kind: &SpanStyle) -> Style {
    match kind {
        SpanStyle::Plain => Style::default(),
        SpanStyle::Bold => Style::default().add_modifier(Modifier::BOLD),
        SpanStyle::Italic => Style::default().add_modifier(Modifier::ITALIC),
        SpanStyle::Superscript => Style::default().fg(Color::DarkGray),
        SpanStyle::Link(_) => Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::UNDERLINED),
    }
}

fn document_to_text(doc: &Document) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(RSpan::styled(
        doc.title.clone(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for block in &doc.blocks {
        match block {
            Block::Heading { level, spans } => {
                lines.push(Line::from(""));
                let style = Style::default().fg(Color::Cyan).add_modifier(
                    Modifier::BOLD
                        | if *level <= 2 {
                            Modifier::UNDERLINED
                        } else {
                            Modifier::empty()
                        },
                );
                let text = spans
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join("");
                lines.push(Line::from(RSpan::styled(text, style)));
            }
            Block::Paragraph(spans) => {
                lines.push(Line::from(spans_to_rspans(spans)));
                lines.push(Line::from(""));
            }
            Block::ListItem {
                ordered,
                index,
                depth,
                spans,
            } => {
                let indent = "  ".repeat(*depth as usize);
                let bullet = if *ordered {
                    format!("{index}.")
                } else {
                    "•".to_string()
                };
                let mut rspans = vec![RSpan::raw(format!("{indent}{bullet} "))];
                rspans.extend(spans_to_rspans(spans));
                lines.push(Line::from(rspans));
            }
            Block::Blockquote(spans) => {
                let mut rspans = vec![RSpan::styled("▌ ", Style::default().fg(Color::DarkGray))];
                rspans.extend(spans_to_rspans(spans));
                lines.push(Line::from(rspans));
                lines.push(Line::from(""));
            }
            Block::Code(text) => {
                for line in text.lines() {
                    lines.push(Line::from(RSpan::styled(
                        format!("    {line}"),
                        Style::default().fg(Color::Green),
                    )));
                }
                lines.push(Line::from(""));
            }
            Block::Rule => {
                lines.push(Line::from(RSpan::styled(
                    "─".repeat(40),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Block::Table(rows) => {
                for row in rows {
                    lines.push(Line::from(RSpan::styled(
                        row.clone(),
                        Style::default().fg(Color::Gray),
                    )));
                }
                lines.push(Line::from(""));
            }
            Block::Infobox(rows) => {
                lines.push(Line::from(RSpan::styled(
                    "┌─ infobox ─────────────────",
                    Style::default().fg(Color::Yellow),
                )));
                for (label, value) in rows {
                    let text = if label.is_empty() {
                        format!("│ {value}")
                    } else {
                        format!("│ {label}: {value}")
                    };
                    lines.push(Line::from(RSpan::styled(
                        text,
                        Style::default().fg(Color::Yellow),
                    )));
                }
                lines.push(Line::from(RSpan::styled(
                    "└───────────────────────────",
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from(""));
            }
            Block::Image(alt) => {
                lines.push(Line::from(RSpan::styled(
                    format!("[image: {alt}]"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )));
                lines.push(Line::from(""));
            }
        }
    }

    Text::from(lines)
}

/// The search-excerpt field arrives with `<span class="searchmatch">` tags
/// wrapping matched terms; the results list is plain-styled for now, so
/// strip them down to plain text.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn spans_to_rspans(spans: &[crate::doc::Span]) -> Vec<RSpan<'static>> {
    spans
        .iter()
        .map(|s| RSpan::styled(s.text.clone(), span_style(&s.style)))
        .collect()
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    match app.mode {
        Mode::Reading | Mode::Help => draw_reading(frame, app, chunks[0]),
        Mode::Search => draw_reading(frame, app, chunks[0]),
        Mode::Results => draw_results(frame, app, chunks[0]),
    }

    draw_status_bar(frame, app, chunks[1]);

    if app.mode == Mode::Help {
        draw_help_overlay(frame, area);
    }
}

fn draw_reading(frame: &mut Frame, app: &mut App, area: Rect) {
    match &app.doc {
        Some(doc) => {
            let text = document_to_text(doc);
            let visible_height = area.height.max(1);
            let total_lines = text.lines.len() as u16;
            app.max_scroll = total_lines.saturating_sub(visible_height);
            app.scroll = app.scroll.min(app.max_scroll);

            let paragraph = Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((app.scroll, 0));
            frame.render_widget(paragraph, area);
        }
        None => {
            let welcome = Text::from(vec![
                Line::from(""),
                Line::from(RSpan::styled(
                    "wikitui",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(RSpan::styled(
                    format!("{}.wikipedia.org", app.lang),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from("Press / to search Wikipedia, ? for help, q to quit."),
            ]);
            frame.render_widget(Paragraph::new(welcome), area);
        }
    }
}

fn draw_results(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut lines = vec![Line::from(RSpan::styled(
                r.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            if let Some(desc) = &r.description {
                lines.push(Line::from(RSpan::styled(
                    desc.clone(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if let Some(excerpt) = &r.excerpt {
                // The API returns highlight markup as HTML <span> tags around
                // matched terms; strip tags here since the results list is
                // plain-styled (FR-SR-2's full "highlighted snippet" styling
                // is future work, see PRD FR-SR-2).
                let plain = strip_tags(excerpt);
                if !plain.is_empty() {
                    lines.push(Line::from(RSpan::styled(
                        plain,
                        Style::default()
                            .fg(Color::Gray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
            }
            let style = if i == app.selected_result {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };
            ListItem::new(lines).style(style)
        })
        .collect();

    let title = format!(
        "Results for \"{}\" ({} found)",
        app.search_input,
        app.results.len()
    );
    let list = List::new(items).block(UiBlock::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let text = match app.mode {
        Mode::Search => format!("/{}", app.search_input),
        Mode::Results => "Enter: open   Esc: cancel   j/k: move".to_string(),
        Mode::Help => "Press any key to close help".to_string(),
        Mode::Reading if app.loading => "Loading…".to_string(),
        Mode::Reading => app.status.clone(),
    };
    let style = if app.mode == Mode::Search {
        Style::default().fg(Color::Black).bg(Color::Yellow)
    } else {
        Style::default().fg(Color::White).bg(Color::DarkGray)
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let width = 60.min(area.width.saturating_sub(4)).max(20);
    let height = 16.min(area.height.saturating_sub(4)).max(8);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let help_text = Text::from(vec![
        Line::from(RSpan::styled(
            "wikitui — help",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("j/k, ↓/↑     scroll"),
        Line::from("Ctrl-d/u     half page down/up"),
        Line::from("gg / G       top / bottom"),
        Line::from("/            search"),
        Line::from("Enter        open selected result"),
        Line::from("Esc          cancel / close"),
        Line::from("?            toggle this help"),
        Line::from("q            quit"),
    ]);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(help_text).block(UiBlock::default().borders(Borders::ALL).title("Help")),
        popup,
    );
}
