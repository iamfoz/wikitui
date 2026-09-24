//! The narrow, `#[doc(hidden)]` entry points `benches/` drives (PRD §9's
//! "criterion benches for parse/layout on the pathological corpus", §6.8's
//! in-process rows). **Not an API**: nothing outside this repository's own
//! benches and perf tests should call it, and it changes whenever they do.
//!
//! Every module in this crate stays private (see `lib.rs`'s crate doc), so
//! instead of widening their visibility this facade wraps exactly the real
//! production calls each §6.8 row exercises — `doc::parse_article_html`,
//! `layout::layout_document`, `cache::PageCache`'s zstd read, and
//! `ui::draw` over a real `App` — behind opaque handles. A bench measuring
//! a wrapper here measures the same code the reader runs, not a copy of it.

use std::collections::HashMap;
use std::path::Path;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::api::TitleSuggestion;
use crate::app::{App, Mode};
use crate::cache::{DEFAULT_FORCE_REFETCH_SECS, DEFAULT_MAX_BYTES, FRESH_TTL_SECS, PageCache};
use crate::doc::{self, Document};
use crate::layout::{self, Layout, LayoutOptions};
use crate::theme::Theme;

/// The benches' single wiki scope/language: the primary Wikipedia scope
/// (empty string, see `api::wiki_scope`) in English.
const WIKI: &str = "";
const LANG: &str = "en";

/// A parsed article (`doc::Document`), opaque to callers.
pub struct Article(Document);

/// Parse Parsoid HTML exactly as an article open does (PRD §6.3's
/// "parse" stage — sanitizing, SEC-3 caps, citation extraction included).
pub fn parse(title: &str, html: &str) -> Article {
    Article(doc::parse_article_html(title, html))
}

impl Article {
    pub fn block_count(&self) -> usize {
        self.0.blocks.len()
    }

    pub fn citation_count(&self) -> usize {
        self.0.citations.len()
    }

    /// Whether the parse hit a SEC-3 cap (a fixture that trips one would
    /// be measuring the truncation path, not a full parse).
    pub fn truncated(&self) -> bool {
        self.0.truncated
    }
}

/// A laid-out article (`layout::Layout`), opaque to callers.
pub struct LaidOut(Layout);

/// Lay `article` out at `width` columns with the default reading options,
/// no folds and no inline images (the text themes' case) — the entry point
/// and arguments the reading view's `App::ensure_layout` uses: the "layout"
/// stage of §6.8's parse+layout row (and the L1-miss half of an L2-hit
/// open).
pub fn layout(article: &Article, width: u16) -> LaidOut {
    LaidOut(layout::layout_document_with_images(
        &article.0,
        width,
        LayoutOptions::default(),
        // What `App::image_box_map` yields with images off: an empty map.
        &HashMap::<String, (u16, u16)>::new(),
        &[],
    ))
}

impl LaidOut {
    pub fn line_count(&self) -> usize {
        self.0.lines.len()
    }
}

/// The real L2 page cache (`cache::PageCache`: an index JSON + a
/// zstd-compressed blob per page) rooted at a caller-owned directory.
pub struct DiskCache(PageCache);

impl DiskCache {
    pub fn at(dir: &Path) -> Self {
        DiskCache(PageCache::at(
            dir.to_path_buf(),
            DEFAULT_MAX_BYTES,
            FRESH_TTL_SECS,
            DEFAULT_FORCE_REFETCH_SECS,
        ))
    }

    pub fn put(&self, title: &str, html: &str, revid: u64) {
        self.0.put(WIKI, LANG, title, html, revid, None);
    }

    /// The L2 read an open does (`PageCache::get`: index read, blob read,
    /// zstd decompress, UTF-8 check, recency touch).
    pub fn get(&self, title: &str) -> Option<(String, u64)> {
        self.0.get(WIKI, LANG, title).map(|p| (p.html, p.revid))
    }
}

/// A real `App` drawing into a ratatui `TestBackend` of a fixed size — the
/// same `ui::draw` the event loop calls every frame, minus only the
/// terminal write (the pty harness in `tests/perf/` measures that part).
pub struct Reader {
    app: App,
    terminal: Terminal<TestBackend>,
}

impl Reader {
    pub fn new(width: u16, height: u16) -> Self {
        let app = App::new(LANG.to_string(), Theme::terminal(), false);
        let terminal =
            Terminal::new(TestBackend::new(width, height)).expect("TestBackend never fails");
        Reader { app, terminal }
    }

    /// Install `article` as a fresh navigation (`App::open_document`, the
    /// path every open funnels through), tagged with `revid` as the L1
    /// cache key requires. Layout happens lazily on the next [`draw`].
    ///
    /// [`draw`]: Reader::draw
    pub fn open(&mut self, article: Article, revid: u64) {
        self.app.active_tab_mut().current_revid = revid;
        self.app.open_document(article.0);
    }

    /// The article-open path from the L2 cache: read + decompress, parse,
    /// install. Returns `false` on a cache miss (nothing installed).
    pub fn open_from_cache(&mut self, cache: &DiskCache, title: &str) -> bool {
        let Some((html, revid)) = cache.get(title) else {
            return false;
        };
        self.open(parse(title, &html), revid);
        true
    }

    /// [`open_from_cache`](Reader::open_from_cache), but installed the way
    /// back/forward navigation does (`App::set_document`, back/forward
    /// stacks untouched) — so a bench can reopen the same article thousands
    /// of times without growing the tab's history stacks as it goes.
    pub fn reopen_from_cache(&mut self, cache: &DiskCache, title: &str) -> bool {
        let Some((html, revid)) = cache.get(title) else {
            return false;
        };
        self.app.active_tab_mut().current_revid = revid;
        self.app.set_document(parse(title, &html).0);
        true
    }

    /// One full frame: `ui::draw` into the backend (laying out first if the
    /// installed article has no current layout — an L1 lookup, then a
    /// relayout on a miss).
    pub fn draw(&mut self) {
        let app = &mut self.app;
        self.terminal
            .draw(|f| crate::ui::draw(f, app))
            .expect("TestBackend never fails");
    }

    pub fn scroll_by(&mut self, lines: i32) {
        self.app.scroll_by(lines);
    }

    /// Scroll to `fraction` (0.0–1.0) of the article's scrollable range.
    /// Needs a prior [`draw`](Reader::draw) so the range is known.
    pub fn scroll_to_fraction(&mut self, fraction: f64) {
        let max = self.app.active_tab().max_scroll;
        let target = (f64::from(max) * fraction.clamp(0.0, 1.0)).round() as u16;
        self.app.active_tab_mut().scroll = target;
    }

    pub fn scroll(&self) -> u16 {
        self.app.active_tab().scroll
    }

    /// How many real `layout_document` passes this reader has run — lets a
    /// bench assert an "L1 hit" really was one.
    pub fn layout_computations(&self) -> u32 {
        self.app.layout_computations
    }

    /// Drop every L1 entry, so the next [`draw`](Reader::draw) of any
    /// article is a relayout (the L2-hit case).
    pub fn forget_layouts(&mut self) {
        self.app.layout_cache = layout::LayoutCache::new(layout::DEFAULT_L1_CAPACITY);
        self.app.layout = None;
    }

    /// Put the reader in Search mode with `query` typed and `suggestions`
    /// as the typeahead dropdown — the state the event loop is in right
    /// after a typeahead response is installed (PRD FR-SR-1).
    pub fn show_suggestions(&mut self, query: &str, suggestions: &[(String, String)]) {
        self.app.mode = Mode::Search;
        self.app.search_input = query.to_string();
        self.app.typeahead = suggestions
            .iter()
            .map(|(title, description)| TitleSuggestion {
                title: title.clone(),
                description: Some(description.clone()),
            })
            .collect();
        self.app.selected_suggestion = 0;
    }

    /// The rendered screen as plain text rows (for asserting a bench drew
    /// what it claims to).
    pub fn screen_text(&self) -> Vec<String> {
        let buf = self.terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = "<html><head><title>Bench Probe</title></head><body>\
        <p>Alpha beta gamma delta epsilon.</p><h2>Later</h2><p>Zeta eta theta.</p>\
        </body></html>";

    /// The facade's reader really drives `ui::draw`: an opened article's text
    /// is on the backend's screen after one draw.
    #[test]
    fn reader_draws_the_opened_article() {
        let mut reader = Reader::new(80, 24);
        reader.open(parse("Bench Probe", HTML), 1);
        reader.draw();
        assert!(
            reader
                .screen_text()
                .iter()
                .any(|row| row.contains("Alpha beta gamma")),
            "{:#?}",
            reader.screen_text()
        );
    }

    /// Reopening an article at the same width is an L1 hit (no second
    /// layout pass), and `forget_layouts` turns the next one into a miss —
    /// the two states the L1/L2 open benches depend on.
    #[test]
    fn reopen_is_an_l1_hit_until_layouts_are_forgotten() {
        let mut reader = Reader::new(80, 24);
        reader.open(parse("Bench Probe", HTML), 1);
        reader.draw();
        assert_eq!(reader.layout_computations(), 1);
        reader.open(parse("Bench Probe", HTML), 1);
        reader.draw();
        assert_eq!(
            reader.layout_computations(),
            1,
            "same identity+width: L1 hit"
        );
        reader.forget_layouts();
        reader.open(parse("Bench Probe", HTML), 1);
        reader.draw();
        assert_eq!(reader.layout_computations(), 2, "L1 cleared: relayout");
    }

    /// The L2 round trip returns exactly what was stored.
    #[test]
    fn disk_cache_round_trips_through_zstd() {
        let dir = std::env::temp_dir().join(format!("wikitui-bench-l2-{}", std::process::id()));
        let cache = DiskCache::at(&dir);
        cache.put("Bench Probe", HTML, 7);
        assert_eq!(cache.get("Bench Probe"), Some((HTML.to_string(), 7)));
        let mut reader = Reader::new(80, 24);
        assert!(reader.open_from_cache(&cache, "Bench Probe"));
        assert!(!reader.open_from_cache(&cache, "Missing"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
