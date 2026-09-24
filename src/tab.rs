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

/// PRD §6.8 `low_memory`: a zstd-compressed copy of the exact HTML a tab's
/// installed document was parsed from. Kept (only in low-memory mode — see
/// `App::remember_source`) so a tab that goes off screen can drop its parsed
/// [`Document`], links, sections and layout and later re-parse *the same
/// bytes* on focus: parsing is deterministic, so the rehydrated document —
/// and therefore every block index that scroll, folds, the focused link and
/// find results point into — is identical to the one dropped. Keeping the
/// source in memory (rather than re-reading L2 on focus) is what makes that
/// exact: L2 holds only the latest revision of a title and can evict or be
/// overwritten by a background revalidation, and a saved/ZIM/`:noredirect`
/// page may never have been in L2 at all. A long article compresses to
/// roughly a tenth of its HTML, and far less than its parsed form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceHtml {
    /// A boxed slice, not a `Vec`: `zstd::bulk::compress` returns a buffer
    /// sized for the worst case (≈ the input length), so keeping the `Vec`
    /// as-is would hold the uncompressed size in spare capacity and save
    /// nothing. `into_boxed_slice` reallocates it down to the compressed
    /// length.
    compressed: Box<[u8]>,
    len: usize,
}

impl SourceHtml {
    /// zstd level 3 (a few ms for a 1.5 MB article). `None` only if the
    /// compressor itself fails, in which case the tab simply stays resident.
    pub fn compress(html: &str) -> Option<Self> {
        let compressed = zstd::bulk::compress(html.as_bytes(), 3)
            .ok()?
            .into_boxed_slice();
        Some(Self {
            compressed,
            len: html.len(),
        })
    }

    pub fn decompress(&self) -> Option<String> {
        let bytes = zstd::bulk::decompress(&self.compressed, self.len).ok()?;
        String::from_utf8(bytes).ok()
    }

    /// Bytes held in memory for this copy.
    #[cfg(test)]
    pub fn compressed_len(&self) -> usize {
        self.compressed.len()
    }
}

/// What a dehydrated tab keeps in place of its parsed [`Document`] (PRD §6.8
/// `low_memory`): only the facts other code reads *without* focusing it —
/// the title (tab bar, pickers, session snapshot, `:save tabs`,
/// reading-position save on close, revalidation and retry routing) and its
/// word count (the FR-PF-3 dwell signal applied when a background tab
/// closes). Everything else that is per-view — scroll,
/// folds, focused link, selected section, table offset, find state,
/// back/forward stacks, `disambig_pending`, `pending_open`, history/dwell
/// tracking — stays on the [`Tab`] untouched, which is why rehydration
/// restores the view exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dehydrated {
    pub title: String,
    pub word_count: u32,
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
    /// `current_revid` as it was when the current document was installed —
    /// the revision that document actually is, kept apart from
    /// `current_revid` because the open paths set the *next* document's revid
    /// before installing it. What `App::recent_docs` files a document under
    /// when it's navigated away from.
    pub doc_revid: u64,
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
    /// PRD §7 "Disambiguation page": set when a disambiguation document is
    /// installed and its chooser hasn't been shown yet. Consumed (cleared) by
    /// `App::show_pending_disambig` the first time this tab is on screen —
    /// immediately for a foreground open, or on first focus for a document
    /// that landed in a background tab — so the chooser appears exactly once
    /// per install and "Esc: view as text" sticks across tab switches.
    pub disambig_pending: bool,
    /// PRD §6.8 / §11 cache-hit KPI: the fetch tier of the installed document
    /// when it was installed by a reader-requested open that hasn't been
    /// counted yet. Armed by `App::set_document` / `main::
    /// apply_tab_load_outcome` and consumed (counted into `App::open_log`) by
    /// the first layout of this tab — `App::ensure_layout` or `App::
    /// layout_for_tab` — which is also where an L1 hit upgrades the tier.
    /// `install_document` clears it (a fresh install is unarmed until its
    /// caller says it was an open); see `hitrate.rs` for what counts.
    pub pending_open: Option<crate::hitrate::OpenSource>,
    /// PRD §6.8 `low_memory`: the compressed source of the installed
    /// document (see [`SourceHtml`]); `None` outside low-memory mode, and for
    /// an install whose caller had no HTML to hand over (such a tab simply
    /// stays resident). Cleared by every fresh install.
    pub source_html: Option<SourceHtml>,
    /// PRD §6.8 `low_memory`: `Some` while this tab is off screen with its
    /// parsed document dropped (`doc` is then `None`); see [`Tab::dehydrate`]
    /// / [`Tab::rehydrate`] and `App::enforce_residency`.
    pub dehydrated: Option<Dehydrated>,
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
            doc_revid: 0,
            loading: false,
            pending_title: None,
            pending_reload: None,
            history_visit_id: None,
            visit_started_at: None,
            interest_dwell_signaled: false,
            interest_scroll_signaled: false,
            redirected_from: None,
            disambig_pending: false,
            pending_open: None,
            source_html: None,
            dehydrated: None,
        }
    }

    /// What the tab bar and pickers display for this tab: the document title
    /// once loaded, the pending target title while a background fetch is in
    /// flight, or a placeholder for a still-empty tab.
    pub fn display_title(&self) -> String {
        if let Some(title) = self.article_title() {
            title.to_string()
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
    /// The installed article's title whether it is resident or dehydrated
    /// (PRD §6.8 `low_memory`) — what every reader of a possibly-off-screen
    /// tab's identity uses instead of `doc.title`.
    pub fn article_title(&self) -> Option<&str> {
        match (&self.doc, &self.dehydrated) {
            (Some(doc), _) => Some(doc.title.as_str()),
            (None, Some(d)) => Some(d.title.as_str()),
            (None, None) => None,
        }
    }

    /// PRD §6.8 `low_memory`: drop the parsed document and everything derived
    /// from it (links, reference markers, section outline), keeping a
    /// [`Dehydrated`] stand-in. Only possible when the tab holds both a
    /// document and its [`SourceHtml`]; returns whether it dehydrated. The
    /// view state (scroll, folds, focused link, …) is deliberately left in
    /// place — see [`Dehydrated`].
    pub fn dehydrate(&mut self) -> bool {
        if self.dehydrated.is_some() || self.source_html.is_none() {
            return false;
        }
        let Some(doc) = self.doc.take() else {
            return false;
        };
        self.dehydrated = Some(Dehydrated {
            title: doc.title.clone(),
            word_count: crate::doc::word_count(&doc),
        });
        // Fresh (zero-capacity) vectors, not `clear()`, so the memory is
        // actually released rather than kept as spare capacity.
        self.links = Vec::new();
        self.reference_markers = Vec::new();
        self.sections = Vec::new();
        true
    }

    /// PRD §6.8 `low_memory`: re-parse the kept [`SourceHtml`] and restore the
    /// document, links, reference markers and section outline — the same
    /// derivations `install_document` makes, but *without* its resets, so
    /// scroll, folds, the focused link, find state and every tracking flag
    /// survive untouched. Returns whether it rehydrated (`false` when the tab
    /// wasn't dehydrated, or — never expected — the copy failed to
    /// decompress, in which case the tab stays dehydrated rather than
    /// showing a wrong document).
    pub fn rehydrate(&mut self) -> bool {
        let Some(d) = self.dehydrated.as_ref() else {
            return false;
        };
        let Some(html) = self.source_html.as_ref().and_then(SourceHtml::decompress) else {
            return false;
        };
        // `parse_article_html` prefers the page's own `<title>` and falls back
        // to this argument, so passing the title it produced last time yields
        // the same document either way.
        let doc = crate::doc::parse_article_html(&d.title, &html);
        drop(html);
        self.links = collect_links(&doc);
        self.reference_markers = collect_reference_markers(&doc);
        self.sections = section_outline(&doc);
        self.doc = Some(doc);
        self.dehydrated = None;
        true
    }

    pub fn install_document(&mut self, doc: Document) {
        self.links = collect_links(&doc);
        self.reference_markers = collect_reference_markers(&doc);
        self.focused_link = if self.links.is_empty() { None } else { Some(0) };
        self.sections = section_outline(&doc);
        self.selected_section = 0;
        // A fresh document's block indices are new — any folds from the
        // previous document would collapse the wrong ranges (PRD FR-NV-3).
        self.folded_blocks.clear();
        self.disambig_pending = doc.is_disambiguation;
        // Unarmed until the installing caller says this was an open (PRD
        // §6.8 KPI) — a split duplicate or a rehydration never counts.
        self.pending_open = None;
        // PRD §6.8 low_memory: a new document has a new source (the caller
        // hands it over via `App::remember_source`) and is resident.
        self.source_html = None;
        self.dehydrated = None;
        self.doc_revid = self.current_revid;
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
        self.disambig_pending = false;
        self.pending_open = None;
        self.source_html = None;
        self.dehydrated = None;
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
        self.article_title().map(|title| HistoryEntry {
            wiki: self.wiki.clone(),
            lang: self.lang.clone(),
            title: title.to_string(),
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

    // ---- PRD §6.8 `low_memory`: SourceHtml / dehydrate / rehydrate ---------

    const LM_HTML: &str = "<html><head><title>Alan Turing</title></head><body>\
        <h2>Early life</h2><p>Born in <a href=\"./London\">London</a>.</p>\
        <h2>Career</h2><p>Worked at <a href=\"./Bletchley_Park\">Bletchley</a>.</p>\
        </body></html>";

    #[test]
    fn source_html_round_trips_and_holds_only_the_compressed_bytes() {
        let big: String = (0..2000)
            .map(|i| format!("<p id=\"mw{i:X}\">Paragraph {i} about computing.</p>"))
            .collect();
        let src = SourceHtml::compress(&big).unwrap();
        assert_eq!(src.decompress().as_deref(), Some(big.as_str()));
        assert!(
            src.compressed_len() * 4 < big.len(),
            "{} compressed vs {} raw",
            src.compressed_len(),
            big.len()
        );
        // A boxed slice: no worst-case-sized spare capacity kept around.
        assert_eq!(src.compressed.len(), src.compressed_len());
    }

    #[test]
    fn dehydrate_needs_a_source_and_rehydrate_restores_only_derived_data() {
        let mut tab = Tab::new(1, "en".to_string());
        tab.install_document(crate::doc::parse_article_html("Alan Turing", LM_HTML));
        assert!(!tab.dehydrate(), "no kept source: stays resident");
        assert!(tab.doc.is_some());

        tab.source_html = SourceHtml::compress(LM_HTML);
        tab.scroll = 3;
        tab.focused_link = Some(1);
        tab.pending_open = Some(crate::hitrate::OpenSource::Network);
        tab.history_visit_id = Some(42);
        let links_before = format!("{:?}", tab.links);
        let doc_before = format!("{:?}", tab.doc);
        assert!(tab.dehydrate());
        assert!(!tab.dehydrate(), "already dehydrated");
        assert!(tab.doc.is_none() && tab.links.is_empty() && tab.sections.is_empty());
        assert_eq!(tab.display_title(), "Alan Turing");
        assert_eq!(
            tab.dehydrated.as_ref().map(|d| d.word_count),
            Some(crate::doc::word_count(&crate::doc::parse_article_html(
                "Alan Turing",
                LM_HTML
            )))
        );

        assert!(tab.rehydrate());
        assert!(!tab.rehydrate(), "already resident");
        assert_eq!(format!("{:?}", tab.doc), doc_before);
        assert_eq!(format!("{:?}", tab.links), links_before);
        assert_eq!(tab.sections.len(), 2);
        // Unlike `install_document`, nothing per-view or per-visit resets.
        assert_eq!(tab.scroll, 3);
        assert_eq!(tab.focused_link, Some(1));
        assert_eq!(tab.pending_open, Some(crate::hitrate::OpenSource::Network));
        assert_eq!(tab.history_visit_id, Some(42));
    }

    #[test]
    fn a_fresh_install_or_blank_drops_the_old_source_and_dehydrated_state() {
        let mut tab = Tab::new(1, "en".to_string());
        tab.install_document(crate::doc::parse_article_html("Alan Turing", LM_HTML));
        tab.source_html = SourceHtml::compress(LM_HTML);
        assert!(tab.dehydrate());
        tab.install_document(doc("Enigma machine"));
        assert!(
            tab.source_html.is_none(),
            "the old source is not this doc's"
        );
        assert!(tab.dehydrated.is_none());
        assert_eq!(tab.display_title(), "Enigma machine");

        tab.source_html = SourceHtml::compress(LM_HTML);
        tab.clear_to_blank();
        assert!(tab.source_html.is_none() && tab.dehydrated.is_none());
    }
}
