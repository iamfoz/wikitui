use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::api::{SearchResult, TitleSuggestion, WikiRegistryEntry};
use crate::bidi;
use crate::bookmarks::{
    self, Bookmark, BookmarkStore, ReadLaterEntry, ReadLaterStore, ToggleOutcome,
};
use crate::cache::PageCache;
use crate::cite::CiteStyle;
use crate::config::ConfigContext;
use crate::doc::{Citation, Document};
use crate::fetch_queue::FetchQueue;
use crate::hints::{self, HintTarget};
use crate::hyperlink::HyperlinkMode;
use crate::layout::{self, Layout, LayoutCache, LayoutOptions};
use crate::prefetch::FeedCache;
use crate::registry;
use crate::research::{ResearchStore, SavedCitation};
use crate::saved::{SavedPages, Tier};
use crate::session;
use crate::startpage::{self, OnThisDayModel, OtdType, StartPageConfig, StartPageModel};
use crate::tab::{HistoryEntry, Tab, TabId};
use crate::theme::{ColorDepth, LoadedUserTheme, Theme};
use crate::tts::TtsRuntime;

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

/// PRD FR-CS-2's `:open`/`:o` Tab-completion landing slot — a title fetch's
/// `(query, titles)` result, shared between `App::complete_command_tab` and
/// `main::fire_command_typeahead` without a dedicated channel (see
/// `App::command_typeahead_slot`'s own doc comment). Named so both sides
/// spell out the same type once instead of clippy's `type_complexity` lint
/// forcing an inline repeat at every use.
pub type CommandTypeaheadSlot = Arc<Mutex<Option<(String, Vec<String>)>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reading,
    Search,
    Results,
    Toc,
    /// `gs` (PRD FR-NV-2): the fuzzy section jump — a type-to-filter list
    /// over the active tab's section outline, distinct from [`Mode::Toc`]'s
    /// plain browse-everything list the same way [`Mode::Palette`] is
    /// distinct from a picker with a separate `/`-filter submode (mirrors
    /// its single-mode "type narrows, Enter jumps" shape rather than the
    /// `BookmarkPicker`/`BookmarkFilter` two-mode split). Enter jumps to the
    /// highlighted section (`App::jump_to_section`, which already returns to
    /// [`Mode::Reading`]); Esc cancels back to Reading directly, since `gs`
    /// is only ever armed from there.
    SectionJump,
    /// `v` (PRD FR-NV-10): visual-selection text yank. `j`/`k`/arrows extend
    /// the selection from `App::visual_anchor_line` to `App::
    /// visual_cursor_line`; `y` yanks the selected lines' plain text to the
    /// clipboard (the same OSC 52 path `y`/`Y` already use in Reading) and
    /// returns to Reading; Esc cancels without copying anything. Granularity
    /// is whole laid-out (wrapped) lines, not characters — see
    /// `App::enter_visual`'s doc comment for why.
    Visual,
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
    /// `:saved` (PRD §5.7 / FR-OFF-4): the saved-pages browser — a selectable
    /// list of pinned pages showing tier, size, saved date, and integrity
    /// (ok/corrupt). Enter offline-serves the page from the saved store, `d`
    /// un-pins it, Esc closes.
    SavedPicker,
    /// §7's "Offline, uncached link" card: shown when the network is down and
    /// a followed link is neither cached nor saved. `f` queues it for fetch
    /// when online, `s` opens the saved-pages browser, Esc dismisses.
    OfflineCard,
    /// PRD FR-DL-5 / §7's "Redlink followed" card: shown instead of
    /// attempting a fetch when the link just followed is already known not
    /// to exist (see `App::is_redlink`). `s` searches for a similar title,
    /// `y` yanks the wiki's "create this page" URL, Esc dismisses.
    RedlinkCard,
    /// `:prefetch-log` (PRD FR-PF-4): the prefetch transparency/debug panel —
    /// recent prefetch actions with their reason strings, status, and bytes,
    /// plus the live budget state. Read-only; any key / Esc closes it.
    PrefetchLog,
    /// `:interests` (PRD FR-PF-3 / FR-PF-4): the interest-model inspector —
    /// the top topic affinities with their scores, the decay half-life, the
    /// on/off/incognito state, and the current morelike seeds. Read-only,
    /// "the whole model is human-readable"; any key / Esc closes it.
    Interests,
    /// `:stats` (PRD FR-PC-3): the local reading-stats view — articles read,
    /// total time, streaks, and topic distribution. Read-only; any key / Esc
    /// closes it. The same numbers `wikitui stats` prints, shown in the TUI.
    Stats,
    /// `:today` (PRD FR-DL-2): the on-this-day panel — events/births/deaths/
    /// holidays/selected tabs, each a selectable list of entries; Enter opens
    /// the focused entry's linked article, Esc closes. Distinct from the
    /// start page's own condensed on-this-day strip (FR-DL-1), which reuses
    /// the bundled feed rather than this panel's dedicated per-type fetch.
    OnThisDay,
    /// `gR` / `:related` (PRD FR-SR-6): the Related panel for the article
    /// on screen, powered by `morelike:{title}` — a selectable list of
    /// related titles with descriptions; Enter opens one, Esc closes.
    /// Fetched lazily off the event loop (see `main::fire_related`) and
    /// cached per (lang, title) for the session (`App::related_cache`), so
    /// reopening the panel for the same article — or switching back to a
    /// tab that already showed it — costs no second request.
    Related,
    /// `:lang` (bare) — PRD FR-ML-1's language switcher: a fuzzy picker of
    /// the article on screen's langlinks, preferred languages pinned to the
    /// top. Enter switches the active tab to that edition (a real
    /// navigation, pushing history); `/` enters [`Mode::LangFilter`]; Esc
    /// closes. Fetched lazily and session-cached, same idiom as
    /// [`Mode::Related`] — see `App::open_lang_picker`.
    LangPicker,
    /// The language picker's `/` live filter — mirrors
    /// [`Mode::BookmarkFilter`]'s split: every keystroke narrows
    /// `App::lang_filter_input`; Enter/Esc both return to
    /// [`Mode::LangPicker`] with the filter still applied.
    LangFilter,
    /// `Ctrl-p` — PRD FR-CS-1's command palette: a fuzzy list over every
    /// registry command applicable in the context it was opened from, each
    /// row showing display name, current keybinding, and one-line help. Enter
    /// runs the highlighted command, Esc cancels back to `palette_prior_mode`.
    Palette,
    /// PRD FR-CS-8's first-run onboarding: a one-screen tour shown once, over
    /// the start page, when no config file exists yet. Any key dismisses it,
    /// writing the default config so it never shows again.
    Onboarding,
    /// `K` peek popup (PRD FR-NV-4 footnote peek / FR-NV-5 link preview): a
    /// floating card over the reading view showing either a reference's text
    /// (resolved locally, no network) or a link target's summary (title,
    /// description, lead extract). `Ctrl-o`/`Esc` close it; the content lives
    /// in `App::peek`. Drawn like the help/offline overlays, over `draw_reading`.
    Peek,
    /// `i` / `:info` (PRD §10 "content attribution (display)", Appendix B's
    /// `i` "article info/attribution"): a floating card over the reading view
    /// showing the article's title, canonical URL, revision id, license, and
    /// a permalink to its revision history — no network call, everything is
    /// already on the tab. `Esc` closes it; the content lives in `App::info`.
    /// Same overlay idiom as [`Mode::Peek`].
    Info,
    /// PRD §5.9's manual code-paste login prompt: shown when `:login paste`
    /// (or `:login` falling back) asks the reader to paste the authorization
    /// code (or the full redirect URL) after approving in the browser. The
    /// typed input lives in `App::login_input`; Enter submits it for token
    /// exchange, Esc cancels. The loopback path does *not* use this mode — it
    /// completes inside the one blocking `:login` call.
    Login,
    /// `:watchlist` (PRD FR-ACC-2, logged in only): the watchlist pane, two
    /// tabs — the raw watched-pages list and the "what changed" activity
    /// feed since last seen. `Tab`/`h`/`l` switch tabs, `j`/`k` move, Enter
    /// opens the focused entry's article, Esc closes. Mirrors
    /// [`Mode::OnThisDay`]'s two-tab shape.
    Watchlist,
    /// `:notifications` (PRD FR-ACC-3, logged in only): the Echo pane, two
    /// tabs — alerts and messages. `Tab`/`h`/`l` switch, `j`/`k` move, `d`
    /// marks the focused entry read, `A` marks every entry read, Esc closes.
    Notifications,
    /// `:contribs [username]` (PRD FR-ACC-4): a selectable list of recent
    /// edits — the logged-in user's own by default, or any given username
    /// (works logged out too, since `usercontribs` is public). Enter opens
    /// the edited article, `t` thanks the focused edit (logged in only,
    /// FR-ACC-6), Esc closes.
    Contribs,
    /// `:prefs` (PRD FR-ACC-7, logged in only): a read-only card of a
    /// curated subset of `meta=userinfo&uiprop=options` — skin, language,
    /// email-confirmed, edit count. Nothing here is ever written back. Esc
    /// closes. Same overlay idiom as [`Mode::Info`].
    Prefs,
    /// Bare `:wiki` (PRD FR-ML-4): a selectable list of known wikis —
    /// Wikipedia and the four sister projects — highlighting the currently
    /// active one. Enter switches (`main::switch_wiki`), Esc cancels. Local
    /// state only, no network (unlike [`Mode::LangPicker`]'s langlinks
    /// fetch) — the list is the same five entries every time.
    WikiPicker,
    /// `:trail` (PRD FR-HS-3): the wander-graph view — this run's session
    /// history (or a wider scope, `:trail all`/`:trail days N`) rendered as
    /// a navigable, git-log-graph-styled tree (`trail::flatten`'s line
    /// list). `j`/`k` move the selection, Enter reopens the selected
    /// article (wiki-aware — `main::open_trail_node`), Esc closes. Export
    /// (`:trail export md|dot|mermaid [path]`) is a separate command, not a
    /// picker key, and always exports the session scope regardless of what
    /// wider scope this view happens to be showing (see `App::export_trail`).
    Trail,
    /// PRD §7 "Disambiguation page": entered automatically (`App::
    /// set_document`) instead of [`Mode::Reading`] whenever the just-installed
    /// document's `doc::Document::is_disambiguation` is set — a first-class
    /// chooser over `doc::disambiguation_candidates`, never plain prose.
    /// `j`/`k` move the selection, Enter opens the highlighted candidate
    /// (a real navigation, same as any other picker), Esc falls back to
    /// [`Mode::Reading`] to show the raw page — the parsed prose is already
    /// installed in the tab either way, so "cancel" and "show the raw page"
    /// are the same transition.
    Disambig,
}

/// The content of the `K` peek popup (PRD FR-NV-4/FR-NV-5). Two visually
/// distinct kinds share one overlay mode; the draw code branches on this to
/// pick the border label and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeekPopup {
    /// FR-NV-4 footnote peek: a reference marker's resolved text, pulled
    /// locally from the parsed citations — never a network call.
    Footnote { marker: String, text: String },
    /// FR-NV-5 link preview: the target's `(lang, title)`; the body (title,
    /// description, extract) is read live from `App::summary_cache`, showing
    /// "loading…" until the async fetch lands there.
    LinkPreview { lang: String, title: String },
}

/// Where the currently open article's content came from (PRD FR-OFF-6's
/// offline-indicator states): ● fresh from the network, ◐ served from the
/// evictable cache, ○ network failed and a (possibly stale) cached copy stood
/// in, ▣ served from the *pinned* saved-pages store. The ▣ glyph is
/// deliberately distinct from ◐/○ so a pinned saved page (integrity-checked,
/// never evicted — PRD §5.7) reads as different from a best-effort cache hit.
/// ◈ (PRD FR-OFF-8) is distinct again: content read straight out of a local
/// ZIM archive, never fetched or cached at all — see `zim::ZimArchive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSource {
    None,
    Live,
    Cached {
        age_secs: u64,
    },
    Offline {
        age_secs: u64,
    },
    /// Served from the pinned saved-pages store (`saved.rs`). `age_secs` is
    /// time since it was saved.
    Saved {
        age_secs: u64,
    },
    /// Served from a currently loaded ZIM archive (PRD FR-OFF-8). No age or
    /// revid — a ZIM article's only "freshness" is whenever the archive
    /// itself was built, which this module doesn't attempt to surface.
    Zim,
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
            Self::Saved { age_secs } => {
                format!("▣ saved {} ago · ", crate::cache::age_human(*age_secs))
            }
            Self::Zim => "◈ zim · ".to_string(),
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
    /// Index into `tabs` of the tab currently on screen. When a [`split`] is
    /// active this is kept equal to the focused pane's tab index (the module
    /// invariant in `src/split.rs`), so every key handler that routes through
    /// `active_tab()` reaches the focused pane with no changes.
    ///
    /// [`split`]: App::split
    pub active: usize,
    /// PRD FR-TB-4 / FR-ML-3: the live two-pane split overlay, or `None` for
    /// the ordinary single-pane view. Additive — when `None`, the app behaves
    /// exactly as it did before splits existed. See `src/split.rs` for the
    /// panes model.
    pub split: Option<crate::split::Split>,
    /// The `Ctrl-w` window-command chord's pending-key latch (PRD FR-TB-4,
    /// Appendix B `Ctrl-w v`): armed by `Ctrl-w`, resolved by the next key
    /// (`v` split, `w`/`h`/`l` focus, `c`/`o` close) — mirrors `pending_g`.
    pub pending_ctrl_w: bool,
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
    /// PRD FR-SR-4's other zero/poor-result signal: the "showing results for
    /// X" auto-correction the search engine already applied to produce
    /// `results` (`api::SearchOutcome::rewritten_query`) — distinct from
    /// `search_suggestion`'s opt-in "did you mean", and typically present
    /// alongside non-empty `results` rather than only on a zero-result miss.
    pub search_rewritten_query: Option<String>,
    /// PRD FR-SR-4 / §7's "offline-results section": how many local
    /// saved/cached matches the offline index found for the same query when
    /// an *online* search came back with zero results — `run_search`'s own
    /// fallback check, distinct from `results_offline` (which means the
    /// search never even tried the API). `0` when the online search found
    /// something, or hasn't run, or (`run_offline_search`) never applies.
    pub offline_fallback_count: usize,
    /// PRD FR-SR-7: whether `results` came from the local offline index
    /// rather than the API — set by `main::run_offline_search`, consulted
    /// by `ui::draw_results`/`draw_zero_results` (the "(offline)" header
    /// label) and by `Mode::Results`'s Enter handler (routes to
    /// `main::open_offline_result`'s local-only serve instead of
    /// `main::open_title`'s network-first one).
    pub results_offline: bool,
    /// PRD FR-SR-7's explicit `:search-offline` toggle: when set, `run_search`
    /// skips the API entirely and goes straight to the local index, even
    /// while online — independent of the automatic offline/API-failure
    /// fallback, which applies regardless of this flag.
    pub force_offline_search: bool,
    /// PRD FR-SR-3b: the operator cheat-sheet overlay, toggled by `?` while
    /// in `Mode::Search` — deliberately not the generic `Mode::Help` (whose
    /// "any key closes" doesn't fit a prompt still being typed into): only
    /// `?` or Esc close it, every other key is swallowed rather than typed
    /// into `search_input` while the sheet is up.
    pub search_operator_help: bool,
    pub should_quit: bool,
    /// The app-global "current" language for new searches and opens (PRD
    /// FR-ML-1/2, MVP slice). Kept in sync with the active tab's `lang` when
    /// switching tabs; each tab additionally records the language its own
    /// article was fetched in (for history restore and background routing).
    pub lang: String,
    /// PRD FR-ML-4/5: the registry name of the wiki `client`'s host template
    /// currently addresses (`"wikipedia"`, a sister project, or a custom
    /// `[wiki.<name>]` name) — mirrors `client.active_wiki_name()`, kept on
    /// `App` too so status-bar/picker/`:wiki` code that only has `&App` (no
    /// `&WikiClient`) doesn't need one threaded in just to read it.
    pub active_wiki_name: String,
    /// PRD FR-ML-4/5's `:wiki <name>` switch targets, resolved once at
    /// startup (`config::ResolvedWikiRegistry`) and converted to the
    /// `api`-facing shape here: Wikipedia, the four sister projects, and
    /// every `[wiki.<name>]` config section. `main::switch_wiki` looks a
    /// name up here rather than re-reading the config file.
    pub wiki_registry: std::collections::BTreeMap<String, WikiRegistryEntry>,
    /// Selection cursor for the `:wiki` picker (bare `:wiki`, PRD FR-ML-4).
    pub selected_wiki_pick: usize,
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
    /// PRD FR-TH-3: the terminal color depth every theme change degrades
    /// `theme`'s colors to (`set_theme`'s job) — resolved once at startup
    /// from `color_depth = auto|truecolor|256|16|mono` (`main::run`), and
    /// re-read on every `set_theme` call thereafter (`T`-cycling, `:theme`,
    /// `:set theme=`, `:config reload`) rather than only at startup, so a
    /// mid-session theme change still respects it.
    pub color_depth: ColorDepth,
    /// PRD FR-TH-1: user theme files loaded once at startup
    /// (`theme::load_user_themes`) from `$XDG_CONFIG_HOME/wikitui/themes/`.
    /// `set_theme`/`:theme <name>` resolve against this list after the six
    /// built-ins (`theme::resolve_named`); a theme's own declared
    /// `[fallback]` (if any) is what `set_theme` looks up here for
    /// `Theme::adapt`'s precedence-over-computed-quantization rule.
    pub user_themes: Vec<LoadedUserTheme>,
    /// PRD FR-ACS-6 (`ACCESSIBLE=1`): drives the layout's collapse-to-list
    /// table path (and could gate further linear-leaning behavior). Set once
    /// at startup from the `ACCESSIBLE` environment variable; part of
    /// `layout_options`, so toggling it invalidates cached layouts.
    pub accessible: bool,
    /// PRD FR-NV-9: whether mouse capture is currently active. Off by
    /// default (see `config::ResolvedTerminal::mouse`'s doc comment) — set
    /// from config/`:set mouse=on|off` at the same two call sites
    /// (`main::run`, `main::execute_command`) that also toggle the real
    /// `crossterm::event::EnableMouseCapture`/`DisableMouseCapture` terminal
    /// state, so this flag and the terminal's actual capture state can never
    /// drift apart. Every mouse action this flag gates has a keyboard
    /// equivalent that works whether this is true or false (PRD FR-ACS-3's
    /// keyboard-only guarantee: mouse support may only ever be *additive*).
    pub mouse_enabled: bool,
    /// PRD FR-ACS-4: no-motion mode (`animations = none`, `WIKITUI_ANIMATIONS
    /// =none`, or implied by `ACCESSIBLE=1`). There is no smooth-scroll,
    /// spinner, or blink anywhere in this codebase today for this to actually
    /// disable (confirmed: no `Modifier::SLOW_BLINK`/`RAPID_BLINK` use, no
    /// per-frame animation state — loading already renders the static
    /// "Loading…"/"…" text FR-ACS-2 requires) — this flag is nonetheless
    /// real, wired end to end, and is the guard rail any future animation
    /// must check before this flag can be called decorative.
    pub no_motion: bool,
    /// PRD FR-RD-2 / SEC-2: whether/when to emit OSC 8 hyperlinks for links
    /// (`hyperlink::active` resolves `Auto` against the live terminal/
    /// ACCESSIBLE state at emission time in `main::emit_hyperlinks`).
    pub hyperlinks_mode: HyperlinkMode,
    /// PRD FR-ML-7 (experimental RTL): `config bidi = auto|on|off`. Read
    /// alongside [`Self::bidi_terminal_active`] (the resolved, env-checked
    /// verdict `main::emit_bidi_mode` acts on) rather than re-deriving env
    /// state on every draw.
    pub bidi_mode: bidi::BidiMode,
    /// Whether this *session* manages the terminal's bidi mode at all —
    /// `bidi::active(bidi_mode, env_supported)`, resolved once at startup
    /// (`main::run`) from `bidi_mode` plus the `VTE_VERSION`/`TERM` env
    /// heuristic. `ui.rs`'s `bidi::should_app_reorder` reads this to enforce
    /// FR-ML-7's double-reordering hazard gate: app-side `rtl_reorder` must
    /// never engage while this is true.
    pub bidi_terminal_active: bool,
    /// Whether `main::emit_bidi_mode` has already sent
    /// [`bidi::VTE_BIDI_AUTODETECT_ENABLE`] this run — sent lazily, exactly
    /// once, the first time an RTL tab is actually on screen (never
    /// speculatively at startup), so this flips permanently `true` on first
    /// use rather than toggling per tab switch (see that function's own doc
    /// comment for why toggling isn't needed: the escape is a session-wide
    /// "turn on auto-bidi" switch, harmless to leave on while reading LTR
    /// content afterward).
    pub bidi_terminal_sent: bool,
    /// PRD FR-ML-7's **off-by-default** app-side logical→visual reorder
    /// fallback (`config rtl_reorder`). Only actually engages through
    /// `bidi::should_app_reorder`'s gate (`ui::draw_reading`/`paint_pane`),
    /// which also checks [`Self::bidi_terminal_active`] — never this field
    /// alone — to avoid the double-reordering hazard.
    pub rtl_reorder: bool,
    /// PRD FR-NV-9: the reading/content area's rect as of the most recent
    /// draw, so a mouse click's absolute terminal `(row, col)` can be
    /// translated into the reading view's own line/column coordinate space.
    /// One-frame-stale by construction, exactly like `layout_width`/
    /// `viewport_height` above (set during `ui::draw`, consulted by the next
    /// input event) — never the other way around.
    pub last_content_area: ratatui::layout::Rect,
    /// The tab bar's rect as of the most recent draw, or `None` when it isn't
    /// shown (a single open tab never renders one). Same one-frame-stale
    /// contract as `last_content_area`.
    pub last_tab_bar_area: Option<ratatui::layout::Rect>,
    /// Research mode's candidate list for the *active tab's* article: element
    /// 0 is always the article's own citation (`research::self_citation`);
    /// the rest are its extracted References entries, in document order.
    /// Rebuilt whenever the active document changes (open, reload, tab
    /// switch), so Research mode always reflects the tab on screen.
    pub citations: Vec<Citation>,
    pub selected_citation: usize,
    /// The running bibliography, persisted to disk (PRD §6.4 plain files).
    /// Defaults to an in-memory store for the same reason `history` does
    /// (that field's doc comment explains why): dozens of tests build an
    /// `App` via `App::new` and save citations directly, so a real on-disk
    /// default here would make `cargo test` write into the developer's
    /// actual data directory on every run. `main::run` swaps in the real
    /// on-disk store right after construction, same as `history`.
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
    /// PRD FR-CS-2's Tab-completion: the current match list — either
    /// `command::complete_command_name`'s results (cursor still in the
    /// command word) or a landed `:open`/`:o` title-typeahead fetch's
    /// titles. Repeated Tab cycles through this rather than recomputing it
    /// every press.
    pub command_completions: Vec<String>,
    /// Which `command_completions` entry the next Tab installs — also how
    /// `complete_command_tab` recognizes "am I continuing an existing
    /// cycle": if `command_input`'s current word/title already equals
    /// `command_completions[command_completion_index]`, this Tab advances
    /// it; otherwise it's a fresh prefix to match from scratch.
    pub command_completion_index: usize,
    /// PRD FR-CS-2's best-effort `:open`/`:o` title typeahead: the in-flight
    /// fetch's landing slot. A plain `Arc<Mutex<_>>` rather than a new
    /// channel threaded through `handle_key`'s already-long parameter list —
    /// this is an on-demand, Tab-triggered fill, not a per-keystroke
    /// subsystem like Search's own debounced typeahead. `(query, titles)` so
    /// a reply for a title the reader has since typed past is recognizable
    /// as stale, mirroring `typeahead_is_current`'s own staleness check.
    pub command_typeahead_slot: CommandTypeaheadSlot,
    /// Whether a `:open` Tab-completion fetch relevant to the CURRENT
    /// title-so-far is outstanding — set whenever `complete_command_tab`
    /// fires one, cleared only once a result matching that exact prefix is
    /// consulted (a stale reply for a prefix already typed past is
    /// discarded, not treated as resolving this). Unlike
    /// `summary_loading`/`related_loading` this drives no "still loading…"
    /// UI and gates no poll-vs-block decision: nothing changes on screen
    /// until the reader presses Tab again, which is exactly the point where
    /// the slot gets checked anyway.
    pub command_typeahead_loading: bool,
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
    /// PRD §7 "429 / maxlag on interactive request": how many foreground
    /// article-open retries (`main::fire_foreground_retry`) are currently
    /// in flight, mirroring `pending_revalidations`'s "scope the poll
    /// timeout to this being nonzero" role — the retry itself runs on a
    /// detached task honoring `Retry-After`, so this only needs to keep the
    /// main loop waking on a timer instead of blocking on keyboard input
    /// while the countdown runs.
    pub pending_foreground_retries: u32,
    /// PRD §7 "429 / maxlag on interactive request": delivers a foreground
    /// retry's eventual outcome back into the main loop non-blockingly,
    /// applied exactly like a background-tab fetch
    /// (`main::apply_tab_load_outcome`) — the retry never touches this
    /// `App` directly, only this channel, so it can run fully detached from
    /// the render loop. Kept on `App` (rather than threaded as a parameter
    /// through every one of `open_title`'s dozen-plus call chains) since the
    /// one caller that needs it, `main::open_title`, already has `&mut App`
    /// in hand.
    pub foreground_retry_tx: tokio::sync::mpsc::UnboundedSender<crate::TabLoadOutcome>,
    pub foreground_retry_rx: tokio::sync::mpsc::UnboundedReceiver<crate::TabLoadOutcome>,
    /// PRD §7 "Disambiguation page": the highlighted row in the chooser
    /// (`Mode::Disambig`) over `doc::disambiguation_candidates`. Reset to `0`
    /// whenever `set_document` installs a disambiguation page; not persisted
    /// (a fresh chooser always starts at the top, same as every other picker
    /// entered by a document install rather than an explicit open call).
    pub disambig_selected: usize,
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
    /// PRD FR-RD-11's reading-time WPM divisor (`config::resolve_reading_wpm`
    /// / `:set reading_wpm=N`, default 230). Part of `layout_options()`, so
    /// changing it invalidates the cached layout the same way `measure`
    /// does.
    pub reading_wpm: u32,
    /// PRD FR-PC-1's session-global text alignment (`:set text_align=`,
    /// `[reading] text_align` in config, default `center`). A tab's own
    /// `TabOverrides::text_align` wins over this when set — see
    /// `App::layout_options`.
    pub text_align: layout::TextAlign,
    /// PRD FR-PC-1's session-global left margin in cells (`:set margin=`,
    /// `[reading] margin`, default 0).
    pub margin: u16,
    /// PRD FR-PC-1's session-global blank-rows-between-blocks (`:set
    /// paragraph_spacing=`, `[reading] paragraph_spacing`, default 1 —
    /// today's pre-FR-PC-1 behavior).
    pub paragraph_spacing: u8,
    /// PRD FR-PC-1's session-global blank-rows-after-every-wrapped-line
    /// (`:set line_spacing=`, `[reading] line_spacing`, default 0). See
    /// `layout::LayoutOptions::line_spacing`'s doc comment for the honest
    /// "no literal 1.5" framing.
    pub line_spacing: u8,
    /// PRD FR-PC-1's session-global extra inter-word gap width in cells
    /// (`:set word_spacing=`, `[reading] word_spacing`, default 0).
    pub word_spacing: u8,
    /// PRD FR-RD-9's session-global full justification toggle (`:set
    /// justify=on|off`, `[reading] justify`, default false — ragged-right).
    /// A tab's own `TabOverrides::justify` wins over this when set.
    pub justify: bool,
    /// PRD FR-RD-9's session-global soft-hyphenation toggle (`:set
    /// hyphenate=on|off`, `[reading] hyphenate`, default false). A tab's own
    /// `TabOverrides::hyphenate` wins over this when set.
    pub hyphenate: bool,
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
    /// quality-M2: bounds how many inline/POTD image fetches run at once
    /// (`main::request_visible_images`/`request_start_page_image`) — see
    /// `image::new_fetch_limiter`'s doc comment. One per `App` so every image
    /// fetch this session spawns shares the same cap.
    pub image_fetch_limiter: std::sync::Arc<tokio::sync::Semaphore>,
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
    /// Defaults to an in-memory store — same "`App::new` empty, `main::run`
    /// installs the real one" split as `history`/`research` (see `history`'s
    /// doc comment for why): plenty of tests bookmark a title directly
    /// through `App::new`, and a real on-disk default would make `cargo
    /// test` write into the developer's actual data directory.
    pub bookmarks: BookmarkStore,
    /// The read-later queue (PRD FR-BM-3/7), persisted to disk. Same
    /// in-memory-by-default split as `bookmarks`.
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
    /// PRD FR-PR-3's incognito gate — the flag every persistence write in
    /// this module consults via `crate::privacy::decide` before touching
    /// disk (see that module's doc comment for the full policy). Set from
    /// `--incognito` (`cli::Cli::incognito`) at startup, and flipped live at
    /// runtime by `zz` (`resolve_z_prefix`, `main::handle_key`).
    pub incognito: bool,
    /// The `z`-prefix chord's pending-key latch (mirrors `pending_g`/
    /// `pending_b`): `zz` toggles incognito (PRD FR-PR-3, Appendix B). Any
    /// other second key falls through unhandled (there is no other `z`
    /// binding yet — section folding's `za`/`zM`/`zR`, FR-NV-3, is a later
    /// chunk; `resolve_z_prefix` already has a `PassThrough` arm ready for
    /// them).
    pub pending_z: bool,
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
    /// Unix seconds this run started (`history::now_unix()`, set once in
    /// `App::new` and never touched again) — the default cutoff `:trail`'s
    /// session scope filters on (PRD FR-HS-3: "session-scope is the natural
    /// wander graph"). Deliberately a wall-clock timestamp, not a
    /// process-lifetime flag, so it survives being read alongside
    /// `history::Visit::opened_at` (also unix seconds) without a unit
    /// conversion.
    pub session_started_at: i64,

    // -- Trail / wander-graph view (PRD FR-HS-3) ----------------------------
    /// The currently-built trail (`trail::build`), rebuilt fresh every time
    /// `open_trail`/`open_trail_dag` runs — never mutated incrementally,
    /// since a trail is a point-in-time snapshot of the history table, not a
    /// live-updating view. Defaults to an empty trail before `:trail` is
    /// ever opened. `trail.graph` alone is enough to render either layout
    /// (`trail.tree` for `TrailLayout::Tree`, `trail::dag_from_graph(&trail.
    /// graph)` computed on demand for `TrailLayout::Dag` — the same "don't
    /// cache the flattened form" posture `trail::flatten` already has for
    /// the tree).
    pub trail: crate::trail::Trail,
    /// Which of FR-HS-3's two layouts `:trail`/`:trail dag` last opened
    /// (PRD: "v1.x ships tree layout; true DAG layout is v2"). Tree is the
    /// default; `open_trail`/`open_trail_dag` are the only two writers.
    pub trail_layout: crate::trail::TrailLayout,
    /// Selection cursor into whichever line list `trail_layout` currently
    /// renders (`trail::flatten(&self.trail.tree)` for `Tree`,
    /// `trail::dag_from_graph(&self.trail.graph).nodes` for `Dag`).
    pub trail_selected: usize,
    /// The mode `open_trail`/`open_trail_dag` was entered from, restored on
    /// Esc.
    pub trail_prior_mode: Mode,
    /// Export-overwrite confirmation for `:trail export`, mirroring
    /// `pending_bookmark_export_overwrite`.
    pub pending_trail_export_overwrite: Option<std::path::PathBuf>,

    // -- Saved pages, bulk save, offline card (PRD §5.7, FR-OFF-4..7) -------
    /// The pinned saved-pages store (PRD FR-OFF-4). Distinct from the
    /// evictable `PageCache` in `main` — see `saved.rs`. Defaults to an
    /// in-memory store, same "`App::new` empty, `main::run` installs the
    /// real one" split as `history`/`search_index` (see `history`'s doc
    /// comment for why).
    pub saved: SavedPages,
    /// PRD FR-SR-7's local full-text index over saved + cached pages — see
    /// `offline_search`'s module doc. Defaults to an in-memory store for the
    /// same reason `history` does (its doc comment explains why): several
    /// `main.rs` functions index as a side effect of ordinary navigation
    /// (opening or saving an article), so a real on-disk default here would
    /// make `cargo test` write into the developer's actual cache directory
    /// on every test that opens a document through those paths.
    /// `main::run` swaps in the real on-disk index right after construction,
    /// same as `history`. `Clone`-able (an `Arc`-backed handle) so `main.rs`
    /// also keeps its own copy to pass into spawned background tasks that
    /// have no `&mut App` to read this field from.
    pub search_index: crate::offline_search::OfflineIndex,
    /// Selection cursor for the `:saved` browser.
    pub selected_saved: usize,
    /// The mode `open_saved_picker` was entered from, restored on Esc.
    pub saved_prior_mode: Mode,
    /// The offline fetch queue (PRD FR-OFF-6). Populated by the offline-card's
    /// `f`, drained by `:fetch-queue`. Defaults to an in-memory store, same
    /// "`App::new` empty, `main::run` installs the real one" split as
    /// `saved`/`history` (see `history`'s doc comment for why).
    pub fetch_queue: FetchQueue,
    /// The `(lang, title)` the offline card is currently offering to queue or
    /// find in saved pages — `Some` exactly while `mode == Mode::OfflineCard`.
    pub offline_card_target: Option<(String, String)>,
    /// PRD FR-OFF-8: the currently loaded Kiwix ZIM archive, if any —
    /// `--zim <path>`/`:zim open <path>` populate it, `:zim close` clears
    /// it. At most one archive at a time (this chunk doesn't attempt
    /// multi-archive sessions); `None` is the default, identical-to-before
    /// behavior every existing call site sees.
    pub zim: Option<crate::zim::ZimArchive>,
    /// PRD FR-DL-3: per-`(lang, title)` quality-class cache for the session.
    /// The current article's status-bar badge (`current_quality_badge`) and a
    /// batch of search-result rows (`quality_badge_for`) both read from this
    /// one map, so a title looked up once — whichever path touched it first —
    /// never re-fetches for the rest of the session. Populated by
    /// `main::open_title`/`open_history_entry` (one title) and `main::
    /// run_search` (one batched call for every result row) — never a
    /// per-article fanout (§6.2 rule 10 / NF-NET-5).
    pub quality_cache: HashMap<(String, String, String), crate::api::QualityClass>,
    /// PRD FR-DL-5: `(lang, title)` pairs the batched `generator=links&
    /// prop=info` missing-flag check has confirmed don't exist — the
    /// async-checked complement to a `LinkRef`'s own parse-time `redlink`
    /// flag (Parsoid's `class="new"` pre-marking). Consulted at paint time
    /// (`ui::confirmed_redlink_titles`, layered on exactly like visited-link
    /// coloring) and by `is_redlink` before following a link, so a redlink
    /// this session already confirmed never even attempts a fetch that would
    /// just 404 a second time.
    pub confirmed_redlinks: HashSet<(String, String, String)>,
    /// PRD FR-DL-5: `(lang, title)` source articles `enrich_article`'s
    /// batched `generator=links&prop=info` check has already run for this
    /// session — a source with zero redlinks would otherwise leave no trace
    /// in `confirmed_redlinks` and get re-checked on every revisit.
    pub checked_redlink_sources: HashSet<(String, String, String)>,
    /// The `(lang, title)` the redlink card (PRD FR-DL-5 / §7 "Redlink
    /// followed") is currently showing — `Some` exactly while `mode ==
    /// Mode::RedlinkCard`. Mirrors `offline_card_target`'s shape.
    pub redlink_card_target: Option<(String, String)>,
    /// A bulk save awaiting the reader's y/n confirmation (PRD FR-OFF-5's cost
    /// preview): the resolved target list and tier, held until `y` proceeds.
    pub pending_bulk_save: Option<BulkSaveRequest>,
    /// How many save fetches are in flight (single or bulk). Keeps the event
    /// loop on its scoped-poll path (like `pending_revalidations`) so save
    /// completions land without a keypress; decremented per applied outcome.
    pub pending_saves: u32,
    /// Export-overwrite confirmation for `:save export`, mirroring
    /// `pending_bookmark_export_overwrite`.
    pub pending_saved_export_overwrite: Option<std::path::PathBuf>,

    // -- Prefetch (PRD §5.8, FR-PF-1..6) -----------------------------------
    /// The background substrate handle (PRD §5.8). `None` in tests and any
    /// path that never spins the runtime; `main::run` installs the real one.
    /// The app reads it for the `:prefetch-log` panel (FR-PF-4) and toggles it
    /// for the `:set prefetch` kill switch (FR-PF-6).
    pub prefetch: Option<crate::netqueue::SubstrateHandle>,
    /// The mode `open_prefetch_log` was entered from, restored on close.
    pub prefetch_prior_mode: Mode,
    /// The `:prefetch-log` panel's scroll offset (PRD FR-CS-4-style fix, UX-7):
    /// read-only inspector panels must scroll their overflow like the `?` help
    /// overlay does, rather than dismiss on the very key that was meant to
    /// scroll them. Reset to 0 on every `open_prefetch_log`.
    pub prefetch_log_scroll: u16,

    // -- Interest model + reading stats (PRD FR-PF-3, FR-PC-3, FR-PR-2) -----
    /// The local, private interest-affinity model (`interest::InterestModel`).
    /// `App::new` defaults to an empty in-memory model (like `history`) so
    /// tests never touch the real state dir; `main::run` loads the on-disk
    /// `interest.json` and sets `interest_path` to enable persistence.
    pub interest: crate::interest::InterestModel,
    /// PRD FR-PF-3 config `interest_learning` (default on). Whether reading
    /// signals update the model at all — see `interest_active` for how it
    /// combines with incognito (which disables learning regardless).
    pub interest_learning: bool,
    /// Where the interest model persists (`$XDG_STATE/wikitui/interest.json`).
    /// `None` in tests (no persistence); `Some` in `main::run`. `persist_
    /// interest` writes only when this is set, mirroring how `session_path`
    /// gates session writes.
    pub interest_path: Option<std::path::PathBuf>,
    /// The mode `open_interests` was entered from, restored on close.
    pub interest_prior_mode: Mode,
    /// The `:interests` panel's scroll offset — see `prefetch_log_scroll`'s
    /// doc comment for why this exists (UX-7). Reset to 0 on `open_interests`.
    pub interests_scroll: u16,
    /// The mode `open_stats` was entered from, restored on close.
    pub stats_prior_mode: Mode,
    /// The `:stats` view's scroll offset — see `prefetch_log_scroll`'s doc
    /// comment for why this exists (UX-7). Reset to 0 on `open_stats`.
    pub stats_scroll: u16,

    // -- Start page, on-this-day panel, TIL widget (PRD FR-DL-1,2,7) -------
    /// The daily Wikifeeds cache, shared with the background substrate's
    /// executor (`main::BgExecutor`) via the same `Arc<Mutex<_>>` — the "one
    /// daily call" seam `prefetch::FeedCache::get` documents. `None` in tests
    /// and any path that never spins the runtime, exactly like `prefetch`
    /// above; `main::run` installs the real one.
    pub feed_cache: Option<Arc<Mutex<FeedCache>>>,
    /// `startpage = feed|blank|resume` (PRD FR-DL-1), resolved once at
    /// startup from `config::ResolvedConfig::startpage`.
    pub startpage_config: StartPageConfig,
    /// Selection cursor into the current `start_page_model()`'s flat item
    /// list — rebuilt fresh each draw, so this index (not an item reference)
    /// is what persists across frames.
    pub start_selected: usize,
    /// FR-DL-7's "or on a key" TIL rotation: bumped by the start page's `t`
    /// binding, folded into `startpage::pick_til`'s index alongside the
    /// day's date so a reroll changes the fact without waiting for tomorrow.
    /// Session-only (never persisted) — a fresh launch starts back at the
    /// day's plain deterministic pick.
    pub start_til_reroll: u64,
    /// The `:today` panel's fetched entries, one list per type (FR-DL-2).
    pub otd: OnThisDayModel,
    /// Which type tab the panel is showing.
    pub otd_tab: OtdType,
    /// Selection cursor into `otd.entries(otd_tab)` — reset to 0 whenever
    /// the tab changes (a stale index into a different type's list would be
    /// meaningless).
    pub otd_selected: usize,
    /// The mode `open_on_this_day` was entered from, restored on close.
    pub otd_prior_mode: Mode,

    // -- Related panel (PRD FR-SR-6) ----------------------------------------
    /// `morelike:{title}` results, session-cached per `(lang, title)` so the
    /// panel never re-fetches for an article it has already shown this
    /// session (switching tabs, reopening the panel, revisiting the
    /// article). Never persisted — a fresh launch starts empty.
    pub related_cache: HashMap<(String, String, String), Vec<SearchResult>>,
    /// Selection cursor into `App::related_items()`.
    pub selected_related: usize,
    /// The mode `open_related` was entered from, restored on close.
    pub related_prior_mode: Mode,
    /// A `morelike:` fetch for the panel's current article is in flight
    /// (PRD FR-SR-6's "fetched lazily... don't block"): `main::fire_related`
    /// spawns it off the event loop and `App::deliver_related` clears this
    /// once the result (or failure) lands.
    pub related_loading: bool,

    // -- Language switcher & fallback chain (PRD FR-ML-1/2) -----------------
    /// The reader's preferred languages, in configured order (`config
    /// languages = [...]`) — pins the `:lang` picker's matching rows to the
    /// top (FR-ML-1) and is the fallback chain `main::open_title` walks when
    /// a plain-title open's own language turns up nothing (FR-ML-2). Empty
    /// when unconfigured, in which case both features are simply inert: one
    /// attempt, nothing pinned, same as before this chunk.
    pub languages: Vec<String>,
    /// An article's langlinks, session-cached per `(lang, title)` — the
    /// *source* article's identity, not the target edition's — mirroring
    /// `related_cache`'s shape and reasoning: switching tabs, reopening the
    /// picker, or revisiting the article later are all cache hits. Never
    /// persisted.
    pub langlinks_cache: HashMap<(String, String, String), Vec<crate::api::LangLink>>,
    /// Selection cursor into `App::lang_picker_rows()`.
    pub selected_lang: usize,
    /// The mode `open_lang_picker` was entered from, restored on close.
    pub lang_prior_mode: Mode,
    /// The picker's live `/` filter input, matched against autonym, English
    /// langname, or code (PRD FR-ML-1).
    pub lang_filter_input: String,
    /// A langlinks fetch for the picker's current article is in flight,
    /// mirroring `related_loading`.
    pub lang_loading: bool,
    /// How many langlinks fetches are currently in flight — both the
    /// picker's own (`lang_loading` is the UI-facing subset of this) and
    /// the automatic one `main::open_title` fires after every fresh open to
    /// power the preferred-language hint below. Unlike `lang_loading`, this
    /// counter is what keeps the event loop's scoped-poll path awake (PRD
    /// FR-ML-2's hint must still land without a keypress) — mirrors
    /// `pending_revalidations`/`pending_saves` exactly: incremented at each
    /// fetch's dispatch site in `main.rs`, decremented where its result is
    /// drained.
    pub pending_langlinks: u32,
    /// PRD FR-ML-2's "available in your preferred language" hint: set by
    /// `refresh_language_hint` whenever fresh langlinks land for whatever is
    /// currently on screen; `None` when nothing to suggest (no preferred
    /// languages configured, the article is already in one, or none of its
    /// langlinks point at one). Rendered as a low-priority status-bar
    /// suffix — see `ui::draw_status_bar` — never a takeover notice.
    pub language_hint: Option<String>,
    /// PRD FR-CS-1/3/4's command registry + active keymap: the single source
    /// of truth the palette, the help overlay, and any user/preset key
    /// rebinding read from. The built-in vim default keeps its override layer
    /// empty, so `handle_key`'s hardcoded arms stay the dispatch path for
    /// every default binding — a preset or `keymap.toml` populates the
    /// overrides that `handle_key` consults ahead of them.
    pub keymap: registry::Keymap,
    /// The `Ctrl-p` command-palette query the user is typing (PRD FR-CS-1).
    pub palette_input: String,
    /// Which palette match Enter runs (index into the live fuzzy-filtered rows).
    pub palette_selected: usize,
    /// The mode `Ctrl-p` was pressed from, restored on Esc and used to scope
    /// which commands the palette offers (its [`registry::KeyContext`]).
    pub palette_prior_mode: Mode,
    /// PRD FR-NV-2's `gs` fuzzy section jump: the type-to-filter query, an
    /// `App`-level field (not per-tab, like [`Tab::selected_section`] is)
    /// because it's transient picker input, the same shape `palette_input`
    /// already established.
    pub section_jump_input: String,
    /// Which fuzzy-filtered section row Enter jumps to (index into
    /// `App::section_jump_rows`'s live list, not `Tab::sections` directly —
    /// mirrors `palette_selected`).
    pub section_jump_selected: usize,
    /// PRD FR-NV-10's visual-selection yank: the line the selection was
    /// anchored at (`App::enter_visual`) — absolute indices into the
    /// current layout's `lines`, the same coordinate space `Tab::scroll`
    /// itself uses. Paired with `visual_cursor_line` to form the selected
    /// range (`App::visual_selected_range`).
    pub visual_anchor_line: usize,
    /// The line `j`/`k`/arrows have moved the visual cursor to since
    /// entering (`App::visual_move`) — starts equal to `visual_anchor_line`.
    pub visual_cursor_line: usize,
    /// The scroll offset of the `?` help overlay (PRD FR-CS-4): the sheet is
    /// scrollable so it can never clip, however tall the terminal.
    pub help_scroll: u16,

    // -- K peek popup (PRD FR-NV-4 footnote peek / FR-NV-5 link preview) -----
    /// The open peek popup's content, or `None` when no popup is up. Set by
    /// `open_peek_at_focus`, cleared by `close_peek`.
    pub peek: Option<PeekPopup>,
    /// The mode `K` was pressed from, restored when the popup closes.
    pub peek_prior_mode: Mode,
    /// FR-NV-5 link-target summaries, session-cached per `(lang, title)` —
    /// shares the same "fetched lazily, never blocks, cached for the session"
    /// idiom as `related_cache`/`langlinks_cache`, and doubles as the peek's
    /// loading indicator (absent entry = still loading). Never persisted.
    pub summary_cache: HashMap<(String, String, String), crate::api::SummaryData>,
    /// A link-preview summary fetch for the open popup is in flight (PRD
    /// FR-NV-5's "don't block; show loading… then fill") — mirrors
    /// `related_loading` for the scoped-poll keep-awake.
    pub summary_loading: bool,

    // -- `i` / `:info` article-attribution overlay (PRD §10, Appendix B) ----
    /// The open `:info` overlay's content, or `None` when it isn't up. Set
    /// by `open_info`, cleared by `close_info`. Unlike `peek`, this is never
    /// partially filled while loading — everything it needs is already on
    /// the tab, so there is nothing to fetch.
    pub info: Option<crate::attribution::ArticleAttribution>,
    /// The mode `i`/`:info` was opened from, restored when the overlay closes.
    pub info_prior_mode: Mode,

    // -- Reading-position memory (PRD FR-NV-8) ------------------------------
    /// Set right after installing a document the reader has an earlier saved
    /// position for (`History::position`), when that position isn't the top —
    /// drives the non-blocking "resume at §… (r)" toast and is consumed by
    /// `resume_to_saved_position` when the reader presses `r`. Cleared the
    /// moment any other key dismisses the toast.
    pub pending_resume: Option<ResumePosition>,
    /// Suppresses the resume toast for the very next `set_document` — set by
    /// back/forward navigation and the SWR reload, which restore their own
    /// scroll and must not also raise the cross-session resume prompt.
    pub suppress_resume_once: bool,

    // -- Session auto-restore (PRD FR-TB-5) ---------------------------------
    /// Where `persist_session` writes (`$XDG_STATE_HOME/wikitui/session.json`
    /// — see `session::resolve_session_path`), or `None` when no platform
    /// state directory could be determined (matches `history_path`'s own
    /// "silently don't persist" degradation) or a test hasn't set one.
    /// `App::new` leaves this `None` deliberately — every existing
    /// `App::new` test call site is unaffected by session writes unless it
    /// opts in by setting this field, the same convention `bookmarks`/
    /// `saved`/`history` already use (in-memory by default; `main::run`
    /// is the one place that installs the real on-disk path).
    pub session_path: Option<PathBuf>,
    /// Scroll/fold-set to restore once a session-restore background fetch
    /// (PRD FR-TB-5) lands, keyed by the tab it belongs to — `install_document`
    /// always resets a tab's scroll/folds to the top, so these can't be
    /// applied until *after* that reset, once `main::apply_tab_load_outcome`
    /// sees the fetch complete. An entry lingers harmlessly if its tab
    /// closes or its fetch fails before landing (removed either way by the
    /// call site, never read again).
    pub pending_session_restore: HashMap<TabId, PendingSessionRestore>,

    // -- TTS piping (PRD FR-PC-2, SEC-5) ------------------------------------
    /// The resolved `tts_command` config (`main::run` sets this once at
    /// startup from `ResolvedConfig::tts_command`); `None` means TTS is
    /// disabled — `main::start_tts_playback` reports a notice rather than
    /// silently doing nothing. `App::new` leaves this `None`, the same
    /// in-memory-default convention `session_path`/`auth` already use.
    pub tts_command: Option<String>,
    /// Shared playback-control state (see `TtsRuntime`'s doc comment for the
    /// full threading story) — cloned onto every spawned playback task so
    /// `:tts stop` (or starting a fresh play) can invalidate whatever is
    /// currently running.
    pub tts: TtsRuntime,
    /// Whether a playback loop is believed to be running — informational
    /// only (status bar / `:tts stop`'s messaging), not consulted by the
    /// playback loop itself (which reads `tts`'s generation counter, not
    /// this flag). Set `true` by `main::start_tts_playback`, `false` by
    /// `main::stop_tts_playback`; there is no completion callback that
    /// clears it automatically once a full article finishes speaking on its
    /// own (a documented, narrow gap — see `main::start_tts_playback`'s doc
    /// comment), so a `:tts stop` after natural completion is a harmless,
    /// silent no-op rather than an error.
    pub tts_playing: bool,

    // -- Command-sequence macros (PRD FR-CS-5) ------------------------------
    /// Every `[command.<name>]` macro from config, name to its ordered step
    /// list (`main::run` sets this once at startup from `ResolvedConfig::
    /// macros`). `App::new` leaves this empty, the same in-memory-default
    /// convention every other config-sourced `App` field above uses.
    pub macros: std::collections::BTreeMap<String, Vec<String>>,

    // -- OAuth login & tokens (PRD §5.9, FR-ACC-1/9, SEC-4) -----------------
    /// The live logged-in session, or `None` when logged out (PRD FR-ACC-1).
    /// Loaded from the token store at startup (`main::run`) so the indicator
    /// shows immediately without a network round trip; set by `:login`,
    /// cleared by `:logout`. `App::new` leaves it `None` — every existing
    /// `App::new` test call site stays logged out, the same in-memory-default
    /// convention `history`/`session_path` use.
    pub auth: Option<crate::auth::AuthState>,
    /// A login flow in progress (PRD §5.9): the PKCE verifier + CSRF state +
    /// redirect URI generated when `:login` built the authorization URL, held
    /// until the manual-paste [`Mode::Login`] prompt resolves. `None` outside
    /// a login. The loopback path never populates this — it completes inside
    /// the one blocking `:login` call — so this is exactly the manual-paste
    /// fallback's state.
    pub pending_login: Option<PendingLogin>,
    /// The manual-paste prompt's in-progress input ([`Mode::Login`]).
    pub login_input: String,
    /// PRD §5.9 / FR-ACC-1: the OAuth client identity + endpoints `:login`
    /// needs to start a flow (the token host is distinct from the per-`{lang}`
    /// wiki host, so it can't be derived from `App::lang`). `App::new`
    /// defaults it to "no consumer registered, Meta-Wiki endpoints"; `main::
    /// run` overrides it from `[auth]` config, the same convention
    /// `history`/`session_path` use for their real values.
    pub auth_runtime: AuthRuntime,

    // -- Watchlist (PRD FR-ACC-2) --------------------------------------------
    /// The raw watched-pages list, refetched fresh every time the pane opens
    /// (so a `w` toggle followed by reopening the pane always shows the
    /// server's current state — no client-side cache to drift out of sync).
    pub watchlist_raw: Vec<String>,
    /// The "what changed" activity feed: changes to watched pages since
    /// `watchlist_last_seen`, computed by `account::changes_since` at fetch
    /// time.
    pub watchlist_changes: Vec<crate::account::WatchlistChange>,
    /// Which of the watchlist pane's two tabs is focused.
    pub watchlist_tab: WatchlistTab,
    /// Selection cursor into whichever list `watchlist_tab` names — reset to
    /// 0 on every tab switch and every fresh open.
    pub watchlist_selected: usize,
    /// The mode `:watchlist` was opened from, restored on close.
    pub watchlist_prior_mode: Mode,
    /// The last-seen timestamp loaded from `$XDG_STATE_HOME/wikitui/
    /// watchlist.json` at startup (`main::run`) — `None` until the pane has
    /// ever been opened once. `App::new` leaves this `None`, matching every
    /// other on-disk store's in-memory-default test convention.
    pub watchlist_last_seen: Option<String>,
    /// Where `watchlist_last_seen` persists to (`account::watchlist_state_path`),
    /// or `None` when no platform state directory resolves. `App::new` leaves
    /// this `None` — `main::run` installs the real path.
    pub watchlist_state_path: Option<PathBuf>,

    // -- Notifications / Echo (PRD FR-ACC-3) --------------------------------
    /// The alerts list, populated when `:notifications` opens.
    pub notif_alerts: Vec<crate::account::Notification>,
    /// The messages list, populated when `:notifications` opens.
    pub notif_messages: Vec<crate::account::Notification>,
    /// Which of the notifications pane's two tabs is focused.
    pub notif_tab: NotifTab,
    /// Selection cursor into whichever list `notif_tab` names.
    pub notif_selected: usize,
    /// The mode `:notifications` was opened from, restored on close.
    pub notif_prior_mode: Mode,
    /// The unread-count badge's source of truth (PRD FR-ACC-3): fetched at
    /// login/startup and on opening the pane (see `account.rs`'s poll-cadence
    /// doc comment), then kept current locally by mark-read/mark-all-read
    /// without a further network call. Zero (the default) shows no badge.
    pub notif_counts: crate::account::NotifCounts,

    // -- Contributions (PRD FR-ACC-4) ---------------------------------------
    /// The contributions list for `contribs_username`.
    pub contribs: Vec<crate::account::Contribution>,
    /// Whose contributions are on screen — the logged-in user by default, or
    /// whatever username `:contribs <name>` gave.
    pub contribs_username: String,
    /// Selection cursor into `contribs`.
    pub contribs_selected: usize,
    /// The mode `:contribs` was opened from, restored on close.
    pub contribs_prior_mode: Mode,

    // -- CSRF / watch tokens (PRD §6.2 rule 8) ------------------------------
    /// Every write action (`w`, thank, mark-read) shares one cache so a
    /// session's csrf/watch tokens are fetched at most once each — cleared
    /// on `:logout` (`cmd_logout`), since a new session needs its own.
    pub tokens: crate::account::TokenCache,

    // -- Typo-fix editing (PRD FR-ACC-8, gated) -----------------------------
    /// The config half of the editing double opt-in gate (`editing_enabled`,
    /// default false). `:enable-editing` flips it on for the session; the
    /// config key persists it. Even `true` never enables writing on its own —
    /// the `editpage` OAuth grant (`AuthState::has_editpage`) is the required
    /// second half (see `editing::edit_gate`).
    pub editing_enabled: bool,
    /// A prepared, unsaved edit awaiting the reader's explicit confirmation
    /// (PRD FR-ACC-8's "REQUIRE explicit confirmation before saving"). `Some`
    /// only between `:edit`'s $EDITOR step and the `y`/`n` diff-preview
    /// decision; the save request is fired ONLY from the `y` branch, so there
    /// is structurally no path that saves without a confirm.
    pub pending_edit: Option<PendingEdit>,

    // -- Preferences (PRD FR-ACC-7, read-only) ------------------------------
    /// The curated prefs card's content, `None` until `:prefs` has fetched
    /// once.
    pub prefs: Option<crate::account::UserPrefs>,
    /// The mode `:prefs` was opened from, restored on close.
    pub prefs_prior_mode: Mode,

    // -- Reading List sync (PRD FR-BM-5) / watchlist mirror (PRD FR-BM-6) --
    /// Config `watchlist_mirror_tag` (default `"watched"`): which bookmark
    /// tag `:sync`/`:mirror-watchlist` mirrors to the real watchlist.
    pub watchlist_mirror_tag: String,
    /// Where the Reading List sync's id-map state persists
    /// (`account::readinglist_sync_state_path`), loaded once at startup like
    /// `watchlist_state_path`; `None` when no state directory resolves.
    pub readinglist_sync_state_path: Option<PathBuf>,
    /// Where the watch-mirror's own "what did *we* watch" state persists
    /// (`account::watch_mirror_state_path`) — see `account::watch_mirror_
    /// diff`'s doc comment for why this can't just be re-derived from the
    /// live watchlist.
    pub watch_mirror_state_path: Option<PathBuf>,

    // -- Delight & discovery (PRD §5.16: FR-DL-4/6/8) -----------------------
    /// PRD FR-DL-4's `:set show-cn` toggle: whether citation-needed markers
    /// paint dim and become `]c`/`[c`-navigable and status-bar-counted.
    /// Default off (opt-in) — a `SpanStyle::CitationNeeded` span always
    /// parses and always renders its canonical text either way (see
    /// `ui::kind_style`'s doc comment); this only controls the highlighting.
    pub show_cn: bool,
    /// The `]`/`[`-prefix chord's pending-key latch for FR-DL-4's `]c`/`[c`
    /// jump — armed only while `show_cn` is on (see `main::handle_key`'s `]`/
    /// `[` arms), so the bare keys' existing table-scroll meaning
    /// (`App::scroll_tables`) is completely untouched while the feature is
    /// off, its default. Mirrors `pending_r`'s "the prefix key has its own
    /// standalone meaning" shape (`app::RPrefixAction`) rather than `pending_g`'s
    /// "dead prefix" shape, since `]`/`[` alone are never dead.
    pub pending_cn_bracket: Option<CnBracket>,
    /// PRD FR-DL-6's in-progress (or just-finished) wiki-walk. `None` means
    /// ordinary, unrestricted reading. While `Some` and not yet won,
    /// navigation is restricted to link-follows (`main::execute_command`'s
    /// game guard, and the `/`/`gr`/`gR` key guards) — see `game`'s module
    /// doc for the full rules.
    pub game: Option<crate::game::GameState>,
    /// PRD FR-DL-8's "fires once, not every nav" rule: the id of every
    /// achievement (`achievements::ACHIEVEMENTS`) already toasted this
    /// session. Never cleared — an achievement is a one-time session event,
    /// not a repeating notification.
    pub achievements_shown: std::collections::HashSet<&'static str>,
    /// PRD FR-DL-8's `pro = true` config: disables every easter egg
    /// (`:xyzzy`) and achievement toast outright. Default `false` (the
    /// egg/toast behavior is the default experience); `main::run`/
    /// `apply_config_reload` set this from `[pro]`... — see
    /// `config::ResolvedConfig::pro`'s own doc comment for the exact key.
    pub pro: bool,
    /// PRD FR-DL-3 v2's `liftwing = true` config: whether `enrich_article`
    /// even attempts the Lift Wing ML-quality fallback once a wiki's
    /// `pageassessments` capability is off. Default `false` — see
    /// `config::ResolvedConfig::liftwing`'s own doc comment for why
    /// (gateway survival unverified, SP-4).
    pub liftwing_enabled: bool,
    /// The Lift Wing gateway endpoint template (§6.2 rule 2), from
    /// `config::ResolvedConfig::liftwing_base_url` — see that field's doc
    /// comment. Set at startup and re-applied on `:config reload`
    /// (`main::apply_config_reload`), same as `liftwing_enabled`.
    pub liftwing_base_url: String,
}

/// Which bracket armed FR-DL-4's citation-needed jump chord (`]c`/`[c`) —
/// `]` jumps forward, `[` jumps backward; a second key other than `c`
/// resolves to that bracket's own ordinary meaning (`App::scroll_tables`),
/// mirroring `RPrefixAction`'s "the prefix key has a standalone meaning, so
/// an unrecognized second key still performs it" shape rather than `g`/`b`/
/// `z`'s "dead prefix" shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CnBracket {
    /// `]` was pressed: `]c` jumps to the next marker, anything else scrolls
    /// tables right (`App::scroll_tables(1)`).
    Next,
    /// `[` was pressed: `[c` jumps to the previous marker, anything else
    /// scrolls tables left (`App::scroll_tables(-1)`).
    Prev,
}

/// The watchlist pane's two tabs (PRD FR-ACC-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatchlistTab {
    #[default]
    Pages,
    Changes,
}

impl WatchlistTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pages => "Watched pages",
            Self::Changes => "Recent changes",
        }
    }

    /// `Tab`/`l` (PRD FR-ACC-2, mirroring `OtdType::next`).
    pub fn next(self) -> Self {
        match self {
            Self::Pages => Self::Changes,
            Self::Changes => Self::Pages,
        }
    }

    /// `Shift-Tab`/`h`.
    pub fn prev(self) -> Self {
        self.next() // exactly two tabs: next and prev coincide
    }
}

/// The notifications pane's two tabs (PRD FR-ACC-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifTab {
    #[default]
    Alerts,
    Messages,
}

impl NotifTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Alerts => "Alerts",
            Self::Messages => "Messages",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Alerts => Self::Messages,
            Self::Messages => Self::Alerts,
        }
    }

    pub fn prev(self) -> Self {
        self.next()
    }
}

/// PRD §5.9 / FR-ACC-1: the OAuth client identity and endpoints a login flow
/// is built from — the `App`-resident view of `[auth]` config, plus the
/// NF-NET-2 contact the token client's User-Agent needs.
#[derive(Debug, Clone, Default)]
pub struct AuthRuntime {
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub contact: String,
}

impl AuthRuntime {
    /// Whether login is configured at all: an empty `client_id` means no
    /// OAuth consumer has been registered, so `:login` reports that instead
    /// of starting a flow that can only fail (PRD §5.9).
    pub fn is_configured(&self) -> bool {
        !self.client_id.trim().is_empty()
    }
}

/// PRD §5.9's manual code-paste state: everything needed to finish the
/// exchange once the reader pastes the code, carried from the moment `:login
/// paste` built the authorization URL. The `verifier` proves we are the same
/// client (PKCE); `state` is checked against the pasted redirect's own state
/// (CSRF) when one is present; `redirect_uri` must match what the
/// authorization request advertised.
#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub verifier: String,
    pub state: String,
    pub redirect_uri: String,
    pub authorize_url: String,
}

/// See [`App::pending_session_restore`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingSessionRestore {
    pub scroll: u16,
    pub folded_blocks: HashSet<usize>,
}

/// PRD FR-NV-8's pending resume: what pressing `r` on the resume toast should
/// restore. `revid_matches` decides between an exact restore (same revision —
/// the saved absolute scroll and fold set are still valid) and the anchor
/// fallback (the article changed — best-effort scroll to the nearest surviving
/// section heading named `anchor` instead of trusting a stale offset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePosition {
    pub scroll: u16,
    pub folds: Vec<usize>,
    pub revid_matches: bool,
    pub anchor: Option<String>,
}

/// A confirmed-and-resolved bulk save (PRD FR-OFF-5): the human label for the
/// cost preview, the tier to pin at, and the concrete `(lang, title)` targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkSaveRequest {
    pub label: String,
    pub tier: Tier,
    pub targets: Vec<(String, String)>,
}

/// PRD FR-ACC-8: a prepared, unsaved typo-fix edit awaiting the reader's
/// explicit confirmation. Everything needed to fire the save is captured here
/// at `:edit` time (after the $EDITOR step) so the confirm handler splices and
/// posts with no re-fetch and no further decisions — the diff the reader sees
/// (`before`/`after`) is exactly the change that will be saved. `baserevid`/
/// `basetimestamp` were captured at fetch time and travel to the API for
/// conflict detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEdit {
    pub lang: String,
    pub title: String,
    /// The full article wikitext with the sentence already spliced — what a
    /// confirmed save posts verbatim (byte-preserving outside the sentence).
    pub new_wikitext: String,
    /// The located sentence's original wikitext span (diff "before").
    pub before: String,
    /// The reader's edited wikitext span (diff "after").
    pub after: String,
    pub summary: String,
    pub baserevid: u64,
    pub basetimestamp: String,
}

impl App {
    pub fn new(lang: String, theme: Theme, no_color: bool) -> Self {
        // Every session starts with exactly one (empty) tab; the invariant
        // "`tabs` is never empty while running" holds from here.
        let first_tab = Tab::new(0, lang.clone());
        // PRD §7 "429 / maxlag on interactive request": created here (not in
        // `main::run`) so the one caller that needs it, `main::open_title`,
        // can reach it through the `&mut App` it already has — see the
        // field's own doc comment for why this channel lives on `App`
        // rather than being threaded as a parameter like every other
        // background-result channel.
        let (foreground_retry_tx, foreground_retry_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            mode: Mode::Reading,
            prior_mode: Mode::Reading,
            tabs: vec![first_tab],
            active: 0,
            split: None,
            pending_ctrl_w: false,
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
            search_rewritten_query: None,
            offline_fallback_count: 0,
            results_offline: false,
            force_offline_search: false,
            search_operator_help: false,
            should_quit: false,
            lang,
            // PRD FR-ML-4/5: `main::run` overwrites both from the resolved
            // config before the first paint (same "App::new is Wikipedia by
            // default, main wires the real config in" split as `theme`/
            // `color_depth` above) — every existing `App::new` call site
            // (every test included) sees exactly the pre-FR-ML-4 behavior.
            active_wiki_name: "wikipedia".to_string(),
            wiki_registry: std::collections::BTreeMap::new(),
            selected_wiki_pick: 0,
            loading: false,
            pending_g: false,
            theme,
            no_color,
            // Truecolor is the identity mapping (`Theme::adapt` is a no-op
            // at this depth), so a caller that never touches this field —
            // every existing `App::new` test call site — sees exactly the
            // pre-FR-TH-3 behavior. `main::run` overwrites both before the
            // first paint, same as `measure`/`mouse_enabled` below.
            color_depth: ColorDepth::Truecolor,
            user_themes: Vec::new(),
            accessible: false,
            mouse_enabled: false,
            no_motion: false,
            hyperlinks_mode: HyperlinkMode::Auto,
            bidi_mode: bidi::BidiMode::Auto,
            bidi_terminal_active: false,
            bidi_terminal_sent: false,
            rtl_reorder: false,
            last_content_area: ratatui::layout::Rect::default(),
            last_tab_bar_area: None,
            citations: Vec::new(),
            selected_citation: 0,
            research: ResearchStore::in_memory(),
            selected_library: 0,
            cite_style: CiteStyle::Apa,
            library_prior_mode: Mode::Reading,
            pending_export_overwrite: None,
            command_input: String::new(),
            command_completions: Vec::new(),
            command_completion_index: 0,
            command_typeahead_slot: Arc::new(Mutex::new(None)),
            command_typeahead_loading: false,
            notice: None,
            layout: None,
            layout_cache: LayoutCache::new(layout::DEFAULT_L1_CAPACITY),
            layout_computations: 0,
            pending_revalidations: 0,
            pending_foreground_retries: 0,
            foreground_retry_tx,
            foreground_retry_rx,
            disambig_selected: 0,
            layout_width: 80,
            viewport_height: 0,
            measure: 88,
            ambiguous_wide: false,
            reading_wpm: 230,
            text_align: layout::TextAlign::Center,
            margin: 0,
            paragraph_spacing: 1,
            line_spacing: 0,
            word_spacing: 0,
            justify: false,
            hyphenate: false,
            hint_targets: Vec::new(),
            hint_input: String::new(),
            hint_background: false,
            config_ctx: ConfigContext::default(),
            image_store: crate::image::ImageStore::new(),
            image_fetch_limiter: crate::image::new_fetch_limiter(),
            image_epoch: 0,
            images_override: None,
            include_nonfree: false,
            graphics_env: crate::graphics::GraphicsEnv::default(),
            bookmarks: BookmarkStore::in_memory(),
            readlater: ReadLaterStore::in_memory(),
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
            pending_z: false,
            history_pick_matches: Vec::new(),
            history_pick_selected: 0,
            history_pick_filter: String::new(),
            history_pick_prior_mode: Mode::Reading,
            session_started_at: crate::history::now_unix(),
            trail: crate::trail::Trail::default(),
            trail_layout: crate::trail::TrailLayout::default(),
            trail_selected: 0,
            trail_prior_mode: Mode::Reading,
            pending_trail_export_overwrite: None,
            saved: SavedPages::in_memory(),
            search_index: crate::offline_search::OfflineIndex::in_memory(),
            selected_saved: 0,
            saved_prior_mode: Mode::Reading,
            fetch_queue: FetchQueue::in_memory(),
            offline_card_target: None,
            zim: None,
            quality_cache: HashMap::new(),
            confirmed_redlinks: HashSet::new(),
            checked_redlink_sources: HashSet::new(),
            redlink_card_target: None,
            pending_bulk_save: None,
            pending_saves: 0,
            pending_saved_export_overwrite: None,
            prefetch: None,
            prefetch_prior_mode: Mode::Reading,
            prefetch_log_scroll: 0,
            interest: crate::interest::InterestModel::default(),
            interest_learning: true,
            interest_path: None,
            interest_prior_mode: Mode::Reading,
            interests_scroll: 0,
            stats_prior_mode: Mode::Reading,
            stats_scroll: 0,
            feed_cache: None,
            startpage_config: StartPageConfig::default(),
            start_selected: 0,
            start_til_reroll: 0,
            otd: OnThisDayModel::default(),
            otd_tab: OtdType::default(),
            otd_selected: 0,
            otd_prior_mode: Mode::Reading,
            related_cache: HashMap::new(),
            selected_related: 0,
            related_prior_mode: Mode::Reading,
            related_loading: false,
            languages: Vec::new(),
            langlinks_cache: HashMap::new(),
            selected_lang: 0,
            lang_prior_mode: Mode::Reading,
            lang_filter_input: String::new(),
            lang_loading: false,
            pending_langlinks: 0,
            language_hint: None,
            keymap: registry::Keymap::vim(),
            palette_input: String::new(),
            palette_selected: 0,
            palette_prior_mode: Mode::Reading,
            section_jump_input: String::new(),
            section_jump_selected: 0,
            visual_anchor_line: 0,
            visual_cursor_line: 0,
            help_scroll: 0,
            peek: None,
            peek_prior_mode: Mode::Reading,
            summary_cache: HashMap::new(),
            summary_loading: false,
            info: None,
            info_prior_mode: Mode::Reading,
            pending_resume: None,
            suppress_resume_once: false,
            session_path: None,
            pending_session_restore: HashMap::new(),
            tts_command: None,
            tts: TtsRuntime::new(),
            tts_playing: false,
            macros: std::collections::BTreeMap::new(),
            auth: None,
            pending_login: None,
            login_input: String::new(),
            auth_runtime: AuthRuntime {
                client_id: String::new(),
                authorize_url: crate::auth::DEFAULT_AUTHORIZE_URL.to_string(),
                token_url: crate::auth::DEFAULT_TOKEN_URL.to_string(),
                contact: crate::api::DEFAULT_CONTACT.to_string(),
            },
            watchlist_raw: Vec::new(),
            watchlist_changes: Vec::new(),
            watchlist_tab: WatchlistTab::default(),
            watchlist_selected: 0,
            watchlist_prior_mode: Mode::Reading,
            watchlist_last_seen: None,
            watchlist_state_path: None,
            notif_alerts: Vec::new(),
            notif_messages: Vec::new(),
            notif_tab: NotifTab::default(),
            notif_selected: 0,
            notif_prior_mode: Mode::Reading,
            notif_counts: crate::account::NotifCounts::default(),
            contribs: Vec::new(),
            contribs_username: String::new(),
            contribs_selected: 0,
            contribs_prior_mode: Mode::Reading,
            tokens: crate::account::TokenCache::new(),
            editing_enabled: false,
            pending_edit: None,
            prefs: None,
            prefs_prior_mode: Mode::Reading,
            watchlist_mirror_tag: "watched".to_string(),
            readinglist_sync_state_path: None,
            watch_mirror_state_path: None,
            show_cn: false,
            pending_cn_bracket: None,
            game: None,
            achievements_shown: std::collections::HashSet::new(),
            pro: false,
            liftwing_enabled: false,
            liftwing_base_url: crate::config::DEFAULT_LIFTWING_BASE_URL.to_string(),
        }
    }

    /// PRD FR-ACC-1: the logged-in username, or `None` when logged out —
    /// the status-bar indicator's source of truth.
    pub fn logged_in_username(&self) -> Option<&str> {
        self.auth.as_ref().map(|a| a.username())
    }

    /// PRD FR-CS-1: open the command palette over the current context. The
    /// context (which commands apply) and the mode to restore on cancel are
    /// captured from wherever `Ctrl-p` was pressed.
    pub fn open_palette(&mut self) {
        self.palette_prior_mode = self.mode;
        self.palette_input.clear();
        self.palette_selected = 0;
        self.mode = Mode::Palette;
    }

    /// The [`registry::KeyContext`] a mode maps to, for scoping the palette
    /// and generating the help cheatsheet (PRD FR-CS-1/4).
    pub fn key_context(&self, mode: Mode) -> registry::KeyContext {
        use registry::KeyContext;
        match mode {
            Mode::Reading if self.active_tab().doc.is_none() => KeyContext::StartPage,
            Mode::Reading => KeyContext::Reading,
            Mode::Search => KeyContext::Search,
            _ => KeyContext::Picker,
        }
    }

    /// The live, fuzzy-filtered palette rows for the current query (PRD
    /// FR-CS-1) — scoped to the context the palette was opened from.
    pub fn palette_rows(&self) -> Vec<registry::PaletteRow> {
        registry::palette_matches(
            &self.keymap,
            self.key_context(self.palette_prior_mode),
            &self.palette_input,
        )
    }

    /// Move the palette selection, clamped to the current match count.
    pub fn palette_move(&mut self, delta: i32) {
        let len = self.palette_rows().len();
        if len == 0 {
            self.palette_selected = 0;
            return;
        }
        let max = len - 1;
        self.palette_selected =
            (self.palette_selected as i32 + delta).clamp(0, max as i32) as usize;
    }

    /// The action the highlighted palette row would run, if any.
    pub fn palette_selection(&self) -> Option<registry::Action> {
        self.palette_rows()
            .get(self.palette_selected)
            .map(|r| r.action)
    }

    /// PRD FR-PF-4: open the `:prefetch-log` transparency panel.
    pub fn open_prefetch_log(&mut self) {
        self.prefetch_prior_mode = self.mode;
        self.mode = Mode::PrefetchLog;
        self.prefetch_log_scroll = 0;
        self.status = "Prefetch log — j/k: scroll  Esc: close".to_string();
    }

    /// Close the prefetch-log panel, restoring the prior mode.
    pub fn close_prefetch_log(&mut self) {
        self.mode = self.prefetch_prior_mode;
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    /// PRD FR-PF-6 kill switch: flip the substrate's enabled flag (if a
    /// substrate is installed) and report. Prefetch off never breaks anything —
    /// scheduling simply stops enqueueing.
    pub fn set_prefetch(&mut self, on: bool) {
        if let Some(handle) = &self.prefetch {
            handle.set_enabled(on);
        }
        self.notice = Some(format!("prefetch={}", if on { "on" } else { "off" }));
    }

    /// PRD FR-NV-9: flip the app-side mouse flag (`:set mouse=on|off`). The
    /// caller (`main::execute_command`) is responsible for also toggling the
    /// real terminal mouse-capture state via `crashguard::set_mouse_capture`
    /// — this method only updates the flag every mouse-handling call site
    /// reads, so the two can never independently drift, but it never touches
    /// the terminal itself (this module has no I/O of its own, by design).
    pub fn set_mouse(&mut self, on: bool) {
        self.mouse_enabled = on;
        self.notice = Some(format!(
            "mouse={} (terminal selection/copy {})",
            if on { "on" } else { "off" },
            if on {
                "off while mouse is on"
            } else {
                "restored"
            }
        ));
    }

    /// PRD FR-ACS-4: `:set animations=full|none`.
    pub fn set_no_motion(&mut self, none: bool) {
        self.no_motion = none;
        self.notice = Some(format!("animations={}", if none { "none" } else { "full" }));
    }

    /// PRD FR-RD-2 / SEC-2: `:set hyperlinks=auto|on|off`.
    pub fn set_hyperlinks_mode(&mut self, mode: HyperlinkMode) {
        self.hyperlinks_mode = mode;
        self.notice = Some(format!("hyperlinks={}", mode.as_str()));
    }

    /// Whether prefetch is currently active for *this* session: the kill
    /// switch is on AND `privacy::decide` allows it (`Write::Prefetch` is
    /// passive, so incognito always denies it — PRD FR-PR-3). The single
    /// gate every prefetch-scheduling decision consults.
    pub fn prefetch_active(&self) -> bool {
        crate::privacy::decide(self.incognito, crate::privacy::Write::Prefetch)
            == crate::privacy::Verdict::Allow
            && self
                .prefetch
                .as_ref()
                .is_some_and(crate::netqueue::SubstrateHandle::is_enabled)
    }

    // -- Interest model (PRD FR-PF-3, FR-PR-2) ------------------------------

    /// Whether the interest model may learn from reading signals right now:
    /// the `interest_learning` config is on AND `privacy::decide` allows a
    /// `Write::Interest` (incognito denies it — PRD FR-PR-3 "no interest
    /// updates", so the model is off in incognito regardless of config). The
    /// single gate every signal-application path consults, routed through the
    /// same privacy chokepoint as history/prefetch so the audit is uniform.
    pub fn interest_active(&self) -> bool {
        self.interest_learning
            && crate::privacy::decide(self.incognito, crate::privacy::Write::Interest)
                == crate::privacy::Verdict::Allow
    }

    /// The interest-model clock: unix seconds, this module's single wall-clock
    /// read for interest signals (mirrors `history::now_unix`), so a signal is
    /// never timestamped against a second, independently-read "now". The model
    /// methods themselves take `now` as a parameter (deterministic in tests);
    /// this is only where the *live* app reads it.
    fn interest_now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// PRD FR-PF-3 "open" signal: record that `title` was read with these raw
    /// (API-form) categories and apply +1.0 to its topics. A no-op when
    /// `interest_active` is false (learning off / incognito). On a revisit
    /// whose categories are already cached this session, `raw_categories` may
    /// be empty — the model applies the open signal from the cached categories.
    /// Persists the model afterward (best-effort).
    pub fn note_article_read(&mut self, wiki: &str, title: &str, raw_categories: &[String]) {
        if !self.interest_active() {
            return;
        }
        let now = Self::interest_now();
        if self.interest.knows_categories(wiki, title) {
            self.interest
                .apply_signal_for_title(wiki, title, crate::interest::SIGNAL_OPEN, now);
        } else {
            self.interest.note_open(wiki, title, raw_categories, now);
        }
        self.persist_interest();
    }

    /// Apply a signal `amount` (bookmark/save) to the *active article*'s
    /// categories — the discrete signals the reader triggers on the page
    /// they're reading. Returns whether the model was affected (false when
    /// learning is off, no article is open, or its categories aren't known
    /// yet). Persists on a real change.
    pub fn apply_active_article_signal(&mut self, amount: f64) -> bool {
        if !self.interest_active() {
            return false;
        }
        let Some(title) = self.active_tab().doc.as_ref().map(|d| d.title.clone()) else {
            return false;
        };
        let wiki = self.active_tab().wiki.clone();
        let changed =
            self.interest
                .apply_signal_for_title(&wiki, &title, amount, Self::interest_now());
        if changed {
            self.persist_interest();
        }
        changed
    }

    /// PRD FR-PF-3's explicit "not interested" (`:not-interested` / key): tank
    /// the active article's categories by the strong negative signal and set a
    /// notice. Reports honestly when there's nothing to act on (no article, or
    /// its categories aren't known yet).
    pub fn mark_not_interested(&mut self) {
        if !self.interest_active() {
            self.notice = Some(if self.incognito {
                "Interest learning is off in incognito".to_string()
            } else {
                "Interest learning is off".to_string()
            });
            return;
        }
        let Some(title) = self.active_tab().doc.as_ref().map(|d| d.title.clone()) else {
            self.notice = Some("No article open".to_string());
            return;
        };
        let wiki = self.active_tab().wiki.clone();
        if self
            .interest
            .not_interested(&wiki, &title, Self::interest_now())
        {
            self.persist_interest();
            self.notice = Some(format!(
                "Marked \"{title}\" not interesting — its topics dropped"
            ));
        } else {
            self.notice = Some("No topics known for this article yet — open it first".to_string());
        }
    }

    /// Persist the interest model to `interest_path`, if one is set (only in a
    /// real run, never in tests). Best-effort: a write failure is logged to
    /// stderr, never surfaced to the reader — the model is non-critical, same
    /// posture as `history`.
    fn persist_interest(&self) {
        if let Some(path) = &self.interest_path
            && let Err(e) = self.interest.save(path)
        {
            eprintln!("wikitui: interest: save failed: {e}");
        }
    }

    /// PRD FR-PF-3 / FR-PF-4: open the `:interests` model inspector.
    pub fn open_interests(&mut self) {
        self.interest_prior_mode = self.mode;
        self.mode = Mode::Interests;
        self.interests_scroll = 0;
        self.status = "Interest model — j/k: scroll  Esc: close".to_string();
    }

    /// Close the interests panel, restoring the prior mode.
    pub fn close_interests(&mut self) {
        self.mode = self.interest_prior_mode;
        self.restore_reading_status();
    }

    /// PRD FR-PC-3: open the `:stats` reading-stats view.
    pub fn open_stats(&mut self) {
        self.stats_prior_mode = self.mode;
        self.mode = Mode::Stats;
        self.stats_scroll = 0;
        self.status = "Reading stats — j/k: scroll  Esc: close".to_string();
    }

    /// Close the stats view, restoring the prior mode.
    pub fn close_stats(&mut self) {
        self.mode = self.stats_prior_mode;
        self.restore_reading_status();
    }

    /// The current reading stats (PRD FR-PC-3) for the `:stats` view — computed
    /// live from history + the interest model's top topics, the same
    /// `stats::compute` the `wikitui stats` CLI uses.
    pub fn reading_stats(&self) -> crate::stats::ReadingStats {
        crate::stats::compute(&self.history.all_visits(), self.interest.top_categories(8))
    }

    /// Reset the status line to the article title (or the default hint) —
    /// shared by the read-only panels' close paths.
    fn restore_reading_status(&mut self) {
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    // -- Start page (PRD FR-DL-1) -------------------------------------------

    /// Whether the daily feed hasn't arrived yet but a fetch could still
    /// land (something is queued/in-flight on the substrate) — the start
    /// page shows its loading skeleton exactly while this is true, and the
    /// event loop's scoped poll (`main::run`) stays awake so the feed's
    /// arrival redraws without a keypress (§6.8: never block startup on it).
    /// Once nothing is pending and the feed still never showed up (offline,
    /// the fetch failed, prefetch is off/incognito), this goes `false` and
    /// `start_page_model` falls back to `StartPageModel::offline_fallback`
    /// instead of spinning forever.
    pub fn start_page_pending(&self) -> bool {
        let Some(fc) = &self.feed_cache else {
            return false;
        };
        if fc.lock().unwrap().get().is_some() {
            return false; // already arrived — nothing to wait for
        }
        // `pending()` alone would flip to 0 the instant the worker dequeues
        // the job, before its HTTP round trip finishes — `any_inflight()`
        // stays true across execution (see its doc comment), which is the
        // signal that actually means "still worth waiting."
        self.prefetch
            .as_ref()
            .is_some_and(|h| h.pending() > 0 || h.any_inflight())
    }

    /// Builds the current start-page render model (PRD FR-DL-1): the parsed
    /// feed's sections plus the TIL pick if the feed has arrived, the
    /// loading skeleton while a fetch could still land, or the graceful
    /// offline fallback (recent history / saved pages) otherwise. Cheap
    /// enough (a handful of short strings) to rebuild on every draw rather
    /// than cache — see the module doc comment on `startpage`.
    pub fn start_page_model(&self) -> StartPageModel {
        let (feed, date) = match &self.feed_cache {
            Some(fc) => {
                let guard = fc.lock().unwrap();
                (
                    guard.get().cloned(),
                    guard.date().unwrap_or_default().to_string(),
                )
            }
            None => (None, String::new()),
        };
        match feed {
            Some(feed) => {
                let til = startpage::pick_til(&feed, &date, self.start_til_reroll);
                StartPageModel::from_feed(&feed, til)
            }
            None if self.start_page_pending() => StartPageModel::skeleton(),
            None => {
                // PRD FR-DL-1's networking discipline: offline (or the fetch
                // gave up) degrades to recent history / saved pages, never an
                // error screen.
                let recent: Vec<(String, String)> = self
                    .history
                    .recent(5)
                    .into_iter()
                    .map(|v| (v.lang, v.title))
                    .collect();
                let saved: Vec<(String, String)> = self
                    .saved
                    .list()
                    .iter()
                    .take(5)
                    .map(|r| (r.lang.clone(), r.title.clone()))
                    .collect();
                StartPageModel::offline_fallback(&recent, &saved)
            }
        }
    }

    /// Moves the start page's selection by `delta` (j/k/Tab/Shift-Tab —
    /// PRD FR-DL-1: "a real navigable view"), wrapping and clamped to
    /// whatever the model currently has (the feed can arrive mid-session and
    /// replace a shorter skeleton).
    pub fn start_page_move(&mut self, delta: i32) {
        let model = self.start_page_model();
        self.start_selected = model.clamp_selection(self.start_selected);
        self.start_selected = model.move_selection(self.start_selected, delta);
    }

    /// The `(lang override, title)` Enter should open for the focused
    /// start-page item, owned so the caller can `.await` a fetch after
    /// releasing the borrow on `self`.
    pub fn start_page_open_target(&self) -> Option<(Option<String>, String)> {
        let model = self.start_page_model();
        let index = model.clamp_selection(self.start_selected);
        model
            .open_target(index)
            .map(|(lang, title)| (lang.map(str::to_string), title.to_string()))
    }

    /// FR-DL-7's manual reroll: steps the TIL widget to a different
    /// candidate within today's feed without waiting for tomorrow. A no-op
    /// (still fine to press) when the feed has no on-this-day entries to
    /// rotate through.
    pub fn reroll_til(&mut self) {
        self.start_til_reroll = self.start_til_reroll.wrapping_add(1);
    }

    /// PRD FR-DL-1's `gh`/`:start` "home" action: returns the active tab to
    /// the start page, pushing whatever it was showing onto the back stack
    /// first (browser-style "Home" — `H` still returns to the article) —
    /// the tab-content half of `go_home`; the caller is responsible for
    /// re-syncing anything derived from the active tab's document (this
    /// mirrors `open_document`'s split between stack bookkeeping and content
    /// install).
    pub fn go_home(&mut self) {
        let index = self.active;
        self.flush_tab_dwell(index);
        if let Some(entry) = self.active_tab().current_entry() {
            self.active_tab_mut().back_stack.push(entry);
        }
        self.active_tab_mut().forward_stack.clear();
        self.active_tab_mut().clear_to_blank();
        self.start_selected = 0;
        self.citations.clear();
        self.selected_citation = 0;
        self.mode = Mode::Reading;
        self.layout = None;
        self.language_hint = None;
        self.status = "Press / to search, ? for help, q to quit".to_string();
    }

    // -- On-this-day panel (PRD FR-DL-2) -------------------------------------

    /// Opens the `:today` panel. The caller (`main::open_on_this_day`) is
    /// responsible for kicking off the per-type fetches; this only handles
    /// the mode/status bookkeeping so it can run before the `.await`s start
    /// (the panel shows its own "loading" status in the meantime).
    pub fn open_on_this_day(&mut self) {
        self.otd_prior_mode = self.mode;
        self.mode = Mode::OnThisDay;
        self.otd_tab = OtdType::default();
        self.otd_selected = 0;
        self.status = "Loading on this day…".to_string();
    }

    /// Closes the on-this-day panel, restoring the prior mode.
    pub fn close_on_this_day(&mut self) {
        self.mode = self.otd_prior_mode;
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    /// Moves the selection within the current type tab, wrapping.
    pub fn otd_move(&mut self, delta: i32) {
        self.otd_selected = self
            .otd
            .move_selection(self.otd_tab, self.otd_selected, delta);
    }

    /// Switches to the next type tab (PRD FR-DL-2's events/births/deaths/
    /// holidays/selected), resetting the selection — a stale index into a
    /// different type's list would be meaningless.
    pub fn otd_next_tab(&mut self) {
        self.otd_tab = self.otd_tab.next();
        self.otd_selected = 0;
    }

    /// The previous-tab counterpart of `otd_next_tab`.
    pub fn otd_prev_tab(&mut self) {
        self.otd_tab = self.otd_tab.prev();
        self.otd_selected = 0;
    }

    /// The article title Enter should open for the focused entry in the
    /// current type tab, or `None` if it links nothing.
    pub fn otd_open_target(&self) -> Option<String> {
        self.otd
            .open_target(self.otd_tab, self.otd_selected)
            .map(str::to_string)
    }

    // -- Watchlist (PRD FR-ACC-2) ---------------------------------------------
    //
    // Every method below is mode/selection bookkeeping only, exactly like
    // `open_on_this_day`/`otd_move` above — the login gate and the network
    // fetch both live in `main.rs` (`require_login`, `open_watchlist`), which
    // calls `App::enter_watchlist` only *after* confirming a session exists.
    // This mirrors `open_info`'s "no article, no mode change" gate: a logged-
    // out `:watchlist` never touches `self.mode` at all, so there is nothing
    // here that could accidentally show the pane before the guard runs.

    /// Enters the watchlist pane's mode/selection state (mirrors
    /// `open_on_this_day`). Called by `main::open_watchlist` once login is
    /// confirmed and the fetch is about to start.
    pub fn enter_watchlist(&mut self) {
        self.watchlist_prior_mode = self.mode;
        self.mode = Mode::Watchlist;
        self.watchlist_tab = WatchlistTab::default();
        self.watchlist_selected = 0;
        self.status = "Loading watchlist…".to_string();
    }

    /// Closes the watchlist pane, restoring the prior mode.
    pub fn close_watchlist(&mut self) {
        self.mode = self.watchlist_prior_mode;
        self.refresh_reading_status();
    }

    /// Moves the selection within the current tab, wrapping.
    pub fn watchlist_move(&mut self, delta: i32) {
        let len = match self.watchlist_tab {
            WatchlistTab::Pages => self.watchlist_raw.len(),
            WatchlistTab::Changes => self.watchlist_changes.len(),
        };
        self.watchlist_selected = wrap_move(self.watchlist_selected, len, delta);
    }

    /// Switches to the other tab, resetting the selection.
    pub fn watchlist_next_tab(&mut self) {
        self.watchlist_tab = self.watchlist_tab.next();
        self.watchlist_selected = 0;
    }

    /// The `Shift-Tab`/`h` counterpart of `watchlist_next_tab`.
    pub fn watchlist_prev_tab(&mut self) {
        self.watchlist_tab = self.watchlist_tab.prev();
        self.watchlist_selected = 0;
    }

    /// The article title Enter should open for the focused entry, or `None`
    /// on an empty list.
    pub fn watchlist_open_target(&self) -> Option<String> {
        match self.watchlist_tab {
            WatchlistTab::Pages => self.watchlist_raw.get(self.watchlist_selected).cloned(),
            WatchlistTab::Changes => self
                .watchlist_changes
                .get(self.watchlist_selected)
                .map(|c| c.title.clone()),
        }
    }

    // -- Notifications / Echo (PRD FR-ACC-3) -----------------------------------
    //
    // Same "bookkeeping here, login gate + network in main.rs" split as the
    // watchlist section above.

    /// Enters the notifications pane's mode/selection state.
    pub fn enter_notifications(&mut self) {
        self.notif_prior_mode = self.mode;
        self.mode = Mode::Notifications;
        self.notif_tab = NotifTab::default();
        self.notif_selected = 0;
        self.status = "Loading notifications…".to_string();
    }

    /// Closes the notifications pane, restoring the prior mode.
    pub fn close_notifications(&mut self) {
        self.mode = self.notif_prior_mode;
        self.refresh_reading_status();
    }

    fn notif_list(&self) -> &[crate::account::Notification] {
        match self.notif_tab {
            NotifTab::Alerts => &self.notif_alerts,
            NotifTab::Messages => &self.notif_messages,
        }
    }

    fn notif_list_mut(&mut self) -> &mut Vec<crate::account::Notification> {
        match self.notif_tab {
            NotifTab::Alerts => &mut self.notif_alerts,
            NotifTab::Messages => &mut self.notif_messages,
        }
    }

    /// Moves the selection within the current tab, wrapping.
    pub fn notif_move(&mut self, delta: i32) {
        let len = self.notif_list().len();
        self.notif_selected = wrap_move(self.notif_selected, len, delta);
    }

    /// Switches to the other tab, resetting the selection.
    pub fn notif_next_tab(&mut self) {
        self.notif_tab = self.notif_tab.next();
        self.notif_selected = 0;
    }

    /// The `Shift-Tab`/`h` counterpart of `notif_next_tab`.
    pub fn notif_prev_tab(&mut self) {
        self.notif_tab = self.notif_tab.prev();
        self.notif_selected = 0;
    }

    /// The focused notification's id, for a mark-read request — `None` on
    /// an empty list.
    pub fn notif_focused_id(&self) -> Option<String> {
        self.notif_list()
            .get(self.notif_selected)
            .map(|n| n.id.clone())
    }

    /// Marks one notification read *locally* (PRD FR-ACC-3 / the module's
    /// poll-cadence contract: no network round trip just to reflect what the
    /// write we already made changed) and recomputes the badge from the
    /// union of both lists — the caller (`main::mark_notification_read`)
    /// only invokes this after the `echomarkread` write itself succeeded.
    pub fn mark_notif_read_locally(&mut self, id: &str) {
        for n in self.notif_list_mut() {
            if n.id == id {
                n.read = true;
            }
        }
        self.recompute_notif_counts();
    }

    /// Marks every notification read locally, the mark-all-read counterpart
    /// of `mark_notif_read_locally`.
    pub fn mark_all_notifs_read_locally(&mut self) {
        for n in self
            .notif_alerts
            .iter_mut()
            .chain(self.notif_messages.iter_mut())
        {
            n.read = true;
        }
        self.recompute_notif_counts();
    }

    fn recompute_notif_counts(&mut self) {
        let mut combined: Vec<crate::account::Notification> =
            Vec::with_capacity(self.notif_alerts.len() + self.notif_messages.len());
        combined.extend(self.notif_alerts.iter().cloned());
        combined.extend(self.notif_messages.iter().cloned());
        self.notif_counts = crate::account::counts_from_list(&combined);
    }

    /// The status-bar badge (PRD FR-ACC-3), `None` when nothing is unread —
    /// the `[@user …]` indicator's source for the `✉N` suffix.
    pub fn notification_badge(&self) -> Option<String> {
        crate::account::format_badge(self.notif_counts)
    }

    // -- Contributions (PRD FR-ACC-4) -------------------------------------------

    /// Enters the contributions view's mode/selection state for `username`.
    /// Unlike watchlist/notifications, this has no login gate of its own —
    /// `usercontribs` is public (FR-ACC-4) — so `main::open_contribs` calls
    /// this unconditionally once it has resolved which username to show.
    pub fn enter_contribs(&mut self, username: String) {
        self.contribs_prior_mode = self.mode;
        self.mode = Mode::Contribs;
        self.contribs_username = username;
        self.contribs_selected = 0;
        self.status = "Loading contributions…".to_string();
    }

    /// Closes the contributions view, restoring the prior mode.
    pub fn close_contribs(&mut self) {
        self.mode = self.contribs_prior_mode;
        self.refresh_reading_status();
    }

    /// Moves the selection, wrapping.
    pub fn contribs_move(&mut self, delta: i32) {
        self.contribs_selected = wrap_move(self.contribs_selected, self.contribs.len(), delta);
    }

    /// The article title Enter should open for the focused edit, or `None`
    /// on an empty list.
    pub fn contribs_open_target(&self) -> Option<String> {
        self.contribs
            .get(self.contribs_selected)
            .map(|c| c.title.clone())
    }

    /// The focused edit's revision id, for `t` (PRD FR-ACC-6's thank).
    pub fn contribs_focused_revid(&self) -> Option<u64> {
        self.contribs.get(self.contribs_selected).map(|c| c.revid)
    }

    // -- Preferences (PRD FR-ACC-7, read-only) ----------------------------------

    /// Enters the read-only prefs card's mode state (mirrors `open_info`'s
    /// overlay shape). `main::open_prefs` calls this once login is confirmed
    /// and the fetch is about to start.
    pub fn enter_prefs(&mut self) {
        self.prefs_prior_mode = self.mode;
        self.mode = Mode::Prefs;
        self.status = "Loading preferences…".to_string();
    }

    /// Closes the prefs card, restoring the prior mode.
    pub fn close_prefs(&mut self) {
        self.mode = self.prefs_prior_mode;
        self.prefs = None;
        self.refresh_reading_status();
    }

    // -- Related panel (PRD FR-SR-6) -----------------------------------------

    /// The active tab's `(wiki, lang, title)` session-cache key, or `None`
    /// when no document is open — shared by the Related panel (nothing to be
    /// "related to" then) and the language picker (nothing to fetch langlinks
    /// for). The wiki dimension (PRD FR-ML-4) keeps a same-titled article on
    /// two different wikis from colliding in any of the session maps.
    fn current_article_key(&self) -> Option<(String, String, String)> {
        let tab = self.active_tab();
        tab.doc
            .as_ref()
            .map(|doc| (tab.wiki.clone(), tab.lang.clone(), doc.title.clone()))
    }

    /// Opens the Related panel for the active tab's article. Returns
    /// whether the caller must kick off a `morelike:` fetch
    /// (`main::open_related` does so only when this is `true`): a session
    /// cache hit needs no network round trip at all, which is exactly what
    /// "cached per-article for the session" (PRD FR-SR-6) means. `false`
    /// also covers "no article open" — the panel still opens (showing its
    /// own empty/`status` message) rather than refusing the keypress.
    pub fn open_related(&mut self) -> bool {
        self.related_prior_mode = self.mode;
        self.mode = Mode::Related;
        self.selected_related = 0;
        match self.current_article_key() {
            None => {
                self.related_loading = false;
                self.status = "Open an article first".to_string();
                false
            }
            Some(key) if self.related_cache.contains_key(&key) => {
                self.related_loading = false;
                self.status = "Related articles — Enter: open   Esc: close".to_string();
                false
            }
            Some(_) => {
                self.related_loading = true;
                self.status = "Loading related articles…".to_string();
                true
            }
        }
    }

    /// Closes the Related panel, restoring the prior mode.
    pub fn close_related(&mut self) {
        self.mode = self.related_prior_mode;
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    /// The panel's current item list: whatever the session cache holds for
    /// the active tab's article, or empty if nothing has arrived (or
    /// there's no article open at all).
    pub fn related_items(&self) -> &[SearchResult] {
        match self.current_article_key() {
            Some(key) => self
                .related_cache
                .get(&key)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            None => &[],
        }
    }

    /// Moves the panel's selection by `delta`, wrapping — a no-op (stays 0)
    /// on an empty list.
    pub fn related_move(&mut self, delta: i32) {
        let len = self.related_items().len();
        if len == 0 {
            self.selected_related = 0;
            return;
        }
        let cur = (self.selected_related as i32).rem_euclid(len as i32);
        self.selected_related = (cur + delta).rem_euclid(len as i32) as usize;
    }

    /// The title Enter should open for the focused related item, or `None`
    /// on an empty list.
    pub fn related_open_target(&self) -> Option<String> {
        self.related_items()
            .get(self.selected_related)
            .map(|r| r.title.clone())
    }

    /// Installs a completed (or failed) `morelike:` fetch into the session
    /// cache, keyed by `(lang, title)` — PRD FR-SR-6's "cached per-article
    /// for the session": switching tabs, reopening the panel, or revisiting
    /// the same article later all become cache hits from here on. A
    /// network failure caches an empty list rather than nothing at all, so
    /// a flaky request doesn't get silently retried every time the panel
    /// reopens — the reader sees "no related articles found" once and that
    /// stands for the rest of the session (mirroring how `current_article_key`
    /// treats "no entry yet" as "still loading" only via `related_loading`,
    /// never by falling through to a repeat fetch). Applied even if the
    /// reader has since navigated away from `title` — it's still cached for
    /// when they come back — but the loading flag and status line only
    /// update if the panel is still showing exactly this article.
    pub fn deliver_related(
        &mut self,
        wiki: String,
        lang: String,
        title: String,
        result: Result<Vec<SearchResult>, String>,
    ) {
        let items = result.unwrap_or_default();
        let is_current_and_open = self.mode == Mode::Related
            && self.current_article_key() == Some((wiki.clone(), lang.clone(), title.clone()));
        self.related_cache.insert((wiki, lang, title), items);
        if is_current_and_open {
            self.related_loading = false;
            self.status = if self.related_items().is_empty() {
                "No related articles found — Esc to close".to_string()
            } else {
                "Related articles — Enter: open   Esc: close".to_string()
            };
        }
    }

    // -- Language switcher & fallback chain (PRD FR-ML-1/2) ------------------

    /// Raw cached langlinks for the active tab's article, unfiltered and in
    /// the server's own order — empty while still loading, with no article
    /// open, or genuinely nothing cached yet.
    pub fn lang_links(&self) -> &[crate::api::LangLink] {
        match self.current_article_key() {
            Some(key) => self
                .langlinks_cache
                .get(&key)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            None => &[],
        }
    }

    /// `:lang` (bare): opens the picker for the active tab's article.
    /// Returns whether the caller must fire a fetch (`main::open_lang_picker`
    /// does so only when this is `true`) — mirrors `open_related`'s split
    /// exactly: a session-cache hit needs no round trip, and "no article
    /// open" is `false` too (the picker still opens, showing its own status
    /// message, rather than refusing the keypress).
    pub fn open_lang_picker(&mut self) -> bool {
        self.lang_prior_mode = self.mode;
        self.mode = Mode::LangPicker;
        self.selected_lang = 0;
        self.lang_filter_input.clear();
        match self.current_article_key() {
            None => {
                self.lang_loading = false;
                self.status = "Open an article first".to_string();
                false
            }
            Some(key) if self.langlinks_cache.contains_key(&key) => {
                self.lang_loading = false;
                self.refresh_lang_picker_status();
                false
            }
            Some(_) => {
                self.lang_loading = true;
                self.status = "Loading language editions…".to_string();
                true
            }
        }
    }

    /// Closes the picker, restoring the prior mode — mirrors
    /// `close_related`.
    pub fn close_lang_picker(&mut self) {
        self.mode = self.lang_prior_mode;
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    /// The picker's live row list: cached langlinks, preferred-pinned and
    /// fuzzy-filtered by `lang_filter_input` — see [`order_and_filter_langlinks`].
    pub fn lang_picker_rows(&self) -> Vec<crate::api::LangLink> {
        order_and_filter_langlinks(self.lang_links(), &self.languages, &self.lang_filter_input)
    }

    /// Moves the picker's selection, wrapping over the filtered row list —
    /// a no-op with nothing visible.
    pub fn cycle_lang(&mut self, forward: bool) {
        let len = self.lang_picker_rows().len();
        if len == 0 {
            self.selected_lang = 0;
            return;
        }
        self.selected_lang = if forward {
            (self.selected_lang + 1) % len
        } else {
            (self.selected_lang + len - 1) % len
        };
    }

    /// Enter's target: the `(lang code, title in that edition)` to switch
    /// to, or `None` on an empty/filtered-to-nothing list.
    pub fn lang_picker_target(&self) -> Option<(String, String)> {
        self.lang_picker_rows()
            .get(self.selected_lang)
            .map(|l| (l.code.clone(), l.title.clone()))
    }

    /// `:lang <code>`'s disambiguation (PRD FR-ML-2): the translated title
    /// to switch to when the active tab's article has a *cached* langlink
    /// for `code`, or `None` when it doesn't — not yet loaded, a failed
    /// fetch cached an empty list, or genuinely no edition in that
    /// language. `main::set_or_switch_lang` is the only caller; kept as a
    /// pure `App` method (rather than inlined there) so the disambiguation
    /// itself is testable without a network round trip.
    pub fn lang_link_title_for_code(&self, code: &str) -> Option<String> {
        self.lang_links()
            .iter()
            .find(|l| l.code == code)
            .map(|l| l.title.clone())
    }

    /// Recomputes the picker's status line from its current (already
    /// filtered) row list — called after a fetch lands and after the filter
    /// text changes, so it never goes stale mid-session.
    fn refresh_lang_picker_status(&mut self) {
        self.status = if !self.lang_picker_rows().is_empty() {
            "j/k: move   /: filter   Enter: switch   Esc: close".to_string()
        } else if self.lang_filter_input.is_empty() {
            "No language editions found — Esc to close".to_string()
        } else {
            "No matches — Esc to close".to_string()
        };
    }

    /// Installs a completed (or failed) langlinks fetch into the session
    /// cache, keyed by `(lang, title)` of the *source* article — mirrors
    /// `deliver_related` exactly, including caching an empty list on failure
    /// so a flaky request isn't silently retried every time the picker
    /// reopens. Also recomputes the FR-ML-2 "available in your preferred
    /// language" hint when this is the article currently on screen,
    /// regardless of whether the picker itself is open — the hint is meant
    /// to surface passively.
    pub fn deliver_langlinks(
        &mut self,
        wiki: String,
        lang: String,
        title: String,
        result: Result<Vec<crate::api::LangLink>, String>,
    ) {
        let links = result.unwrap_or_default();
        let is_current =
            self.current_article_key() == Some((wiki.clone(), lang.clone(), title.clone()));
        self.langlinks_cache.insert((wiki, lang, title), links);
        if is_current {
            self.refresh_language_hint();
            if self.mode == Mode::LangPicker {
                self.lang_loading = false;
                self.refresh_lang_picker_status();
            }
        }
    }

    /// PRD FR-ML-2: recomputes `language_hint` from whatever langlinks are
    /// cached for the active tab's article right now. Called whenever fresh
    /// langlinks land for it (`deliver_langlinks`) — not on every draw, since
    /// the answer only ever changes when a fetch completes or the article
    /// changes (and a fresh article's `set_document` clears it below).
    fn refresh_language_hint(&mut self) {
        let current_lang = self.active_tab().lang.clone();
        self.language_hint =
            preferred_language_hint(self.lang_links(), &current_lang, &self.languages)
                .map(|autonym| format!("also in {autonym} — :lang"));
    }

    /// The tab currently on screen. `tabs` is never empty while the app runs
    /// (closing the last tab quits), so indexing is safe.
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    // -- Typo-fix editing (PRD FR-ACC-8) ------------------------------------

    /// The FR-ACC-8 editing gate for the current session: the config opt-in
    /// (`editing_enabled`) AND a logged-in session carrying the `editpage`
    /// grant. The single predicate every edit command routes through — see
    /// `editing::edit_gate`.
    pub fn edit_gate(&self) -> crate::editing::EditGate {
        let logged_in = self.auth.is_some();
        let has_grant = self.auth.as_ref().is_some_and(|a| a.has_editpage());
        crate::editing::edit_gate(self.editing_enabled, logged_in, has_grant)
    }

    /// PRD FR-ACC-8: the rendered sentence the reader has selected to edit —
    /// the sentence containing the currently focused link's anchor text, drawn
    /// from that link's own paragraph. `None` when nothing is focused, no
    /// document is open, or the anchor couldn't be resolved to a sentence.
    /// This is the *rendered* text; `editing::locate_sentence` maps it to the
    /// wikitext source span.
    pub fn focused_sentence(&self) -> Option<String> {
        let tab = self.active_tab();
        let doc = tab.doc.as_ref()?;
        let link_index = tab.focused_link?;
        let anchor = tab.links.get(link_index)?.text.clone();
        let block_text = crate::editing::block_text_containing_link(doc, link_index)?;
        let sentences = crate::editing::split_sentences(&block_text);
        crate::editing::sentence_containing(&sentences, &anchor).cloned()
    }

    /// The cache/session-state scope key (`api::wiki_scope`) for the wiki new
    /// opens currently address — i.e. the app-global active wiki (PRD
    /// FR-ML-4). A freshly installed document is stamped with this so its tab
    /// remembers the wiki it came from; an *existing* tab keeps whatever it
    /// was stamped with, so a `:wiki` switch never retroactively rebrands
    /// already-open tabs. Mirrors `client.wiki_scope()` for code that only
    /// has `&App`.
    pub fn active_wiki_scope(&self) -> &str {
        crate::api::wiki_scope(&self.active_wiki_name)
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

    /// Allocates a fresh, unfocused, blank tab — the shared plumbing behind
    /// `open_background_tab` (which additionally marks it loading with a
    /// target title) and session restore (PRD FR-TB-5, `main::
    /// restore_session_tabs`), which sets `loading`/`pending_title` itself
    /// only for a persisted tab that actually had an article open.
    pub fn push_blank_tab(&mut self, lang: String) -> TabId {
        let id = self.allocate_tab_id();
        self.tabs.push(Tab::new(id, lang));
        id
    }

    /// PRD FR-TB-3: open `title` in a new, *unfocused* background tab and
    /// return its id so the caller can spawn the keyed fetch. Focus does not
    /// move; the tab shows its target title + "…" in the bar until the fetch
    /// lands (budget-aware prefetch scheduling arrives with a later chunk —
    /// for now the fetch fires immediately via the existing channel pattern).
    pub fn open_background_tab(&mut self, title: String, lang: String) -> TabId {
        let id = self.push_blank_tab(lang);
        let tab = self
            .tabs
            .last_mut()
            .expect("push_blank_tab just pushed one");
        tab.loading = true;
        tab.pending_title = Some(title);
        // PRD FR-TB-5: a new tab is a "meaningful change" — see
        // `persist_session`'s doc comment for the full trigger list.
        self.persist_session();
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
        // Any tab close collapses an active split: its panes reference tabs by
        // id, and rather than special-case "was this one of the panes" the
        // simplest safe rule is that closing a tab drops the two-pane view.
        self.split = None;
        // PRD FR-HS-1's dwell time stops accumulating the moment a tab
        // closes; flush before the tab (and its tracking fields) are gone.
        self.flush_tab_dwell(index);
        // PRD FR-NV-8: and its reading position is saved before it's gone.
        self.save_reading_position(index);
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

    /// PRD FR-TB-5: discards every open tab and returns to exactly one
    /// blank one — the runtime `:session <name>` switch's "replace the
    /// whole tab set" step (see `main::switch_to_named_session`'s doc
    /// comment), the same vim `:source`-on-a-session-file semantics
    /// `:mksession`'s own borrowed name implies: a session switch is a
    /// wholesale replacement, not a merge with whatever was already open.
    /// Reuses [`Self::close_tab`]'s per-tab teardown (dwell-time flush,
    /// reading-position save, close-undo snapshot — `u` can still reopen
    /// any of these tabs afterward) for every tab but the last, since
    /// `close_tab` refuses to drop the app below one tab; the survivor is
    /// torn down the same way in place (`Tab::clear_to_blank`) rather than
    /// snapshotted, since it never actually closes.
    pub fn reset_to_single_blank_tab(&mut self) {
        self.split = None;
        while self.tabs.len() > 1 {
            self.close_tab(0);
        }
        self.flush_tab_dwell(0);
        self.save_reading_position(0);
        self.tabs[0].clear_to_blank();
        self.active = 0;
        self.selected_tab_pick = 0;
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
                self.split = None;
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
                self.sync_active_tab();
                true
            }
            None => false,
        }
    }

    /// `gt` — focus the next tab, wrapping (PRD FR-TB-1). Collapses any split
    /// first (see [`Self::switch_to_tab`]).
    pub fn next_tab(&mut self) {
        self.split = None;
        if self.tabs.len() < 2 {
            return;
        }
        self.active = (self.active + 1) % self.tabs.len();
        self.sync_active_tab();
    }

    /// `gT` — focus the previous tab, wrapping. Collapses any split first.
    pub fn prev_tab(&mut self) {
        self.split = None;
        if self.tabs.len() < 2 {
            return;
        }
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.sync_active_tab();
    }

    /// Tab-picker Enter — focus a tab by index.
    pub fn switch_to_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            // Explicitly choosing a different tab collapses any active split
            // (you asked to look at that tab full-width). Dropped *before* the
            // switch so `sync_active_tab`'s layout invalidation lands on the
            // full-width single-pane view.
            self.split = None;
            self.active = index;
            self.sync_active_tab();
        }
    }

    // -- Wiki switcher (PRD FR-ML-4/5) ---------------------------------------

    /// Bare `:wiki` (PRD FR-ML-4): opens the picker over Wikipedia and the
    /// four sister projects, selecting the row for the currently active wiki
    /// so a reader who already switched sees where they are rather than the
    /// cursor resetting to the top. Local state only, unlike
    /// `open_lang_picker`'s langlinks fetch — the list is the same five
    /// entries every time (`main::switch_wiki` supports switching to a
    /// custom `[wiki.<name>]` site too, via `:wiki <name>`, just not through
    /// this picker — see its own doc comment).
    pub fn open_wiki_picker(&mut self) {
        self.prior_mode = self.mode;
        self.mode = Mode::WikiPicker;
        self.selected_wiki_pick = crate::sisters::all_known_projects()
            .iter()
            .position(|p| p.name == self.active_wiki_name)
            .unwrap_or(0);
    }

    /// Moves the picker's selection, wrapping over the five known projects.
    pub fn cycle_wiki_pick(&mut self, forward: bool) {
        let len = crate::sisters::all_known_projects().len();
        self.selected_wiki_pick = if forward {
            (self.selected_wiki_pick + 1) % len
        } else {
            (self.selected_wiki_pick + len - 1) % len
        };
    }

    /// Enter's target: the registry name of the picker's highlighted row.
    pub fn wiki_picker_target(&self) -> Option<String> {
        crate::sisters::all_known_projects()
            .get(self.selected_wiki_pick)
            .map(|p| p.name.to_string())
    }

    // -- Disambiguation chooser (PRD §7) -------------------------------------

    /// The active tab's disambiguation candidates, recomputed from its
    /// document on every call (like `wiki_picker_target`'s static list)
    /// rather than cached on `App` — the list is only ever read while
    /// `Mode::Disambig` is showing, and it's cheap to re-derive from
    /// `doc::disambiguation_candidates` for the short lists a real
    /// disambiguation page has. Empty for a tab with no document, or one
    /// whose document isn't a disambiguation page at all.
    pub fn disambig_candidates(&self) -> Vec<crate::doc::DisambigCandidate> {
        self.active_tab()
            .doc
            .as_ref()
            .map(crate::doc::disambiguation_candidates)
            .unwrap_or_default()
    }

    /// Moves the chooser's selection, wrapping over however many candidates
    /// this disambiguation page has. A no-op (rather than a panic on a `% 0`)
    /// when there are none — shouldn't happen for a genuine disambiguation
    /// page, but a malformed one (no followable links in any list item)
    /// must still be navigable, not a crash.
    pub fn cycle_disambig(&mut self, forward: bool) {
        let len = self.disambig_candidates().len();
        if len == 0 {
            return;
        }
        self.disambig_selected = if forward {
            (self.disambig_selected + 1) % len
        } else {
            (self.disambig_selected + len - 1) % len
        };
    }

    /// Enter's target: the highlighted candidate's resolved internal title.
    pub fn disambig_target(&self) -> Option<String> {
        self.disambig_candidates()
            .get(self.disambig_selected)
            .map(|c| c.title.clone())
    }

    // -- Splits & bilingual view (PRD FR-TB-4, FR-ML-3) ---------------------

    /// PRD FR-TB-4 `:vsplit` / `Ctrl-w v`: split the content area into two
    /// side-by-side panes. The focused tab's document is duplicated into a
    /// fresh tab (the right pane) so each pane has independent scroll/link/fold
    /// state; focus stays on the left (original) pane. `content_width` is the
    /// current content-area width; the split is refused (PRD §6.3) when it is
    /// too narrow for two [`crate::split::MIN_PANE_WIDTH`] panes plus a
    /// divider. Returns `Err` with the user-facing reason on refusal.
    pub fn open_split(&mut self, content_width: u16) -> Result<(), String> {
        if self.split.is_some() {
            return Err("already split — :only to close it first".to_string());
        }
        if self.active_tab().doc.is_none() {
            return Err("open an article first, then :vsplit".to_string());
        }
        if !crate::split::fits(content_width) {
            return Err(format!(
                "terminal too narrow to split (need ≥ {} cols)",
                crate::split::MIN_SPLIT_WIDTH
            ));
        }
        let active_idx = self.active;
        let focused_id = self.tabs[active_idx].id;
        let (doc, revid, page_source, scroll, folded, lang) = {
            let t = &self.tabs[active_idx];
            (
                t.doc.clone(),
                t.current_revid,
                t.page_source,
                t.scroll,
                t.folded_blocks.clone(),
                t.lang.clone(),
            )
        };
        let dup_id = self.push_blank_tab(lang);
        let dup_idx = self.tabs.len() - 1;
        if let Some(doc) = doc {
            let t = &mut self.tabs[dup_idx];
            t.install_document(doc);
            t.current_revid = revid;
            t.page_source = page_source;
            t.scroll = scroll;
            t.folded_blocks = folded;
        }
        // Focus stays on the original (left) pane; `active` already points at
        // it and `push_blank_tab` appended without moving focus.
        self.split = Some(crate::split::Split::new([focused_id, dup_id]));
        self.layout = None;
        Ok(())
    }

    /// PRD FR-ML-3 `:bilingual`: install a two-pane split whose left pane is
    /// the article already active and whose right pane (`other_idx`) is the
    /// same article in another language, freshly fetched into its own tab by
    /// the caller. Focus stays on the left (original-language) pane; scroll is
    /// section-synced by default. Sets the "not translations" notice.
    pub fn begin_bilingual_split(&mut self, original_id: TabId, other_id: TabId) {
        let Some(orig_idx) = self.tab_index_by_id(original_id) else {
            return;
        };
        self.split = Some(crate::split::Split::bilingual([original_id, other_id]));
        self.active = orig_idx;
        // Not a full `sync_active_tab` (which would reset mode/persist): a
        // lighter refocus onto the original pane.
        self.lang = self.active_tab().lang.clone();
        self.rebuild_citations();
        self.layout = None;
        self.mode = Mode::Reading;
        self.notice = Some(
            "Bilingual view — these are independent articles in each language, not a translation \
             (Ctrl-w w to switch panes, :only to close)"
                .to_string(),
        );
    }

    /// PRD FR-TB-4 `Ctrl-w c` / `:only`: collapse the split back to a single
    /// pane, keeping the focused pane's tab on screen full-width. A plain
    /// `:vsplit`'s throwaway duplicate right pane is removed (no tab clutter);
    /// a `:bilingual` split's other-language pane is kept as an ordinary
    /// background tab. Returns whether a split was actually closed.
    pub fn close_split(&mut self) -> bool {
        let Some(split) = self.split.take() else {
            return false;
        };
        let focused_id = split.focused_id();
        let other_id = split.other_id();
        if !split.bilingual
            && let Some(idx) = self.tab_index_by_id(other_id)
        {
            self.tabs.remove(idx);
        }
        if let Some(idx) = self.tab_index_by_id(focused_id) {
            self.active = idx;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.sync_active_tab();
        true
    }

    /// PRD FR-TB-4 `Ctrl-w w`: toggle focus to the other pane. No-op with no
    /// split. Returns whether focus moved.
    pub fn focus_split_other(&mut self) -> bool {
        let Some(cur) = self.split.as_ref().map(|s| s.focused) else {
            return false;
        };
        self.focus_split_pane(1 - cur)
    }

    /// PRD FR-TB-4 `Ctrl-w h`/`Ctrl-w l`: focus the pane in slot `slot`
    /// (`0` = left, `1` = right), keeping the module invariant
    /// (`active` == focused pane's tab). No-op with no split or an already-
    /// focused slot. Returns whether focus moved.
    pub fn focus_split_pane(&mut self, slot: usize) -> bool {
        let id = match self.split.as_mut() {
            Some(s) if slot < 2 && slot != s.focused => {
                s.focused = slot;
                s.panes[slot]
            }
            _ => return false,
        };
        if let Some(idx) = self.tab_index_by_id(id) {
            self.active = idx;
            // A lighter refocus than `sync_active_tab`: adopt the pane's
            // language, rebuild citations for its article, and drop the cached
            // layout (the focused pane lays out at its own pane width). No mode
            // reset or session persist — a pane focus switch is intra-view.
            self.lang = self.active_tab().lang.clone();
            self.rebuild_citations();
            self.layout = None;
            self.refresh_reading_status();
        }
        true
    }

    /// PRD FR-TB-4 `:set scrollbind`: couple/uncouple the active split's two
    /// panes in lockstep. A no-op notice when there is no split. Leaves a
    /// bilingual split's default section-sync in place only when turning
    /// scrollbind *off* would otherwise strand it — off always means
    /// independent here (the explicit toggle wins).
    pub fn set_scrollbind(&mut self, on: bool) {
        use crate::split::SyncMode;
        let applied = if let Some(s) = self.split.as_mut() {
            s.sync = if on {
                SyncMode::Lockstep
            } else {
                SyncMode::Independent
            };
            true
        } else {
            false
        };
        self.notice = Some(if applied {
            format!("scrollbind {}", if on { "on" } else { "off" })
        } else {
            "scrollbind applies to a split — :vsplit or :bilingual first".to_string()
        });
    }

    /// PRD FR-ML-3: the `(language code, title-in-that-edition)` `:bilingual`
    /// opens in the second pane, chosen from the active article's cached
    /// langlinks — the first of the reader's preferred `languages` that has an
    /// edition, else the first langlink that isn't the current language.
    /// `None` when no langlinks are cached or none point elsewhere.
    pub fn bilingual_target(&self) -> Option<(String, String)> {
        let links = self.lang_links();
        if links.is_empty() {
            return None;
        }
        let cur = self.active_tab().lang.clone();
        for pref in &self.languages {
            if *pref == cur {
                continue;
            }
            if let Some(l) = links.iter().find(|l| l.code == *pref) {
                return Some((l.code.clone(), l.title.clone()));
            }
        }
        links
            .iter()
            .find(|l| l.code != cur)
            .map(|l| (l.code.clone(), l.title.clone()))
    }

    /// After any change of which tab is active: adopt the tab's language for
    /// new searches/opens, rebuild the app-global citation list from its
    /// document, drop the cached layout so the next draw rebuilds for this
    /// tab (an L1 hit, not a relayout), re-arm the SWR "updated — r to
    /// reload" notice iff this tab has one pending, land in Reading mode, and
    /// refresh the status line.
    pub(crate) fn sync_active_tab(&mut self) {
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
        // PRD FR-TB-5: a tab switch (and, via this shared chokepoint, a tab
        // close/reopen/new-tab too) is a "meaningful change" — see
        // `persist_session`'s doc comment for the full trigger list.
        self.persist_session();
    }

    // -- Session auto-restore (PRD FR-TB-5) ---------------------------------

    /// Builds the on-disk session shape from the live tab set: each tab's
    /// `(lang, title, scroll, folded sections, revid)` plus its back/forward
    /// stacks, and which tab is active. A tab with no installed document
    /// persists `title: None` unless a background fetch for it is still in
    /// flight (`pending_title`) — a permanently failed background load
    /// (`loading` false, `doc` still `None`) persists as blank rather than
    /// as its "{title} (failed)" placeholder, so a restore never retries a
    /// title that's spelled wrong or genuinely broken on every single
    /// launch.
    pub(crate) fn session_snapshot(&self) -> session::SessionState {
        session::SessionState {
            active: self.active,
            tabs: self
                .tabs
                .iter()
                .map(|t| {
                    let title = match &t.doc {
                        Some(doc) => Some(doc.title.clone()),
                        None if t.loading => t.pending_title.clone(),
                        None => None,
                    };
                    let mut folded_blocks: Vec<usize> = t.folded_blocks.iter().copied().collect();
                    folded_blocks.sort_unstable();
                    session::SessionTab {
                        lang: t.lang.clone(),
                        wiki: t.wiki.clone(),
                        title,
                        scroll: t.scroll,
                        folded_blocks,
                        current_revid: t.current_revid,
                        back_stack: t.back_stack.clone(),
                        forward_stack: t.forward_stack.clone(),
                    }
                })
                .collect(),
        }
    }

    /// Writes the current tab set to `session_path` (PRD FR-TB-5) — a
    /// complete, atomic snapshot (`session::save`'s temp+fsync+rename, see
    /// its doc comment) called from every "meaningful change" chokepoint:
    /// `sync_active_tab` (tab switch/close/reopen/new-tab), `set_document`
    /// (fresh open, following a link, a bookmark/history-picker reopen, the
    /// SWR "r to reload"), `open_background_tab`, fold toggles
    /// (`toggle_fold_at_cursor`/`fold_all`/`unfold_all`), and
    /// `resume_to_saved_position`. Deliberately *not* called on every scroll
    /// tick or keystroke — that would mean a disk write on every `j`/`k` —
    /// so the guarantee is "a crash loses at most whatever changed since the
    /// last meaningful-change save," not "at most nothing." `open_history_entry`
    /// (`main.rs`, back/forward) additionally calls this a second time after
    /// restoring its historical scroll, since that assignment happens after
    /// `set_document` (which resets scroll to 0) already fired this once.
    ///
    /// PRD FR-PR-3's privacy gate: never writes while incognito. Silently a
    /// no-op when `session_path` is `None` (no platform state directory, or
    /// a test that never opted in) — the same "storage is best-effort,
    /// reading must never depend on it" posture every other store in this
    /// codebase already has.
    pub fn persist_session(&self) {
        if self.incognito {
            return;
        }
        let Some(path) = &self.session_path else {
            return;
        };
        let _ = session::save(&self.session_snapshot(), path);
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

    /// The layout options derived from the current reading preferences —
    /// PRD FR-PC-4's merge point: **config < session (`:set`, the plain
    /// `self.*` fields, themselves seeded from config at startup) < this
    /// tab's `TabOverrides`**. Every overridable field reads the tab's own
    /// override first and only falls back to the session-global field when
    /// the tab has never set one (`Option::unwrap_or`), so a tab with no
    /// overrides at all renders exactly like the session default, and a tab
    /// with one override still inherits every other setting from the
    /// session — never a partial/stale mix of old and new values.
    pub fn layout_options(&self) -> LayoutOptions {
        self.layout_options_for(self.active_tab())
    }

    /// The effective [`LayoutOptions`] for a *specific* tab: its own per-tab
    /// overrides (PRD FR-PC-4) layered over the session-global settings.
    /// Factored out of [`Self::layout_options`] so split rendering can lay a
    /// non-active pane out with that pane's own overrides
    /// ([`Self::layout_for_tab`]).
    pub fn layout_options_for(&self, tab: &Tab) -> LayoutOptions {
        let ov = &tab.overrides;
        LayoutOptions {
            measure: ov.measure.unwrap_or(self.measure),
            ambiguous_wide: ov.ambiguous_wide.unwrap_or(self.ambiguous_wide),
            accessible: self.accessible,
            table_col_offset: tab.table_col_offset,
            image_epoch: self.image_epoch,
            reading_wpm: self.reading_wpm,
            images_on: self.images_enabled(),
            text_align: ov.text_align.unwrap_or(self.text_align),
            margin: ov.margin.unwrap_or(self.margin),
            paragraph_spacing: ov.paragraph_spacing.unwrap_or(self.paragraph_spacing),
            line_spacing: ov.line_spacing.unwrap_or(self.line_spacing),
            word_spacing: ov.word_spacing.unwrap_or(self.word_spacing),
            justify: ov.justify.unwrap_or(self.justify),
            hyphenate: ov.hyphenate.unwrap_or(self.hyphenate),
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
        // PRD FR-NV-3: the active tab's folded-heading set is a layout input,
        // so a fold/unfold makes a cached layout stale exactly like a
        // width/options change does.
        let folds = self.folds_sorted();
        let stale = match &self.layout {
            Some(l) => l.width != width || l.options != opts || l.folds != folds,
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
                folds: folds.clone(),
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
            layout::layout_document_with_images(doc, width, opts, &img_map, &folds)
        };
        self.layout_cache.put(key, computed.clone());
        self.layout = Some(computed);
    }

    /// PRD FR-TB-4 split rendering: the width-aware layout for the tab at
    /// `tab_idx` laid out at `width`, honoring that tab's own folds and
    /// per-tab overrides — the split-pane analogue of [`Self::ensure_layout`],
    /// but for an arbitrary (possibly non-active) tab and width, returning the
    /// layout rather than storing it in `self.layout`. Shares the one L1 cache
    /// (`layout_cache`), which keys on `(lang, title, revid, width, options,
    /// folds)`, so laying a pane out at half width is an ordinary cache
    /// lookup/miss — the same machinery single-pane reading uses, just at a
    /// narrower `width`. Returns `None` when the tab has no document.
    ///
    /// A pane's `width` is simply its (narrower) share of the content area:
    /// the FR-RD-9 measure (default 88) is applied inside the layout engine as
    /// `min(width, measure)`, so at a typical ~49-col half-pane the measure
    /// never binds and the pane just uses its full width — exactly the
    /// documented behavior for splits.
    pub fn layout_for_tab(&mut self, tab_idx: usize, width: u16) -> Option<Layout> {
        let (opts, folds, key) = {
            let tab = self.tabs.get(tab_idx)?;
            let doc = tab.doc.as_ref()?;
            let opts = self.layout_options_for(tab);
            let mut folds: Vec<usize> = tab.folded_blocks.iter().copied().collect();
            folds.sort_unstable();
            let key = layout::LayoutCacheKey {
                lang: tab.lang.clone(),
                title: doc.title.clone(),
                revid: tab.current_revid,
                width,
                options: opts,
                folds: folds.clone(),
                schema_version: layout::LAYOUT_SCHEMA_VERSION,
            };
            (opts, folds, key)
        };
        if let Some(cached) = self.layout_cache.get(&key) {
            return Some(cached);
        }
        let img_map = {
            let tab = &self.tabs[tab_idx];
            self.image_box_map_for(tab, width, opts.measure)
        };
        self.layout_computations += 1;
        let computed = {
            let doc = self.tabs[tab_idx]
                .doc
                .as_ref()
                .expect("keyed above, so a document is present");
            layout::layout_document_with_images(doc, width, opts, &img_map, &folds)
        };
        self.layout_cache.put(key, computed.clone());
        Some(computed)
    }

    /// PRD FR-TH-7 / FR-RD-8 / FR-PC-4: whether inline images render right
    /// now for the *active tab* — that tab's own `:set-tab images=`
    /// override if it has one, else the session-global runtime `:set
    /// images=` override, else the active theme's default. Same three-rung
    /// precedence as `layout_options`.
    pub fn images_enabled(&self) -> bool {
        self.active_tab()
            .overrides
            .images
            .unwrap_or(self.images_override.unwrap_or(self.theme.images))
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
        self.image_box_map_for(
            self.active_tab(),
            self.layout_width,
            self.layout_options().measure,
        )
    }

    /// The reserved image boxes for a *specific* tab's document at `width`
    /// with the effective `measure` (PRD FR-RD-8). Factored out so a split
    /// pane reserves boxes sized to its own (narrower) width
    /// ([`Self::layout_for_tab`]); [`Self::image_box_map`] is the active-tab,
    /// full-width caller.
    fn image_box_map_for(
        &self,
        tab: &Tab,
        width: u16,
        measure: u16,
    ) -> std::collections::HashMap<String, (u16, u16)> {
        let mut map = std::collections::HashMap::new();
        if matches!(
            self.graphics_protocol(),
            crate::graphics::GraphicsProtocol::None
        ) {
            return map;
        }
        let content_width = (width as usize).min(measure as usize);
        let max_cols = content_width.min(layout::IMAGE_MAX_COLS as usize) as u16;
        if max_cols < layout::IMAGE_MIN_COLS {
            return map;
        }
        if let Some(doc) = tab.doc.as_ref() {
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

    /// PRD FR-PC-4's `:set-tab <key>=<value>` / `:set-tab <key>=`: sets (or,
    /// given `None`, clears) one render-option override on the *active* tab
    /// only — every other open tab is untouched, and this tab falls straight
    /// back to the session-global setting the moment the override is
    /// cleared. `key` is one of `command::TAB_SCOPED_KEYS`; the value string
    /// was already validated by the command parser, so parsing here is
    /// infallible-in-practice the same way `execute_command`'s `Command::
    /// Set` arms treat their own pre-validated values. Always forces a
    /// relayout (`self.layout = None`) since every one of these keys feeds
    /// `layout_options`/`images_enabled`.
    pub fn set_tab_override(&mut self, key: &str, value: Option<&str>) {
        let tab = self.active_tab_mut();
        match key {
            "measure" => tab.overrides.measure = value.and_then(|v| v.parse().ok()),
            "ambiguous_width" => tab.overrides.ambiguous_wide = value.map(|v| v == "2"),
            "images" => tab.overrides.images = value.map(|v| v == "on"),
            "text_align" => tab.overrides.text_align = value.and_then(layout::TextAlign::parse),
            "margin" => tab.overrides.margin = value.and_then(|v| v.parse().ok()),
            "paragraph_spacing" => {
                tab.overrides.paragraph_spacing = value.and_then(|v| v.parse().ok())
            }
            "line_spacing" => tab.overrides.line_spacing = value.and_then(|v| v.parse().ok()),
            "word_spacing" => tab.overrides.word_spacing = value.and_then(|v| v.parse().ok()),
            "justify" => tab.overrides.justify = value.map(|v| v == "on"),
            "hyphenate" => tab.overrides.hyphenate = value.map(|v| v == "on"),
            _ => {}
        }
        self.layout = None;
        self.notice = Some(match value {
            Some(v) => format!("{key}={v} (this tab)"),
            None => format!("{key}: reset to session default (this tab)"),
        });
    }

    /// Switch the color theme, relayouting only if the change flips whether
    /// images render (PRD FR-TH-7): image boxes depend on the theme's
    /// `images` default, so a text↔image theme swap must rebuild the layout,
    /// while any other theme swap stays O(paint) as before.
    ///
    /// PRD FR-TH-3: also the single choke point capability degradation runs
    /// through — every caller (startup, `T`-cycling, `:theme`, `:set
    /// theme=`, `:config reload`) hands this an always-truecolor `Theme`
    /// (from `Theme::by_name`/`theme::resolve_named`/`Theme::next`), and
    /// this maps it down to `self.color_depth` before storing it, honoring
    /// the theme's own declared `[fallback]` (found by name in
    /// `self.user_themes`) ahead of computed quantization. `ui::colored`/
    /// `base_style` never see a depth or a fallback table — only the
    /// already-adapted colors on `self.theme`.
    pub fn set_theme(&mut self, theme: Theme) {
        let before = self.images_enabled();
        let fallback = self
            .user_themes
            .iter()
            .find(|t| t.name == theme.name)
            .map(|t| &t.fallback);
        self.theme = theme.adapt(self.color_depth, fallback);
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
        // PRD FR-NV-8: and its scroll/fold position is saved before the new
        // document replaces it — this is the "navigating away" save point.
        self.save_reading_position(index);
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
            .map(|e| (e.wiki.clone(), e.lang.clone(), e.title.clone()));

        // Element 0 is always this article's own citation; the rest are
        // whatever it cites (Research mode, PRD-adjacent feature request).
        let mut citations = vec![crate::research::self_citation(&doc.title, &lang)];
        citations.extend(doc.citations.iter().cloned());
        self.citations = citations;
        self.selected_citation = 0;

        // PRD §7 "Disambiguation page": captured before `install_document`
        // (below) moves `doc` in — this decides the mode switch a few lines
        // down, the one place every document-install path (fresh
        // navigation, back/forward, bookmark/read-later/history reopen, the
        // SWR "r to reload") funnels through, so the chooser shows up no
        // matter how the disambiguation page was reached.
        let is_disambiguation = doc.is_disambiguation;

        let wiki = self.active_wiki_scope().to_string();
        {
            let tab = self.active_tab_mut();
            tab.lang = lang;
            // PRD FR-ML-4: stamp the tab with the wiki this open addressed, so
            // every cache read/write and session-state lookup it later drives
            // keys on its own wiki — never on a subsequently-switched active
            // wiki. Back/forward overrides this afterward for a cross-wiki
            // history entry (see `main::open_history_entry`).
            tab.wiki = wiki;
            tab.install_document(doc);
        }
        self.record_history_visit(index, referrer);
        // PRD FR-NV-8: offer to resume if this article has a saved position —
        // raised after the visit is recorded and before the status refresh
        // below, so its toast (a `notice`) is the last word for this open.
        self.check_resume_position(index);
        if is_disambiguation {
            self.disambig_selected = 0;
            self.mode = Mode::Disambig;
        } else {
            self.mode = Mode::Reading;
        }
        // A new document invalidates the cached layout; it is rebuilt lazily
        // (from L1 if available, else a fresh layout pass) on the next draw
        // or mapping lookup at the current width — see `ensure_layout`.
        self.layout = None;
        // PRD FR-ML-2: whatever hint was showing belonged to the article
        // just navigated away from — clearing it here (rather than leaving
        // it to the new article's own langlinks fetch to overwrite) means
        // the status bar never briefly shows a stale "also in X" for the
        // wrong page between this install and that fetch landing.
        self.language_hint = None;
        self.refresh_reading_status();
        // PRD FR-TB-5: installing a document is a "meaningful change" — see
        // `persist_session`'s doc comment for the full trigger list and why
        // back/forward navigation additionally calls this a second time
        // after restoring its historical scroll.
        self.persist_session();
    }

    // -- Reading history (PRD FR-HS-1/2/4) ---------------------------------

    /// Records a visit to whatever document is now installed at
    /// `tabs[index]` (PRD FR-HS-1), unless `privacy::decide` denies it
    /// (`Write::History` is passive — PRD FR-PR-3 — so incognito always
    /// denies it; this is the one gate every write in this module routes
    /// through). Starts that tab's dwell clock running from now. Called by
    /// `set_document` for the active tab and by `main::apply_tab_load_outcome`
    /// for a background tab's fetch completion — the only two places a
    /// document is ever installed. A no-op if the tab index is gone or has
    /// no document (nothing to record).
    pub(crate) fn record_history_visit(
        &mut self,
        index: usize,
        referrer: Option<(String, String, String)>,
    ) {
        if crate::privacy::decide(self.incognito, crate::privacy::Write::History)
            == crate::privacy::Verdict::Deny
        {
            return;
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(title) = tab.doc.as_ref().map(|d| d.title.clone()) else {
            return;
        };
        let lang = tab.lang.clone();
        // PRD FR-ML-4/FR-HS-3: the tab's own wiki (already stamped with this
        // article's scope by `set_document`, which runs before this call —
        // see its doc comment) so the trail view's nodes/edges carry the
        // right wiki, never whichever wiki happens to be active *now*.
        let wiki = tab.wiki.clone();
        let referrer_ref = referrer
            .as_ref()
            .map(|(w, l, t)| (w.as_str(), l.as_str(), t.as_str()));
        let id = self
            .history
            .record_visit(&wiki, &lang, &title, referrer_ref);
        let tab = &mut self.tabs[index];
        tab.history_visit_id = id;
        tab.visit_started_at = Some(std::time::Instant::now());
        // PRD FR-DL-8: every recorded visit can newly cross an achievement
        // threshold — checked here, the one place both the active-tab and
        // background-tab-load install paths funnel through, same posture as
        // `check_resume_position` above it in `set_document`.
        self.check_achievements();
    }

    /// PRD FR-DL-8: after a visit is recorded, checks whether this session's
    /// trail (`trail::stats`, the seam `trail.rs` left for this) just
    /// crossed a new achievement threshold and, unless `pro` disabled
    /// easter eggs, toasts the first newly-crossed one (`self.notice`) —
    /// "fires once, not every nav": `achievements_shown` remembers which ids
    /// already toasted this session, so a stat that stays above its
    /// threshold on every later visit never toasts again. Rebuilds the
    /// session trail from scratch each call (bounded by `session_started_at`,
    /// not full history) rather than maintaining incremental counters —
    /// simple and correct; a long enough session for this to matter is not
    /// this feature's concern.
    fn check_achievements(&mut self) {
        if self.pro {
            return;
        }
        let visits = self.trail_scoped_visits(crate::command::TrailScope::Session);
        let trail = crate::trail::build(&visits);
        let stats = crate::trail::stats(&trail);
        let newly = crate::achievements::newly_crossed(&stats, &self.achievements_shown);
        if let Some(achievement) = newly.first() {
            self.achievements_shown.insert(achievement.id);
            self.notice = Some(format!("Achievement unlocked: {}", achievement.message));
        }
    }

    /// PRD FR-DL-6: advances the active wiki-walk (if any) after a link
    /// follow installed a new document in the active tab — the shared tail
    /// `main::follow_internal_link` calls for every `counts_toward_game`
    /// caller. Reads the *active tab's* now-installed document title (the
    /// real, possibly-redirected title, matching what `record_history_visit`
    /// itself just recorded) rather than the raw string the reader clicked,
    /// so a redirect still resolves goal-matching correctly. A no-op with no
    /// game active or no document installed.
    pub fn record_game_move(&mut self) {
        let Some(title) = self.active_tab().doc.as_ref().map(|d| d.title.clone()) else {
            return;
        };
        let Some(game) = &mut self.game else {
            return;
        };
        let was_won = game.won;
        game.follow(title);
        if game.won && !was_won {
            self.notice = Some(format!("You won! {}", crate::game::share_card(game)));
        }
    }

    /// PRD FR-DL-4's `]c`/`[c` jump (armed only while `show_cn` is on — see
    /// `App::pending_cn_bracket`'s doc comment): moves the active tab's
    /// scroll to the next/previous citation-needed marker relative to the
    /// current scroll offset, wrapping around either end, and centers it
    /// (`center_scroll`, the same treatment `find_next`/`find_prev` give a
    /// find match). A `notice` when the page has no markers at all.
    pub fn jump_citation_needed(&mut self, forward: bool) {
        self.ensure_layout();
        let Some(layout) = &self.layout else {
            return;
        };
        let lines = crate::layout::citation_needed_lines(&layout.lines, &layout.continuation);
        if lines.is_empty() {
            self.notice = Some("No citation-needed markers on this page".to_string());
            return;
        }
        let current = self.active_tab().scroll;
        let target = if forward {
            lines
                .iter()
                .copied()
                .find(|&l| l as u16 > current)
                .unwrap_or(lines[0])
        } else {
            lines
                .iter()
                .copied()
                .rev()
                .find(|&l| (l as u16) < current)
                .unwrap_or(*lines.last().expect("checked non-empty above"))
        };
        let scroll = self.center_scroll(target as u16);
        self.active_tab_mut().scroll = scroll;
    }

    /// `:set show-cn`/`:set noshow-cn` (PRD FR-DL-4): toggles citation-needed
    /// highlighting. No relayout needed — `SpanKind::CitationNeeded` is
    /// theme/toggle-independent (PRD §6.3's "theme applied at paint time");
    /// only `ui::kind_style`'s dim-or-plain decision and the status-bar count
    /// depend on this flag, both read fresh every draw.
    pub fn set_show_cn(&mut self, on: bool) {
        self.show_cn = on;
        self.notice = Some(format!("show-cn={}", if on { "on" } else { "off" }));
    }

    /// Whether the active tab's document carries at least one
    /// citation-needed marker (PRD FR-DL-4). `main::handle_key`'s `]`/`[`
    /// arms only arm `pending_cn_bracket` while this is true (UX-1 fix): with
    /// `show_cn` on but a page with no markers at all, the jump chord could
    /// never do anything, so arming it just to eat the reader's very next
    /// keystroke for nothing was a pure regression, not a real feature.
    pub fn doc_has_citation_needed(&self) -> bool {
        self.active_tab()
            .doc
            .as_ref()
            .is_some_and(|d| crate::doc::count_citation_needed(d) > 0)
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
            let dwell = started.elapsed().as_secs();
            self.history.update_dwell(id, dwell);
            // PRD FR-PF-3: the normalized dwell interest signal, applied once
            // per read (the flag guards repeated flushes from re-adding it).
            self.apply_dwell_signal(index, dwell);
        }
    }

    /// PRD FR-PF-3's dwell signal: `normalized_dwell(dwell, expected_read_time)`
    /// (≤ +2.0) added to the article's topics, once per read. `expected_read_
    /// time` is the FR-RD-11 estimate (word count ÷ WPM), so a fully-read
    /// article contributes the whole +2.0 and a glance proportionally less.
    /// A no-op when learning is off, already applied, or the dwell is zero.
    fn apply_dwell_signal(&mut self, index: usize, dwell_secs: u64) {
        if !self.interest_active() {
            return;
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.interest_dwell_signaled {
            return;
        }
        let Some(doc) = tab.doc.as_ref() else {
            return;
        };
        let title = doc.title.clone();
        let wiki = tab.wiki.clone();
        let words = crate::doc::word_count(doc);
        let expected = (words as f64 / self.reading_wpm.max(1) as f64) * 60.0;
        let amount = crate::interest::normalized_dwell(dwell_secs as i64, expected);
        if amount <= 0.0 {
            return;
        }
        let changed =
            self.interest
                .apply_signal_for_title(&wiki, &title, amount, Self::interest_now());
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.interest_dwell_signaled = true;
        }
        if changed {
            self.persist_interest();
        }
    }

    /// Flushes dwell time for every open tab (PRD FR-HS-1): called once,
    /// right before the app exits — "the tab closes or the app exits" from
    /// the requirement's dwell-tracking wording. Closing an individual tab
    /// mid-session goes through `close_tab`, which flushes just that one.
    pub fn flush_all_tab_dwell(&mut self) {
        for index in 0..self.tabs.len() {
            self.flush_tab_dwell(index);
            // PRD FR-NV-8: the app is exiting — persist every tab's reading
            // position, not just its dwell.
            self.save_reading_position(index);
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
            wiki: visit.wiki.clone(),
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

    // -- Trail / wander-graph view (PRD FR-HS-3) ----------------------------

    /// The visits `open_trail`'s scope selects, oldest first (matching
    /// `history::History::all_visits`'s own order, which `trail::build`
    /// needs — see its doc comment).
    fn trail_scoped_visits(&self, scope: crate::command::TrailScope) -> Vec<crate::history::Visit> {
        let visits = self.history.all_visits();
        match scope {
            crate::command::TrailScope::Session => visits
                .into_iter()
                .filter(|v| v.opened_at >= self.session_started_at)
                .collect(),
            crate::command::TrailScope::All => visits,
            crate::command::TrailScope::Days(days) => {
                let cutoff = crate::history::now_unix() - i64::from(days) * 86_400;
                visits
                    .into_iter()
                    .filter(|v| v.opened_at >= cutoff)
                    .collect()
            }
        }
    }

    /// `:trail [all|days N]` (PRD FR-HS-3): builds the wander graph from
    /// whichever visits `scope` selects (default, bare `:trail`: this run's
    /// own session) and opens the navigable tree view — always resets
    /// `trail_layout` to `Tree`, so a stray `:trail` after a `:trail dag`
    /// session reliably lands back on the default, never leaving the DAG
    /// layout silently active under the plain command's own status text.
    pub fn open_trail(&mut self, scope: crate::command::TrailScope) {
        self.trail_layout = crate::trail::TrailLayout::Tree;
        self.open_trail_common(scope);
    }

    /// `:trail dag [all|days N]` (PRD FR-HS-3 v2): identical scope handling
    /// to [`Self::open_trail`], but opens the true-DAG view — a node with
    /// more than one referrer shows every parent instead of one plus an
    /// "also from" note.
    pub fn open_trail_dag(&mut self, scope: crate::command::TrailScope) {
        self.trail_layout = crate::trail::TrailLayout::Dag;
        self.open_trail_common(scope);
    }

    /// Shared build-and-open steps behind [`Self::open_trail`]/
    /// [`Self::open_trail_dag`] — everything except which `trail_layout`
    /// the caller already set.
    fn open_trail_common(&mut self, scope: crate::command::TrailScope) {
        self.trail_prior_mode = self.mode;
        let visits = self.trail_scoped_visits(scope);
        self.trail = crate::trail::build(&visits);
        self.trail_selected = 0;
        self.mode = Mode::Trail;
        self.status = if self.trail.graph.nodes.is_empty() {
            "No trail yet — open an article and follow a few links".to_string()
        } else {
            "j/k: move   Enter: reopen   Esc: close".to_string()
        };
    }

    pub fn close_trail(&mut self) {
        self.mode = self.trail_prior_mode;
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    /// The article keys of whichever line list `trail_layout` currently
    /// renders, in display order — the one thing `cycle_trail`/
    /// `selected_trail_target` need and the one place they'd otherwise have
    /// to duplicate the `Tree`/`Dag` branch.
    fn trail_line_articles(&self) -> Vec<crate::trail::ArticleKey> {
        match self.trail_layout {
            crate::trail::TrailLayout::Tree => crate::trail::flatten(&self.trail.tree)
                .into_iter()
                .map(|l| l.article)
                .collect(),
            crate::trail::TrailLayout::Dag => crate::trail::dag_from_graph(&self.trail.graph)
                .nodes
                .into_iter()
                .map(|n| n.article)
                .collect(),
        }
    }

    /// Moves the picker's selection, wrapping — a no-op with nothing shown.
    pub fn cycle_trail(&mut self, forward: bool) {
        let len = self.trail_line_articles().len();
        if len == 0 {
            return;
        }
        self.trail_selected = if forward {
            (self.trail_selected + 1) % len
        } else {
            (self.trail_selected + len - 1) % len
        };
    }

    /// The `(wiki, lang, title)` of the currently-selected trail line, for
    /// `main::handle_key`'s Enter arm to reopen (mirrors
    /// `App::selected_saved_target`'s "plain data getter, main.rs does the
    /// actual fetch" split).
    pub fn selected_trail_target(&self) -> Option<(String, String, String)> {
        self.trail_line_articles()
            .get(self.trail_selected)
            .map(|a| (a.wiki.clone(), a.lang.clone(), a.title.clone()))
    }

    /// `:trail export md|dot|mermaid [path]` (PRD FR-HS-3). Always exports
    /// **this run's own session** trail, freshly built — independent of
    /// whatever scope a currently-open `:trail`/`:trail all`/`:trail days`
    /// view happens to be showing. This keeps the export predictable (it
    /// never depends on "did I open `:trail` first, and with which scope")
    /// and matches the PRD's stated default: session-scope is the trail
    /// export's *only* scope, exactly as `:trail export` takes no scope
    /// argument of its own.
    pub fn export_trail(&mut self, format: &str, path: Option<&std::path::Path>) {
        let visits = self.trail_scoped_visits(crate::command::TrailScope::Session);
        let trail = crate::trail::build(&visits);
        if trail.graph.nodes.is_empty() {
            self.notice = Some("Nothing to export — no trail yet".to_string());
            return;
        }
        let target = match path {
            Some(p) => p.to_path_buf(),
            None => match crate::trail_export::default_export_path(format) {
                Some(p) => p,
                None => {
                    self.notice = Some(format!(
                        "unknown export format {format:?} — one of: {}",
                        crate::trail_export::FORMATS.join(", ")
                    ));
                    return;
                }
            },
        };
        let Some(content) = crate::trail_export::render(&trail, format, &crate::research::today())
        else {
            self.notice = Some(format!(
                "unknown export format {format:?} — one of: {}",
                crate::trail_export::FORMATS.join(", ")
            ));
            return;
        };
        if target.exists()
            && self.pending_trail_export_overwrite.as_deref() != Some(target.as_path())
        {
            self.pending_trail_export_overwrite = Some(target.clone());
            self.notice = Some(format!(
                "{} already exists — run the export again to overwrite",
                target.display()
            ));
            return;
        }
        self.pending_trail_export_overwrite = None;
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        self.notice = Some(match std::fs::write(&target, content) {
            Ok(()) => format!(
                "Exported the trail ({} articles) to {}",
                trail.graph.nodes.len(),
                target.display()
            ),
            Err(e) => format!("Export failed: {e}"),
        });
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
        // The reload reads L2 in the active tab's own wiki scope (PRD
        // FR-ML-4) — the wiki the revalidation wrote this content under.
        let wiki = self.active_tab().wiki.clone();
        if let Some(page) = cache.get(&wiki, &pending.lang, &pending.title) {
            let document = crate::doc::parse_article_html(&pending.title, &page.html);
            {
                let tab = self.active_tab_mut();
                tab.current_revid = page.revid;
                tab.page_source = PageSource::Live;
            }
            // PRD FR-NV-8: the SWR reload reinstalls the same article the
            // reader is already looking at — the resume toast would be noise.
            self.suppress_resume_once = true;
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
    /// its references) to the research bibliography. PRD FR-PR-3: an
    /// explicit save (the reader chose this exact citation), so incognito
    /// never suppresses it — only warns, via `privacy::
    /// append_warning_if_needed` (see `toggle_bookmark`'s doc comment for
    /// the same reasoning).
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
        // A `notice`, not `status` — Research mode's own status-bar arm is a
        // static hint string that never reads `status` at all (see
        // `ui::status_bar_text`), so this confirmation (and the incognito
        // warning `append_warning_if_needed` may append) would otherwise be
        // computed and then silently never drawn.
        self.notice = Some(crate::privacy::append_warning_if_needed(
            self.incognito,
            crate::privacy::Write::Citation,
            format!(
                "Saved to research collection ({} total)",
                self.research.citations.len()
            ),
        ));
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
        // PRD FR-NV-3: "in-page search auto-unfolds hits" — a match inside a
        // folded section is invisible to `find_matches` (it laid out no
        // lines), so any folded section whose body contains the query is
        // unfolded first, then the layout is rebuilt below.
        let query = self.active_tab().find_input.clone();
        self.auto_unfold_for_find(&query);
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

    /// `gs` (PRD FR-NV-2): opens the fuzzy section jump over the active
    /// tab's outline. Armed only from Reading (`resolve_g_prefix`'s `gs`),
    /// so unlike `open_lang_picker`/`open_palette` there's no prior mode to
    /// capture — Esc and a confirmed jump both land back in
    /// [`Mode::Reading`] unconditionally, the same contract `jump_to_section`
    /// already has.
    pub fn open_section_jump(&mut self) {
        self.section_jump_input.clear();
        self.section_jump_selected = 0;
        self.mode = Mode::SectionJump;
        self.status = if self.active_tab().sections.is_empty() {
            "No sections on this page — Esc to close".to_string()
        } else {
            "Type to filter   Enter: jump   Esc: cancel".to_string()
        };
    }

    /// The live, fuzzy-filtered section rows for the current query (PRD
    /// FR-NV-2) — see [`filter_sections`]. Each entry is `(original index
    /// into `Tab::sections`, cloned `SectionRef`)`, so a caller has both the
    /// index `jump_to_section` needs and the section to render/display
    /// without a second lookup.
    pub fn section_jump_rows(&self) -> Vec<(usize, crate::doc::SectionRef)> {
        filter_sections(&self.active_tab().sections, &self.section_jump_input)
            .into_iter()
            .map(|i| (i, self.active_tab().sections[i].clone()))
            .collect()
    }

    /// Move the section-jump selection, clamped to the current match count —
    /// mirrors `palette_move`.
    pub fn section_jump_move(&mut self, delta: i32) {
        let len = self.section_jump_rows().len();
        if len == 0 {
            self.section_jump_selected = 0;
            return;
        }
        let max = len - 1;
        self.section_jump_selected =
            (self.section_jump_selected as i32 + delta).clamp(0, max as i32) as usize;
    }

    /// Enter's action: jump to the highlighted row, or just return to
    /// Reading when the filter matched nothing (mirrors
    /// `jump_to_section`'s own "index out of range is a no-op scroll, not a
    /// panic" contract, extended to "nothing to jump to at all").
    pub fn confirm_section_jump(&mut self) {
        match self.section_jump_rows().get(self.section_jump_selected) {
            Some((index, _)) => self.jump_to_section(*index),
            None => self.mode = Mode::Reading,
        }
    }

    /// PRD FR-CS-2's `:` command-line Tab: completes the COMMAND NAME while
    /// the cursor is still in the first word (synchronous, from
    /// `command::complete_command_name` — the documented priority), or
    /// cycles/fetches TITLE completions once the word is `open`/`o` and a
    /// title has started (a best-effort, on-demand fill — see
    /// `command_typeahead_slot`'s own doc comment for why this stays
    /// deliberately lighter-weight than Search's per-keystroke typeahead).
    /// Returns the title to fetch typeahead suggestions for when one is
    /// needed and none is already in flight — `main::handle_key` is what
    /// owns `client` and actually fires it, mirroring `open_lang_picker`'s
    /// "App decides, caller fetches" split.
    pub fn complete_command_tab(&mut self) -> Option<String> {
        let input = self.command_input.clone();
        let (word, rest) = match input.split_once(char::is_whitespace) {
            Some((w, r)) => (w.to_string(), Some(r.trim().to_string())),
            None => (input, None),
        };

        let Some(rest) = rest else {
            // Still typing the command word itself. When the input already
            // holds one of the cached candidates verbatim — the previous
            // Tab just installed it — this press continues that cycle;
            // comparing against what's actually displayed (rather than a
            // separately tracked "query" string) means it can't drift out
            // of sync with `command_input` the way re-deriving "the prefix"
            // from an already-completed word would.
            if self.command_completions.get(self.command_completion_index) != Some(&word) {
                let matches = crate::command::complete_command_name(&word);
                if matches.is_empty() {
                    return None;
                }
                self.command_completions = matches.into_iter().map(str::to_string).collect();
                self.command_completion_index = 0;
            } else {
                self.command_completion_index =
                    (self.command_completion_index + 1) % self.command_completions.len();
            }
            self.command_input = self.command_completions[self.command_completion_index].clone();
            return None;
        };

        if !(word == "open" || word == "o") || rest.is_empty() {
            return None;
        }

        // Continuing an existing title cycle: same "compare against what's
        // displayed" test as the command-word branch above.
        if self.command_completions.get(self.command_completion_index) == Some(&rest) {
            self.command_completion_index =
                (self.command_completion_index + 1) % self.command_completions.len();
            self.command_input = format!(
                "{word} {}",
                self.command_completions[self.command_completion_index]
            );
            return None;
        }

        // A landed fetch for this exact title-so-far takes precedence over
        // firing a new one; a fetch for some other (since-typed-past) title
        // is discarded rather than shown.
        if let Some((query, titles)) = self.command_typeahead_slot.lock().unwrap().take()
            && query == rest
        {
            self.command_completions = titles;
            self.command_completion_index = 0;
            self.command_typeahead_loading = false;
            if let Some(first) = self.command_completions.first() {
                self.command_input = format!("{word} {first}");
            }
            return None;
        }

        self.command_typeahead_loading = true;
        Some(rest)
    }

    pub fn cycle_link(&mut self, forward: bool) {
        let len = self.active_tab().links.len();
        if len == 0 {
            self.status = "No links on this page".to_string();
            return;
        }
        // PRD FR-NV-3: links inside a folded section aren't laid out, so Tab
        // cycling skips them (`Layout::link_visible`). With nothing folded
        // every link is visible and this is the plain wrap-around it always was.
        self.ensure_layout();
        let visible: Vec<usize> = match &self.layout {
            Some(l) => (0..len)
                .filter(|&i| l.link_visible.get(i).copied().unwrap_or(true))
                .collect(),
            None => (0..len).collect(),
        };
        if visible.is_empty() {
            self.status = "No visible links — unfold a section (zR) first".to_string();
            return;
        }
        let next = match self.active_tab().focused_link {
            None => visible[if forward { 0 } else { visible.len() - 1 }],
            Some(cur) => match visible.iter().position(|&v| v == cur) {
                Some(pos) if forward => visible[(pos + 1) % visible.len()],
                Some(pos) => visible[(pos + visible.len() - 1) % visible.len()],
                // The focused link was just folded away: land on the first
                // (or last) visible one rather than nowhere.
                None => visible[if forward { 0 } else { visible.len() - 1 }],
            },
        };
        self.active_tab_mut().focused_link = Some(next);
        self.scroll_focused_link_into_view();
    }

    // -- Section folding (PRD FR-NV-3) --------------------------------------

    /// The active tab's folded-heading-block set as a sorted vec — the shape
    /// `layout_document_with_images` and the L1 cache key both take.
    pub fn folds_sorted(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.active_tab().folded_blocks.iter().copied().collect();
        v.sort_unstable();
        v
    }

    /// The section (index into the active tab's `sections`) whose range
    /// contains the current scroll position — the one `za` folds. Defined as
    /// the last section whose heading line is at or above the scroll line; the
    /// first section when the scroll sits in the lead above every heading, so
    /// `za` always has something to act on when the article has sections at
    /// all. `None` only for a section-less article.
    fn current_section_index(&mut self) -> Option<usize> {
        self.ensure_layout();
        let scroll = self.active_tab().scroll as usize;
        let block_lines = self.layout.as_ref().map(|l| l.block_lines.clone())?;
        let tab = self.active_tab();
        if tab.sections.is_empty() {
            return None;
        }
        let mut current = 0usize;
        for (i, section) in tab.sections.iter().enumerate() {
            let line = block_lines.get(section.block).copied().unwrap_or(0);
            if line <= scroll {
                current = i;
            } else {
                break;
            }
        }
        Some(current)
    }

    /// `za` (PRD FR-NV-3): fold/unfold the section at the cursor. Rebuilds the
    /// layout, keeps the (un)folded heading on screen, and repairs the focused
    /// link if it was just folded away.
    pub fn toggle_fold_at_cursor(&mut self) {
        let Some(section_idx) = self.current_section_index() else {
            self.status = "No sections to fold on this page".to_string();
            return;
        };
        let block = self.active_tab().sections[section_idx].block;
        let now_folded = {
            let folds = &mut self.active_tab_mut().folded_blocks;
            if folds.remove(&block) {
                false
            } else {
                folds.insert(block);
                true
            }
        };
        self.layout = None;
        self.ensure_layout();
        self.fix_focus_after_fold();
        self.scroll_block_into_view(block);
        self.status = if now_folded {
            "Folded section (za to unfold)".to_string()
        } else {
            "Unfolded section".to_string()
        };
        // PRD FR-TB-5: fold state is part of what a session persists.
        self.persist_session();
    }

    /// `zM` (PRD FR-NV-3): fold every section.
    pub fn fold_all(&mut self) {
        let blocks: Vec<usize> = self.active_tab().sections.iter().map(|s| s.block).collect();
        if blocks.is_empty() {
            self.status = "No sections to fold on this page".to_string();
            return;
        }
        for b in blocks {
            self.active_tab_mut().folded_blocks.insert(b);
        }
        self.layout = None;
        self.ensure_layout();
        self.fix_focus_after_fold();
        self.clamp_scroll();
        self.status = "Folded all sections (zR to unfold)".to_string();
        self.persist_session();
    }

    /// `zR` (PRD FR-NV-3): unfold every section.
    pub fn unfold_all(&mut self) {
        if self.active_tab().folded_blocks.is_empty() {
            self.status = "Nothing is folded".to_string();
            return;
        }
        self.active_tab_mut().folded_blocks.clear();
        self.layout = None;
        self.status = "Unfolded all sections".to_string();
        self.persist_session();
    }

    /// After a fold change, move focus off a link that is no longer visible
    /// (folded away) to the first visible link, or clear it if none remain.
    fn fix_focus_after_fold(&mut self) {
        self.ensure_layout();
        let Some(focused) = self.active_tab().focused_link else {
            return;
        };
        let still_visible = self
            .layout
            .as_ref()
            .map(|l| l.link_visible.get(focused).copied().unwrap_or(false))
            .unwrap_or(false);
        if still_visible {
            return;
        }
        let next = self
            .layout
            .as_ref()
            .and_then(|l| l.link_visible.iter().position(|&v| v));
        self.active_tab_mut().focused_link = next;
    }

    /// Scroll so `block`'s laid-out line is visible after a fold change, and
    /// clamp to the article's new extent.
    fn scroll_block_into_view(&mut self, block: usize) {
        let line = self
            .layout
            .as_ref()
            .and_then(|l| l.block_lines.get(block).copied())
            .unwrap_or(0) as u16;
        let max = self.computed_max_scroll();
        self.active_tab_mut().scroll = line.min(max);
    }

    /// Clamp the active tab's scroll to the current layout's extent (used
    /// after folding shrinks the article under the cursor).
    fn clamp_scroll(&mut self) {
        let max = self.computed_max_scroll();
        let tab = self.active_tab_mut();
        tab.scroll = tab.scroll.min(max);
    }

    /// The greatest scroll offset the current layout permits at the last-drawn
    /// viewport height — `total_lines - viewport_height`, floored at 0.
    fn computed_max_scroll(&self) -> u16 {
        let total = self
            .layout
            .as_ref()
            .map(|l| l.lines.len() as u16)
            .unwrap_or(0);
        total.saturating_sub(self.viewport_height)
    }

    /// PRD FR-NV-3's "in-page search auto-unfolds hits": unfold any folded
    /// section whose body contains `query` (smart-case, matching `find`'s own
    /// rule), so the match becomes a real laid-out line the finder can locate.
    fn auto_unfold_for_find(&mut self, query: &str) {
        if query.is_empty() || self.active_tab().folded_blocks.is_empty() {
            return;
        }
        let case_sensitive = layout::is_case_sensitive(query);
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        let Some(doc) = self.active_tab().doc.as_ref() else {
            return;
        };
        let folded: Vec<usize> = self.active_tab().folded_blocks.iter().copied().collect();
        let mut to_unfold = Vec::new();
        for &h in &folded {
            let level = match doc.blocks.get(h) {
                Some(crate::doc::Block::Heading { level, .. }) => *level,
                _ => continue,
            };
            let end = layout::fold_range_end(&doc.blocks, h, level);
            let hit = doc.blocks[h..end]
                .iter()
                .any(|b| block_matches_query(b, &needle, case_sensitive));
            if hit {
                to_unfold.push(h);
            }
        }
        if !to_unfold.is_empty() {
            for h in to_unfold {
                self.active_tab_mut().folded_blocks.remove(&h);
            }
            self.layout = None;
        }
    }

    // -- K peek popup (PRD FR-NV-4 footnote peek / FR-NV-5 link preview) -----

    /// `K` (PRD FR-NV-4/5): open the peek popup for the focused link. Context
    /// decides the kind — a reference marker (`[n]`, a `#cite…` anchor) opens
    /// the footnote peek, resolved locally from the parsed citations with no
    /// network; an internal link opens the link preview from the page summary.
    /// Returns the `(lang, title)` a preview must fetch a summary for when it
    /// isn't cached yet (the caller fires it off the event loop), else `None`
    /// (a footnote, an already-cached preview, or nothing focusable to peek).
    pub fn open_peek_at_focus(&mut self) -> Option<(String, String)> {
        self.ensure_layout();
        let link = self
            .active_tab()
            .focused_link
            .and_then(|i| self.active_tab().links.get(i))
            .cloned();
        if let Some(link) = &link {
            // A directly-installed reference `LinkRef` still peeks as a
            // footnote. Markers are otherwise absent from `links` (0b86bf0),
            // so this arises only when a caller sets one deliberately; the
            // live `K`-on-a-`[n]` route is the scroll-proximity path below.
            if is_reference_marker(&link.href) {
                let (href, text) = (link.href.clone(), link.text.clone());
                self.open_footnote_peek(&href, &text);
                return None;
            }
            // FR-NV-5 link preview wins when the focused link is one the reader
            // has settled on — an internal link actually on screen (Tab-cycling
            // scrolls its target into view). A focused link scrolled out of the
            // viewport is no longer what `K` is "on", so it yields to the
            // footnote-peek path; `focused_link_in_view` keeps the historical
            // behavior before the first draw sets a real viewport height.
            if link.internal_title.is_some() && self.focused_link_in_view() {
                return self.open_link_preview(link.internal_title.clone().unwrap());
            }
        }
        // FR-NV-4 footnote peek (restored): peek the reference marker nearest
        // the reading position. Reference markers are not followable links and
        // can't be Tab-focused, so this proximity route is how `K` reaches a
        // `[n]` marker once folding/cycling has stopped landing on them.
        if self.peek_reference_near_scroll() {
            return None;
        }
        // No reference near the cursor: fall back to previewing a focused
        // internal link even off-screen, so `K` is never a silent no-op when a
        // link is focused.
        if let Some(link) = link {
            if let Some(title) = link.internal_title {
                return self.open_link_preview(title);
            }
            self.status = "External link — nothing to preview".to_string();
        } else {
            self.status =
                "Nothing to peek here — Tab to a link or scroll to a reference".to_string();
        }
        None
    }

    /// FR-NV-5 link preview: open the peek popup for an internal link `title`
    /// on the active tab's wiki. Returns the `(lang, title)` the caller must
    /// fetch a summary for when it isn't cached yet, else `None` (already
    /// cached — the popup renders immediately).
    fn open_link_preview(&mut self, title: String) -> Option<(String, String)> {
        let lang = self.lang.clone();
        self.peek_prior_mode = self.mode;
        self.peek = Some(PeekPopup::LinkPreview {
            lang: lang.clone(),
            title: title.clone(),
        });
        self.mode = Mode::Peek;
        if self.summary_cache.contains_key(&(
            self.active_tab().wiki.clone(),
            lang.clone(),
            title.clone(),
        )) {
            self.summary_loading = false;
            self.status = "Link preview — Ctrl-o/Esc: close   Enter: open".to_string();
            None
        } else {
            self.summary_loading = true;
            self.status = "Loading preview…".to_string();
            Some((lang, title))
        }
    }

    /// FR-NV-4 footnote peek: resolve a reference marker `(href, text)` to its
    /// citation and open the peek popup — locally, from the parsed citations,
    /// never a network call. A graceful "reference not found" status when it
    /// doesn't resolve. Shared by the focused-marker path and the
    /// scroll-proximity path so the footnote peek is spelled exactly once.
    fn open_footnote_peek(&mut self, href: &str, text: &str) {
        let citation = self
            .active_tab()
            .doc
            .as_ref()
            .and_then(|d| resolve_reference(&d.citations, href, text).cloned());
        match citation {
            Some(c) => {
                self.peek_prior_mode = self.mode;
                self.peek = Some(PeekPopup::Footnote {
                    marker: text.to_string(),
                    text: c.text,
                });
                self.mode = Mode::Peek;
                self.status = "Reference — Ctrl-o/Esc: close".to_string();
            }
            None => self.status = "Reference not found".to_string(),
        }
    }

    /// FR-NV-4 (restored): open the footnote peek for the reference marker
    /// nearest the current reading position, resolved locally with no network.
    /// Reference markers are deliberately not followable links (0b86bf0), so
    /// they can't be Tab-focused — this proximity search over the tab's
    /// `reference_markers` (mapped to lines via `Layout::block_lines`) is the
    /// live route to a `[n]` marker's text. Returns whether a marker was found
    /// (so the caller can fall back when the page has none).
    fn peek_reference_near_scroll(&mut self) -> bool {
        self.ensure_layout();
        let scroll = self.active_tab().scroll as usize;
        let Some(block_lines) = self.layout.as_ref().map(|l| l.block_lines.clone()) else {
            return false;
        };
        let nearest = self
            .active_tab()
            .reference_markers
            .iter()
            .filter(|m| is_reference_marker(&m.href))
            .map(|m| {
                let line = block_lines.get(m.block).copied().unwrap_or(0);
                (line.abs_diff(scroll), line, m.href.clone(), m.text.clone())
            })
            .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let Some((_, _, href, text)) = nearest else {
            return false;
        };
        self.open_footnote_peek(&href, &text);
        true
    }

    /// Whether the active tab's focused link is currently within the drawn
    /// viewport — the signal `K` uses to decide a focused internal link is the
    /// reader's deliberate preview target (FR-NV-5) rather than one they have
    /// scrolled past. `true` when no link is focused-and-visible is `false`;
    /// `true` when there is no viewport height yet (before the first draw / in
    /// unit tests) so the historical "the focused link is the peek target"
    /// behavior stands with nothing to test against.
    fn focused_link_in_view(&self) -> bool {
        let Some(occ) = self.active_tab().focused_link else {
            return false;
        };
        let vh = self.viewport_height;
        if vh == 0 {
            return true;
        }
        let Some(layout) = self.layout.as_ref() else {
            return true;
        };
        if !layout.link_visible.get(occ).copied().unwrap_or(false) {
            return false;
        }
        let line = layout.link_lines.get(occ).copied().unwrap_or(0) as u16;
        let scroll = self.active_tab().scroll;
        line >= scroll && line < scroll.saturating_add(vh)
    }

    /// Close the peek popup (`Ctrl-o`/`Esc`), restoring the prior mode.
    pub fn close_peek(&mut self) {
        self.mode = self.peek_prior_mode;
        self.peek = None;
        self.summary_loading = false;
        self.refresh_reading_status();
    }

    /// The cached summary for the open link-preview popup, or `None` while it
    /// is still loading (the popup then shows "loading…").
    pub fn peek_summary(&self) -> Option<&crate::api::SummaryData> {
        match &self.peek {
            // The preview targets an internal link in the active tab's
            // article, so it shares that tab's wiki scope (PRD FR-ML-4).
            Some(PeekPopup::LinkPreview { lang, title }) => self.summary_cache.get(&(
                self.active_tab().wiki.clone(),
                lang.clone(),
                title.clone(),
            )),
            _ => None,
        }
    }

    /// The `(lang, title)` Enter opens from an open link-preview popup.
    pub fn peek_open_target(&self) -> Option<(String, String)> {
        match &self.peek {
            Some(PeekPopup::LinkPreview { lang, title }) => Some((lang.clone(), title.clone())),
            _ => None,
        }
    }

    /// Installs a completed (or failed) link-preview summary into the session
    /// cache (PRD FR-NV-5's "cached per-title for the session"), and — if the
    /// popup is still showing exactly this target — clears its loading state.
    /// A failure caches a default (empty) entry so a flaky request isn't
    /// retried on every reopen, mirroring `deliver_related`.
    pub fn deliver_summary(
        &mut self,
        lang: String,
        title: String,
        result: Result<crate::api::SummaryData, String>,
    ) {
        let data = result.unwrap_or_default();
        let is_current = self.mode == Mode::Peek
            && matches!(
                &self.peek,
                Some(PeekPopup::LinkPreview { lang: l, title: t }) if *l == lang && *t == title
            );
        let wiki = self.active_tab().wiki.clone();
        self.summary_cache.insert((wiki, lang, title), data);
        if is_current {
            self.summary_loading = false;
            self.status = "Link preview — Ctrl-o/Esc: close   Enter: open".to_string();
        }
    }

    /// `gK` (PRD FR-NV-4): scroll to the References/Notes section, or report
    /// that the article has none.
    pub fn jump_to_references(&mut self) {
        match find_references_section(&self.active_tab().sections) {
            Some(i) => self.jump_to_section(i),
            None => self.status = "No references section on this page".to_string(),
        }
    }

    // -- `i` / `:info` article-attribution overlay (PRD §10, Appendix B) ----

    /// `i` / `:info`: open the attribution overlay for the article on
    /// screen — title, canonical URL, revision id, license, and a permalink
    /// to its revision history (PRD §10's display-side attribution
    /// requirement). Nothing to show without an article open, so this
    /// mirrors `yank_url`'s "Open an article first" guard rather than
    /// opening an empty card. Returns whether it opened.
    pub fn open_info(&mut self) -> bool {
        let tab = self.active_tab();
        let Some(doc) = tab.doc.as_ref() else {
            self.status = "Open an article first".to_string();
            return false;
        };
        let info = crate::attribution::article_attribution(
            &doc.title,
            &tab.lang,
            tab.current_revid,
            &crate::research::today(),
        );
        self.info = Some(info);
        self.info_prior_mode = self.mode;
        self.mode = Mode::Info;
        self.status = "Article info — Esc: close".to_string();
        true
    }

    /// Close the `:info` overlay (`Esc`), restoring the prior mode.
    pub fn close_info(&mut self) {
        self.mode = self.info_prior_mode;
        self.info = None;
        self.refresh_reading_status();
    }

    // -- Reading-position memory (PRD FR-NV-8) ------------------------------

    /// The section title the given tab's current scroll sits in, for the
    /// anchor-based resume fallback (PRD FR-NV-8). Only computable for the
    /// active tab (the one `self.layout` describes); `None` otherwise or when
    /// the scroll is above every heading.
    fn anchor_at_scroll(&self, index: usize) -> Option<String> {
        if index != self.active {
            return None;
        }
        let layout = self.layout.as_ref()?;
        let tab = self.tabs.get(index)?;
        let scroll = tab.scroll as usize;
        let mut anchor = None;
        for section in &tab.sections {
            let line = layout.block_lines.get(section.block).copied().unwrap_or(0);
            if line <= scroll {
                anchor = Some(section.title.clone());
            } else {
                break;
            }
        }
        anchor
    }

    /// PRD FR-NV-8: persist the given tab's scroll/fold position for its
    /// article, keyed to the current revid, unless incognito denies it (the
    /// passive-write privacy gate — the same bucket as history). Called
    /// wherever a tab's current view is ending: navigating away, closing the
    /// tab, or quitting.
    pub(crate) fn save_reading_position(&mut self, index: usize) {
        if crate::privacy::decide(self.incognito, crate::privacy::Write::Position)
            == crate::privacy::Verdict::Deny
        {
            return;
        }
        let anchor = self.anchor_at_scroll(index);
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(doc) = tab.doc.as_ref() else {
            return;
        };
        let title = doc.title.clone();
        let lang = tab.lang.clone();
        let wiki = tab.wiki.clone();
        let revid = tab.current_revid;
        let scroll = tab.scroll;
        let mut folds: Vec<usize> = tab.folded_blocks.iter().copied().collect();
        folds.sort_unstable();
        self.history.save_position(
            &wiki,
            &lang,
            &title,
            revid,
            scroll,
            &folds,
            anchor.as_deref(),
        );
    }

    /// PRD FR-NV-8: after installing a document, raise the "resume at §… (r)"
    /// toast when a saved position exists for it that isn't the top. Skipped
    /// for the one navigation that already restored its own scroll
    /// (`suppress_resume_once`, set by back/forward and the SWR reload).
    fn check_resume_position(&mut self, index: usize) {
        if self.suppress_resume_once {
            self.suppress_resume_once = false;
            return;
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(doc) = tab.doc.as_ref() else {
            return;
        };
        let title = doc.title.clone();
        let lang = tab.lang.clone();
        let wiki = tab.wiki.clone();
        let revid = tab.current_revid;
        let Some(saved) = self.history.position(&wiki, &lang, &title) else {
            return;
        };
        if saved.scroll == 0 && saved.folds.is_empty() {
            return; // top of the article — nothing worth resuming to
        }
        let revid_matches = revid != 0 && saved.revid == revid;
        self.notice = Some(match &saved.anchor {
            Some(anchor) if !anchor.is_empty() => format!("resume at §{anchor}? (r)"),
            _ => "resume where you left off? (r)".to_string(),
        });
        self.pending_resume = Some(ResumePosition {
            scroll: saved.scroll,
            folds: saved.folds,
            revid_matches,
            anchor: saved.anchor,
        });
    }

    /// PRD FR-NV-8: apply the pending resume when the reader presses `r` on the
    /// toast. Exact restore (scroll + folds) when the revision still matches;
    /// otherwise the best-effort anchor fallback — scroll to the nearest
    /// surviving heading named in the saved anchor rather than trust a stale
    /// absolute offset against changed content.
    pub fn resume_to_saved_position(&mut self) {
        let Some(resume) = self.pending_resume.take() else {
            return;
        };
        self.notice = None;
        if resume.revid_matches {
            self.active_tab_mut().folded_blocks = resume.folds.iter().copied().collect();
            self.layout = None;
            self.ensure_layout();
            let max = self.computed_max_scroll();
            self.active_tab_mut().scroll = resume.scroll.min(max);
            self.status = "Resumed reading position".to_string();
        } else {
            match resume
                .anchor
                .as_deref()
                .and_then(|a| self.section_index_by_title(a))
            {
                Some(i) => {
                    self.jump_to_section(i);
                    self.status = "Article changed — resumed at the nearest heading".to_string();
                }
                None => {
                    self.status =
                        "Article changed — couldn't restore the exact position".to_string();
                }
            }
        }
        // PRD FR-TB-5: resuming changes scroll/folds, both session-persisted.
        self.persist_session();
    }

    /// The section (index into the active tab's outline) whose title matches
    /// `title` case-insensitively — the anchor-fallback lookup for PRD FR-NV-8.
    fn section_index_by_title(&self, title: &str) -> Option<usize> {
        let needle = title.trim().to_lowercase();
        self.active_tab()
            .sections
            .iter()
            .position(|s| s.title.trim().to_lowercase() == needle)
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

    // -- Visual-selection yank (PRD FR-NV-10) -------------------------------

    /// `v`: enters visual-selection mode, anchored at the top of the current
    /// viewport (`scroll`) — Reading has no independent per-line reading
    /// cursor to anchor on instead, so the first visible line is the least
    /// surprising starting point. A no-op status message (no mode change)
    /// when there's no document to select from.
    ///
    /// **Granularity**: whole laid-out (wrapped) lines, not characters or
    /// words. A precise character/word range would need grapheme-column
    /// tracking through the layout engine's own wrap decisions, which
    /// nothing downstream of `layout::Layout` currently exposes at this call
    /// site (the closest existing precedent, `MatchSpan`, only ever
    /// addresses a single already-known line, never a multi-line span) —
    /// building that machinery is a substantially larger change than this
    /// chunk's scope. A line-range yank already covers FR-NV-10's core case
    /// (copying a passage/paragraph out of the article), so this documents
    /// the simplification rather than blocking on the fuller version.
    pub fn enter_visual(&mut self) {
        if self.active_tab().doc.is_none() {
            self.status = "Open an article first".to_string();
            return;
        }
        self.ensure_layout();
        let line = self.active_tab().scroll as usize;
        self.visual_anchor_line = line;
        self.visual_cursor_line = line;
        self.mode = Mode::Visual;
        self.status = "j/k: extend selection   y: yank   Esc: cancel".to_string();
    }

    /// The selected line range, inclusive, lowest index first.
    pub fn visual_selected_range(&self) -> (usize, usize) {
        (
            self.visual_anchor_line.min(self.visual_cursor_line),
            self.visual_anchor_line.max(self.visual_cursor_line),
        )
    }

    /// Moves the visual cursor by `delta` lines, clamped to the laid-out
    /// document, and scrolls it into view exactly like
    /// `scroll_focused_link_into_view` does for a Tab-cycled link.
    pub fn visual_move(&mut self, delta: i32) {
        self.ensure_layout();
        let max_line = self
            .layout
            .as_ref()
            .map(|l| l.lines.len().saturating_sub(1))
            .unwrap_or(0);
        let new = (self.visual_cursor_line as i32 + delta).clamp(0, max_line as i32) as usize;
        self.visual_cursor_line = new;

        if self.viewport_height == 0 {
            return;
        }
        let (scroll, max_scroll) = {
            let tab = self.active_tab();
            (tab.scroll, tab.max_scroll)
        };
        let line = new as u16;
        let vh = self.viewport_height;
        let bottom = scroll.saturating_add(vh);
        let new_scroll = if line < scroll {
            line.min(max_scroll)
        } else if line >= bottom {
            line.saturating_sub(vh.saturating_sub(1)).min(max_scroll)
        } else {
            scroll
        };
        self.active_tab_mut().scroll = new_scroll;
    }

    /// `y` in visual mode: the plain text of every selected line,
    /// newline-joined — `main::handle_key` copies this to the clipboard via
    /// the same OSC 52 path `y`/`Y` already use in Reading (PRD FR-NV-10).
    /// `None` only when there's no layout to read from (shouldn't happen
    /// once `Mode::Visual` is entered via `enter_visual`, which requires a
    /// document; a defensive `None` beats a panic if it ever did).
    pub fn visual_selected_text(&self) -> Option<String> {
        let layout = self.layout.as_ref()?;
        let (start, end) = self.visual_selected_range();
        let end = end.min(layout.lines.len().saturating_sub(1));
        if layout.lines.is_empty() || start > end {
            return None;
        }
        let text = layout.lines[start..=end]
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(text)
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
        self.maybe_signal_scroll();
        self.sync_bound_pane(delta);
    }

    pub fn scroll_to_top(&mut self) {
        let old = self.active_tab().scroll as i32;
        self.active_tab_mut().scroll = 0;
        self.sync_bound_pane(-old);
    }

    pub fn scroll_to_bottom(&mut self) {
        let old = self.active_tab().scroll as i32;
        let max = self.active_tab().max_scroll;
        self.active_tab_mut().scroll = max;
        self.maybe_signal_scroll();
        self.sync_bound_pane(max as i32 - old);
    }

    /// PRD FR-TB-4 scroll coupling: after the focused pane's scroll just moved
    /// by `requested_delta` lines, carry the change into the *bound* pane per
    /// the split's [`crate::split::SyncMode`]. A no-op with no split, or a
    /// split in [`SyncMode::Independent`]. Each pane clamps to its own length
    /// (panes of different lengths clamp independently — best-effort
    /// alignment). [`SyncMode::Section`] ignores the delta and re-derives the
    /// bound pane's position from section boundaries
    /// ([`crate::split::section_synced_scroll`]).
    fn sync_bound_pane(&mut self, requested_delta: i32) {
        use crate::split::SyncMode;
        let (sync, other_id, focused_lines, other_lines) = match self.split.as_ref() {
            Some(s) => (
                s.sync,
                s.other_id(),
                s.pane_section_lines[s.focused].clone(),
                s.pane_section_lines[1 - s.focused].clone(),
            ),
            None => return,
        };
        let Some(idx) = self.tab_index_by_id(other_id) else {
            return;
        };
        match sync {
            SyncMode::Independent => {}
            SyncMode::Lockstep => {
                let t = &mut self.tabs[idx];
                t.scroll = (t.scroll as i32 + requested_delta).clamp(0, t.max_scroll as i32) as u16;
            }
            SyncMode::Section => {
                let focused_scroll = self.active_tab().scroll;
                let target = crate::split::section_synced_scroll(
                    focused_scroll,
                    &focused_lines,
                    &other_lines,
                );
                let t = &mut self.tabs[idx];
                t.scroll = target.min(t.max_scroll);
            }
        }
    }

    /// PRD FR-PF-3's scroll signal: +1.0 to the article's topics once the
    /// reader has scrolled past 70% of it — fired at most once per read (the
    /// per-tab flag). A no-op when learning is off, the article fits without
    /// scrolling (`max_scroll == 0`), or the threshold isn't reached yet.
    fn maybe_signal_scroll(&mut self) {
        if !self.interest_active() {
            return;
        }
        let tab = self.active_tab();
        if tab.interest_scroll_signaled || tab.max_scroll == 0 {
            return;
        }
        if (tab.scroll as f64 / tab.max_scroll as f64) < 0.70 {
            return;
        }
        let Some(title) = tab.doc.as_ref().map(|d| d.title.clone()) else {
            return;
        };
        let wiki = tab.wiki.clone();
        let changed = self.interest.apply_signal_for_title(
            &wiki,
            &title,
            crate::interest::SIGNAL_SCROLL,
            Self::interest_now(),
        );
        self.active_tab_mut().interest_scroll_signaled = true;
        if changed {
            self.persist_interest();
        }
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
    /// gets feedback, not just on the add half. PRD FR-PR-3: a bookmark is
    /// an *explicit* save (the reader named this article), unlike passive
    /// history/prefetch — so incognito never suppresses it, but the add
    /// half's notice carries `privacy::EXPLICIT_SAVE_WARNING` via
    /// `privacy::append_warning_if_needed`, since the reader should still
    /// know it persisted (removing a bookmark needs no such warning — that
    /// write only ever deletes).
    pub fn toggle_bookmark(&mut self) {
        let Some(doc) = self.active_tab().doc.as_ref() else {
            self.status = "Open an article first".to_string();
            return;
        };
        let title = doc.title.clone();
        let lang = self.active_tab().lang.clone();
        // PRD FR-ML-4: bookmark under the on-screen tab's own wiki scope, so a
        // same-titled article on another wiki is an independent bookmark.
        let wiki = self.active_tab().wiki.clone();
        let revid = self.active_tab().current_revid;
        let revid = (revid != 0).then_some(revid);

        match self.bookmarks.toggle(&wiki, &lang, &title, revid) {
            ToggleOutcome::Added => {
                // PRD FR-PF-3: a bookmark is a strong interest signal (+3.0)
                // on the article's topics — applied only when the add half
                // fires, and only if interest learning is active (off in
                // incognito, so an incognito bookmark still persists but never
                // teaches the model).
                self.apply_active_article_signal(crate::interest::SIGNAL_BOOKMARK);
                self.notice = Some(crate::privacy::append_warning_if_needed(
                    self.incognito,
                    crate::privacy::Write::Bookmark,
                    format!("Bookmarked \"{title}\""),
                ))
            }
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
            let wiki = bookmark.wiki.clone();
            let lang = bookmark.lang.clone();
            let title = bookmark.title.clone();
            let tags = bookmarks::parse_tags(&self.bookmark_tag_input);
            self.bookmarks.set_tags(&wiki, &lang, &title, tags);
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

    // -- Saved pages browser (PRD §5.7, FR-OFF-4) --------------------------

    /// `:saved` — open the saved-pages browser.
    pub fn open_saved_picker(&mut self) {
        self.saved_prior_mode = self.mode;
        self.mode = Mode::SavedPicker;
        self.selected_saved = self
            .selected_saved
            .min(self.saved.list().len().saturating_sub(1));
        self.status = "Enter: open offline   d: remove   Esc: close   j/k: move".to_string();
    }

    pub fn close_saved_picker(&mut self) {
        self.mode = self.saved_prior_mode;
        self.status = match &self.active_tab().doc {
            Some(doc) => doc.title.clone(),
            None => "Press / to search, ? for help, q to quit".to_string(),
        };
    }

    pub fn cycle_saved(&mut self, forward: bool) {
        let len = self.saved.list().len();
        if len == 0 {
            self.selected_saved = 0;
            return;
        }
        self.selected_saved = if forward {
            (self.selected_saved + 1) % len
        } else {
            (self.selected_saved + len - 1) % len
        };
    }

    /// The `(wiki, lang, title)` the saved-browser selection refers to, or
    /// `None` when the store is empty. The wiki (PRD FR-ML-4) lets Enter open
    /// — and `d` un-pin — the exact pinned copy, even across wikis.
    pub fn selected_saved_target(&self) -> Option<(String, String, String)> {
        self.saved
            .list()
            .get(self.selected_saved)
            .map(|r| (r.wiki.clone(), r.lang.clone(), r.title.clone()))
    }

    /// `d` in the saved browser: un-pin the selection, keeping the cursor in
    /// range over whatever remains.
    pub fn delete_selected_saved(&mut self) {
        let Some((wiki, lang, title)) = self.selected_saved_target() else {
            return;
        };
        if let Some((removed, persisted)) = self.saved.remove(&wiki, &lang, &title) {
            // PRD FR-SR-7: an un-saved page must stop showing up in offline
            // search. The index row is removed under the record's own wiki
            // scope (PRD FR-ML-4), so un-saving a cross-wiki page prunes the
            // right row regardless of which wiki is currently active.
            self.search_index.remove(&wiki, &lang, &title);
            let len = self.saved.list().len();
            self.selected_saved = if len == 0 {
                0
            } else {
                self.selected_saved.min(len - 1)
            };
            self.status = match persisted {
                Ok(()) => format!("Removed saved page \"{}\"", removed.title),
                Err(e) => format!(
                    "Un-pinned \"{}\" this session, but updating the index failed: {e}",
                    removed.title
                ),
            };
        }
    }

    // -- Offline uncached-link card (PRD FR-OFF-6, §7) ---------------------

    /// Show §7's "Offline, uncached link" card for `(lang, title)` — the
    /// followed link is neither cached nor saved and the network is down.
    pub fn show_offline_card(&mut self, lang: String, title: String) {
        self.offline_card_target = Some((lang, title));
        self.mode = Mode::OfflineCard;
    }

    pub fn close_offline_card(&mut self) {
        self.offline_card_target = None;
        self.mode = Mode::Reading;
    }

    /// The offline card's `f`: enqueue the pending target for fetch-when-online
    /// (PRD FR-OFF-6) and dismiss the card. Returns whether something new was
    /// queued (a duplicate is reported, not re-added).
    pub fn queue_offline_target(&mut self) -> bool {
        let Some((lang, title)) = self.offline_card_target.clone() else {
            return false;
        };
        let added = self.fetch_queue.enqueue(&lang, &title);
        self.close_offline_card();
        // PRD FR-PR-3: explicit ("fetch this specific thing") — warn, don't
        // suppress; see `toggle_bookmark`'s doc comment.
        self.notice = Some(if added {
            crate::privacy::append_warning_if_needed(
                self.incognito,
                crate::privacy::Write::FetchQueue,
                format!("Queued \"{title}\" to fetch when online"),
            )
        } else {
            format!("\"{title}\" is already in the fetch queue")
        });
        added
    }

    // -- Redlinks (PRD FR-DL-5, §7) -----------------------------------------

    /// Whether `title` is already known not to exist on the active tab's
    /// wiki edition — either the very link that names it was Parsoid-marked
    /// (`LinkRef::redlink`, the cheapest signal, checked first) or the
    /// batched info check confirmed it after some article that links to it
    /// loaded (`confirmed_redlinks`). Either is sufficient; this is the one
    /// place both signals are combined, so call sites (following a link,
    /// painting it) never have to know there are two.
    pub fn is_redlink(&self, title: &str) -> bool {
        let tab = self.active_tab();
        self.confirmed_redlinks
            .contains(&(tab.wiki.clone(), tab.lang.clone(), title.to_string()))
            || tab
                .links
                .iter()
                .any(|l| l.redlink && l.internal_title.as_deref() == Some(title))
    }

    /// Show §7's "Redlink followed" card for `(lang, title)` — called
    /// instead of attempting a fetch that would just 404, since the target is
    /// already known not to exist (`is_redlink`).
    pub fn show_redlink_card(&mut self, lang: String, title: String) {
        self.redlink_card_target = Some((lang, title));
        self.mode = Mode::RedlinkCard;
    }

    pub fn close_redlink_card(&mut self) {
        self.redlink_card_target = None;
        self.mode = Mode::Reading;
    }

    /// The redlink card's `y`: the wiki's own "create this page" URL for the
    /// pending target, ready to yank (PRD §7's "yankable create-URL").
    pub fn redlink_create_url(&self) -> Option<String> {
        let (lang, title) = self.redlink_card_target.as_ref()?;
        Some(crate::research::create_page_url(title, lang))
    }

    // -- Quality badges (PRD FR-DL-3) ---------------------------------------

    /// The active tab's article's quality badge (`★FA`/`+GA`/`B`/`C`/`Start`/
    /// `Stub`), or `None` when nothing is cached for it yet — an unassessed
    /// article, a non-PageAssessments wiki (both indistinguishable from "not
    /// fetched yet", by design: FR-DL-3 says both simply show no badge), or
    /// the batched fetch that would populate `quality_cache` hasn't landed.
    pub fn current_quality_badge(&self) -> Option<&'static str> {
        let tab = self.active_tab();
        let title = tab.doc.as_ref()?.title.as_str();
        // The badge for the open article lives in that tab's own wiki scope.
        self.quality_badge_for(&tab.wiki, title)
    }

    /// Whether the article on screen is bookmarked (PRD FR-BM-1): wires
    /// `BookmarkStore::is_bookmarked`, which previously had no caller outside
    /// its own tests — the store tracked bookmarks correctly, but nothing in
    /// the UI ever asked it whether the *current* article was one, so a
    /// bookmarked page and an unbookmarked page were indistinguishable while
    /// reading (only `m`'s one-keypress toggle confirmation, and the
    /// bookmark picker itself, ever showed the state). Scoped by wiki (PRD
    /// FR-ML-4), same as every other bookmark lookup — a same-titled article
    /// on another wiki never borrows this one's bookmark.
    pub fn is_active_bookmarked(&self) -> bool {
        let tab = self.active_tab();
        match &tab.doc {
            Some(doc) => self
                .bookmarks
                .is_bookmarked(&tab.wiki, &tab.lang, &doc.title),
            None => false,
        }
    }

    /// `title`'s quality badge from the session cache, in `wiki`'s scope for
    /// the active tab's language — shared by `current_quality_badge` (which
    /// passes the active tab's own wiki) and the search-results list
    /// (`ui::draw_results`, which passes the active wiki the search ran on),
    /// so both read the exact same map regardless of which one populated it
    /// first. The wiki dimension (PRD FR-ML-4) keeps a same-titled article on
    /// another wiki from showing this wiki's badge.
    pub fn quality_badge_for(&self, wiki: &str, title: &str) -> Option<&'static str> {
        self.quality_cache
            .get(&(
                wiki.to_string(),
                self.active_tab().lang.clone(),
                title.to_string(),
            ))
            .map(|class| class.badge())
    }

    // -- Saved-page export (PRD FR-OFF-7) ----------------------------------

    /// `:save export md|txt|html [path]`: export the article on screen (which,
    /// for a page opened from the `:saved` browser, is the pinned copy) with
    /// the §10 attribution footer. When the page is saved with T1+ thumbnails
    /// *and* the reader opted into non-free content, the HTML export embeds the
    /// pinned images; otherwise images are alt text only (see `saved_export`).
    pub fn export_saved_page(&mut self, format: &str, path: Option<&std::path::Path>) {
        let Some(doc) = self.active_tab().doc.clone() else {
            self.notice = Some("Open an article first".to_string());
            return;
        };
        let lang = self.active_tab().lang.clone();
        let title = doc.title.clone();
        // PRD FR-ML-4: the pinned copy (and its thumbnails) live under the
        // on-screen tab's own wiki scope — a same-titled page pinned on
        // another wiki has its own separate thumbnails.
        let wiki = self.active_tab().wiki.clone();

        let stored_thumbs = self
            .saved
            .find(&wiki, &lang, &title)
            .map(|r| r.thumbs.clone())
            .unwrap_or_default();
        let thumbs = crate::saved_export::embeddable_from(&stored_thumbs, |src| {
            self.saved.thumb_bytes(&wiki, &lang, &title, src)
        });

        let Some(content) =
            crate::saved_export::render(&doc, &lang, format, self.include_nonfree, &thumbs)
        else {
            self.notice = Some(format!(
                "unknown export format {format:?} — one of: {}",
                crate::saved_export::FORMATS.join(", ")
            ));
            return;
        };

        let target = match path {
            Some(p) => p.to_path_buf(),
            None => match crate::saved_export::default_export_path(&title, format) {
                Some(p) => p,
                None => {
                    self.notice = Some(format!("unknown export format {format:?}"));
                    return;
                }
            },
        };

        if target.exists()
            && self.pending_saved_export_overwrite.as_deref() != Some(target.as_path())
        {
            self.pending_saved_export_overwrite = Some(target.clone());
            self.notice = Some(format!(
                "{} already exists — run the export again to overwrite",
                target.display()
            ));
            return;
        }
        self.pending_saved_export_overwrite = None;
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        self.notice = Some(match std::fs::write(&target, content) {
            Ok(()) => format!("Exported \"{title}\" to {}", target.display()),
            Err(e) => format!("Export failed: {e}"),
        });
    }
}

/// PRD FR-OFF-5's bulk cost preview, matching §7's wording ("Category:Physics
/// → 412 articles, est. 14 MB. Proceed?"). A pure function of the count and
/// tier so the estimate math is table-testable without any store or network:
/// per-article bytes come from `Tier::estimate_bytes` (FR-OFF-9's figures).
pub fn bulk_cost_preview(label: &str, count: usize, tier: Tier) -> String {
    let bytes = count as u64 * tier.estimate_bytes();
    let mb = bytes as f64 / (1024.0 * 1024.0);
    format!(
        "{label} → {count} articles, est. {mb:.1} MB ({}). Proceed? (y/n)",
        tier.label()
    )
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
/// PRD FR-SR-4 / §7's "Search: zero results" row: `suggestion` is the
/// server's "did you mean" (opt-in, via Enter); `rewritten_query`, when
/// present, means the query the reader typed was ALREADY auto-corrected and
/// re-searched — that combination (a rewrite that still finds nothing) is
/// rare but not impossible, so it's handled rather than assumed away.
/// `offline_fallback` is the count `run_search` found in the local index
/// when the online search itself came back empty (`0` suppresses the
/// clause entirely) — §7's "offline-results section if applicable".
pub fn zero_results_message(
    query: &str,
    suggestion: Option<&str>,
    rewritten_query: Option<&str>,
    offline_fallback: usize,
) -> String {
    let mut msg = if let Some(rewritten) = rewritten_query {
        format!("No results for \"{rewritten}\" (rewritten from \"{query}\")")
    } else {
        match suggestion {
            Some(s) => format!("No results for \"{query}\". Did you mean {s}? (Enter to search)"),
            None => format!("No results for \"{query}\""),
        }
    };
    if offline_fallback > 0 {
        msg.push_str(&format!(
            "; {offline_fallback} in your saved pages (:search-offline to browse)"
        ));
    }
    msg
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

/// What the `z`-prefix chord's second key means (PRD FR-PR-3's `zz`, plus
/// FR-NV-3's section folding `za`/`zM`/`zR`, Appendix B). Any other second key
/// falls through unhandled, the same dead-prefix fallback `resolve_g_prefix`/
/// `resolve_b_prefix` use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZPrefixAction {
    /// `zz`: toggle incognito.
    ToggleIncognito,
    /// `za` (PRD FR-NV-3): fold/unfold the section at the cursor.
    ToggleFold,
    /// `zM` (PRD FR-NV-3): fold every section.
    FoldAll,
    /// `zR` (PRD FR-NV-3): unfold every section.
    UnfoldAll,
    /// Any other second key: dead prefix — `handle_key` processes it as if
    /// `z` had never been typed.
    PassThrough,
}

pub fn resolve_z_prefix(second_key: char) -> ZPrefixAction {
    match second_key {
        'z' => ZPrefixAction::ToggleIncognito,
        'a' => ZPrefixAction::ToggleFold,
        'M' => ZPrefixAction::FoldAll,
        'R' => ZPrefixAction::UnfoldAll,
        _ => ZPrefixAction::PassThrough,
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

/// What the `g`-prefix chord's second key means (PRD Appendix B: `gg`,
/// `gt`/`gT`, `gb`, `gh`, plus FR-SR-5's `gr` and FR-SR-6's `gR` added
/// here). A pure decision like `resolve_b_prefix`/`resolve_r_prefix`, so
/// `main.rs`'s `handle_key` only has to turn the answer into the actual
/// mode change/fetch, not decide it inline where a test can't reach it
/// without a live terminal — this one was inline in `handle_key` before
/// `gr`/`gR` needed adding, and factoring it out here now is what makes
/// those two additions (and the untouched five) equally testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GPrefixAction {
    /// `gg`: scroll to the top.
    Top,
    /// `gt`: next tab.
    NextTab,
    /// `gT`: previous tab.
    PrevTab,
    /// `gb`: the back-stack picker.
    BackStack,
    /// `gh`: home / start page.
    Home,
    /// `gr` (PRD FR-SR-5): open a random article.
    Random,
    /// `gR` (PRD FR-SR-6): open the Related panel for the current article.
    Related,
    /// `gK` (PRD FR-NV-4): jump to the References/Notes section.
    References,
    /// `gW` (PRD FR-ACC-2): open the watchlist pane — the one keybinding
    /// this chunk adds a default for (Appendix B lists `w` for watch/unwatch
    /// but no dedicated open key; `gW` fits the existing `g`-prefix
    /// panel-open convention `gr`/`gR`/`gb` already established).
    Watchlist,
    /// `gs` (PRD FR-NV-2): open the fuzzy section jump.
    SectionJump,
    /// Any other second key: dead prefix — `handle_key` processes it as if
    /// `g` had never been typed (e.g. `gj` still scrolls).
    PassThrough,
}

pub fn resolve_g_prefix(second_key: char) -> GPrefixAction {
    match second_key {
        'g' => GPrefixAction::Top,
        't' => GPrefixAction::NextTab,
        'T' => GPrefixAction::PrevTab,
        'b' => GPrefixAction::BackStack,
        'h' => GPrefixAction::Home,
        'r' => GPrefixAction::Random,
        'R' => GPrefixAction::Related,
        's' => GPrefixAction::SectionJump,
        'K' => GPrefixAction::References,
        'W' => GPrefixAction::Watchlist,
        _ => GPrefixAction::PassThrough,
    }
}

/// Moves a selection index by `delta`, wrapping in both directions — shared
/// by every picker-style list this chunk adds (watchlist, notifications,
/// contributions) instead of each reimplementing `OnThisDayModel::
/// move_selection`'s same three-line `rem_euclid` dance. A no-op (stays 0)
/// on an empty list, matching that function's own contract.
fn wrap_move(current: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as i32;
    let cur = (current as i32).rem_euclid(len);
    (cur + delta).rem_euclid(len) as usize
}

/// PRD FR-ML-1's picker ordering: rows whose code appears in `preferred`
/// are pinned to the top, in `preferred`'s own configured order (so a
/// reader's first-choice language is always row 0 when the article has an
/// edition in it); every other langlink follows in the server's original
/// order. `filter` narrows the whole list first, by the shared subsequence
/// matcher (`fuzzy::fuzzy_matches`) against autonym, English langname, or
/// code — whichever the reader typed matches any of the three, so someone
/// who can't type "日本語" can still reach it via "japan" or "ja". A pure
/// function (no `App` access) so the pinning/filter/ordering interplay is
/// directly testable without a session cache or a mode.
pub fn order_and_filter_langlinks(
    links: &[crate::api::LangLink],
    preferred: &[String],
    filter: &str,
) -> Vec<crate::api::LangLink> {
    let matches = |l: &crate::api::LangLink| {
        crate::fuzzy::fuzzy_matches(&l.autonym, filter)
            || crate::fuzzy::fuzzy_matches(&l.langname, filter)
            || crate::fuzzy::fuzzy_matches(&l.code, filter)
    };
    let mut rows: Vec<crate::api::LangLink> = Vec::with_capacity(links.len());
    for code in preferred {
        if let Some(l) = links.iter().find(|l| &l.code == code && matches(l)) {
            rows.push(l.clone());
        }
    }
    for l in links {
        if matches(l) && !preferred.iter().any(|p| p == &l.code) {
            rows.push(l.clone());
        }
    }
    rows
}

/// PRD FR-NV-2's `gs` fuzzy section jump: every section title fuzzy-scored
/// against `query` (`fuzzy::fuzzy_score`, the same subsequence matcher
/// `palette_matches` ranks commands with), best match first, ties broken by
/// original document order rather than alphabetically — a table of contents
/// reads top-to-bottom, so two equally-good matches (including every
/// section when `query` is empty, all scoring a perfect `1.0`) should still
/// list in reading order, not shuffle alphabetically. Returns indices into
/// `sections` itself so a caller can feed a result straight to
/// `App::jump_to_section`. A pure function over `&[SectionRef]` (no `App`
/// access), the same testability `order_and_filter_langlinks` established
/// for its own picker.
pub fn filter_sections(sections: &[crate::doc::SectionRef], query: &str) -> Vec<usize> {
    let mut rows: Vec<(f64, usize)> = sections
        .iter()
        .enumerate()
        .filter_map(|(i, s)| crate::fuzzy::fuzzy_score(&s.title, query).map(|score| (score, i)))
        .collect();
    rows.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    rows.into_iter().map(|(_, i)| i).collect()
}

/// PRD FR-ML-2's "available in your preferred language" hint: the autonym
/// of the earliest-configured preferred language that `links` has an
/// edition in, or `None` when there's nothing to suggest — no preferred
/// languages configured, `current_lang` is itself one of them (the reader
/// is already home, matching FR-ML-2's "viewing a *non-preferred*
/// language" trigger condition literally), or none of `links` matches any
/// preferred code. A pure function so the surfacing rule is testable
/// without a fetch or a picker.
pub fn preferred_language_hint(
    links: &[crate::api::LangLink],
    current_lang: &str,
    preferred: &[String],
) -> Option<String> {
    if preferred.is_empty() || preferred.iter().any(|p| p == current_lang) {
        return None;
    }
    preferred.iter().find_map(|p| {
        links
            .iter()
            .find(|l| &l.code == p)
            .map(|l| l.autonym.clone())
    })
}

/// PRD FR-ML-2's fallback-chain order: `primary` (whatever brought the
/// reader here — an explicit override or just the session default) tried
/// first, then any of `preferred`'s configured `languages` not already
/// equal to it, in their own order, deduplicated. A reader who never set
/// `languages` gets a single-entry chain, identical to pre-FR-ML-2
/// behavior. A pure function — `main::open_title` is the only caller, and
/// this is the part of it worth testing without a network round trip.
pub fn fallback_chain(primary: &str, preferred: &[String]) -> Vec<String> {
    let mut chain = vec![primary.to_string()];
    for lang in preferred {
        if !chain.contains(lang) {
            chain.push(lang.clone());
        }
    }
    chain
}

/// The reader-visible text of one block, flattened to a single string — used
/// by the fold auto-unfold search (PRD FR-NV-3) to decide whether a folded
/// section's body contains an in-page-find query without laying it out.
fn block_text(block: &crate::doc::Block) -> String {
    use crate::doc::Block;
    let spans_text =
        |spans: &[crate::doc::Span]| spans.iter().map(|s| s.text.as_str()).collect::<String>();
    match block {
        Block::Heading { spans, .. } | Block::Paragraph(spans) | Block::Blockquote(spans) => {
            spans_text(spans)
        }
        Block::ListItem { spans, .. } => spans_text(spans),
        Block::Code(text) => text.clone(),
        Block::Table(table) => table.to_list_lines().join(" "),
        Block::Infobox(rows) => rows
            .iter()
            .map(|(l, v)| format!("{l} {v}"))
            .collect::<Vec<_>>()
            .join(" "),
        Block::Image { alt, caption, .. } => {
            format!("{alt} {}", caption.clone().unwrap_or_default())
        }
        Block::Gallery(items) => items
            .iter()
            .map(|i| i.caption.clone())
            .collect::<Vec<_>>()
            .join(" "),
        Block::Math { tex, .. } => tex.clone(),
        Block::Rule => String::new(),
    }
}

/// Whether one block's text contains `needle` (PRD FR-NV-3 auto-unfold).
/// `needle` is already lower-cased by the caller when `case_sensitive` is
/// false, matching `find`'s smart-case handling.
fn block_matches_query(block: &crate::doc::Block, needle: &str, case_sensitive: bool) -> bool {
    let text = block_text(block);
    if case_sensitive {
        text.contains(needle)
    } else {
        text.to_lowercase().contains(needle)
    }
}

/// PRD FR-NV-4's `gK` target: the index of the article's References/Notes
/// section, or `None` when it has none. Matches the common heading titles
/// case-insensitively, preferring the *last* match (an article's citation
/// apparatus sits at the end, after any in-prose "notes"). A pure function of
/// the section outline so the choice is testable without a layout.
pub fn find_references_section(sections: &[crate::doc::SectionRef]) -> Option<usize> {
    const NAMES: &[&str] = &[
        "references",
        "notes",
        "footnotes",
        "citations",
        "sources",
        "works cited",
        "bibliography",
    ];
    sections.iter().enumerate().rev().find_map(|(i, s)| {
        let title = s.title.trim().to_lowercase();
        NAMES.iter().any(|n| title == *n).then_some(i)
    })
}

/// PRD FR-NV-4: resolve a focused reference marker (`[n]`) to its citation.
/// The marker link's `href` is a same-page anchor (`#cite_note-1`); match it
/// against `citations` by id first, then fall back to the bracketed number in
/// the marker text (`[2]` → the 2nd citation) for markup that doesn't anchor
/// by a matching id. `None` when neither resolves (a graceful "reference not
/// found"). Pure so the marker→citation mapping is testable in isolation.
pub fn resolve_reference<'a>(
    citations: &'a [Citation],
    href: &str,
    text: &str,
) -> Option<&'a Citation> {
    let anchor = href.trim_start_matches('#');
    if let Some(c) = citations.iter().find(|c| c.id == anchor) {
        return Some(c);
    }
    let n: usize = text
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()?;
    n.checked_sub(1).and_then(|i| citations.get(i))
}

/// Whether a focused link is a reference marker (PRD FR-NV-4) rather than a
/// followable link: its href is a same-page `#cite`/`#cite_note` anchor. Used
/// by the `K` dispatch to choose footnote peek over link preview.
pub fn is_reference_marker(href: &str) -> bool {
    href.starts_with("#cite") || href.starts_with("#endnote") || href.starts_with("#cite_note")
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
            degraded_parse: false,
            is_disambiguation: false,
        }
    }

    /// H3 (test-isolation / user-data hazard): `App::new` is called from
    /// roughly 200 call sites in this crate's own test suite. If any of the
    /// five persisted stores below defaulted to `X::load()` (the real
    /// on-disk store) rather than `X::in_memory()`, every one of those tests
    /// would read the developer's actual bookmarks/read-later/research/
    /// saved/fetch-queue files on `cargo test` — making outcomes depend on
    /// whatever the developer happens to have saved, and any test that
    /// triggers a write (several do, e.g. `save_selected_citation_marks_it_
    /// saved_and_records_the_source_article` before this fix explicitly
    /// reset `app.research` to guard against exactly this) would corrupt
    /// that real data. `main::run` — production's one entry point — is the
    /// only place that installs the real, on-disk stores (see `App.history`'s
    /// doc comment for the established precedent this mirrors).
    #[test]
    fn app_new_defaults_every_persisted_store_to_in_memory() {
        let app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(
            app.research.is_in_memory(),
            "research must not resolve the real platform data directory"
        );
        assert!(
            app.bookmarks.is_in_memory(),
            "bookmarks must not resolve the real platform data directory"
        );
        assert!(
            app.readlater.is_in_memory(),
            "read-later must not resolve the real platform data directory"
        );
        assert!(
            app.saved.is_in_memory(),
            "saved pages must not resolve the real platform data directory"
        );
        assert!(
            app.fetch_queue.is_in_memory(),
            "the fetch queue must not resolve the real platform state directory"
        );
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
    fn prefetch_active_respects_kill_switch_and_incognito() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(!app.prefetch_active(), "no substrate installed yet");
        app.prefetch = Some(crate::netqueue::SubstrateHandle::new(
            crate::netqueue::SubstrateConfig::default(),
        ));
        assert!(app.prefetch_active(), "installed, enabled, not incognito");
        app.set_prefetch(false);
        assert!(!app.prefetch_active(), "FR-PF-6 kill switch off");
        app.set_prefetch(true);
        app.incognito = true;
        assert!(
            !app.prefetch_active(),
            "FR-PR-3 incognito forces prefetch off"
        );
    }

    // -- Start page, on-this-day panel, TIL widget (PRD FR-DL-1,2,7) -------

    fn feed_cache_with(date: &str, feed: crate::prefetch::FeaturedFeed) -> Arc<Mutex<FeedCache>> {
        let mut cache = FeedCache::default();
        cache.store(date.to_string(), feed);
        Arc::new(Mutex::new(cache))
    }

    fn sample_feed() -> crate::prefetch::FeaturedFeed {
        crate::prefetch::FeaturedFeed {
            tfa: Some("Alan Turing".to_string()),
            extract: Some("A mathematician.".to_string()),
            mostread: vec![crate::prefetch::MostRead {
                title: "Enigma machine".to_string(),
                views: 1000,
            }],
            potd: None,
            potd_thumb_url: None,
            news: Vec::new(),
            onthisday: vec![
                crate::prefetch::OtdEntry {
                    year: Some(1912),
                    text: "Turing born.".to_string(),
                    page_title: Some("Alan Turing".to_string()),
                },
                crate::prefetch::OtdEntry {
                    year: Some(1954),
                    text: "Turing died.".to_string(),
                    page_title: None,
                },
            ],
            onthisday_events: 2,
        }
    }

    #[test]
    fn start_page_model_without_a_feed_cache_is_the_graceful_offline_fallback() {
        let app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(app.feed_cache.is_none(), "no substrate installed in tests");
        assert!(!app.start_page_pending());
        let model = app.start_page_model();
        assert!(!model.loading);
        assert!(
            model.offline,
            "no feed cache at all degrades to offline, not an error"
        );
    }

    #[test]
    fn start_page_pending_is_false_once_the_feed_has_arrived() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.feed_cache = Some(feed_cache_with("2026-07-14", sample_feed()));
        assert!(
            !app.start_page_pending(),
            "the feed already arrived — nothing left to wait for"
        );
    }

    #[test]
    fn start_page_model_uses_the_cached_feed_when_present() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.feed_cache = Some(feed_cache_with("2026-07-14", sample_feed()));
        let model = app.start_page_model();
        assert!(!model.loading);
        assert!(!model.offline);
        assert!(
            model
                .items
                .iter()
                .any(|i| i.section == startpage::Section::Tfa && i.title == "Alan Turing")
        );
        assert!(
            model
                .items
                .iter()
                .any(|i| i.section == startpage::Section::Til),
            "a TIL fact is picked from the cached feed's onthisday entries"
        );
    }

    #[test]
    fn start_page_move_and_open_target_navigate_the_cached_model() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.feed_cache = Some(feed_cache_with("2026-07-14", sample_feed()));
        let len = app.start_page_model().items.len();
        assert!(len >= 2, "fixture has at least TFA + mostread");

        app.start_page_move(1);
        assert_eq!(app.start_selected, 1);
        let (_, title_at_1) = app.start_page_open_target().unwrap();
        assert_eq!(app.start_page_model().items[1].title, title_at_1);

        // Wrapping: from the top, moving up lands on the last item.
        app.start_selected = 0;
        app.start_page_move(-1);
        assert_eq!(app.start_selected, len - 1);
    }

    #[test]
    fn reroll_til_can_change_the_picked_fact() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.feed_cache = Some(feed_cache_with("2026-07-14", sample_feed()));
        let mut texts = std::collections::HashSet::new();
        for _ in 0..sample_feed().onthisday.len() {
            let til = app
                .start_page_model()
                .items
                .into_iter()
                .find(|i| i.section == startpage::Section::Til)
                .map(|i| i.title);
            if let Some(t) = til {
                texts.insert(t);
            }
            app.reroll_til();
        }
        assert!(
            texts.len() > 1,
            "stepping the reroll seed across the candidate count must surface more than one fact: {texts:?}"
        );
    }

    #[test]
    fn go_home_blanks_the_active_tab_and_preserves_back_history() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        assert!(app.active_tab().doc.is_some());

        app.go_home();

        assert!(
            app.active_tab().doc.is_none(),
            "home clears the tab's content"
        );
        assert_eq!(
            back_titles(&app),
            vec!["Alan Turing"],
            "the outgoing article is pushed onto the back stack, browser-style"
        );
        assert_eq!(app.mode, Mode::Reading);
        assert_eq!(app.start_selected, 0);
    }

    #[test]
    fn open_and_close_on_this_day_restores_the_prior_mode() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.mode = Mode::Reading;

        app.open_on_this_day();
        assert_eq!(app.mode, Mode::OnThisDay);
        assert_eq!(app.otd_tab, OtdType::Events);
        assert_eq!(app.otd_selected, 0);

        app.close_on_this_day();
        assert_eq!(app.mode, Mode::Reading);
    }

    #[test]
    fn otd_navigation_moves_selection_and_switches_type_tabs() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_on_this_day();
        app.otd.set(
            OtdType::Events,
            vec![
                crate::prefetch::OtdEntry {
                    year: Some(1912),
                    text: "Event one.".to_string(),
                    page_title: Some("Alan Turing".to_string()),
                },
                crate::prefetch::OtdEntry {
                    year: None,
                    text: "Event two.".to_string(),
                    page_title: None,
                },
            ],
        );

        app.otd_move(1);
        assert_eq!(app.otd_selected, 1);
        assert_eq!(
            app.otd_open_target(),
            None,
            "the second entry links nothing"
        );

        app.otd_move(-1);
        assert_eq!(app.otd_selected, 0);
        assert_eq!(app.otd_open_target(), Some("Alan Turing".to_string()));

        app.otd_next_tab();
        assert_eq!(app.otd_tab, OtdType::Births);
        assert_eq!(app.otd_selected, 0, "switching type resets the selection");

        app.otd_prev_tab();
        assert_eq!(app.otd_tab, OtdType::Events);
    }

    // ---- Related panel (PRD FR-SR-6) --------------------------------------

    fn related_result(title: &str, description: &str) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            description: Some(description.to_string()),
            excerpt: None,
            size: None,
            wordcount: None,
            timestamp: None,
        }
    }

    #[test]
    fn open_related_with_no_article_shows_a_status_and_needs_no_fetch() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(!app.open_related(), "no article open — nothing to fetch");
        assert_eq!(app.mode, Mode::Related);
        assert_eq!(app.related_items().len(), 0);
        assert_eq!(app.status, "Open an article first");
    }

    #[test]
    fn open_related_needs_a_fetch_the_first_time_then_hits_the_session_cache() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));

        assert!(
            app.open_related(),
            "first open for this article — nothing cached yet"
        );
        assert!(app.related_loading);

        app.deliver_related(
            "".to_string(),
            "en".to_string(),
            "Alan Turing".to_string(),
            Ok(vec![related_result("Enigma machine", "cipher device")]),
        );
        assert!(!app.related_loading);
        assert_eq!(app.related_items().len(), 1);

        // Close and reopen: the session cache already has this article's
        // results, so no second fetch is requested (PRD FR-SR-6 "cached
        // per-article for the session").
        app.close_related();
        assert!(!app.open_related(), "a cache hit needs no second fetch");
        assert_eq!(app.related_items()[0].title, "Enigma machine");
    }

    #[test]
    fn related_move_wraps_and_enter_target_reads_the_selected_title() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_related();
        app.deliver_related(
            "".to_string(),
            "en".to_string(),
            "Alan Turing".to_string(),
            Ok(vec![
                related_result("Enigma machine", "a"),
                related_result("Computer science", "b"),
            ]),
        );

        assert_eq!(
            app.related_open_target(),
            Some("Enigma machine".to_string())
        );
        app.related_move(1);
        assert_eq!(
            app.related_open_target(),
            Some("Computer science".to_string())
        );
        app.related_move(1);
        assert_eq!(
            app.related_open_target(),
            Some("Enigma machine".to_string()),
            "wraps back to the first item"
        );
        app.related_move(-1);
        assert_eq!(
            app.related_open_target(),
            Some("Computer science".to_string())
        );
    }

    #[test]
    fn related_gracefully_handles_an_empty_or_failed_fetch() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_related();
        app.deliver_related(
            "".to_string(),
            "en".to_string(),
            "Alan Turing".to_string(),
            Err("network error".to_string()),
        );
        assert!(!app.related_loading);
        assert_eq!(
            app.related_items().len(),
            0,
            "a failed fetch caches an empty list, not nothing"
        );
        assert_eq!(app.related_open_target(), None);
        assert_eq!(app.status, "No related articles found — Esc to close");
    }

    /// A result that lands after the reader has already backed out of the
    /// panel (or navigated elsewhere) must not touch the live status/loading
    /// state — but it's still worth caching for when they come back.
    #[test]
    fn a_late_related_result_after_closing_the_panel_is_cached_but_does_not_touch_live_state() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_related();
        app.close_related(); // reader backed out before the fetch landed

        app.deliver_related(
            "".to_string(),
            "en".to_string(),
            "Alan Turing".to_string(),
            Ok(vec![related_result("Enigma machine", "cipher device")]),
        );
        assert_eq!(app.mode, Mode::Reading, "closing the panel is not undone");

        // The result is still cached: reopening needs no second fetch.
        assert!(!app.open_related());
        assert_eq!(app.related_items().len(), 1);
    }

    // -- Language switcher & fallback chain (PRD FR-ML-1/2) ------------------

    fn langlink(code: &str, autonym: &str, langname: &str, title: &str) -> crate::api::LangLink {
        crate::api::LangLink {
            code: code.to_string(),
            autonym: autonym.to_string(),
            langname: langname.to_string(),
            title: title.to_string(),
            url: None,
        }
    }

    /// PRD FR-ML-1: preferred languages are pinned to the top, in the
    /// reader's own configured order — not the server's — and everything
    /// else keeps the server's original order behind them.
    #[test]
    fn order_and_filter_langlinks_pins_preferred_to_top_in_configured_order() {
        let links = vec![
            langlink("de", "Deutsch", "German", "Alan Turing"),
            langlink("ja", "日本語", "Japanese", "アラン・チューリング"),
            langlink("fr", "Français", "French", "Alan Turing"),
        ];
        // Configured preference is ja first, then de — the reverse of the
        // server's own de/ja/fr order — so a correct pin proves the rows
        // follow `preferred`, not `links`.
        let preferred = vec!["ja".to_string(), "de".to_string()];
        let rows = order_and_filter_langlinks(&links, &preferred, "");
        let codes: Vec<&str> = rows.iter().map(|l| l.code.as_str()).collect();
        assert_eq!(
            codes,
            vec!["ja", "de", "fr"],
            "ja and de pinned in preferred's own order; fr (unpreferred) follows"
        );
    }

    /// No `languages` configured: nothing is pinned, and the server's own
    /// order survives untouched.
    #[test]
    fn order_and_filter_langlinks_with_no_preferred_keeps_server_order() {
        let links = vec![
            langlink("de", "Deutsch", "German", "Alan Turing"),
            langlink("ja", "日本語", "Japanese", "アラン・チューリング"),
        ];
        let rows = order_and_filter_langlinks(&links, &[], "");
        let codes: Vec<&str> = rows.iter().map(|l| l.code.as_str()).collect();
        assert_eq!(codes, vec!["de", "ja"]);
    }

    /// PRD FR-ML-1's fuzzy filter matches autonym, English langname, or
    /// code — whichever the reader typed — via the shared subsequence
    /// matcher, not a substring/prefix rule.
    #[test]
    fn order_and_filter_langlinks_fuzzy_filters_over_autonym_langname_and_code() {
        let links = vec![
            langlink("de", "Deutsch", "German", "Alan Turing"),
            langlink("ja", "日本語", "Japanese", "アラン・チューリング"),
            langlink("fr", "Français", "French", "Alan Turing"),
        ];

        // "germ" is a subsequence of the English langname "German" only.
        let by_langname = order_and_filter_langlinks(&links, &[], "germ");
        assert_eq!(
            by_langname
                .iter()
                .map(|l| l.code.as_str())
                .collect::<Vec<_>>(),
            vec!["de"]
        );

        // "fran" is a subsequence of the autonym "Français".
        let by_autonym = order_and_filter_langlinks(&links, &[], "fran");
        assert_eq!(
            by_autonym
                .iter()
                .map(|l| l.code.as_str())
                .collect::<Vec<_>>(),
            vec!["fr"]
        );

        // The bare code itself always matches.
        let by_code = order_and_filter_langlinks(&links, &[], "ja");
        assert_eq!(
            by_code.iter().map(|l| l.code.as_str()).collect::<Vec<_>>(),
            vec!["ja"]
        );

        // No match at all: an empty result, not an error.
        assert!(order_and_filter_langlinks(&links, &[], "xyz").is_empty());
    }

    /// PRD FR-ML-2's hint: present only when the current language is
    /// genuinely non-preferred and a preferred edition exists; absent in
    /// every other case (no preferred configured, already reading a
    /// preferred edition, or no matching langlink).
    #[test]
    fn preferred_language_hint_surfaces_only_for_a_non_preferred_article_with_a_match() {
        let links = vec![
            langlink("de", "Deutsch", "German", "Alan Turing"),
            langlink("ja", "日本語", "Japanese", "アラン・チューリング"),
        ];

        // Reading "en" (not in the langlinks at all) with ja preferred: hint.
        assert_eq!(
            preferred_language_hint(&links, "en", &["ja".to_string(), "fr".to_string()]),
            Some("日本語".to_string())
        );

        // No preferred languages configured at all: nothing to suggest.
        assert_eq!(preferred_language_hint(&links, "en", &[]), None);

        // Already reading a preferred edition ("de" is itself preferred):
        // FR-ML-2's trigger is explicitly "viewing a non-preferred language".
        assert_eq!(
            preferred_language_hint(&links, "de", &["de".to_string(), "ja".to_string()]),
            None
        );

        // Preferred languages configured, but none has a langlink here.
        assert_eq!(
            preferred_language_hint(&links, "en", &["zh".to_string()]),
            None
        );

        // Earliest-configured preferred match wins when more than one matches.
        assert_eq!(
            preferred_language_hint(&links, "en", &["ja".to_string(), "de".to_string()]),
            Some("日本語".to_string()),
            "ja is listed first, so it wins over de even though de also matches"
        );
    }

    /// PRD FR-ML-2's fallback chain: primary first, then preferred
    /// languages in their configured order, deduplicated.
    #[test]
    fn fallback_chain_orders_primary_first_then_preferred_deduplicated() {
        assert_eq!(
            fallback_chain("de", &["de".to_string(), "en".to_string()]),
            vec!["de".to_string(), "en".to_string()],
            "primary already equals the first preferred entry — no duplicate"
        );
        assert_eq!(
            fallback_chain("xx", &["de".to_string(), "en".to_string()]),
            vec!["xx".to_string(), "de".to_string(), "en".to_string()]
        );
        assert_eq!(
            fallback_chain("en", &[]),
            vec!["en".to_string()],
            "no languages configured — a single-entry chain, pre-FR-ML-2 behavior"
        );
    }

    #[test]
    fn open_lang_picker_with_no_article_shows_a_status_and_needs_no_fetch() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(
            !app.open_lang_picker(),
            "no article open — nothing to fetch"
        );
        assert_eq!(app.mode, Mode::LangPicker);
        assert!(app.lang_picker_rows().is_empty());
        assert_eq!(app.status, "Open an article first");
    }

    #[test]
    fn open_lang_picker_needs_a_fetch_the_first_time_then_hits_the_session_cache() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));

        assert!(
            app.open_lang_picker(),
            "first open for this article — nothing cached yet"
        );
        assert!(app.lang_loading);

        app.deliver_langlinks(
            "".to_string(),
            "en".to_string(),
            "Alan Turing".to_string(),
            Ok(vec![langlink("de", "Deutsch", "German", "Alan Turing")]),
        );
        assert!(!app.lang_loading);
        assert_eq!(app.lang_picker_rows().len(), 1);

        // Close and reopen: the session cache already has this article's
        // langlinks, so no second fetch is requested.
        app.close_lang_picker();
        assert!(!app.open_lang_picker(), "a cache hit needs no second fetch");
        assert_eq!(app.lang_picker_rows()[0].code, "de");
    }

    #[test]
    fn cycle_lang_wraps_and_lang_picker_target_reads_the_selected_row() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_lang_picker();
        app.deliver_langlinks(
            "".to_string(),
            "en".to_string(),
            "Alan Turing".to_string(),
            Ok(vec![
                langlink("de", "Deutsch", "German", "Alan Turing"),
                langlink("ja", "日本語", "Japanese", "アラン・チューリング"),
            ]),
        );

        assert_eq!(
            app.lang_picker_target(),
            Some(("de".to_string(), "Alan Turing".to_string()))
        );
        app.cycle_lang(true);
        assert_eq!(
            app.lang_picker_target(),
            Some(("ja".to_string(), "アラン・チューリング".to_string()))
        );
        app.cycle_lang(true);
        assert_eq!(
            app.lang_picker_target(),
            Some(("de".to_string(), "Alan Turing".to_string())),
            "wraps back to the first row"
        );
        app.cycle_lang(false);
        assert_eq!(
            app.lang_picker_target(),
            Some(("ja".to_string(), "アラン・チューリング".to_string()))
        );
    }

    /// PRD FR-ML-4: bare `:wiki` opens the picker with the cursor already on
    /// the currently active wiki, not reset to the top — a reader who
    /// already switched sees where they are.
    #[test]
    fn open_wiki_picker_selects_the_currently_active_wiki() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.active_wiki_name = "wikiquote".to_string();
        app.open_wiki_picker();
        assert_eq!(app.mode, Mode::WikiPicker);
        assert_eq!(app.wiki_picker_target(), Some("wikiquote".to_string()));
    }

    /// An unrecognized `active_wiki_name` (shouldn't normally happen, but
    /// `App::new`'s own default before `main::run` wires the real config in
    /// is one — see its doc comment) degrades to selecting the first row
    /// rather than panicking on `.unwrap()`.
    #[test]
    fn open_wiki_picker_defaults_to_the_first_row_for_an_unknown_active_wiki() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.active_wiki_name = "some-custom-site".to_string();
        app.open_wiki_picker();
        assert_eq!(app.selected_wiki_pick, 0);
    }

    #[test]
    fn cycle_wiki_pick_wraps_over_the_five_known_projects() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_wiki_picker(); // wikipedia, index 0
        assert_eq!(app.wiki_picker_target(), Some("wikipedia".to_string()));
        app.cycle_wiki_pick(false); // wraps backward to the last row
        assert_eq!(app.wiki_picker_target(), Some("wikinews".to_string()));
        app.cycle_wiki_pick(true);
        assert_eq!(app.wiki_picker_target(), Some("wikipedia".to_string()));
    }

    #[test]
    fn lang_picker_gracefully_handles_an_empty_or_failed_fetch() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_lang_picker();
        app.deliver_langlinks(
            "".to_string(),
            "en".to_string(),
            "Alan Turing".to_string(),
            Err("network error".to_string()),
        );
        assert!(!app.lang_loading);
        assert!(
            app.lang_picker_rows().is_empty(),
            "a failed fetch caches an empty list, not nothing"
        );
        assert_eq!(app.lang_picker_target(), None);
        assert_eq!(app.status, "No language editions found — Esc to close");
    }

    /// PRD FR-ML-2's `:lang <code>` disambiguation: a code with a cached
    /// langlink resolves to its translated title; a code without one (not
    /// yet loaded, an empty cache entry, or genuinely absent) is `None` —
    /// the signal `main::set_or_switch_lang` uses to fall back to setting
    /// the default language instead of switching.
    #[test]
    fn lang_link_title_for_code_resolves_present_codes_and_is_none_otherwise() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));

        // Not loaded yet: no cache entry at all.
        assert_eq!(app.lang_link_title_for_code("de"), None);

        app.langlinks_cache.insert(
            (String::new(), "en".to_string(), "Alan Turing".to_string()),
            vec![langlink("ja", "日本語", "Japanese", "アラン・チューリング")],
        );
        assert_eq!(
            app.lang_link_title_for_code("ja"),
            Some("アラン・チューリング".to_string())
        );
        assert_eq!(
            app.lang_link_title_for_code("de"),
            None,
            "cached, but this article has no de edition"
        );
    }

    /// A fresh document (a real navigation, or `go_home`) clears whatever
    /// hint belonged to the article just left — PRD FR-ML-2's hint must
    /// never survive onto a different page's status line.
    #[test]
    fn a_fresh_document_clears_any_stale_language_hint() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.language_hint = Some("also in Deutsch — :lang".to_string());

        app.open_document(doc("Enigma machine"));
        assert_eq!(app.language_hint, None);
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
                redlink: false,
            },
            LinkRef {
                href: "./B".into(),
                text: "B".into(),
                internal_title: Some("B".into()),
                redlink: false,
            },
            LinkRef {
                href: "./C".into(),
                text: "C".into(),
                internal_title: Some("C".into()),
                redlink: false,
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

    /// PRD FR-NV-4/FR-RD-6 (this chunk's fix): a reference marker is no
    /// longer part of `collect_links`'s output at all (wired in through
    /// `set_document`), so Tab-cycling has nothing non-navigable to land on
    /// — it skips straight from one real link to the next.
    #[test]
    fn cycle_link_only_ever_lands_on_real_links_never_a_reference_marker() {
        let html = "<html><body>\
            <p>See <a href=\"./Enigma_machine\">Enigma</a><sup class=\"reference\">\
            <a href=\"#cite_note-1\">[1]</a></sup> and \
            <a href=\"./Alan_Turing\">Turing</a><sup class=\"reference\">\
            <a href=\"#cite_note-2\">[2]</a></sup>.</p>\
            </body></html>";
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", html));

        assert_eq!(
            app.active_tab().links.len(),
            2,
            "only the two real links, no reference markers: {:?}",
            app.active_tab().links
        );
        assert!(
            app.active_tab()
                .links
                .iter()
                .all(|l| !l.href.starts_with('#')),
            "no reference marker belongs in the followable set"
        );

        for _ in 0..6 {
            app.cycle_link(true);
            let href = app
                .active_tab()
                .focused_link
                .and_then(|i| app.active_tab().links.get(i))
                .map(|l| l.href.as_str())
                .unwrap_or("");
            assert!(
                !href.starts_with('#'),
                "Tab must never focus a reference marker, got {href:?}"
            );
        }
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

    // ---- PRD FR-NV-2 `gs` fuzzy section jump ------------------------------

    fn section_jump_html() -> &'static str {
        "<html><body><p>lead</p>\
         <h2>History</h2><p>history body</p>\
         <h2>Legacy and impact</h2><p>legacy body</p>\
         <h2>See also</h2><p>see-also body</p></body></html>"
    }

    #[test]
    fn filter_sections_ranks_the_best_fuzzy_match_first_ties_in_document_order() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", section_jump_html()));
        let sections = &app.active_tab().sections;
        assert_eq!(sections.len(), 3);

        // An empty query matches everything, ties broken by document order —
        // not alphabetically, which would put "History" after "Legacy...".
        let all = filter_sections(sections, "");
        assert_eq!(all, vec![0, 1, 2]);

        // "legacy" is a tight contiguous match against "Legacy and impact"
        // only — the other two titles don't contain it as a subsequence at
        // all, so they're absent, not merely ranked lower.
        let legacy = filter_sections(sections, "legacy");
        assert_eq!(legacy, vec![1]);
    }

    #[test]
    fn gs_opens_filters_and_jumps_to_the_selected_section() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", section_jump_html()));
        app.layout_width = 80;
        // A unit test never draws, so `max_scroll` (normally set by
        // `draw_reading` from the real viewport height) stays at its
        // default 0 — generous enough here that `jump_to_section`'s own
        // clamp never kicks in, matching `jump_to_section_clamps_to_max_
        // scroll`'s own workaround for the same gap.
        app.active_tab_mut().max_scroll = 200;
        app.mode = Mode::Reading;

        app.open_section_jump();
        assert_eq!(app.mode, Mode::SectionJump);
        assert_eq!(
            app.section_jump_rows().len(),
            3,
            "an empty filter lists every section"
        );

        // Typing narrows to just "See also".
        for c in "see".chars() {
            app.section_jump_input.push(c);
            app.section_jump_selected = 0;
        }
        let rows = app.section_jump_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.title, "See also");

        app.confirm_section_jump();
        assert_eq!(
            app.mode,
            Mode::Reading,
            "a confirmed jump returns to Reading"
        );
        // The jump landed on the matched section's own block, not section 0
        // — `jump_to_section` is fed the *original* index the filtered row
        // carried, not its position in the narrowed list.
        let expected_line = app.layout.as_ref().unwrap().block_lines[rows[0].1.block];
        assert_eq!(app.active_tab().scroll, expected_line as u16);
    }

    #[test]
    fn gs_with_no_match_confirms_back_to_reading_without_moving_scroll() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", section_jump_html()));
        app.layout_width = 80;
        app.open_section_jump();
        app.section_jump_input = "zzz-no-such-section".to_string();
        assert!(app.section_jump_rows().is_empty());

        let scroll_before = app.active_tab().scroll;
        app.confirm_section_jump();
        assert_eq!(app.mode, Mode::Reading);
        assert_eq!(app.active_tab().scroll, scroll_before);
    }

    #[test]
    fn section_jump_move_wraps_within_the_filtered_list_only() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", section_jump_html()));
        app.open_section_jump();
        app.section_jump_input = "history".to_string();
        assert_eq!(app.section_jump_rows().len(), 1);

        app.section_jump_move(1);
        assert_eq!(
            app.section_jump_selected, 0,
            "clamped to the single filtered row, not the full 3-section list"
        );
    }

    // ---- PRD FR-CS-2 `:` command-line Tab-completion ----------------------

    #[test]
    fn complete_command_tab_completes_the_command_name_and_cycles_ties() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.command_input = "hi".to_string();
        assert_eq!(app.complete_command_tab(), None, "no network fetch needed");
        assert_eq!(
            app.command_input, "history",
            "the only command starting with \"hi\""
        );

        // A prefix with more than one match cycles through them on repeat
        // Tab rather than picking arbitrarily and stopping.
        app.command_input = "se".to_string();
        app.complete_command_tab();
        let first = app.command_input.clone();
        app.complete_command_tab();
        let second = app.command_input.clone();
        assert_ne!(first, second, "a second Tab cycles to the next match");
        assert!(first.starts_with("se") && second.starts_with("se"));
    }

    #[test]
    fn complete_command_tab_on_an_unknown_prefix_is_a_no_op() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.command_input = "zzzznotacommand".to_string();
        assert_eq!(app.complete_command_tab(), None);
        assert_eq!(
            app.command_input, "zzzznotacommand",
            "no match means the input is left exactly as typed"
        );
    }

    #[test]
    fn complete_command_tab_on_open_with_a_title_requests_a_typeahead_fetch() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.command_input = "open Alan Tur".to_string();
        let query = app.complete_command_tab();
        assert_eq!(
            query.as_deref(),
            Some("Alan Tur"),
            "no completions cached yet, so the caller must go fetch some"
        );
        assert!(app.command_typeahead_loading);

        // A landed fetch for that exact title installs and cycles on the
        // very next Tab, without a second network round trip.
        *app.command_typeahead_slot.lock().unwrap() = Some((
            "Alan Tur".to_string(),
            vec![
                "Alan Turing".to_string(),
                "Alan Turing Institute".to_string(),
            ],
        ));
        let query = app.complete_command_tab();
        assert_eq!(query, None, "a cached match needs no fetch");
        assert_eq!(app.command_input, "open Alan Turing");
        assert!(!app.command_typeahead_loading);

        let query = app.complete_command_tab();
        assert_eq!(query, None);
        assert_eq!(
            app.command_input, "open Alan Turing Institute",
            "a third Tab cycles to the second candidate"
        );
    }

    #[test]
    fn complete_command_tab_ignores_non_open_commands_with_arguments() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.command_input = "lang de".to_string();
        assert_eq!(
            app.complete_command_tab(),
            None,
            "only :open/:o get title completion"
        );
        assert_eq!(app.command_input, "lang de", "left untouched");
    }

    // ---- PRD FR-NV-10 visual-selection yank -------------------------------

    fn visual_html() -> &'static str {
        "<html><body><p>alpha line</p><p>bravo line</p><p>charlie line</p>\
         <p>delta line</p><p>echo line</p></body></html>"
    }

    #[test]
    fn enter_visual_anchors_at_the_current_scroll_and_needs_a_document() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.enter_visual();
        assert_eq!(
            app.mode,
            Mode::Reading,
            "no document open — visual mode must not start"
        );

        app.set_document(crate::doc::parse_article_html("T", visual_html()));
        app.layout_width = 80;
        app.active_tab_mut().scroll = 2;
        app.enter_visual();
        assert_eq!(app.mode, Mode::Visual);
        assert_eq!(app.visual_anchor_line, 2);
        assert_eq!(app.visual_cursor_line, 2);
        assert_eq!(app.visual_selected_range(), (2, 2));
    }

    #[test]
    fn visual_move_extends_the_range_in_either_direction_from_the_anchor() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", visual_html()));
        app.layout_width = 80;
        app.viewport_height = 10;
        app.enter_visual();
        assert_eq!(app.visual_anchor_line, 0);

        app.visual_move(1);
        app.visual_move(1);
        assert_eq!(
            app.visual_selected_range(),
            (0, 2),
            "the anchor stays put; the range grows toward the cursor"
        );

        // Reversing past the anchor flips which end is the low side —
        // `visual_selected_range` is always (min, max), not (anchor, cursor).
        app.visual_move(-4);
        assert_eq!(app.visual_cursor_line, 0);
        assert_eq!(app.visual_selected_range(), (0, 0));
    }

    #[test]
    fn visual_move_clamps_to_the_laid_out_document() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", visual_html()));
        app.layout_width = 80;
        app.viewport_height = 10;
        app.enter_visual();

        app.visual_move(-5);
        assert_eq!(app.visual_cursor_line, 0, "must not go negative");

        app.visual_move(1000);
        let max_line = app.layout.as_ref().unwrap().lines.len() - 1;
        assert_eq!(app.visual_cursor_line, max_line, "clamped to the last line");
    }

    #[test]
    fn visual_selected_text_joins_the_selected_lines_plain_text() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", visual_html()));
        app.layout_width = 80;
        app.viewport_height = 10;
        app.ensure_layout();
        // Each `<p>` lays out on its own line with a blank separator line
        // between (the layout engine's own paragraph spacing) — line 3 is
        // "alpha line", line 5 is "bravo line", per the actual laid-out
        // document, not an assumption about contiguous body text.
        assert_eq!(
            app.layout.as_ref().unwrap().lines[3]
                .spans
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>(),
            "alpha line"
        );
        app.active_tab_mut().scroll = 3;
        app.enter_visual();
        app.visual_move(2); // extend down to line 5, "bravo line"

        let text = app.visual_selected_text().expect("a layout exists");
        assert_eq!(
            text, "alpha line\n\nbravo line",
            "the blank separator line between paragraphs is part of the selection too"
        );
    }

    // ---- PRD FR-NV-3 section folding --------------------------------------

    const FOLD_HTML: &str = "<html><body><p>lead</p>\
        <h2>History</h2><p>history body needle one</p><p>history body two</p>\
        <h2>Legacy</h2><p>legacy body text</p></body></html>";

    fn laid_text(app: &App) -> String {
        app.layout
            .as_ref()
            .map(|l| {
                l.lines
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|s| s.text.as_str())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    fn fold_app() -> App {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", FOLD_HTML));
        app.layout_width = 80;
        app.viewport_height = 6;
        app.ensure_layout();
        app
    }

    #[test]
    fn za_folds_the_section_at_the_cursor_and_toggles_back() {
        let mut app = fold_app();
        let hist_block = app.active_tab().sections[0].block;
        // Put the cursor in the History section.
        let line = app.layout.as_ref().unwrap().block_lines[hist_block];
        app.active_tab_mut().scroll = line as u16;

        app.toggle_fold_at_cursor();
        assert!(app.active_tab().folded_blocks.contains(&hist_block));
        app.ensure_layout();
        let folded = laid_text(&app);
        assert!(folded.contains("▸ History"), "summary line: {folded:?}");
        assert!(!folded.contains("history body needle one"), "body gone");
        assert!(folded.contains("legacy body text"), "other section intact");

        app.toggle_fold_at_cursor();
        assert!(
            !app.active_tab().folded_blocks.contains(&hist_block),
            "za on a folded section unfolds it"
        );
    }

    #[test]
    fn zm_folds_all_and_zr_unfolds_all() {
        let mut app = fold_app();
        app.fold_all();
        assert_eq!(app.active_tab().folded_blocks.len(), 2);
        app.ensure_layout();
        let folded = laid_text(&app);
        assert!(folded.contains("▸ History"));
        assert!(folded.contains("▸ Legacy"));
        assert!(!folded.contains("history body needle one"));

        app.unfold_all();
        assert!(app.active_tab().folded_blocks.is_empty());
        app.ensure_layout();
        assert!(laid_text(&app).contains("history body needle one"));
    }

    #[test]
    fn find_auto_unfolds_the_section_containing_a_hit() {
        let mut app = fold_app();
        let hist_block = app.active_tab().sections[0].block;
        app.fold_all();
        app.active_tab_mut().find_input = "needle".to_string();
        app.update_find();
        assert!(
            !app.active_tab().folded_blocks.contains(&hist_block),
            "the folded section with a hit auto-unfolds (PRD FR-NV-3)"
        );
        assert!(
            !app.active_tab().find_matches.is_empty(),
            "and the match is now locatable"
        );
    }

    #[test]
    fn cycle_link_skips_links_inside_a_folded_section() {
        let html = "<html><body><p>lead <a href=\"./Lead\">lead link</a></p>\
            <h2>History</h2><p>a <a href=\"./Hist\">hist link</a> here</p>\
            <h2>Legacy</h2><p>a <a href=\"./Leg\">legacy link</a></p></body></html>";
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", html));
        app.layout_width = 80;
        app.viewport_height = 20;
        // The middle link (occ 1) lives in History; fold that section.
        let hist_block = app.active_tab().sections[0].block;
        app.active_tab_mut().folded_blocks.insert(hist_block);
        app.ensure_layout();
        // Cycle through every link; focus must never land on the folded one.
        app.active_tab_mut().focused_link = None;
        let mut seen = Vec::new();
        for _ in 0..4 {
            app.cycle_link(true);
            seen.push(app.active_tab().focused_link.unwrap());
        }
        assert!(
            !seen.contains(&1),
            "the folded link (occ 1) is never focusable: {seen:?}"
        );
        assert!(seen.contains(&0) && seen.contains(&2));
    }

    #[test]
    fn cycle_link_reaches_a_link_after_a_folded_section_with_a_citation_marker() {
        // A folded section whose body holds a `#cite_note` reference marker
        // must not desync link numbering: the marker is excluded from
        // `collect_links` and gets no `SpanKind::Link` occurrence, so folding
        // it must not advance the counter past the real link that follows. If
        // it did (the `block_link_count` bug), that link would inherit a
        // phantom index its `link_visible` slot never sets, and Tab-cycling
        // would silently skip it.
        let html = "<html><body><p>lead <a href=\"./Lead\">lead link</a></p>\
            <h2>History</h2>\
            <p>a claim<sup class=\"reference\"><a href=\"#cite_note-1\">[1]</a></sup> here</p>\
            <h2>Legacy</h2><p>a <a href=\"./Leg\">legacy link</a></p></body></html>";
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", html));
        app.layout_width = 80;
        app.viewport_height = 20;
        // Two followable links; the citation marker is not one.
        assert_eq!(app.active_tab().links.len(), 2);
        let hist_block = app.active_tab().sections[0].block;
        app.active_tab_mut().folded_blocks.insert(hist_block);
        app.ensure_layout();
        // The visibility vector stays link-length, not inflated by the marker.
        assert_eq!(app.layout.as_ref().unwrap().link_visible.len(), 2);
        app.active_tab_mut().focused_link = None;
        let mut seen = Vec::new();
        for _ in 0..4 {
            app.cycle_link(true);
            seen.push(app.active_tab().focused_link.unwrap());
        }
        assert!(seen.contains(&0), "the lead link is reachable: {seen:?}");
        assert!(
            seen.contains(&1),
            "the link after the fold is reachable, not skipped: {seen:?}"
        );
    }

    // ---- PRD FR-NV-4 footnote peek + gK -----------------------------------

    fn cite(id: &str, text: &str) -> Citation {
        Citation {
            id: id.to_string(),
            text: text.to_string(),
            url: None,
        }
    }

    #[test]
    fn resolve_reference_matches_by_id_then_by_bracket_number() {
        let cites = vec![
            cite("cite_note-1", "First source"),
            cite("cite_note-2", "Second source"),
        ];
        assert_eq!(
            resolve_reference(&cites, "#cite_note-2", "[2]").map(|c| c.text.as_str()),
            Some("Second source")
        );
        // Fallback: a non-matching anchor but a bracket number.
        assert_eq!(
            resolve_reference(&cites, "#unknown", "[1]").map(|c| c.text.as_str()),
            Some("First source")
        );
        // Missing reference resolves to nothing, gracefully.
        assert!(resolve_reference(&cites, "#cite_note-9", "[9]").is_none());
    }

    #[test]
    fn is_reference_marker_detects_cite_anchors() {
        assert!(is_reference_marker("#cite_note-1"));
        assert!(is_reference_marker("#cite_ref-3"));
        assert!(!is_reference_marker("./Some_Article"));
        assert!(!is_reference_marker("https://example.com"));
    }

    #[test]
    fn find_references_section_prefers_the_trailing_apparatus() {
        let sections = vec![
            crate::doc::SectionRef {
                level: 2,
                title: "Notes on style".to_string(),
                block: 1,
            },
            crate::doc::SectionRef {
                level: 2,
                title: "History".to_string(),
                block: 5,
            },
            crate::doc::SectionRef {
                level: 2,
                title: "References".to_string(),
                block: 9,
            },
        ];
        assert_eq!(find_references_section(&sections), Some(2));
        // No apparatus at all → None.
        let none = vec![crate::doc::SectionRef {
            level: 2,
            title: "History".to_string(),
            block: 1,
        }];
        assert_eq!(find_references_section(&none), None);
    }

    #[test]
    fn k_on_a_reference_marker_opens_the_footnote_peek_without_network() {
        let html = "<html><body>\
            <p>Claim<sup class=\"reference\"><a href=\"#cite_note-1\">[1]</a></sup>.</p>\
            <div class=\"mw-references-wrap\"><ol class=\"references\">\
            <li id=\"cite_note-1\"><span class=\"reference-text\">Hodges, Andrew. The Enigma.</span></li>\
            </ol></div></body></html>";
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", html));
        // A `#cite_note-*` reference marker is no longer among
        // `doc::collect_links`'s output (it's a footnote marker, not a
        // followable link — see `collect_links`'s own doc comment), so
        // `Tab::install_document` never populates `tab.links` with one and
        // Tab-focus can no longer reach it. This test's remaining job is the
        // resolve-and-render machinery `open_peek_at_focus` still owns once
        // *something* names a reference marker's `(href, text)` — exercised
        // here by constructing that `LinkRef` directly, the same shape a
        // future reference-marker-aware trigger would hand it.
        app.active_tab_mut().links = vec![crate::doc::LinkRef {
            href: "#cite_note-1".to_string(),
            text: "[1]".to_string(),
            internal_title: None,
            redlink: false,
        }];
        app.active_tab_mut().focused_link = Some(0);
        let fetch = app.open_peek_at_focus();
        assert!(fetch.is_none(), "a footnote peek never triggers a fetch");
        assert_eq!(app.mode, Mode::Peek);
        match &app.peek {
            Some(PeekPopup::Footnote { text, .. }) => {
                assert!(text.contains("Hodges"), "resolved locally: {text:?}");
            }
            other => panic!("expected a footnote peek, got {other:?}"),
        }
        app.close_peek();
        assert_eq!(app.mode, Mode::Reading);
        assert!(app.peek.is_none());
    }

    #[test]
    fn k_peeks_the_nearest_reference_marker_without_a_focused_link() {
        // A `[1]` marker in the body and a matching References section. The
        // marker is not a followable link (0b86bf0) and cannot be Tab-focused,
        // so `K` must reach it by reading-position proximity — the footnote
        // peek path that regressed to dead code when markers left `links`.
        let html = "<html><body>\
            <p>Lead with a <a href=\"./Enigma\">real link</a>.</p>\
            <p>A claim<sup class=\"reference\"><a href=\"#cite_note-1\">[1]</a></sup> here.</p>\
            <h2>References</h2>\
            <div class=\"mw-references-wrap\"><ol class=\"references\">\
            <li id=\"cite_note-1\"><span class=\"reference-text\">Hodges, Andrew. The Enigma.</span></li>\
            </ol></div></body></html>";
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", html));
        app.layout_width = 80;
        app.viewport_height = 20;

        // 0b86bf0 intact: the marker is not in the followable link set.
        assert!(
            app.active_tab()
                .links
                .iter()
                .all(|l| !l.href.starts_with('#')),
            "reference markers stay out of the followable link set"
        );
        // But it is tracked for footnote peek.
        assert!(
            app.active_tab()
                .reference_markers
                .iter()
                .any(|m| m.href == "#cite_note-1"),
            "the marker is tracked as a reference marker"
        );

        // Tab-focus never lands on a marker: cycling only reaches the one real
        // link, whose href is not a `#`-anchor.
        app.active_tab_mut().focused_link = None;
        app.cycle_link(true);
        let focused = app
            .active_tab()
            .focused_link
            .expect("a real link is focusable");
        assert!(
            !is_reference_marker(&app.active_tab().links[focused].href),
            "Tab focus never lands on a reference marker"
        );

        // Put the reading position on the marker's paragraph, with no link
        // focused, and peek: the footnote branch fires again (was unreachable).
        app.ensure_layout();
        let marker_block = app.active_tab().reference_markers[0].block;
        let line = app.layout.as_ref().unwrap().block_lines[marker_block];
        app.active_tab_mut().scroll = line as u16;
        app.active_tab_mut().focused_link = None;

        let fetch = app.open_peek_at_focus();
        assert!(fetch.is_none(), "a footnote peek never triggers a fetch");
        assert_eq!(app.mode, Mode::Peek, "the footnote peek is reachable again");
        match &app.peek {
            Some(PeekPopup::Footnote { marker, text }) => {
                assert_eq!(marker, "[1]");
                assert!(text.contains("Hodges"), "resolved locally: {text:?}");
            }
            other => panic!("expected a footnote peek, got {other:?}"),
        }
    }

    #[test]
    fn k_prefers_a_nearby_reference_over_an_offscreen_focused_link() {
        // The focused link is at the top; the reader has scrolled down to a
        // citation. `K` peeks the reference in view, not the link scrolled off
        // above it (FR-NV-4 over a stale FR-NV-5 focus).
        let html = "<html><body>\
            <p>Lead with a <a href=\"./Enigma\">real link</a>.</p>\
            <p>one</p><p>two</p><p>three</p><p>four</p><p>five</p>\
            <p>A claim<sup class=\"reference\"><a href=\"#cite_note-1\">[1]</a></sup> here.</p>\
            <h2>References</h2>\
            <div class=\"mw-references-wrap\"><ol class=\"references\">\
            <li id=\"cite_note-1\"><span class=\"reference-text\">Hodges, Andrew. The Enigma.</span></li>\
            </ol></div></body></html>";
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", html));
        app.layout_width = 80;
        app.viewport_height = 3;
        app.ensure_layout();
        // The first link is focused (install default) but scrolled out of view.
        assert_eq!(app.active_tab().focused_link, Some(0));
        let marker_block = app.active_tab().reference_markers[0].block;
        let line = app.layout.as_ref().unwrap().block_lines[marker_block];
        assert!(
            line as u16 > app.viewport_height,
            "the marker is below the top link"
        );
        app.active_tab_mut().scroll = line as u16;

        let fetch = app.open_peek_at_focus();
        assert!(fetch.is_none(), "the footnote peek makes no fetch");
        match &app.peek {
            Some(PeekPopup::Footnote { text, .. }) => {
                assert!(
                    text.contains("Hodges"),
                    "peeked the nearby reference: {text:?}"
                );
            }
            other => panic!("expected a footnote peek, got {other:?}"),
        }
    }

    // ---- PRD FR-NV-5 link preview -----------------------------------------

    #[test]
    fn k_on_an_internal_link_opens_a_distinct_preview_and_fetches_once() {
        let html = "<html><body><p>See <a href=\"./Enigma_machine\">Enigma</a>.</p></body></html>";
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", html));
        app.active_tab_mut().focused_link = Some(0);

        let fetch = app.open_peek_at_focus();
        assert_eq!(
            fetch,
            Some(("en".to_string(), "Enigma machine".to_string())),
            "an uncached internal link asks the caller to fetch its summary"
        );
        assert_eq!(app.mode, Mode::Peek);
        assert!(app.summary_loading, "shows loading until the summary lands");
        assert!(app.peek_summary().is_none());
        assert!(matches!(app.peek, Some(PeekPopup::LinkPreview { .. })));

        // Deliver the summary.
        app.deliver_summary(
            "en".to_string(),
            "Enigma machine".to_string(),
            Ok(crate::api::SummaryData {
                title: "Enigma machine".to_string(),
                description: "cipher device".to_string(),
                extract: "The Enigma machine was a cipher device.".to_string(),
                thumbnail: None,
            }),
        );
        assert!(!app.summary_loading);
        assert_eq!(
            app.peek_summary().map(|s| s.description.as_str()),
            Some("cipher device")
        );

        // Reopen: a cache hit needs no second fetch.
        app.close_peek();
        app.active_tab_mut().focused_link = Some(0);
        assert!(
            app.open_peek_at_focus().is_none(),
            "the second peek is served from the session cache"
        );
        assert!(!app.summary_loading);
    }

    #[test]
    fn k_with_no_focused_link_peeks_nothing() {
        let html = "<html><body><p>No links here.</p></body></html>";
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("T", html));
        app.active_tab_mut().focused_link = None;
        assert!(app.open_peek_at_focus().is_none());
        assert_eq!(app.mode, Mode::Reading, "no popup opens");
    }

    // ---- PRD §10 / Appendix B: `i` / `:info` article-attribution overlay --

    #[test]
    fn i_opens_the_info_overlay_populated_from_the_open_article() {
        let html = "<html><body><p>lead paragraph</p></body></html>";
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html("Alan Turing", html));
        app.active_tab_mut().current_revid = 123456;

        assert!(app.open_info(), "an open article has attribution to show");
        assert_eq!(app.mode, Mode::Info);
        let info = app.info.as_ref().expect("overlay content is populated");
        assert_eq!(info.title, "Alan Turing");
        assert_eq!(
            info.canonical_url,
            "https://en.wikipedia.org/wiki/Alan_Turing"
        );
        assert_eq!(info.revid, 123456);
        assert!(info.license.contains("CC BY-SA 4.0"));
        assert!(info.permalink_url.contains("oldid=123456"));
        assert!(info.history_url.ends_with("action=history"));
        assert!(!info.retrieved_on.is_empty());

        app.close_info();
        assert_eq!(app.mode, Mode::Reading, "closing restores the prior mode");
        assert!(app.info.is_none());
    }

    #[test]
    fn info_with_no_article_open_reports_instead_of_opening() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(
            !app.open_info(),
            "nothing to attribute without an open article"
        );
        assert_eq!(app.mode, Mode::Reading, "no overlay opens");
        assert!(app.info.is_none());
        assert_eq!(app.status, "Open an article first");
    }

    // ---- PRD FR-NV-8 reading-position memory ------------------------------

    const RESUME_HTML: &str = "<html><body><p>lead paragraph</p>\
        <h2>History</h2><p>h1</p><p>h2</p><p>h3</p><p>h4</p><p>h5</p>\
        <h2>Legacy</h2><p>l1</p><p>l2</p><p>l3</p><p>l4</p><p>l5</p></body></html>";

    #[test]
    fn revisiting_offers_resume_and_r_restores_the_saved_scroll() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.layout_width = 80;
        app.viewport_height = 4;

        // First open of the article, scroll down, then navigate away (which
        // saves the position for the outgoing article).
        app.set_document(crate::doc::parse_article_html("Alan Turing", RESUME_HTML));
        app.active_tab_mut().current_revid = 5;
        app.ensure_layout();
        app.active_tab_mut().scroll = 7;
        assert!(
            app.pending_resume.is_none(),
            "no toast on a first, unread open"
        );
        app.set_document(crate::doc::parse_article_html(
            "Enigma machine",
            RESUME_HTML,
        ));

        // Reopen the first article: the saved position raises the resume toast.
        app.active_tab_mut().current_revid = 5;
        app.set_document(crate::doc::parse_article_html("Alan Turing", RESUME_HTML));
        assert!(app.pending_resume.is_some(), "revisit offers a resume");
        assert!(
            app.notice.as_deref().unwrap_or_default().contains("resume"),
            "a non-blocking toast is shown: {:?}",
            app.notice
        );

        app.ensure_layout();
        app.resume_to_saved_position();
        assert_eq!(app.active_tab().scroll, 7, "r restores the exact scroll");
        assert!(app.pending_resume.is_none(), "resume is consumed");
    }

    #[test]
    fn resume_falls_back_to_the_nearest_heading_when_the_revision_changed() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.layout_width = 80;
        app.viewport_height = 4;
        // Directly seed a saved position at an anchor, with a revid that won't
        // match, to exercise the fallback path deterministically.
        app.history
            .save_position("", "en", "Alan Turing", 5, 99, &[], Some("Legacy"));
        app.active_tab_mut().current_revid = 9; // different revision
        app.set_document(crate::doc::parse_article_html("Alan Turing", RESUME_HTML));
        assert!(app.pending_resume.is_some());
        let resume = app.pending_resume.clone().unwrap();
        assert!(
            !resume.revid_matches,
            "a changed revision uses the anchor path"
        );

        app.ensure_layout();
        app.active_tab_mut().max_scroll = 100; // a long viewport, as after a draw
        app.resume_to_saved_position();
        // Landed on the Legacy heading rather than the stale absolute offset.
        let legacy = app
            .active_tab()
            .sections
            .iter()
            .position(|s| s.title == "Legacy")
            .unwrap();
        let legacy_line =
            app.layout.as_ref().unwrap().block_lines[app.active_tab().sections[legacy].block];
        assert_eq!(app.active_tab().scroll as usize, legacy_line);
    }

    #[test]
    fn incognito_never_writes_a_reading_position() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.incognito = true;
        app.layout_width = 80;
        app.viewport_height = 4;
        app.set_document(crate::doc::parse_article_html("Alan Turing", RESUME_HTML));
        app.active_tab_mut().current_revid = 5;
        app.ensure_layout();
        app.active_tab_mut().scroll = 7;
        // Navigating away would save a position — but incognito denies it.
        app.set_document(crate::doc::parse_article_html(
            "Enigma machine",
            RESUME_HTML,
        ));
        assert!(
            app.history.position("", "en", "Alan Turing").is_none(),
            "incognito must not persist a reading position (privacy gate)"
        );
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
            "",
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
            crate::doc::render_plain(app.active_tab().doc.as_ref().unwrap(), "en")
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
            zero_results_message("Alan Truing", Some("Alan Turing"), None, 0),
            "No results for \"Alan Truing\". Did you mean Alan Turing? (Enter to search)"
        );
        assert_eq!(
            zero_results_message("xyzzy", None, None, 0),
            "No results for \"xyzzy\""
        );
    }

    /// FR-SR-4's rewrite clause takes over from the plain/did-you-mean
    /// wording — a rewritten query that still comes back empty is rare
    /// (normally it finds the results it was rewritten *for*), but the
    /// message must still make sense rather than silently ignoring it.
    #[test]
    fn zero_results_message_shows_the_rewritten_query_when_present() {
        assert_eq!(
            zero_results_message("teh alan tuning", None, Some("Alan Turing"), 0),
            "No results for \"Alan Turing\" (rewritten from \"teh alan tuning\")"
        );
    }

    /// FR-SR-4 / §7's offline-results section: an online zero-result search
    /// that still has local matches says so, appended to whichever base
    /// message applied; `0` suppresses the clause entirely (an ordinary
    /// zero-result message must not gain a stray suffix).
    #[test]
    fn zero_results_message_appends_the_offline_fallback_count_when_nonzero() {
        assert_eq!(
            zero_results_message("xyzzy", None, None, 3),
            "No results for \"xyzzy\"; 3 in your saved pages (:search-offline to browse)"
        );
        assert_eq!(
            zero_results_message("xyzzy", None, None, 0),
            "No results for \"xyzzy\"",
            "zero offline matches must not add the clause at all"
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

    /// PRD FR-PC-4: `layout_options` merges **config < session (`:set`) <
    /// this tab's override** — a tab with no override at all sees the
    /// session-global value; setting a `:set-tab` override on the active tab
    /// changes only that tab's `layout_options()`, a second tab (freshly
    /// pushed, no override) still sees the session-global default, and
    /// resetting (`value: None`) drops the override back to the session
    /// default for the tab that had it.
    #[test]
    fn tab_override_layers_over_session_which_layers_over_config_default() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        // "Config" (here: the built-in default baked into `App::new`) < "session"
        // (`:set measure=70`, mirroring what `execute_command`'s `measure` arm does).
        assert_eq!(app.layout_options().measure, 88, "config/session default");
        app.measure = 70;
        assert_eq!(
            app.layout_options().measure,
            70,
            "a plain `:set measure=70` changes the session-global value everyone without an override sees"
        );

        // A second tab, never touched, still sees the session-global value.
        let second = app.push_blank_tab("en".to_string());
        assert_eq!(
            app.layout_options().measure,
            70,
            "still on tab 0 — unaffected by pushing a new tab"
        );

        // `:set-tab measure=50` on the active tab (still tab 0) wins over the
        // session-global 70 for *this* tab only.
        app.set_tab_override("measure", Some("50"));
        assert_eq!(
            app.layout_options().measure,
            50,
            "the active tab's override wins over the session-global setting"
        );

        // Tab 1 (the one just pushed) was never given an override — switching
        // to it must show the session-global 70, not tab 0's 50.
        app.switch_to_tab(app.tab_index_by_id(second).unwrap());
        assert_eq!(
            app.layout_options().measure,
            70,
            "a different tab with no override of its own is unaffected"
        );

        // Back on tab 0: the override is still there until explicitly reset.
        app.switch_to_tab(0);
        assert_eq!(app.layout_options().measure, 50);
        app.set_tab_override("measure", None);
        assert_eq!(
            app.layout_options().measure,
            70,
            "`:set-tab measure=` (reset) falls back to the session-global value"
        );
    }

    /// The same three-rung precedence, exercised across every FR-PC-1
    /// spacing field at once (not just `measure`) plus `ambiguous_width` —
    /// each field is independent, so overriding one never disturbs another.
    #[test]
    fn tab_override_covers_every_spacing_field_independently() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.paragraph_spacing = 1;
        app.line_spacing = 0;
        app.word_spacing = 0;
        app.margin = 0;
        app.text_align = layout::TextAlign::Center;
        app.ambiguous_wide = false;

        app.set_tab_override("paragraph_spacing", Some("2"));
        app.set_tab_override("line_spacing", Some("1"));
        app.set_tab_override("word_spacing", Some("1"));
        app.set_tab_override("margin", Some("4"));
        app.set_tab_override("text_align", Some("left"));
        app.set_tab_override("ambiguous_width", Some("2"));

        let opts = app.layout_options();
        assert_eq!(opts.paragraph_spacing, 2);
        assert_eq!(opts.line_spacing, 1);
        assert_eq!(opts.word_spacing, 1);
        assert_eq!(opts.margin, 4);
        assert_eq!(opts.text_align, layout::TextAlign::Left);
        assert!(opts.ambiguous_wide);

        // Resetting one field never touches the others.
        app.set_tab_override("line_spacing", None);
        let opts = app.layout_options();
        assert_eq!(opts.line_spacing, 0, "reset back to the session default");
        assert_eq!(
            opts.paragraph_spacing, 2,
            "untouched by the line_spacing reset"
        );
        assert_eq!(opts.margin, 4, "untouched by the line_spacing reset");
    }

    /// PRD FR-PC-4 / FR-TH-7: `:set-tab images=` follows the same
    /// per-tab-wins-over-session precedence as the layout options, and
    /// `layout_options().images_on` (the L1 cache-key discriminator) agrees
    /// with `images_enabled()` at all times.
    #[test]
    fn tab_override_for_images_wins_over_the_session_global_toggle() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(!app.images_enabled(), "terminal theme defaults images off");
        app.set_images(true);
        assert!(app.images_enabled(), "session-global :set images=on");
        assert!(app.layout_options().images_on);

        app.set_tab_override("images", Some("off"));
        assert!(
            !app.images_enabled(),
            "this tab's override wins over the session-global on"
        );
        assert!(!app.layout_options().images_on);

        let second = app.push_blank_tab("en".to_string());
        app.switch_to_tab(app.tab_index_by_id(second).unwrap());
        assert!(
            app.images_enabled(),
            "a tab with no override of its own still sees the session-global on"
        );

        app.switch_to_tab(0);
        app.set_tab_override("images", None);
        assert!(
            app.images_enabled(),
            "reset falls back to session-global on"
        );
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
            link_visible: vec![],
            link_cols: vec![],
            continuation: vec![true],
            folds: vec![],
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

    // ---- OAuth login state (PRD FR-ACC-1) -------------------------------

    #[test]
    fn a_fresh_app_is_logged_out() {
        let app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(app.auth.is_none());
        assert_eq!(app.logged_in_username(), None);
        assert!(app.pending_login.is_none());
        assert_eq!(app.mode, Mode::Reading);
    }

    #[test]
    fn auth_runtime_is_configured_only_with_a_client_id() {
        let mut rt = AuthRuntime {
            client_id: String::new(),
            authorize_url: "a".into(),
            token_url: "t".into(),
            contact: "c".into(),
        };
        assert!(!rt.is_configured(), "no client_id → login unavailable");
        rt.client_id = "   ".into();
        assert!(
            !rt.is_configured(),
            "whitespace-only client_id is not configured"
        );
        rt.client_id = "real-consumer".into();
        assert!(rt.is_configured());
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
        assert!(app.bookmarks.is_bookmarked("", "en", "Alan Turing"));
        assert!(app.notice.as_deref().unwrap().contains("Bookmarked"));
        assert_eq!(app.bookmarks.bookmarks[0].revid_at_bookmark, Some(7));

        app.toggle_bookmark();
        assert!(!app.bookmarks.is_bookmarked("", "en", "Alan Turing"));
        assert!(app.notice.as_deref().unwrap().contains("Removed"));
    }

    #[test]
    fn toggle_bookmark_with_no_document_reports_instead_of_panicking() {
        let mut app = app_with_bookmarks();
        app.toggle_bookmark();
        assert!(app.status.contains("Open an article first"));
    }

    /// H1 (PRD FR-ML-4): `m` bookmarks under the on-screen tab's own wiki, so
    /// the same title bookmarked while reading a non-default wiki is stored
    /// under that wiki — independent of the default wiki's same-titled page.
    #[test]
    fn toggle_bookmark_records_under_the_tabs_wiki() {
        let mut app = app_with_bookmarks();
        app.set_document(doc("Mercury"));
        app.active_tab_mut().wiki = "wiktionary".to_string();

        app.toggle_bookmark();
        assert!(
            app.bookmarks.is_bookmarked("wiktionary", "en", "Mercury"),
            "bookmarked under the tab's wiki"
        );
        assert!(
            !app.bookmarks.is_bookmarked("", "en", "Mercury"),
            "not under the default wiki the tab isn't on"
        );
    }

    #[test]
    fn bookmark_picker_filter_narrows_the_visible_list() {
        let mut app = app_with_bookmarks();
        app.bookmarks.toggle("", "en", "Enigma machine", None);
        app.bookmarks
            .set_tags("", "en", "Enigma machine", vec!["crypto".into()]);
        app.bookmarks.toggle("", "en", "Ada Lovelace", None);

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
        app.bookmarks.toggle("", "en", "A", None);
        app.bookmarks.set_tags("", "en", "A", vec!["x".into()]);
        app.bookmarks.toggle("", "en", "B", None); // untagged
        app.bookmarks.toggle("", "en", "C", None);
        app.bookmarks.set_tags("", "en", "C", vec!["x".into()]);

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
        app.bookmarks.toggle("", "en", "A", None);
        app.bookmarks.toggle("", "en", "B", None);
        app.open_bookmark_picker();
        app.selected_bookmark = 1; // "B"

        app.delete_selected_bookmark();
        assert!(!app.bookmarks.is_bookmarked("", "en", "B"));
        assert!(app.bookmarks.is_bookmarked("", "en", "A"));
        assert_eq!(app.selected_bookmark, 0);
    }

    #[test]
    fn tag_edit_round_trips_through_the_prompt() {
        let mut app = app_with_bookmarks();
        app.bookmarks.toggle("", "en", "Alan Turing", None);
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
            app.bookmarks.find("", "en", "Alan Turing").unwrap().tags,
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
            wiki: String::new(),
            enqueued_at: crate::bookmarks::now_ts(),
            priority: 0,
        });
        app.readlater.enqueue(crate::bookmarks::ReadLaterEntry {
            title: "Second".to_string(),
            lang: "en".to_string(),
            wiki: String::new(),
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
            wiki: String::new(),
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
            wiki: String::new(),
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
        app.bookmarks.toggle("", "en", "Alan Turing", Some(1));

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
        app.bookmarks.toggle("", "en", "Alan Turing", None);
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

    /// PRD Appendix B's `g`-prefix chords, plus FR-SR-5's `gr` and
    /// FR-SR-6's `gR` added by this change — every recognized second key
    /// dispatches correctly, and an unrecognized one is a dead prefix
    /// (PassThrough), matching `gj` still scrolling.
    #[test]
    fn g_prefix_dispatches_every_chord_including_random_and_related() {
        assert_eq!(resolve_g_prefix('g'), GPrefixAction::Top);
        assert_eq!(resolve_g_prefix('t'), GPrefixAction::NextTab);
        assert_eq!(resolve_g_prefix('T'), GPrefixAction::PrevTab);
        assert_eq!(resolve_g_prefix('b'), GPrefixAction::BackStack);
        assert_eq!(resolve_g_prefix('h'), GPrefixAction::Home);
        assert_eq!(resolve_g_prefix('r'), GPrefixAction::Random);
        assert_eq!(resolve_g_prefix('R'), GPrefixAction::Related);
        assert_eq!(resolve_g_prefix('s'), GPrefixAction::SectionJump);
        assert_eq!(resolve_g_prefix('j'), GPrefixAction::PassThrough);
    }

    /// PRD FR-PR-3's `zz` incognito toggle plus FR-NV-3's folding chords
    /// (`za`/`zM`/`zR`); every other second key is a dead prefix.
    #[test]
    fn z_prefix_dispatches_zz_to_toggle_incognito_and_folding_chords() {
        assert_eq!(resolve_z_prefix('z'), ZPrefixAction::ToggleIncognito);
        assert_eq!(resolve_z_prefix('a'), ZPrefixAction::ToggleFold);
        assert_eq!(resolve_z_prefix('M'), ZPrefixAction::FoldAll);
        assert_eq!(resolve_z_prefix('R'), ZPrefixAction::UnfoldAll);
        assert_eq!(resolve_z_prefix('x'), ZPrefixAction::PassThrough);
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

    // ---- PRD FR-PF-3 interest-model wiring --------------------------------

    fn cats(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn score_of(app: &App, cat: &str) -> f64 {
        app.interest
            .top_categories(50)
            .into_iter()
            .find(|(c, _)| c == cat)
            .map(|(_, s)| s)
            .unwrap_or(0.0)
    }

    #[test]
    fn note_article_read_learns_the_articles_topics() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Enigma machine"));
        app.note_article_read(
            "",
            "Enigma machine",
            &cats(&[
                "Category:Cryptography",
                "Category:Articles with dead external links",
            ]),
        );
        assert_eq!(
            score_of(&app, "Cryptography"),
            crate::interest::SIGNAL_OPEN,
            "the open signal lands on the real topic"
        );
        assert_eq!(
            score_of(&app, "Articles with dead external links"),
            0.0,
            "maintenance categories are filtered out of the model"
        );
    }

    #[test]
    fn incognito_reading_does_not_update_the_interest_model() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.incognito = true;
        app.open_document(doc("Enigma machine"));
        app.note_article_read("", "Enigma machine", &cats(&["Category:Cryptography"]));
        assert!(
            app.interest.is_empty(),
            "incognito must leave the interest model untouched (privacy gate)"
        );
        assert!(!app.interest_active());
    }

    #[test]
    fn disabling_interest_learning_stops_updates() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.interest_learning = false;
        app.open_document(doc("Enigma machine"));
        app.note_article_read("", "Enigma machine", &cats(&["Category:Cryptography"]));
        assert!(app.interest.is_empty(), "learning off → no model updates");
    }

    #[test]
    fn bookmarking_applies_the_bookmark_signal_to_the_articles_topics() {
        // `app_with_bookmarks` gives an in-memory bookmark store — without it,
        // `toggle_bookmark` shares the real on-disk store across parallel
        // tests and can hit the Removed branch (no signal) nondeterministically.
        let mut app = app_with_bookmarks();
        app.open_document(doc("Enigma machine"));
        app.note_article_read("", "Enigma machine", &cats(&["Category:Cryptography"]));
        app.toggle_bookmark();
        // Approximate, not exact: the two signals go through the app's real
        // wall clock (`interest_now`), so a second boundary crossing between
        // them applies a negligible (< 1e-6) decay to the first — the sum is
        // 4.0 to any meaningful precision, never a brittle exact-f64 compare.
        let expected = crate::interest::SIGNAL_OPEN + crate::interest::SIGNAL_BOOKMARK;
        assert!(
            (score_of(&app, "Cryptography") - expected).abs() < 1e-3,
            "open (+1) plus the bookmark boost (+3) are both visible (got {})",
            score_of(&app, "Cryptography")
        );
    }

    #[test]
    fn not_interested_drives_the_current_articles_topics_negative() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Enigma machine"));
        app.note_article_read("", "Enigma machine", &cats(&["Category:Cryptography"]));
        app.mark_not_interested();
        assert!(
            score_of(&app, "Cryptography") < 0.0,
            "a veto outweighs the read"
        );
    }

    #[test]
    fn not_interested_reports_when_no_topics_are_known_yet() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Enigma machine"));
        // No categories fetched yet.
        app.mark_not_interested();
        assert!(app.interest.is_empty());
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("No topics")),
            "the reader is told there's nothing to act on yet"
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

    // ---- Trail / wander-graph view (PRD FR-HS-3) --------------------------

    #[test]
    fn open_trail_builds_a_tree_from_real_navigation() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_document(doc("Enigma machine"));
        app.open_trail(crate::command::TrailScope::Session);
        assert_eq!(app.mode, Mode::Trail);
        assert_eq!(app.trail.graph.nodes.len(), 2);
        assert_eq!(app.trail.tree.roots.len(), 1);
        assert_eq!(app.trail.tree.roots[0].article.title, "Alan Turing");
        assert_eq!(
            app.trail.tree.roots[0].children[0].article.title,
            "Enigma machine"
        );
    }

    /// PRD FR-HS-3's branching case: `A -> B`, back to `A`, `A -> C` puts
    /// both `B` and `C` under the same root `A`.
    #[test]
    fn a_branching_trail_shows_two_children_under_the_same_root() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("A"));
        app.open_document(doc("B")); // A -> B
        // "Back to A", the same install-without-touching-stacks shape
        // `main::open_history_entry` uses (the stacks were already adjusted
        // by `navigate_back_target`).
        let entry = app.navigate_back_target().expect("A is on the back stack");
        assert_eq!(entry.title, "A");
        app.set_document(doc("A"));
        app.open_document(doc("C")); // A -> C, a second branch off A

        app.open_trail(crate::command::TrailScope::Session);
        assert_eq!(app.trail.tree.roots.len(), 1);
        let root = &app.trail.tree.roots[0];
        assert_eq!(root.article.title, "A");
        let mut child_titles: Vec<&str> = root
            .children
            .iter()
            .map(|c| c.article.title.as_str())
            .collect();
        child_titles.sort_unstable();
        assert_eq!(child_titles, vec!["B", "C"]);
    }

    #[test]
    fn open_trail_with_no_history_says_no_trail_yet() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_trail(crate::command::TrailScope::Session);
        assert!(app.trail.graph.nodes.is_empty());
        assert!(app.status.contains("No trail yet"), "{:?}", app.status);
    }

    #[test]
    fn open_and_close_trail_round_trips_the_prior_mode() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.mode = Mode::Reading;
        app.open_trail(crate::command::TrailScope::Session);
        assert_eq!(app.mode, Mode::Trail);
        app.close_trail();
        assert_eq!(app.mode, Mode::Reading);
    }

    #[test]
    fn cycle_trail_wraps_in_both_directions() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("A"));
        app.open_document(doc("B"));
        app.open_trail(crate::command::TrailScope::Session);
        assert_eq!(app.trail_selected, 0);
        app.cycle_trail(true);
        assert_eq!(app.trail_selected, 1);
        app.cycle_trail(true);
        assert_eq!(app.trail_selected, 0, "wraps forward past the end");
        app.cycle_trail(false);
        assert_eq!(app.trail_selected, 1, "wraps backward past the start");
    }

    #[test]
    fn cycle_trail_on_an_empty_trail_is_a_harmless_no_op() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_trail(crate::command::TrailScope::Session);
        app.cycle_trail(true);
        assert_eq!(app.trail_selected, 0);
    }

    // ---- Trail DAG view (PRD FR-HS-3 v2) -----------------------------------

    /// Builds the module doc's own merge scenario purely through the public
    /// navigation API (`open_document`/`navigate_back_target`/
    /// `set_document`), the same technique
    /// `a_branching_trail_shows_two_children_under_the_same_root` already
    /// uses for a simpler shape: A -> B, back to A, A -> C (so both branches
    /// exist), then B -> D and, after returning to C, C -> D again — D ends
    /// up reached from both B and C.
    fn open_a_merge_trail(app: &mut App) {
        app.open_document(doc("A"));
        app.open_document(doc("B")); // A -> B
        app.open_document(doc("D")); // B -> D (first visit, referrer B)
        let back_to_b = app.navigate_back_target().expect("B is on the back stack");
        assert_eq!(back_to_b.title, "B");
        app.set_document(doc("B"));
        let back_to_a = app.navigate_back_target().expect("A is on the back stack");
        assert_eq!(back_to_a.title, "A");
        app.set_document(doc("A"));
        app.open_document(doc("C")); // A -> C, the second branch off A
        app.open_document(doc("D")); // C -> D (second visit, referrer C)
    }

    #[test]
    fn open_trail_dag_sets_the_dag_layout_and_open_trail_resets_it_to_tree() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        open_a_merge_trail(&mut app);
        app.open_trail_dag(crate::command::TrailScope::Session);
        assert_eq!(app.mode, Mode::Trail);
        assert_eq!(app.trail_layout, crate::trail::TrailLayout::Dag);
        app.open_trail(crate::command::TrailScope::Session);
        assert_eq!(
            app.trail_layout,
            crate::trail::TrailLayout::Tree,
            "plain :trail must always land back on the tree layout"
        );
    }

    /// The scenario the DAG view exists for: D was reached from both B and
    /// C, so `:trail dag` must show D with BOTH as parents — distinct from
    /// the tree view (already covered by
    /// `a_multi_parent_node_is_placed_under_its_first_referrer_with_an_
    /// also_from_note` at the `trail` module level), which keeps only one.
    #[test]
    fn trail_dag_shows_a_merge_nodes_full_parent_set() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        open_a_merge_trail(&mut app);
        app.open_trail_dag(crate::command::TrailScope::Session);
        let dag = crate::trail::dag_from_graph(&app.trail.graph);
        let d = dag
            .nodes
            .iter()
            .find(|n| n.article.title == "D")
            .expect("D must be a DAG node");
        let mut parent_titles: Vec<&str> = d.parents.iter().map(|p| p.title.as_str()).collect();
        parent_titles.sort_unstable();
        assert_eq!(parent_titles, vec!["B", "C"], "D must carry both parents");

        // The tree, over the very same underlying graph, keeps only one.
        let tree_b = app
            .trail
            .tree
            .roots
            .iter()
            .find(|r| r.article.title == "A")
            .unwrap()
            .children
            .iter()
            .find(|c| c.article.title == "B")
            .unwrap();
        assert_eq!(tree_b.children.len(), 1, "D is the tree's child of B only");
        assert_eq!(tree_b.children[0].also_from, vec![key_of(&app, "C")]);
    }

    /// `cycle_trail`/`selected_trail_target` must read the DAG's own
    /// topologically ordered node list while `trail_layout == Dag`, not the
    /// tree's flattened line list.
    #[test]
    fn cycle_trail_and_selected_target_use_the_dag_list_when_dag_layout_is_active() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        open_a_merge_trail(&mut app);
        app.open_trail_dag(crate::command::TrailScope::Session);
        let dag = crate::trail::dag_from_graph(&app.trail.graph);
        assert_eq!(app.trail_selected, 0);
        for expected in dag.nodes.iter().skip(1) {
            app.cycle_trail(true);
            let (_, _, title) = app.selected_trail_target().unwrap();
            assert_eq!(title, expected.article.title);
        }
    }

    /// Small helper: the `ArticleKey` `open_a_merge_trail`'s articles use,
    /// for asserting against `also_from`/`parents` lists without repeating
    /// the wiki/lang boilerplate at each call site.
    fn key_of(app: &App, title: &str) -> crate::trail::ArticleKey {
        crate::trail::ArticleKey {
            wiki: String::new(),
            lang: app.active_tab().lang.clone(),
            title: title.to_string(),
        }
    }

    /// The session cutoff is a real `>=` filter, not decoration: forcing it
    /// strictly past an already-recorded visit's timestamp excludes that
    /// visit from `Session` scope while `All` still shows it. This is the
    /// deterministic way to exercise the boundary without reaching into
    /// `history.rs`'s private timestamp storage (its own tests backdate rows
    /// via raw SQL because they're *in* that module; `App`'s public surface
    /// has no such hook, by design).
    #[test]
    fn open_trail_session_scope_is_a_real_cutoff_on_session_started_at() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.session_started_at = crate::history::now_unix() + 10_000;
        app.open_trail(crate::command::TrailScope::Session);
        assert!(
            app.trail.graph.nodes.is_empty(),
            "a visit recorded before the (forced) session cutoff is excluded"
        );
        app.open_trail(crate::command::TrailScope::All);
        assert_eq!(
            app.trail.graph.nodes.len(),
            1,
            "All scope ignores the session cutoff entirely"
        );
    }

    #[test]
    fn open_trail_days_scope_includes_a_visit_within_the_window() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_trail(crate::command::TrailScope::Days(7));
        assert_eq!(app.trail.graph.nodes.len(), 1);
    }

    /// The trail node's wiki round-trips from `history::Visit::wiki` through
    /// `trail::build` to `selected_trail_target` — the exact value
    /// `main::open_trail_node`'s Enter arm reopens.
    #[test]
    fn selected_trail_target_carries_a_non_default_wiki() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.history
            .record_visit("wiktionary", "en", "Mercury", None);
        app.open_trail(crate::command::TrailScope::Session);
        app.trail_selected = 0;
        assert_eq!(
            app.selected_trail_target(),
            Some((
                "wiktionary".to_string(),
                "en".to_string(),
                "Mercury".to_string()
            ))
        );
    }

    #[test]
    fn selected_trail_target_is_none_with_nothing_shown() {
        let app = App::new("en".to_string(), Theme::terminal(), false);
        assert_eq!(app.selected_trail_target(), None);
    }

    #[test]
    fn export_trail_writes_the_file_and_requires_a_second_run_to_overwrite() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.open_document(doc("Enigma machine"));

        let dir =
            std::env::temp_dir().join(format!("wikitui-trail-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("trail.md");

        app.export_trail("md", Some(&target));
        let first = std::fs::read_to_string(&target).unwrap();
        assert!(first.contains("Enigma machine"));
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("Exported the trail")
        );

        // A hand-edit shouldn't silently be clobbered without a second
        // confirming run — same two-press pattern as `export_bookmarks_to`.
        std::fs::write(&target, "hand-annotated").unwrap();
        app.export_trail("md", Some(&target));
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("run the export again to overwrite"),
            "{:?}",
            app.notice
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hand-annotated");

        app.export_trail("md", Some(&target));
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains("Enigma machine")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_trail_with_nothing_recorded_reports_instead_of_writing() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.export_trail("md", None);
        assert!(app.notice.as_deref().unwrap().contains("Nothing to export"));
    }

    #[test]
    fn export_trail_rejects_an_unknown_format() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.export_trail("carrier-pigeon", None);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("unknown export format")
        );
    }

    /// `App::export_trail`'s documented independence from whatever scope a
    /// currently-open `:trail` view happens to show (see its doc comment):
    /// forcing the session cutoff past an already-recorded visit and then
    /// opening the *wider* `All` view (which does show it) must not leak
    /// into the export, which always rebuilds session-scope fresh.
    #[test]
    fn export_trail_always_uses_session_scope_regardless_of_the_open_views_scope() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.session_started_at = crate::history::now_unix() + 10_000;
        app.open_trail(crate::command::TrailScope::All);
        assert_eq!(
            app.trail.graph.nodes.len(),
            1,
            "the wider view does show it"
        );

        let dir = std::env::temp_dir().join(format!(
            "wikitui-trail-export-scope-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("trail.md");
        app.export_trail("md", Some(&target));
        assert!(
            app.notice.as_deref().unwrap().contains("Nothing to export"),
            "export rebuilds session scope fresh (empty here), ignoring the open All view: {:?}",
            app.notice
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Saved pages, offline card, bulk cost preview (PRD FR-OFF-4..7) ---

    /// PRD FR-OFF-6's ▣ Saved glyph must be distinct from ◐ cached and ○
    /// offline, so a pinned page reads differently from a cache hit.
    #[test]
    fn saved_page_source_glyph_is_distinct_from_cached_and_offline() {
        let saved = PageSource::Saved { age_secs: 120 }.prefix();
        assert!(saved.starts_with('▣'), "saved uses ▣: {saved:?}");
        assert!(saved.contains("saved"));
        let cached = PageSource::Cached { age_secs: 120 }.prefix();
        let offline = PageSource::Offline { age_secs: 120 }.prefix();
        assert!(cached.starts_with('◐'));
        assert!(offline.starts_with('○'));
        assert_ne!(saved, cached);
        assert_ne!(saved, offline);
    }

    /// PRD FR-OFF-8's ◈ Zim glyph must likewise be distinct from every
    /// other source indicator.
    #[test]
    fn zim_page_source_glyph_is_distinct_from_every_other_source() {
        let zim = PageSource::Zim.prefix();
        assert!(zim.starts_with('◈'), "zim uses ◈: {zim:?}");
        for other in [
            PageSource::Live.prefix(),
            PageSource::Cached { age_secs: 1 }.prefix(),
            PageSource::Offline { age_secs: 1 }.prefix(),
            PageSource::Saved { age_secs: 1 }.prefix(),
        ] {
            assert_ne!(zim, other);
        }
    }

    /// PRD FR-OFF-5's cost preview: the estimate is count × the tier's
    /// per-article figure (FR-OFF-9), and the wording matches §7's row.
    #[test]
    fn bulk_cost_preview_estimates_and_matches_prd_wording() {
        // 412 T0 articles at ~30 KB each ≈ 12.1 MB.
        let preview = bulk_cost_preview("Category:Physics", 412, Tier::T0);
        assert!(preview.starts_with("Category:Physics → 412 articles, est. "));
        assert!(preview.contains("MB"));
        assert!(preview.ends_with("Proceed? (y/n)"));
        // T1's per-article estimate is larger than T0's for the same count.
        let t0 = bulk_cost_preview("x", 100, Tier::T0);
        let t1 = bulk_cost_preview("x", 100, Tier::T1);
        assert_ne!(t0, t1);
    }

    #[test]
    fn offline_card_show_and_dismiss_routing() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.show_offline_card("en".to_string(), "Nonexistent".to_string());
        assert_eq!(app.mode, Mode::OfflineCard);
        assert_eq!(
            app.offline_card_target,
            Some(("en".to_string(), "Nonexistent".to_string()))
        );
        app.close_offline_card();
        assert_eq!(app.mode, Mode::Reading);
        assert!(app.offline_card_target.is_none());
    }

    #[test]
    fn offline_card_f_enqueues_the_target_and_dedups() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.fetch_queue = crate::fetch_queue::FetchQueue::in_memory();
        app.show_offline_card("en".to_string(), "Deep Learning".to_string());
        assert!(app.queue_offline_target(), "first queue is new");
        assert!(app.fetch_queue.contains("en", "Deep Learning"));
        assert_eq!(app.mode, Mode::Reading, "card dismisses after queueing");

        // Queuing the same target again is rejected as a duplicate.
        app.show_offline_card("en".to_string(), "Deep Learning".to_string());
        assert!(!app.queue_offline_target(), "duplicate rejected");
    }

    /// PRD FR-DL-5 / §7's "Redlink followed" card — same show/dismiss shape
    /// as the offline card.
    #[test]
    fn redlink_card_show_and_dismiss_routing() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.show_redlink_card("en".to_string(), "Nonexistent Concept X".to_string());
        assert_eq!(app.mode, Mode::RedlinkCard);
        assert_eq!(
            app.redlink_card_target,
            Some(("en".to_string(), "Nonexistent Concept X".to_string()))
        );
        app.close_redlink_card();
        assert_eq!(app.mode, Mode::Reading);
        assert!(app.redlink_card_target.is_none());
    }

    /// The redlink card's `y`: the wiki's own create-page URL for whatever
    /// target the card is currently showing, `None` when no card is up.
    #[test]
    fn redlink_create_url_is_the_wikis_edit_action_url() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert_eq!(app.redlink_create_url(), None, "no card showing yet");
        app.show_redlink_card("de".to_string(), "New Concept".to_string());
        assert_eq!(
            app.redlink_create_url().as_deref(),
            Some("https://de.wikipedia.org/w/index.php?title=New_Concept&action=edit")
        );
    }

    /// PRD FR-DL-5: `is_redlink` combines both signals — a `LinkRef` Parsoid
    /// itself pre-marked (`class="new"`), and a title the batched info check
    /// separately confirmed via `confirmed_redlinks` — either one is
    /// sufficient, and an ordinary link matches neither.
    #[test]
    fn is_redlink_checks_both_the_parsoid_flag_and_the_confirmed_set() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.active_tab_mut().links = vec![
            LinkRef {
                href: "./Nonexistent_Concept_X".into(),
                text: "concept".into(),
                internal_title: Some("Nonexistent Concept X".into()),
                redlink: true,
            },
            LinkRef {
                href: "./Computer_science".into(),
                text: "computer science".into(),
                internal_title: Some("Computer science".into()),
                redlink: false,
            },
        ];
        assert!(app.is_redlink("Nonexistent Concept X"));
        assert!(!app.is_redlink("Computer science"));
        assert!(
            !app.is_redlink("Uncharted Topic Y"),
            "not yet confirmed by anything"
        );

        app.confirmed_redlinks.insert((
            String::new(),
            "en".to_string(),
            "Uncharted Topic Y".to_string(),
        ));
        assert!(
            app.is_redlink("Uncharted Topic Y"),
            "the batched-check signal alone must be sufficient"
        );
    }

    // -- Quality badges (PRD FR-DL-3) ---------------------------------------

    #[test]
    fn quality_badge_for_reads_the_session_cache_by_lang_and_title() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert_eq!(
            app.quality_badge_for("", "Alan Turing"),
            None,
            "nothing cached yet"
        );
        app.quality_cache.insert(
            (String::new(), "en".to_string(), "Alan Turing".to_string()),
            crate::api::QualityClass::Fa,
        );
        assert_eq!(app.quality_badge_for("", "Alan Turing"), Some("★FA"));
        assert_eq!(
            app.quality_badge_for("", "Some Other Article"),
            None,
            "a different title must not pick up an unrelated cache entry"
        );
    }

    /// `current_quality_badge` reads the *active tab's* document title —
    /// proving it's wired to whatever article is actually on screen, not a
    /// hardcoded lookup.
    #[test]
    fn current_quality_badge_reflects_the_active_tabs_document() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert_eq!(app.current_quality_badge(), None, "no document open yet");
        app.set_document(Document {
            title: "Enigma machine".to_string(),
            blocks: Vec::new(),
            citations: Vec::new(),
            truncated: false,
            degraded_parse: false,
            is_disambiguation: false,
        });
        assert_eq!(
            app.current_quality_badge(),
            None,
            "open, but nothing cached for it yet"
        );
        app.quality_cache.insert(
            (
                String::new(),
                "en".to_string(),
                "Enigma machine".to_string(),
            ),
            crate::api::QualityClass::Ga,
        );
        assert_eq!(app.current_quality_badge(), Some("+GA"));
    }

    /// PRD FR-ML-4: a tab remembers the wiki it was opened on. Switching the
    /// app-global active wiki afterward must not retroactively rebrand an
    /// already-open tab — its session-cache key still targets its own wiki.
    #[test]
    fn a_tab_keeps_its_wiki_when_the_active_wiki_switches() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        // Open an article while the active wiki is a sister project.
        app.active_wiki_name = "wiktionary".to_string();
        app.open_document(doc("Mercury"));
        assert_eq!(app.active_tab().wiki, "wiktionary");
        assert_eq!(
            app.current_article_key(),
            Some((
                "wiktionary".to_string(),
                "en".to_string(),
                "Mercury".to_string()
            ))
        );

        // A `:wiki` switch changes only what NEW opens address; this tab keeps
        // serving — and keying — its own wiki.
        app.active_wiki_name = "wikipedia".to_string();
        assert_eq!(app.active_tab().wiki, "wiktionary");
        assert_eq!(
            app.current_article_key(),
            Some((
                "wiktionary".to_string(),
                "en".to_string(),
                "Mercury".to_string()
            )),
            "the tab's cache lookup still targets the wiki it was opened on"
        );
    }

    /// PRD FR-ML-4: the same `(lang, title)` cached for two different wikis in
    /// a session map never collides — langlinks delivered for Wikipedia's
    /// "Mercury" and a sister's "Mercury" are kept independently, and the
    /// picker reads only the active tab's wiki's slice.
    #[test]
    fn session_langlinks_are_isolated_per_wiki() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.deliver_langlinks(
            String::new(),
            "en".to_string(),
            "Mercury".to_string(),
            Ok(vec![langlink("de", "Merkur", "German", "Merkur (Planet)")]),
        );
        app.deliver_langlinks(
            "wiktionary".to_string(),
            "en".to_string(),
            "Mercury".to_string(),
            Ok(vec![langlink("fr", "mercure", "French", "mercure")]),
        );

        // The active tab is on the default wiki, so its picker sees only the
        // Wikipedia langlinks, never the wiktionary ones.
        app.open_document(doc("Mercury"));
        assert_eq!(app.active_tab().wiki, "");
        let codes: Vec<&str> = app.lang_links().iter().map(|l| l.code.as_str()).collect();
        assert_eq!(codes, vec!["de"], "only this wiki's langlinks are visible");
    }

    #[test]
    fn saved_picker_cycles_and_deletes() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.saved = crate::saved::SavedPages::in_memory();
        app.saved
            .save("", "en", "A", 1, Tier::T0, "<p>a</p>", &[], vec![], "")
            .unwrap();
        app.saved
            .save("", "en", "B", 1, Tier::T0, "<p>b</p>", &[], vec![], "")
            .unwrap();
        app.open_saved_picker();
        assert_eq!(app.mode, Mode::SavedPicker);
        assert_eq!(
            app.selected_saved_target(),
            Some(("".into(), "en".into(), "A".into()))
        );
        app.cycle_saved(true);
        assert_eq!(
            app.selected_saved_target(),
            Some(("".into(), "en".into(), "B".into()))
        );
        app.cycle_saved(true);
        assert_eq!(
            app.selected_saved_target(),
            Some(("".into(), "en".into(), "A".into())),
            "wraps"
        );

        app.delete_selected_saved();
        assert_eq!(app.saved.list().len(), 1);
        // Selection stays in range over what remains.
        assert!(app.selected_saved_target().is_some());
    }

    // ---- Command palette & context (PRD FR-CS-1) ---------------------------

    /// `Ctrl-p` opens the palette, capturing the mode it was opened from so
    /// Esc restores it and the offered commands are scoped to that context.
    #[test]
    fn open_palette_captures_the_prior_mode_and_context() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(doc("Alan Turing"));
        app.mode = Mode::Reading;
        app.open_palette();
        assert_eq!(app.mode, Mode::Palette);
        assert_eq!(app.palette_prior_mode, Mode::Reading);
        assert_eq!(
            app.key_context(app.palette_prior_mode),
            registry::KeyContext::Reading
        );
    }

    /// The start page (Reading with no document) is its own key context.
    #[test]
    fn start_page_is_the_start_page_context() {
        let app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(app.active_tab().doc.is_none());
        assert_eq!(
            app.key_context(Mode::Reading),
            registry::KeyContext::StartPage
        );
    }

    /// The palette fuzzy-filters over context-applicable commands and runs the
    /// highlighted one; the selection clamps to the match count.
    #[test]
    fn palette_filters_selects_and_clamps() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(doc("Alan Turing"));
        app.open_palette();

        // Empty query lists every reading/global palette command.
        let all = app.palette_rows();
        assert!(all.iter().any(|r| r.action == registry::Action::Toc));
        assert!(
            all.iter()
                .any(|r| r.action == registry::Action::RandomArticle)
        );

        // "toc" narrows to the table-of-contents command, ranked first.
        app.palette_input = "toc".to_string();
        app.palette_selected = 0;
        let rows = app.palette_rows();
        assert_eq!(rows[0].action, registry::Action::Toc);
        assert_eq!(app.palette_selection(), Some(registry::Action::Toc));

        // Moving down past the end clamps to the last row, not out of bounds.
        for _ in 0..50 {
            app.palette_move(1);
        }
        assert!(app.palette_selected < rows.len().max(1));
        assert!(app.palette_selection().is_some());
    }

    /// PRD FR-CS-4: the help view length is generated per view, and the
    /// reading cheatsheet is long enough that a fixed popup would clip — which
    /// is exactly why the overlay scrolls. (Model assertion, not pixels.)
    #[test]
    fn reading_help_is_long_enough_to_require_scrolling() {
        let rows = registry::reading_help(&registry::Keymap::vim());
        assert!(
            rows.len() > 30,
            "the reading cheatsheet has {} rows — it must be scrollable",
            rows.len()
        );
    }

    // -- Session auto-restore (PRD FR-TB-5) ---------------------------------

    fn session_temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "wikitui-session-test-{tag}-{}.json",
            std::process::id()
        ))
    }

    #[test]
    fn session_snapshot_captures_every_tabs_lang_title_scroll_folds_and_stacks() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "Alan Turing",
            "<html><body><h2>A</h2><p>x</p><h2>B</h2><p>y</p></body></html>",
        ));
        app.active_tab_mut().scroll = 7;
        app.active_tab_mut().folded_blocks = [1usize, 3usize].into_iter().collect();
        app.active_tab_mut()
            .back_stack
            .push(crate::tab::HistoryEntry {
                wiki: String::new(),
                lang: "en".to_string(),
                title: "Earlier".to_string(),
                scroll: 2,
            });
        app.new_foreground_tab(); // tab 1 stays blank.

        let snap = app.session_snapshot();
        assert_eq!(snap.active, 1);
        assert_eq!(snap.tabs.len(), 2);
        assert_eq!(snap.tabs[0].lang, "en");
        assert_eq!(snap.tabs[0].title.as_deref(), Some("Alan Turing"));
        assert_eq!(snap.tabs[0].scroll, 7);
        assert_eq!(snap.tabs[0].folded_blocks, vec![1, 3]);
        assert_eq!(snap.tabs[0].back_stack.len(), 1);
        assert_eq!(snap.tabs[0].back_stack[0].title, "Earlier");
        assert_eq!(
            snap.tabs[1].title, None,
            "a blank tab persists with no title"
        );
    }

    #[test]
    fn persist_session_writes_a_file_that_session_load_reads_back() {
        let path = session_temp_path("roundtrip");
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.session_path = Some(path.clone());
        app.set_document(crate::doc::parse_article_html(
            "Alan Turing",
            "<html><body><p>x</p></body></html>",
        ));
        app.active_tab_mut().scroll = 5;
        app.persist_session();

        let loaded = crate::session::load(&path).expect("persist_session must have written it");
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].title.as_deref(), Some("Alan Turing"));
        assert_eq!(loaded.tabs[0].scroll, 5);
        let _ = std::fs::remove_file(&path);
    }

    /// PRD FR-PR-3: the one privacy gate every session write routes through.
    #[test]
    fn persist_session_never_writes_while_incognito() {
        let path = session_temp_path("incognito");
        let _ = std::fs::remove_file(&path); // guard against a leftover from a prior failed run
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.session_path = Some(path.clone());
        app.incognito = true;
        app.set_document(crate::doc::parse_article_html(
            "Alan Turing",
            "<html><body><p>x</p></body></html>",
        ));
        assert!(!path.exists(), "incognito must never write a session file");
    }

    /// PRD FR-TB-5: `reset_to_single_blank_tab` (the runtime `:session
    /// <name>` switch's "discard whatever was open" step) leaves exactly
    /// one blank tab regardless of how many were open, and the survivor is
    /// actually blank (not just the *last* tab's stale content wearing tab
    /// index 0).
    #[test]
    fn reset_to_single_blank_tab_collapses_every_tab_to_one_blank_survivor() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "Alan Turing",
            "<html><body><p>x</p></body></html>",
        ));
        app.active_tab_mut().scroll = 9;
        app.new_foreground_tab();
        app.set_document(crate::doc::parse_article_html(
            "Enigma machine",
            "<html><body><p>y</p></body></html>",
        ));
        app.active_tab_mut().scroll = 3;
        app.new_foreground_tab(); // a third, blank tab.
        assert_eq!(app.tabs.len(), 3);

        app.reset_to_single_blank_tab();

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active, 0);
        assert!(app.tabs[0].doc.is_none(), "the survivor must be blank");
        assert_eq!(app.tabs[0].scroll, 0);
        assert!(app.split.is_none());
    }

    /// PRD FR-TB-5's "crash-safe continuous" guarantee: after *every*
    /// meaningful-change call, the on-disk file already matches the live
    /// state at that moment — proving a crash right after any one of these
    /// steps loses nothing from *before* that step, only (at most) whatever
    /// hadn't yet triggered a save.
    #[test]
    fn a_save_after_each_meaningful_action_reflects_that_actions_own_state() {
        let path = session_temp_path("crash-safety");
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.session_path = Some(path.clone());

        // Action 1: open an article.
        app.set_document(crate::doc::parse_article_html(
            "Alan Turing",
            "<html><body><h2>A</h2><p>x</p></body></html>",
        ));
        let after_open = crate::session::load(&path).unwrap();
        assert_eq!(after_open.tabs[0].title.as_deref(), Some("Alan Turing"));

        // Action 2: fold a section.
        app.toggle_fold_at_cursor();
        let after_fold = crate::session::load(&path).unwrap();
        assert!(
            !after_fold.tabs[0].folded_blocks.is_empty(),
            "the fold must already be on disk"
        );

        // Action 3: open a second tab and navigate it.
        app.new_foreground_tab();
        app.set_document(crate::doc::parse_article_html(
            "Enigma machine",
            "<html><body><p>y</p></body></html>",
        ));
        let after_new_tab = crate::session::load(&path).unwrap();
        assert_eq!(after_new_tab.tabs.len(), 2);
        assert_eq!(after_new_tab.active, 1);
        assert_eq!(
            after_new_tab.tabs[1].title.as_deref(),
            Some("Enigma machine")
        );

        // Action 4: switch back to the first tab.
        app.switch_to_tab(0);
        let after_switch = crate::session::load(&path).unwrap();
        assert_eq!(after_switch.active, 0);

        let _ = std::fs::remove_file(&path);
    }

    // -- Splits & bilingual (PRD FR-TB-4, FR-ML-3) --------------------------

    /// A ready-to-split app: one tab with a short article installed.
    fn split_app() -> App {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "Alan Turing",
            "<html><body><p>alpha</p><p>bravo</p></body></html>",
        ));
        app
    }

    #[test]
    fn vsplit_duplicates_the_active_tab_into_a_second_pane_focus_left() {
        let mut app = split_app();
        assert!(app.split.is_none());
        assert_eq!(app.tabs.len(), 1);

        app.open_split(100)
            .expect("100 cols is wide enough to split");

        let split = app.split.as_ref().expect("a split is now active");
        assert_eq!(app.tabs.len(), 2, "the right pane is a real duplicate tab");
        assert_eq!(split.focused, 0, "focus stays on the original (left) pane");
        assert_eq!(
            app.active_tab().id,
            split.focused_id(),
            "the invariant holds: active tab == focused pane"
        );
        assert!(!split.bilingual, "a plain :vsplit is not a bilingual split");
        assert_eq!(
            app.tabs[0].doc.as_ref().map(|d| d.title.as_str()),
            app.tabs[1].doc.as_ref().map(|d| d.title.as_str()),
            "both panes show the same article"
        );
    }

    #[test]
    fn vsplit_refuses_when_the_terminal_is_too_narrow() {
        let mut app = split_app();
        let err = app.open_split(70).expect_err("70 cols is too narrow");
        assert!(err.contains("too narrow"), "got: {err}");
        assert!(app.split.is_none(), "no broken split is left behind");
        assert_eq!(app.tabs.len(), 1, "no duplicate tab was created");
    }

    #[test]
    fn vsplit_refuses_a_second_split_and_with_no_article() {
        let mut app = split_app();
        app.open_split(100).unwrap();
        assert!(
            app.open_split(100).is_err(),
            "already-split refuses another :vsplit"
        );

        let mut empty = App::new("en".to_string(), Theme::terminal(), false);
        assert!(
            empty.open_split(100).is_err(),
            "an empty tab has nothing to split"
        );
    }

    #[test]
    fn focus_movement_switches_which_pane_receives_keys() {
        let mut app = split_app();
        app.open_split(100).unwrap();
        let left_id = app.tabs[0].id;
        let right_id = app.tabs[1].id;

        assert_eq!(app.active_tab().id, left_id);
        assert!(app.focus_split_other(), "Ctrl-w w moves focus");
        assert_eq!(
            app.active_tab().id,
            right_id,
            "keys now route to the right pane"
        );
        assert_eq!(app.split.as_ref().unwrap().focused, 1);

        assert!(app.focus_split_pane(0), "Ctrl-w h focuses the left pane");
        assert_eq!(app.active_tab().id, left_id);
        assert!(!app.focus_split_pane(0), "already focused — no-op");
    }

    #[test]
    fn scroll_routes_to_the_focused_pane_only_without_scrollbind() {
        let mut app = split_app();
        app.open_split(100).unwrap();
        app.tabs[0].max_scroll = 100;
        app.tabs[1].max_scroll = 100;

        app.scroll_by(5);
        assert_eq!(app.tabs[0].scroll, 5, "the focused (left) pane scrolled");
        assert_eq!(
            app.tabs[1].scroll, 0,
            "the unfocused pane did NOT move — independent scroll"
        );
    }

    #[test]
    fn scrollbind_scrolls_both_panes_in_lockstep() {
        let mut app = split_app();
        app.open_split(100).unwrap();
        app.tabs[0].max_scroll = 100;
        app.tabs[1].max_scroll = 100;
        app.set_scrollbind(true);

        app.scroll_by(7);
        assert_eq!(app.tabs[0].scroll, 7);
        assert_eq!(app.tabs[1].scroll, 7, "scrollbind mirrors the delta");

        app.scroll_by(-3);
        assert_eq!(app.tabs[0].scroll, 4);
        assert_eq!(app.tabs[1].scroll, 4);

        app.set_scrollbind(false);
        app.scroll_by(10);
        assert_eq!(app.tabs[0].scroll, 14);
        assert_eq!(app.tabs[1].scroll, 4, "off again: back to independent");
    }

    #[test]
    fn scrollbind_clamps_each_pane_to_its_own_length() {
        let mut app = split_app();
        app.open_split(100).unwrap();
        app.tabs[0].max_scroll = 100; // a long focused pane
        app.tabs[1].max_scroll = 4; // a short bound pane
        app.set_scrollbind(true);

        app.scroll_by(50);
        assert_eq!(app.tabs[0].scroll, 50, "the long pane scrolls freely");
        assert_eq!(
            app.tabs[1].scroll, 4,
            "the short pane clamps to its own max — best-effort alignment"
        );
    }

    #[test]
    fn only_collapses_the_split_and_drops_the_duplicate_pane() {
        let mut app = split_app();
        app.open_split(100).unwrap();
        assert_eq!(app.tabs.len(), 2);
        let left_id = app.tabs[0].id;

        assert!(app.close_split(), "a split was closed");
        assert!(app.split.is_none());
        assert_eq!(
            app.tabs.len(),
            1,
            "a plain :vsplit's throwaway duplicate is removed on :only"
        );
        assert_eq!(app.active_tab().id, left_id, "the focused article stays");
        assert!(!app.close_split(), "closing again is a no-op");
    }

    #[test]
    fn closing_the_split_from_the_right_pane_keeps_that_pane() {
        let mut app = split_app();
        app.open_split(100).unwrap();
        app.focus_split_other(); // focus the right (duplicate) pane
        let right_id = app.tabs[1].id;

        app.close_split();
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(
            app.active_tab().id,
            right_id,
            "the pane you were focused on is the one that stays"
        );
    }

    #[test]
    fn switching_tabs_collapses_an_active_split() {
        let mut app = split_app();
        app.open_split(100).unwrap();
        app.next_tab();
        assert!(
            app.split.is_none(),
            "gt/gT leave the split — you asked to look at a tab full-width"
        );
    }

    #[test]
    fn bilingual_target_prefers_a_configured_language_then_first_langlink() {
        let mut app = split_app();
        let key = app.current_article_key().unwrap();
        app.langlinks_cache.insert(
            key,
            vec![
                crate::api::LangLink {
                    code: "fr".to_string(),
                    autonym: "français".to_string(),
                    langname: "French".to_string(),
                    title: "Alan Turing".to_string(),
                    url: None,
                },
                crate::api::LangLink {
                    code: "ja".to_string(),
                    autonym: "日本語".to_string(),
                    langname: "Japanese".to_string(),
                    title: "アラン・チューリング".to_string(),
                    url: None,
                },
            ],
        );

        // With ja preferred, it wins over the first-listed fr.
        app.languages = vec!["ja".to_string()];
        assert_eq!(
            app.bilingual_target(),
            Some(("ja".to_string(), "アラン・チューリング".to_string()))
        );

        // With no relevant preference, the first langlink is taken.
        app.languages = vec![];
        assert_eq!(
            app.bilingual_target(),
            Some(("fr".to_string(), "Alan Turing".to_string()))
        );
    }

    #[test]
    fn begin_bilingual_split_marks_it_and_sets_the_not_translation_notice() {
        let mut app = split_app();
        let original_id = app.active_tab().id;
        // A second tab standing in for the fetched other-language article.
        app.new_foreground_tab();
        app.tabs.last_mut().unwrap().lang = "ja".to_string();
        app.set_document(crate::doc::parse_article_html(
            "アラン・チューリング",
            "<html><body><p>ja</p></body></html>",
        ));
        let other_id = app.active_tab().id;

        app.begin_bilingual_split(original_id, other_id);

        let split = app.split.as_ref().expect("a bilingual split is active");
        assert!(split.bilingual);
        assert_eq!(split.sync, crate::split::SyncMode::Section);
        assert_eq!(
            app.active_tab().id,
            original_id,
            "focus is the left/original pane"
        );
        let notice = app.notice.as_deref().unwrap_or_default();
        assert!(
            notice.contains("not a translation"),
            "FR-ML-3 UX copy must set the not-a-translation expectation: {notice:?}"
        );
    }

    #[test]
    fn layout_for_tab_fits_every_line_within_the_pane_width() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(crate::doc::parse_article_html(
            "Wide",
            "<html><body><p>The quick brown fox jumps over the lazy dog again and again \
             and again to force wrapping at a narrow pane width.</p></body></html>",
        ));
        let pane_width = 45u16;
        let layout = app
            .layout_for_tab(0, pane_width)
            .expect("the tab has a document");
        assert_eq!(layout.width, pane_width);
        for line in &layout.lines {
            let w: usize = line
                .spans
                .iter()
                .map(|s| crate::layout::display_width(&s.text, false))
                .sum();
            assert!(
                w <= pane_width as usize,
                "a laid-out line ({w} cells) overflowed the pane width {pane_width}"
            );
        }
    }

    // ---- FR-DL-4: `:set show-cn` toggle + `]c`/`[c` jump -------------------

    fn citation_needed_doc() -> Document {
        let html = "<html><body><p>First claim<sup typeof=\"mw:Transclusion\" \
             data-mw='{&quot;parts&quot;:[{&quot;template&quot;:{&quot;target&quot;:\
             {&quot;wt&quot;:&quot;Citation needed&quot;}}}]}'>x</sup>.</p>\
             <p>Second claim<sup typeof=\"mw:Transclusion\" \
             data-mw='{&quot;parts&quot;:[{&quot;template&quot;:{&quot;target&quot;:\
             {&quot;wt&quot;:&quot;Fact&quot;}}}]}'>x</sup>.</p></body></html>";
        crate::doc::parse_article_html("Cited", html)
    }

    #[test]
    fn set_show_cn_flips_the_flag_and_leaves_a_notice() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(!app.show_cn, "default is off");
        app.set_show_cn(true);
        assert!(app.show_cn);
        assert_eq!(app.notice.as_deref(), Some("show-cn=on"));
        app.set_show_cn(false);
        assert!(!app.show_cn);
        assert_eq!(app.notice.as_deref(), Some("show-cn=off"));
    }

    #[test]
    fn jump_citation_needed_with_no_markers_notices_instead_of_moving_scroll() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(doc("Plain"));
        app.layout_width = 80;
        app.viewport_height = 24;
        app.jump_citation_needed(true);
        assert_eq!(
            app.notice.as_deref(),
            Some("No citation-needed markers on this page")
        );
    }

    #[test]
    fn jump_citation_needed_moves_scroll_to_the_next_and_previous_marker() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.set_document(citation_needed_doc());
        app.layout_width = 80;
        app.viewport_height = 24;
        app.active_tab_mut().scroll = 0;
        app.jump_citation_needed(true);
        let first_hit = app.active_tab().scroll;
        // A second forward jump from further down must land on a later (or
        // equal, if both markers are within one centered viewport) line,
        // never go backwards — the whole point of `]c`.
        app.jump_citation_needed(true);
        let second_hit = app.active_tab().scroll;
        assert!(
            second_hit >= first_hit,
            "forward jumps must not move backwards: {first_hit} then {second_hit}"
        );
        // Wrapping: pushing scroll past the last marker and jumping forward
        // again wraps to the first one again rather than doing nothing.
        app.active_tab_mut().scroll = app.active_tab().max_scroll;
        app.jump_citation_needed(true);
        assert_eq!(
            app.active_tab().scroll,
            first_hit,
            "forward from the very end must wrap to the first marker"
        );
    }

    // ---- FR-DL-6: the wiki-walk game's App-level plumbing -------------------

    #[test]
    fn record_game_move_advances_the_active_game_and_detects_a_win() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.game = Some(crate::game::GameState::new(
            "Alan Turing",
            "Computer science",
            0,
        ));
        app.open_document(doc("Enigma machine"));
        app.record_game_move();
        {
            let game = app.game.as_ref().unwrap();
            assert_eq!(game.clicks(), 1);
            assert!(!game.won);
        }
        app.open_document(doc("Computer science"));
        app.record_game_move();
        let game = app.game.as_ref().unwrap();
        assert_eq!(game.clicks(), 2);
        assert!(game.won);
        assert!(
            app.notice
                .as_deref()
                .unwrap_or_default()
                .contains("You won!"),
            "a win must post a result notice: {:?}",
            app.notice
        );
    }

    #[test]
    fn record_game_move_with_no_active_game_is_a_no_op() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.open_document(doc("Alan Turing"));
        app.record_game_move(); // must not panic with `app.game == None`
        assert!(app.game.is_none());
    }

    // ---- FR-DL-8: achievement toasts from trail stats ----------------------

    #[test]
    fn crossing_five_articles_toasts_curious_exactly_once() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        for title in ["A", "B", "C", "D", "E"] {
            app.open_document(doc(title));
        }
        assert!(
            app.notice
                .as_deref()
                .unwrap_or_default()
                .contains("Curious: 5 articles in one session"),
            "the 5th distinct article must toast the threshold: {:?}",
            app.notice
        );
        app.notice = None;
        // A 6th article does not re-cross the same threshold — no new toast.
        app.open_document(doc("F"));
        assert_eq!(
            app.notice, None,
            "an already-shown achievement must not toast again"
        );
    }

    #[test]
    fn crossing_fifteen_articles_toasts_rabbit_hole() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        for i in 0..15 {
            app.open_document(doc(&format!("Article {i}")));
        }
        assert!(
            app.notice
                .as_deref()
                .unwrap_or_default()
                .contains("Rabbit Hole: 15 articles in one session"),
            "the 15th distinct article must toast rabbit-hole-15: {:?}",
            app.notice
        );
    }

    #[test]
    fn pro_true_suppresses_achievement_toasts_entirely() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.pro = true;
        for title in ["A", "B", "C", "D", "E"] {
            app.open_document(doc(title));
        }
        assert!(
            app.achievements_shown.is_empty(),
            "pro=true must skip achievement bookkeeping entirely"
        );
        assert!(
            !app.notice
                .as_deref()
                .unwrap_or_default()
                .contains("Achievement"),
            "pro=true must never toast: {:?}",
            app.notice
        );
    }
}
