use crate::api::SearchResult;
use crate::cite::CiteStyle;
use crate::doc::{
    Citation, Document, LinkRef, SectionRef, collect_links, find_matches, section_outline,
};
use crate::research::{ResearchStore, SavedCitation};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reading,
    Search,
    Results,
    Toc,
    Find,
    Research,
    Library,
    Help,
}

/// Where the currently open article's content came from (PRD FR-OFF-6's
/// offline-indicator states): ● fresh from the network, ◐ served from
/// cache, ○ network failed and a (possibly stale) cached copy stood in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSource {
    None,
    Live,
    Cached { age_secs: u64 },
    Offline { age_secs: u64 },
}

impl PageSource {
    /// The status-bar prefix, e.g. "◐ cached 3h ago · ".
    pub fn prefix(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::Live => "● ".to_string(),
            Self::Cached { age_secs } => {
                format!("◐ cached {} ago · ", crate::cache::age_human(*age_secs))
            }
            Self::Offline { age_secs } => format!(
                "○ offline — cached {} ago · ",
                crate::cache::age_human(*age_secs)
            ),
        }
    }
}

pub struct App {
    pub mode: Mode,
    pub prior_mode: Mode,
    pub doc: Option<Document>,
    pub links: Vec<LinkRef>,
    pub focused_link: Option<usize>,
    pub sections: Vec<SectionRef>,
    pub selected_section: usize,
    pub back_stack: Vec<String>,
    pub forward_stack: Vec<String>,
    pub scroll: u16,
    pub max_scroll: u16,
    pub status: String,
    pub search_input: String,
    pub results: Vec<SearchResult>,
    pub selected_result: usize,
    pub should_quit: bool,
    pub lang: String,
    pub loading: bool,
    pub pending_g: bool,
    pub theme: Theme,
    /// Set once at startup from the `NO_COLOR` environment variable (PRD
    /// FR-TH-5): when true, every style still applies but with colors
    /// stripped, regardless of which theme is selected.
    pub no_color: bool,
    /// In-page find (PRD FR-NV-6): what the reader typed into `Ctrl-f`.
    pub find_input: String,
    /// Lines to scroll to for each block matching `find_input`, in reading
    /// order.
    pub find_matches: Vec<u16>,
    /// Which entry in `find_matches` `n`/`N` last jumped to.
    pub find_index: usize,
    /// Research mode's candidate list for the open article: element 0 is
    /// always the article's own citation (`research::self_citation`);
    /// the rest are its extracted References entries, in document order.
    pub citations: Vec<Citation>,
    pub selected_citation: usize,
    /// The running bibliography, persisted to disk (PRD §6.4 plain files).
    pub research: ResearchStore,
    /// The library view's selection into `research.citations`.
    pub selected_library: usize,
    /// The citation style the library view previews and exports in.
    pub cite_style: CiteStyle,
    /// The mode `open_library` was entered from, restored on Esc (the
    /// library is reachable from both Reading and Research).
    pub library_prior_mode: Mode,
    /// Export-overwrite confirmation: the filename the user was just
    /// warned about; a second `e` for the same filename proceeds.
    pub pending_export_overwrite: Option<String>,
    /// Where the open article's content came from — set by the fetch path
    /// before `set_document`, which folds it into the status line.
    pub page_source: PageSource,
}

impl App {
    pub fn new(lang: String, theme: Theme, no_color: bool) -> Self {
        Self {
            mode: Mode::Reading,
            prior_mode: Mode::Reading,
            doc: None,
            links: Vec::new(),
            focused_link: None,
            sections: Vec::new(),
            selected_section: 0,
            back_stack: Vec::new(),
            forward_stack: Vec::new(),
            scroll: 0,
            max_scroll: 0,
            status: "Press / to search, ? for help, q to quit".to_string(),
            search_input: String::new(),
            results: Vec::new(),
            selected_result: 0,
            should_quit: false,
            lang,
            loading: false,
            pending_g: false,
            theme,
            no_color,
            find_input: String::new(),
            find_matches: Vec::new(),
            find_index: 0,
            citations: Vec::new(),
            selected_citation: 0,
            research: ResearchStore::load(),
            selected_library: 0,
            cite_style: CiteStyle::Apa,
            library_prior_mode: Mode::Reading,
            pending_export_overwrite: None,
            page_source: PageSource::None,
        }
    }

    pub fn cycle_theme(&mut self) {
        self.theme = self.theme.next();
        self.status = format!("Theme: {}", self.theme.name);
    }

    /// The open article's canonical URL — the `y` yank payload (FR-NV-10).
    pub fn yank_url(&self) -> Option<String> {
        self.doc
            .as_ref()
            .map(|d| crate::research::article_url(&d.title, &self.lang))
    }

    /// A Markdown link to the open article — the `Y` yank payload; terminal
    /// users paste these into notes constantly (PRD FR-NV-10's rationale).
    pub fn yank_markdown(&self) -> Option<String> {
        self.doc.as_ref().map(|d| {
            format!(
                "[{}]({})",
                d.title,
                crate::research::article_url(&d.title, &self.lang)
            )
        })
    }

    /// Open a document reached by a fresh navigation (search result, CLI
    /// title, or following a link): the article we were reading, if any,
    /// becomes the back-stack top, and any forward history is discarded —
    /// standard browser back/forward semantics (PRD FR-TB-2).
    pub fn open_document(&mut self, doc: Document) {
        if let Some(current) = &self.doc {
            self.back_stack.push(current.title.clone());
        }
        self.forward_stack.clear();
        self.set_document(doc);
    }

    /// Returns the title to fetch for "go back", already adjusting the
    /// back/forward stacks — the caller fetches it and finishes the
    /// navigation with `set_document`, which must NOT touch the stacks
    /// again (this method already did).
    pub fn navigate_back_target(&mut self) -> Option<String> {
        let target = self.back_stack.pop()?;
        if let Some(current) = &self.doc {
            self.forward_stack.push(current.title.clone());
        }
        Some(target)
    }

    /// The forward-history counterpart of `navigate_back_target`.
    pub fn navigate_forward_target(&mut self) -> Option<String> {
        let target = self.forward_stack.pop()?;
        if let Some(current) = &self.doc {
            self.back_stack.push(current.title.clone());
        }
        Some(target)
    }

    /// Install a document without touching the back/forward stacks (used
    /// after `navigate_back_target`/`navigate_forward_target`, which
    /// already adjusted them).
    pub fn set_document(&mut self, doc: Document) {
        self.links = collect_links(&doc);
        self.focused_link = if self.links.is_empty() { None } else { Some(0) };
        self.sections = section_outline(&doc);
        self.selected_section = 0;

        // Element 0 is always this article's own citation; the rest are
        // whatever it cites (Research mode, PRD-adjacent feature request).
        let mut citations = vec![crate::research::self_citation(&doc.title, &self.lang)];
        citations.extend(doc.citations.iter().cloned());
        self.citations = citations;
        self.selected_citation = 0;

        self.status = format!(
            "{}{} — {} blocks, {} links, {} sections, {} citations",
            self.page_source.prefix(),
            doc.title,
            doc.blocks.len(),
            self.links.len(),
            self.sections.len(),
            self.citations.len()
        );
        self.doc = Some(doc);
        self.scroll = 0;
        self.mode = Mode::Reading;
        self.clear_find();
    }

    pub fn cycle_citation(&mut self, forward: bool) {
        if self.citations.is_empty() {
            return;
        }
        let len = self.citations.len();
        self.selected_citation = if forward {
            (self.selected_citation + 1) % len
        } else {
            (self.selected_citation + len - 1) % len
        };
    }

    /// Saves the currently-selected citation (the article's own, or one of
    /// its references) to the research bibliography.
    pub fn save_selected_citation(&mut self) {
        let Some(citation) = self.citations.get(self.selected_citation).cloned() else {
            return;
        };
        let source_article = self
            .doc
            .as_ref()
            .map(|d| d.title.clone())
            .unwrap_or_default();
        // The synthetic self-citation (id "self", always element 0 — see
        // set_document) is the only entry with known structure that styles
        // can re-format; everything else is verbatim reference text.
        let kind = if citation.id == "self" {
            crate::research::CitationKind::Article
        } else {
            crate::research::CitationKind::Reference
        };
        self.research.add(SavedCitation {
            source_article,
            source_lang: self.lang.clone(),
            text: citation.text,
            url: citation.url,
            saved_at: crate::research::today(),
            kind,
        });
        self.status = format!(
            "Saved to research collection ({} total)",
            self.research.citations.len()
        );
    }

    /// Whether the Research picker's entry at `index` is currently present
    /// in the saved bibliography. A live lookup rather than a session flag,
    /// so deleting the entry from the library immediately un-checks it in
    /// the picker instead of leaving a stale "already saved" marker.
    pub fn is_citation_saved(&self, index: usize) -> bool {
        self.citations.get(index).is_some_and(|c| {
            self.research
                .citations
                .iter()
                .any(|saved| saved.text == c.text && saved.url == c.url)
        })
    }

    /// Opens the library view over the whole saved bibliography. The
    /// status line doubles as the transient-feedback channel here (delete
    /// and export overwrite it with their outcome), so it starts as a key
    /// hint.
    pub fn open_library(&mut self) {
        self.library_prior_mode = self.mode;
        self.mode = Mode::Library;
        self.selected_library = self
            .selected_library
            .min(self.research.citations.len().saturating_sub(1));
        self.pending_export_overwrite = None;
        self.status =
            "j/k: move   s: style   d: delete   e: export to file   Esc: close".to_string();
    }

    /// Returns to wherever the library was opened from (Reading or
    /// Research), clearing the library's transient status so its key hints
    /// don't linger on the reading status bar.
    pub fn close_library(&mut self) {
        self.mode = self.library_prior_mode;
        self.status = match &self.doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    pub fn cycle_library(&mut self, forward: bool) {
        let len = self.research.citations.len();
        if len == 0 {
            return;
        }
        self.selected_library = if forward {
            (self.selected_library + 1) % len
        } else {
            (self.selected_library + len - 1) % len
        };
    }

    pub fn cycle_cite_style(&mut self) {
        self.cite_style = self.cite_style.next();
        self.status = format!("Citation style: {}", self.cite_style.label());
    }

    /// Deletes the library-selected entry from the bibliography (and its
    /// on-disk file), keeping the selection on a valid index afterwards.
    /// A persistence failure is reported honestly — the entry is gone from
    /// this session but will be back next launch.
    pub fn delete_selected_library(&mut self) {
        if let Some((_, persisted)) = self.research.remove(self.selected_library) {
            let len = self.research.citations.len();
            if len == 0 {
                self.selected_library = 0;
            } else {
                self.selected_library = self.selected_library.min(len - 1);
            }
            self.status = match persisted {
                Ok(()) => format!("Deleted — {len} citations remain"),
                Err(e) => {
                    format!("Deleted from this session, but updating the file failed: {e}")
                }
            };
        }
    }

    /// Exports the whole bibliography, in the current style, to a Markdown
    /// file in the working directory (where a researcher's project lives;
    /// `--export-bibliography` covers the pipe-to-anywhere case).
    pub fn export_bibliography(&mut self) {
        self.export_bibliography_to(std::path::Path::new("."));
    }

    /// The testable core of `export_bibliography`: same behavior, explicit
    /// target directory. Overwriting an existing export (which the user
    /// may have hand-annotated) requires a second confirming `e` press.
    pub fn export_bibliography_to(&mut self, dir: &std::path::Path) {
        if self.research.citations.is_empty() {
            self.status = "Nothing to export — the bibliography is empty".to_string();
            return;
        }
        let filename = format!("bibliography-{}.md", self.cite_style.name());
        let target = dir.join(&filename);
        if target.exists() && self.pending_export_overwrite.as_deref() != Some(filename.as_str()) {
            self.pending_export_overwrite = Some(filename.clone());
            self.status = format!("./{filename} already exists — press e again to overwrite");
            return;
        }
        self.pending_export_overwrite = None;
        let content = crate::cite::format_bibliography(&self.research.citations, self.cite_style);
        self.status = match std::fs::write(&target, content) {
            Ok(()) => format!(
                "Exported {} citations to ./{filename} ({})",
                self.research.citations.len(),
                self.cite_style.label()
            ),
            Err(e) => format!("Export failed: {e}"),
        };
    }

    /// Clears any in-page find state — a fresh article's matches would be
    /// meaningless leftovers from whatever was open before.
    pub fn clear_find(&mut self) {
        self.find_input.clear();
        self.find_matches.clear();
        self.find_index = 0;
    }

    /// Recomputes `find_matches` for the current `find_input` against the
    /// open document and jumps to the first hit, if any.
    pub fn update_find(&mut self) {
        self.find_matches = match &self.doc {
            Some(doc) => find_matches(doc, &self.find_input),
            None => Vec::new(),
        };
        self.find_index = 0;
        if let Some(&line) = self.find_matches.first() {
            self.scroll = line.min(self.max_scroll);
        }
    }

    pub fn find_next(&mut self) {
        if self.find_matches.is_empty() {
            return;
        }
        self.find_index = (self.find_index + 1) % self.find_matches.len();
        self.scroll = self.find_matches[self.find_index].min(self.max_scroll);
    }

    pub fn find_prev(&mut self) {
        if self.find_matches.is_empty() {
            return;
        }
        self.find_index = (self.find_index + self.find_matches.len() - 1) % self.find_matches.len();
        self.scroll = self.find_matches[self.find_index].min(self.max_scroll);
    }

    /// Scroll to the given section's heading line, clamped to what's
    /// actually scrollable (a section near the end of a short article may
    /// not have `max_scroll` lines below it).
    pub fn jump_to_section(&mut self, index: usize) {
        if let Some(section) = self.sections.get(index) {
            self.scroll = section.line.min(self.max_scroll);
        }
        self.mode = Mode::Reading;
    }

    pub fn cycle_link(&mut self, forward: bool) {
        if self.links.is_empty() {
            self.status = "No links on this page".to_string();
            return;
        }
        let len = self.links.len();
        self.focused_link = Some(match self.focused_link {
            None => 0,
            Some(i) if forward => (i + 1) % len,
            Some(i) => (i + len - 1) % len,
        });
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let new = (self.scroll as i32 + delta).clamp(0, self.max_scroll as i32);
        self.scroll = new as u16;
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.max_scroll;
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
        }
    }

    #[test]
    fn back_and_forward_mirror_browser_semantics() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);

        app.open_document(doc("A"));
        app.open_document(doc("B"));
        app.open_document(doc("C"));
        assert_eq!(app.back_stack, vec!["A", "B"]);
        assert!(app.forward_stack.is_empty());
        assert_eq!(app.doc.as_ref().unwrap().title, "C");

        let target = app.navigate_back_target().unwrap();
        assert_eq!(target, "B");
        assert_eq!(app.back_stack, vec!["A"]);
        assert_eq!(app.forward_stack, vec!["C"]);
        // navigate_back_target only adjusts the stacks; the caller installs
        // the fetched document via set_document.
        app.set_document(doc("B"));

        let target = app.navigate_forward_target().unwrap();
        assert_eq!(target, "C");
        assert_eq!(app.back_stack, vec!["A", "B"]);
        assert!(app.forward_stack.is_empty());
    }

    #[test]
    fn following_a_link_after_going_back_discards_forward_history() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("A"));
        app.open_document(doc("B"));

        let target = app.navigate_back_target().unwrap();
        assert_eq!(target, "A");
        app.set_document(doc("A"));
        assert_eq!(app.forward_stack, vec!["B"]);

        // Reading A and following a different link (fresh navigation) should
        // drop the "forward to B" branch, exactly like a browser.
        app.open_document(doc("Z"));
        assert!(app.forward_stack.is_empty());
        assert_eq!(app.back_stack, vec!["A"]);
    }

    #[test]
    fn navigate_back_on_empty_history_returns_none_and_does_not_panic() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert_eq!(app.navigate_back_target(), None);
        assert_eq!(app.navigate_forward_target(), None);
    }

    #[test]
    fn cycle_link_wraps_in_both_directions() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.links = vec![
            LinkRef {
                href: "./A".into(),
                text: "A".into(),
                internal_title: Some("A".into()),
            },
            LinkRef {
                href: "./B".into(),
                text: "B".into(),
                internal_title: Some("B".into()),
            },
            LinkRef {
                href: "./C".into(),
                text: "C".into(),
                internal_title: Some("C".into()),
            },
        ];
        app.focused_link = None;

        app.cycle_link(true);
        assert_eq!(app.focused_link, Some(0));
        app.cycle_link(true);
        app.cycle_link(true);
        assert_eq!(app.focused_link, Some(2));
        app.cycle_link(true); // wraps forward past the end
        assert_eq!(app.focused_link, Some(0));
        app.cycle_link(false); // wraps backward past the start
        assert_eq!(app.focused_link, Some(2));
    }

    #[test]
    fn cycle_link_on_linkless_page_leaves_focus_unset() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.cycle_link(true);
        assert_eq!(app.focused_link, None);
    }

    #[test]
    fn jump_to_section_clamps_to_max_scroll() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.sections = vec![SectionRef {
            level: 2,
            title: "Late Section".to_string(),
            line: 500,
        }];
        app.max_scroll = 30; // a short article: the recorded line is past the end
        app.mode = Mode::Toc;

        app.jump_to_section(0);
        assert_eq!(app.scroll, 30);
        assert_eq!(app.mode, Mode::Reading);
    }

    #[test]
    fn jump_to_section_out_of_range_does_not_panic() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.mode = Mode::Toc;
        app.jump_to_section(5); // no sections at all
        assert_eq!(app.mode, Mode::Reading, "should still return to Reading");
    }

    #[test]
    fn cycle_theme_advances_through_all_builtins() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let mut seen = vec![app.theme.name];
        for _ in 0..Theme::NAMES.len() {
            app.cycle_theme();
            seen.push(app.theme.name);
        }
        // Started at terminal, cycled through every theme, and landed back
        // on terminal — proving `T` really does visit all six.
        assert_eq!(seen.first(), Some(&"terminal"));
        assert_eq!(seen.last(), Some(&"terminal"));
        assert_eq!(seen.len(), Theme::NAMES.len() + 1);
    }

    #[test]
    fn update_find_locates_matches_and_jumps_to_the_first() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let html = "<html><body><p>alpha</p><p>bravo alpha</p><p>charlie</p></body></html>";
        app.set_document(crate::doc::parse_article_html("Test", html));
        app.max_scroll = 100; // pretend a long article so clamping never kicks in

        app.find_input = "alpha".to_string();
        app.update_find();

        assert_eq!(app.find_matches.len(), 2, "two paragraphs mention alpha");
        assert_eq!(
            app.scroll, app.find_matches[0],
            "should jump straight to the first match"
        );
    }

    #[test]
    fn find_next_and_prev_wrap_around() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let html = "<html><body><p>alpha</p><p>alpha</p><p>alpha</p></body></html>";
        app.set_document(crate::doc::parse_article_html("Test", html));
        app.max_scroll = 100;
        app.find_input = "alpha".to_string();
        app.update_find();
        assert_eq!(app.find_matches.len(), 3);

        assert_eq!(app.find_index, 0);
        app.find_next();
        assert_eq!(app.find_index, 1);
        app.find_next();
        assert_eq!(app.find_index, 2);
        app.find_next(); // wraps forward past the last match
        assert_eq!(app.find_index, 0);
        app.find_prev(); // wraps backward past the first match
        assert_eq!(app.find_index, 2);
    }

    #[test]
    fn opening_a_new_document_clears_stale_find_state() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "First",
            "<html><body><p>alpha</p></body></html>",
        ));
        app.max_scroll = 100;
        app.find_input = "alpha".to_string();
        app.update_find();
        assert_eq!(app.find_matches.len(), 1);

        app.set_document(crate::doc::parse_article_html(
            "Second",
            "<html><body><p>bravo</p></body></html>",
        ));
        assert!(
            app.find_input.is_empty(),
            "a new article's matches must not carry over from the old one"
        );
        assert!(app.find_matches.is_empty());
    }

    #[test]
    fn find_next_and_prev_on_no_matches_do_not_panic() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.find_next();
        app.find_prev();
        assert_eq!(app.find_index, 0);
    }

    /// A citations-bearing fixture, used instead of the plain `doc()`
    /// helper so `set_document`'s extracted-citations wiring has real
    /// References-section content to pick up.
    fn doc_with_two_references(title: &str) -> Document {
        let html = r##"<html><body>
            <ol class="references">
              <li id="cite_note-1"><span class="reference-text">First source</span></li>
              <li id="cite_note-2"><span class="reference-text">Second source. <a href="https://example.com/b">https://example.com/b</a></span></li>
            </ol>
        </body></html>"##;
        crate::doc::parse_article_html(title, html)
    }

    #[test]
    fn set_document_puts_the_self_citation_first() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(doc_with_two_references("Test Article"));

        assert_eq!(
            app.citations.len(),
            3,
            "self-citation plus two extracted references"
        );
        assert_eq!(app.citations[0].id, "self");
        assert!(app.citations[0].text.contains("Test Article"));
        assert_eq!(app.citations[1].id, "cite_note-1");
        assert_eq!(app.citations[2].id, "cite_note-2");
        assert!(
            (0..3).all(|i| !app.is_citation_saved(i)),
            "nothing saved yet"
        );
    }

    #[test]
    fn cycle_citation_wraps_in_both_directions() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(doc_with_two_references("Test"));
        assert_eq!(app.citations.len(), 3);

        assert_eq!(app.selected_citation, 0);
        app.cycle_citation(true);
        assert_eq!(app.selected_citation, 1);
        app.cycle_citation(true);
        assert_eq!(app.selected_citation, 2);
        app.cycle_citation(true); // wraps forward past the last
        assert_eq!(app.selected_citation, 0);
        app.cycle_citation(false); // wraps backward past the first
        assert_eq!(app.selected_citation, 2);
    }

    #[test]
    fn cycle_citation_with_no_document_is_a_no_op() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.cycle_citation(true);
        assert_eq!(app.selected_citation, 0);
    }

    #[test]
    fn save_selected_citation_marks_it_saved_and_records_the_source_article() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        // Never touch the real platform data directory from a test.
        app.research = crate::research::ResearchStore::in_memory();
        app.set_document(doc_with_two_references("Test Article"));

        app.selected_citation = 2; // the reference with a URL
        app.save_selected_citation();

        assert!(!app.is_citation_saved(0));
        assert!(!app.is_citation_saved(1));
        assert!(app.is_citation_saved(2));
        assert_eq!(app.research.citations.len(), 1);
        let saved = &app.research.citations[0];
        assert_eq!(saved.source_article, "Test Article");
        assert!(saved.text.contains("Second source"));
        assert_eq!(saved.url.as_deref(), Some("https://example.com/b"));
    }

    #[test]
    fn saving_the_self_citation_entry_works_too() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.research = crate::research::ResearchStore::in_memory();
        app.set_document(doc_with_two_references("Test Article"));

        app.selected_citation = 0; // the article's own citation
        app.save_selected_citation();

        assert_eq!(app.research.citations.len(), 1);
        assert!(app.research.citations[0].text.contains("Test Article"));
    }

    #[test]
    fn save_selected_citation_out_of_range_does_not_panic() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.research = crate::research::ResearchStore::in_memory();
        app.selected_citation = 99; // no document loaded, citations is empty
        app.save_selected_citation();
        assert!(app.research.citations.is_empty());
    }

    #[test]
    fn opening_a_new_document_resets_citation_selection_and_saved_flags() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.research = crate::research::ResearchStore::in_memory();
        app.set_document(doc_with_two_references("First"));
        app.selected_citation = 2;
        app.save_selected_citation();
        assert!(app.is_citation_saved(2));

        app.set_document(doc("Second")); // no references section
        assert_eq!(
            app.citations.len(),
            1,
            "just the self-citation for the new article"
        );
        assert!(
            !app.is_citation_saved(0),
            "the new article's own citation hasn't been saved"
        );
        assert_eq!(app.selected_citation, 0);
        // The previous save is still in the running bibliography, though —
        // switching articles must not lose earlier session saves.
        assert_eq!(app.research.citations.len(), 1);
    }

    /// The staleness bug from the code review: save a citation, delete it
    /// from the library, and the Research picker's checkmark must clear —
    /// telling the user something is in their bibliography when it isn't
    /// would silently hole the bibliography.
    #[test]
    fn deleting_from_the_library_unchecks_the_research_picker() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.research = crate::research::ResearchStore::in_memory();
        app.set_document(doc_with_two_references("Test Article"));

        app.selected_citation = 1;
        app.save_selected_citation();
        assert!(app.is_citation_saved(1));

        app.open_library();
        app.selected_library = 0;
        app.delete_selected_library();

        assert!(
            !app.is_citation_saved(1),
            "the picker must reflect the deletion immediately"
        );
    }

    /// An App with an in-memory store pre-seeded with three saved
    /// citations, for exercising the library view's state machine.
    fn app_with_library() -> App {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.research = crate::research::ResearchStore::in_memory();
        app.set_document(doc_with_two_references("Test Article"));
        for i in 0..3 {
            app.selected_citation = i;
            app.save_selected_citation();
        }
        app
    }

    #[test]
    fn open_library_clamps_a_stale_selection() {
        let mut app = app_with_library();
        app.selected_library = 99;
        app.open_library();
        assert_eq!(app.mode, Mode::Library);
        assert_eq!(app.selected_library, 2, "clamped to the last valid index");
    }

    #[test]
    fn open_library_with_empty_store_does_not_underflow() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.research = crate::research::ResearchStore::in_memory();
        app.open_library();
        assert_eq!(app.selected_library, 0);
    }

    #[test]
    fn cycle_library_wraps_and_delete_keeps_selection_valid() {
        let mut app = app_with_library();
        app.open_library();

        app.cycle_library(false); // wraps backward from 0
        assert_eq!(app.selected_library, 2);

        // Deleting the last entry must pull the selection back in range.
        app.delete_selected_library();
        assert_eq!(app.research.citations.len(), 2);
        assert_eq!(app.selected_library, 1);

        app.delete_selected_library();
        app.delete_selected_library();
        assert!(app.research.citations.is_empty());
        assert_eq!(app.selected_library, 0);
        // One more delete on an empty library must be a no-op.
        app.delete_selected_library();
        assert_eq!(app.selected_library, 0);
    }

    #[test]
    fn export_writes_the_bibliography_file_where_asked() {
        let dir = std::env::temp_dir().join(format!("wikitui-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = app_with_library();
        app.cite_style = crate::cite::CiteStyle::Harvard;
        app.export_bibliography_to(&dir);

        let exported = std::fs::read_to_string(dir.join("bibliography-harvard.md"))
            .expect("export file written");
        assert!(exported.contains("Harvard"));
        assert!(exported.contains("## Sources consulted"));
        assert!(exported.contains("'Test Article' (n.d.) Wikipedia."));
        assert!(
            app.status.contains("Exported 3 citations"),
            "{}",
            app.status
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Overwriting an existing export (which the user may have edited)
    /// needs a confirming second press; changing style resets the pending
    /// confirmation because the target filename changes.
    #[test]
    fn export_over_an_existing_file_requires_a_second_press() {
        let dir =
            std::env::temp_dir().join(format!("wikitui-export-confirm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = app_with_library();
        let target = dir.join("bibliography-apa.md");
        std::fs::write(&target, "hand-annotated notes").unwrap();

        app.export_bibliography_to(&dir);
        assert!(
            app.status.contains("press e again to overwrite"),
            "{}",
            app.status
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "hand-annotated notes",
            "the first press must not touch the file"
        );

        app.export_bibliography_to(&dir);
        assert!(
            app.status.contains("Exported 3 citations"),
            "{}",
            app.status
        );
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains("## Sources consulted")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn library_esc_returns_to_the_mode_it_was_opened_from() {
        let mut app = app_with_library();
        app.mode = Mode::Research;
        app.open_library();
        assert_eq!(app.mode, Mode::Library);
        app.close_library();
        assert_eq!(
            app.mode,
            Mode::Research,
            "opened from Research, must return there"
        );

        app.mode = Mode::Reading;
        app.open_library();
        app.close_library();
        assert_eq!(app.mode, Mode::Reading);
        assert!(
            !app.status.contains("d: delete"),
            "library key hints must not linger on the reading status bar"
        );
    }

    #[test]
    fn export_with_empty_library_reports_instead_of_writing() {
        let dir = std::env::temp_dir().join(format!("wikitui-export-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.research = crate::research::ResearchStore::in_memory();
        app.export_bibliography_to(&dir);

        assert!(app.status.contains("Nothing to export"));
        assert!(!dir.join("bibliography-apa.md").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
