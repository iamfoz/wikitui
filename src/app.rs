use crate::api::SearchResult;
use crate::doc::Document;

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

    pub fn open_document(&mut self, doc: Document) {
        self.status = format!("{} — {} blocks", doc.title, doc.blocks.len());
        self.doc = Some(doc);
        self.scroll = 0;
        self.mode = Mode::Reading;
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
