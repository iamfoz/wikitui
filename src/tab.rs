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

use serde::{Deserialize, Serialize};

use crate::app::PendingReload;
use crate::doc::{
    Document, LinkRef, ReferenceMarker, SectionRef, collect_links, collect_reference_markers,
    section_outline,
};
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
///
/// `Serialize`/`Deserialize` (PRD FR-TB-5): a tab's back/forward stacks are
/// part of what session auto-restore persists (`session::SessionTab`
/// reuses this type directly rather than a parallel copy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// The wiki scope (`api::wiki_scope`) this entry was read on (PRD
    /// FR-ML-4), so back/forward re-reads the *right* wiki's cache even after
    /// `:wiki` switched the active wiki. `#[serde(default)]` (empty =
    /// default Wikipedia) so a back/forward stack persisted before wiki
    /// scoping existed restores as the default wiki.
    #[serde(default)]
    pub wiki: String,
    pub lang: String,
    pub title: String,
    pub scroll: u16,
}

/// PRD FR-PC-4's per-tab render override layer: `:set-tab key=value` sets a
/// field here; `:set-tab key=` (empty right-hand side) clears it back to
/// `None`. Every field starts `None` (a fresh tab has no overrides, so it
/// renders at whatever the session-global `App` field says); `App::
/// layout_options`/`images_enabled` are the only readers, each falling back
/// to the session-global setting one field at a time — so a tab override
/// never has to re-derive the config-then-session precedence chain, only
/// add one more rung on top of it: **config < session (`:set`) < this
/// tab's override**. Deliberately narrower than the full session-global
/// `:set` surface — only the render/typography knobs that actually feed
/// `layout::LayoutOptions` (plus `images`, which feeds the image box map
/// alongside it) make sense to vary per article; `theme`/`prefetch`/
/// `mouse`/`animations`/`hyperlinks`/`reading_wpm` stay session-only
/// (`command::TAB_SCOPED_KEYS` is the parser's matching allow-list).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TabOverrides {
    pub measure: Option<u16>,
    pub ambiguous_wide: Option<bool>,
    pub images: Option<bool>,
    pub text_align: Option<layout::TextAlign>,
    pub margin: Option<u16>,
    pub paragraph_spacing: Option<u8>,
    pub line_spacing: Option<u8>,
    pub word_spacing: Option<u8>,
    /// PRD FR-RD-9 (v1.x): per-tab full justification / soft hyphenation
    /// overrides. `None` inherits the session-global `App::justify`/
    /// `App::hyphenate`.
    pub justify: Option<bool>,
    pub hyphenate: Option<bool>,
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
    /// PRD FR-ML-4: the wiki scope (`api::wiki_scope`) this tab's article was
    /// fetched from — the *tab's own* wiki, frozen at open time. Every cache
    /// read/write and session-state lookup this tab drives keys on this, not
    /// on `App`'s current active wiki, so a `:wiki` switch changes only what
    /// *new* opens address while every existing tab keeps serving — and
    /// caching — its own wiki's content. Empty string is the default
    /// Wikipedia scope (see `api::wiki_scope`).
    pub wiki: String,
    pub doc: Option<Document>,
    pub links: Vec<LinkRef>,
    /// PRD FR-NV-4: the article's same-page reference markers (`[n]` →
    /// `#cite_note-…`), in document order — the set `collect_links` excludes
    /// from `links` (0b86bf0) so they are never Tab-followable. Kept
    /// separately so `K` can peek the reference nearest the reading position
    /// (`App::open_peek_at_focus`) without a marker ever becoming a focusable
    /// link. Recomputed on every document install, cleared on blank.
    pub reference_markers: Vec<ReferenceMarker>,
    pub focused_link: Option<usize>,
    pub sections: Vec<SectionRef>,
    pub selected_section: usize,
    /// PRD FR-NV-3: the `doc.blocks` indices of headings folded shut in this
    /// tab (`za`/`zM`/`zR` view state). Stored as heading-block indices — the
    /// exact set `layout::layout_document_with_images` takes as its `folds`
    /// input and the L1 cache key carries — rather than section-outline
    /// indices, so it needs no translation at layout time and stays valid for
    /// the life of one document. Reset whenever a new document is installed.
    pub folded_blocks: std::collections::HashSet<usize>,
    pub back_stack: Vec<HistoryEntry>,
    pub forward_stack: Vec<HistoryEntry>,
    pub scroll: u16,
    pub max_scroll: u16,
    /// PRD FR-RD-4's horizontal table scroll offset for this tab: how many
    /// leading columns every wide table in the article skips. Reset to 0 when
    /// a new document is installed; adjusted by `App::scroll_tables` (`[`/`]`).
    pub table_col_offset: u16,
    /// PRD FR-PC-4: this tab's render-option overrides (`:set-tab`). Survives
    /// a fresh document being installed (`install_document`/`clear_to_blank`
    /// deliberately don't touch it) — a per-*tab* preference like "this one
    /// article reads better narrower" is not tied to whichever article
    /// happens to be open in the tab right now.
    pub overrides: TabOverrides,
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
    /// PRD FR-PF-3: whether the dwell interest signal has already been applied
    /// for the currently-installed document, so repeated dwell flushes (tab
    /// switching back and forth) don't re-add it — the signal is per-read,
    /// capped at +2.0, not per-flush. Reset on each document install.
    pub interest_dwell_signaled: bool,
    /// PRD FR-PF-3: whether the scroll-≥-70% interest signal has already fired
    /// for the currently-installed document (it fires once when the reader
    /// first passes 70% of the article). Reset on each document install.
    pub interest_scroll_signaled: bool,
    /// PRD §7 "Redirect": `Some(requested_title)` when the currently
    /// installed document was reached by following a redirect — the alias
    /// the reader actually asked for (`"UK"`), not the resolved canonical
    /// title already sitting in `doc.title` (`"United Kingdom"`). `:noredirect`
    /// reads this to know what to re-fetch without following. Reset to `None`
    /// by `install_document` on every fresh document; `main::open_title` sets
    /// it back to `Some` right after, when that particular fetch did redirect
    /// — the one caller with the fetch-level knowledge this field needs.
    pub redirected_from: Option<String>,
}

impl Tab {
    /// A fresh, empty tab for `lang` on the default (Wikipedia) wiki scope.
    /// The wiki is set to the article's real scope when a document is
    /// installed (`App::set_document`) or a background load completes.
    pub fn new(id: TabId, lang: String) -> Self {
        Self {
            id,
            lang,
            wiki: String::new(),
            doc: None,
            links: Vec::new(),
            reference_markers: Vec::new(),
            focused_link: None,
            sections: Vec::new(),
            selected_section: 0,
            folded_blocks: std::collections::HashSet::new(),
            back_stack: Vec::new(),
            forward_stack: Vec::new(),
            scroll: 0,
            max_scroll: 0,
            table_col_offset: 0,
            overrides: TabOverrides::default(),
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
            interest_dwell_signaled: false,
            interest_scroll_signaled: false,
            redirected_from: None,
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
        self.reference_markers = collect_reference_markers(&doc);
        self.focused_link = if self.links.is_empty() { None } else { Some(0) };
        self.sections = section_outline(&doc);
        self.selected_section = 0;
        // A fresh document's block indices are new — any folds from the
        // previous document would collapse the wrong ranges (PRD FR-NV-3).
        self.folded_blocks.clear();
        self.doc = Some(doc);
        self.scroll = 0;
        self.table_col_offset = 0;
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
        self.interest_dwell_signaled = false;
        self.interest_scroll_signaled = false;
        // PRD §7 "Redirect": a fresh document is, by definition, not (yet)
        // known to have arrived via a redirect — `main::open_title` sets
        // this back to `Some` right after, when this particular fetch did.
        self.redirected_from = None;
        self.clear_find();
    }

    /// Returns this tab to the blank state `Tab::new` starts from — the
    /// document and everything derived from it — while leaving `id`, `lang`,
    /// and the back/forward stacks untouched (PRD FR-DL-1's `gh`/`:start`
    /// "home" action: it is a navigation, not a tab reset, so `H` must still
    /// be able to return to whatever was showing). Mirrors
    /// `install_document`'s resets with `doc` set to `None` instead of
    /// `Some`.
    pub fn clear_to_blank(&mut self) {
        self.doc = None;
        self.links.clear();
        self.reference_markers.clear();
        self.focused_link = None;
        self.sections.clear();
        self.selected_section = 0;
        self.folded_blocks.clear();
        self.scroll = 0;
        self.max_scroll = 0;
        self.table_col_offset = 0;
        self.page_source = crate::app::PageSource::None;
        self.current_revid = 0;
        self.loading = false;
        self.pending_title = None;
        self.pending_reload = None;
        self.history_visit_id = None;
        self.visit_started_at = None;
        self.interest_dwell_signaled = false;
        self.interest_scroll_signaled = false;
        self.redirected_from = None;
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
            wiki: self.wiki.clone(),
            lang: self.lang.clone(),
            title: d.title.clone(),
            scroll: self.scroll,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(title: &str) -> Document {
        Document {
            title: title.to_string(),
            blocks: Vec::new(),
            citations: Vec::new(),
            truncated: false,
            degraded_parse: false,
            is_disambiguation: false,
        }
    }

    /// PRD FR-DL-1's `gh`/`:start` "home" action (`App::go_home`) clears a
    /// tab's content back to blank but must not disturb its identity or its
    /// back/forward history — `H` still has to work afterward.
    #[test]
    fn clear_to_blank_resets_content_but_preserves_identity_and_history() {
        let mut tab = Tab::new(7, "en".to_string());
        tab.install_document(doc("Alan Turing"));
        tab.scroll = 12;
        tab.focused_link = Some(0);
        tab.back_stack.push(HistoryEntry {
            wiki: String::new(),
            lang: "en".to_string(),
            title: "Earlier Article".to_string(),
            scroll: 3,
        });
        tab.forward_stack.push(HistoryEntry {
            wiki: String::new(),
            lang: "en".to_string(),
            title: "Later Article".to_string(),
            scroll: 0,
        });

        tab.clear_to_blank();

        assert!(tab.doc.is_none());
        assert_eq!(tab.scroll, 0);
        assert!(tab.focused_link.is_none());
        assert!(tab.links.is_empty());
        assert!(tab.sections.is_empty());
        assert_eq!(tab.id, 7, "identity is untouched");
        assert_eq!(tab.lang, "en", "language is untouched");
        assert_eq!(
            tab.back_stack.len(),
            1,
            "back stack is untouched — the caller pushes the outgoing article onto it separately"
        );
        assert_eq!(
            tab.forward_stack.len(),
            1,
            "forward stack is untouched by clear_to_blank itself"
        );
    }
}
