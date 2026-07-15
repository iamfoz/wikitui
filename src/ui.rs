use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout as UiLayout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span as RSpan, Text};
use ratatui::widgets::{
    Block as UiBlock, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{self, App, Mode};
use crate::doc::LinkRef;
use crate::layout::{
    FLOOR_MIN_HEIGHT, FLOOR_MIN_WIDTH, LaidLine, MatchSpan, SizeTier, SpanKind, size_tier,
};
use crate::registry;
use crate::search_ops;
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
    visited_titles_for(app, app.active_tab())
}

/// PRD FR-HS-2 visited styling for a *specific* tab (split rendering lays each
/// pane out against its own tab). The active-tab [`visited_titles`] delegates
/// here.
fn visited_titles_for<'a>(app: &'a App, tab: &'a crate::tab::Tab) -> HashSet<&'a str> {
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

/// PRD FR-DL-5: titles the batched `generator=links&prop=info` check has
/// confirmed missing on the active tab's wiki edition, for the redlink
/// styling `kind_style` layers on at paint time — the async-checked
/// complement to `LinkRef::redlink`'s own parse-time (`class="new"`) signal,
/// built fresh each draw exactly like `visited_titles` reads
/// `history::History` fresh each draw rather than caching a snapshot.
fn confirmed_redlink_titles(app: &App) -> HashSet<&str> {
    let tab = app.active_tab();
    confirmed_redlink_titles_for(app, &tab.wiki, &tab.lang)
}

/// PRD FR-DL-5 confirmed redlinks for a *specific* wiki+language edition
/// (split panes may hold two editions, and a `:wiki` switch two projects).
/// The active-tab [`confirmed_redlink_titles`] delegates here.
fn confirmed_redlink_titles_for<'a>(app: &'a App, wiki: &str, lang: &str) -> HashSet<&'a str> {
    app.confirmed_redlinks
        .iter()
        .filter(|(w, l, _)| w == wiki && l == lang)
        .map(|(_, _, title)| title.as_str())
        .collect()
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
#[allow(clippy::too_many_arguments)]
fn kind_style(
    kind: &SpanKind,
    focused_link: Option<usize>,
    links: &[LinkRef],
    visited: &HashSet<&str>,
    redlinks: &HashSet<&str>,
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
            // PRD FR-DL-5: a redlink is dim/struck regardless of focus or
            // visited state — Parsoid's own `class="new"` pre-marking
            // (`LinkRef::redlink`) or the batched info-check's confirmation
            // (`redlinks`, populated from `App::confirmed_redlinks` exactly
            // like `visited` is populated from history) are two independent
            // signals for the same fact, either one enough. Checked first,
            // ahead of focus: a focused redlink should still read as "this
            // doesn't exist" rather than the ordinary focus highlight lying
            // about it.
            let is_redlink = links.get(*occ).is_some_and(|l| {
                l.redlink
                    || l.internal_title
                        .as_deref()
                        .is_some_and(|title| redlinks.contains(title))
            });
            if is_redlink {
                return colored(no_color, theme.dim).add_modifier(Modifier::CROSSED_OUT);
            }
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
        // PRD FR-RD-7: dim italics — visually distinct from ordinary prose
        // and from a caption (`Caption` is also dim italic, but math never
        // appears where the two could be confused) without needing a new
        // theme color (`§FR-RD-7` allows "a math color *or* dim").
        SpanKind::Math => colored(no_color, theme.dim).add_modifier(Modifier::ITALIC),
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
    redlinks: &HashSet<&str>,
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
        let base_style = kind_style(
            &s.kind,
            focused_link,
            links,
            visited,
            redlinks,
            theme,
            no_color,
        );
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
    redlinks: &HashSet<&str>,
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
                    redlinks,
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

    // PRD FR-NV-9: stashed so a mouse event arriving on the *next* input
    // turn can translate its absolute terminal coordinates into these areas'
    // own space — one-frame-stale like `layout_width`/`viewport_height`
    // (see `App::last_content_area`'s doc comment).
    app.last_content_area = content_area;
    app.last_tab_bar_area = tab_bar_area;

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
        // PRD §5.9's login paste prompt draws over the reading view (like
        // Command); its prompt lives in the status bar.
        Mode::Reading
        | Mode::Help
        | Mode::Search
        | Mode::Find
        | Mode::Command
        | Mode::Hint
        | Mode::Login => draw_reading(frame, app, content_area),
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
        Mode::Interests => draw_interests(frame, app, content_area),
        Mode::Stats => draw_stats(frame, app, content_area),
        // The offline card overlays the reading view (drawn after the status
        // bar below, like the help overlay).
        Mode::OfflineCard => draw_reading(frame, app, content_area),
        // PRD FR-DL-5 / §7's "Redlink followed" card — same overlay pattern.
        Mode::RedlinkCard => draw_reading(frame, app, content_area),
        Mode::OnThisDay => draw_on_this_day(frame, app, content_area),
        Mode::Related => draw_related(frame, app, content_area),
        // The `/` filter draws over the same picker view; only the status
        // line changes to show the live prompt.
        Mode::LangPicker | Mode::LangFilter => draw_lang_picker(frame, app, content_area),
        // The palette (FR-CS-1) and onboarding (FR-CS-8) are modal overlays
        // painted after the status bar below; the reading view sits behind them.
        Mode::Palette | Mode::Onboarding => draw_reading(frame, app, content_area),
        // PRD FR-NV-4/5's `K` peek popup overlays the reading view, drawn
        // after the status bar below like the help/offline overlays.
        Mode::Peek => draw_reading(frame, app, content_area),
        // PRD §10's `i`/`:info` overlay: same "overlay the reading view"
        // treatment as the peek popup above.
        Mode::Info => draw_reading(frame, app, content_area),
        // PRD FR-ACC-2/3/4: three picker-style views, same idiom as
        // `Mode::OnThisDay`/`Mode::Related` above.
        Mode::Watchlist => draw_watchlist(frame, app, content_area),
        Mode::Notifications => draw_notifications(frame, app, content_area),
        Mode::Contribs => draw_contribs(frame, app, content_area),
        // PRD FR-ACC-7: a floating card, same treatment as `Mode::Info`.
        Mode::Prefs => draw_reading(frame, app, content_area),
        // PRD FR-ML-4's bare `:wiki` picker — same list-picker idiom as
        // `Mode::TabPicker`/`Mode::HistoryPicker` above.
        Mode::WikiPicker => draw_wiki_picker(frame, app, content_area),
    }

    draw_status_bar(frame, app, status_area);

    if app.mode == Mode::OfflineCard {
        draw_offline_card(frame, app, area);
    }

    if app.mode == Mode::RedlinkCard {
        draw_redlink_card(frame, app, area);
    }

    // PRD FR-SR-3b: the operator cheat-sheet takes priority over the
    // typeahead dropdown below — showing both at once would be clutter, and
    // the cheat-sheet's own key handling (`main::handle_key`) already
    // swallows every key but `?`/Esc while it's up, so the dropdown
    // wouldn't be interactive underneath it anyway.
    if app.mode == Mode::Search && app.search_operator_help {
        draw_operator_cheatsheet(frame, app, content_area);
    } else if app.mode == Mode::Search && !app.typeahead.is_empty() {
        // The typeahead dropdown floats over the reading view, anchored
        // just above the prompt it belongs to (PRD FR-SR-1) — drawn after
        // the status bar so it layers on top, same ordering as the help
        // overlay below.
        draw_search_suggestions(frame, app, content_area);
    }

    if app.mode == Mode::Help {
        draw_help_overlay(frame, app, area);
    }

    // PRD FR-NV-4/5's `K` peek popup: a floating card over the reading view.
    if app.mode == Mode::Peek {
        draw_peek_popup(frame, app, area);
    }

    // PRD §10's `i`/`:info` overlay: same floating-card treatment as peek.
    if app.mode == Mode::Info {
        draw_info_overlay(frame, app, area);
    }

    // PRD FR-ACC-7's read-only prefs card: same floating-card treatment.
    if app.mode == Mode::Prefs {
        draw_prefs_overlay(frame, app, area);
    }

    // PRD FR-CS-1's command palette and FR-CS-8's onboarding: modal overlays,
    // drawn last so they layer over everything (same ordering as help above).
    if app.mode == Mode::Palette {
        draw_palette(frame, app, area);
    }
    if app.mode == Mode::Onboarding {
        draw_onboarding(frame, app, area);
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
    build_tab_bar_indexed(labels, width)
        .into_iter()
        .map(|(_, seg)| seg)
        .collect()
}

/// PRD FR-NV-9: which tab (original index into `labels`) sits under column
/// `col` of a tab bar built at `width` — shares `build_tab_bar_indexed` with
/// the painter above so a click can never disagree with what's actually
/// drawn. `None` past the last rendered segment (clicking blank tab-bar
/// space is a no-op, not a guess).
pub fn tab_bar_hit_test(labels: &[TabLabel], width: usize, col: usize) -> Option<usize> {
    let mut acc = 0usize;
    for (idx, seg) in build_tab_bar_indexed(labels, width) {
        let w = display_width(&seg.text);
        if col >= acc && col < acc + w {
            return Some(idx);
        }
        acc += w;
    }
    None
}

/// The shared implementation behind `build_tab_bar`/`tab_bar_hit_test`: same
/// segments, paired with each one's original index into `labels` so a hit
/// test can report *which tab* a column belongs to, not just its text.
fn build_tab_bar_indexed(labels: &[TabLabel], width: usize) -> Vec<(usize, TabBarSegment)> {
    if labels.is_empty() {
        return Vec::new();
    }
    let full: Vec<String> = labels.iter().map(|l| render_tab_segment(l, 24)).collect();
    let total: usize = full.iter().map(|s| display_width(s)).sum();
    if total <= width {
        return labels
            .iter()
            .zip(full)
            .enumerate()
            .map(|(i, (l, text))| {
                (
                    i,
                    TabBarSegment {
                        text,
                        active: l.active,
                    },
                )
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
    chosen
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
    // PRD FR-TB-4 / FR-ML-3: a split renders two panes side by side instead of
    // the single reading column. Additive — every mode that draws the reading
    // view (Reading, and overlays like Command/Help/Peek behind them) routes
    // through here, so the split shows behind those overlays too.
    if app.split.is_some() {
        draw_split(frame, app, area);
        return;
    }
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
        let redlinks = confirmed_redlink_titles(app);
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
                &redlinks,
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

/// PRD FR-TB-4 / FR-ML-3: draw the two-pane split. The content area is divided
/// into left | one-column divider | right, each pane rendering its tab's
/// document laid out at the pane's own (narrower) width. A per-pane header row
/// shows the article title + language, highlighted for the focused pane (which
/// receives keys); a bilingual split adds a top banner making clear the two
/// editions are independent articles, not a translation (FR-ML-3 UX copy). The
/// focused pane's layout/width/height are mirrored back onto the app-global
/// fields so find, section-jump, and link-focus (which read `app.layout`)
/// operate on the focused pane at its pane width.
fn draw_split(frame: &mut Frame, app: &mut App, area: Rect) {
    let (panes, focused_slot, bilingual) = {
        let s = app
            .split
            .as_ref()
            .expect("draw_split is only reached with a split active");
        (s.panes, s.focused, s.bilingual)
    };
    let (Some(left_idx), Some(right_idx)) =
        (app.tab_index_by_id(panes[0]), app.tab_index_by_id(panes[1]))
    else {
        // A pane's tab vanished (shouldn't happen — every close path drops the
        // split): degrade to single-pane rather than panic.
        app.split = None;
        draw_reading(frame, app, area);
        return;
    };

    // PRD FR-ML-3 UX copy: the "not a translation" banner above a bilingual
    // split, on the terminal's own status colors so it reads as chrome.
    let mut body = area;
    if bilingual {
        let banner = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        let banner_style = colored_bg(app.no_color, app.theme.status_fg, app.theme.status_bg);
        frame.render_widget(
            Paragraph::new(Line::from(RSpan::styled(
                "interwiki — independent articles in each language, not a translation",
                banner_style.add_modifier(Modifier::BOLD),
            )))
            .style(banner_style),
            banner,
        );
        body = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(1),
        };
    }

    // Two panes plus a one-column vertical divider between them.
    let divider_w = 1u16;
    let avail = body.width.saturating_sub(divider_w);
    let left_w = avail / 2;
    let right_w = avail - left_w;
    let left_area = Rect {
        x: body.x,
        y: body.y,
        width: left_w,
        height: body.height,
    };
    let divider_area = Rect {
        x: body.x + left_w,
        y: body.y,
        width: divider_w,
        height: body.height,
    };
    let right_area = Rect {
        x: body.x + left_w + divider_w,
        y: body.y,
        width: right_w,
        height: body.height,
    };
    // The header eats the pane's top row; content fills the rest.
    let content_h = body.height.saturating_sub(1);

    // Lay each pane out at its own width (an L1 cache lookup/miss — the cache
    // keys on width, so this shares entries with any single-pane view at the
    // same width).
    let left_layout = app.layout_for_tab(left_idx, left_w);
    let right_layout = app.layout_for_tab(right_idx, right_w);

    // Clamp each pane's scroll to its own laid-out length; record max_scroll so
    // scroll-sync and the scroll keys stay in bounds per pane.
    for (idx, layout) in [(left_idx, &left_layout), (right_idx, &right_layout)] {
        let total = layout.as_ref().map(|l| l.lines.len() as u16).unwrap_or(0);
        let max_scroll = total.saturating_sub(content_h.max(1));
        let t = &mut app.tabs[idx];
        t.max_scroll = max_scroll;
        t.scroll = t.scroll.min(max_scroll);
    }

    // Refresh the split's section-boundary line cache (one-frame-stale, per the
    // field's doc comment) so SyncMode::Section can align without relayout.
    let left_sec = left_layout
        .as_ref()
        .map(|l| pane_section_lines(&app.tabs[left_idx], l))
        .unwrap_or_default();
    let right_sec = right_layout
        .as_ref()
        .map(|l| pane_section_lines(&app.tabs[right_idx], l))
        .unwrap_or_default();
    if let Some(s) = app.split.as_mut() {
        s.pane_section_lines = [left_sec, right_sec];
    }

    // Per-pane content rects (header on top).
    let pane_content = |pane: Rect| Rect {
        x: pane.x,
        y: pane.y + 1,
        width: pane.width,
        height: pane.height.saturating_sub(1),
    };
    let left_content = pane_content(left_area);
    let right_content = pane_content(right_area);

    // Mirror the focused pane's geometry/layout onto the app-global fields the
    // reading-view helpers read (find, section jump, link-focus, mouse hit).
    let (focused_content, focused_w, focused_layout) = if focused_slot == 0 {
        (left_content, left_w, &left_layout)
    } else {
        (right_content, right_w, &right_layout)
    };
    app.layout_width = focused_w;
    app.viewport_height = content_h.max(1);
    app.last_content_area = focused_content;
    app.layout = focused_layout.clone();

    // Paint the divider as a full-height rule.
    let divider_style = colored(app.no_color, app.theme.dim);
    let divider_lines: Vec<Line> = (0..divider_area.height)
        .map(|_| Line::from(RSpan::styled("│", divider_style)))
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(divider_lines)).style(base_style(&app.theme, app.no_color)),
        divider_area,
    );

    paint_pane(
        frame,
        app,
        left_area,
        left_content,
        left_idx,
        left_layout.as_ref(),
        focused_slot == 0,
    );
    paint_pane(
        frame,
        app,
        right_area,
        right_content,
        right_idx,
        right_layout.as_ref(),
        focused_slot == 1,
    );
}

/// The sorted, deduped set of laid-out line positions at which `tab`'s sections
/// begin, for a given `layout` — the section-boundary list PRD FR-ML-3's
/// heuristic scroll-sync ([`crate::split::section_synced_scroll`]) aligns on.
fn pane_section_lines(tab: &crate::tab::Tab, layout: &crate::layout::Layout) -> Vec<u16> {
    let mut v: Vec<u16> = tab
        .sections
        .iter()
        .filter_map(|s| layout.block_lines.get(s.block).copied())
        .map(|l| l as u16)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Paint one split pane (PRD FR-TB-4): a header row (article title + language,
/// highlighted when `focused`) above the laid-out content scrolled to the tab's
/// own offset. The unfocused pane's header is dimmed — the focus indicator.
#[allow(clippy::too_many_arguments)]
fn paint_pane(
    frame: &mut Frame,
    app: &App,
    _pane_area: Rect,
    content_area: Rect,
    tab_idx: usize,
    layout: Option<&crate::layout::Layout>,
    focused: bool,
) {
    let tab = &app.tabs[tab_idx];
    let header_area = Rect {
        x: content_area.x,
        y: content_area.y.saturating_sub(1),
        width: content_area.width,
        height: 1,
    };
    let header_text = format!("{} ({})", tab.display_title(), tab.lang);
    let header_style = if focused {
        colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        colored(app.no_color, app.theme.dim)
    };
    frame.render_widget(
        Paragraph::new(Line::from(RSpan::styled(header_text, header_style)))
            .style(base_style(&app.theme, app.no_color)),
        header_area,
    );

    match layout {
        Some(layout) => {
            let visited = visited_titles_for(app, tab);
            let redlinks = confirmed_redlink_titles_for(app, &tab.wiki, &tab.lang);
            let text = paint_document(
                &layout.lines,
                tab.focused_link,
                &tab.links,
                &visited,
                &redlinks,
                &app.theme,
                app.no_color,
                &tab.find_occurrences,
                tab.find_index,
                &app.image_store,
            );
            frame.render_widget(
                Paragraph::new(text)
                    .style(base_style(&app.theme, app.no_color))
                    .scroll((tab.scroll, 0)),
                content_area,
            );
        }
        None => {
            frame.render_widget(
                Paragraph::new(RSpan::styled(
                    "(empty pane)",
                    colored(app.no_color, app.theme.dim),
                ))
                .style(base_style(&app.theme, app.no_color)),
                content_area,
            );
        }
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
        Line::from("Press / to search Wikipedia, Ctrl-T to cycle themes, ? for help, q to quit."),
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
            // PRD FR-DL-3: the quality badge, prefixed on the title exactly
            // like the status bar's own current-article badge — session
            // cache only (`App::quality_cache`, populated in one batched call
            // right after the search that produced these results), so a wiki
            // with no PageAssessments support (or an unassessed title) simply
            // shows no prefix.
            let title = match app.quality_badge_for(app.active_wiki_scope(), &r.title) {
                Some(badge) => format!("{badge} {}", r.title),
                None => r.title.clone(),
            };
            let mut lines = vec![Line::from(RSpan::styled(
                title,
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
            if let Some(meta) = result_meta_line(r, app.reading_wpm) {
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

    // PRD FR-SR-7 / §7's "offline-results section": a header label, distinct
    // from each result's own "(offline · saved|cached)" description line
    // (`main::run_offline_search`) — the header marks the whole list, the
    // per-row text marks each result's provenance.
    let title = if app.results_offline {
        format!(
            "Offline results for \"{}\" ({} found)",
            app.search_input,
            app.results.len()
        )
    } else {
        format!(
            "Results for \"{}\" ({} found)",
            app.search_input,
            app.results.len()
        )
    };
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_result);
}

/// PRD FR-NV-9: row→result-index hit test for a mouse click on the Results
/// list. Mirrors `draw_results`'s own per-item line count exactly (title,
/// optional description, optional highlighted excerpt, optional meta line)
/// so the two can never drift apart — see `result_item_line_count`.
///
/// Returns `None` when the click falls outside every rendered item,
/// including whenever the results don't all fit the viewport at once:
/// ratatui's stateful `List` auto-scrolls internally in that case and does
/// not expose the scroll offset it chose back to the caller, so a click on a
/// scrolled list cannot be mapped to a specific entry without risking
/// selecting the wrong one. The keyboard (`j`/`k`) remains the reliable way
/// to reach an off-screen entry — mouse support stays strictly additive
/// (PRD FR-ACS-3).
pub fn results_row_to_index(app: &App, area: Rect, row: u16) -> Option<usize> {
    if app.results.is_empty() {
        return None;
    }
    let inner_top = area.y + 1;
    let inner_height = area.height.saturating_sub(2);
    let heights: Vec<u16> = app
        .results
        .iter()
        .map(|r| result_item_line_count(r, app.reading_wpm) as u16)
        .collect();
    let total: u16 = heights.iter().sum();
    if total > inner_height || row < inner_top {
        return None;
    }
    let mut acc = inner_top;
    for (i, h) in heights.iter().enumerate() {
        if row < acc + h {
            return Some(i);
        }
        acc += h;
    }
    None
}

/// The exact number of lines `draw_results` renders for one result — title
/// (always), description (iff present), a highlighted excerpt (iff present
/// and not all-whitespace, matching `draw_results`'s own filter), and the
/// size/wordcount/date meta line (iff `result_meta_line` has anything to
/// show).
fn result_item_line_count(r: &crate::api::SearchResult, reading_wpm: u32) -> usize {
    let mut n = 1;
    if r.description.is_some() {
        n += 1;
    }
    if let Some(excerpt) = &r.excerpt {
        let runs = parse_searchmatch(excerpt);
        if runs.iter().any(|(text, _)| !text.trim().is_empty()) {
            n += 1;
        }
    }
    if result_meta_line(r, reading_wpm).is_some() {
        n += 1;
    }
    n
}

/// PRD FR-NV-9: row→index hit test for any single-line-per-item selectable
/// list built via `render_selectable_list` (TOC, tab picker, bookmark
/// picker, ...) — every one of them renders exactly one line per entry
/// inside a `Borders::ALL` block, so the mapping is the same arithmetic
/// everywhere. Same "only when it fits the viewport" limitation as
/// `results_row_to_index` — see its doc comment.
pub fn single_line_list_row_to_index(area: Rect, item_count: usize, row: u16) -> Option<usize> {
    if item_count == 0 {
        return None;
    }
    let inner_top = area.y + 1;
    let inner_height = area.height.saturating_sub(2);
    if item_count as u16 > inner_height || row < inner_top {
        return None;
    }
    let idx = (row - inner_top) as usize;
    (idx < item_count).then_some(idx)
}

/// FR-SR-2's "size, wordcount, last-edit date" line, plus FR-RD-11's reading
/// time wherever a result already carries a `wordcount` (the search API's
/// own field — no extra fetch needed for this one, unlike the current
/// article's estimate which needs the full document model): omits whatever
/// fields the endpoint didn't provide, and the whole line if it provided
/// none — callers never see a line of empty punctuation.
fn result_meta_line(r: &crate::api::SearchResult, reading_wpm: u32) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(size) = r.size {
        parts.push(human_bytes(size));
    }
    if let Some(words) = r.wordcount {
        parts.push(format!("{words} words"));
        let minutes = crate::doc::reading_minutes(words, reading_wpm);
        if minutes > 0 {
            parts.push(format!("{minutes} min read"));
        }
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
    // PRD FR-SR-7: an offline zero-results screen gets its own graceful
    // message — "did you mean" is an online-only affordance
    // (`app.search_suggestion` is always `None` here for an offline search,
    // see `main::run_offline_search`), so a plain "no offline results" line
    // is more honest than a message shaped for a suggestion that never
    // comes.
    let message = if app.results_offline {
        format!("No offline results for \"{}\"", app.search_input)
    } else {
        crate::app::zero_results_message(&app.search_input, app.search_suggestion.as_deref())
    };
    let text = Text::from(vec![
        Line::from(""),
        Line::from(RSpan::styled(
            message,
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ]);
    let title = if app.results_offline {
        format!("Offline results for \"{}\" (0 found)", app.search_input)
    } else {
        format!("Results for \"{}\" (0 found)", app.search_input)
    };
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

/// PRD FR-SR-3b: the search prompt's operator cheat-sheet, toggled by `?`.
/// A static overlay (no selection state) listing every operator
/// `search_ops::OPERATORS` documents, one line each — argument completion
/// (category names, template names, …) is out of scope, noted via the
/// operators' own descriptions rather than a separate caveat line.
fn draw_operator_cheatsheet(frame: &mut Frame, app: &App, area: Rect) {
    let width = 64.min(area.width.saturating_sub(4)).max(20);
    let height = (search_ops::OPERATORS.len() as u16 + 5)
        .min(area.height.saturating_sub(2))
        .max(8);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let mut lines = vec![
        Line::from(RSpan::styled(
            "Search operators",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for op in search_ops::OPERATORS {
        lines.push(Line::from(vec![
            RSpan::styled(
                format!("{}:", op.name),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            RSpan::raw(format!(" {}", op.about)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(RSpan::styled(
        "Tab completes an operator name   ?/Esc: close",
        colored(app.no_color, app.theme.dim),
    )));

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(base_style(&app.theme, app.no_color))
            .block(UiBlock::default().borders(Borders::ALL).title("Operators")),
        popup,
    );
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

/// Bare `:wiki`'s picker (PRD FR-ML-4): Wikipedia + the four sister
/// projects, each row showing its display name and interwiki prefix
/// (`wikt`/`voy`/`q`/`n` — the deep-link forms `target::parse` accepts); the
/// currently active wiki is marked, not just highlighted, so it's still
/// visible after the selection cursor moves off it.
fn draw_wiki_picker(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = crate::sisters::all_known_projects()
        .iter()
        .enumerate()
        .map(|(i, project)| {
            let active_marker = if project.name == app.active_wiki_name {
                "● "
            } else {
                "  "
            };
            let prefix = project
                .interwiki_prefix
                .map(|p| format!("   {p}:"))
                .unwrap_or_default();
            let line = Line::from(vec![
                RSpan::styled(
                    format!("{active_marker}{}", project.display_name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::styled(prefix, colored(app.no_color, app.theme.dim)),
            ]);
            let style = if i == app.selected_wiki_pick {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let title = format!(
        "Wiki (active: {}) — Enter: switch  Esc: cancel",
        app.active_wiki_name
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, area, app.selected_wiki_pick);
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

/// PRD FR-PF-3 / FR-PF-4's `:interests` panel — the interest-model inspector.
/// "The whole model is human-readable": it shows the learning on/off/incognito
/// state, the decay half-life, then every tracked topic with its affinity
/// score (a small bar plus the number), and the current morelike seeds. A
/// paint function — reads the model, mutates nothing.
fn draw_interests(frame: &mut Frame, app: &App, area: Rect) {
    let model = &app.interest;
    let top = model.top_categories(40);

    let state = if app.incognito {
        "OFF (incognito — not learning this session)".to_string()
    } else if app.interest_learning {
        "ON".to_string()
    } else {
        "OFF (interest_learning = false)".to_string()
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        RSpan::styled("learning: ", colored(app.no_color, app.theme.dim)),
        RSpan::styled(state, Style::default().add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(RSpan::styled(
        format!(
            "{} topics · half-life {:.0}d · local only, never leaves this machine",
            model.len(),
            model.half_life_days(),
        ),
        colored(app.no_color, app.theme.dim),
    )));
    lines.push(Line::from(""));

    if model.is_empty() {
        lines.push(Line::from(RSpan::styled(
            "No topics yet — read a few articles (and bookmark or save the ones you like).",
            colored(app.no_color, app.theme.dim),
        )));
    } else {
        // Scale the bars to the largest-magnitude score so the display is
        // readable regardless of absolute values.
        let peak = top
            .iter()
            .map(|(_, s)| s.abs())
            .fold(0.0_f64, f64::max)
            .max(1.0);
        for (cat, score) in &top {
            let filled = ((score.abs() / peak) * 16.0).round() as usize;
            let bar: String = "█".repeat(filled.min(16));
            let bar_style = if *score < 0.0 {
                colored(app.no_color, app.theme.warning)
            } else {
                colored(app.no_color, app.theme.link)
            };
            lines.push(Line::from(vec![
                RSpan::styled(format!("{score:>7.2}  "), Style::default()),
                RSpan::styled(format!("{bar:<16} "), bar_style),
                RSpan::styled(cat.clone(), Style::default().add_modifier(Modifier::BOLD)),
            ]));
        }
    }

    // The morelike seeds the model would prefetch from — the reason strings
    // made visible without waiting for the prefetch log.
    let recent: Vec<String> = app
        .history
        .recent(30)
        .into_iter()
        .filter(|v| v.lang == app.lang)
        .map(|v| v.title)
        .collect();
    let seeds = model.morelike_seeds(&recent, 3);
    if !seeds.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(RSpan::styled(
            "Prefetch seeds:",
            colored(app.no_color, app.theme.dim),
        )));
        for s in &seeds {
            lines.push(Line::from(RSpan::styled(
                format!(
                    "  {}",
                    crate::prefetch::morelike_reason(&s.category, s.affinity)
                ),
                colored(app.no_color, app.theme.dim),
            )));
        }
    }

    let title = format!("Interests ({} topics) — Esc: close", model.len());
    let para = Paragraph::new(lines)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    frame.render_widget(para, area);
}

/// PRD FR-PC-3's `:stats` view — local reading stats: articles read, total
/// time, streaks, and the interest model's topic distribution. Suppressed by
/// incognito upstream (incognito never writes history/interest), so it simply
/// shows whatever non-incognito reading produced. A paint function.
fn draw_stats(frame: &mut Frame, app: &App, area: Rect) {
    let stats = app.reading_stats();
    let mut lines: Vec<Line> = Vec::new();

    let row = |label: &str, value: String| {
        Line::from(vec![
            RSpan::styled(format!("{label:<16}"), colored(app.no_color, app.theme.dim)),
            RSpan::styled(value, Style::default().add_modifier(Modifier::BOLD)),
        ])
    };
    lines.push(row("articles read", stats.distinct_articles.to_string()));
    lines.push(row("page loads", stats.total_visits.to_string()));
    lines.push(row(
        "reading time",
        crate::stats::human_duration(stats.total_time_secs),
    ));
    lines.push(row(
        "current streak",
        format!("{} day(s)", stats.current_streak_days),
    ));
    lines.push(row(
        "longest streak",
        format!("{} day(s)", stats.longest_streak_days),
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(RSpan::styled(
        "Top topics (from the interest model):",
        colored(app.no_color, app.theme.dim),
    )));
    if stats.top_topics.is_empty() {
        lines.push(Line::from(RSpan::styled(
            "  (none yet)",
            colored(app.no_color, app.theme.dim),
        )));
    } else {
        for (cat, score) in &stats.top_topics {
            lines.push(Line::from(vec![
                RSpan::styled(
                    format!("  {score:>6.2}  "),
                    colored(app.no_color, app.theme.link),
                ),
                RSpan::styled(cat.clone(), Style::default()),
            ]));
        }
    }
    if app.incognito {
        lines.push(Line::from(""));
        lines.push(Line::from(RSpan::styled(
            "Incognito: this session's reading is not counted.",
            colored(app.no_color, app.theme.dim),
        )));
    }

    let para = Paragraph::new(lines)
        .style(base_style(&app.theme, app.no_color))
        .block(
            UiBlock::default()
                .borders(Borders::ALL)
                .title("Reading stats — Esc: close"),
        );
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

/// PRD FR-SR-6's Related panel: `morelike:{title}` results for the article
/// on screen, mirroring `draw_results`'s title+description list styling.
/// Distinct empty states for "still fetching" (`related_loading`) vs.
/// "fetched, nothing came back" — never the same blank list for both.
fn draw_related(frame: &mut Frame, app: &App, area: Rect) {
    let heading = match &app.active_tab().doc {
        Some(doc) => format!("Related to \"{}\"", doc.title),
        None => "Related".to_string(),
    };
    if app.related_loading {
        let text = Text::from(vec![
            Line::from(""),
            Line::from("Loading related articles…"),
        ]);
        frame.render_widget(
            Paragraph::new(text)
                .style(base_style(&app.theme, app.no_color))
                .block(UiBlock::default().borders(Borders::ALL).title(heading)),
            area,
        );
        return;
    }
    let items_data = app.related_items();
    if items_data.is_empty() {
        let text = Text::from(vec![Line::from(""), Line::from(app.status.clone())]);
        frame.render_widget(
            Paragraph::new(text)
                .style(base_style(&app.theme, app.no_color))
                .block(UiBlock::default().borders(Borders::ALL).title(heading)),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = items_data
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
            let style = if i == app.selected_related {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(lines).style(style)
        })
        .collect();
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(heading));
    render_selectable_list(frame, list, area, app.selected_related);
}

/// PRD FR-ACC-2's watchlist pane: two tabs, same layout as `draw_on_this_day`
/// (a one-row tab strip over a selectable list) — "Watched pages" (the raw
/// list) and "Recent changes" (the since-last-seen activity feed).
fn draw_watchlist(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = UiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let tabs = [app::WatchlistTab::Pages, app::WatchlistTab::Changes];
    let tab_spans: Vec<RSpan> = tabs
        .iter()
        .map(|&t| {
            let style = if t == app.watchlist_tab {
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

    let (items, len): (Vec<ListItem>, usize) = match app.watchlist_tab {
        app::WatchlistTab::Pages => {
            let items = app
                .watchlist_raw
                .iter()
                .enumerate()
                .map(|(i, title)| {
                    let style = if i == app.watchlist_selected {
                        colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Line::from(title.clone())).style(style)
                })
                .collect();
            (items, app.watchlist_raw.len())
        }
        app::WatchlistTab::Changes => {
            let items = app
                .watchlist_changes
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let mut lines = vec![Line::from(RSpan::styled(
                        c.title.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ))];
                    let comment = c.comment.as_deref().unwrap_or("");
                    lines.push(Line::from(RSpan::styled(
                        format!("{} · {} · {comment}", c.timestamp, c.user),
                        colored(app.no_color, app.theme.dim),
                    )));
                    let style = if i == app.watchlist_selected {
                        colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
                    } else {
                        Style::default()
                    };
                    ListItem::new(lines).style(style)
                })
                .collect();
            (items, app.watchlist_changes.len())
        }
    };
    let title = format!(
        "Watchlist — {} ({len}) — Enter: open  Tab/h/l: switch  Esc: close",
        app.watchlist_tab.label()
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, chunks[1], app.watchlist_selected);
}

/// PRD FR-ACC-3's notifications pane: two tabs (Alerts / Messages), same
/// shape as `draw_watchlist` above. An already-read entry renders dimmed so
/// unread ones stand out at a glance, mirroring the visited-link styling
/// convention (FR-HS-2) rather than inventing a new one.
fn draw_notifications(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = UiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let tabs = [app::NotifTab::Alerts, app::NotifTab::Messages];
    let tab_spans: Vec<RSpan> = tabs
        .iter()
        .map(|&t| {
            let style = if t == app.notif_tab {
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

    let list_data = match app.notif_tab {
        app::NotifTab::Alerts => &app.notif_alerts,
        app::NotifTab::Messages => &app.notif_messages,
    };
    let items: Vec<ListItem> = list_data
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let base = if n.read {
                colored(app.no_color, app.theme.dim)
            } else {
                Style::default()
            };
            let mark = if n.read { "  " } else { "\u{25cf} " };
            let style = if i == app.notif_selected {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                base
            };
            ListItem::new(Line::from(format!("{mark}{}", n.text))).style(style)
        })
        .collect();
    let title = format!(
        "Notifications — {} ({}) — d: mark read  A: mark all  Tab/h/l: switch  Esc: close",
        app.notif_tab.label(),
        list_data.len()
    );
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(title));
    render_selectable_list(frame, list, chunks[1], app.notif_selected);
}

/// PRD FR-ACC-4's contributions view: a single selectable list (no tabs),
/// each row the title plus a dimmed second line of timestamp/comment/size
/// delta — the size delta signed and colored the way a diff stat reads
/// everywhere else (`+N` grown, `-N` shrunk).
fn draw_contribs(frame: &mut Frame, app: &App, area: Rect) {
    let heading = if app.contribs_username.is_empty() {
        "Contributions".to_string()
    } else {
        format!("Contributions — {}", app.contribs_username)
    };
    if app.contribs.is_empty() {
        let text = Text::from(vec![Line::from(""), Line::from(app.status.clone())]);
        frame.render_widget(
            Paragraph::new(text)
                .style(base_style(&app.theme, app.no_color))
                .block(UiBlock::default().borders(Borders::ALL).title(heading)),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = app
        .contribs
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut lines = vec![Line::from(RSpan::styled(
                c.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            let comment = c.comment.as_deref().unwrap_or("");
            let delta = if c.sizediff >= 0 {
                format!("+{}", c.sizediff)
            } else {
                c.sizediff.to_string()
            };
            lines.push(Line::from(RSpan::styled(
                format!("{} · {delta} · {comment}", c.timestamp),
                colored(app.no_color, app.theme.dim),
            )));
            let style = if i == app.contribs_selected {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(lines).style(style)
        })
        .collect();
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(heading));
    render_selectable_list(frame, list, area, app.contribs_selected);
}

/// PRD FR-ACC-7's read-only prefs card: same floating-card treatment as
/// `draw_info_overlay` (title/URL/... key-value block) — skin, language,
/// email-confirmed, edit count, and nothing writable anywhere on it.
fn draw_prefs_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let width = 60.min(area.width.saturating_sub(4)).max(24);
    let height = 9.min(area.height.saturating_sub(2)).max(6);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let dim = colored(app.no_color, app.theme.dim);
    let label = |text: &'static str| RSpan::styled(text, dim.add_modifier(Modifier::BOLD));
    let body = match &app.prefs {
        Some(prefs) => Text::from(vec![
            Line::from(vec![
                label("Skin:       "),
                RSpan::raw(prefs.skin.clone().unwrap_or_else(|| "—".to_string())),
            ]),
            Line::from(vec![
                label("Language:   "),
                RSpan::raw(prefs.language.clone().unwrap_or_else(|| "—".to_string())),
            ]),
            Line::from(vec![
                label("Email:      "),
                RSpan::raw(if prefs.email_confirmed {
                    "confirmed".to_string()
                } else {
                    "not confirmed".to_string()
                }),
            ]),
            Line::from(vec![
                label("Edit count: "),
                RSpan::raw(
                    prefs
                        .editcount
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "—".to_string()),
                ),
            ]),
        ]),
        None => Text::from(""),
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body)
            .style(base_style(&app.theme, app.no_color))
            .wrap(Wrap { trim: false })
            .block(
                UiBlock::default()
                    .borders(Borders::ALL)
                    .title("Preferences (read-only)"),
            ),
        popup,
    );
}

/// PRD FR-ML-1's `:lang` picker: the article on screen's langlinks,
/// preferred-pinned and fuzzy-filtered (`App::lang_picker_rows`). Each row
/// shows the autonym plus the English langname in parentheses ("Deutsch
/// (German)") so a reader who can't read the target script still knows
/// which edition a row is, with the short code dimmed alongside it and a
/// `*` marking a pinned (preferred) row.
fn draw_lang_picker(frame: &mut Frame, app: &App, area: Rect) {
    let heading = match &app.active_tab().doc {
        Some(doc) => format!("Language editions of \"{}\"", doc.title),
        None => "Language editions".to_string(),
    };
    if app.lang_loading {
        let text = Text::from(vec![
            Line::from(""),
            Line::from("Loading language editions…"),
        ]);
        frame.render_widget(
            Paragraph::new(text)
                .style(base_style(&app.theme, app.no_color))
                .block(UiBlock::default().borders(Borders::ALL).title(heading)),
            area,
        );
        return;
    }
    let rows = app.lang_picker_rows();
    if rows.is_empty() {
        let text = Text::from(vec![Line::from(""), Line::from(app.status.clone())]);
        frame.render_widget(
            Paragraph::new(text)
                .style(base_style(&app.theme, app.no_color))
                .block(UiBlock::default().borders(Borders::ALL).title(heading)),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let pinned = app.languages.iter().any(|p| p == &l.code);
            let marker = if pinned { "* " } else { "  " };
            let line = Line::from(vec![
                RSpan::styled(
                    format!("{marker}{} ({})", l.autonym, l.langname),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::styled(
                    format!("   [{}]", l.code),
                    colored(app.no_color, app.theme.dim),
                ),
            ]);
            let style = if i == app.selected_lang {
                colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();
    let list = List::new(items)
        .style(base_style(&app.theme, app.no_color))
        .block(UiBlock::default().borders(Borders::ALL).title(heading));
    render_selectable_list(frame, list, area, app.selected_lang);
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

/// PRD FR-DL-5 / §7's "Redlink followed" card: shown instead of attempting a
/// fetch that would just 404, since a redlink is already known not to exist
/// (Parsoid's `class="new"` or the batched info check — see
/// `App::show_redlink_card`). Offers the two documented actions: search for
/// a similar title, or yank the wiki's own "create this page" URL.
fn draw_redlink_card(frame: &mut Frame, app: &App, area: Rect) {
    let width = 58.min(area.width.saturating_sub(4)).max(20);
    let height = 9.min(area.height.saturating_sub(2)).max(7);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let title = app
        .redlink_card_target
        .as_ref()
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    let body = Text::from(vec![
        Line::from(RSpan::styled(
            "Article doesn't exist yet",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("\"{title}\" has no article on this wiki yet.")),
        Line::from(""),
        Line::from("s  search for a similar title"),
        Line::from("y  yank the create-page URL"),
        Line::from("Esc  dismiss"),
    ]);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body)
            .style(base_style(&app.theme, app.no_color))
            .block(UiBlock::default().borders(Borders::ALL).title("Redlink")),
        popup,
    );
}

/// PRD FR-NV-4/5's `K` peek popup: a footnote peek (a reference's text,
/// resolved locally) or a link preview (a target's title, Wikidata
/// description, and lead extract). The two are visually distinct by their
/// border label ("Reference [n]" vs "Preview") so a reader can tell at a
/// glance which kind of peek is up. The body wraps to the popup width, so no
/// laid line overflows the card (`Wrap` is safe here — this is a fixed short
/// card, not the reading view's line-mapped content).
fn draw_peek_popup(frame: &mut Frame, app: &App, area: Rect) {
    let width = 64.min(area.width.saturating_sub(4)).max(24);
    let height = 12.min(area.height.saturating_sub(2)).max(6);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let dim = colored(app.no_color, app.theme.dim);
    let (title, body): (String, Text) = match &app.peek {
        Some(crate::app::PeekPopup::Footnote { marker, text }) => {
            let body = Text::from(vec![
                Line::from(RSpan::styled(
                    "Reference",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(text.clone()),
            ]);
            (format!("Reference {marker}"), body)
        }
        Some(crate::app::PeekPopup::LinkPreview { title, .. }) => {
            let mut lines = vec![Line::from(RSpan::styled(
                title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            match app.peek_summary() {
                Some(summary) => {
                    if !summary.description.is_empty() {
                        lines.push(Line::from(RSpan::styled(
                            summary.description.clone(),
                            dim.add_modifier(Modifier::ITALIC),
                        )));
                    }
                    lines.push(Line::from(""));
                    lines.push(Line::from(if summary.extract.is_empty() {
                        "(no extract available)".to_string()
                    } else {
                        summary.extract.clone()
                    }));
                }
                None => {
                    lines.push(Line::from(""));
                    lines.push(Line::from(RSpan::styled("loading…", dim)));
                }
            }
            ("Preview".to_string(), Text::from(lines))
        }
        None => ("Peek".to_string(), Text::from("")),
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body)
            .style(base_style(&app.theme, app.no_color))
            .wrap(Wrap { trim: false })
            .block(UiBlock::default().borders(Borders::ALL).title(title)),
        popup,
    );
}

/// PRD §10's `i`/`:info` overlay: the article-attribution card — title,
/// canonical URL, revision id, license, and a permalink to the article's
/// history, plus the retrieval date. Read-only, same floating-card idiom as
/// [`draw_peek_popup`]; unlike that popup, this one never shows "loading…" —
/// nothing here is fetched over the network (see `App::open_info`).
fn draw_info_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let width = 76.min(area.width.saturating_sub(4)).max(24);
    let height = 11.min(area.height.saturating_sub(2)).max(6);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let dim = colored(app.no_color, app.theme.dim);
    let label = |text: &'static str| RSpan::styled(text, dim.add_modifier(Modifier::BOLD));
    let body = match &app.info {
        Some(info) => Text::from(vec![
            Line::from(vec![label("Title:      "), RSpan::raw(info.title.clone())]),
            Line::from(vec![
                label("URL:        "),
                RSpan::raw(info.canonical_url.clone()),
            ]),
            Line::from(vec![
                label("Revision:   "),
                RSpan::raw(info.revid.to_string()),
            ]),
            Line::from(vec![
                label("License:    "),
                RSpan::raw(info.license.clone()),
            ]),
            Line::from(vec![
                label("History:    "),
                RSpan::raw(info.history_url.clone()),
            ]),
            Line::from(vec![
                label("Retrieved:  "),
                RSpan::raw(info.retrieved_on.clone()),
            ]),
        ]),
        None => Text::from(""),
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body)
            .style(base_style(&app.theme, app.no_color))
            .wrap(Wrap { trim: false })
            .block(
                UiBlock::default()
                    .borders(Borders::ALL)
                    .title("Article info"),
            ),
        popup,
    );
}

/// PRD FR-PR-3's "visible status glyph": a persistent, plain-text
/// `[incognito]` marker prepended to the status bar in *every* mode (not
/// just Reading — a picker or a prompt is exactly when a reader most needs
/// the reminder that nothing passive is being recorded right now), pure and
/// unit-testable ahead of `draw_status_bar`'s own Frame-drawing plumbing.
/// Plain text rather than an emoji/Unicode glyph (contra "🕶 incognito") so
/// it reads identically under `NO_COLOR` (PRD FR-TH-5, which strips color
/// but never text) and on the VT100-ish terminal floor (§8) that may not
/// shape an emoji at all.
fn with_incognito_glyph(text: String, incognito: bool) -> String {
    if incognito {
        format!("[incognito] {text}")
    } else {
        text
    }
}

/// PRD FR-ACC-1: the logged-in indicator — a `[@username]` prefix on the
/// status bar whenever a session is active, so the reader can always see who
/// they're logged in as (and that they are logged in at all). Absent when
/// logged out. Prefixed like the incognito glyph so it survives whatever
/// segment (`notice`/focused link/breadcrumb) fills the rest of the bar.
/// `badge` is PRD FR-ACC-3's unread-notification suffix (`✉N`, `App::
/// notification_badge`) — folded into the same bracket rather than a second
/// prefix, so a logged-in-with-unread reader sees one glyph, `[@user ✉3]`,
/// not two competing ones.
fn with_login_glyph(text: String, username: Option<&str>, badge: Option<&str>) -> String {
    match username {
        Some(user) => match badge {
            Some(b) => format!("[@{user} {b}] {text}"),
            None => format!("[@{user}] {text}"),
        },
        None => text,
    }
}

/// Modes whose status bar shows the reader's own live input — the search/
/// command/find/hint prompts, the `/` filters, and the command palette.
/// `status_bar_text`'s uniform notice priority skips these: a `notice`
/// showing up here would clobber text the reader is still typing, and for
/// `Command`/`Hint` specifically it's moot anyway — both land back in
/// `Mode::Reading` (see `Mode::Command`'s `Enter` arm and `exit_hint_mode` in
/// `main.rs`) before their own actions ever call `App::notice`'s setter, so
/// any notice they raise is already showing under `Mode::Reading` by the
/// time the next frame draws.
fn mode_shows_input_prompt(mode: Mode) -> bool {
    matches!(
        mode,
        Mode::Search
            | Mode::Command
            | Mode::Find
            | Mode::Hint
            | Mode::BookmarkFilter
            | Mode::BookmarkTagEdit
            | Mode::ReadingHistoryFilter
            | Mode::LangFilter
            | Mode::Palette
    )
}

/// The status bar's text for the current frame, pulled out of
/// `draw_status_bar` so the priority chain is unit-testable without a
/// `Frame`/`TestBackend`.
///
/// A systemic bug independently hit by three prior chunks: `app.notice` — the
/// transient one-keypress-lifetime feedback channel (a save confirmation, a
/// yank, a bookmark toggle, an incognito warning, "External link: ...") — was
/// only ever checked by `Mode::Reading`'s own arm (and only *there* when no
/// link was focused, since the focused-link line took over first). Every
/// other mode's arm was a per-mode hint/status string that never looked at
/// `notice` at all, so feedback set while in Research, on the redlink card,
/// etc. was computed and then silently never drawn. The fix is one early
/// check here — not a copy of the same `if app.notice.is_some()` pasted into
/// every arm below — so a notice, wherever it's set, always outranks the
/// current mode's own hint content in every mode where that makes sense
/// (`mode_shows_input_prompt` carves out the handful where it doesn't).
fn status_bar_text(app: &App, width: u16) -> String {
    let tab = app.active_tab();

    // Reading's own "Loading…" is checked ahead of a notice: it's rarer and
    // more urgent than feedback left over from the keypress that triggered
    // the fetch (preserves this bar's pre-existing order for Reading).
    let reading_loading = app.mode == Mode::Reading && app.loading;
    if !reading_loading
        && !mode_shows_input_prompt(app.mode)
        && let Some(notice) = &app.notice
    {
        return notice.clone();
    }

    match app.mode {
        Mode::Search if app.search_operator_help => {
            "Search operators — ?/Esc: close   Tab: complete operator name".to_string()
        }
        Mode::Search if app.typeahead.is_empty() => format!(
            "/{}   Tab: full-text search   Esc: cancel",
            app.search_input
        ),
        Mode::Search => format!(
            "/{}   \u{2191}/\u{2193} or Ctrl-n/p: move   Enter: open   Tab: full-text search",
            app.search_input
        ),
        Mode::Command => format!(":{}", app.command_input),
        // PRD §5.9's manual code-paste prompt — keeps the authorization URL in
        // view (from the pending login) so the reader can re-open it, then the
        // live paste input.
        Mode::Login => match app.pending_login.as_ref() {
            Some(pending) if app.login_input.is_empty() => {
                format!(
                    "authorize at: {}   then paste code   Esc: cancel",
                    pending.authorize_url
                )
            }
            _ => format!(
                "paste code: {}   Enter: submit   Esc: cancel",
                app.login_input
            ),
        },
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
        Mode::WikiPicker => "Enter: switch wiki   Esc: cancel   j/k: move".to_string(),
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
        Mode::RedlinkCard => {
            "s: search similar titles   y: yank create URL   Esc: dismiss".to_string()
        }
        // PRD FR-NV-4/5: the `K` peek popup carries its own status text
        // (set by `open_peek_at_focus`/`deliver_summary`).
        Mode::Peek => app.status.clone(),
        // PRD §10: the `:info` overlay carries its own status text (set by
        // `open_info`).
        Mode::Info => app.status.clone(),
        Mode::ReadingHistory => app.status.clone(),
        Mode::ReadingHistoryFilter => {
            format!("filter: {}   Esc: apply", app.history_pick_filter)
        }
        Mode::PrefetchLog | Mode::Interests | Mode::Stats => app.status.clone(),
        Mode::OnThisDay => app.status.clone(),
        Mode::Related => app.status.clone(),
        Mode::LangPicker => app.status.clone(),
        Mode::LangFilter => format!("filter: {}   Esc: apply", app.lang_filter_input),
        // PRD FR-ACC-2/3/4/7: each of these carries its own transient status
        // text (set by `main.rs`'s `open_watchlist`/`open_notifications`/
        // `open_contribs`/`open_prefs`, and overwritten by `w`/`d`/`A`/`t`'s
        // own confirmations via `notice`, which the priority check above
        // already outranks this with).
        Mode::Watchlist => app.status.clone(),
        Mode::Notifications => app.status.clone(),
        Mode::Contribs => app.status.clone(),
        Mode::Prefs => app.status.clone(),
        Mode::Help => "j/k: scroll   Esc/?/q: close".to_string(),
        Mode::Palette => format!(
            "> {}   Enter: run   \u{2191}/\u{2193}: move   Esc: cancel",
            app.palette_input
        ),
        Mode::Onboarding => "Press any key to start reading".to_string(),
        Mode::Reading if app.loading => "Loading…".to_string(),
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
        Mode::Reading => {
            // PRD FR-ML-2's "available in your preferred language" hint: a
            // low-priority suffix appended to *every* branch below, not
            // just the least-common one (a focused link — nearly always
            // true, since a fresh document focuses link 0 whenever it has
            // any — would otherwise hide the hint for as long as the
            // reader was looking at any article with links at all).
            // `notice`/find above still outrank it — those are transient,
            // explicit feedback, so they still win outright — but among
            // Reading's own "what's on screen" segments the hint always
            // gets a spot.
            let hint = app
                .language_hint
                .as_deref()
                .map(|h| format!("   {h}"))
                .unwrap_or_default();
            // PRD FR-DL-3: the current article's quality badge, same
            // low-priority-suffix treatment as the language hint — shown
            // whenever this session has an assessment cached for it
            // (`App::current_quality_badge`), silently absent otherwise
            // (unassessed article, non-PageAssessments wiki, or the batched
            // fetch hasn't landed yet).
            let badge = app
                .current_quality_badge()
                .map(|b| format!("   {b}"))
                .unwrap_or_default();
            match tab.focused_link.and_then(|i| tab.links.get(i)) {
                Some(link) if link.internal_title.is_some() => {
                    format!(
                        "{}→ {} ({}/{})   Tab/S-Tab: cycle   Enter: open   H: back   L: forward{hint}{badge}",
                        tab.page_source.prefix(),
                        link.text,
                        tab.focused_link.unwrap() + 1,
                        tab.links.len()
                    )
                }
                Some(link) => format!(
                    "{}→ {} (external, not yet followable){hint}{badge}",
                    tab.page_source.prefix(),
                    link.text
                ),
                // FR-NV-7: with no notice, find, or focused link to show,
                // the default segment is the active tab's breadcrumb
                // trail; falls back to the plain status line before any
                // history has built up.
                None => {
                    let titles = app.breadcrumb_titles();
                    if titles.len() > 1 {
                        let prefix = tab.page_source.prefix();
                        let budget = (width as usize).saturating_sub(display_width(&prefix));
                        format!("{prefix}{}{hint}{badge}", build_breadcrumb(&titles, budget))
                    } else {
                        format!("{}{hint}{badge}", app.status)
                    }
                }
            }
        }
    }
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let text = status_bar_text(app, area.width);
    let text = with_login_glyph(
        text,
        app.logged_in_username(),
        app.notification_badge().as_deref(),
    );
    let text = with_incognito_glyph(text, app.incognito);
    let style = if matches!(
        app.mode,
        Mode::Search
            | Mode::Find
            | Mode::Command
            | Mode::Login
            | Mode::Hint
            | Mode::BookmarkFilter
            | Mode::BookmarkTagEdit
            | Mode::LangFilter
    ) {
        colored_bg(app.no_color, app.theme.focus_fg, app.theme.focus_bg)
    } else {
        colored_bg(app.no_color, app.theme.status_fg, app.theme.status_bg)
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

/// PRD FR-CS-4's context-sensitive help overlay — the direct answer to
/// wiki-tui's most-upvoted issue (#177). The cheatsheet is *generated* from
/// the command registry + active keymap (`registry::reading_help` /
/// `picker_help`), so it can never drift from the real bindings, and it shows
/// the keys the reader has actually configured. It is **scrollable** (j/k,
/// arrows, Ctrl-d/u, g/G — handled in `main::handle_key`) so however tall the
/// content and short the terminal, it can always reach its own bottom: the
/// single fixed-height popup this replaced clipped ~40 rows into ~29.
fn draw_help_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let (title, lines) = help_content(app);

    let width = 66.min(area.width.saturating_sub(4)).max(20);
    // Take most of the screen height; the content scrolls within it.
    let height = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(6);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    // Rows visible inside the border, and the furthest we can usefully scroll.
    let inner_rows = popup.height.saturating_sub(2);
    let max_scroll = (lines.len() as u16).saturating_sub(inner_rows);
    let offset = app.help_scroll.min(max_scroll);
    let heading = if max_scroll > 0 {
        let last = (offset + inner_rows).min(lines.len() as u16);
        format!("{title}  ({}-{}/{})", offset + 1, last, lines.len())
    } else {
        title
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(base_style(&app.theme, app.no_color))
            .scroll((offset, 0))
            .block(UiBlock::default().borders(Borders::ALL).title(heading)),
        popup,
    );
}

/// The number of lines the current help cheatsheet holds — `main::handle_key`
/// uses it to clamp the scroll offset so `?`-help can never over-scroll past
/// its own bottom (PRD FR-CS-4).
pub fn help_view_len(app: &App) -> usize {
    help_content(app).1.len()
}

/// Build the help title and lines for whatever view the overlay was opened
/// from (`app.prior_mode`) — PRD FR-CS-4's per-view cheatsheet.
fn help_content(app: &App) -> (String, Vec<Line<'static>>) {
    let heading = |s: &str| {
        Line::from(RSpan::styled(
            s.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ))
    };
    let row = |key: &str, help: &str| {
        Line::from(vec![
            RSpan::styled(
                format!("{key:<12}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            RSpan::raw(help.to_string()),
        ])
    };

    // The start page is a Reading mode with no document — its own short sheet.
    if app.prior_mode == Mode::Reading && app.active_tab().doc.is_none() {
        let lines = vec![
            heading("Start page"),
            Line::from(""),
            row("j/k", "move the selection"),
            row("Enter", "open the focused item"),
            row("t", "reroll the TIL widget"),
            row("/", "search Wikipedia"),
            row(":", "command line"),
            row("Ctrl-p", "command palette"),
            row("gr", "random article"),
            row("Ctrl-T", "cycle color theme"),
            row("?", "this help"),
            row("q", "close / quit"),
        ];
        return ("Start page — keys".to_string(), lines);
    }

    if app.prior_mode == Mode::Reading {
        let mut lines = vec![heading("Reading"), Line::from("")];
        for r in registry::reading_help(&app.keymap) {
            lines.push(row(&r.key, &r.help));
        }
        return ("Reading — keys".to_string(), lines);
    }

    // Every other view is a picker: the generated navigation sheet plus the
    // concrete extras this particular picker binds.
    let (name, extras) = picker_help_extras(app.prior_mode);
    let mut lines = vec![heading(name), Line::from("")];
    for r in registry::picker_help(&app.keymap) {
        lines.push(row(&r.key, &r.help));
    }
    if !extras.is_empty() {
        lines.push(Line::from(""));
        for (key, help) in extras {
            lines.push(row(key, help));
        }
    }
    (format!("{name} — keys"), lines)
}

/// The human name and the picker-specific extra keys for a picker mode (PRD
/// FR-CS-4). The generic j/k/Enter/Esc/? navigation is generated from the
/// registry (`registry::picker_help`); this fills in each view's own keys.
fn picker_help_extras(mode: Mode) -> (&'static str, &'static [(&'static str, &'static str)]) {
    match mode {
        Mode::Toc => ("Table of contents", &[("Enter", "jump to the section")]),
        Mode::Results => (
            "Search results",
            &[("Enter", "open (or search the suggestion)")],
        ),
        Mode::Research => (
            "Research",
            &[("s/Enter", "save citation"), ("R", "open the library")],
        ),
        Mode::Library => (
            "Library",
            &[
                ("s", "cycle citation style"),
                ("d", "delete"),
                ("e", "export bibliography"),
            ],
        ),
        Mode::TabPicker => ("Tab picker", &[("d", "close the highlighted tab")]),
        Mode::HistoryPicker => ("Back-stack", &[]),
        Mode::BookmarkPicker => (
            "Bookmarks",
            &[
                ("/", "filter (#tag or fuzzy title)"),
                ("t", "edit tags"),
                ("d", "delete"),
            ],
        ),
        Mode::ReadLaterPicker => ("Read-later queue", &[("d", "remove without opening")]),
        Mode::ReadingHistory => (
            "Reading history",
            &[("/", "filter"), ("d", "delete this article's history")],
        ),
        Mode::SavedPicker => ("Saved pages", &[("d", "un-pin")]),
        Mode::OnThisDay => ("On this day", &[("Tab / h l", "switch type")]),
        Mode::Related => ("Related", &[]),
        Mode::LangPicker => (
            "Language editions",
            &[("/", "filter (autonym / langname / code)")],
        ),
        Mode::WikiPicker => ("Wiki", &[]),
        _ => ("Help", &[]),
    }
}

/// PRD FR-CS-1's command palette (`Ctrl-p`): a fuzzy list over every registry
/// command applicable in the context it was opened from, each row showing the
/// display name, its current keybinding, and the one-line help. Enter runs the
/// highlighted command, Esc cancels.
fn draw_palette(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.palette_rows();

    let width = 72.min(area.width.saturating_sub(4)).max(24);
    let height = (rows.len() as u16 + 4)
        .min(area.height.saturating_sub(2))
        .max(6);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    // A visible window around the selection so a long, filtered list still
    // shows the highlighted row (same idiom as the other selectable lists).
    let list_rows = popup.height.saturating_sub(3) as usize;
    let start = app
        .palette_selected
        .saturating_sub(list_rows.saturating_sub(1));
    let key_col = rows
        .iter()
        .map(|r| r.key.chars().count())
        .max()
        .unwrap_or(0)
        .max(3);

    let mut lines: Vec<Line> = vec![
        Line::from(RSpan::styled(
            format!("> {}", app.palette_input),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if rows.is_empty() {
        lines.push(Line::from(RSpan::styled(
            "no matching command",
            colored(app.no_color, app.theme.dim),
        )));
    }
    for (i, r) in rows.iter().enumerate().skip(start).take(list_rows) {
        let style = if i == app.palette_selected {
            colored_bg(app.no_color, app.theme.selected_fg, app.theme.selected_bg)
        } else {
            Style::default()
        };
        lines.push(
            Line::from(vec![
                RSpan::raw(format!("{:<width$}  ", r.display, width = 26)),
                RSpan::styled(
                    format!("{:<key_col$}  ", r.key, key_col = key_col),
                    colored(app.no_color, app.theme.dim),
                ),
                RSpan::styled(r.help.to_string(), colored(app.no_color, app.theme.dim)),
            ])
            .style(style),
        );
    }

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(base_style(&app.theme, app.no_color))
            .block(
                UiBlock::default()
                    .borders(Borders::ALL)
                    .title("Command palette"),
            ),
        popup,
    );
}

/// PRD FR-CS-8's first-run onboarding: a one-screen tour naming the three keys
/// a newcomer needs first, shown once over the start page and dismissed with
/// any key (which also writes the default config so it never shows again).
fn draw_onboarding(frame: &mut Frame, app: &App, area: Rect) {
    let width = 56.min(area.width.saturating_sub(4)).max(24);
    let height = 15.min(area.height.saturating_sub(2)).max(8);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let key = |k: &str, help: &str| {
        Line::from(vec![
            RSpan::styled(
                format!("  {k:<8}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            RSpan::raw(help.to_string()),
        ])
    };
    let lines = vec![
        Line::from(RSpan::styled(
            "Welcome to wikitui",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("A keyboard-first Wikipedia reader. Three keys to start:"),
        Line::from(""),
        key("/", "search Wikipedia"),
        key("?", "help — every key for the current view"),
        key("Ctrl-T", "cycle the color theme"),
        Line::from(""),
        Line::from(RSpan::styled(
            "A config file with commented defaults is being written",
            colored(app.no_color, app.theme.dim),
        )),
        Line::from(RSpan::styled(
            "for you. Press any key to begin.",
            colored(app.no_color, app.theme.dim),
        )),
    ];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(base_style(&app.theme, app.no_color))
            .block(UiBlock::default().borders(Borders::ALL).title("First run")),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{parse_article_html, section_outline};
    use crate::layout::{LayoutOptions, layout_document};

    /// PRD FR-CS-4: the `?` cheatsheet is per-view and generated from the
    /// registry + keymap. The Reading sheet is long (hence scrollable, hence
    /// the overflow fix); a picker sheet names that view and its own keys.
    /// Asserts the model, not pixels.
    #[test]
    fn help_content_is_per_view_and_registry_generated() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(parse_article_html("Alan Turing", "<p>x</p>"));

        // The reading view: a long sheet (would clip a fixed popup — the very
        // bug this replaced), titled from the view.
        app.prior_mode = Mode::Reading;
        let (title, lines) = help_content(&app);
        assert!(title.starts_with("Reading"), "reading title: {title}");
        assert!(
            lines.len() > 30,
            "reading cheatsheet has {} rows — must be scrollable",
            lines.len()
        );
        let reading_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        // A representative registry-sourced key + help pairing.
        assert!(reading_text.contains('j') && reading_text.contains("scroll one line down"));

        // A picker view: named for that view, with its own extra keys plus
        // the generic navigation generated from the registry.
        app.prior_mode = Mode::Toc;
        let (title, lines) = help_content(&app);
        assert!(title.starts_with("Table of contents"), "toc title: {title}");
        let picker_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            picker_text.contains("jump to the section"),
            "picker extra shown: {picker_text}"
        );
        assert!(
            picker_text.contains("selection") || picker_text.contains("confirm"),
            "generic navigation from the registry shown"
        );
    }

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
            &HashSet::new(),
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

    /// PRD FR-DL-5: a redlink (Parsoid's `class="new"`) renders dim/struck
    /// regardless of focus or visited state — distinct from every other link
    /// color the theme defines.
    #[test]
    fn redlink_renders_dim_and_struck_even_when_focused() {
        let html = concat!(
            "<html><body><p>See <a href=\"./Nonexistent\" class=\"new\">a redlink</a>",
            " and <a href=\"./Computer_science\">a real link</a>.</p></body></html>"
        );
        let doc = parse_article_html("Test", html);
        let links = crate::doc::collect_links(&doc);
        assert!(links[0].redlink);
        assert!(!links[1].redlink);

        let theme = Theme::full();
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        // Focus the redlink itself (occurrence 0) — it must still render as
        // a redlink, not the ordinary focus highlight.
        let text = paint_document(
            &layout.lines,
            Some(0),
            &links,
            &HashSet::new(),
            &HashSet::new(),
            &theme,
            false,
            &[],
            0,
            &crate::image::ImageStore::new(),
        );
        let line = text
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("redlink")))
            .expect("paragraph line present");
        let redlink_span = line
            .spans
            .iter()
            .find(|s| s.content.contains("redlink"))
            .unwrap();
        let real_span = line
            .spans
            .iter()
            .find(|s| s.content.contains("real link"))
            .unwrap();
        assert_eq!(redlink_span.style.fg, Some(theme.dim));
        assert!(
            redlink_span
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert_ne!(
            real_span.style.fg,
            Some(theme.dim),
            "an ordinary link must not pick up the redlink styling"
        );
    }

    /// A title confirmed missing by the batched info check (`redlinks`,
    /// distinct from `LinkRef::redlink`) must render the same dim/struck way
    /// — the two detection paths are independent but produce identical
    /// styling.
    #[test]
    fn confirmed_redlink_from_the_batch_check_renders_the_same_as_a_parsoid_one() {
        let html = r#"<html><body><p>See <a href="./Uncharted_Topic">an uncharted topic</a>.</p></body></html>"#;
        let doc = parse_article_html("Test", html);
        let links = crate::doc::collect_links(&doc);
        assert!(
            !links[0].redlink,
            "not pre-marked by Parsoid in this fixture"
        );

        let mut redlinks = HashSet::new();
        redlinks.insert("Uncharted Topic");
        let theme = Theme::full();
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let text = paint_document(
            &layout.lines,
            None,
            &links,
            &HashSet::new(),
            &redlinks,
            &theme,
            false,
            &[],
            0,
            &crate::image::ImageStore::new(),
        );
        let span = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("uncharted"))
            .unwrap();
        assert_eq!(span.style.fg, Some(theme.dim));
        assert!(span.style.add_modifier.contains(Modifier::CROSSED_OUT));
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

    /// PRD FR-NV-9: clicking anywhere inside a tab's own rendered segment
    /// resolves to that tab's index, agreeing with what `build_tab_bar`
    /// actually painted at every column.
    #[test]
    fn tab_bar_hit_test_agrees_with_the_painted_segments() {
        let labels = vec![
            label(1, "Alan Turing", false, true),
            label(2, "Enigma", false, false),
            label(3, "Bletchley Park", false, false),
        ];
        let width = 80;
        let segments = build_tab_bar(&labels, width);
        let mut col = 0usize;
        for (i, seg) in segments.iter().enumerate() {
            let w = display_width(&seg.text);
            for c in col..col + w {
                assert_eq!(
                    tab_bar_hit_test(&labels, width, c),
                    Some(i),
                    "column {c} should hit tab {i} ({:?})",
                    seg.text
                );
            }
            col += w;
        }
        // Past the end of the bar (or its own painted content) is a no-op.
        assert_eq!(tab_bar_hit_test(&labels, width, width + 5), None);
    }

    /// The overflow (collapsed) tab bar still resolves clicks to the correct
    /// *original* tab index, not the compacted position.
    #[test]
    fn tab_bar_hit_test_resolves_the_original_index_on_overflow() {
        let mut labels: Vec<TabLabel> = (1..=17)
            .map(|n| label(n, "Article With A Fairly Long Title", false, false))
            .collect();
        labels[2].active = true; // the 3rd tab (index 2)
        let width = 30;
        let segments = build_tab_bar(&labels, width);
        // The first segment is the compact `[3/17] ...` indicator for tab
        // index 2 (0-based) — clicking its first column must resolve to 2,
        // not to whatever position it occupies in the compacted bar.
        assert!(segments[0].text.contains("[3/17]"));
        assert_eq!(tab_bar_hit_test(&labels, width, 0), Some(2));
    }

    #[test]
    fn tab_bar_hit_test_is_none_when_there_are_no_tabs() {
        assert_eq!(tab_bar_hit_test(&[], 80, 0), None);
    }

    // ---- Mouse click hit-testing for single-line/multi-line pickers -------

    fn area(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn single_line_list_row_to_index_maps_rows_inside_the_border() {
        let a = area(0, 0, 20, 7); // 1 border row top+bottom -> 5 usable rows
        assert_eq!(single_line_list_row_to_index(a, 5, 0), None, "top border");
        assert_eq!(single_line_list_row_to_index(a, 5, 1), Some(0));
        assert_eq!(single_line_list_row_to_index(a, 5, 3), Some(2));
        assert_eq!(single_line_list_row_to_index(a, 5, 5), Some(4));
        assert_eq!(
            single_line_list_row_to_index(a, 5, 6),
            None,
            "bottom border"
        );
    }

    #[test]
    fn single_line_list_row_to_index_is_none_when_scrolled_or_empty() {
        let a = area(0, 0, 20, 7); // 5 usable rows
        assert_eq!(
            single_line_list_row_to_index(a, 6, 1),
            None,
            "6 items don't fit 5 usable rows — scrolled, so no-op"
        );
        assert_eq!(single_line_list_row_to_index(a, 0, 1), None, "empty list");
    }

    fn search_result(title: &str) -> crate::api::SearchResult {
        crate::api::SearchResult {
            title: title.to_string(),
            description: None,
            excerpt: None,
            size: None,
            wordcount: None,
            timestamp: None,
        }
    }

    #[test]
    fn results_row_to_index_maps_single_line_results() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.results = vec![
            search_result("Alan Turing"),
            search_result("Enigma"),
            search_result("Bletchley Park"),
        ];
        let a = area(0, 0, 40, 6); // 4 usable rows, 3 one-line items fit
        assert_eq!(results_row_to_index(&app, a, 0), None, "top border");
        assert_eq!(results_row_to_index(&app, a, 1), Some(0));
        assert_eq!(results_row_to_index(&app, a, 2), Some(1));
        assert_eq!(results_row_to_index(&app, a, 3), Some(2));
        assert_eq!(results_row_to_index(&app, a, 4), None, "past the last item");
    }

    #[test]
    fn results_row_to_index_accounts_for_multi_line_items() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let mut with_desc = search_result("Alan Turing");
        with_desc.description = Some("English mathematician".to_string());
        app.results = vec![with_desc, search_result("Enigma")];
        let a = area(0, 0, 40, 6); // 4 usable rows: item 0 takes 2, item 1 takes 1
        assert_eq!(results_row_to_index(&app, a, 1), Some(0), "title line");
        assert_eq!(
            results_row_to_index(&app, a, 2),
            Some(0),
            "description line"
        );
        assert_eq!(results_row_to_index(&app, a, 3), Some(1), "second result");
    }

    #[test]
    fn results_row_to_index_is_none_when_scrolled_or_empty() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.results = (0..10)
            .map(|i| search_result(&format!("Result {i}")))
            .collect();
        let a = area(0, 0, 40, 6); // only 4 usable rows for 10 items
        assert_eq!(results_row_to_index(&app, a, 1), None, "scrolled — no-op");
        app.results.clear();
        assert_eq!(results_row_to_index(&app, a, 1), None, "empty results");
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

    // ---- Incognito status glyph (PRD FR-PR-3) -----------------------------

    #[test]
    fn incognito_glyph_prefixes_the_status_text_only_when_incognito() {
        assert_eq!(
            with_incognito_glyph("Alan Turing".to_string(), false),
            "Alan Turing"
        );
        assert_eq!(
            with_incognito_glyph("Alan Turing".to_string(), true),
            "[incognito] Alan Turing"
        );
    }

    // ---- Logged-in indicator (PRD FR-ACC-1) -------------------------------

    #[test]
    fn login_glyph_prefixes_the_status_only_when_logged_in() {
        assert_eq!(
            with_login_glyph("Alan Turing".to_string(), None, None),
            "Alan Turing"
        );
        assert_eq!(
            with_login_glyph("Alan Turing".to_string(), Some("MockWikipedian"), None),
            "[@MockWikipedian] Alan Turing"
        );
    }

    /// PRD FR-ACC-3: the unread-notification badge folds into the same
    /// bracket as the username rather than adding a second prefix, and is
    /// simply absent (not an empty `[@user ]`) for a logged-out reader even
    /// if a badge string were somehow supplied.
    #[test]
    fn login_glyph_folds_the_notification_badge_into_the_same_bracket() {
        assert_eq!(
            with_login_glyph(
                "Alan Turing".to_string(),
                Some("MockWikipedian"),
                Some("\u{2709}3")
            ),
            "[@MockWikipedian \u{2709}3] Alan Turing"
        );
        assert_eq!(
            with_login_glyph("Alan Turing".to_string(), None, Some("\u{2709}3")),
            "Alan Turing",
            "no badge is ever shown while logged out"
        );
    }

    /// The glyph must show up in the actual rendered frame — not just in the
    /// pure helper — and it must be plain text so it survives `NO_COLOR`
    /// (no styling is required to read it, unlike a color-only indicator).
    #[test]
    fn incognito_glyph_appears_in_the_rendered_status_bar_and_not_otherwise() {
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
        assert!(!rendered.contains("incognito"));

        app.incognito = true;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("[incognito]"));
    }

    // ---- Uniform notice priority across status-bar modes ------------------
    //
    // Three prior chunks each independently hit a variant of the same bug:
    // a mode's own status-bar arm never looked at `app.notice` at all (or,
    // for Reading, only did so when no link was focused — nearly never
    // true), so action feedback was computed and then silently never drawn.
    // These tests pin `status_bar_text`'s fix: a notice, wherever it's set,
    // outranks the current mode's own hint/content in every mode that isn't
    // itself a live input prompt.

    /// A Reading-mode app with one internal and one external link on the
    /// page, whichever one `focus_external` names left focused — mirroring
    /// `install_document`'s real "link 0 focuses itself" default, which is
    /// exactly what made an external link's own status line so easy to hide
    /// behind (nearly every article the reader focuses right after
    /// following something has *a* link focused).
    fn app_with_links(focus_external: bool) -> App {
        let html = r#"<html><body><p>See <a href="./Internal_Target">Alpha</a> and
            <a href="https://example.com/">External</a>.</p></body></html>"#;
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.research = crate::research::ResearchStore::in_memory();
        app.set_document(crate::doc::parse_article_html("Test Article", html));
        app.active_tab_mut().focused_link = Some(if focus_external { 1 } else { 0 });
        app
    }

    /// Regression guard for deliverable 4: with no active notice, Reading's
    /// rich focused-link line (page-source glyph prefix, cycle position,
    /// keys) must render exactly as it always has.
    #[test]
    fn reading_focused_internal_link_line_is_unchanged_without_a_notice() {
        let app = app_with_links(false);
        assert_eq!(app.notice, None);
        let text = status_bar_text(&app, 80);
        assert_eq!(
            text,
            "→ Alpha (1/2)   Tab/S-Tab: cycle   Enter: open   H: back   L: forward"
        );
    }

    /// PRD-adjacent B2: an external link's "External link: ..." notice must
    /// win the status bar even though a link (nearly always) is focused —
    /// previously, whatever `main::handle_key` wrote there landed in
    /// `app.status`, which Reading's focused-link arm never even looks at.
    #[test]
    fn reading_notice_outranks_the_focused_external_link_line() {
        let mut app = app_with_links(true);
        // Confirms the fixture actually has the bug's precondition: a link
        // focused, and it's the external one whose own line would otherwise
        // take over the bar (see the `Some(link) => ...` arm without
        // `internal_title`).
        assert_eq!(
            status_bar_text(&app, 80),
            "→ External (external, not yet followable)"
        );
        app.notice = Some("External link: https://example.com/".to_string());
        assert_eq!(
            status_bar_text(&app, 80),
            "External link: https://example.com/",
            "the notice must outrank the focused-link line, not be hidden behind it"
        );
    }

    /// Same outranking, proven with a focused *internal* link too — the fix
    /// is unconditional on the notice channel, not specific to which kind of
    /// link happens to be focused.
    #[test]
    fn reading_notice_outranks_the_focused_internal_link_line() {
        let mut app = app_with_links(false);
        app.notice = Some("Bookmarked \"Test Article\"".to_string());
        assert_eq!(status_bar_text(&app, 80), "Bookmarked \"Test Article\"");
    }

    /// Reading's "Loading…" is rarer and more urgent than a leftover notice
    /// from the keypress that triggered the fetch — this bar's pre-existing
    /// order, preserved by `status_bar_text`'s `reading_loading` guard.
    #[test]
    fn reading_loading_still_outranks_a_pending_notice() {
        let mut app = app_with_links(false);
        app.notice = Some("Bookmarked \"Test Article\"".to_string());
        app.loading = true;
        assert_eq!(status_bar_text(&app, 80), "Loading…");
    }

    /// B13/B14: Research mode's status-bar arm used to be a hardcoded static
    /// hint that never read `app.notice`, so `save_selected_citation`'s
    /// confirmation (and any incognito warning it carries) was computed and
    /// then never drawn. Exercises the *real* `App::save_selected_citation`
    /// — not a simulated notice — to prove the fix end to end.
    #[test]
    fn citation_save_notice_renders_in_research_mode() {
        let mut app = app_with_links(false);
        app.mode = Mode::Research;
        assert_eq!(
            status_bar_text(&app, 80),
            "Enter/s: save citation   R: library   Esc: done   j/k: move",
            "no notice yet: the static hint still shows"
        );

        app.save_selected_citation();
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("Saved to research collection")),
            "save_selected_citation must set a notice: {:?}",
            app.notice
        );
        assert_eq!(
            status_bar_text(&app, 80),
            app.notice.clone().unwrap(),
            "the save confirmation must outrank Research's static hint"
        );
    }

    /// The incognito half of the same fix: `save_selected_citation`'s
    /// `privacy::append_warning_if_needed` persist-warning must also reach
    /// the bar, not just the plain confirmation.
    #[test]
    fn citation_save_incognito_warning_renders_in_research_mode() {
        let mut app = app_with_links(false);
        app.mode = Mode::Research;
        app.incognito = true;

        app.save_selected_citation();
        let notice = app.notice.clone().expect("save must set a notice");
        assert!(
            notice.contains("persist") || notice.contains("Saved to research collection"),
            "incognito citation save should still warn or confirm: {notice:?}"
        );
        assert_eq!(status_bar_text(&app, 80), notice);
    }

    /// B14: the redlink card's `y`-yank confirmation, and the offline card's
    /// own notices, must render — both cards used to be hardcoded static
    /// hint strings with no notice check at all.
    #[test]
    fn redlink_card_yank_notice_renders_over_the_static_hint() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.mode = Mode::RedlinkCard;
        assert_eq!(
            status_bar_text(&app, 80),
            "s: search similar titles   y: yank create URL   Esc: dismiss"
        );
        app.notice =
            Some("Yanked https://en.wikipedia.org/w/index.php?title=X&action=edit".to_string());
        assert_eq!(
            status_bar_text(&app, 80),
            "Yanked https://en.wikipedia.org/w/index.php?title=X&action=edit"
        );
    }

    #[test]
    fn offline_card_notice_renders_over_the_static_hint() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.mode = Mode::OfflineCard;
        assert_eq!(
            status_bar_text(&app, 80),
            "f: queue for fetch when online   s: search saved pages   Esc: dismiss"
        );
        app.notice = Some("Queued \"X\" to fetch when online".to_string());
        assert_eq!(
            status_bar_text(&app, 80),
            "Queued \"X\" to fetch when online"
        );
    }

    /// Text-input prompts are the deliberate exception (deliverable 1): a
    /// notice must NOT clobber a line the reader is still typing into.
    #[test]
    fn input_prompt_modes_keep_showing_their_own_input_even_with_a_notice_set() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.notice = Some("should not appear".to_string());

        app.mode = Mode::Search;
        app.search_input = "turing".to_string();
        assert!(status_bar_text(&app, 80).starts_with("/turing"));
        assert!(!status_bar_text(&app, 80).contains("should not appear"));

        app.mode = Mode::Command;
        app.command_input = "theme dark".to_string();
        assert_eq!(status_bar_text(&app, 80), ":theme dark");

        app.mode = Mode::Hint;
        app.hint_input = "a".to_string();
        assert_eq!(status_bar_text(&app, 80), "hint: a   Esc: cancel");
    }

    /// The notice lifecycle deliverable: set → visible → the next keypress's
    /// choke point clears it → the mode's normal content returns. The
    /// choke point itself is the single `app.notice = None` at the very top
    /// of `main::handle_key` (before any mode dispatch) — this pins the
    /// render-side contract that clearing depends on: once `notice` goes
    /// back to `None`, the mode's own hint reappears unaided.
    #[test]
    fn notice_lifecycle_clearing_restores_the_modes_own_hint() {
        let mut app = app_with_links(false);
        app.mode = Mode::Research;

        app.notice = Some("Saved to research collection (1 total)".to_string());
        assert_eq!(
            status_bar_text(&app, 80),
            "Saved to research collection (1 total)"
        );

        // What `main::handle_key`'s choke point does on the next keypress.
        app.notice = None;
        assert_eq!(
            status_bar_text(&app, 80),
            "Enter/s: save citation   R: library   Esc: done   j/k: move",
            "clearing the notice must restore Research's own hint line"
        );
    }
}
