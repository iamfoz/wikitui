//! Buffer-style tabs (PRD FR-TB-1..3, §2.2 "tabs-as-buffers"): the central
//! v1.0 architecture split. Everything that describes *one open reading
//! context* — the document, its links/sections, the scroll position, the
//! in-page find state, the per-tab back/forward history — lives on a [`Tab`].
//! `App` holds a `Vec<Tab>` plus the app-global chrome (mode, theme, search
//! and command input, the research library) and routes its per-view methods
//! through the active tab.
//!
//! The L1 layout cache (`layout::LayoutCache`) is deliberately *not*
//! duplicated here: it is already keyed on `(lang, title, revid, width, …)`,
//! so a single shared cache serves every tab, and switching tabs is an L1
//! hit rather than a relayout (§6.8).

use crate::app::PendingReload;
use crate::doc::{Document, LinkRef, SectionRef, collect_links, section_outline};
use crate::layout;

/// A stable, monotonically-assigned per-tab identity (PRD FR-TB-3). Background
/// fetch/revalidation completions target a tab by this id, never by `Vec`
/// index (indices shift when a tab to the left closes) and never by
/// `(lang, title)` (two tabs can hold the same article). A closed tab's id is
/// never reused, so a completion that arrives for it is dropped gracefully.
pub type TabId = u64;

/// One entry in a tab's back/forward history (PRD FR-TB-2, v1.0 upgrade). The
/// MVP stored bare titles; each entry now carries the `(lang, title)` to
/// re-fetch (always an L2 cache hit, so back/forward is instant) plus the
/// `scroll` offset to restore — "preserving scroll state". Fold state will
/// join this record with the later folding chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub lang: String,
    pub title: String,
    pub scroll: u16,
}

/// One open reading context. Owns everything that is per-view; the app-global
/// state (theme, mode, research store, command line…) stays on `App`.
pub struct Tab {
    /// Stable identity for background-completion routing (see [`TabId`]).
    pub id: TabId,
    /// The language edition this tab's article was fetched in — recorded so
    /// history entries and background completions restore/route to the right
    /// wiki even after `:lang` changed the app-global default.
    pub lang: String,
    pub doc: Option<Document>,
    pub links: Vec<LinkRef>,
    pub focused_link: Option<usize>,
    pub sections: Vec<SectionRef>,
    pub selected_section: usize,
    pub back_stack: Vec<HistoryEntry>,
    pub forward_stack: Vec<HistoryEntry>,
    pub scroll: u16,
    pub max_scroll: u16,
    pub find_input: String,
    pub find_matches: Vec<u16>,
    pub find_occurrences: Vec<layout::Occurrence>,
    pub find_index: usize,
    pub page_source: crate::app::PageSource,
    /// The revid of this tab's document (PRD FR-OFF-1); participates in the L1
    /// layout-cache key. `0` in degraded mode.
    pub current_revid: u64,
    /// Whether a background fetch for this tab is still in flight (PRD
    /// FR-TB-3): drives the "…" indicator in the tab bar and keeps the event
    /// loop on its scoped-poll path so the UI stays responsive.
    pub loading: bool,
    /// The title to show in the tab bar before the document arrives (a
    /// background tab opened by `Ctrl-Enter` shows its target title + "…"
    /// while loading); `None` once the document is installed.
    pub pending_title: Option<String>,
    /// Set when a background revalidation (PRD FR-OFF-2) wrote newer content
    /// for *this tab's* article into L2. The "updated — r to reload" notice
    /// only shows while this tab is active; switching to a tab that has this
    /// armed re-shows the notice (per the tab-routing requirement).
    pub pending_reload: Option<PendingReload>,
    /// The row id `history::History::record_visit` returned for the
    /// currently-installed document (PRD FR-HS-1), or `None` when nothing
    /// is being tracked (empty tab, incognito, or the write failed). Paired
    /// with `visit_started_at`; both are cleared together by
    /// `App::flush_tab_dwell` and by `install_document` resetting them for
    /// a fresh document.
    pub history_visit_id: Option<i64>,
    /// When the currently-installed document became "the one on screen" in
    /// this tab, for dwell-time accounting (PRD FR-HS-1). `std::time::
    /// Instant`, not a `chrono` timestamp: this measures *elapsed* wall
    /// time, the same monotonic-clock job `app.rs`'s typeahead debounce
    /// already uses `Instant` for — not a second, independent clock
    /// convention.
    pub visit_started_at: Option<std::time::Instant>,
}

impl Tab {
    /// A fresh, empty tab for `lang`.
    pub fn new(id: TabId, lang: String) -> Self {
        Self {
            id,
            lang,
            doc: None,
            links: Vec::new(),
            focused_link: None,
            sections: Vec::new(),
            selected_section: 0,
            back_stack: Vec::new(),
            forward_stack: Vec::new(),
            scroll: 0,
            max_scroll: 0,
            find_input: String::new(),
            find_matches: Vec::new(),
            find_occurrences: Vec::new(),
            find_index: 0,
            page_source: crate::app::PageSource::None,
            current_revid: 0,
            loading: false,
            pending_title: None,
            pending_reload: None,
            history_visit_id: None,
            visit_started_at: None,
        }
    }

    /// What the tab bar and pickers display for this tab: the document title
    /// once loaded, the pending target title while a background fetch is in
    /// flight, or a placeholder for a still-empty tab.
    pub fn display_title(&self) -> String {
        if let Some(doc) = &self.doc {
            doc.title.clone()
        } else if let Some(t) = &self.pending_title {
            t.clone()
        } else {
            "(new tab)".to_string()
        }
    }

    /// Install a freshly-fetched document into this tab, recomputing its
    /// links/sections and resetting scroll and find state — the per-tab half
    /// of `App::set_document`, with none of the app-global side effects
    /// (citations, mode, layout invalidation) so it is safe to call on a
    /// *non-active* tab when a background fetch completes.
    pub fn install_document(&mut self, doc: Document) {
        self.links = collect_links(&doc);
        self.focused_link = if self.links.is_empty() { None } else { Some(0) };
        self.sections = section_outline(&doc);
        self.selected_section = 0;
        self.doc = Some(doc);
        self.scroll = 0;
        self.pending_title = None;
        // A newly installed document is, by definition, not the one a still
        // pending reload notice was about.
        self.pending_reload = None;
        // Whatever history-visit/dwell tracking belonged to the previous
        // document is stale now; `App::set_document` (the only caller with
        // access to `history::History`) is responsible for flushing that
        // dwell *before* calling this and for recording the new visit
        // *after* — see its doc comment. Resetting unconditionally here
        // keeps that invariant true even for a background tab's first-ever
        // install (`main::apply_tab_load_outcome`), which had nothing to
        // flush in the first place.
        self.history_visit_id = None;
        self.visit_started_at = None;
        self.clear_find();
    }

    /// Clears in-page find state — a fresh article's matches would be
    /// meaningless leftovers from whatever was open before.
    pub fn clear_find(&mut self) {
        self.find_input.clear();
        self.find_matches.clear();
        self.find_occurrences.clear();
        self.find_index = 0;
    }

    /// The `(lang, title, scroll)` history record for whatever is currently
    /// open in this tab, or `None` if it is empty. Used to push the current
    /// article onto a stack before navigating away.
    pub fn current_entry(&self) -> Option<HistoryEntry> {
        self.doc.as_ref().map(|d| HistoryEntry {
            lang: self.lang.clone(),
            title: d.title.clone(),
            scroll: self.scroll,
        })
    }
}
