use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout as UiLayout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span as RSpan, Text};
use ratatui::widgets::{Block as UiBlock, Borders, Clear, List, ListItem, ListState, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{App, Mode};
use crate::doc::LinkRef;
use crate::layout::{
    FLOOR_MIN_HEIGHT, FLOOR_MIN_WIDTH, LaidLine, MatchSpan, SizeTier, SpanKind, size_tier,
};
use crate::startpage::{self, StartPageConfig, StartPageModel};
use crate::theme::Theme;

/// Every title that should render as "visited" in the active tab (PRD
/// FR-HS-2): this session's own back-stack, forward-stack, and the article
/// currently on screen — so a page opened this session but not yet
/// committed to the persistent history (or opened while incognito) still
/// shows visited — **plus** every `(lang, title)` the persistent,
/// cross-session `history::History` store has ever recorded for this tab's
/// language, so a link stays visited across restarts too. The persistent
/// half is a single `HashMap` lookup (`History::visited_titles_for_lang`),
/// not a query — see that method's doc comment for why: this runs once per
/// draw, but the caller checks membership once per visible link, and a
/// disk hit per link per frame is exactly what the in-memory cache exists
/// to avoid.
fn visited_titles(app: &App) -> HashSet<&str> {
    let tab = app.active_tab();
    let mut set: HashSet<&str> = tab.back_stack.iter().map(|e| e.title.as_str()).collect();
    set.extend(tab.forward_stack.iter().map(|e| e.title.as_str()));
    if let Some(doc) = &tab.doc {
        set.insert(doc.title.as_str());
    }
    if let Some(titles) = app.history.visited_titles_for_lang(&tab.lang) {
        set.extend(titles.iter().map(String::as_str));
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

/// The opaque RGB the half-block renderer composites image alpha over (PRD
/// FR-RD-8): the theme's background. Themes with inline images set an RGB bg
/// (`full` #101418, `paper` cream); anything without a concrete RGB bg falls
/// back to black, the safe assumption for a dark terminal.
fn theme_bg_rgb(theme: &Theme) -> (u8, u8, u8) {
    match theme.bg {
        Some(Color::Rgb(r, g, b)) => (r, g, b),
        _ => (0, 0, 0),
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

/// Map a semantic span kind (from the width-aware layout) to a concrete
/// `ratatui` style, applying the theme, focus state, and visited state at
/// paint time — the layout itself is theme-independent (PRD §6.3), so this is
/// the only place colors enter and a theme/focus change is O(paint).
fn kind_style(
    kind: &SpanKind,
    focused_link: Option<usize>,
    links: &[LinkRef],
    visited: &HashSet<&str>,
    theme: &Theme,
    no_color: bool,
) -> Style {
    match kind {
        SpanKind::Plain => Style::default(),
        SpanKind::Bold => Style::default().add_modifier(Modifier::BOLD),
        SpanKind::Italic => Style::default().add_modifier(Modifier::ITALIC),
        SpanKind::Dim => colored(no_color, theme.dim),
        SpanKind::Title => Style::default().add_modifier(Modifier::BOLD),
        SpanKind::Heading(level) => colored(no_color, theme.heading).add_modifier(
            Modifier::BOLD
                | if *level <= 2 {
                    Modifier::UNDERLINED
                } else {
                    Modifier::empty()
                },
        ),
        SpanKind::Quote => colored(no_color, theme.quote),
        SpanKind::Code => colored(no_color, theme.code),
        SpanKind::Table => colored(no_color, theme.table),
        SpanKind::Infobox => colored(no_color, theme.infobox),
        SpanKind::Image => colored(no_color, theme.image).add_modifier(Modifier::ITALIC),
        // A caption under an image/gallery: dim italics (PRD FR-RD-8).
        SpanKind::Caption => colored(no_color, theme.dim).add_modifier(Modifier::ITALIC),
        // An image-box row is replaced by half-block cells in `paint_line`
        // before this ever renders it; a plain style is the safe fallback for
        // the window where the decoded pixels aren't reachable.
        SpanKind::ImageRow { .. } => Style::default(),
        SpanKind::Link(occ) => {
            if Some(*occ) == focused_link {
                colored_bg(no_color, theme.focus_fg, theme.focus_bg).add_modifier(Modifier::BOLD)
            } else {
                let is_visited = links
                    .get(*occ)
                    .and_then(|l| l.internal_title.as_deref())
                    .is_some_and(|title| visited.contains(title));
                let color = if is_visited {
                    theme.link_visited
                } else {
                    theme.link
                };
                colored(no_color, color).add_modifier(Modifier::UNDERLINED)
            }
        }
        // PRD FR-NV-1: the focus colors, swapped, so a hint label reads as
        // visually distinct from the Tab-cycled focused-link highlight
        // (`colored_bg(no_color, theme.focus_fg, theme.focus_bg)` above) even
        // on the same link.
        SpanKind::Hint => {
            colored_bg(no_color, theme.focus_bg, theme.focus_fg).add_modifier(Modifier::BOLD)
        }
    }
}

/// The in-page-find highlight style (PRD FR-NV-6b): the theme's `match`
/// slot, bold so it reads at a glance against any other semantic color it
/// overrides (`Style::patch`'d on top of the span's own `kind_style`).
fn match_style(theme: &Theme, no_color: bool) -> Style {
    colored(no_color, theme.match_fg).add_modifier(Modifier::BOLD)
}

/// Paints one laid-out line, splicing in find-match highlighting where
/// `matches` (ascending, non-overlapping grapheme-column ranges local to
/// this line) says to. `is_current` marks exactly one of those ranges (the
/// `n`/`N` cursor) for extra emphasis — a closure rather than a plain value
/// so the caller can compare `(line, range)` instead of `range` alone,
/// since two different lines can easily share an identical column range
/// (e.g. every row of a list starting at the same indent).
#[allow(clippy::too_many_arguments)]
fn paint_line(
    line: &LaidLine,
    focused_link: Option<usize>,
    links: &[LinkRef],
    visited: &HashSet<&str>,
    theme: &Theme,
    no_color: bool,
    matches: &[MatchSpan],
    is_current: &dyn Fn(MatchSpan) -> bool,
    image_store: &crate::image::ImageStore,
) -> Line<'static> {
    let mut spans: Vec<RSpan<'static>> = Vec::new();
    let mut col = 0usize; // grapheme column from the start of the line
    let mut mi = 0usize; // index into `matches`, advances monotonically

    for s in &line.spans {
        // An inline-image box row (PRD FR-RD-8): replace the reserved blank
        // span with `cols` upper-half-block (`▀`) cells whose fg/bg come from
        // the decoded pixels. Image rows carry no find matches (they are
        // blank spaces at layout time), so they bypass the match splicing.
        if let SpanKind::ImageRow { src, row, rows } = &s.kind {
            let width = s.text.graphemes(true).count();
            if let Some(img) = image_store.ready(src) {
                let bg = theme_bg_rgb(theme);
                for cell in crate::image::half_block_row(img, width as u16, *rows, *row, bg) {
                    spans.push(RSpan::styled(
                        crate::image::HALF_BLOCK,
                        Style::default()
                            .fg(Color::Rgb(cell.fg.0, cell.fg.1, cell.fg.2))
                            .bg(Color::Rgb(cell.bg.0, cell.bg.1, cell.bg.2)),
                    ));
                }
            } else {
                // The decode isn't reachable (raced with a store change):
                // keep the row's width as blanks rather than shifting layout.
                spans.push(RSpan::raw(s.text.clone()));
            }
            col += width;
            continue;
        }
        let base_style = kind_style(&s.kind, focused_link, links, visited, theme, no_color);
        let graphemes: Vec<&str> = s.text.graphemes(true).collect();
        let mut cursor = 0usize;
        while cursor < graphemes.len() {
            let abs = col + cursor;
            while mi < matches.len() && matches[mi].end <= abs {
                mi += 1;
            }
            if mi < matches.len() && matches[mi].start <= abs {
                let end_in_span = (matches[mi].end - col).min(graphemes.len());
                let text: String = graphemes[cursor..end_in_span].concat();
                let mut style = base_style.patch(match_style(theme, no_color));
                if is_current(matches[mi]) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                spans.push(RSpan::styled(text, style));
                cursor = end_in_span;
            } else {
                let next_start = if mi < matches.len() {
                    (matches[mi].start - col).min(graphemes.len())
                } else {
                    graphemes.len()
                };
                let text: String = graphemes[cursor..next_start].concat();
                spans.push(RSpan::styled(text, base_style));
                cursor = next_start;
            }
        }
        col += graphemes.len();
    }
    // Image placeholders note whether the active theme would actually render
    // the image; kept out of the (theme-independent) layout and applied here.
    if !theme.images && line.spans.iter().any(|s| matches!(s.kind, SpanKind::Image)) {
        spans.push(RSpan::styled(
            " (images off in this theme)".to_string(),
            colored(no_color, theme.image).add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
}

/// Paint laid-out lines into styled text — one laid line per row, no
/// runtime wrapping (the layout already broke lines to width). Takes the
/// line slice directly, rather than a whole `Layout`, so hint mode (PRD
/// FR-NV-1) can feed it a hint-overlaid copy of `Layout::lines`
/// (`hints::overlay_hint_labels`) without this function needing to know
/// hints exist at all.
///
/// `find_occurrences` is `App::find_occurrences` verbatim — one entry per
/// query occurrence, in document order, each carrying one or more `(line,
/// range)` pieces (more than one only when a wrap split it) — and
/// `find_index` is `App::find_index`. Every piece of the occurrence `n`/`N`
/// last landed on gets the current-match emphasis, not just its first
/// piece, so a match straddling a wrap reads as one highlighted unit
/// (PRD FR-NV-6b); every other occurrence gets just the plain highlight.
#[allow(clippy::too_many_arguments)]
fn paint_document(
    lines: &[LaidLine],
    focused_link: Option<usize>,
    links: &[LinkRef],
    visited: &HashSet<&str>,
    theme: &Theme,
    no_color: bool,
    find_occurrences: &[crate::layout::Occurrence],
    find_index: usize,
    image_store: &crate::image::ImageStore,
) -> Text<'static> {
    let mut per_line: Vec<Vec<MatchSpan>> = vec![Vec::new(); lines.len()];
    for occurrence in find_occurrences {
        for &(line, range) in &occurrence.pieces {
            if let Some(slot) = per_line.get_mut(line) {
                slot.push(range);
            }
        }
    }
    let current_pieces: &[(usize, MatchSpan)] = find_occurrences
        .get(find_index)
        .map(|occ| occ.pieces.as_slice())
        .unwrap_or(&[]);

    Text::from(
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let is_current = |m: MatchSpan| current_pieces.contains(&(i, m));
                paint_line(
                    l,
                    focused_link,
                    links,
                    visited,
                    theme,
                    no_color,
                    &per_line[i],
                    &is_current,
                    image_store,
                )
            })
            .collect::<Vec<_>>(),
    )
}

/// Parses the REST search API's highlighted excerpt markup (PRD §6.2 rule
/// 5 / Appendix A: `<span class="searchmatch">…</span>` wraps each matched
/// term) into plain-text runs tagged with whether they're inside a match,
/// for `draw_results` to paint in `theme.match_fg` (FR-SR-2) instead of the
/// old plain-strip behavior this replaces. Any other markup the API might
/// emit is dropped like plain HTML, matching that old behavior.
///
/// Hostile-markup safe: nesting is tracked with a depth counter rather than
/// a boolean, so `<span class="searchmatch">a<span class="searchmatch">b</span>c</span>`
/// stays highlighted throughout instead of dropping out after the inner
/// close; an unclosed opening span highlights to the end of the string
/// instead of losing the rest; a stray closing tag with no opener
/// saturates at depth zero instead of underflowing. Never panics on
/// malformed input.
pub fn parse_searchmatch(html: &str) -> Vec<(String, bool)> {
    const OPEN: &str = "<span class=\"searchmatch\">";
    const CLOSE: &str = "</span>";
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut depth: u32 = 0;
    let mut current = String::new();
    let mut rest = html;

    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix(OPEN) {
            if !current.is_empty() {
                out.push((std::mem::take(&mut current), depth > 0));
            }
            depth += 1;
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix(CLOSE) {
            if !current.is_empty() {
                out.push((std::mem::take(&mut current), depth > 0));
            }
            depth = depth.saturating_sub(1);
            rest = tail;
        } else if rest.starts_with('<') {
            // Any other tag, or a lone unmatched '<': drop through the next
            // '>', or the rest of the string if it's never terminated.
            match rest.find('>') {
                Some(i) => rest = &rest[i + 1..],
                None => break,
            }
        } else {
            let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
            current.push_str(&rest[..ch_len]);
            rest = &rest[ch_len..];
        }
    }
    if !current.is_empty() {
        out.push((current, depth > 0));
    }
    out
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // PRD §6.3 hard floor: below 60×16 there isn't room to read an article at
    // all, so show a dedicated "terminal too small" screen instead of a
    // mangled layout. Checked first, before any tab-bar/content split.
    if size_tier(area.width, area.height) == SizeTier::Floor {
        draw_too_small(frame, app, area);
        return;
    }

    // The tab bar (PRD FR-TB-1) is one row above the content, shown only when
    // more than one tab is open — a single tab keeps the current zero-chrome
    // look exactly.
    let show_tab_bar = app.tabs.len() > 1;
    let chunks = if show_tab_bar {
        UiLayout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area)
    } else {
        UiLayout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area)
    };
    let (tab_bar_area, content_area, status_area) = if show_tab_bar {
        (Some(chunks[0]), chunks[1], chunks[2])
    } else {
        (None, chunks[0], chunks[1])
    };

    // Paint the whole frame in the theme's background/foreground first so
    // areas a widget doesn't explicitly style (e.g. the empty tail of a
    // short article) still match the theme, not the terminal default.
    frame.render_widget(
        UiBlock::default().style(base_style(&app.theme, app.no_color)),
        area,
    );

    if let Some(bar) = tab_bar_area {
        draw_tab_bar(frame, app, bar);
    }

    match app.mode {
        Mode::Reading | Mode::Help | Mode::Search | Mode::Find | Mode::Command | Mode::Hint => {
            draw_reading(frame, app, content_area)
        }
        Mode::Results => draw_results(frame, app, content_area),
        Mode::Toc => draw_toc(frame, app, content_area),
        Mode::Research => draw_research(frame, app, content_area),
        Mode::Library => draw_library(frame, app, content_area),
        Mode::TabPicker => draw_tab_picker(frame, app, content_area),
        Mode::HistoryPicker => draw_history_picker(frame, app, content_area),
        // The `/` filter and `t` tag editor draw over the same picker view;
        // only the status line changes to show the live prompt.
        Mode::BookmarkPicker | Mode::BookmarkFilter | Mode::BookmarkTagEdit => {
            draw_bookmark_picker(frame, app, content_area)
        }
        Mode::ReadLaterPicker => draw_readlater_picker(frame, app, content_area),
        Mode::ReadingHistory | Mode::ReadingHistoryFilter => {
            draw_reading_history_picker(frame, app, content_area)
        }
        Mode::SavedPicker => draw_saved_picker(frame, app, content_area),
        Mode::PrefetchLog => draw_prefetch_log(frame, app, content_area),
        // The offline card overlays the reading view (drawn after the status
        // bar below, like the help overlay).
        Mode::OfflineCard => draw_reading(frame, app, content_area),
        Mode::OnThisDay => draw_on_this_day(frame, app, content_area),
    }

    draw_status_bar(frame, app, status_area);

    if app.mode == Mode::OfflineCard {
        draw_offline_card(frame, app, area);
    }

    // The typeahead dropdown floats over the reading view, anchored just
    // above the prompt it belongs to (PRD FR-SR-1) — drawn after the status
    // bar so it layers on top, same ordering as the help overlay below.
    if app.mode == Mode::Search && !app.typeahead.is_empty() {
        draw_search_suggestions(frame, app, content_area);
    }

    if app.mode == Mode::Help {
        draw_help_overlay(frame, app, area);
    }
}

/// PRD §6.3's hard-floor screen: the terminal is below 60×16, so instead of
/// the article we show an honest "too small" message with the required size
/// and the current one, plus the `--dump` escape hatch (which needs no
/// minimum size at all — FR-RD-12).
fn draw_too_small(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        UiBlock::default().style(base_style(&app.theme, app.no_color)),
        area,
    );
    let text = Text::from(vec![
        Line::from(RSpan::styled(
            "Terminal too small",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "wikitui needs at least {FLOOR_MIN_WIDTH}×{FLOOR_MIN_HEIGHT} (now {}×{}).",
            area.width, area.height
        )),
        Line::from("Resize the window — or run `wikitui --dump <title>`."),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .style(base_style(&app.theme, app.no_color))
            .alignment(ratatui::layout::Alignment::Center),
        area,
    );
}

/// One tab's data for the pure bar/picker builders (kept free of `ratatui`
/// types so the overflow logic is table-testable).
pub struct TabLabel {
    /// 1-based display index.
    pub number: usize,
    pub title: String,
    pub loading: bool,
    pub active: bool,
}

/// One painted run of the tab bar: its text and whether it is the active tab
/// (which the painter highlights with the theme's selected slots).
#[derive(Debug, PartialEq, Eq)]
pub struct TabBarSegment {
    pub text: String,
    pub active: bool,
}

fn tab_labels(app: &App) -> Vec<TabLabel> {
    app.tabs
        .iter()
        .enumerate()
        .map(|(i, t)| TabLabel {
            number: i + 1,
            title: t.display_title(),
            loading: t.loading,
            active: i == app.active,
        })
        .collect()
}

/// Display width of a string, EAW-narrow (the tab bar and breadcrumb are
/// chrome, not article body, so the article's ambiguous-width setting doesn't
/// apply here).
fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(s)
}

fn take_width_prefix(s: &str, budget: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = UnicodeWidthStr::width(g);
        if w + gw > budget {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out
}

fn take_width_suffix(s: &str, budget: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true).collect::<Vec<_>>().into_iter().rev() {
        let gw = UnicodeWidthStr::width(g);
        if w + gw > budget {
            break;
        }
        out.insert_str(0, g);
        w += gw;
    }
    out
}

/// Middle-truncate `s` to at most `max` display columns, inserting an ellipsis
/// (PRD FR-TB-1 "middle-truncated titles"). Grapheme- and width-aware so CJK
/// titles never split a cell.
pub fn middle_truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let budget = max - 1; // room for the ellipsis
    let head_budget = budget.div_ceil(2);
    let tail_budget = budget - head_budget;
    format!(
        "{}…{}",
        take_width_prefix(s, head_budget),
        take_width_suffix(s, tail_budget)
    )
}

fn render_tab_segment(label: &TabLabel, max_title: usize) -> String {
    let title = middle_truncate(&label.title, max_title);
    // Loading tabs get a static "…" (PRD FR-ACS-2/FR-ACS-4: no animation).
    if label.loading {
        format!(" {}:{} … ", label.number, title)
    } else {
        format!(" {}:{} ", label.number, title)
    }
}

/// Build the tab-bar segments (PRD FR-TB-1). When every numbered title fits in
/// `width`, all are rendered. On overflow the active tab collapses to the
/// compact `[3/17] Alan Turing` indicator and as many neighbors as fit are
/// added outward from it, then everything is ordered left-to-right by index.
pub fn build_tab_bar(labels: &[TabLabel], width: usize) -> Vec<TabBarSegment> {
    if labels.is_empty() {
        return Vec::new();
    }
    let full: Vec<String> = labels.iter().map(|l| render_tab_segment(l, 24)).collect();
    let total: usize = full.iter().map(|s| display_width(s)).sum();
    if total <= width {
        return labels
            .iter()
            .zip(full)
            .map(|(l, text)| TabBarSegment {
                text,
                active: l.active,
            })
            .collect();
    }

    let n = labels.len();
    let active_idx = labels.iter().position(|l| l.active).unwrap_or(0);
    let active = &labels[active_idx];
    let compact = {
        // The compact indicator must itself fit the bar, so its title budget
        // is bounded by the width left after the `[a/n]` chrome, not a fixed
        // cap — otherwise a very long active title would overflow a narrow bar.
        let head = format!(" [{}/{}] ", active_idx + 1, n);
        let marker = if active.loading { " …" } else { "" };
        let chrome = display_width(&head) + display_width(marker) + 1;
        let title_budget = width.saturating_sub(chrome).clamp(1, 24);
        let title = middle_truncate(&active.title, title_budget);
        format!("{head}{title}{marker} ")
    };
    let mut used = display_width(&compact);
    let mut chosen: Vec<(usize, TabBarSegment)> = vec![(
        active_idx,
        TabBarSegment {
            text: compact,
            active: true,
        },
    )];

    // Grow outward, right then left, adding a neighbor only if it still fits.
    let mut next_right = (active_idx + 1 < n).then_some(active_idx + 1);
    let mut next_left = active_idx.checked_sub(1);
    loop {
        let mut added = false;
        if let Some(r) = next_right {
            let seg = render_tab_segment(&labels[r], 16);
            if used + display_width(&seg) <= width {
                used += display_width(&seg);
                chosen.push((
                    r,
                    TabBarSegment {
                        text: seg,
                        active: false,
                    },
                ));
                next_right = (r + 1 < n).then_some(r + 1);
                added = true;
            } else {
                next_right = None;
            }
        }
        if let Some(l) = next_left {
            let seg = render_tab_segment(&labels[l], 16);
            if used + display_width(&seg) <= width {
                used += display_width(&seg);
                chosen.push((
                    l,
                    TabBarSegment {
                        text: seg,
                        active: false,
                    },
                ));
                next_left = l.checked_sub(1);
                added = true;
            } else {
                next_left = None;
            }
        }
        if !added {
            break;
        }
    }

    chosen.sort_by_key(|(i, _)| *i);
    chosen.into_iter().map(|(_, seg)| seg).collect()
}

/// Build the active tab's breadcrumb string (PRD FR-NV-7): the trail titles
/// joined with " → ", middle-truncated to `width`.
pub fn build_breadcrumb(titles: &[String], width: usize) -> String {
    if titles.is_empty() {
        return String::new();
    }
    middle_truncate(&titles.join(" → "), width)
}

fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let labels = tab_labels(app);
    let segments = build_tab_bar(&labels, area.width as usize);
    let spans: Vec<RSpan<'static>> = segments
        .into_iter()
        .map(|seg| {
            let style = if seg.active {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                colored_bg(app.no_color, app.theme.status_fg, app.theme.status_bg)
            };
            RSpan::styled(seg.text, style)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(colored_bg(
            app.no_color,
            app.theme.status_fg,
            app.theme.status_bg,
        )),
        area,
    );
}

fn draw_reading(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.active_tab().doc.is_some() {
        let visible_height = area.height.max(1);
        // Build (or reuse) the width-aware layout for this width, so scroll
        // offset is measured in the same laid-out lines the app's mappings
        // use. No runtime Wrap: the layout already broke lines to width.
        app.layout_width = area.width;
        app.viewport_height = visible_height;
        app.ensure_layout();
        // PRD FR-NV-1's "hints survive reflow": recomputed every draw (not
        // just once on entry) so a resize while hinting re-labels from the
        // layout just rebuilt above instead of replaying stale positions —
        // see `App::refresh_hint_targets`'s doc comment. A no-op outside hint
        // mode.
        app.refresh_hint_targets();

        let total_lines = app
            .layout
            .as_ref()
            .map(|l| l.lines.len() as u16)
            .unwrap_or(0);
        let max_scroll = total_lines.saturating_sub(visible_height);
        {
            let tab = app.active_tab_mut();
            tab.max_scroll = max_scroll;
            tab.scroll = tab.scroll.min(max_scroll);
        }
        let scroll = app.active_tab().scroll;

        let visited = visited_titles(app);
        if let Some(layout) = app.layout.as_ref() {
            let tab = app.active_tab();
            // In hint mode, paint a hint-overlaid COPY of the layout's lines
            // (PRD FR-NV-1) rather than the cached lines themselves — hints
            // are transient interactive state, never written back into the
            // cacheable `Layout` (see `layout::SpanKind::Hint`'s doc comment).
            let lines: std::borrow::Cow<[LaidLine]> = if app.mode == Mode::Hint {
                std::borrow::Cow::Owned(crate::hints::overlay_hint_labels(
                    &layout.lines,
                    &app.hint_targets,
                    &app.hint_input,
                    app.ambiguous_wide,
                ))
            } else {
                std::borrow::Cow::Borrowed(layout.lines.as_slice())
            };
            let text = paint_document(
                &lines,
                tab.focused_link,
                &tab.links,
                &visited,
                &app.theme,
                app.no_color,
                &tab.find_occurrences,
                tab.find_index,
                &app.image_store,
            );
            let paragraph = Paragraph::new(text)
                .style(base_style(&app.theme, app.no_color))
                .scroll((scroll, 0));
            frame.render_widget(paragraph, area);
        }
    } else if app.startpage_config == StartPageConfig::Blank {
        draw_blank_welcome(frame, app, area);
    } else {
        draw_start_page(frame, app, area);
    }
}

/// PRD FR-DL-1's `startpage = blank`: the original minimal welcome text —
/// no network, no navigation, exactly what every tab showed before the
/// start page existed.
fn draw_blank_welcome(frame: &mut Frame, app: &App, area: Rect) {
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
        Line::from("Press / to search Wikipedia, T to cycle themes, ? for help, q to quit."),
    ]);
    frame.render_widget(
        Paragraph::new(welcome).style(base_style(&app.theme, app.no_color)),
        area,
    );
}

/// PRD FR-DL-1's start page: today's featured article, top-5 most-read,
/// "in the news", an on-this-day strip, the TIL widget, and picture of the
/// day — built fresh from `App::start_page_model` every draw (cheap: a
/// handful of short strings). A flat `Paragraph` rather than the `List`
/// widget every picker uses: the section headers interspersed among the
/// navigable rows aren't themselves selectable, so this paints the
/// highlight manually instead of fighting `ListState`'s "index into this
/// exact list" contract. The item count is small enough (TFA + top-5 +
/// news + a 3-entry OTD strip + TIL) that this never needs to scroll — a
/// known, documented simplification, not an oversight.
fn draw_start_page(frame: &mut Frame, app: &App, area: Rect) {
    let model = app.start_page_model();
    let selected = model.clamp_selection(app.start_selected);

    let mut lines: Vec<Line> = vec![
        Line::from(RSpan::styled(
            "wikitui — today",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    if model.loading {
        lines.push(Line::from(RSpan::styled(
            "Loading today's picks…",
            colored(app.no_color, app.theme.dim),
        )));
        frame.render_widget(
            Paragraph::new(Text::from(lines)).style(base_style(&app.theme, app.no_color)),
            area,
        );
        return;
    }

    if model.offline {
        lines.push(Line::from(RSpan::styled(
            "offline — feed unavailable",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    let potd_max_cols = area.width.clamp(1, crate::layout::IMAGE_MAX_COLS);
    push_potd_lines(&mut lines, app, &model, potd_max_cols);

    let mut last_section: Option<startpage::Section> = None;
    for (i, item) in model.items.iter().enumerate() {
        if last_section != Some(item.section) {
            if last_section.is_some() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(RSpan::styled(
                item.section.label(),
                colored(app.no_color, app.theme.heading).add_modifier(Modifier::BOLD),
            )));
            last_section = Some(item.section);
        }

        let row_style = if i == selected {
            colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
        } else {
            Style::default()
        };
        let mut spans = Vec::new();
        if let Some(badge) = item.badge {
            spans.push(RSpan::styled(
                format!("{badge} "),
                row_style.patch(colored(app.no_color, app.theme.match_fg)),
            ));
        }
        spans.push(RSpan::styled(item.title.clone(), row_style));
        lines.push(Line::from(spans));

        if let Some(detail) = &item.detail {
            for detail_line in detail.lines() {
                lines.push(Line::from(RSpan::styled(
                    format!("  {detail_line}"),
                    colored(app.no_color, app.theme.dim),
                )));
            }
        }
    }

    if model.items.is_empty() {
        lines.push(Line::from(
            "Nothing to show — try again once you're back online.",
        ));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(base_style(&app.theme, app.no_color)),
        area,
    );
}

/// A start-page hero image is a "here's today's picture" banner, not the
/// whole page — capped far below `layout::IMAGE_MAX_ROWS` (which sizes an
/// inline article image on an otherwise-scrollable page) so it leaves room
/// for the rest of the sections on a page that, unlike an article, never
/// scrolls (see `draw_start_page`'s doc comment).
const POTD_MAX_ROWS: u16 = 8;

/// Appends the picture-of-the-day lines (PRD FR-DL-1): the decoded half-block
/// image (same pipeline `paint_line`'s `SpanKind::ImageRow` uses — PRD
/// FR-RD-8) when the theme renders images and the decode has landed;
/// caption-only text otherwise (text theme, images off, or still loading) —
/// never a broken box.
fn push_potd_lines(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    model: &StartPageModel,
    max_cols: u16,
) {
    let Some(title) = &model.potd_title else {
        return;
    };
    let images_would_render = app.images_enabled()
        && !matches!(
            app.graphics_protocol(),
            crate::graphics::GraphicsProtocol::None
        );
    let ready = model
        .potd_thumb_url
        .as_deref()
        .filter(|_| images_would_render)
        .and_then(|src| app.image_store.ready(src));

    if let Some(img) = ready {
        let (cols, rows) =
            crate::image::image_box_cells(img.width, img.height, max_cols, POTD_MAX_ROWS);
        let bg = theme_bg_rgb(&app.theme);
        for row in 0..rows.max(1) {
            let spans: Vec<RSpan<'static>> = crate::image::half_block_row(img, cols, rows, row, bg)
                .into_iter()
                .map(|cell| {
                    RSpan::styled(
                        crate::image::HALF_BLOCK,
                        Style::default()
                            .fg(Color::Rgb(cell.fg.0, cell.fg.1, cell.fg.2))
                            .bg(Color::Rgb(cell.bg.0, cell.bg.1, cell.bg.2)),
                    )
                })
                .collect();
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(RSpan::styled(
            format!("Picture of the day: {title}"),
            colored(app.no_color, app.theme.dim).add_modifier(Modifier::ITALIC),
        )));
    } else if images_would_render && model.potd_thumb_url.is_some() {
        lines.push(Line::from(RSpan::styled(
            format!("Picture of the day: {title} (loading…)"),
            colored(app.no_color, app.theme.dim).add_modifier(Modifier::ITALIC),
        )));
    } else {
        // Text theme, images off, or no thumbnail URL in the feed: FR-DL-1's
        // "skipped/caption-only" path.
        lines.push(Line::from(RSpan::styled(
            format!("Picture of the day: {title} (images off)"),
            colored(app.no_color, app.theme.dim).add_modifier(Modifier::ITALIC),
        )));
    }
    lines.push(Line::from(""));
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
    if app.results.is_empty() {
        draw_zero_results(frame, app, area);
        return;
    }

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
                // FR-SR-2: paint the API's own highlighted terms in
                // `theme.match_fg` instead of stripping the markup down to
                // plain text — the rest of the snippet stays dim italic.
                let runs = parse_searchmatch(excerpt);
                if runs.iter().any(|(text, _)| !text.trim().is_empty()) {
                    let spans: Vec<RSpan<'static>> = runs
                        .into_iter()
                        .map(|(text, is_match)| {
                            let style = if is_match {
                                colored(app.no_color, app.theme.match_fg)
                                    .add_modifier(Modifier::BOLD | Modifier::ITALIC)
                            } else {
                                colored(app.no_color, app.theme.dim).add_modifier(Modifier::ITALIC)
                            };
                            RSpan::styled(text, style)
                        })
                        .collect();
                    lines.push(Line::from(spans));
                }
            }
            if let Some(meta) = result_meta_line(r) {
                lines.push(Line::from(RSpan::styled(
                    meta,
                    colored(app.no_color, app.theme.dim),
                )));
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

/// FR-SR-2's "size, wordcount, last-edit date" line: omits whatever fields
/// the endpoint didn't provide, and the whole line if it provided none —
/// callers never see a line of empty punctuation.
fn result_meta_line(r: &crate::api::SearchResult) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(size) = r.size {
        parts.push(human_bytes(size));
    }
    if let Some(words) = r.wordcount {
        parts.push(format!("{words} words"));
    }
    if let Some(ts) = &r.timestamp {
        parts.push(format!("edited {}", ts.get(..10).unwrap_or(ts)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

/// PRD FR-SR-4 / §7's "Search: zero results" row: offers the did-you-mean
/// suggestion (when the server sent one) in place of an empty list, with
/// the exact "(Enter to search)" affordance §7 specifies.
fn draw_zero_results(frame: &mut Frame, app: &App, area: Rect) {
    let message =
        crate::app::zero_results_message(&app.search_input, app.search_suggestion.as_deref());
    let text = Text::from(vec![
        Line::from(""),
        Line::from(RSpan::styled(
            message,
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ]);
    let title = format!("Results for \"{}\" (0 found)", app.search_input);
    frame.render_widget(
        Paragraph::new(text)
            .style(base_style(&app.theme, app.no_color))
            .block(UiBlock::default().borders(Borders::ALL).title(title)),
        area,
    );
}

/// FR-SR-1's typeahead dropdown: floats over the reading view, anchored to
/// the bottom of `content_area` — just above the status-bar prompt line
/// it's completing. Reuses the same per-item-styled selectable-list
/// pattern as `draw_results`/`draw_toc` rather than introducing a second
/// selection-styling mechanism.
fn draw_search_suggestions(frame: &mut Frame, app: &App, content_area: Rect) {
    let visible = app.typeahead.len().clamp(1, 8) as u16;
    let height = (visible * 2 + 2).min(content_area.height);
    let width = content_area.width.clamp(20, 70);
    let popup = Rect {
        x: content_area.x,
        y: content_area
            .y
            .saturating_add(content_area.height)
            .saturating_sub(height),
        width,
        height,
    };

    let items: Vec<ListItem> = app
        .typeahead
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut lines = vec![Line::from(RSpan::styled(
                s.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            if let Some(desc) = &s.description {
                lines.push(Line::from(RSpan::styled(
                    desc.clone(),
                    colored(app.no_color, app.theme.dim),
                )));
            }
            let style = if i == app.selected_suggestion {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(lines).style(style)
        })
        .collect();

    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(
            UiBlock::default()
                .borders(Borders::ALL)
                .title("Suggestions"),
        );

    frame.render_widget(Clear, popup);
    render_selectable_list(frame, list, popup, app.selected_suggestion);
}

fn draw_toc(frame: &mut Frame, app: &App, area: Rect) {
    let tab = app.active_tab();
    let items: Vec<ListItem> = tab
        .sections
        .iter()
        .enumerate()
        .map(|(i, section)| {
            // Level 2 is the top-level "== Heading ==" tier; deeper levels
            // get progressively indented.
            let indent = "  ".repeat(section.level.saturating_sub(2) as usize);
            let style = if i == tab.selected_section {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(format!("{indent}{}", section.title))).style(style)
        })
        .collect();

    let title = format!("Table of contents ({} sections)", tab.sections.len());
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, tab.selected_section);
}

/// The `bb` / `:tabs` tab picker (PRD FR-TB-1): index, title, language, and a
/// loading marker per tab. Follows the same selectable-list pattern as the
/// TOC and library views.
fn draw_tab_picker(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let marker = if tab.loading { "  … loading" } else { "" };
            let line = Line::from(vec![
                RSpan::styled(
                    format!("{}: {}", i + 1, tab.display_title()),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::styled(
                    format!("   [{}]{}", tab.lang, marker),
                    colored(app.no_color, app.theme.dim),
                ),
            ]);
            let style = if i == app.selected_tab_pick {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let title = format!(
        "Tabs ({}) — Enter: switch  d: close  Esc: cancel",
        app.tabs.len()
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_tab_pick);
}

/// The `gb` back-stack picker (PRD FR-NV-7): the active tab's history trail,
/// oldest at the top. Enter jumps to the highlighted entry (browser-style).
fn draw_history_picker(frame: &mut Frame, app: &App, area: Rect) {
    let tab = app.active_tab();
    let items: Vec<ListItem> = tab
        .back_stack
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let line = Line::from(vec![
                RSpan::styled(
                    entry.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::styled(
                    format!("   [{}]", entry.lang),
                    colored(app.no_color, app.theme.dim),
                ),
            ]);
            let style = if i == app.selected_history {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let current = tab
        .doc
        .as_ref()
        .map(|d| d.title.as_str())
        .unwrap_or("(none)");
    let title = format!("History → {current} — Enter: jump  Esc: cancel");
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_history);
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

/// The `B` / `:bookmarks` picker (PRD FR-BM-1): title, `#tag` list (dim),
/// note-preview first line (dim italic), and created date per entry — the
/// same selectable-list pattern as the TOC/library views, over the
/// currently-filtered subset (`App::visible_bookmarks`). The `/` filter and
/// `t` tag editor draw this same view; only the status line changes.
fn draw_bookmark_picker(frame: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible_bookmarks();
    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(row, &store_index)| {
            let b = &app.bookmarks.bookmarks[store_index];
            let mut header: Vec<RSpan> = vec![RSpan::styled(
                b.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )];
            if !b.tags.is_empty() {
                let tags = b
                    .tags
                    .iter()
                    .map(|t| format!("#{t}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                header.push(RSpan::styled(
                    format!("   {tags}"),
                    colored(app.no_color, app.theme.dim),
                ));
            }
            header.push(RSpan::styled(
                format!("   {}", crate::bookmarks::display_date(&b.created_at)),
                colored(app.no_color, app.theme.dim),
            ));
            let mut lines = vec![Line::from(header)];
            if let Some(note) = b.note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                let first = note.lines().next().unwrap_or(note);
                lines.push(Line::from(RSpan::styled(
                    format!("      {first}"),
                    colored(app.no_color, app.theme.dim).add_modifier(Modifier::ITALIC),
                )));
            }
            let style = if row == app.selected_bookmark {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(lines).style(style)
        })
        .collect();

    let filter_note = if app.bookmark_filter_input.is_empty() {
        String::new()
    } else {
        format!(" (filter: {})", app.bookmark_filter_input)
    };
    let title = format!(
        "Bookmarks — {} of {}{filter_note}",
        visible.len(),
        app.bookmarks.bookmarks.len()
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_bookmark);
}

/// The `:readlater` queue view (PRD FR-BM-3): title + enqueued date +
/// reading-time placeholder. The estimate itself arrives with FR-RD-11
/// (chunked reading time); until then every row shows "—" — the marked seam
/// is `reading_time_estimate` below, the single place a real estimate will
/// slot in.
fn draw_readlater_picker(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .readlater
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let line = Line::from(vec![
                RSpan::styled(
                    entry.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::styled(
                    format!(
                        "   [{}]   enqueued {}   ~{} read",
                        entry.lang,
                        crate::bookmarks::display_date(&entry.enqueued_at),
                        reading_time_estimate(entry),
                    ),
                    colored(app.no_color, app.theme.dim),
                ),
            ]);
            let style = if i == app.selected_readlater {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let title = format!(
        "Read later ({}) — Enter: open  d: remove  Esc: close",
        app.readlater.entries.len()
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_readlater);
}

/// FR-RD-11 seam: the per-entry reading-time estimate the read-later view
/// shows. Returns the placeholder "—" until the chunked reading-time
/// feature lands and can compute a real figure from the cached article —
/// isolated here so that later chunk changes exactly one function.
fn reading_time_estimate(_entry: &crate::bookmarks::ReadLaterEntry) -> &'static str {
    "—"
}

/// `Ctrl-h` / `:history`'s persistent reading-history picker (PRD FR-HS-1):
/// title, language (only when it differs from the app-global default —
/// most installs read one language, so a `[en]` tag on every row would be
/// noise), relative visit time, and dwell when it's long enough to be worth
/// mentioning. Reads from `app.history_pick_matches`
/// (`App::refresh_history_matches`'s output), not a live query — this is a
/// paint function, not a place to hit SQLite from.
fn draw_reading_history_picker(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .history_pick_matches
        .iter()
        .enumerate()
        .map(|(i, visit)| {
            let mut detail = format!(
                "   {}",
                crate::history::relative_time(crate::history::now_unix() - visit.opened_at)
            );
            if visit.lang != app.lang {
                detail = format!("   [{}]{detail}", visit.lang);
            }
            if visit.dwell_secs >= 30 {
                detail.push_str(&format!(
                    "   ~{}",
                    crate::cache::age_human(visit.dwell_secs as u64)
                ));
            }
            let line = Line::from(vec![
                RSpan::styled(
                    visit.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::styled(detail, colored(app.no_color, app.theme.dim)),
            ]);
            let style = if i == app.history_pick_selected {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let filter_note = if app.history_pick_filter.is_empty() {
        String::new()
    } else {
        format!(" (filter: {})", app.history_pick_filter)
    };
    let title = format!(
        "History ({}){filter_note} — Enter: open  /: filter  d: delete  Esc: close",
        app.history_pick_matches.len()
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.history_pick_selected);
}

/// The `:saved` saved-pages browser (PRD §5.7 / FR-OFF-4): title, tier, size,
/// saved date, and the integrity verdict (ok/corrupt — the sha256 check). The
/// pinned store is visually distinct from the read-later/history pickers by
/// carrying its own size + integrity columns, matching the "quota-visible,
/// integrity-checked" contract §5.7 draws around saved pages.
fn draw_saved_picker(frame: &mut Frame, app: &App, area: Rect) {
    let integrity = app.saved.verify_all();
    let items: Vec<ListItem> = app
        .saved
        .list()
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let ok = integrity
                .get(i)
                .map(|(_, _, v)| *v == crate::saved::Integrity::Ok)
                .unwrap_or(true);
            let integrity_note = if ok { "ok" } else { "CORRUPT" };
            let detail = format!(
                "   [{}] {}   {}   saved {}   integrity: {integrity_note}",
                rec.lang,
                rec.tier.label(),
                human_size(rec.size_total),
                crate::bookmarks::display_date(&rec.saved_at),
            );
            let line = Line::from(vec![
                RSpan::styled(
                    rec.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::styled(detail, colored(app.no_color, app.theme.dim)),
            ]);
            let style = if i == app.selected_saved {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let title = format!(
        "Saved pages ({}, {}) — Enter: open  d: remove  Esc: close",
        app.saved.list().len(),
        human_size(app.saved.total_bytes()),
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_saved);
}

/// A compact byte-size for the saved browser ("30 KB" / "1.4 MB").
fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// PRD FR-PF-4's `:prefetch-log` panel — the prefetch transparency and debug
/// tool. Shows the kill-switch/incognito state, the live budget (request and
/// byte windows, metered posture, queue depth), then each recent action with
/// its reason string, status (queued/done/failed/skipped-budget/rate-limited),
/// and bytes. Read-only; a paint function, so it reads snapshots off the
/// substrate handle rather than mutating anything.
fn draw_prefetch_log(frame: &mut Frame, app: &App, area: Rect) {
    let Some(handle) = app.prefetch.as_ref() else {
        let para = Paragraph::new("Prefetch substrate is not active in this session.")
            .style(base_style(&app.theme, app.no_color))
            .block(
                UiBlock::default()
                    .borders(Borders::ALL)
                    .title("Prefetch log"),
            );
        frame.render_widget(para, area);
        return;
    };
    let entries = handle.log_recent();
    let budget = handle.budget_snapshot();
    let pending = handle.pending();

    let state = if app.incognito {
        "OFF (incognito)".to_string()
    } else if handle.is_enabled() {
        "ON".to_string()
    } else {
        "OFF (:set prefetch=on to enable)".to_string()
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        RSpan::styled("prefetch: ", colored(app.no_color, app.theme.dim)),
        RSpan::styled(state, Style::default().add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(RSpan::styled(
        format!(
            "budget: {}/{} req/h · {}/{} today · metered: {}{} · {pending} queued",
            budget.requests_used,
            budget.requests_cap,
            human_size(budget.bytes_used),
            human_size(budget.bytes_cap),
            budget.metered.label(),
            if budget.suspended { " (suspended)" } else { "" },
        ),
        colored(app.no_color, app.theme.dim),
    )));
    lines.push(Line::from(""));

    if entries.is_empty() {
        lines.push(Line::from(RSpan::styled(
            "No prefetch actions yet — open an article to prime its links.",
            colored(app.no_color, app.theme.dim),
        )));
    } else {
        for e in &entries {
            use crate::netqueue::LogStatus;
            let status_style = match e.status {
                LogStatus::Done => Style::default().add_modifier(Modifier::BOLD),
                LogStatus::Failed | LogStatus::RateLimited => {
                    colored(app.no_color, app.theme.match_fg).add_modifier(Modifier::BOLD)
                }
                LogStatus::SkippedBudget | LogStatus::Queued => {
                    colored(app.no_color, app.theme.dim)
                }
            };
            let bytes = if e.bytes > 0 {
                format!("  ({})", human_size(e.bytes))
            } else {
                String::new()
            };
            lines.push(Line::from(vec![
                RSpan::styled(format!("[{}] ", e.status.label()), status_style),
                RSpan::styled(
                    e.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::styled(
                    format!("  {}{bytes}", e.reason),
                    colored(app.no_color, app.theme.dim),
                ),
            ]));
        }
    }

    let title = format!("Prefetch log ({}) — Esc: close", entries.len());
    let para = Paragraph::new(lines)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    frame.render_widget(para, area);
}

/// PRD FR-DL-2's `:today` panel: a one-row strip of type tabs (events/
/// births/deaths/holidays/selected) above a selectable list of that type's
/// entries — Tab/Shift-Tab (or h/l) switch types, j/k move, Enter opens.
fn draw_on_this_day(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = UiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let tab_spans: Vec<RSpan> = startpage::OtdType::ALL
        .iter()
        .map(|&t| {
            let style = if t == app.otd_tab {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                colored(app.no_color, app.theme.dim)
            };
            RSpan::styled(format!(" {} ", t.label()), style)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Line::from(tab_spans)).style(base_style(&app.theme, app.no_color)),
        chunks[0],
    );

    let entries = app.otd.entries(app.otd_tab);
    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let text = match e.year {
                Some(y) => format!("{y} — {}", e.text),
                None => e.text.clone(),
            };
            let style = if i == app.otd_selected {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(text)).style(style)
        })
        .collect();
    let title = format!(
        "On this day — {} ({} entries) — Enter: open  Tab/h/l: switch type  Esc: close",
        app.otd_tab.label(),
        entries.len()
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, chunks[1], app.otd_selected);
}

/// §7's "Offline, uncached link" card: a centered overlay offering the two
/// documented choices (queue for fetch when online / search saved pages).
fn draw_offline_card(frame: &mut Frame, app: &App, area: Rect) {
    let width = 54.min(area.width.saturating_sub(4)).max(20);
    let height = 8.min(area.height.saturating_sub(2)).max(6);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let title = app
        .offline_card_target
        .as_ref()
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    let body = Text::from(vec![
        Line::from(RSpan::styled(
            "Not available offline",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("\"{title}\" isn't cached or saved.")),
        Line::from(""),
        Line::from("f  queue for fetch when online"),
        Line::from("s  search saved pages"),
        Line::from("Esc  dismiss"),
    ]);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body)
            .style(base_style(&app.theme, app.no_color))
            .block(UiBlock::default().borders(Borders::ALL).title("Offline")),
        popup,
    );
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let tab = app.active_tab();
    let text = match app.mode {
        Mode::Search if app.typeahead.is_empty() => format!(
            "/{}   Tab: full-text search   Esc: cancel",
            app.search_input
        ),
        Mode::Search => format!(
            "/{}   \u{2191}/\u{2193} or Ctrl-n/p: move   Enter: open   Tab: full-text search",
            app.search_input
        ),
        Mode::Command => format!(":{}", app.command_input),
        // PRD FR-NV-1's hint-mode status line, ahead of everything Reading
        // shows (notice, find, focused link, breadcrumb) by being its own
        // `Mode` arm here — the same priority mechanism `Find`/`Command`
        // already use, not a special case bolted onto `Mode::Reading`.
        Mode::Hint => format!("hint: {}   Esc: cancel", app.hint_input),
        Mode::Find if tab.find_matches.is_empty() && !tab.find_input.is_empty() => {
            format!("find: {} (no matches)", tab.find_input)
        }
        Mode::Find if !tab.find_matches.is_empty() => {
            format!(
                "find: {} ({}/{})",
                tab.find_input,
                tab.find_index + 1,
                tab.find_matches.len()
            )
        }
        Mode::Find => format!("find: {}", tab.find_input),
        Mode::Results if app.results.is_empty() && app.search_suggestion.is_some() => {
            "Enter: search the suggestion   Esc: cancel".to_string()
        }
        Mode::Results => "Enter: open   Esc: cancel   j/k: move".to_string(),
        Mode::Toc => "Enter: jump to section   Esc: cancel   j/k: move".to_string(),
        Mode::TabPicker => "Enter: switch tab   d: close   Esc: cancel   j/k: move".to_string(),
        Mode::HistoryPicker => "Enter: jump   Esc: cancel   j/k: move".to_string(),
        Mode::Research => "Enter/s: save citation   R: library   Esc: done   j/k: move".to_string(),
        // The library's status line carries transient action feedback
        // (delete/export/style outcomes overwrite it) — see open_library.
        Mode::Library => app.status.clone(),
        // The `/` filter and `t` tag editor show their live prompt; the
        // plain picker carries its transient action feedback via `status`.
        Mode::BookmarkFilter => format!("filter: {}   Esc: apply", app.bookmark_filter_input),
        Mode::BookmarkTagEdit => {
            format!(
                "tags (comma/space): {}   Enter: save   Esc: cancel",
                app.bookmark_tag_input
            )
        }
        Mode::BookmarkPicker => app.status.clone(),
        Mode::ReadLaterPicker => app.status.clone(),
        Mode::SavedPicker => app.status.clone(),
        Mode::OfflineCard => {
            "f: queue for fetch when online   s: search saved pages   Esc: dismiss".to_string()
        }
        Mode::ReadingHistory => app.status.clone(),
        Mode::ReadingHistoryFilter => {
            format!("filter: {}   Esc: apply", app.history_pick_filter)
        }
        Mode::PrefetchLog => app.status.clone(),
        Mode::OnThisDay => app.status.clone(),
        Mode::Help => "Press any key to close help".to_string(),
        Mode::Reading if app.loading => "Loading…".to_string(),
        // Command feedback outranks the focused-link line until the next
        // keypress clears it (see App::notice).
        Mode::Reading if app.notice.is_some() => app.notice.clone().unwrap_or_default(),
        // PRD FR-DL-1: the start page's own nav hints, ranked right after
        // notice/loading — there is no focused link or find state to show
        // instead (the tab has no document). `startpage = blank` shows the
        // plain welcome text instead of the navigable start page, so it
        // keeps the original bare status line rather than hinting at
        // bindings that view doesn't have.
        Mode::Reading if tab.doc.is_none() && app.startpage_config != StartPageConfig::Blank => {
            "j/k or Tab: move   Enter: open   t: shuffle Did You Know   gh: home".to_string()
        }
        Mode::Reading if !tab.find_matches.is_empty() => {
            format!(
                "match {}/{} for \"{}\"   n/N: cycle   Esc: clear",
                tab.find_index + 1,
                tab.find_matches.len(),
                tab.find_input
            )
        }
        // The focused-link line takes over the status bar on any page with
        // links (nearly all of them), so the page-source indicator must
        // prefix it too or ◐/○ would never actually be seen (app.status
        // already carries the prefix via set_document).
        Mode::Reading => match tab.focused_link.and_then(|i| tab.links.get(i)) {
            Some(link) if link.internal_title.is_some() => {
                format!(
                    "{}→ {} ({}/{})   Tab/S-Tab: cycle   Enter: open   H: back   L: forward",
                    tab.page_source.prefix(),
                    link.text,
                    tab.focused_link.unwrap() + 1,
                    tab.links.len()
                )
            }
            Some(link) => format!(
                "{}→ {} (external, not yet followable)",
                tab.page_source.prefix(),
                link.text
            ),
            // FR-NV-7: with no notice, find, or focused link to show, the
            // default segment is the active tab's breadcrumb trail; falls
            // back to the plain status line before any history has built up.
            None => {
                let titles = app.breadcrumb_titles();
                if titles.len() > 1 {
                    let prefix = tab.page_source.prefix();
                    let budget = (area.width as usize).saturating_sub(display_width(&prefix));
                    format!("{prefix}{}", build_breadcrumb(&titles, budget))
                } else {
                    app.status.clone()
                }
            }
        },
    };
    let style = if matches!(
        app.mode,
        Mode::Search
            | Mode::Find
            | Mode::Command
            | Mode::Hint
            | Mode::BookmarkFilter
            | Mode::BookmarkTagEdit
    ) {
        colored_bg(app.no_color, app.theme.focus_fg, app.theme.focus_bg)
    } else {
        colored_bg(app.no_color, app.theme.status_fg, app.theme.status_bg)
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn draw_help_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let width = 62.min(area.width.saturating_sub(4)).max(20);
    let height = 31.min(area.height.saturating_sub(4)).max(8);
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
        Line::from("f            link hints: type the label to follow"),
        Line::from("F            link hints: open the label in a background tab"),
        Line::from("Enter        follow link / open selected result"),
        Line::from("Ctrl-Enter   open focused link in a background tab"),
        Line::from("H / L        back / forward (per tab, restores scroll)"),
        Line::from("gb           back-stack picker (breadcrumb trail)"),
        Line::from("Ctrl-h       reading history (persistent)   :history clear today|all"),
        Line::from("gt / gT      next / previous tab"),
        Line::from("bb           tab picker    u  reopen closed tab"),
        Line::from("t            table of contents"),
        Line::from("[ / ]        scroll wide tables left / right"),
        Line::from("T            cycle color theme"),
        Line::from("y / Y        yank URL / Markdown link"),
        Line::from(":            command (:open, :lang, :theme, :tab, :tabs, :q)"),
        Line::from("r            research mode: cite this page & its sources"),
        Line::from("R            library: browse/export saved bibliography"),
        Line::from("m            bookmark / un-bookmark this article (toggle)"),
        Line::from("B            bookmark picker   ba  annotate ($EDITOR note)"),
        Line::from("  in picker  /: filter   t: edit tags   d: delete   Enter: open"),
        Line::from("  filter     free text fuzzy-matches title; #tag needs ALL tags"),
        Line::from("rl           read later (focused link, else article)"),
        Line::from(":readlater   read-later queue   :bookmarks[ export md|html|json|netscape]"),
        Line::from("/            search: type for suggestions, Enter opens, Tab full-text"),
        Line::from("Ctrl-f       find in this page (smart-case), n/N: cycle matches"),
        Line::from("Esc          cancel / close"),
        Line::from("?            toggle this help"),
        Line::from("q            close current tab (quits on the last)"),
        Line::from("Q            quit (with y/n confirm)"),
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
    use crate::layout::{LayoutOptions, layout_document};

    /// The paint step maps each heading's layout line (via
    /// `section_outline` + `Layout::block_lines`) back to the rendered row.
    /// This proves the single line-truth source (the layout) drives both the
    /// table of contents and the painted output, so "jump to section" lands
    /// on the heading.
    #[test]
    fn painted_heading_lines_match_the_section_outline() {
        let html = r##"
        <html><body>
        <p>Intro paragraph.</p>
        <h2>First Section</h2>
        <p>Some text.</p>
        <ul><li>One</li><li>Two</li></ul>
        <blockquote><p>A quote.</p></blockquote>
        <h2>Second Section</h2>
        <p>More text.</p>
        </body></html>
        "##;
        let doc = parse_article_html("Test Article", html);
        let sections = section_outline(&doc);
        assert_eq!(sections.len(), 2, "fixture has exactly two headings");

        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let theme = Theme::terminal();
        let text = paint_document(
            &layout.lines,
            None,
            &[],
            &HashSet::new(),
            &theme,
            false,
            &[],
            0,
            &crate::image::ImageStore::new(),
        );

        for section in &sections {
            let line = layout.block_lines[section.block];
            let rendered: String = text.lines[line]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            assert_eq!(
                rendered.trim(),
                section.title,
                "the heading's layout line must render its own text"
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
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let text = paint_document(
            &layout.lines,
            None,
            &links,
            &visited,
            &theme,
            false,
            &[],
            0,
            &crate::image::ImageStore::new(),
        );

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

    #[test]
    fn parse_searchmatch_splits_matched_and_plain_runs() {
        let runs =
            parse_searchmatch(r#"Alan <span class="searchmatch">Turing</span> was born in 1912"#);
        assert_eq!(
            runs,
            vec![
                ("Alan ".to_string(), false),
                ("Turing".to_string(), true),
                (" was born in 1912".to_string(), false),
            ]
        );
    }

    #[test]
    fn parse_searchmatch_treats_nested_spans_as_still_matched() {
        // Real MediaWiki output never nests, but the parser must not corrupt
        // state (or panic) if a hostile/malformed response does.
        let runs = parse_searchmatch(
            r#"<span class="searchmatch">a<span class="searchmatch">b</span>c</span>d"#,
        );
        assert_eq!(
            runs,
            vec![
                ("a".to_string(), true),
                ("b".to_string(), true),
                ("c".to_string(), true),
                ("d".to_string(), false),
            ]
        );
    }

    #[test]
    fn parse_searchmatch_unclosed_span_highlights_to_the_end() {
        let runs = parse_searchmatch(r#"before <span class="searchmatch">after"#);
        assert_eq!(
            runs,
            vec![("before ".to_string(), false), ("after".to_string(), true)]
        );
    }

    #[test]
    fn parse_searchmatch_stray_closing_tag_does_not_underflow_or_panic() {
        let runs = parse_searchmatch("before</span> middle </span>after");
        assert_eq!(
            runs,
            vec![
                ("before".to_string(), false),
                (" middle ".to_string(), false),
                ("after".to_string(), false),
            ]
        );
    }

    #[test]
    fn parse_searchmatch_strips_other_markup_outside_spans() {
        let runs = parse_searchmatch(r#"<b>bold</b> <span class="searchmatch">hit</span>"#);
        assert_eq!(
            runs,
            // The space between the tags is real text, not markup, and
            // stays — only the `<b>`/`</b>` tags themselves are dropped.
            vec![("bold ".to_string(), false), ("hit".to_string(), true)]
        );
    }

    #[test]
    fn parse_searchmatch_empty_input_is_empty() {
        assert!(parse_searchmatch("").is_empty());
    }

    /// End-to-end through `paint_document`: a find match on one line paints
    /// in `theme.match_fg`, and the *current* occurrence additionally gets
    /// `Modifier::REVERSED` while a non-current one on another line doesn't.
    #[test]
    fn paint_document_highlights_matches_and_emphasizes_the_current_one() {
        let doc = parse_article_html(
            "Test",
            "<html><body><p>turing</p><p>turing again</p></body></html>",
        );
        let theme = Theme::full();
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let occurrences =
            crate::layout::find_matches(&layout.lines, &layout.continuation, "turing");
        assert_eq!(occurrences.len(), 2, "one hit per paragraph");

        let text = paint_document(
            &layout.lines,
            None,
            &[],
            &HashSet::new(),
            &theme,
            false,
            &occurrences,
            0, // the first occurrence is "current"
            &crate::image::ImageStore::new(),
        );

        let match_spans: Vec<_> = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.content.as_ref() == "turing")
            .collect();
        assert_eq!(match_spans.len(), 2, "both occurrences painted as matches");
        for s in &match_spans {
            assert_eq!(s.style.fg, Some(theme.match_fg));
        }
        let reversed_count = match_spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .count();
        assert_eq!(
            reversed_count, 1,
            "exactly the current occurrence gets REVERSED emphasis"
        );
    }

    /// The exact bug interactive pty verification caught: a query that
    /// straddles a visual line wrap must have BOTH of its pieces get the
    /// current-match emphasis when it's the current occurrence — not just
    /// the piece on whichever line happens to come first.
    #[test]
    fn paint_document_emphasizes_every_piece_of_a_wrapped_current_match() {
        use crate::layout::{MatchSpan, Occurrence};

        let doc = parse_article_html("Test", "<html><body><p>filler</p></body></html>");
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        // Two lines, standing in for a paragraph that wrapped mid-phrase;
        // the occurrence has one piece on each.
        let mut layout = layout;
        layout.lines = vec![
            crate::layout::LaidLine {
                spans: vec![crate::layout::LaidSpan {
                    text: "computer".to_string(),
                    kind: SpanKind::Plain,
                }],
            },
            crate::layout::LaidLine {
                spans: vec![crate::layout::LaidSpan {
                    text: "science".to_string(),
                    kind: SpanKind::Plain,
                }],
            },
        ];
        layout.continuation = vec![true];

        let occurrence = Occurrence {
            pieces: vec![
                (0, MatchSpan { start: 0, end: 8 }),
                (1, MatchSpan { start: 0, end: 7 }),
            ],
        };
        let theme = Theme::full();
        let text = paint_document(
            &layout.lines,
            None,
            &[],
            &HashSet::new(),
            &theme,
            false,
            &[occurrence],
            0,
            &crate::image::ImageStore::new(),
        );

        let reversed_on = |line: usize, needle: &str| {
            text.lines[line].spans.iter().any(|s| {
                s.content.as_ref() == needle && s.style.add_modifier.contains(Modifier::REVERSED)
            })
        };
        assert!(
            reversed_on(0, "computer"),
            "the first piece of the wrapped current match must be emphasized"
        );
        assert!(
            reversed_on(1, "science"),
            "the second piece of the SAME wrapped current match must be emphasized too"
        );
    }

    // ---- Tab bar & breadcrumb builders (PRD FR-TB-1, FR-NV-7) -----------

    fn label(number: usize, title: &str, loading: bool, active: bool) -> TabLabel {
        TabLabel {
            number,
            title: title.to_string(),
            loading,
            active,
        }
    }

    #[test]
    fn tab_bar_renders_every_tab_when_they_fit() {
        let labels = vec![
            label(1, "Alan Turing", false, true),
            label(2, "Enigma", false, false),
        ];
        let segments = build_tab_bar(&labels, 80);
        assert_eq!(segments.len(), 2, "both tabs shown when they fit");
        let joined: String = segments.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("1:Alan Turing"));
        assert!(joined.contains("2:Enigma"));
        assert_eq!(
            segments.iter().filter(|s| s.active).count(),
            1,
            "exactly one active segment"
        );
        assert!(segments[0].active, "tab 1 is the active one");
    }

    #[test]
    fn tab_bar_collapses_to_compact_indicator_on_overflow() {
        // 17 tabs with long titles, active = tab 3 (number 3), narrow bar.
        let mut labels: Vec<TabLabel> = (1..=17)
            .map(|n| label(n, "Article With A Fairly Long Title", false, false))
            .collect();
        labels[2].active = true; // the 3rd tab
        let width = 30;
        let segments = build_tab_bar(&labels, width);

        let joined: String = segments.iter().map(|s| s.text.as_str()).collect();
        assert!(
            joined.contains("[3/17]"),
            "overflow shows the [active/total] indicator: {joined:?}"
        );
        assert_eq!(
            segments.iter().filter(|s| s.active).count(),
            1,
            "still exactly one active segment on overflow"
        );
        assert!(
            display_width(&joined) <= width,
            "the compact bar must fit the width ({}<={width}): {joined:?}",
            display_width(&joined)
        );
    }

    #[test]
    fn tab_bar_marks_a_loading_tab_with_a_static_ellipsis() {
        let labels = vec![
            label(1, "Alan Turing", false, true),
            label(2, "Enigma", true, false), // loading
        ];
        let segments = build_tab_bar(&labels, 80);
        let loading_seg = &segments[1];
        assert!(
            loading_seg.text.contains('…'),
            "a loading tab shows a static ellipsis: {:?}",
            loading_seg.text
        );
    }

    #[test]
    fn middle_truncate_keeps_head_and_tail_within_budget() {
        let s = "A Very Long Article Title That Will Not Fit";
        let out = middle_truncate(s, 20);
        assert!(out.contains('…'), "truncation inserts an ellipsis: {out:?}");
        assert!(
            display_width(&out) <= 20,
            "truncated width {} must fit budget",
            display_width(&out)
        );
        assert!(out.starts_with('A'), "keeps the head: {out:?}");
        assert!(out.ends_with('t'), "keeps the tail: {out:?}");
    }

    #[test]
    fn middle_truncate_is_a_noop_when_it_fits() {
        assert_eq!(middle_truncate("Short", 20), "Short");
    }

    #[test]
    fn middle_truncate_is_width_aware_for_cjk() {
        // Each CJK glyph is 2 cells wide; truncation must not split a cell.
        let s = "アラン・チューリング計算機科学";
        let out = middle_truncate(s, 10);
        assert!(display_width(&out) <= 10, "CJK width respected: {out:?}");
        assert!(out.contains('…'));
    }

    #[test]
    fn breadcrumb_joins_the_trail_with_arrows() {
        let trail = vec![
            "Turing".to_string(),
            "Enigma".to_string(),
            "Bletchley Park".to_string(),
        ];
        let out = build_breadcrumb(&trail, 80);
        assert_eq!(out, "Turing → Enigma → Bletchley Park");
    }

    #[test]
    fn breadcrumb_middle_truncates_a_long_trail() {
        let trail = vec![
            "Alan Turing".to_string(),
            "Enigma machine".to_string(),
            "Bletchley Park".to_string(),
            "Government Code and Cypher School".to_string(),
        ];
        let out = build_breadcrumb(&trail, 30);
        assert!(display_width(&out) <= 30, "breadcrumb fits: {out:?}");
        assert!(out.contains('…'), "long trail is truncated: {out:?}");
    }

    #[test]
    fn breadcrumb_of_empty_trail_is_empty() {
        assert_eq!(build_breadcrumb(&[], 40), "");
    }

    /// PRD §6.3 hard floor: below 60×16 the whole draw is replaced by the
    /// "terminal too small" screen, whatever mode the app is in.
    #[test]
    fn a_sub_floor_terminal_draws_the_too_small_screen() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let mut terminal = Terminal::new(TestBackend::new(50, 10)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            rendered.contains("Terminal too small"),
            "sub-floor terminal must show the too-small screen, got: {rendered:?}"
        );
    }

    /// A comfortably-sized terminal draws the normal reading view, not the
    /// too-small screen.
    #[test]
    fn a_normal_terminal_does_not_draw_the_too_small_screen() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!rendered.contains("Terminal too small"));
    }
}
