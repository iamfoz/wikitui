use crate::api::SearchResult;
use crate::doc::{Document, LinkRef, collect_links};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reading,
    Search,
    Results,
    Help,
}

pub struct App {
    pub mode: Mode,
    pub prior_mode: Mode,
    pub doc: Option<Document>,
    pub links: Vec<LinkRef>,
    pub focused_link: Option<usize>,
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
}

impl App {
    pub fn new(lang: String) -> Self {
        Self {
            mode: Mode::Reading,
            prior_mode: Mode::Reading,
            doc: None,
            links: Vec::new(),
            focused_link: None,
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
        }
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
        self.status = format!(
            "{} — {} blocks, {} links",
            doc.title,
            doc.blocks.len(),
            self.links.len()
        );
        self.doc = Some(doc);
        self.scroll = 0;
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
        let mut app = App::new("en".to_string());

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
        let mut app = App::new("en".to_string());
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
        let mut app = App::new("en".to_string());
        assert_eq!(app.navigate_back_target(), None);
        assert_eq!(app.navigate_forward_target(), None);
    }

    #[test]
    fn cycle_link_wraps_in_both_directions() {
        let mut app = App::new("en".to_string());
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
        let mut app = App::new("en".to_string());
        app.cycle_link(true);
        assert_eq!(app.focused_link, None);
    }
}
