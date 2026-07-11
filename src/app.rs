use crate::api::SearchResult;
use crate::doc::{Document, LinkRef, SectionRef, collect_links, find_matches, section_outline};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reading,
    Search,
    Results,
    Toc,
    Find,
    Help,
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
        }
    }

    pub fn cycle_theme(&mut self) {
        self.theme = self.theme.next();
        self.status = format!("Theme: {}", self.theme.name);
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
        self.status = format!(
            "{} — {} blocks, {} links, {} sections",
            doc.title,
            doc.blocks.len(),
            self.links.len(),
            self.sections.len()
        );
        self.doc = Some(doc);
        self.scroll = 0;
        self.mode = Mode::Reading;
        self.clear_find();
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
}
