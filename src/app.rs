use std::time::{Duration, Instant};

use crate::api::{SearchResult, TitleSuggestion};
use crate::bookmarks::{
    self, Bookmark, BookmarkStore, ReadLaterEntry, ReadLaterStore, ToggleOutcome,
};
use crate::cache::PageCache;
use crate::cite::CiteStyle;
use crate::config::ConfigContext;
use crate::doc::{Citation, Document};
use crate::hints::{self, HintTarget};
use crate::layout::{self, Layout, LayoutCache, LayoutOptions};
use crate::research::{ResearchStore, SavedCitation};
use crate::tab::{HistoryEntry, Tab, TabId};
use crate::theme::Theme;

/// How many closed tabs the undo stack (`u` to reopen — PRD FR-TB-1) keeps.
/// A hard cap so a long session's closed-tab snapshots — which hold whole
/// `Document`s — don't accumulate unboundedly: closing beyond this drops the
/// oldest snapshot, so its `Document` is freed (§6.8's 10-tabs-< 150 MB
/// memory target). The tradeoff is deliberate: `u` walks back this many
/// closes, not to the dawn of the session.
pub const CLOSED_TABS_CAP: usize = 10;

/// PRD FR-SR-1's typeahead debounce window (spec calls for 150-250ms; 200ms
/// splits the difference). Reset on every keystroke while the Search prompt
/// has a query, so a burst of typing only fires one request, after it stops.
pub const TYPEAHEAD_DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reading,
    Search,
    Results,
    Toc,
    Find,
    Research,
    Library,
    Command,
    Help,
    /// The `bb` fuzzy tab picker (PRD FR-TB-1): a selectable list of open
    /// tabs — Enter switches, `d` closes, Esc cancels.
    TabPicker,
    /// The `gb` back-stack picker (PRD FR-NV-7): the active tab's history
    /// trail — Enter jumps to that entry (browser-style), Esc cancels.
    HistoryPicker,
    /// Vimium-style link hints (PRD FR-NV-1): every link visible in the
    /// viewport is labeled; typing narrows to one and follows it. Entered by
    /// `f` (follow in this tab) or `F` (open in a background tab — see
    /// `App::hint_background`); Esc cancels back to Reading.
    Hint,
    /// `B` / `:bookmarks` (PRD FR-BM-1): a selectable list of saved
    /// bookmarks. Enter opens (same tab), `d` deletes, `t` edits tags,
    /// `/` enters [`Mode::BookmarkFilter`]; Esc closes.
    BookmarkPicker,
    /// The bookmark picker's `/` live filter (tag-expression + fuzzy-title
    /// grammar — see `bookmarks::parse_filter`): typed characters narrow
    /// `App::bookmark_filter_input` on every keystroke. Enter/Esc return to
    /// [`Mode::BookmarkPicker`] with the filter still applied — only the
    /// picker's own Esc clears it.
    BookmarkFilter,
    /// The bookmark picker's `t` inline tag editor: a small prompt seeded
    /// with the selected bookmark's current tags, comma/space-separated.
    /// Enter replaces the tag set (`bookmarks::parse_tags`); Esc cancels.
    BookmarkTagEdit,
    /// `:readlater` (PRD FR-BM-3): the read-later queue, oldest first.
    /// Enter opens and (per `readlater_auto_dequeue`) removes the entry;
    /// `d` removes without opening; Esc closes.
    ReadLaterPicker,
    /// `Ctrl-h` / `:history` (PRD FR-HS-1): the *persistent*, cross-session
    /// reading-history picker backed by `history::History` — distinct from
    /// [`Mode::HistoryPicker`] above, which is the active tab's own
    /// back-stack trail and never survives a restart. Recency-weighted,
    /// fuzzy-filterable via `/` ([`Mode::ReadingHistoryFilter`]); `d`
    /// deletes that article's history, Enter reopens into the current tab,
    /// Esc closes.
    ReadingHistory,
    /// The reading-history picker's `/` live filter — mirrors
    /// [`Mode::BookmarkFilter`]'s split: every keystroke narrows
    /// `App::history_pick_filter` and rebuilds `App::history_pick_matches`;
    /// Enter/Esc both return to [`Mode::ReadingHistory`] with the filter
    /// still applied.
    ReadingHistoryFilter,
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

/// A background revalidation (PRD FR-OFF-2) found a newer revid and wrote
/// it into L2 while this exact (lang, title) was on screen; the "updated —
/// r to reload" notice and this value travel together, and `r` reloads
/// from L2 by that identity. Scoped this tightly so a revalidation result
/// for an article the reader has since navigated away from can never
/// clobber whatever they're reading now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReload {
    pub lang: String,
    pub title: String,
}

/// What a resolved link hint should do (PRD FR-NV-1), returned by
/// `App::resolve_hint_action` — see its doc comment for why this is decided
/// as plain data rather than performed inline (testability without the
/// network).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintFollowAction {
    /// Follow this internal link in the current tab (`f`).
    Foreground(String),
    /// Open this internal link in a new background tab (`F`).
    Background(String),
    /// The resolved link has no internal target — same "not yet followable"
    /// notice Enter already shows for a focused external link.
    External(String),
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
    /// Every open tab (PRD FR-TB-1, §2.2 tabs-as-buffers). Always non-empty
    /// while the app runs: closing the last tab quits. Per-view state (the
    /// document, links, scroll, per-tab history, find state) lives on each
    /// [`Tab`], not here — see `src/tab.rs`.
    pub tabs: Vec<Tab>,
    /// Index into `tabs` of the tab currently on screen.
    pub active: usize,
    /// Snapshots of closed tabs for `u` (reopen — PRD FR-TB-1), most-recent
    /// last. Capped at [`CLOSED_TABS_CAP`] so retained `Document`s drop.
    pub closed_tabs: Vec<Tab>,
    /// The next tab id to hand out (PRD FR-TB-3): stable across closes so a
    /// background completion routes to the right tab even after indices shift.
    pub next_tab_id: TabId,
    /// Selection cursor for the `bb` tab picker.
    pub selected_tab_pick: usize,
    /// Selection cursor for the `gb` back-stack picker.
    pub selected_history: usize,
    /// The `b`-prefix chord's pending-key latch (mirrors `pending_g` for
    /// `gg`): dispatches to `bb` (tab picker, PRD FR-TB-1) or `ba` (annotate
    /// the current article's bookmark, PRD FR-BM-2) by its second key — see
    /// `resolve_b_prefix`.
    pub pending_b: bool,
    /// `Q`'s one-keypress quit confirmation (PRD Appendix B): armed by `Q`,
    /// resolved by the next key (`y` quits, anything else cancels).
    pub pending_quit_confirm: bool,
    pub status: String,
    pub search_input: String,
    pub results: Vec<SearchResult>,
    pub selected_result: usize,
    /// PRD FR-SR-1's typeahead dropdown: title completions for the current
    /// `search_input`, most-recently-applied response only (stale ones are
    /// discarded before they ever reach this field — see
    /// `typeahead_is_current`).
    pub typeahead: Vec<TitleSuggestion>,
    /// Which `typeahead` entry Up/Down/Ctrl-n/Ctrl-p has highlighted; Enter
    /// opens this one directly.
    pub selected_suggestion: usize,
    /// When the typeahead debounce timer should fire next, set on every
    /// keystroke in Search mode (`queue_typeahead`) and cleared once sent.
    /// `None` means no request is queued.
    pub search_debounce_at: Option<Instant>,
    /// PRD FR-SR-4 / §7's "did you mean" suggestion from the last full-text
    /// search, shown by the zero-results view; `None` when the search had
    /// results or hasn't run yet.
    pub search_suggestion: Option<String>,
    pub should_quit: bool,
    /// The app-global "current" language for new searches and opens (PRD
    /// FR-ML-1/2, MVP slice). Kept in sync with the active tab's `lang` when
    /// switching tabs; each tab additionally records the language its own
    /// article was fetched in (for history restore and background routing).
    pub lang: String,
    /// A foreground blocking operation (search, initial open) is in progress.
    /// Distinct from a [`Tab`]'s own `loading` flag, which tracks a
    /// *background* tab's in-flight fetch and drives the tab bar's "…"
    /// indicator (PRD FR-TB-3).
    pub loading: bool,
    pub pending_g: bool,
    pub theme: Theme,
    /// Set once at startup from the `NO_COLOR` environment variable (PRD
    /// FR-TH-5): when true, every style still applies but with colors
    /// stripped, regardless of which theme is selected.
    pub no_color: bool,
    /// PRD FR-ACS-6 (`ACCESSIBLE=1`): drives the layout's collapse-to-list
    /// table path (and could gate further linear-leaning behavior). Set once
    /// at startup from the `ACCESSIBLE` environment variable; part of
    /// `layout_options`, so toggling it invalidates cached layouts.
    pub accessible: bool,
    /// Research mode's candidate list for the *active tab's* article: element
    /// 0 is always the article's own citation (`research::self_citation`);
    /// the rest are its extracted References entries, in document order.
    /// Rebuilt whenever the active document changes (open, reload, tab
    /// switch), so Research mode always reflects the tab on screen.
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
    /// The `:` command line's in-progress input (PRD FR-CS-2).
    pub command_input: String,
    /// Transient feedback from the last `:` command (":lang de" →
    /// "Language: de"), shown in the status bar with priority over the
    /// focused-link line — which would otherwise hide it instantly on any
    /// page with links — until the next keypress clears it.
    pub notice: Option<String>,
    /// The width-aware layout of the current document at the current
    /// `(width, options)` — a plain cache of the L1 lookup below, kept as
    /// its own field so painting and scroll math never touch `layout_cache`
    /// directly. Invalidated on doc change (set to `None`) and recomputed
    /// (or reused from `layout_cache`) on resize. Scroll math, section
    /// jump, find, and focused-link auto-scroll read line positions from
    /// here, so a theme/focus change stays O(paint) with no relayout.
    pub layout: Option<Layout>,
    /// PRD FR-OFF-1's L1 layer: a small in-memory LRU of already-laid-out
    /// documents (`ensure_layout` is the only reader/writer), so reopening
    /// an article at an unchanged identity/width/options doesn't repeat the
    /// layout pass. Keyed additionally by `current_revid` and the layout
    /// engine's schema version — see `layout::LayoutCacheKey`.
    pub layout_cache: LayoutCache,
    /// Incremented only when `ensure_layout` actually calls
    /// `layout::layout_document` (an L1 cache miss) — never on a hit or on
    /// the fast "nothing changed since last draw" path. A plain counter
    /// instrumentation point: proving an L1 hit skips relayout this way is
    /// far simpler than rigging up pointer-identity checks through a clone.
    pub layout_computations: u32,
    /// How many background revalidations are currently in flight (PRD
    /// FR-OFF-2). The main loop scopes its `event::poll` timeout to this
    /// being nonzero (mirroring Search mode's debounce-driven poll) so a
    /// revalidation's result is noticed without a keypress, while Reading
    /// mode still blocks in `event::read()` (0% idle CPU, PRD FR-ACS-2)
    /// whenever nothing is in flight. A counter rather than a bool because
    /// rapid navigation can overlap two revalidations (one per article).
    pub pending_revalidations: u32,
    /// The terminal width the reading view last drew at; layout is built for
    /// this width. Defaults to a sane 80 so line mappings resolve even before
    /// the first draw (e.g. in tests).
    pub layout_width: u16,
    /// The reading viewport height from the last draw, used to center find
    /// matches and to scroll a focused link into view. Zero until first draw.
    pub viewport_height: u16,
    /// Maximum line measure in cells (FR-RD-9, default 88); a config file
    /// will wire this later.
    pub measure: u16,
    /// East-Asian-Ambiguous width toggle (FR-RD-10, default false); a config
    /// file will wire this later.
    pub ambiguous_wide: bool,
    /// The active tab's currently-labeled link hints (PRD FR-NV-1), valid
    /// only while `mode == Mode::Hint`. Recomputed from scratch by
    /// `refresh_hint_targets` on entry and on every draw — never patched
    /// incrementally — so a resize mid-hint-mode can't leave a stale
    /// line/column behind (see that method's doc comment).
    pub hint_targets: Vec<HintTarget>,
    /// What's been typed so far in hint mode, narrowing `hint_targets` down
    /// to the labels that still start with it.
    pub hint_input: String,
    /// Which key opened hint mode: `false` for `f` (follow in this tab),
    /// `true` for `F` (open in a background tab, FR-TB-3 integration — see
    /// `resolve_hint_action`).
    pub hint_background: bool,
    /// The CLI/env overrides and config file path resolved at startup
    /// (PRD §6.7), kept so `:config reload` and SIGHUP can re-run
    /// resolution against the exact same precedence layers. Defaulted by
    /// `App::new` and overwritten by `main` right after construction — most
    /// tests never touch it (there's no file to reload from `::default()`).
    pub config_ctx: ConfigContext,

    // -- Inline images (PRD FR-RD-8, FR-TH-7) -----------------------------
    /// Decoded inline-image pixels for the session, keyed by source URL,
    /// populated by async fetch/decode (never blocking the UI). Read by
    /// `ensure_layout` (to reserve boxes) and paint (to fill them).
    pub image_store: crate::image::ImageStore,
    /// Bumped whenever inline-image state changes (a decode lands, `:set
    /// images` flips, a theme change flips `images`). Feeds `LayoutOptions`
    /// so a change forces exactly one relayout and no pre-decode cached
    /// layout is replayed with a stale image box (PRD FR-RD-8, FR-OFF-1).
    pub image_epoch: u64,
    /// Runtime override of the theme's `images` default (PRD FR-TH-7 `:set
    /// images=on|off`, and the config `images` key at startup). `None` means
    /// "follow the active theme".
    pub images_override: Option<bool>,
    /// PRD §10 licensing policy: whether non-free/fair-use images may be
    /// used. Default false. Today a policy flag with a documented seam — the
    /// saved-page image persistence that must honor it isn't built yet.
    pub include_nonfree: bool,
    /// The terminal graphics capability snapshot (`$TERM`/`$TERM_PROGRAM`/…),
    /// captured once at startup; `App::graphics_protocol` layers the live
    /// `no_color`/`images_enabled` decision on top (PRD FR-RD-8 §6.3).
    pub graphics_env: crate::graphics::GraphicsEnv,

    // -- Bookmarks, annotations, read-later (PRD §5.6) --------------------
    /// The saved-bookmarks store (PRD FR-BM-1/7), persisted to disk.
    pub bookmarks: BookmarkStore,
    /// The read-later queue (PRD FR-BM-3/7), persisted to disk.
    pub readlater: ReadLaterStore,
    /// The `r`-prefix chord's pending-key latch (`rl` read-later vs `r`'s
    /// own "open Research mode"). See `Mode::Reading`'s `pending_r` arm in
    /// `main.rs` for the documented precedence against the unrelated
    /// "background revalidation ready — `r` reloads" notice, which never
    /// engages this latch at all.
    pub pending_r: bool,
    /// Selection cursor into the *filtered* bookmark list (`App::
    /// visible_bookmarks`), not into `bookmarks.bookmarks` directly.
    pub selected_bookmark: usize,
    /// The mode `open_bookmark_picker` was entered from, restored on Esc.
    pub bookmark_prior_mode: Mode,
    /// The bookmark picker's live `/` filter input (PRD FR-BM-1's tag-
    /// expression + fuzzy-title grammar).
    pub bookmark_filter_input: String,
    /// The `t` inline tag-editor's in-progress input, seeded from the
    /// selected bookmark's current tags on entry.
    pub bookmark_tag_input: String,
    /// Selection cursor for the read-later queue picker.
    pub selected_readlater: usize,
    /// The mode `open_readlater_picker` was entered from, restored on Esc.
    pub readlater_prior_mode: Mode,
    /// PRD FR-BM-3's "(config)" `readlater_auto_dequeue`: whether opening a
    /// read-later entry removes it from the queue. Defaults to `true`;
    /// live-set from `config.toml`/`WIKITUI_READLATER_AUTO_DEQUEUE` at
    /// startup and on `:config reload` (see `config::resolve`).
    pub readlater_auto_dequeue: bool,
    /// Export-overwrite confirmation for `:bookmarks export`, mirroring
    /// `pending_export_overwrite` — the exact target path just warned
    /// about; running the same export again for that same path proceeds.
    pub pending_bookmark_export_overwrite: Option<std::path::PathBuf>,

    // -- Reading history (PRD §5.5, FR-HS-1/2/4) ---------------------------
    /// The persistent, SQLite-backed reading history (`history::History`).
    /// Defaults to an in-memory store (see `History::in_memory`'s doc
    /// comment) — `main::run` swaps in the real on-disk store right after
    /// construction, exactly like `readlater_auto_dequeue`/`config_ctx`
    /// below. This is deliberate, not an oversight: `set_document` (this
    /// module) records a visit on every successful document install, and
    /// dozens of existing tests build an `App` via `App::new` and call
    /// `set_document`/`open_document` directly — defaulting to an on-disk
    /// store would make `cargo test` write real rows into the developer's
    /// actual state directory on every run.
    pub history: crate::history::History,
    /// PRD FR-PR-3's incognito gate (that chunk is not built yet — this one
    /// only adds the flag and routes every write in this module through it,
    /// per the PRD's "architectural requirement, do early"): when `true`,
    /// `record_history_visit` and dwell tracking (`flush_tab_dwell`) become
    /// no-ops. The future incognito chunk just has to flip this — from
    /// `--incognito` (`cli::Cli::incognito`, wired in `main::run`) today,
    /// and presumably a runtime keybind once FR-PR-3 lands.
    pub incognito: bool,
    /// The reading-history picker's live list (PRD FR-HS-1), rebuilt by
    /// `refresh_history_matches` whenever `history_pick_filter` changes or
    /// the picker (re)opens: recency-weighted, optionally fuzzy-filtered.
    pub history_pick_matches: Vec<crate::history::Visit>,
    /// Selection cursor into `history_pick_matches`.
    pub history_pick_selected: usize,
    /// The reading-history picker's `/` filter input.
    pub history_pick_filter: String,
    /// The mode `open_reading_history_picker` was entered from, restored on
    /// Esc (mirrors `bookmark_prior_mode`).
    pub history_pick_prior_mode: Mode,
}

impl App {
    pub fn new(lang: String, theme: Theme, no_color: bool) -> Self {
        // Every session starts with exactly one (empty) tab; the invariant
        // "`tabs` is never empty while running" holds from here.
        let first_tab = Tab::new(0, lang.clone());
        Self {
            mode: Mode::Reading,
            prior_mode: Mode::Reading,
            tabs: vec![first_tab],
            active: 0,
            closed_tabs: Vec::new(),
            next_tab_id: 1,
            selected_tab_pick: 0,
            selected_history: 0,
            pending_b: false,
            pending_quit_confirm: false,
            status: "Press / to search, ? for help, q to quit".to_string(),
            search_input: String::new(),
            results: Vec::new(),
            selected_result: 0,
            typeahead: Vec::new(),
            selected_suggestion: 0,
            search_debounce_at: None,
            search_suggestion: None,
            should_quit: false,
            lang,
            loading: false,
            pending_g: false,
            theme,
            no_color,
            accessible: false,
            citations: Vec::new(),
            selected_citation: 0,
            research: ResearchStore::load(),
            selected_library: 0,
            cite_style: CiteStyle::Apa,
            library_prior_mode: Mode::Reading,
            pending_export_overwrite: None,
            command_input: String::new(),
            notice: None,
            layout: None,
            layout_cache: LayoutCache::new(layout::DEFAULT_L1_CAPACITY),
            layout_computations: 0,
            pending_revalidations: 0,
            layout_width: 80,
            viewport_height: 0,
            measure: 88,
            ambiguous_wide: false,
            hint_targets: Vec::new(),
            hint_input: String::new(),
            hint_background: false,
            config_ctx: ConfigContext::default(),
            image_store: crate::image::ImageStore::new(),
            image_epoch: 0,
            images_override: None,
            include_nonfree: false,
            graphics_env: crate::graphics::GraphicsEnv::default(),
            bookmarks: BookmarkStore::load(),
            readlater: ReadLaterStore::load(),
            pending_r: false,
            selected_bookmark: 0,
            bookmark_prior_mode: Mode::Reading,
            bookmark_filter_input: String::new(),
            bookmark_tag_input: String::new(),
            selected_readlater: 0,
            readlater_prior_mode: Mode::Reading,
            readlater_auto_dequeue: true,
            pending_bookmark_export_overwrite: None,
            history: crate::history::History::in_memory(),
            incognito: false,
            history_pick_matches: Vec::new(),
            history_pick_selected: 0,
            history_pick_filter: String::new(),
            history_pick_prior_mode: Mode::Reading,
        }
    }

    /// The tab currently on screen. `tabs` is never empty while the app runs
    /// (closing the last tab quits), so indexing is safe.
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Index of the tab with the given stable id, if it is still open (PRD
    /// FR-TB-3): a background completion whose tab has since closed resolves
    /// to `None` and is dropped gracefully.
    pub fn tab_index_by_id(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn allocate_tab_id(&mut self) -> TabId {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    /// PRD FR-TB-3: open `title` in a new, *unfocused* background tab and
    /// return its id so the caller can spawn the keyed fetch. Focus does not
    /// move; the tab shows its target title + "…" in the bar until the fetch
    /// lands (budget-aware prefetch scheduling arrives with a later chunk —
    /// for now the fetch fires immediately via the existing channel pattern).
    pub fn open_background_tab(&mut self, title: String, lang: String) -> TabId {
        let id = self.allocate_tab_id();
        let mut tab = Tab::new(id, lang);
        tab.loading = true;
        tab.pending_title = Some(title);
        self.tabs.push(tab);
        id
    }

    /// `:tab new` — create a fresh tab and switch to it. The caller fetches
    /// into the now-active tab via the normal foreground path (or leaves it
    /// empty on the welcome screen).
    pub fn new_foreground_tab(&mut self) {
        let id = self.allocate_tab_id();
        let lang = self.lang.clone();
        self.tabs.push(Tab::new(id, lang));
        self.active = self.tabs.len() - 1;
        self.sync_active_tab();
    }

    /// Close the tab at `index`, snapshotting it onto the close-undo stack
    /// and fixing the active index browser-style (focus moves to the tab that
    /// slides into place, or the new last tab). Returns `true` iff that was
    /// the last tab — the caller quits, since there is always ≥ 1 tab while
    /// running.
    pub fn close_tab(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return false;
        }
        // PRD FR-HS-1's dwell time stops accumulating the moment a tab
        // closes; flush before the tab (and its tracking fields) are gone.
        self.flush_tab_dwell(index);
        let tab = self.tabs.remove(index);
        self.push_closed(tab);
        if self.tabs.is_empty() {
            return true;
        }
        if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            self.active = self.active.min(self.tabs.len() - 1);
        }
        self.selected_tab_pick = self.selected_tab_pick.min(self.tabs.len() - 1);
        self.sync_active_tab();
        false
    }

    /// `q` / `:tab close` — close the active tab (quitting if it was the
    /// last). See [`Self::close_tab`].
    pub fn close_active_tab(&mut self) -> bool {
        self.close_tab(self.active)
    }

    fn push_closed(&mut self, tab: Tab) {
        self.closed_tabs.push(tab);
        // §6.8 memory note: cap the undo stack so closed tabs' `Document`s
        // actually drop — dropping the oldest snapshot is the deliberate
        // tradeoff for a bounded footprint (10 tabs < 150 MB).
        if self.closed_tabs.len() > CLOSED_TABS_CAP {
            self.closed_tabs.remove(0);
        }
    }

    /// `u` — reopen the most-recently-closed tab, restoring its whole
    /// snapshot (document, history stacks, scroll) and focusing it (PRD
    /// FR-TB-1). Returns `false` if there was nothing to reopen.
    pub fn reopen_closed_tab(&mut self) -> bool {
        match self.closed_tabs.pop() {
            Some(tab) => {
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
                self.sync_active_tab();
                true
            }
            None => false,
        }
    }

    /// `gt` — focus the next tab, wrapping (PRD FR-TB-1).
    pub fn next_tab(&mut self) {
        if self.tabs.len() < 2 {
            return;
        }
        self.active = (self.active + 1) % self.tabs.len();
        self.sync_active_tab();
    }

    /// `gT` — focus the previous tab, wrapping.
    pub fn prev_tab(&mut self) {
        if self.tabs.len() < 2 {
            return;
        }
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.sync_active_tab();
    }

    /// Tab-picker Enter — focus a tab by index.
    pub fn switch_to_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            self.sync_active_tab();
        }
    }

    /// After any change of which tab is active: adopt the tab's language for
    /// new searches/opens, rebuild the app-global citation list from its
    /// document, drop the cached layout so the next draw rebuilds for this
    /// tab (an L1 hit, not a relayout), re-arm the SWR "updated — r to
    /// reload" notice iff this tab has one pending, land in Reading mode, and
    /// refresh the status line.
    fn sync_active_tab(&mut self) {
        self.lang = self.active_tab().lang.clone();
        self.rebuild_citations();
        self.layout = None;
        self.pending_g = false;
        self.pending_b = false;
        self.mode = Mode::Reading;
        self.notice = self
            .active_tab()
            .pending_reload
            .as_ref()
            .map(|_| "updated — r to reload".to_string());
        self.refresh_reading_status();
    }

    /// Rebuild [`Self::citations`] from the active tab's document (or clear
    /// it when the tab is empty).
    fn rebuild_citations(&mut self) {
        let lang = self.lang.clone();
        let built = self.active_tab().doc.as_ref().map(|doc| {
            let mut c = vec![crate::research::self_citation(&doc.title, &lang)];
            c.extend(doc.citations.iter().cloned());
            c
        });
        self.citations = built.unwrap_or_default();
        self.selected_citation = 0;
    }

    /// Recompute the Reading-mode status line from the active tab.
    fn refresh_reading_status(&mut self) {
        let ncites = self.citations.len();
        let status = {
            let tab = self.active_tab();
            tab.doc.as_ref().map(|doc| {
                format!(
                    "{}{} — {} blocks, {} links, {} sections, {} citations",
                    tab.page_source.prefix(),
                    doc.title,
                    doc.blocks.len(),
                    tab.links.len(),
                    tab.sections.len(),
                    ncites
                )
            })
        };
        if let Some(status) = status {
            self.status = status;
        }
    }

    /// Whether any open tab has a background fetch in flight (PRD FR-TB-3):
    /// the event loop stays on its scoped-poll path while this holds, so a
    /// background tab's "…"→title transition happens without a keypress.
    pub fn any_tab_loading(&self) -> bool {
        self.tabs.iter().any(|t| t.loading)
    }

    /// The layout options derived from the current reading preferences.
    pub fn layout_options(&self) -> LayoutOptions {
        LayoutOptions {
            measure: self.measure,
            ambiguous_wide: self.ambiguous_wide,
            accessible: self.accessible,
            table_col_offset: self.active_tab().table_col_offset,
            image_epoch: self.image_epoch,
        }
    }

    /// PRD FR-RD-4's horizontal table scroll (`[`/`]` in the reading view):
    /// shift the shared column window of every wide table in the active
    /// article, clamped so it can't run past the widest table's columns.
    /// Changing it invalidates the cached layout (the offset is part of the
    /// L1 key), so the next draw relayouts with the new window.
    pub fn scroll_tables(&mut self, delta: i16) {
        let max = self.max_table_columns().saturating_sub(1) as u16;
        if max == 0 {
            self.status = "No wide table to scroll on this page".to_string();
            return;
        }
        let cur = self.active_tab().table_col_offset;
        let next = if delta < 0 {
            cur.saturating_sub((-delta) as u16)
        } else {
            cur.saturating_add(delta as u16)
        }
        .min(max);
        if next != cur {
            self.active_tab_mut().table_col_offset = next;
            self.layout = None;
        }
    }

    /// The greatest column count among the active article's tables — the
    /// bound for how far table scrolling can travel. Zero when the article
    /// has no tables.
    fn max_table_columns(&self) -> usize {
        self.active_tab()
            .doc
            .as_ref()
            .map(|doc| {
                doc.blocks
                    .iter()
                    .filter_map(|b| match b {
                        crate::doc::Block::Table(t) => Some(t.cols()),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// Ensure `self.layout` is current for `(layout_width, options)`. Three
    /// tiers, cheapest first: (1) nothing changed since the last draw at
    /// this width/options — reuse `self.layout` outright, never touching
    /// the L1 cache (so scrolling never even does a cache lookup); (2) the
    /// document/width/options changed but an L1 entry for that exact
    /// identity already exists (PRD FR-OFF-1) — reuse it, no relayout; (3)
    /// a genuine miss — lay out fresh and store it in L1 for next time. A
    /// no-op when no document is open.
    pub fn ensure_layout(&mut self) {
        let width = self.layout_width;
        let opts = self.layout_options();
        let stale = match &self.layout {
            Some(l) => l.width != width || l.options != opts,
            None => true,
        };
        if !stale {
            return;
        }
        // The L1 cache key is keyed on the *active tab's* article identity
        // (lang/title/revid), which the shared `layout_cache` disambiguates
        // across tabs — so a single cache serves every tab and a tab switch
        // is an L1 hit, not a relayout (§6.8).
        let key = {
            let tab = self.active_tab();
            tab.doc.as_ref().map(|doc| layout::LayoutCacheKey {
                lang: tab.lang.clone(),
                title: doc.title.clone(),
                revid: tab.current_revid,
                width,
                options: opts,
                schema_version: layout::LAYOUT_SCHEMA_VERSION,
            })
        };
        let Some(key) = key else {
            self.layout = None;
            return;
        };
        if let Some(cached) = self.layout_cache.get(&key) {
            self.layout = Some(cached);
            return;
        }
        // Reserve boxes for any decoded inline images (empty when images are
        // off / no graphics protocol / below the size tier). Computed before
        // the mutable borrows below so it can read the store immutably.
        let img_map = self.image_box_map();
        self.layout_computations += 1;
        let computed = {
            let doc = self
                .active_tab()
                .doc
                .as_ref()
                .expect("keyed above, so a document is present");
            layout::layout_document_with_images(doc, width, opts, &img_map)
        };
        self.layout_cache.put(key, computed.clone());
        self.layout = Some(computed);
    }

    /// PRD FR-TH-7 / FR-RD-8: whether inline images render right now — the
    /// runtime `:set images` (or config) override, else the active theme's
    /// default.
    pub fn images_enabled(&self) -> bool {
        self.images_override.unwrap_or(self.theme.images)
    }

    /// The graphics protocol wikitui would use for images right now (PRD
    /// FR-RD-8 §6.3): the startup terminal snapshot plus the live
    /// `no_color`/`images_enabled` decision. `None` means alt text.
    pub fn graphics_protocol(&self) -> crate::graphics::GraphicsProtocol {
        let env = crate::graphics::GraphicsEnv {
            no_color: self.no_color,
            images_enabled: self.images_enabled(),
            ..self.graphics_env.clone()
        };
        crate::graphics::detect_protocol(&env)
    }

    /// The reserved image boxes for the active document at the current width
    /// (PRD FR-RD-8). Empty when images are disabled, the terminal has no
    /// graphics protocol, or the content column is below [`layout::
    /// IMAGE_MIN_COLS`] — every image then falls back to its placeholder.
    fn image_box_map(&self) -> std::collections::HashMap<String, (u16, u16)> {
        let mut map = std::collections::HashMap::new();
        if matches!(
            self.graphics_protocol(),
            crate::graphics::GraphicsProtocol::None
        ) {
            return map;
        }
        let content_width = (self.layout_width as usize).min(self.measure as usize);
        let max_cols = content_width.min(layout::IMAGE_MAX_COLS as usize) as u16;
        if max_cols < layout::IMAGE_MIN_COLS {
            return map;
        }
        if let Some(doc) = self.active_tab().doc.as_ref() {
            for block in &doc.blocks {
                if let crate::doc::Block::Image { src: Some(src), .. } = block
                    && let Some(b) = self
                        .image_store
                        .box_for(src, max_cols, layout::IMAGE_MAX_ROWS)
                {
                    map.insert(src.clone(), b);
                }
            }
        }
        map
    }

    /// PRD FR-TH-7: flip inline-image rendering at runtime (`:set
    /// images=on|off`). Off falls straight back to alt-text placeholders; on
    /// makes the next frame lazily fetch. Either way one relayout is forced.
    pub fn set_images(&mut self, on: bool) {
        self.images_override = Some(on);
        self.note_image_state_change();
        self.status = format!("Images: {}", if on { "on" } else { "off" });
    }

    /// Bump the image epoch and drop the cached layout so the next
    /// `ensure_layout` rebuilds with the new inline-image state (PRD FR-RD-8):
    /// a decode landing, an images toggle, or a theme change that flips the
    /// theme's `images` default.
    pub fn note_image_state_change(&mut self) {
        self.image_epoch = self.image_epoch.wrapping_add(1);
        self.layout = None;
    }

    /// Switch the color theme, relayouting only if the change flips whether
    /// images render (PRD FR-TH-7): image boxes depend on the theme's
    /// `images` default, so a text↔image theme swap must rebuild the layout,
    /// while any other theme swap stays O(paint) as before.
    pub fn set_theme(&mut self, theme: Theme) {
        let before = self.images_enabled();
        self.theme = theme;
        if self.images_enabled() != before {
            self.note_image_state_change();
        }
    }

    /// The scroll offset that centers `line` in the viewport, clamped to the
    /// active tab's scrollable range. Before the first draw `viewport_height`
    /// is 0, which degrades gracefully to top-aligning the line.
    fn center_scroll(&self, line: u16) -> u16 {
        line.saturating_sub(self.viewport_height / 2)
            .min(self.active_tab().max_scroll)
    }

    pub fn cycle_theme(&mut self) {
        self.set_theme(self.theme.next());
        self.status = format!("Theme: {}", self.theme.name);
    }

    /// Install a decoded (or failed) inline image delivered by the async
    /// loader (PRD FR-RD-8). A successful decode makes the box appear on the
    /// next relayout; a failure keeps the placeholder. Either way the image
    /// state changed, so the layout is invalidated.
    pub fn deliver_image(&mut self, src: String, decoded: Option<crate::image::DecodedImage>) {
        match decoded {
            Some(img) => self.image_store.set_ready(src, img),
            None => self.image_store.set_failed(src),
        }
        self.note_image_state_change();
    }

    /// The active tab's canonical article URL — the `y` yank payload
    /// (FR-NV-10).
    pub fn yank_url(&self) -> Option<String> {
        let tab = self.active_tab();
        tab.doc
            .as_ref()
            .map(|d| crate::research::article_url(&d.title, &tab.lang))
    }

    /// A Markdown link to the active tab's article — the `Y` yank payload;
    /// terminal users paste these into notes constantly (PRD FR-NV-10).
    pub fn yank_markdown(&self) -> Option<String> {
        let tab = self.active_tab();
        tab.doc.as_ref().map(|d| {
            format!(
                "[{}]({})",
                d.title,
                crate::research::article_url(&d.title, &tab.lang)
            )
        })
    }

    /// Open a document reached by a fresh navigation (search result, CLI
    /// title, or following a link) *in the active tab*: the article it was
    /// showing, if any, becomes its back-stack top (with its scroll captured
    /// for restoration), and its forward history is discarded — standard
    /// browser semantics, now per-tab (PRD FR-TB-2 v1.0).
    pub fn open_document(&mut self, doc: Document) {
        if let Some(entry) = self.active_tab().current_entry() {
            self.active_tab_mut().back_stack.push(entry);
        }
        self.active_tab_mut().forward_stack.clear();
        self.set_document(doc);
    }

    /// Returns the history entry to fetch for "go back" — `(lang, title,
    /// scroll)` — already adjusting the active tab's back/forward stacks. The
    /// caller fetches it and finishes with `set_document` (which must NOT
    /// touch the stacks again) then restores `entry.scroll`.
    pub fn navigate_back_target(&mut self) -> Option<HistoryEntry> {
        let target = self.active_tab_mut().back_stack.pop()?;
        if let Some(entry) = self.active_tab().current_entry() {
            self.active_tab_mut().forward_stack.push(entry);
        }
        Some(target)
    }

    /// The forward-history counterpart of `navigate_back_target`.
    pub fn navigate_forward_target(&mut self) -> Option<HistoryEntry> {
        let target = self.active_tab_mut().forward_stack.pop()?;
        if let Some(entry) = self.active_tab().current_entry() {
            self.active_tab_mut().back_stack.push(entry);
        }
        Some(target)
    }

    /// Jump the active tab's history straight to `back_stack[index]`
    /// (browser-style, PRD FR-NV-7's `gb`): the current page and every
    /// back-stack entry newer than the target move onto the forward stack —
    /// newest first — so Forward walks back the way you came. Returns the
    /// entry to fetch, or `None` if the index is out of range.
    pub fn jump_to_back_entry(&mut self, index: usize) -> Option<HistoryEntry> {
        let current = self.active_tab().current_entry();
        let tab = self.active_tab_mut();
        if index >= tab.back_stack.len() {
            return None;
        }
        if let Some(cur) = current {
            tab.forward_stack.push(cur);
        }
        while tab.back_stack.len() > index + 1 {
            if let Some(entry) = tab.back_stack.pop() {
                tab.forward_stack.push(entry);
            }
        }
        tab.back_stack.pop()
    }

    /// Install a document into the active tab without touching its
    /// back/forward stacks (used after `navigate_*`/`jump_to_back_entry`,
    /// which already adjusted them). The caller sets the tab's
    /// `page_source`/`current_revid` first; this installs the document,
    /// rebuilds the app-global citation list, refreshes the status line,
    /// and — PRD FR-HS-1 — is the one place a reading-history visit gets
    /// recorded: this is "an article successfully renders in a tab," the
    /// installation point every navigation path (`open_document`, back/
    /// forward, bookmark/read-later/history-picker reopen, the SWR "r to
    /// reload") funnels through.
    pub fn set_document(&mut self, doc: Document) {
        let lang = self.lang.clone();
        let index = self.active;

        // Whatever the active tab was showing before this moment stops
        // accumulating dwell time now (PRD FR-HS-1) — must happen before
        // `install_document` clears the tracking fields below.
        self.flush_tab_dwell(index);
        // The referrer (PRD FR-HS-1's "referrer article") is the top of the
        // back stack at this exact moment: for a fresh navigation
        // (`open_document`) that is precisely the article just pushed off
        // screen by this move; for back/forward navigation it reflects an
        // earlier point in the tab's own trail rather than "pressed back" —
        // a documented simplification, not a distinct code path per
        // navigation kind.
        let referrer = self
            .active_tab()
            .back_stack
            .last()
            .map(|e| (e.lang.clone(), e.title.clone()));

        // Element 0 is always this article's own citation; the rest are
        // whatever it cites (Research mode, PRD-adjacent feature request).
        let mut citations = vec![crate::research::self_citation(&doc.title, &lang)];
        citations.extend(doc.citations.iter().cloned());
        self.citations = citations;
        self.selected_citation = 0;

        {
            let tab = self.active_tab_mut();
            tab.lang = lang;
            tab.install_document(doc);
        }
        self.record_history_visit(index, referrer);
        self.mode = Mode::Reading;
        // A new document invalidates the cached layout; it is rebuilt lazily
        // (from L1 if available, else a fresh layout pass) on the next draw
        // or mapping lookup at the current width — see `ensure_layout`.
        self.layout = None;
        self.refresh_reading_status();
    }

    // -- Reading history (PRD FR-HS-1/2/4) ---------------------------------

    /// Records a visit to whatever document is now installed at
    /// `tabs[index]` (PRD FR-HS-1), unless `self.incognito` — the one gate
    /// every write in this module routes through (PRD FR-PR-3, not built
    /// yet). Starts that tab's dwell clock running from now. Called by
    /// `set_document` for the active tab and by `main::apply_tab_load_outcome`
    /// for a background tab's fetch completion — the only two places a
    /// document is ever installed. A no-op if the tab index is gone or has
    /// no document (nothing to record).
    pub(crate) fn record_history_visit(
        &mut self,
        index: usize,
        referrer: Option<(String, String)>,
    ) {
        if self.incognito {
            return;
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(title) = tab.doc.as_ref().map(|d| d.title.clone()) else {
            return;
        };
        let lang = tab.lang.clone();
        let referrer_ref = referrer.as_ref().map(|(l, t)| (l.as_str(), t.as_str()));
        let id = self.history.record_visit(&lang, &title, referrer_ref);
        let tab = &mut self.tabs[index];
        tab.history_visit_id = id;
        tab.visit_started_at = Some(std::time::Instant::now());
    }

    /// Flushes accumulated dwell time for `tabs[index]`'s current visit
    /// (PRD FR-HS-1): called right before that tab's document is replaced
    /// (`set_document`), the tab closes (`close_tab`), or the app exits
    /// (`flush_all_tab_dwell`). A no-op if nothing is being tracked there
    /// (no document, incognito — in which case `history_visit_id` was
    /// never set — or a write already failed and produced no id).
    fn flush_tab_dwell(&mut self, index: usize) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let id = tab.history_visit_id.take();
        let started = tab.visit_started_at.take();
        if let (Some(id), Some(started)) = (id, started) {
            self.history.update_dwell(id, started.elapsed().as_secs());
        }
    }

    /// Flushes dwell time for every open tab (PRD FR-HS-1): called once,
    /// right before the app exits — "the tab closes or the app exits" from
    /// the requirement's dwell-tracking wording. Closing an individual tab
    /// mid-session goes through `close_tab`, which flushes just that one.
    pub fn flush_all_tab_dwell(&mut self) {
        for index in 0..self.tabs.len() {
            self.flush_tab_dwell(index);
        }
    }

    /// `Ctrl-h` / `:history` (PRD FR-HS-1): opens the persistent
    /// reading-history picker over the most recent visit to each article,
    /// most-recent-first.
    pub fn open_reading_history_picker(&mut self) {
        self.history_pick_prior_mode = self.mode;
        self.mode = Mode::ReadingHistory;
        self.history_pick_filter.clear();
        self.refresh_history_matches();
        self.status = "j/k: move   /: filter   d: delete   Enter: open   Esc: close".to_string();
    }

    pub fn close_reading_history_picker(&mut self) {
        self.mode = self.history_pick_prior_mode;
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    /// How many rows the picker asks for — generous enough that "recent
    /// history" and "fuzzy search over history" both feel unbounded in
    /// practice without ever loading the entire table into the picker.
    const HISTORY_PICKER_LIMIT: usize = 200;

    /// Rebuilds `history_pick_matches` from `history_pick_filter` (PRD
    /// FR-HS-1): an empty filter is plain recency (`History::recent`); a
    /// non-empty one is `History::search`'s fuzzy, recency-weighted
    /// ranking (see that method's doc comment for exactly how the two
    /// combine). Resets the selection — a narrower or wider list makes the
    /// old cursor position meaningless.
    pub fn refresh_history_matches(&mut self) {
        self.history_pick_matches = if self.history_pick_filter.trim().is_empty() {
            self.history.recent(Self::HISTORY_PICKER_LIMIT)
        } else {
            self.history
                .search(&self.history_pick_filter, Self::HISTORY_PICKER_LIMIT)
        };
        self.history_pick_selected = 0;
    }

    /// Moves the picker's selection, wrapping — a no-op with nothing shown.
    pub fn cycle_history_pick(&mut self, forward: bool) {
        let len = self.history_pick_matches.len();
        if len == 0 {
            return;
        }
        self.history_pick_selected = if forward {
            (self.history_pick_selected + 1) % len
        } else {
            (self.history_pick_selected + len - 1) % len
        };
    }

    /// `d` in the picker (PRD FR-HS-4): deletes every visit to the selected
    /// article, then refreshes the list so it disappears immediately.
    pub fn delete_selected_history(&mut self) {
        let Some(visit) = self
            .history_pick_matches
            .get(self.history_pick_selected)
            .cloned()
        else {
            return;
        };
        self.status = match self.history.clear(crate::history::ClearRange::Article {
            lang: visit.lang.clone(),
            title: visit.title.clone(),
        }) {
            Ok(_) => format!("Removed \"{}\" from history", visit.title),
            Err(e) => format!("Removed from this session, but the database update failed: {e}"),
        };
        self.refresh_history_matches();
    }

    /// `:history clear today|all` (PRD FR-HS-4). "today" clears visits
    /// opened since *local* midnight (`history::today_start_unix`) — the
    /// reader's own calendar day, not UTC's (same reasoning as `research::
    /// today`). Refreshes the picker's list too, in case it's open.
    pub fn clear_history(&mut self, scope: crate::command::HistoryClearScope) {
        let range = match scope {
            crate::command::HistoryClearScope::All => crate::history::ClearRange::All,
            crate::command::HistoryClearScope::Today => {
                crate::history::ClearRange::Since(crate::history::today_start_unix())
            }
        };
        self.notice = Some(match self.history.clear(range) {
            Ok(n) => format!(
                "Cleared {n} history entr{}",
                if n == 1 { "y" } else { "ies" }
            ),
            Err(e) => format!("Clearing history failed: {e}"),
        });
        if self.mode == Mode::ReadingHistory {
            self.refresh_history_matches();
        }
    }

    /// Swaps in the content a background revalidation already wrote to L2
    /// (PRD FR-OFF-2's "r to reload"), without a second network round trip
    /// — the whole point of writing the fetched HTML to L2 *before* ever
    /// showing the notice. A no-op if there's nothing pending, or if the L2
    /// entry has since been evicted (only reachable with a very tight cache
    /// cap) — silently doing nothing beats a confusing partial reload.
    /// `page_source` becomes `Live`: the content just came off the network
    /// via the revalidation's own fetch, so that's the honest status, even
    /// though this exact keypress made no request of its own.
    pub fn reload_from_pending_update(&mut self, cache: &PageCache) {
        let Some(pending) = self.active_tab_mut().pending_reload.take() else {
            return;
        };
        self.notice = None;
        if let Some(page) = cache.get(&pending.lang, &pending.title) {
            let document = crate::doc::parse_article_html(&pending.title, &page.html);
            {
                let tab = self.active_tab_mut();
                tab.current_revid = page.revid;
                tab.page_source = PageSource::Live;
            }
            self.set_document(document);
        }
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
            .active_tab()
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
            source_lang: self.active_tab().lang.clone(),
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
        self.status = match &self.active_tab().doc {
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

    /// Clears the active tab's in-page find state — a fresh article's matches
    /// would be meaningless leftovers from whatever was open before.
    pub fn clear_find(&mut self) {
        self.active_tab_mut().clear_find();
    }

    /// Recomputes every occurrence of the active tab's `find_input` in its
    /// document and jumps to the first hit, if any (PRD FR-NV-6). Char-level,
    /// via `layout::find_matches` on the cached layout — smart-case and
    /// highlight-all fall out of that function, one `Occurrence` per hit
    /// (never split back into one entry per line-piece: that would
    /// double-count a match that straddles a wrap). `find_matches` mirrors
    /// each occurrence's first piece's line, for the scroll/counter code
    /// that only ever needed a line to jump to.
    pub fn update_find(&mut self) {
        self.ensure_layout();
        let occurrences = match &self.layout {
            Some(layout) => {
                let input = self.active_tab().find_input.clone();
                layout::find_matches(&layout.lines, &layout.continuation, &input)
            }
            None => Vec::new(),
        };
        let matches: Vec<u16> = occurrences
            .iter()
            .filter_map(|occ| occ.pieces.first().map(|&(line, _)| line as u16))
            .collect();
        let first = matches.first().copied();
        {
            let tab = self.active_tab_mut();
            tab.find_occurrences = occurrences;
            tab.find_matches = matches;
            tab.find_index = 0;
        }
        if let Some(line) = first {
            let scroll = self.center_scroll(line);
            self.active_tab_mut().scroll = scroll;
        }
    }

    pub fn find_next(&mut self) {
        let (len, index) = {
            let tab = self.active_tab();
            (tab.find_matches.len(), tab.find_index)
        };
        if len == 0 {
            return;
        }
        let new_index = (index + 1) % len;
        let line = self.active_tab().find_matches[new_index];
        let scroll = self.center_scroll(line);
        let tab = self.active_tab_mut();
        tab.find_index = new_index;
        tab.scroll = scroll;
    }

    pub fn find_prev(&mut self) {
        let (len, index) = {
            let tab = self.active_tab();
            (tab.find_matches.len(), tab.find_index)
        };
        if len == 0 {
            return;
        }
        let new_index = (index + len - 1) % len;
        let line = self.active_tab().find_matches[new_index];
        let scroll = self.center_scroll(line);
        let tab = self.active_tab_mut();
        tab.find_index = new_index;
        tab.scroll = scroll;
    }

    /// Scroll the active tab to the given section's heading line, clamped to
    /// what's actually scrollable (a section near the end of a short article
    /// may not have `max_scroll` lines below it). The heading's line is
    /// resolved from the layout's block→line map.
    pub fn jump_to_section(&mut self, index: usize) {
        self.ensure_layout();
        let line = {
            let tab = self.active_tab();
            tab.sections.get(index).and_then(|section| {
                self.layout
                    .as_ref()
                    .and_then(|l| l.block_lines.get(section.block).copied())
            })
        };
        if let Some(line) = line {
            let max = self.active_tab().max_scroll;
            self.active_tab_mut().scroll = (line as u16).min(max);
        }
        self.mode = Mode::Reading;
    }

    pub fn cycle_link(&mut self, forward: bool) {
        let len = self.active_tab().links.len();
        if len == 0 {
            self.status = "No links on this page".to_string();
            return;
        }
        let next = Some(match self.active_tab().focused_link {
            None => 0,
            Some(i) if forward => (i + 1) % len,
            Some(i) => (i + len - 1) % len,
        });
        self.active_tab_mut().focused_link = next;
        self.scroll_focused_link_into_view();
    }

    /// Scroll so the active tab's focused link's first line is visible, if it
    /// isn't already — links cycled past the bottom of a long page would
    /// otherwise be highlighted off-screen. A no-op before the first draw
    /// (no viewport height) or when the layout has no line for the link.
    fn scroll_focused_link_into_view(&mut self) {
        if self.viewport_height == 0 {
            return;
        }
        let Some(occ) = self.active_tab().focused_link else {
            return;
        };
        self.ensure_layout();
        let Some(line) = self
            .layout
            .as_ref()
            .and_then(|l| l.link_lines.get(occ).copied())
        else {
            return;
        };
        let line = line as u16;
        let (scroll, max) = {
            let tab = self.active_tab();
            (tab.scroll, tab.max_scroll)
        };
        let vh = self.viewport_height;
        let bottom = scroll.saturating_add(vh);
        let new = if line < scroll {
            line.min(max)
        } else if line >= bottom {
            line.saturating_sub(vh.saturating_sub(1)).min(max)
        } else {
            scroll
        };
        self.active_tab_mut().scroll = new;
    }

    // -- Link hints (PRD FR-NV-1) --------------------------------------

    /// Enters hint mode: every link visible in the current viewport gets a
    /// home-row label (`hints::visible_link_hints`) painted over its first
    /// cells. `background` distinguishes `f` (follow in this tab) from `F`
    /// (open in a background tab, FR-TB-3 integration — vimium semantics:
    /// `F` follows once and exits hint mode immediately; looping to open
    /// several without leaving hint mode is a v1.x nicety, not this one). A
    /// no-op (with a status message, not a mode change) when nothing is
    /// visible to hint.
    pub fn enter_hint_mode(&mut self, background: bool) {
        self.hint_background = background;
        self.hint_input.clear();
        self.mode = Mode::Hint;
        self.ensure_layout();
        self.refresh_hint_targets();
        if self.hint_targets.is_empty() {
            self.mode = Mode::Reading;
            self.status = "No links visible to hint".to_string();
        }
    }

    /// Leaves hint mode back to Reading, discarding the transient hint
    /// state. Called on Esc and once a hint resolves — hint mode is only
    /// ever entered from Reading, so there is no "prior mode" to restore
    /// (unlike Help/Library).
    pub fn exit_hint_mode(&mut self) {
        self.mode = Mode::Reading;
        self.hint_targets.clear();
        self.hint_input.clear();
    }

    /// Recomputes the visible hint set from the CURRENT layout, scroll, and
    /// viewport height (PRD FR-NV-1: "hints survive reflow"). Called once on
    /// entry and again on every draw while `Mode::Hint` is active
    /// (`ui::draw_reading`) — deliberately not just once — so a resize
    /// mid-hint-mode re-labels from the layout just rebuilt for the new
    /// width instead of replaying line/column positions computed for the
    /// old one. Hint assignment has no state of its own beyond what
    /// `(layout, scroll, viewport_height)` already determines, so recomputing
    /// unconditionally is simpler than tracking "did anything actually
    /// change" and is cheap enough to not matter (a handful of links, no
    /// relayout). A no-op outside Hint mode.
    pub fn refresh_hint_targets(&mut self) {
        if self.mode != Mode::Hint {
            return;
        }
        self.hint_targets = match &self.layout {
            Some(layout) => {
                hints::visible_link_hints(layout, self.active_tab().scroll, self.viewport_height)
            }
            None => Vec::new(),
        };
        // A resize can change which links are visible entirely. If the
        // already-typed prefix no longer matches any label in the new set,
        // drop it rather than leaving the reader stuck typing into a prefix
        // that can never resolve again.
        if !self
            .hint_targets
            .iter()
            .any(|t| t.label.starts_with(&self.hint_input))
        {
            self.hint_input.clear();
        }
    }

    /// Types one more character into the hint-mode prefix (PRD FR-NV-1): a
    /// keystroke that leaves at least one hint label still matching is
    /// committed, narrowing the visible set; one that would eliminate every
    /// remaining hint is silently ignored (the prefix stays what it was) —
    /// deliberately not exiting hint mode outright (the brief's other
    /// documented option), so a single mistyped key doesn't throw the reader
    /// back to Reading mid-hint. Esc and backspace remain the explicit ways
    /// out/back.
    pub fn narrow_hint_input(&mut self, c: char) -> hints::HintOutcome {
        let mut candidate = self.hint_input.clone();
        candidate.push(c);
        let outcome = hints::resolve(&self.hint_targets, &candidate);
        if !matches!(outcome, hints::HintOutcome::Ignored) {
            self.hint_input = candidate;
        }
        outcome
    }

    /// What following the resolved hint at `link_idx` should do (PRD FR-NV-1
    /// `f`/`F`), decided purely from App state — the link's `internal_title`
    /// and whether hint mode was entered via `F` (`hint_background`) — so
    /// it's testable without touching the network. `handle_key` is the only
    /// caller that turns this into an actual fetch (`Foreground` via
    /// `open_title`, `Background` via `open_background_tab`/
    /// `fire_background_load`, mirroring the existing Ctrl-Enter path).
    pub fn resolve_hint_action(&self, link_idx: usize) -> Option<HintFollowAction> {
        let link = self.active_tab().links.get(link_idx)?;
        Some(match &link.internal_title {
            Some(title) if self.hint_background => HintFollowAction::Background(title.clone()),
            Some(title) => HintFollowAction::Foreground(title.clone()),
            None => HintFollowAction::External(link.href.clone()),
        })
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let tab = self.active_tab_mut();
        let new = (tab.scroll as i32 + delta).clamp(0, tab.max_scroll as i32);
        tab.scroll = new as u16;
    }

    pub fn scroll_to_top(&mut self) {
        self.active_tab_mut().scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        let max = self.active_tab().max_scroll;
        self.active_tab_mut().scroll = max;
    }

    /// Build the active tab's breadcrumb trail (PRD FR-NV-7): the last few
    /// back-stack titles plus the current article title, in navigation order.
    /// The status bar renders and middle-truncates it. Empty when the tab has
    /// no document.
    pub fn breadcrumb_titles(&self) -> Vec<String> {
        let tab = self.active_tab();
        let Some(doc) = tab.doc.as_ref() else {
            return Vec::new();
        };
        let mut trail: Vec<String> = tab
            .back_stack
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|e| e.title.clone())
            .collect();
        trail.push(doc.title.clone());
        trail
    }

    /// Arms (or disarms) the typeahead debounce timer on every Search-mode
    /// keystroke (PRD FR-SR-1): an empty query cancels outright — the
    /// dropdown has nothing to complete and shouldn't hold stale
    /// suggestions from a moment ago.
    pub fn queue_typeahead(&mut self) {
        if self.search_input.trim().is_empty() {
            self.search_debounce_at = None;
            self.typeahead.clear();
        } else {
            self.search_debounce_at = Some(Instant::now() + TYPEAHEAD_DEBOUNCE);
        }
    }

    /// Moves the highlighted typeahead suggestion, wrapping like every other
    /// selectable list in the app. A no-op with nothing to select.
    pub fn move_suggestion(&mut self, forward: bool) {
        if self.typeahead.is_empty() {
            return;
        }
        let len = self.typeahead.len();
        self.selected_suggestion = if forward {
            (self.selected_suggestion + 1) % len
        } else {
            (self.selected_suggestion + len - 1) % len
        };
    }

    // -- Bookmarks (PRD FR-BM-1, FR-BM-7) ----------------------------------

    /// `m`: bookmarks the active tab's article, or un-bookmarks it if it
    /// already was (the toggle idiom — see `bookmarks::BookmarkStore::
    /// toggle`'s doc comment). Sets `notice` either way so the reader always
    /// gets feedback, not just on the add half.
    pub fn toggle_bookmark(&mut self) {
        let Some(doc) = self.active_tab().doc.as_ref() else {
            self.status = "Open an article first".to_string();
            return;
        };
        let title = doc.title.clone();
        let lang = self.active_tab().lang.clone();
        let revid = self.active_tab().current_revid;
        let revid = (revid != 0).then_some(revid);

        match self.bookmarks.toggle(&lang, &title, revid) {
            ToggleOutcome::Added => self.notice = Some(format!("Bookmarked \"{title}\"")),
            ToggleOutcome::Removed => {
                self.notice = Some(format!("Removed bookmark for \"{title}\""))
            }
        }
    }

    /// The bookmark picker's live list: indices into `self.bookmarks.
    /// bookmarks` that satisfy the current `/` filter, in store order —
    /// `selected_bookmark` indexes into *this*, not the underlying store
    /// directly, so narrowing the filter never leaves the cursor pointing
    /// at a bookmark that's no longer shown.
    pub fn visible_bookmarks(&self) -> Vec<usize> {
        let filter = bookmarks::parse_filter(&self.bookmark_filter_input);
        self.bookmarks
            .bookmarks
            .iter()
            .enumerate()
            .filter(|(_, b)| bookmarks::matches_filter(b, &filter))
            .map(|(i, _)| i)
            .collect()
    }

    /// The real store index the picker's current selection refers to, or
    /// `None` when the filtered list is empty.
    pub fn selected_bookmark_index(&self) -> Option<usize> {
        self.visible_bookmarks()
            .get(self.selected_bookmark)
            .copied()
    }

    pub fn selected_bookmark_entry(&self) -> Option<&Bookmark> {
        self.selected_bookmark_index()
            .and_then(|i| self.bookmarks.bookmarks.get(i))
    }

    /// `B` / `:bookmarks`: opens the picker over the whole bookmark store.
    pub fn open_bookmark_picker(&mut self) {
        self.bookmark_prior_mode = self.mode;
        self.mode = Mode::BookmarkPicker;
        self.bookmark_filter_input.clear();
        self.selected_bookmark = 0;
        self.status =
            "j/k: move   /: filter   t: tags   d: delete   Enter: open   Esc: close".to_string();
    }

    pub fn close_bookmark_picker(&mut self) {
        self.mode = self.bookmark_prior_mode;
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    /// Moves the picker's selection, wrapping over the *filtered* list —
    /// a no-op with nothing visible.
    pub fn cycle_bookmark(&mut self, forward: bool) {
        let len = self.visible_bookmarks().len();
        if len == 0 {
            self.selected_bookmark = 0;
            return;
        }
        self.selected_bookmark = if forward {
            (self.selected_bookmark + 1) % len
        } else {
            (self.selected_bookmark + len - 1) % len
        };
    }

    /// `d` in the picker: deletes the selected bookmark, keeping the cursor
    /// on a valid index into whatever the filter still shows afterwards.
    pub fn delete_selected_bookmark(&mut self) {
        let Some(index) = self.selected_bookmark_index() else {
            return;
        };
        if let Some((removed, persisted)) = self.bookmarks.remove(index) {
            let visible_len = self.visible_bookmarks().len();
            if visible_len == 0 {
                self.selected_bookmark = 0;
            } else {
                self.selected_bookmark = self.selected_bookmark.min(visible_len - 1);
            }
            self.status = match persisted {
                Ok(()) => format!("Removed bookmark for \"{}\"", removed.title),
                Err(e) => format!(
                    "Removed \"{}\" from this session, but updating the file failed: {e}",
                    removed.title
                ),
            };
        }
    }

    /// `t` in the picker: opens the inline tag editor, seeded with the
    /// selected bookmark's current tags (comma-separated, so re-committing
    /// unchanged input is a no-op edit).
    pub fn begin_tag_edit(&mut self) {
        let Some(bookmark) = self.selected_bookmark_entry() else {
            self.status = "No bookmark selected".to_string();
            return;
        };
        self.bookmark_tag_input = bookmark.tags.join(", ");
        self.mode = Mode::BookmarkTagEdit;
    }

    /// Enter in the tag editor: replaces the selected bookmark's tag set
    /// with `bookmarks::parse_tags(&self.bookmark_tag_input)` and returns
    /// to the picker.
    pub fn commit_tag_edit(&mut self) {
        if let Some(bookmark) = self.selected_bookmark_entry() {
            let lang = bookmark.lang.clone();
            let title = bookmark.title.clone();
            let tags = bookmarks::parse_tags(&self.bookmark_tag_input);
            self.bookmarks.set_tags(&lang, &title, tags);
        }
        self.mode = Mode::BookmarkPicker;
    }

    // -- Read-later queue (PRD FR-BM-3, FR-BM-7) ---------------------------

    /// `rl`'s target (PRD FR-BM-3 "enqueues from link or article"): the
    /// focused link's internal target if one is focused and followable,
    /// else the article on screen; `None` with nothing open at all. A
    /// focused *external* link has no internal target to cache offline, so
    /// it falls back to the article too, same as Enter's own "not yet
    /// followable" treatment of external links elsewhere.
    pub fn read_later_target(&self) -> Option<(String, String)> {
        let tab = self.active_tab();
        if let Some(link) = tab.focused_link.and_then(|i| tab.links.get(i))
            && let Some(title) = &link.internal_title
        {
            return Some((tab.lang.clone(), title.clone()));
        }
        tab.doc
            .as_ref()
            .map(|d| (tab.lang.clone(), d.title.clone()))
    }

    /// The read-later queue's live list, oldest first (append order is
    /// FIFO order — see `ReadLaterStore::enqueue`'s doc comment).
    pub fn open_readlater_picker(&mut self) {
        self.readlater_prior_mode = self.mode;
        self.mode = Mode::ReadLaterPicker;
        self.selected_readlater = self
            .selected_readlater
            .min(self.readlater.entries.len().saturating_sub(1));
        self.status = "Enter: open (auto-dequeues)   d: remove   Esc: close".to_string();
    }

    pub fn close_readlater_picker(&mut self) {
        self.mode = self.readlater_prior_mode;
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    pub fn cycle_readlater(&mut self, forward: bool) {
        let len = self.readlater.entries.len();
        if len == 0 {
            return;
        }
        self.selected_readlater = if forward {
            (self.selected_readlater + 1) % len
        } else {
            (self.selected_readlater + len - 1) % len
        };
    }

    /// `d` in the read-later picker: removes the selected entry without
    /// opening it.
    pub fn remove_selected_readlater(&mut self) {
        if let Some((removed, persisted)) = self.readlater.remove(self.selected_readlater) {
            let len = self.readlater.entries.len();
            self.selected_readlater = if len == 0 {
                0
            } else {
                self.selected_readlater.min(len - 1)
            };
            self.status = match persisted {
                Ok(()) => format!("Removed \"{}\" from the read-later queue", removed.title),
                Err(e) => format!(
                    "Removed \"{}\" from this session, but updating the file failed: {e}",
                    removed.title
                ),
            };
        }
    }

    /// Enter in the read-later picker, PRD FR-BM-3's "opening auto-dequeues
    /// (config)": returns the entry to open, and — when
    /// `readlater_auto_dequeue` is on — removes it from the queue first
    /// (before the caller's own fetch, so a fetch failure doesn't leave a
    /// half-dequeued entry: it's just gone, matching "opening" being the
    /// action that consumed it, not "opening successfully").
    pub fn take_selected_readlater(&mut self) -> Option<ReadLaterEntry> {
        let index = self.selected_readlater;
        if self.readlater_auto_dequeue {
            self.readlater.remove(index).map(|(entry, _)| entry)
        } else {
            self.readlater.entries.get(index).cloned()
        }
    }

    // -- Bookmark export (PRD FR-BM-4) -------------------------------------

    /// `:bookmarks export <format> [path]`: resolves the default timestamped
    /// path under the data dir's `exports/` (§6.4) when `path` is `None`.
    pub fn export_bookmarks(&mut self, format: &str, path: Option<&std::path::Path>) {
        if self.bookmarks.bookmarks.is_empty() {
            self.notice = Some("Nothing to export — no bookmarks saved yet".to_string());
            return;
        }
        let target = match path {
            Some(p) => p.to_path_buf(),
            None => match crate::bookmark_export::default_export_path(format) {
                Some(p) => p,
                None => {
                    self.notice = Some(format!(
                        "unknown export format {format:?} — one of: {}",
                        crate::bookmark_export::FORMATS.join(", ")
                    ));
                    return;
                }
            },
        };
        self.export_bookmarks_to(format, target);
    }

    /// The testable core of `export_bookmarks`: same behavior, explicit
    /// target path. Overwriting an existing export requires a second
    /// confirming run of the same command (mirrors `export_bibliography_to`).
    pub fn export_bookmarks_to(&mut self, format: &str, target: std::path::PathBuf) {
        let Some(content) = crate::bookmark_export::render(&self.bookmarks.bookmarks, format)
        else {
            self.notice = Some(format!(
                "unknown export format {format:?} — one of: {}",
                crate::bookmark_export::FORMATS.join(", ")
            ));
            return;
        };
        if target.exists()
            && self.pending_bookmark_export_overwrite.as_deref() != Some(target.as_path())
        {
            self.pending_bookmark_export_overwrite = Some(target.clone());
            self.notice = Some(format!(
                "{} already exists — run the export again to overwrite",
                target.display()
            ));
            return;
        }
        self.pending_bookmark_export_overwrite = None;
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        self.notice = Some(match std::fs::write(&target, content) {
            Ok(()) => format!(
                "Exported {} bookmarks to {}",
                self.bookmarks.bookmarks.len(),
                target.display()
            ),
            Err(e) => format!("Export failed: {e}"),
        });
    }
}

/// Pure debounce-elapsed check (PRD FR-SR-1), isolated from the real clock
/// and the channel/async plumbing around it so the decision itself is
/// trivially unit-testable: has `now` reached the `deadline` a keystroke
/// armed?
pub fn debounce_due(deadline: Instant, now: Instant) -> bool {
    now >= deadline
}

/// PRD FR-SR-1's in-flight-cancellation rule, as a pure decision: a
/// typeahead response is applied only if the query it answers is still the
/// one in the box. A response for a query the user has since typed past is
/// stale and must be silently dropped — this is the whole discard decision,
/// deliberately kept free of the channel/spawn machinery that delivers the
/// response so it can be tested in isolation from timing and async.
pub fn typeahead_is_current(response_query: &str, live_query: &str) -> bool {
    response_query == live_query
}

/// PRD FR-SR-4 / §7's "Search: zero results" copy: the did-you-mean
/// suggestion when the server offered one, with the exact "(Enter to
/// search)" affordance §7 specifies; a plain "no results" line otherwise.
pub fn zero_results_message(query: &str, suggestion: Option<&str>) -> String {
    match suggestion {
        Some(s) => format!("No results for \"{query}\". Did you mean {s}? (Enter to search)"),
        None => format!("No results for \"{query}\""),
    }
}

/// What the `b`-prefix chord's second key means (PRD FR-TB-1's `bb`, FR-BM-2's
/// `ba`) — a pure decision, like `resolve_hint_action`, so `main.rs`'s
/// `handle_key` only has to turn the answer into the actual mode
/// change/editor spawn, not decide it inline where a test can't reach it
/// without a live terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BPrefixAction {
    /// `bb`: open the tab picker.
    TabPicker,
    /// `ba`: annotate the current article's bookmark.
    Annotate,
    /// Any other second key: dead prefix — `handle_key` processes it as if
    /// `b` had never been typed (mirrors the existing `g`-prefix fallback,
    /// e.g. `gj` still scrolls).
    PassThrough,
}

pub fn resolve_b_prefix(second_key: char) -> BPrefixAction {
    match second_key {
        'b' => BPrefixAction::TabPicker,
        'a' => BPrefixAction::Annotate,
        _ => BPrefixAction::PassThrough,
    }
}

/// What the `r`-prefix chord's second key means (PRD FR-BM-3's `rl`).
/// Reached only when no background-revalidation "r to reload" notice is
/// armed — `Mode::Reading`'s `pending_r` arm in `main.rs` never engages this
/// latch at all when one is (bare `r` reloads on the spot then, as it always
/// has, taking precedence over the read-later chord entirely).
///
/// Unlike the `g`/`b` prefixes — which have no standalone meaning, so an
/// unrecognized second key falls through to being processed as itself
/// (`PassThrough` above) — bare `r` always used to open Research mode
/// outright. That meaning survives here as `OpenResearch`, the fallback for
/// every second key except `l`: the second key is consumed as part of
/// resolving the chord, not reprocessed as its own binding. The two
/// prefixes deliberately disagree on this point because only `r` ever had a
/// standalone action worth preserving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RPrefixAction {
    /// `rl`: enqueue `read_later_target()` for later.
    ReadLater,
    /// Any other second key: `r`'s own original meaning (open Research mode).
    OpenResearch,
}

pub fn resolve_r_prefix(second_key: char) -> RPrefixAction {
    match second_key {
        'l' => RPrefixAction::ReadLater,
        _ => RPrefixAction::OpenResearch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::LinkRef;

    fn doc(title: &str) -> Document {
        Document {
            title: title.to_string(),
            blocks: Vec::new(),
            citations: Vec::new(),
            truncated: false,
        }
    }

    /// The titles on the active tab's back stack, for asserting history
    /// contents now that entries are `(lang, title, scroll)` rather than bare
    /// strings.
    fn back_titles(app: &App) -> Vec<String> {
        app.active_tab()
            .back_stack
            .iter()
            .map(|e| e.title.clone())
            .collect()
    }

    fn forward_titles(app: &App) -> Vec<String> {
        app.active_tab()
            .forward_stack
            .iter()
            .map(|e| e.title.clone())
            .collect()
    }

    /// A truecolor tty snapshot, so `graphics_protocol` resolves to
    /// `HalfBlock` for an image-capable theme.
    fn truecolor_env() -> crate::graphics::GraphicsEnv {
        crate::graphics::GraphicsEnv {
            colorterm: "truecolor".to_string(),
            is_tty: true,
            ..Default::default()
        }
    }

    fn doc_with_image(src: &str) -> Document {
        let mut d = doc("Pic");
        d.blocks.push(crate::doc::Block::Image {
            src: Some(src.to_string()),
            alt: "alt".to_string(),
            caption: None,
        });
        d
    }

    fn tiny_image() -> crate::image::DecodedImage {
        crate::image::DecodedImage {
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
            ],
        }
    }

    #[test]
    fn text_theme_never_enables_images_or_reserves_boxes() {
        // A text theme (homebrew, images=false) resolves to no protocol even
        // on a kitty terminal, and reserves no image boxes — the property the
        // "text theme never fetches" pty check leans on (request_visible_images
        // returns early when the protocol is None).
        let mut app = App::new("en".to_string(), Theme::homebrew(), false);
        app.graphics_env = crate::graphics::GraphicsEnv {
            term: "xterm-kitty".to_string(),
            is_tty: true,
            ..Default::default()
        };
        app.open_document(doc_with_image("u"));
        app.image_store.set_ready("u".to_string(), tiny_image());
        assert!(!app.images_enabled());
        assert_eq!(
            app.graphics_protocol(),
            crate::graphics::GraphicsProtocol::None
        );
        assert!(
            app.image_box_map().is_empty(),
            "no boxes reserved for a text theme"
        );
    }

    #[test]
    fn full_theme_on_truecolor_reserves_a_box_for_a_decoded_image() {
        let mut app = App::new("en".to_string(), Theme::full(), false);
        app.graphics_env = truecolor_env();
        app.open_document(doc_with_image("u"));
        assert!(app.images_enabled());
        assert_eq!(
            app.graphics_protocol(),
            crate::graphics::GraphicsProtocol::HalfBlock
        );
        // Nothing decoded yet -> no box.
        assert!(app.image_box_map().is_empty());
        app.image_store.set_ready("u".to_string(), tiny_image());
        let map = app.image_box_map();
        assert!(map.contains_key("u"), "decoded image reserves a box");
    }

    #[test]
    fn set_images_toggle_flips_enablement_and_forces_relayout() {
        let mut app = App::new("en".to_string(), Theme::full(), false);
        app.graphics_env = truecolor_env();
        app.open_document(doc_with_image("u"));
        app.image_store.set_ready("u".to_string(), tiny_image());
        app.ensure_layout();
        let epoch_before = app.image_epoch;

        app.set_images(false);
        assert!(!app.images_enabled());
        assert!(app.image_epoch > epoch_before, "epoch bumped");
        assert!(app.layout.is_none(), "layout invalidated on toggle");
        assert_eq!(
            app.graphics_protocol(),
            crate::graphics::GraphicsProtocol::None
        );
        assert!(app.image_box_map().is_empty(), "images off -> no boxes");

        app.set_images(true);
        assert!(app.images_enabled());
        assert!(app.image_box_map().contains_key("u"));
    }

    #[test]
    fn switching_between_image_and_text_theme_invalidates_layout() {
        let mut app = App::new("en".to_string(), Theme::full(), false);
        app.graphics_env = truecolor_env();
        app.open_document(doc_with_image("u"));
        app.ensure_layout();
        assert!(app.layout.is_some());
        // full (images) -> homebrew (text) flips images_enabled, so relayout.
        app.set_theme(Theme::homebrew());
        assert!(app.layout.is_none(), "text/image theme swap relayouts");
    }

    #[test]
    fn back_and_forward_mirror_browser_semantics() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);

        app.open_document(doc("A"));
        app.open_document(doc("B"));
        app.open_document(doc("C"));
        assert_eq!(back_titles(&app), vec!["A", "B"]);
        assert!(app.active_tab().forward_stack.is_empty());
        assert_eq!(app.active_tab().doc.as_ref().unwrap().title, "C");

        let target = app.navigate_back_target().unwrap();
        assert_eq!(target.title, "B");
        assert_eq!(back_titles(&app), vec!["A"]);
        assert_eq!(forward_titles(&app), vec!["C"]);
        // navigate_back_target only adjusts the stacks; the caller installs
        // the fetched document via set_document.
        app.set_document(doc("B"));

        let target = app.navigate_forward_target().unwrap();
        assert_eq!(target.title, "C");
        assert_eq!(back_titles(&app), vec!["A", "B"]);
        assert!(app.active_tab().forward_stack.is_empty());
    }

    #[test]
    fn following_a_link_after_going_back_discards_forward_history() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("A"));
        app.open_document(doc("B"));

        let target = app.navigate_back_target().unwrap();
        assert_eq!(target.title, "A");
        app.set_document(doc("A"));
        assert_eq!(forward_titles(&app), vec!["B"]);

        // Reading A and following a different link (fresh navigation) should
        // drop the "forward to B" branch, exactly like a browser.
        app.open_document(doc("Z"));
        assert!(app.active_tab().forward_stack.is_empty());
        assert_eq!(back_titles(&app), vec!["A"]);
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
        app.active_tab_mut().links = vec![
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
        app.active_tab_mut().focused_link = None;

        app.cycle_link(true);
        assert_eq!(app.active_tab().focused_link, Some(0));
        app.cycle_link(true);
        app.cycle_link(true);
        assert_eq!(app.active_tab().focused_link, Some(2));
        app.cycle_link(true); // wraps forward past the end
        assert_eq!(app.active_tab().focused_link, Some(0));
        app.cycle_link(false); // wraps backward past the start
        assert_eq!(app.active_tab().focused_link, Some(2));
    }

    #[test]
    fn cycle_link_on_linkless_page_leaves_focus_unset() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.cycle_link(true);
        assert_eq!(app.active_tab().focused_link, None);
    }

    #[test]
    fn jump_to_section_clamps_to_max_scroll() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        // A heading far down the article: its laid-out line is well past a
        // deliberately tiny max_scroll, so the jump must clamp.
        let mut html = String::from("<html><body>");
        for _ in 0..40 {
            html.push_str("<p>filler paragraph number something</p>");
        }
        html.push_str("<h2>Late Section</h2><p>tail</p></body></html>");
        app.set_document(crate::doc::parse_article_html("T", &html));
        app.layout_width = 80;
        app.active_tab_mut().max_scroll = 30; // a short viewport: the heading's line is past the end
        app.mode = Mode::Toc;

        assert_eq!(app.active_tab().sections.len(), 1);
        app.jump_to_section(0);
        assert_eq!(app.active_tab().scroll, 30);
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
        app.active_tab_mut().max_scroll = 100; // pretend a long article so clamping never kicks in

        app.active_tab_mut().find_input = "alpha".to_string();
        app.update_find();

        assert_eq!(
            app.active_tab().find_matches.len(),
            2,
            "two paragraphs mention alpha"
        );
        assert_eq!(
            app.active_tab().scroll,
            app.active_tab().find_matches[0],
            "should jump straight to the first match"
        );
    }

    #[test]
    fn find_next_and_prev_wrap_around() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let html = "<html><body><p>alpha</p><p>alpha</p><p>alpha</p></body></html>";
        app.set_document(crate::doc::parse_article_html("Test", html));
        app.active_tab_mut().max_scroll = 100;
        app.active_tab_mut().find_input = "alpha".to_string();
        app.update_find();
        assert_eq!(app.active_tab().find_matches.len(), 3);

        assert_eq!(app.active_tab().find_index, 0);
        app.find_next();
        assert_eq!(app.active_tab().find_index, 1);
        app.find_next();
        assert_eq!(app.active_tab().find_index, 2);
        app.find_next(); // wraps forward past the last match
        assert_eq!(app.active_tab().find_index, 0);
        app.find_prev(); // wraps backward past the first match
        assert_eq!(app.active_tab().find_index, 2);
    }

    #[test]
    fn opening_a_new_document_clears_stale_find_state() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "First",
            "<html><body><p>alpha</p></body></html>",
        ));
        app.active_tab_mut().max_scroll = 100;
        app.active_tab_mut().find_input = "alpha".to_string();
        app.update_find();
        assert_eq!(app.active_tab().find_matches.len(), 1);

        app.set_document(crate::doc::parse_article_html(
            "Second",
            "<html><body><p>bravo</p></body></html>",
        ));
        assert!(
            app.active_tab().find_input.is_empty(),
            "a new article's matches must not carry over from the old one"
        );
        assert!(app.active_tab().find_matches.is_empty());
    }

    #[test]
    fn find_next_and_prev_on_no_matches_do_not_panic() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.find_next();
        app.find_prev();
        assert_eq!(app.active_tab().find_index, 0);
    }

    /// A document whose paragraphs wrap: find matches must land on
    /// laid-out-line positions (the row the block starts on in the layout),
    /// not on the logical-line counts the old renderer used — this is the
    /// scroll-unit defect the layout engine exists to fix.
    #[test]
    fn find_matches_are_laid_line_positions_on_a_wrapped_page() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let long = "word ".repeat(60); // ~300 cells: wraps to several lines at width 40
        let html = format!("<html><body><p>{long}</p><p>needle paragraph</p></body></html>");
        app.set_document(crate::doc::parse_article_html("Test", &html));
        app.layout_width = 40;
        app.active_tab_mut().max_scroll = 100;

        app.active_tab_mut().find_input = "needle".to_string();
        app.update_find();

        assert_eq!(app.active_tab().find_matches.len(), 1);
        let layout = app.layout.as_ref().expect("layout built by update_find");
        let wrapped_first_para = layout.block_lines[1] - layout.block_lines[0];
        assert!(
            wrapped_first_para > 2,
            "the first paragraph must actually wrap for this test to bite"
        );
        assert_eq!(
            app.active_tab().find_matches[0] as usize,
            layout.block_lines[1],
            "the match must be the needle block's laid-out line"
        );
    }

    #[test]
    fn cycling_to_an_offscreen_link_scrolls_it_into_view() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let mut html = String::from("<html><body>");
        html.push_str(r##"<p>Top <a href="./A">first link</a>.</p>"##);
        for _ in 0..30 {
            html.push_str("<p>filler paragraph text</p>");
        }
        html.push_str(r##"<p>Bottom <a href="./B">second link</a>.</p></body></html>"##);
        app.set_document(crate::doc::parse_article_html("Test", &html));
        app.layout_width = 80;
        app.viewport_height = 10;
        app.ensure_layout();
        app.active_tab_mut().max_scroll =
            (app.layout.as_ref().unwrap().lines.len() as u16).saturating_sub(10);

        assert_eq!(app.active_tab().focused_link, Some(0));
        assert_eq!(app.active_tab().scroll, 0);
        app.cycle_link(true); // second link, far below the 10-row viewport
        let link_line = app.layout.as_ref().unwrap().link_lines[1] as u16;
        assert!(link_line > 10, "second link must start off-screen");
        assert!(
            app.active_tab().scroll <= link_line && link_line < app.active_tab().scroll + 10,
            "focused link line {link_line} must be inside viewport starting at {}",
            app.active_tab().scroll
        );

        app.cycle_link(true); // wraps to the first link back at the top
        let first_line = app.layout.as_ref().unwrap().link_lines[0] as u16;
        assert!(
            app.active_tab().scroll <= first_line && first_line < app.active_tab().scroll + 10,
            "wrapping back to the top link must scroll it into view (scroll {}, line {first_line})",
            app.active_tab().scroll
        );
    }

    #[test]
    fn layout_cache_survives_scrolling_but_invalidates_on_width_and_options() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "Test",
            "<html><body><p>some content here</p></body></html>",
        ));
        assert!(app.layout.is_none(), "a new document starts unlaid");

        app.layout_width = 80;
        app.ensure_layout();
        let first = app.layout.clone().expect("layout built");
        app.ensure_layout();
        assert_eq!(
            app.layout.as_ref().unwrap().lines,
            first.lines,
            "same width + options must reuse (not change) the layout"
        );

        app.layout_width = 40;
        app.ensure_layout();
        assert_eq!(app.layout.as_ref().unwrap().width, 40, "resize relaid");

        app.ambiguous_wide = true;
        app.ensure_layout();
        assert!(
            app.layout.as_ref().unwrap().options.ambiguous_wide,
            "option change relaid"
        );
    }

    /// PRD FR-OFF-1's L1 layer: reopening a previously laid-out article at
    /// the same identity (lang/title/revid) and width must reuse the L1
    /// entry instead of relaying out — proven via a counter that only
    /// increments on an actual `layout::layout_document` call (see
    /// `layout_computations`'s doc comment for why a counter over
    /// pointer-identity tricks).
    #[test]
    fn reopening_a_previously_laid_out_article_reuses_the_l1_cache_not_a_relayout() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.layout_width = 80;

        app.active_tab_mut().current_revid = 7;
        app.set_document(crate::doc::parse_article_html(
            "Article A",
            "<html><body><p>hello world</p></body></html>",
        ));
        app.ensure_layout();
        assert_eq!(app.layout_computations, 1);

        app.active_tab_mut().current_revid = 9;
        app.set_document(crate::doc::parse_article_html(
            "Article B",
            "<html><body><p>a different article entirely</p></body></html>",
        ));
        app.ensure_layout();
        assert_eq!(app.layout_computations, 2);

        // Navigate back to Article A at the identical identity and width:
        // the L1 cache must serve it without another layout pass.
        app.active_tab_mut().current_revid = 7;
        app.set_document(crate::doc::parse_article_html(
            "Article A",
            "<html><body><p>hello world</p></body></html>",
        ));
        app.ensure_layout();
        assert_eq!(app.layout_computations, 2, "L1 cache hit must not relayout");
    }

    /// A new revid for the same title (PRD FR-OFF-2's post-revalidation
    /// reload) must miss L1 and relayout — reusing Article A's old layout
    /// under its new content would be silently wrong.
    #[test]
    fn a_new_revid_for_the_same_title_misses_the_l1_cache() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.layout_width = 80;

        app.active_tab_mut().current_revid = 1;
        app.set_document(crate::doc::parse_article_html(
            "Article",
            "<html><body><p>old content</p></body></html>",
        ));
        app.ensure_layout();
        assert_eq!(app.layout_computations, 1);

        app.active_tab_mut().current_revid = 2;
        app.set_document(crate::doc::parse_article_html(
            "Article",
            "<html><body><p>new content</p></body></html>",
        ));
        app.ensure_layout();
        assert_eq!(
            app.layout_computations, 2,
            "a different revid must not reuse the old revid's layout"
        );
    }

    #[test]
    fn reload_from_pending_update_swaps_in_l2_content_and_marks_it_live() {
        let dir = std::env::temp_dir().join(format!("wikitui-reload-test-{}", std::process::id()));
        let cache = crate::cache::PageCache::at(
            dir.clone(),
            crate::cache::DEFAULT_MAX_BYTES,
            crate::cache::FRESH_TTL_SECS,
            crate::cache::DEFAULT_FORCE_REFETCH_SECS,
        );
        cache.put(
            "en",
            "Alan Turing",
            "<html><body><p>updated body</p></body></html>",
            2,
            None,
        );

        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "Alan Turing",
            "<html><body><p>original body</p></body></html>",
        ));
        app.active_tab_mut().current_revid = 1;
        app.active_tab_mut().page_source = PageSource::Cached { age_secs: 100_000 };
        app.active_tab_mut().pending_reload = Some(PendingReload {
            lang: "en".to_string(),
            title: "Alan Turing".to_string(),
        });
        app.notice = Some("updated — r to reload".to_string());

        app.reload_from_pending_update(&cache);

        assert!(
            app.active_tab().pending_reload.is_none(),
            "consumed on reload"
        );
        assert!(app.notice.is_none(), "reload clears the notice");
        assert_eq!(app.active_tab().current_revid, 2);
        assert_eq!(app.active_tab().page_source, PageSource::Live);
        assert!(
            crate::doc::render_plain(app.active_tab().doc.as_ref().unwrap())
                .contains("updated body"),
            "the new content must actually be what's rendered"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_from_pending_update_with_nothing_pending_is_a_no_op() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let cache = crate::cache::PageCache::disabled();
        app.reload_from_pending_update(&cache); // must not panic
        assert!(app.active_tab().pending_reload.is_none());
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

    #[test]
    fn debounce_due_fires_only_once_the_deadline_has_passed() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_millis(200);
        assert!(!debounce_due(deadline, t0 + Duration::from_millis(199)));
        assert!(debounce_due(deadline, t0 + Duration::from_millis(200)));
        assert!(debounce_due(deadline, t0 + Duration::from_millis(500)));
    }

    #[test]
    fn typeahead_is_current_discards_stale_responses() {
        assert!(typeahead_is_current("Alan Tur", "Alan Tur"));
        assert!(
            !typeahead_is_current("Alan Tur", "Alan Turi"),
            "a response for an earlier keystroke's query must be dropped"
        );
        assert!(!typeahead_is_current("Alan Tur", ""));
    }

    #[test]
    fn zero_results_message_offers_did_you_mean_when_present() {
        assert_eq!(
            zero_results_message("Alan Truing", Some("Alan Turing")),
            "No results for \"Alan Truing\". Did you mean Alan Turing? (Enter to search)"
        );
        assert_eq!(
            zero_results_message("xyzzy", None),
            "No results for \"xyzzy\""
        );
    }

    #[test]
    fn queue_typeahead_arms_the_debounce_for_a_nonempty_query_and_disarms_for_empty() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.search_input = "Alan".to_string();
        app.queue_typeahead();
        assert!(app.search_debounce_at.is_some());

        app.search_input = "   ".to_string(); // whitespace-only counts as empty
        app.queue_typeahead();
        assert!(app.search_debounce_at.is_none());
        assert!(
            app.typeahead.is_empty(),
            "clearing the query must drop stale suggestions too"
        );
    }

    #[test]
    fn move_suggestion_wraps_in_both_directions() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.typeahead = vec![
            TitleSuggestion {
                title: "A".into(),
                description: None,
            },
            TitleSuggestion {
                title: "B".into(),
                description: None,
            },
            TitleSuggestion {
                title: "C".into(),
                description: None,
            },
        ];

        assert_eq!(app.selected_suggestion, 0);
        app.move_suggestion(true);
        assert_eq!(app.selected_suggestion, 1);
        app.move_suggestion(false);
        assert_eq!(app.selected_suggestion, 0);
        app.move_suggestion(false); // wraps backward past the start
        assert_eq!(app.selected_suggestion, 2);
        app.move_suggestion(true); // wraps forward past the end
        assert_eq!(app.selected_suggestion, 0);
    }

    #[test]
    fn move_suggestion_on_empty_list_is_a_no_op() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.move_suggestion(true);
        assert_eq!(app.selected_suggestion, 0);
    }

    /// PRD FR-RD-4 horizontal scroll: `[`/`]` (App::scroll_tables) clamps the
    /// shared column offset to the widest table's column count and floors at 0.
    #[test]
    fn table_scroll_offset_clamps_to_the_widest_table() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let mut html = String::from("<html><body><table class=\"wikitable\"><tbody><tr>");
        for c in 0..5 {
            html.push_str(&format!("<td>c{c}</td>"));
        }
        html.push_str("</tr></tbody></table></body></html>");
        app.set_document(crate::doc::parse_article_html("T", &html));
        assert_eq!(app.max_table_columns(), 5);

        for _ in 0..10 {
            app.scroll_tables(1);
        }
        assert_eq!(
            app.active_tab().table_col_offset,
            4,
            "offset clamps to columns - 1"
        );
        app.scroll_tables(-100);
        assert_eq!(app.active_tab().table_col_offset, 0, "floors at 0");
    }

    #[test]
    fn table_scroll_is_a_no_op_without_tables() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "T",
            "<html><body><p>no tables here</p></body></html>",
        ));
        app.scroll_tables(1);
        assert_eq!(app.active_tab().table_col_offset, 0);
    }

    #[test]
    fn layout_options_carry_accessible_and_the_tabs_table_offset() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.accessible = true;
        app.active_tab_mut().table_col_offset = 3;
        let opts = app.layout_options();
        assert!(opts.accessible);
        assert_eq!(opts.table_col_offset, 3);
    }

    /// FR-NV-6b: highlighting now covers every literal occurrence, not just
    /// one entry per matching block — locks the upgrade this feature made
    /// over the old block-granularity behavior.
    #[test]
    fn update_find_counts_every_occurrence_not_just_every_block() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "Test",
            "<html><body><p>turing turing turing</p></body></html>",
        ));
        app.active_tab_mut().max_scroll = 100;
        app.active_tab_mut().find_input = "turing".to_string();
        app.update_find();

        assert_eq!(
            app.active_tab().find_matches.len(),
            3,
            "one match per occurrence, all on the same block/line"
        );
        assert_eq!(app.active_tab().find_occurrences.len(), 3);
        // All three occurrences land on the same line (the block didn't
        // wrap), each a single piece, at increasing column ranges.
        let cols: Vec<usize> = app
            .active_tab()
            .find_occurrences
            .iter()
            .map(|occ| {
                assert_eq!(occ.pieces.len(), 1, "no wrap here, so no split occurrence");
                occ.pieces[0].1.start
            })
            .collect();
        assert!(cols.windows(2).all(|w| w[0] < w[1]));
    }

    /// The bug this data model exists to avoid: a query that straddles a
    /// visual line wrap must count as ONE occurrence (not two). Bypasses
    /// real wrapping (whose exact break point is an implementation detail)
    /// by handing `update_find` a hand-built `Layout` where the wrap is
    /// exactly where the test needs it — `App` only ever delegates to
    /// `layout::find_matches` and assigns its result verbatim, so this
    /// pins that delegation stays a straight passthrough, never a
    /// re-flatten-per-line that would double-count a split match (the bug
    /// interactive verification caught before this test existed).
    #[test]
    fn update_find_treats_a_wrapped_match_as_one_occurrence() {
        use crate::layout::{LaidLine, LaidSpan, Layout, LayoutOptions, SpanKind};

        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "Test",
            "<html><body><p>placeholder</p></body></html>",
        ));
        app.layout = Some(Layout {
            width: 20,
            options: LayoutOptions::default(),
            lines: vec![
                LaidLine {
                    spans: vec![LaidSpan {
                        text: "computer".to_string(),
                        kind: SpanKind::Plain,
                    }],
                },
                LaidLine {
                    spans: vec![LaidSpan {
                        text: "science is fun".to_string(),
                        kind: SpanKind::Plain,
                    }],
                },
            ],
            block_lines: vec![0],
            link_lines: vec![],
            link_cols: vec![],
            continuation: vec![true],
        });
        app.layout_width = 20; // matches the hand-built Layout's `width`, so `ensure_layout` (which `update_find` calls) sees it as fresh and doesn't discard it
        app.active_tab_mut().max_scroll = 100;

        app.active_tab_mut().find_input = "computer science".to_string();
        app.update_find();

        assert_eq!(
            app.active_tab().find_occurrences.len(),
            1,
            "one query occurrence must stay one entry even though a wrap splits it"
        );
        assert_eq!(
            app.active_tab().find_occurrences[0].pieces.len(),
            2,
            "split across both lines"
        );
        assert_eq!(app.active_tab().find_matches.len(), 1);
    }

    /// Smart-case (FR-NV-6a) reaches all the way through `App::update_find`,
    /// not just the lower-level `layout::find_matches` it delegates to.
    #[test]
    fn update_find_is_smart_case() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "Test",
            "<html><body><p>Alan Turing was here</p></body></html>",
        ));
        app.active_tab_mut().max_scroll = 100;

        app.active_tab_mut().find_input = "turing".to_string();
        app.update_find();
        assert_eq!(
            app.active_tab().find_matches.len(),
            1,
            "lowercase query is case-insensitive"
        );

        app.active_tab_mut().find_input = "Turing".to_string();
        app.update_find();
        assert_eq!(
            app.active_tab().find_matches.len(),
            1,
            "matching case still matches"
        );

        app.active_tab_mut().find_input = "TURING".to_string();
        app.update_find();
        assert!(
            app.active_tab().find_matches.is_empty(),
            "wrong-case query with an uppercase letter must not match"
        );
    }

    // ---- Tab system (PRD FR-TB-1..3) ------------------------------------

    /// An `App` with `n` article tabs (titles "T0".."T{n-1}"), tab `i`
    /// focused. Each tab holds a distinct one-paragraph document.
    fn app_with_tabs(n: usize, focus: usize) -> App {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        // The first tab already exists; fill it, then open the rest.
        for i in 0..n {
            if i > 0 {
                app.new_foreground_tab();
            }
            app.set_document(doc(&format!("T{i}")));
        }
        app.active = focus;
        app.sync_via_public_switch(focus);
        app
    }

    impl App {
        /// Test helper: switch to a tab index through the public path so the
        /// active-tab bookkeeping (citations, lang) is consistent.
        fn sync_via_public_switch(&mut self, index: usize) {
            self.switch_to_tab(index);
        }
    }

    #[test]
    fn closing_a_tab_left_of_active_shifts_the_active_index() {
        let mut app = app_with_tabs(4, 2); // active = T2
        assert_eq!(app.active_tab().doc.as_ref().unwrap().title, "T2");
        let quit = app.close_tab(0); // close T0, left of active
        assert!(!quit);
        assert_eq!(app.tabs.len(), 3);
        assert_eq!(
            app.active_tab().doc.as_ref().unwrap().title,
            "T2",
            "focus must stay on the same article after a left-of-active close"
        );
    }

    #[test]
    fn closing_the_active_tab_focuses_its_right_neighbor() {
        let mut app = app_with_tabs(4, 1); // active = T1
        app.close_tab(1); // close active
        assert_eq!(app.tabs.len(), 3);
        assert_eq!(
            app.active_tab().doc.as_ref().unwrap().title,
            "T2",
            "closing the active tab focuses the tab that slid into its slot"
        );
    }

    #[test]
    fn closing_the_last_index_active_tab_focuses_the_new_last() {
        let mut app = app_with_tabs(3, 2); // active = last (T2)
        app.close_tab(2);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(
            app.active, 1,
            "closing the last tab moves focus to the new last"
        );
        assert_eq!(app.active_tab().doc.as_ref().unwrap().title, "T1");
    }

    #[test]
    fn closing_the_final_remaining_tab_signals_quit() {
        let mut app = app_with_tabs(1, 0);
        let quit = app.close_active_tab();
        assert!(quit, "closing the last remaining tab must signal a quit");
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn per_tab_back_stacks_are_isolated() {
        let mut app = app_with_tabs(2, 0);
        // Navigate within tab 0 (open_document pushes the current article).
        app.open_document(doc("T0-child"));
        assert_eq!(back_titles(&app), vec!["T0"]);

        // Tab 1's history is untouched by tab 0's navigation.
        app.switch_to_tab(1);
        assert!(
            app.active_tab().back_stack.is_empty(),
            "a fresh tab's back stack is independent of another tab's"
        );
        app.open_document(doc("T1-child"));
        assert_eq!(back_titles(&app), vec!["T1"]);

        // Back in tab 0, its own stack is still exactly what we left it.
        app.switch_to_tab(0);
        assert_eq!(back_titles(&app), vec!["T0"]);
    }

    #[test]
    fn navigating_captures_scroll_and_back_restores_it() {
        let mut app = app_with_tabs(1, 0);
        // Read partway down T0, then follow a link to a child article.
        app.active_tab_mut().scroll = 42;
        app.open_document(doc("Child"));
        assert_eq!(app.active_tab().scroll, 0, "the child opens at the top");

        // Going back returns the entry carrying T0's captured scroll.
        let entry = app.navigate_back_target().unwrap();
        assert_eq!(entry.title, "T0");
        assert_eq!(
            entry.scroll, 42,
            "the back entry restores the scroll position we left at"
        );
    }

    #[test]
    fn close_undo_restores_document_stacks_and_scroll() {
        let mut app = app_with_tabs(2, 0);
        // Build some history + a scroll position in tab 0.
        app.open_document(doc("T0-child"));
        app.active_tab_mut().scroll = 17;
        let before_back = back_titles(&app);
        assert_eq!(before_back, vec!["T0"]);

        // Close tab 0, then reopen it.
        app.close_tab(0);
        assert_eq!(app.tabs.len(), 1);
        assert!(app.reopen_closed_tab());
        assert_eq!(app.tabs.len(), 2);

        let restored = app.active_tab();
        assert_eq!(restored.doc.as_ref().unwrap().title, "T0-child");
        assert_eq!(restored.scroll, 17, "scroll restored from the snapshot");
        assert_eq!(
            restored
                .back_stack
                .iter()
                .map(|e| e.title.clone())
                .collect::<Vec<_>>(),
            vec!["T0"],
            "the back stack survives close/undo intact"
        );
    }

    #[test]
    fn reopen_with_empty_undo_stack_is_a_noop() {
        let mut app = app_with_tabs(1, 0);
        assert!(!app.reopen_closed_tab());
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn close_undo_stack_is_capped_and_drops_oldest() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        // Open and close far more tabs than the cap.
        for i in 0..(CLOSED_TABS_CAP + 5) {
            app.new_foreground_tab();
            app.set_document(doc(&format!("C{i}")));
            app.close_active_tab();
        }
        assert_eq!(
            app.closed_tabs.len(),
            CLOSED_TABS_CAP,
            "the undo stack never grows past its cap, so old Documents drop"
        );
        // The most-recently-closed is on top; the oldest few are gone.
        let newest = app.closed_tabs.last().unwrap();
        assert_eq!(
            newest.doc.as_ref().unwrap().title,
            format!("C{}", CLOSED_TABS_CAP + 4)
        );
    }

    #[test]
    fn next_and_prev_tab_wrap() {
        let mut app = app_with_tabs(3, 0);
        app.next_tab();
        assert_eq!(app.active, 1);
        app.next_tab();
        app.next_tab(); // wraps 2 -> 0
        assert_eq!(app.active, 0);
        app.prev_tab(); // wraps 0 -> 2
        assert_eq!(app.active, 2);
    }

    #[test]
    fn opening_a_background_tab_does_not_move_focus() {
        let mut app = app_with_tabs(1, 0);
        let active_before = app.active;
        let id = app.open_background_tab("Enigma machine".to_string(), "en".to_string());
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active, active_before, "focus must not move");
        let bg = app.tab_index_by_id(id).unwrap();
        assert!(app.tabs[bg].loading, "the background tab starts loading");
        assert_eq!(
            app.tabs[bg].display_title(),
            "Enigma machine",
            "it shows its target title while loading"
        );
    }

    #[test]
    fn jump_to_back_entry_moves_intermediate_history_to_forward() {
        let mut app = app_with_tabs(1, 0);
        // Walk A(=T0) -> B -> C -> D within the tab.
        app.open_document(doc("B"));
        app.open_document(doc("C"));
        app.open_document(doc("D"));
        assert_eq!(back_titles(&app), vec!["T0", "B", "C"]);

        // Jump straight back to the oldest entry (index 0 == "T0").
        let target = app.jump_to_back_entry(0).unwrap();
        assert_eq!(target.title, "T0");
        assert!(app.active_tab().back_stack.is_empty());
        // Current (D) and the skipped-over entries become forward history,
        // ordered so Forward replays D last.
        assert_eq!(forward_titles(&app), vec!["D", "C", "B"]);
    }

    #[test]
    fn breadcrumb_titles_are_the_last_few_plus_current() {
        let mut app = app_with_tabs(1, 0);
        for t in ["B", "C", "D", "E"] {
            app.open_document(doc(t));
        }
        // back_stack = [T0, B, C, D], current = E; trail keeps the last 3
        // back entries plus the current article.
        assert_eq!(
            app.breadcrumb_titles(),
            vec!["B", "C", "D", "E"],
            "breadcrumb trims to the most recent hops"
        );
    }

    #[test]
    fn switching_tabs_rearms_a_pending_reload_notice() {
        let mut app = app_with_tabs(2, 0);
        // Simulate a background revalidation that armed tab 1's reload while
        // tab 0 was on screen.
        app.tabs[1].pending_reload = Some(PendingReload {
            lang: "en".to_string(),
            title: "T1".to_string(),
        });
        assert!(app.notice.is_none());
        app.switch_to_tab(1);
        assert_eq!(
            app.notice.as_deref(),
            Some("updated — r to reload"),
            "switching to a tab with a pending reload re-arms the notice"
        );
    }

    // -- Link hints (PRD FR-NV-1) ----------------------------------------

    fn app_with_html(html: &str) -> App {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", html));
        app.layout_width = 80;
        app.viewport_height = 24;
        app
    }

    #[test]
    fn hint_mode_with_no_visible_links_stays_in_reading() {
        let mut app = app_with_html("<html><body><p>no links here</p></body></html>");
        app.enter_hint_mode(false);
        assert_eq!(
            app.mode,
            Mode::Reading,
            "nothing to hint, so hint mode never actually opens"
        );
        assert!(app.hint_targets.is_empty());
    }

    #[test]
    fn enter_hint_mode_labels_every_visible_link_in_reading_order() {
        let html = r##"<html><body><p>See <a href="./A">Alpha</a> and
            <a href="./B">Bravo</a> and <a href="./C">Charlie</a>.</p></body></html>"##;
        let mut app = app_with_html(html);

        app.enter_hint_mode(false);
        assert_eq!(app.mode, Mode::Hint);
        assert!(!app.hint_background, "`f` follows in the current tab");
        assert_eq!(app.hint_targets.len(), 3);
        assert_eq!(
            app.hint_targets.iter().map(|t| t.link).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        app.exit_hint_mode();
        assert_eq!(app.mode, Mode::Reading);
        assert!(app.hint_targets.is_empty());
        assert!(app.hint_input.is_empty());
    }

    #[test]
    fn entering_hint_mode_with_the_background_flag_records_it() {
        let html = r##"<html><body><p>See <a href="./A">Alpha</a>.</p></body></html>"##;
        let mut app = app_with_html(html);
        app.enter_hint_mode(true);
        assert!(app.hint_background, "`F` opens into a background tab");
    }

    #[test]
    fn narrow_hint_input_ignores_unmatched_chars_and_resolves_a_completed_label() {
        let html = r##"<html><body><p>See <a href="./A">Alpha</a> and
            <a href="./B">Bravo</a>.</p></body></html>"##;
        let mut app = app_with_html(html);
        app.enter_hint_mode(false);

        // Two visible links get the first two HINT_ALPHABET labels, "a" and "s".
        assert_eq!(app.hint_targets[0].label, "a");
        assert_eq!(app.hint_targets[1].label, "s");

        // A character that's the prefix of no label is ignored outright —
        // the typed-so-far input is left exactly as it was.
        let outcome = app.narrow_hint_input('z');
        assert_eq!(outcome, hints::HintOutcome::Ignored);
        assert!(app.hint_input.is_empty());

        // Typing the second link's label resolves to ITS link index, not
        // the first's.
        let outcome = app.narrow_hint_input('s');
        assert_eq!(outcome, hints::HintOutcome::Resolved(1));
    }

    #[test]
    fn resolve_hint_action_routes_by_the_background_flag_and_internal_vs_external() {
        let html = r##"<html><body><p>See <a href="./Internal_Target">Alpha</a> and
            <a href="https://example.com/">External</a>.</p></body></html>"##;
        let mut app = app_with_html(html);

        app.enter_hint_mode(false);
        assert_eq!(
            app.resolve_hint_action(0),
            Some(HintFollowAction::Foreground("Internal Target".to_string())),
            "`f` on an internal link follows it in this tab"
        );
        assert!(
            matches!(
                app.resolve_hint_action(1),
                Some(HintFollowAction::External(_))
            ),
            "an external link's hint resolves to the same notice Enter shows today"
        );

        // The identical link resolves to `Background` once hint mode was
        // entered via `F` instead — `handle_key` is what turns this into an
        // actual `open_background_tab` call (PRD FR-TB-3); this only proves
        // the ROUTING DECISION, no network involved.
        app.enter_hint_mode(true);
        assert_eq!(
            app.resolve_hint_action(0),
            Some(HintFollowAction::Background("Internal Target".to_string()))
        );

        assert_eq!(
            app.resolve_hint_action(99),
            None,
            "an out-of-range link index resolves to nothing, not a panic"
        );
    }

    /// PRD's "hints survive reflow": a resize mid-hint-mode must re-derive
    /// the visible set from the NEW layout, not keep serving the line/column
    /// positions computed for the old width. Built so a long filler
    /// paragraph wraps to a different number of lines at the two widths,
    /// pushing the one link below a viewport that used to include it —
    /// computed from the real layouts directly, not hand-guessed, so the
    /// test fails loudly (rather than passing vacuously) if the premise
    /// ever stops holding.
    #[test]
    fn hint_targets_recompute_from_the_new_layout_after_a_resize() {
        let mut html = String::from("<html><body><p>");
        for _ in 0..15 {
            html.push_str("filler word run that wraps differently at each width ");
        }
        html.push_str("</p><p>See <a href=\"./Target\">Target</a> here.</p></body></html>");
        let document = crate::doc::parse_article_html("T", &html);

        let wide = layout::layout_document(&document, 88, LayoutOptions::default());
        let narrow = layout::layout_document(&document, 30, LayoutOptions::default());
        assert_ne!(
            wide.link_lines[0], narrow.link_lines[0],
            "the filler must wrap to a different line count at these widths, \
             or this test doesn't actually exercise a reflow"
        );

        let viewport_height = (wide.link_lines[0] + 1) as u16;
        assert!(
            narrow.link_lines[0] as u16 >= viewport_height,
            "the narrower layout must push the link below this viewport, \
             or this test doesn't actually exercise a reflow"
        );

        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(document);
        app.viewport_height = viewport_height;
        app.layout_width = 88;

        app.enter_hint_mode(false);
        assert_eq!(
            app.hint_targets.len(),
            1,
            "the link is visible at the wide width"
        );
        assert_eq!(app.hint_targets[0].line, wide.link_lines[0]);

        // Simulate the resize exactly as `ui::draw_reading` would notice it:
        // a new width, then the same `ensure_layout` + `refresh_hint_targets`
        // sequence it runs on every draw.
        app.layout_width = 30;
        app.ensure_layout();
        app.refresh_hint_targets();
        assert!(
            app.hint_targets.is_empty(),
            "the link scrolled below the viewport at the new width and must \
             no longer be hinted — a stale hint here would follow the wrong \
             thing if the reader typed its old label"
        );
    }

    // ---- Bookmarks / read-later (PRD FR-BM-1..4, 7) ----------------------

    fn app_with_bookmarks() -> App {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.bookmarks = crate::bookmarks::BookmarkStore::in_memory();
        app.readlater = crate::bookmarks::ReadLaterStore::in_memory();
        app
    }

    #[test]
    fn toggle_bookmark_adds_then_removes_with_a_notice_both_ways() {
        let mut app = app_with_bookmarks();
        app.set_document(doc("Alan Turing"));
        app.active_tab_mut().current_revid = 7;

        app.toggle_bookmark();
        assert!(app.bookmarks.is_bookmarked("en", "Alan Turing"));
        assert!(app.notice.as_deref().unwrap().contains("Bookmarked"));
        assert_eq!(app.bookmarks.bookmarks[0].revid_at_bookmark, Some(7));

        app.toggle_bookmark();
        assert!(!app.bookmarks.is_bookmarked("en", "Alan Turing"));
        assert!(app.notice.as_deref().unwrap().contains("Removed"));
    }

    #[test]
    fn toggle_bookmark_with_no_document_reports_instead_of_panicking() {
        let mut app = app_with_bookmarks();
        app.toggle_bookmark();
        assert!(app.status.contains("Open an article first"));
    }

    #[test]
    fn bookmark_picker_filter_narrows_the_visible_list() {
        let mut app = app_with_bookmarks();
        app.bookmarks.toggle("en", "Enigma machine", None);
        app.bookmarks
            .set_tags("en", "Enigma machine", vec!["crypto".into()]);
        app.bookmarks.toggle("en", "Ada Lovelace", None);

        app.open_bookmark_picker();
        assert_eq!(
            app.visible_bookmarks().len(),
            2,
            "no filter shows everything"
        );

        app.bookmark_filter_input = "#crypto".to_string();
        assert_eq!(
            app.visible_bookmarks(),
            vec![0],
            "only the tagged one matches"
        );

        app.bookmark_filter_input = "#nope".to_string();
        assert!(
            app.visible_bookmarks().is_empty(),
            "an unused tag matches nothing"
        );

        app.bookmark_filter_input = "lovelace".to_string();
        assert_eq!(app.visible_bookmarks(), vec![1]);
    }

    #[test]
    fn cycle_bookmark_wraps_over_the_filtered_list_not_the_whole_store() {
        let mut app = app_with_bookmarks();
        app.bookmarks.toggle("en", "A", None);
        app.bookmarks.set_tags("en", "A", vec!["x".into()]);
        app.bookmarks.toggle("en", "B", None); // untagged
        app.bookmarks.toggle("en", "C", None);
        app.bookmarks.set_tags("en", "C", vec!["x".into()]);

        app.open_bookmark_picker();
        app.bookmark_filter_input = "#x".to_string();
        assert_eq!(app.visible_bookmarks(), vec![0, 2]);

        assert_eq!(app.selected_bookmark, 0);
        app.cycle_bookmark(true);
        assert_eq!(app.selected_bookmark, 1, "second (and last) visible entry");
        app.cycle_bookmark(true);
        assert_eq!(app.selected_bookmark, 0, "wraps within the filtered list");
    }

    #[test]
    fn delete_selected_bookmark_removes_it_and_keeps_selection_in_range() {
        let mut app = app_with_bookmarks();
        app.bookmarks.toggle("en", "A", None);
        app.bookmarks.toggle("en", "B", None);
        app.open_bookmark_picker();
        app.selected_bookmark = 1; // "B"

        app.delete_selected_bookmark();
        assert!(!app.bookmarks.is_bookmarked("en", "B"));
        assert!(app.bookmarks.is_bookmarked("en", "A"));
        assert_eq!(app.selected_bookmark, 0);
    }

    #[test]
    fn tag_edit_round_trips_through_the_prompt() {
        let mut app = app_with_bookmarks();
        app.bookmarks.toggle("en", "Alan Turing", None);
        app.open_bookmark_picker();

        app.begin_tag_edit();
        assert_eq!(app.mode, Mode::BookmarkTagEdit);
        assert_eq!(
            app.bookmark_tag_input, "",
            "a fresh bookmark starts untagged"
        );

        app.bookmark_tag_input = "crypto, ww2".to_string();
        app.commit_tag_edit();
        assert_eq!(app.mode, Mode::BookmarkPicker);
        assert_eq!(
            app.bookmarks.find("en", "Alan Turing").unwrap().tags,
            vec!["crypto", "ww2"]
        );

        // Re-opening the editor seeds from the now-current tags.
        app.begin_tag_edit();
        assert_eq!(app.bookmark_tag_input, "crypto, ww2");
    }

    #[test]
    fn read_later_target_prefers_the_focused_internal_link_over_the_article() {
        let mut app = app_with_bookmarks();
        let html =
            r##"<html><body><p>See <a href="./Enigma_machine">Enigma</a>.</p></body></html>"##;
        app.set_document(crate::doc::parse_article_html("Alan Turing", html));
        app.active_tab_mut().focused_link = Some(0);

        assert_eq!(
            app.read_later_target(),
            Some(("en".to_string(), "Enigma machine".to_string())),
            "a focused internal link wins over the article on screen"
        );

        app.active_tab_mut().focused_link = None;
        assert_eq!(
            app.read_later_target(),
            Some(("en".to_string(), "Alan Turing".to_string())),
            "with nothing focused, the article itself is the target"
        );
    }

    #[test]
    fn read_later_target_falls_back_to_the_article_for_an_external_focused_link() {
        let mut app = app_with_bookmarks();
        let html = r##"<html><body><p><a href="https://example.com/">Ext</a></p></body></html>"##;
        app.set_document(crate::doc::parse_article_html("Alan Turing", html));
        app.active_tab_mut().focused_link = Some(0);

        assert_eq!(
            app.read_later_target(),
            Some(("en".to_string(), "Alan Turing".to_string())),
            "an external link has nothing internal to cache offline"
        );
    }

    #[test]
    fn read_later_target_with_nothing_open_is_none() {
        let app = app_with_bookmarks();
        assert_eq!(app.read_later_target(), None);
    }

    #[test]
    fn readlater_picker_fifo_order_and_auto_dequeue_on_take() {
        let mut app = app_with_bookmarks();
        app.readlater.enqueue(crate::bookmarks::ReadLaterEntry {
            title: "First".to_string(),
            lang: "en".to_string(),
            enqueued_at: crate::bookmarks::now_ts(),
            priority: 0,
        });
        app.readlater.enqueue(crate::bookmarks::ReadLaterEntry {
            title: "Second".to_string(),
            lang: "en".to_string(),
            enqueued_at: crate::bookmarks::now_ts(),
            priority: 0,
        });
        assert_eq!(
            app.readlater
                .entries
                .iter()
                .map(|e| e.title.as_str())
                .collect::<Vec<_>>(),
            vec!["First", "Second"],
            "append order is queue order"
        );

        app.open_readlater_picker();
        assert_eq!(app.selected_readlater, 0);
        assert!(app.readlater_auto_dequeue, "default is auto-dequeue-on");

        let taken = app.take_selected_readlater().expect("an entry to take");
        assert_eq!(taken.title, "First");
        assert_eq!(
            app.readlater.entries.len(),
            1,
            "auto-dequeue must remove it from the queue"
        );
        assert_eq!(app.readlater.entries[0].title, "Second");
    }

    #[test]
    fn readlater_take_without_auto_dequeue_leaves_the_entry_queued() {
        let mut app = app_with_bookmarks();
        app.readlater_auto_dequeue = false;
        app.readlater.enqueue(crate::bookmarks::ReadLaterEntry {
            title: "Keep me queued".to_string(),
            lang: "en".to_string(),
            enqueued_at: crate::bookmarks::now_ts(),
            priority: 0,
        });
        app.selected_readlater = 0;

        let taken = app.take_selected_readlater().expect("an entry to take");
        assert_eq!(taken.title, "Keep me queued");
        assert_eq!(
            app.readlater.entries.len(),
            1,
            "still queued when the config is off"
        );
    }

    #[test]
    fn remove_selected_readlater_deletes_without_returning_it() {
        let mut app = app_with_bookmarks();
        app.readlater.enqueue(crate::bookmarks::ReadLaterEntry {
            title: "Gone".to_string(),
            lang: "en".to_string(),
            enqueued_at: crate::bookmarks::now_ts(),
            priority: 0,
        });
        app.selected_readlater = 0;
        app.remove_selected_readlater();
        assert!(app.readlater.entries.is_empty());
    }

    #[test]
    fn export_bookmarks_writes_the_file_and_requires_a_second_run_to_overwrite() {
        let mut app = app_with_bookmarks();
        app.bookmarks.toggle("en", "Alan Turing", Some(1));

        let dir = std::env::temp_dir().join(format!(
            "wikitui-bookmark-export-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("bookmarks.md");

        app.export_bookmarks_to("md", target.clone());
        let first = std::fs::read_to_string(&target).unwrap();
        assert!(first.contains("Alan Turing"));
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("Exported 1 bookmarks")
        );

        // A hand-edit the export shouldn't silently clobber without a second
        // confirming run — same two-press pattern as `export_bibliography_to`.
        std::fs::write(&target, "hand-annotated").unwrap();
        app.export_bookmarks_to("md", target.clone());
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("run the export again to overwrite"),
            "{:?}",
            app.notice
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hand-annotated");

        app.export_bookmarks_to("md", target.clone());
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains("Alan Turing")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_bookmarks_with_nothing_saved_reports_instead_of_writing() {
        let mut app = app_with_bookmarks();
        app.export_bookmarks("md", None);
        assert!(app.notice.as_deref().unwrap().contains("Nothing to export"));
    }

    #[test]
    fn export_bookmarks_rejects_an_unknown_format() {
        let mut app = app_with_bookmarks();
        app.bookmarks.toggle("en", "Alan Turing", None);
        app.export_bookmarks("carrier-pigeon", None);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("unknown export format")
        );
    }

    // ---- b-/r-prefix dispatch (PRD FR-TB-1, FR-BM-2, FR-BM-3) -------------

    #[test]
    fn b_prefix_dispatches_bb_to_the_tab_picker_and_ba_to_annotate() {
        assert_eq!(resolve_b_prefix('b'), BPrefixAction::TabPicker);
        assert_eq!(resolve_b_prefix('a'), BPrefixAction::Annotate);
        assert_eq!(resolve_b_prefix('x'), BPrefixAction::PassThrough);
    }

    #[test]
    fn r_prefix_dispatches_rl_to_read_later_and_anything_else_to_research() {
        assert_eq!(resolve_r_prefix('l'), RPrefixAction::ReadLater);
        assert_eq!(resolve_r_prefix('r'), RPrefixAction::OpenResearch);
        assert_eq!(resolve_r_prefix('j'), RPrefixAction::OpenResearch);
    }

    // ---- Reading history (PRD FR-HS-1/2/4) --------------------------------

    #[test]
    fn set_document_records_a_visit_with_the_previous_articles_referrer() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_document(doc("Enigma machine"));

        let recent = app.history.recent(10);
        assert_eq!(recent.len(), 2);
        let enigma = recent.iter().find(|v| v.title == "Enigma machine").unwrap();
        assert_eq!(enigma.referrer_lang.as_deref(), Some("en"));
        assert_eq!(enigma.referrer_title.as_deref(), Some("Alan Turing"));
        let turing = recent.iter().find(|v| v.title == "Alan Turing").unwrap();
        assert!(
            turing.referrer_title.is_none(),
            "the first page in a tab has no referrer"
        );
    }

    #[test]
    fn opening_a_document_starts_dwell_tracking_and_replacing_it_flushes() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        let first_id = app.active_tab().history_visit_id;
        assert!(
            first_id.is_some(),
            "a successful install starts dwell tracking"
        );
        assert!(app.active_tab().visit_started_at.is_some());

        // Replacing the document must flush the old visit's dwell (tested
        // numerically in `history::tests::update_dwell_accumulates_
        // across_multiple_calls`) and start fresh tracking for the new one.
        app.open_document(doc("Enigma machine"));
        let second_id = app.active_tab().history_visit_id;
        assert!(second_id.is_some());
        assert_ne!(first_id, second_id, "a new visit gets its own row id");
    }

    #[test]
    fn closing_a_tab_flushes_its_dwell_before_it_is_gone() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        let visit_id = app.active_tab().history_visit_id.unwrap();
        app.new_foreground_tab();
        assert_eq!(app.tabs.len(), 2);

        // Closing tab 0 (still showing "Alan Turing") must not panic and
        // must flush that tab's dwell via `History::update_dwell` before
        // the tab (and its tracking fields) is removed — proven here by
        // the fact that this doesn't panic reaching into a gone tab, and
        // that the row `update_dwell` targeted is still the same one that
        // was recorded.
        app.close_tab(0);
        assert_eq!(app.tabs.len(), 1);
        let recent = app.history.recent(10);
        let turing = recent.iter().find(|v| v.id == visit_id);
        assert!(
            turing.is_some(),
            "the visit row closing the tab flushed dwell for must still exist"
        );
    }

    #[test]
    fn incognito_suppresses_recording_and_dwell_tracking() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.incognito = true;

        app.open_document(doc("Alan Turing"));
        assert!(
            app.history.recent(10).is_empty(),
            "incognito must not record a visit"
        );
        assert!(
            app.active_tab().history_visit_id.is_none(),
            "incognito must not start dwell tracking either"
        );
        assert!(app.active_tab().visit_started_at.is_none());

        // Flipping incognito off (the future FR-PR-3 chunk's whole job) is
        // enough to resume recording — no other state to reset.
        app.incognito = false;
        app.open_document(doc("Enigma machine"));
        assert_eq!(
            app.history.recent(10).len(),
            1,
            "recording resumes once incognito is turned off"
        );
    }

    #[test]
    fn background_tab_load_completion_records_a_visit_too() {
        // Mirrors `tests::background_load_completion_installs_into_the_
        // right_tab_by_id` in main.rs, but at the `App` level: a background
        // tab's fetch landing is "an article successfully renders in a
        // tab" the same as the active tab's own `set_document` (PRD
        // FR-HS-1) — exercised here via the same `record_history_visit`
        // entrypoint `main::apply_tab_load_outcome` calls.
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let id = app.open_background_tab("Enigma machine".to_string(), "en".to_string());
        let index = app.tab_index_by_id(id).unwrap();
        app.tabs[index].install_document(doc("Enigma machine"));
        app.record_history_visit(index, None);

        let recent = app.history.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].title, "Enigma machine");
        assert!(app.tabs[index].history_visit_id.is_some());
    }

    #[test]
    fn open_reading_history_picker_populates_recent_and_resets_selection() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_document(doc("Enigma machine"));
        app.history_pick_selected = 5; // stale from a previous state
        app.open_reading_history_picker();
        assert_eq!(app.mode, Mode::ReadingHistory);
        assert_eq!(app.history_pick_matches.len(), 2);
        assert_eq!(app.history_pick_selected, 0);
    }

    #[test]
    fn refresh_history_matches_filters_by_the_current_input() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_document(doc("Enigma machine"));
        app.history_pick_filter = "enig".to_string();
        app.refresh_history_matches();
        assert_eq!(app.history_pick_matches.len(), 1);
        assert_eq!(app.history_pick_matches[0].title, "Enigma machine");
    }

    #[test]
    fn delete_selected_history_removes_just_that_article() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_document(doc("Enigma machine"));
        app.open_reading_history_picker();
        // most-recent-first: Enigma machine (opened last) is index 0.
        assert_eq!(app.history_pick_matches[0].title, "Enigma machine");
        app.delete_selected_history();
        assert_eq!(app.history_pick_matches.len(), 1);
        assert_eq!(app.history_pick_matches[0].title, "Alan Turing");
    }

    #[test]
    fn clear_history_all_empties_the_store_and_the_open_picker() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_reading_history_picker();
        app.clear_history(crate::command::HistoryClearScope::All);
        assert!(app.history.recent(10).is_empty());
        assert!(app.history_pick_matches.is_empty());
    }
}
