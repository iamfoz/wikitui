//! The start page (FR-DL-1), the `:today` on-this-day panel (FR-DL-2), and
//! the TIL widget (FR-DL-7) — pure data-model code, unit-testable without
//! the network, exactly like `prefetch.rs`'s feed parsing this builds on.
//!
//! Architecture: `App` owns a `Arc<Mutex<prefetch::FeedCache>>` shared with
//! the background substrate's executor (the "one daily Wikifeeds call" seam
//! `prefetch::FeedCache::get` documents). Every draw while the start page is
//! showing, `App::start_page_model` locks that cache, clones the parsed
//! [`crate::prefetch::FeaturedFeed`] if it has arrived, and calls
//! [`StartPageModel::from_feed`] here — cheap enough (a handful of short
//! strings) to rebuild per frame rather than cache, so there is no second
//! "is this stale" question to answer. Before the feed arrives (or if it
//! never does — offline), the model is a loading skeleton or the graceful
//! [`StartPageModel::offline_fallback`], never an error state.
//!
//! The on-this-day panel (FR-DL-2) is a *separate* concern from the bundled
//! feed: Wikifeeds' dedicated `feed/onthisday/{type}/{m}/{d}` endpoint is one
//! call per type, fetched on demand when `:today` opens (a foreground fetch,
//! like search — see `api::WikiClient::fetch_onthisday_feed`'s doc comment),
//! not folded into the once-a-day prefetch budget.

use crate::prefetch::{FeaturedFeed, OtdEntry};

// -- The start page (FR-DL-1) -----------------------------------------------

/// `startpage = feed|blank|resume` (PRD FR-DL-1). Parsed here so both
/// `config::resolve` (file/env validation) and `App` (behavior) share one
/// definition of the three valid spellings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartPageConfig {
    /// The rich feed-backed start page (default).
    #[default]
    Feed,
    /// The original minimal welcome text — no network, no navigation.
    Blank,
    /// Reopen the last session's article. Session restore proper is a later
    /// chunk (PRD B18); until then this resolves to the most recent reading-
    /// history entry if one exists (cheap: history is already loaded at
    /// startup for the visited-styling feature), else falls back to `Feed` —
    /// both branches implemented at the one call site, `main::run`'s
    /// startup sequence (right after the CLI title/search handling).
    Resume,
}

impl StartPageConfig {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "feed" => Some(Self::Feed),
            "blank" => Some(Self::Blank),
            "resume" => Some(Self::Resume),
            _ => None,
        }
    }
}

/// Which part of the start page an item belongs to — drives the section
/// header `draw_reading`'s start-page renderer groups items under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Tfa,
    MostRead,
    News,
    Otd,
    Til,
    /// The offline-fallback "recent" list (this session's/earlier reading
    /// history) — only ever populated by [`StartPageModel::offline_fallback`].
    Recent,
    /// The offline-fallback pinned-saved-pages list.
    Saved,
}

impl Section {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tfa => "Today's featured article",
            Self::MostRead => "Most read",
            Self::News => "In the news",
            Self::Otd => "On this day",
            Self::Til => "Did you know",
            Self::Recent => "Recent",
            Self::Saved => "Saved pages",
        }
    }
}

/// One focusable/selectable row on the start page (PRD FR-DL-1: "a real
/// navigable view, not static text"). `open_target` is `None` for a row with
/// nothing to follow (e.g. a news item whose story linked no article) —
/// still shown, just not an Enter target.
#[derive(Debug, Clone, PartialEq)]
pub struct StartPageItem {
    pub section: Section,
    /// A short badge shown before the title (`★FA` for the featured
    /// article) — kept separate from `title` so the plain article title is
    /// always what a test or a fetch call sees.
    pub badge: Option<&'static str>,
    pub title: String,
    /// Secondary text under the title: the TFA's extract, a view count, …
    pub detail: Option<String>,
    pub open_target: Option<String>,
    /// The language to switch to before opening `open_target`, when it
    /// differs from the session's current language — only ever set by
    /// `offline_fallback` (history/saved entries can be in any language the
    /// reader visited); feed-derived items are always in the feed's own
    /// language (the session's current `App::lang`), so `None` there means
    /// "open in whatever language is already active."
    pub open_lang: Option<String>,
}

impl StartPageItem {
    fn new(
        section: Section,
        badge: Option<&'static str>,
        title: String,
        detail: Option<String>,
        open_target: Option<String>,
    ) -> Self {
        Self {
            section,
            badge,
            title,
            detail,
            open_target,
            open_lang: None,
        }
    }

    fn with_lang(section: Section, title: String, detail: Option<String>, lang: String) -> Self {
        Self {
            section,
            badge: None,
            title: title.clone(),
            detail,
            open_target: Some(title),
            open_lang: Some(lang),
        }
    }
}

/// One rotation candidate for the TIL widget (FR-DL-7).
#[derive(Debug, Clone, PartialEq)]
pub struct TilFact {
    pub text: String,
    pub open_target: Option<String>,
}

/// The start page's full render model: the flat, navigable item list plus
/// the picture-of-the-day fields (rendered separately from the item list —
/// PRD FR-DL-1 treats POTD as an image/caption, not a link to follow).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StartPageModel {
    pub items: Vec<StartPageItem>,
    pub potd_title: Option<String>,
    pub potd_thumb_url: Option<String>,
    /// The feed hasn't arrived yet, but a fetch is (or may still be) in
    /// flight — render a skeleton, not an error (§6.8: never block startup).
    pub loading: bool,
    /// The feed never arrived and nothing is in flight (offline, or the
    /// fetch failed/was skipped) — `items` then holds the offline fallback
    /// (recent history / saved pages) rather than feed sections.
    pub offline: bool,
}

impl StartPageModel {
    /// Top-5 most-read (PRD FR-DL-1's exact wording).
    pub const MOSTREAD_TOP_N: usize = 5;
    /// A "condensed" on-this-day strip, not the full day (that's `:today`'s
    /// job, FR-DL-2).
    pub const OTD_STRIP_N: usize = 3;

    /// Rendered immediately at startup/`gh`/`:start`, before the daily feed
    /// (if any) has arrived — an empty item list, `loading: true`. Never
    /// blocks: `App::start_page_model` picks this whenever the cache has
    /// nothing yet and a fetch could still land (§6.8 cold-start budget).
    pub fn skeleton() -> Self {
        Self {
            loading: true,
            ..Self::default()
        }
    }

    /// Builds the navigable item list from one day's parsed feed. `til` is
    /// computed by the caller (`pick_til`) since it needs the date/reroll
    /// seed this module has no business reading off a clock itself.
    pub fn from_feed(feed: &FeaturedFeed, til: Option<TilFact>) -> Self {
        let mut items = Vec::new();

        if let Some(title) = &feed.tfa {
            items.push(StartPageItem::new(
                Section::Tfa,
                Some("★FA"),
                title.clone(),
                feed.extract.clone(),
                Some(title.clone()),
            ));
        }

        for mr in feed.mostread.iter().take(Self::MOSTREAD_TOP_N) {
            items.push(StartPageItem::new(
                Section::MostRead,
                None,
                mr.title.clone(),
                Some(format!(
                    "{} views/day",
                    crate::prefetch::format_views(mr.views)
                )),
                Some(mr.title.clone()),
            ));
        }

        for news in &feed.news {
            items.push(StartPageItem::new(
                Section::News,
                None,
                news.headline.clone(),
                None,
                news.page_title.clone(),
            ));
        }

        for otd in feed.onthisday.iter().take(Self::OTD_STRIP_N) {
            let title = match otd.year {
                Some(y) => format!("{y} — {}", otd.text),
                None => otd.text.clone(),
            };
            items.push(StartPageItem::new(
                Section::Otd,
                None,
                title,
                None,
                otd.page_title.clone(),
            ));
        }

        if let Some(til) = til {
            items.push(StartPageItem::new(
                Section::Til,
                None,
                til.text,
                None,
                til.open_target,
            ));
        }

        Self {
            items,
            potd_title: feed.potd.clone(),
            potd_thumb_url: feed.potd_thumb_url.clone(),
            loading: false,
            offline: false,
        }
    }

    /// The graceful degradation PRD FR-DL-1's networking discipline calls
    /// for: no feed arrived and nothing is still in flight (offline, or the
    /// substrate gave up) — show recent history and saved pages instead of
    /// an error. `recent`/`saved` are `(lang, title)` pairs; the caller
    /// (`App`) is responsible for sourcing them from `history::History` and
    /// `saved::SavedPages` so this module stays decoupled from storage.
    pub fn offline_fallback(recent: &[(String, String)], saved: &[(String, String)]) -> Self {
        let mut items = Vec::new();
        for (lang, title) in recent {
            items.push(StartPageItem::with_lang(
                Section::Recent,
                title.clone(),
                None,
                lang.clone(),
            ));
        }
        for (lang, title) in saved {
            items.push(StartPageItem::with_lang(
                Section::Saved,
                title.clone(),
                Some("saved offline".to_string()),
                lang.clone(),
            ));
        }
        Self {
            items,
            potd_title: None,
            potd_thumb_url: None,
            loading: false,
            offline: true,
        }
    }

    /// Moves the selection by `delta`, wrapping in both directions — j/k/
    /// Tab/Shift-Tab all route through this (PRD FR-DL-1: "Make it a real
    /// navigable view"). A no-op (stays 0) on an empty list.
    pub fn move_selection(&self, current: usize, delta: i32) -> usize {
        if self.items.is_empty() {
            return 0;
        }
        let len = self.items.len() as i32;
        let cur = (current as i32).rem_euclid(len);
        (cur + delta).rem_euclid(len) as usize
    }

    /// Clamps a selection index that may have gone stale (the model was
    /// rebuilt with fewer items — e.g. the feed just arrived and replaced
    /// the skeleton).
    pub fn clamp_selection(&self, selected: usize) -> usize {
        if self.items.is_empty() {
            0
        } else {
            selected.min(self.items.len() - 1)
        }
    }

    /// The `(lang override, title)` Enter should open for the item at
    /// `index`, or `None` if that item isn't openable (out of range, or a
    /// news/otd row with no linked article).
    pub fn open_target(&self, index: usize) -> Option<(Option<&str>, &str)> {
        let item = self.items.get(index)?;
        let title = item.open_target.as_deref()?;
        Some((item.open_lang.as_deref(), title))
    }
}

/// A small, fully deterministic string hash (FNV-1a) — used only to pick a
/// stable-per-day TIL index. Not cryptographic or collision-resistant, just
/// needs to be the same for the same `date` string on every call (unlike
/// `std`'s `RandomState`-backed hashers, which are seeded per-process).
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// FR-DL-7's rotation: deterministic per calendar day (stable across
/// redraws and across launches on the same day — reusing the codebase's
/// "thread the date string in, never read the wall clock in logic"
/// convention, see `prefetch::FeedCache`), with `reroll` letting a keypress
/// step to a different candidate within the same day without waiting for
/// tomorrow ("changes per app-launch or on a key" — this implements both:
/// a new day changes it on its own, and `reroll` changes it on demand).
///
/// Candidates are the bundled feed's on-this-day entries. The PRD's other
/// documented option — "a random Good Article extract" — needs
/// `list=random` joined with `prop=pageassessments`, a heavier call than the
/// FR-PF-2 one-call daily budget allows; this rotates through what that one
/// call already fetched instead, and the heavier path stays a documented
/// seam (`pick_til` is the one function to change once it lands).
pub fn pick_til(feed: &FeaturedFeed, date: &str, reroll: u64) -> Option<TilFact> {
    if feed.onthisday.is_empty() {
        return None;
    }
    let idx = (fnv1a(date).wrapping_add(reroll) as usize) % feed.onthisday.len();
    let entry = &feed.onthisday[idx];
    let text = match entry.year {
        Some(y) => format!("Did you know? {y} — {}", entry.text),
        None => format!("Did you know? {}", entry.text),
    };
    Some(TilFact {
        text,
        open_target: entry.page_title.clone(),
    })
}

// -- The `:today` on-this-day panel (FR-DL-2) -------------------------------

/// The five Wikifeeds on-this-day types (§6.2 rule 6 / Appendix A), each its
/// own tab in the `:today` panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OtdType {
    #[default]
    Events,
    Births,
    Deaths,
    Holidays,
    Selected,
}

impl OtdType {
    pub const ALL: [OtdType; 5] = [
        OtdType::Events,
        OtdType::Births,
        OtdType::Deaths,
        OtdType::Holidays,
        OtdType::Selected,
    ];

    /// The Wikifeeds URL path segment / JSON key (`feed/onthisday/{type}`).
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::Births => "births",
            Self::Deaths => "deaths",
            Self::Holidays => "holidays",
            Self::Selected => "selected",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Events => "Events",
            Self::Births => "Births",
            Self::Deaths => "Deaths",
            Self::Holidays => "Holidays",
            Self::Selected => "Selected",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).expect("in ALL")
    }

    /// Cycles forward, wrapping — the panel's Tab/`l` binding.
    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    /// Cycles backward, wrapping — the panel's Shift-Tab/`h` binding.
    pub fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// The `:today` panel's data: every type's entries, fetched up front (five
/// small foreground calls — see `api::WikiClient::fetch_onthisday_feed`'s
/// doc comment) when the panel opens, rather than lazily per tab switch.
/// Simplest coherent model for a v1.0/P1 feature; lazy-per-tab is a
/// documented later optimization if the extra calls ever matter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OnThisDayModel {
    pub events: Vec<OtdEntry>,
    pub births: Vec<OtdEntry>,
    pub deaths: Vec<OtdEntry>,
    pub holidays: Vec<OtdEntry>,
    pub selected: Vec<OtdEntry>,
}

impl OnThisDayModel {
    pub fn entries(&self, t: OtdType) -> &[OtdEntry] {
        match t {
            OtdType::Events => &self.events,
            OtdType::Births => &self.births,
            OtdType::Deaths => &self.deaths,
            OtdType::Holidays => &self.holidays,
            OtdType::Selected => &self.selected,
        }
    }

    pub fn set(&mut self, t: OtdType, entries: Vec<OtdEntry>) {
        match t {
            OtdType::Events => self.events = entries,
            OtdType::Births => self.births = entries,
            OtdType::Deaths => self.deaths = entries,
            OtdType::Holidays => self.holidays = entries,
            OtdType::Selected => self.selected = entries,
        }
    }

    /// Moves the selection within one type's entries by `delta`, wrapping —
    /// mirrors `StartPageModel::move_selection`.
    pub fn move_selection(&self, t: OtdType, current: usize, delta: i32) -> usize {
        let entries = self.entries(t);
        if entries.is_empty() {
            return 0;
        }
        let len = entries.len() as i32;
        let cur = (current as i32).rem_euclid(len);
        (cur + delta).rem_euclid(len) as usize
    }

    /// The Enter target (article title) for the selected entry in type `t`,
    /// or `None` if that entry has no linked article.
    pub fn open_target(&self, t: OtdType, selected: usize) -> Option<&str> {
        self.entries(t).get(selected)?.page_title.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::{MostRead, NewsItem};

    fn sample_feed() -> FeaturedFeed {
        FeaturedFeed {
            tfa: Some("Alan Turing".to_string()),
            extract: Some("Alan Turing was a mathematician.".to_string()),
            mostread: vec![
                MostRead {
                    title: "A".to_string(),
                    views: 9000,
                },
                MostRead {
                    title: "B".to_string(),
                    views: 8000,
                },
                MostRead {
                    title: "C".to_string(),
                    views: 7000,
                },
                MostRead {
                    title: "D".to_string(),
                    views: 6000,
                },
                MostRead {
                    title: "E".to_string(),
                    views: 5000,
                },
                MostRead {
                    title: "F (past top 5)".to_string(),
                    views: 1,
                },
            ],
            potd: Some("File:Example.jpg".to_string()),
            potd_thumb_url: Some("https://example.com/thumb.jpg".to_string()),
            news: vec![NewsItem {
                headline: "Something happened.".to_string(),
                page_title: Some("Enigma machine".to_string()),
            }],
            onthisday: vec![
                OtdEntry {
                    year: Some(1912),
                    text: "Turing born.".to_string(),
                    page_title: Some("Alan Turing".to_string()),
                },
                OtdEntry {
                    year: Some(1954),
                    text: "Turing died.".to_string(),
                    page_title: None,
                },
            ],
            onthisday_events: 2,
        }
    }

    #[test]
    fn from_feed_populates_every_section() {
        let model = StartPageModel::from_feed(&sample_feed(), None);
        let sections: Vec<Section> = model.items.iter().map(|i| i.section).collect();
        assert!(sections.contains(&Section::Tfa));
        assert!(sections.contains(&Section::MostRead));
        assert!(sections.contains(&Section::News));
        assert!(sections.contains(&Section::Otd));
        assert_eq!(model.potd_title.as_deref(), Some("File:Example.jpg"));
        assert_eq!(
            model.potd_thumb_url.as_deref(),
            Some("https://example.com/thumb.jpg")
        );
        assert!(!model.loading);
        assert!(!model.offline);

        let tfa = model
            .items
            .iter()
            .find(|i| i.section == Section::Tfa)
            .unwrap();
        assert_eq!(tfa.badge, Some("★FA"));
        assert_eq!(tfa.title, "Alan Turing");
        assert_eq!(
            tfa.detail.as_deref(),
            Some("Alan Turing was a mathematician.")
        );
        assert_eq!(tfa.open_target.as_deref(), Some("Alan Turing"));
    }

    #[test]
    fn mostread_is_capped_at_the_top_five() {
        let model = StartPageModel::from_feed(&sample_feed(), None);
        let mostread: Vec<&StartPageItem> = model
            .items
            .iter()
            .filter(|i| i.section == Section::MostRead)
            .collect();
        assert_eq!(
            mostread.len(),
            5,
            "top-5 per FR-DL-1, not all 6 fixture entries"
        );
        assert!(!mostread.iter().any(|i| i.title.contains("past top 5")));
    }

    #[test]
    fn til_item_included_when_a_fact_is_supplied() {
        let til = TilFact {
            text: "Did you know? 1912 — Turing born.".to_string(),
            open_target: Some("Alan Turing".to_string()),
        };
        let model = StartPageModel::from_feed(&sample_feed(), Some(til.clone()));
        let row = model
            .items
            .iter()
            .find(|i| i.section == Section::Til)
            .unwrap();
        assert_eq!(row.title, til.text);
        assert_eq!(row.open_target.as_deref(), Some("Alan Turing"));
    }

    #[test]
    fn empty_feed_builds_an_empty_but_valid_model() {
        let model = StartPageModel::from_feed(&FeaturedFeed::default(), None);
        assert!(model.items.is_empty());
        assert!(!model.loading);
        assert!(!model.offline);
    }

    #[test]
    fn skeleton_is_loading_with_no_items() {
        let model = StartPageModel::skeleton();
        assert!(model.loading);
        assert!(model.items.is_empty());
        assert!(!model.offline);
    }

    #[test]
    fn offline_fallback_lists_recent_and_saved_and_never_panics_when_both_are_empty() {
        let model = StartPageModel::offline_fallback(&[], &[]);
        assert!(model.offline);
        assert!(model.items.is_empty());

        let recent = vec![("en".to_string(), "Alan Turing".to_string())];
        let saved = vec![("de".to_string(), "Computer science".to_string())];
        let model = StartPageModel::offline_fallback(&recent, &saved);
        assert_eq!(model.items.len(), 2);
        assert_eq!(model.items[0].section, Section::Recent);
        assert_eq!(model.items[0].open_lang.as_deref(), Some("en"));
        assert_eq!(model.items[1].section, Section::Saved);
        assert_eq!(model.items[1].open_lang.as_deref(), Some("de"));
    }

    #[test]
    fn move_selection_wraps_in_both_directions() {
        let model = StartPageModel::from_feed(&sample_feed(), None);
        let last = model.items.len() - 1;
        assert_eq!(
            model.move_selection(0, -1),
            last,
            "up from the top wraps to the bottom"
        );
        assert_eq!(
            model.move_selection(last, 1),
            0,
            "down from the bottom wraps to the top"
        );
        assert_eq!(model.move_selection(0, 1), 1);
    }

    #[test]
    fn move_selection_on_an_empty_model_stays_at_zero() {
        let model = StartPageModel::default();
        assert_eq!(model.move_selection(0, 1), 0);
        assert_eq!(model.move_selection(0, -1), 0);
    }

    #[test]
    fn open_target_resolves_the_focused_items_article() {
        let model = StartPageModel::from_feed(&sample_feed(), None);
        let tfa_index = model
            .items
            .iter()
            .position(|i| i.section == Section::Tfa)
            .unwrap();
        assert_eq!(model.open_target(tfa_index), Some((None, "Alan Turing")));
        assert_eq!(
            model.open_target(9999),
            None,
            "out of range is None, not a panic"
        );
    }

    #[test]
    fn open_target_carries_the_offline_items_own_language() {
        let recent = vec![("de".to_string(), "Rechenmaschine".to_string())];
        let model = StartPageModel::offline_fallback(&recent, &[]);
        assert_eq!(model.open_target(0), Some((Some("de"), "Rechenmaschine")));
    }

    #[test]
    fn startpage_config_parses_the_three_values_and_rejects_others() {
        assert_eq!(StartPageConfig::parse("feed"), Some(StartPageConfig::Feed));
        assert_eq!(
            StartPageConfig::parse("blank"),
            Some(StartPageConfig::Blank)
        );
        assert_eq!(
            StartPageConfig::parse("resume"),
            Some(StartPageConfig::Resume)
        );
        assert_eq!(StartPageConfig::parse("bogus"), None);
        assert_eq!(StartPageConfig::default(), StartPageConfig::Feed);
    }

    // -- TIL rotation (FR-DL-7) ---------------------------------------------

    #[test]
    fn pick_til_is_deterministic_for_the_same_date_and_reroll() {
        let feed = sample_feed();
        let a = pick_til(&feed, "2026-07-14", 0);
        let b = pick_til(&feed, "2026-07-14", 0);
        assert_eq!(a, b, "same inputs must always pick the same candidate");
    }

    #[test]
    fn pick_til_reroll_can_advance_to_a_different_candidate() {
        let feed = sample_feed();
        let picks: std::collections::HashSet<Option<String>> = (0..feed.onthisday.len() as u64)
            .map(|reroll| pick_til(&feed, "2026-07-14", reroll).map(|f| f.text))
            .collect();
        assert!(
            picks.len() > 1,
            "stepping reroll across the candidate count must visit more than one fact: {picks:?}"
        );
    }

    #[test]
    fn pick_til_returns_none_when_the_feed_has_no_onthisday_entries() {
        assert_eq!(pick_til(&FeaturedFeed::default(), "2026-07-14", 0), None);
    }

    #[test]
    fn pick_til_opens_the_entrys_linked_article() {
        let feed = sample_feed();
        // Entry 0 ("Turing born") links Alan Turing, entry 1 links nothing;
        // stepping reroll across the candidate count must land on the
        // linked one at least once, with `open_target` carried through.
        let found_linked = (0..feed.onthisday.len() as u64).any(|reroll| {
            pick_til(&feed, "2026-07-14", reroll)
                .and_then(|f| f.open_target)
                .as_deref()
                == Some("Alan Turing")
        });
        assert!(
            found_linked,
            "at least one reroll must resolve to Alan Turing"
        );
    }

    // -- On-this-day panel model (FR-DL-2) -----------------------------------

    #[test]
    fn otd_type_cycles_forward_and_backward_through_all_five() {
        let mut t = OtdType::Events;
        for expected in [
            OtdType::Births,
            OtdType::Deaths,
            OtdType::Holidays,
            OtdType::Selected,
            OtdType::Events,
        ] {
            t = t.next();
            assert_eq!(t, expected);
        }
        assert_eq!(
            OtdType::Events.prev(),
            OtdType::Selected,
            "wraps backward too"
        );
    }

    #[test]
    fn otd_type_wire_names_match_the_wikifeeds_path_segments() {
        assert_eq!(OtdType::Events.wire_name(), "events");
        assert_eq!(OtdType::Births.wire_name(), "births");
        assert_eq!(OtdType::Deaths.wire_name(), "deaths");
        assert_eq!(OtdType::Holidays.wire_name(), "holidays");
        assert_eq!(OtdType::Selected.wire_name(), "selected");
    }

    #[test]
    fn onthisday_model_maps_each_type_to_its_own_entries() {
        let mut model = OnThisDayModel::default();
        let births = vec![OtdEntry {
            year: Some(1912),
            text: "Someone born.".to_string(),
            page_title: Some("Alan Turing".to_string()),
        }];
        model.set(OtdType::Births, births.clone());
        assert_eq!(model.entries(OtdType::Births), births.as_slice());
        assert!(model.entries(OtdType::Deaths).is_empty());
        assert_eq!(model.open_target(OtdType::Births, 0), Some("Alan Turing"));
        assert_eq!(model.open_target(OtdType::Deaths, 0), None);
    }

    #[test]
    fn onthisday_model_selection_wraps_per_type_independently() {
        let mut model = OnThisDayModel::default();
        model.set(
            OtdType::Events,
            vec![
                OtdEntry::default(),
                OtdEntry::default(),
                OtdEntry::default(),
            ],
        );
        assert_eq!(model.move_selection(OtdType::Events, 0, -1), 2);
        assert_eq!(model.move_selection(OtdType::Events, 2, 1), 0);
        // A type with no entries yet never panics — it just stays at 0.
        assert_eq!(model.move_selection(OtdType::Deaths, 0, 1), 0);
    }
}
