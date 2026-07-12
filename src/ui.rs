use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span as RSpan, Text};
use ratatui::widgets::{
    Block as UiBlock, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::app::{App, Mode};
use crate::doc::{Block, Document, LinkRef, SpanStyle};
use crate::theme::Theme;

/// Every article title opened this session — back-stack, forward-stack, and
/// the one currently on screen — used to style already-read links
/// differently from unread ones (PRD FR-HS-2).
fn visited_titles(app: &App) -> HashSet<&str> {
    let mut set: HashSet<&str> = app.back_stack.iter().map(String::as_str).collect();
    set.extend(app.forward_stack.iter().map(String::as_str));
    if let Some(doc) = &app.doc {
        set.insert(doc.title.as_str());
    }
    set
}

/// A single color, respecting `NO_COLOR` (PRD FR-TH-5): when set, every
/// style still carries its modifiers (bold/italic/underline) so meaning
/// isn't lost, just the color.
fn colored(no_color: bool, color: Color) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(color)
    }
}

fn colored_bg(no_color: bool, fg: Color, bg: Color) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(fg).bg(bg)
    }
}

/// The base style for a full-area widget: the theme's background/foreground
/// (or nothing at all for `terminal`, which must inherit the user's
/// palette), skipped entirely under `NO_COLOR`.
fn base_style(theme: &Theme, no_color: bool) -> Style {
    if no_color {
        return Style::default();
    }
    let mut style = Style::default();
    if let Some(bg) = theme.bg {
        style = style.bg(bg);
    }
    if let Some(fg) = theme.fg {
        style = style.fg(fg);
    }
    style
}

fn span_style(kind: &SpanStyle, theme: &Theme, no_color: bool) -> Style {
    match kind {
        SpanStyle::Plain => Style::default(),
        SpanStyle::Bold => Style::default().add_modifier(Modifier::BOLD),
        SpanStyle::Italic => Style::default().add_modifier(Modifier::ITALIC),
        SpanStyle::Superscript => colored(no_color, theme.dim),
        // Unvisited-link fallback; `spans_to_rspans` handles the
        // focused/visited cases itself and never delegates a Link span here.
        SpanStyle::Link(_) => colored(no_color, theme.link).add_modifier(Modifier::UNDERLINED),
    }
}

fn document_to_text(
    doc: &Document,
    focused_link: Option<usize>,
    links: &[LinkRef],
    visited: &HashSet<&str>,
    theme: &Theme,
    no_color: bool,
) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Must advance in the exact same order as `doc::collect_links` (which
    // only visits Paragraph/ListItem/Blockquote spans) so a cycled-to link
    // index highlights the same occurrence the app will actually follow.
    let mut link_counter = 0usize;

    lines.push(Line::from(RSpan::styled(
        doc.title.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for block in &doc.blocks {
        match block {
            Block::Heading { level, spans } => {
                lines.push(Line::from(""));
                let style = colored(no_color, theme.heading).add_modifier(
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
                lines.push(Line::from(spans_to_rspans(
                    spans,
                    &mut link_counter,
                    focused_link,
                    links,
                    visited,
                    None,
                    theme,
                    no_color,
                )));
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
                rspans.extend(spans_to_rspans(
                    spans,
                    &mut link_counter,
                    focused_link,
                    links,
                    visited,
                    None,
                    theme,
                    no_color,
                ));
                lines.push(Line::from(rspans));
            }
            Block::Blockquote(spans) => {
                let mut rspans = vec![RSpan::styled("▌ ", colored(no_color, theme.dim))];
                rspans.extend(spans_to_rspans(
                    spans,
                    &mut link_counter,
                    focused_link,
                    links,
                    visited,
                    Some(theme.quote),
                    theme,
                    no_color,
                ));
                lines.push(Line::from(rspans));
                lines.push(Line::from(""));
            }
            Block::Code(text) => {
                for line in text.lines() {
                    lines.push(Line::from(RSpan::styled(
                        format!("    {line}"),
                        colored(no_color, theme.code),
                    )));
                }
                lines.push(Line::from(""));
            }
            Block::Rule => {
                lines.push(Line::from(RSpan::styled(
                    "─".repeat(40),
                    colored(no_color, theme.dim),
                )));
            }
            Block::Table(rows) => {
                for row in rows {
                    lines.push(Line::from(RSpan::styled(
                        row.clone(),
                        colored(no_color, theme.table),
                    )));
                }
                lines.push(Line::from(""));
            }
            Block::Infobox(rows) => {
                let style = colored(no_color, theme.infobox);
                lines.push(Line::from(RSpan::styled(
                    "┌─ infobox ─────────────────",
                    style,
                )));
                for (label, value) in rows {
                    let text = if label.is_empty() {
                        format!("│ {value}")
                    } else {
                        format!("│ {label}: {value}")
                    };
                    lines.push(Line::from(RSpan::styled(text, style)));
                }
                lines.push(Line::from(RSpan::styled(
                    "└───────────────────────────",
                    style,
                )));
                lines.push(Line::from(""));
            }
            Block::Image(alt) => {
                let suffix = if theme.images {
                    ""
                } else {
                    " (images off in this theme)"
                };
                lines.push(Line::from(RSpan::styled(
                    format!("[image: {alt}{suffix}]"),
                    colored(no_color, theme.image).add_modifier(Modifier::ITALIC),
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

/// Converts spans to styled ratatui spans. `focused` highlights the
/// Tab/Shift-Tab-selected link (indexed the same way as `doc::collect_links`,
/// hence needing the parallel `links` list to look up whether a given
/// occurrence's target has been visited this session). `plain_color`
/// overrides `SpanStyle::Plain`'s color for contexts with their own body
/// color (a blockquote's `theme.quote`); pass `None` for ordinary body text.
#[allow(clippy::too_many_arguments)]
fn spans_to_rspans(
    spans: &[crate::doc::Span],
    link_counter: &mut usize,
    focused: Option<usize>,
    links: &[LinkRef],
    visited: &HashSet<&str>,
    plain_color: Option<Color>,
    theme: &Theme,
    no_color: bool,
) -> Vec<RSpan<'static>> {
    spans
        .iter()
        .map(|s| match &s.style {
            SpanStyle::Link(_) => {
                let this_index = *link_counter;
                *link_counter += 1;
                let style = if Some(this_index) == focused {
                    colored_bg(no_color, theme.focus_fg, theme.focus_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    let is_visited = links
                        .get(this_index)
                        .and_then(|l| l.internal_title.as_deref())
                        .is_some_and(|title| visited.contains(title));
                    let color = if is_visited {
                        theme.link_visited
                    } else {
                        theme.link
                    };
                    colored(no_color, color).add_modifier(Modifier::UNDERLINED)
                };
                RSpan::styled(s.text.clone(), style)
            }
            SpanStyle::Plain if plain_color.is_some() => {
                RSpan::styled(s.text.clone(), colored(no_color, plain_color.unwrap()))
            }
            other => RSpan::styled(s.text.clone(), span_style(other, theme, no_color)),
        })
        .collect()
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    // Paint the whole frame in the theme's background/foreground first so
    // areas a widget doesn't explicitly style (e.g. the empty tail of a
    // short article) still match the theme, not the terminal default.
    frame.render_widget(
        UiBlock::default().style(base_style(&app.theme, app.no_color)),
        area,
    );

    match app.mode {
        Mode::Reading | Mode::Help | Mode::Search | Mode::Find | Mode::Command => {
            draw_reading(frame, app, chunks[0])
        }
        Mode::Results => draw_results(frame, app, chunks[0]),
        Mode::Toc => draw_toc(frame, app, chunks[0]),
        Mode::Research => draw_research(frame, app, chunks[0]),
        Mode::Library => draw_library(frame, app, chunks[0]),
    }

    draw_status_bar(frame, app, chunks[1]);

    if app.mode == Mode::Help {
        draw_help_overlay(frame, app, area);
    }
}

fn draw_reading(frame: &mut Frame, app: &mut App, area: Rect) {
    match &app.doc {
        Some(doc) => {
            let visited = visited_titles(app);
            let text = document_to_text(
                doc,
                app.focused_link,
                &app.links,
                &visited,
                &app.theme,
                app.no_color,
            );
            let visible_height = area.height.max(1);
            let total_lines = text.lines.len() as u16;
            app.max_scroll = total_lines.saturating_sub(visible_height);
            app.scroll = app.scroll.min(app.max_scroll);

            let paragraph = Paragraph::new(text)
                .style(base_style(&app.theme, app.no_color))
                .wrap(Wrap { trim: false })
                .scroll((app.scroll, 0));
            frame.render_widget(paragraph, area);
        }
        None => {
            let welcome = Text::from(vec![
                Line::from(""),
                Line::from(RSpan::styled(
                    "wikitui",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(RSpan::styled(
                    format!("{}.wikipedia.org — theme: {}", app.lang, app.theme.name),
                    colored(app.no_color, app.theme.dim),
                )),
                Line::from(""),
                Line::from(
                    "Press / to search Wikipedia, T to cycle themes, ? for help, q to quit.",
                ),
            ]);
            frame.render_widget(
                Paragraph::new(welcome).style(base_style(&app.theme, app.no_color)),
                area,
            );
        }
    }
}

/// Renders a list statefully with the given index selected, so ratatui
/// scrolls the list to keep the selection visible. A plain `render_widget`
/// on a `List` always shows the first page — on long lists j/k would move
/// the selection out of view, which for the library made `d` a blind
/// destructive action.
fn render_selectable_list(frame: &mut Frame, list: List, area: Rect, selected: usize) {
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
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
                    colored(app.no_color, app.theme.dim),
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
                        colored(app.no_color, app.theme.dim).add_modifier(Modifier::ITALIC),
                    )));
                }
            }
            let style = if i == app.selected_result {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
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
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_result);
}

fn draw_toc(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .sections
        .iter()
        .enumerate()
        .map(|(i, section)| {
            // Level 2 is the top-level "== Heading ==" tier; deeper levels
            // get progressively indented.
            let indent = "  ".repeat(section.level.saturating_sub(2) as usize);
            let style = if i == app.selected_section {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(format!("{indent}{}", section.title))).style(style)
        })
        .collect();

    let title = format!("Table of contents ({} sections)", app.sections.len());
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_section);
}

fn draw_research(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .citations
        .iter()
        .enumerate()
        .map(|(i, citation)| {
            // Element 0 is the synthetic self-citation (id "self", set by
            // research::self_citation); everything else is a real
            // footnote anchor from the article's References section, shown
            // so it can be cross-referenced against the live page.
            let label = if citation.id == "self" {
                "[this article]".to_string()
            } else {
                format!("[{}] {}", i, citation.id)
            };
            let saved_marker = if app.is_citation_saved(i) {
                "✓ "
            } else {
                "  "
            };
            let mut lines = vec![Line::from(RSpan::styled(
                format!("{saved_marker}{label} — {}", citation.text),
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            if let Some(url) = &citation.url {
                lines.push(Line::from(RSpan::styled(
                    format!("      {url}"),
                    colored(app.no_color, app.theme.dim),
                )));
            }
            let style = if i == app.selected_citation {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(lines).style(style)
        })
        .collect();

    let title = format!(
        "Research — {} citations here, {} saved overall",
        app.citations.len(),
        app.research.citations.len()
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_citation);
}

/// The full saved bibliography, every entry previewed live in the current
/// citation style (`s` cycles styles, so what you see is exactly what `e`
/// exports).
fn draw_library(frame: &mut Frame, app: &App, area: Rect) {
    // List items don't soft-wrap, and formatted citations (URLs included)
    // routinely exceed the terminal width — wrap them ourselves so the
    // preview really is what exports, not a truncation of it.
    let wrap_width = area.width.saturating_sub(4).max(20) as usize;
    let items: Vec<ListItem> = app
        .research
        .citations
        .iter()
        .enumerate()
        .map(|(i, saved)| {
            let formatted = crate::cite::format_citation(saved, app.cite_style);
            let mut lines: Vec<Line> = textwrap::wrap(&formatted, wrap_width)
                .into_iter()
                .map(|piece| Line::from(RSpan::raw(piece.into_owned())))
                .collect();
            lines.push(Line::from(RSpan::styled(
                format!(
                    "      saved {} while reading \"{}\"",
                    saved.saved_at, saved.source_article
                ),
                colored(app.no_color, app.theme.dim),
            )));
            let style = if i == app.selected_library {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(lines).style(style)
        })
        .collect();

    let title = format!(
        "Library — {} saved citations, style: {}",
        app.research.citations.len(),
        app.cite_style.label()
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_library);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let text = match app.mode {
        Mode::Search => format!("/{}", app.search_input),
        Mode::Command => format!(":{}", app.command_input),
        Mode::Find if app.find_matches.is_empty() && !app.find_input.is_empty() => {
            format!("find: {} (no matches)", app.find_input)
        }
        Mode::Find if !app.find_matches.is_empty() => {
            format!(
                "find: {} ({}/{})",
                app.find_input,
                app.find_index + 1,
                app.find_matches.len()
            )
        }
        Mode::Find => format!("find: {}", app.find_input),
        Mode::Results => "Enter: open   Esc: cancel   j/k: move".to_string(),
        Mode::Toc => "Enter: jump to section   Esc: cancel   j/k: move".to_string(),
        Mode::Research => "Enter/s: save citation   R: library   Esc: done   j/k: move".to_string(),
        // The library's status line carries transient action feedback
        // (delete/export/style outcomes overwrite it) — see open_library.
        Mode::Library => app.status.clone(),
        Mode::Help => "Press any key to close help".to_string(),
        Mode::Reading if app.loading => "Loading…".to_string(),
        // Command feedback outranks the focused-link line until the next
        // keypress clears it (see App::notice).
        Mode::Reading if app.notice.is_some() => app.notice.clone().unwrap_or_default(),
        Mode::Reading if !app.find_matches.is_empty() => {
            format!(
                "match {}/{} for \"{}\"   n/N: cycle   Esc: clear",
                app.find_index + 1,
                app.find_matches.len(),
                app.find_input
            )
        }
        // The focused-link line takes over the status bar on any page with
        // links (nearly all of them), so the page-source indicator must
        // prefix it too or ◐/○ would never actually be seen (app.status
        // already carries the prefix via set_document).
        Mode::Reading => match app.focused_link.and_then(|i| app.links.get(i)) {
            Some(link) if link.internal_title.is_some() => {
                format!(
                    "{}→ {} ({}/{})   Tab/S-Tab: cycle   Enter: open   H: back   L: forward",
                    app.page_source.prefix(),
                    link.text,
                    app.focused_link.unwrap() + 1,
                    app.links.len()
                )
            }
            Some(link) => format!(
                "{}→ {} (external, not yet followable)",
                app.page_source.prefix(),
                link.text
            ),
            None => app.status.clone(),
        },
    };
    let style = if matches!(app.mode, Mode::Search | Mode::Find | Mode::Command) {
        colored_bg(app.no_color, app.theme.focus_fg, app.theme.focus_bg)
    } else {
        colored_bg(app.no_color, app.theme.status_fg, app.theme.status_bg)
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn draw_help_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let width = 60.min(area.width.saturating_sub(4)).max(20);
    let height = 21.min(area.height.saturating_sub(4)).max(8);
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
        Line::from("Tab/S-Tab    cycle links"),
        Line::from("Enter        follow link / open selected result"),
        Line::from("H / L        back / forward"),
        Line::from("t            table of contents"),
        Line::from("T            cycle color theme"),
        Line::from("y / Y        yank URL / Markdown link"),
        Line::from(":            command line (:open, :lang, :theme, :export, :q)"),
        Line::from("r            research mode: cite this page & its sources"),
        Line::from("R            library: browse/export saved bibliography"),
        Line::from("/            search Wikipedia"),
        Line::from("Ctrl-f       find in this page, n/N: cycle matches"),
        Line::from("Esc          cancel / close"),
        Line::from("?            toggle this help"),
        Line::from("q            quit"),
    ]);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(help_text)
            .style(base_style(&app.theme, app.no_color))
            .block(UiBlock::default().borders(Borders::ALL).title("Help")),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{parse_article_html, section_outline};

    /// Exercises every block variant that carries a line count in
    /// `doc::block_line_count` (heading, paragraph, list item, blockquote,
    /// code, rule, table, infobox, image), so a mismatch between that
    /// function and this module's actual line emission would show up here.
    const FIXTURE: &str = r##"
    <html><body>
    <table class="infobox"><tbody>
      <tr><th colspan="2">Subject</th></tr>
      <tr><th>Field</th><td>Testing</td></tr>
    </tbody></table>
    <p>Intro paragraph.</p>
    <h2>First Section</h2>
    <p>Some text.</p>
    <ul><li>One</li><li>Two</li></ul>
    <blockquote><p>A quote.</p></blockquote>
    <pre>line one
line two</pre>
    <hr/>
    <table class="wikitable"><tbody>
      <tr><th>A</th><th>B</th></tr>
      <tr><td>1</td><td>2</td></tr>
    </tbody></table>
    <figure><img src="x.jpg" alt="An image"/></figure>
    <h2>Second Section</h2>
    <p>More text.</p>
    </body></html>
    "##;

    /// `doc::section_outline` computes each heading's line index without
    /// ever building a ratatui `Text` — it must agree with what
    /// `document_to_text` actually renders, or "jump to section" would land
    /// on the wrong line. This proves the two independent implementations
    /// stay in sync as block types are added or their rendering changes.
    #[test]
    fn section_outline_lines_match_the_rendered_heading_lines() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let sections = section_outline(&doc);
        assert_eq!(sections.len(), 2, "fixture has exactly two headings");

        let theme = Theme::terminal();
        let text = document_to_text(&doc, None, &[], &HashSet::new(), &theme, false);

        for section in &sections {
            let rendered_line = &text.lines[section.line as usize];
            let rendered_text: String = rendered_line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            assert_eq!(
                rendered_text, section.title,
                "section_outline's line {} for {:?} doesn't match the rendered line",
                section.line, section.title
            );
        }
    }

    #[test]
    fn no_color_strips_fg_and_bg_but_keeps_modifiers() {
        let theme = Theme::full();
        let style = colored_bg(true, theme.focus_fg, theme.focus_bg).add_modifier(Modifier::BOLD);
        assert_eq!(style.fg, None, "NO_COLOR must not set a foreground color");
        assert_eq!(style.bg, None, "NO_COLOR must not set a background color");
        assert!(
            style.add_modifier.contains(Modifier::BOLD),
            "NO_COLOR must still preserve modifiers"
        );
    }

    #[test]
    fn terminal_theme_base_style_sets_nothing() {
        let theme = Theme::terminal();
        let style = base_style(&theme, false);
        assert_eq!(style.bg, None);
        assert_eq!(style.fg, None);
    }

    #[test]
    fn full_theme_base_style_sets_both() {
        let theme = Theme::full();
        let style = base_style(&theme, false);
        assert_eq!(style.bg, theme.bg);
        assert_eq!(style.fg, theme.fg);
    }

    /// A link to an article already visited this session (PRD FR-HS-2)
    /// renders in `theme.link_visited`; an unvisited one stays `theme.link`
    /// — proving `visited_titles`'s output actually reaches the renderer,
    /// not just that the two colors differ in the abstract.
    #[test]
    fn visited_link_gets_the_visited_color_unvisited_does_not() {
        let html = r##"<html><body><p>See <a href="./Visited_Page">Visited Page</a> and
            <a href="./Unvisited_Page">Unvisited Page</a>.</p></body></html>"##;
        let doc = parse_article_html("Test Article", html);
        let links = crate::doc::collect_links(&doc);
        assert_eq!(links.len(), 2);

        let mut visited = HashSet::new();
        visited.insert("Visited Page");

        let theme = Theme::full();
        let text = document_to_text(&doc, None, &links, &visited, &theme, false);

        let paragraph_line = text
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("Visited Page")))
            .expect("paragraph line present");

        let visited_span = paragraph_line
            .spans
            .iter()
            .find(|s| s.content.contains("Visited Page"))
            .unwrap();
        let unvisited_span = paragraph_line
            .spans
            .iter()
            .find(|s| s.content.contains("Unvisited Page"))
            .unwrap();

        assert_eq!(visited_span.style.fg, Some(theme.link_visited));
        assert_eq!(unvisited_span.style.fg, Some(theme.link));
        assert_ne!(
            theme.link_visited, theme.link,
            "the two colors must actually differ for this test to mean anything"
        );
    }
}
