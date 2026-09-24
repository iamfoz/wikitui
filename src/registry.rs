//! The command registry and keymap layer (PRD §5.13, FR-CS-1/3/4).
//!
//! Architectural rule (PRD §5.13 intro): *every feature is a named command
//! first, keybinding second.* This module is that substrate — the single
//! source of truth the command palette (FR-CS-1), the configurable keymap
//! (FR-CS-3), and the context-sensitive help overlay (FR-CS-4) all read from,
//! so none of them can drift out of sync with the others.
//!
//! Two halves:
//!  - [`Action`] + [`COMMANDS`]: the enumeration of every user action as a
//!    named command carrying a stable name, a display name, a one-line help
//!    string, and the [`KeyContext`]s it applies in.
//!  - [`Keymap`]: `(context, chord) -> action`. The built-in `vim` table
//!    ([`Keymap::vim`]) reproduces the keybindings `main::handle_key`'s
//!    hardcoded arms already implement, *exactly* — it is the authoritative
//!    mirror the palette and help render, and the regression tests assert it
//!    against the known bindings. A second built-in `emacs` preset
//!    ([`Keymap::emacs`]) layers Control-chord scroll/search equivalents on
//!    top. A user `keymap.toml` ([`Keymap::apply_user_toml`]) adds or
//!    overrides bindings; parse errors warn and are skipped, never crash.
//!
//! Runtime dispatch (see `main::dispatch_action`) is driven from the keymap's
//! *override* layer only: the vim default has no overrides, so
//! `handle_key`'s existing arms remain the code path for every default
//! binding and their behavior is untouched. Selecting the emacs preset or a
//! user `keymap.toml` populates the override layer, which `handle_key`
//! consults ahead of its hardcoded arms — that is what makes a rebinding
//! take effect without re-implementing dispatch. Multi-key chords beyond the
//! established `g`/`b`/`r` prefixes remain the keymap's documented job; the
//! prefixes themselves stay in `handle_key`'s latch (their final actions are
//! registry actions all the same, reachable from the palette).

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyModifiers};

/// Every user action wikitui can perform, named once here so the palette,
/// help, and keymap all refer to the same identity. Argument-carrying
/// operations (`:open <title>`, `:theme <name>`) stay in [`crate::command`]'s
/// parser — this enum is the argument-free action vocabulary that a keybind
/// or a palette row can trigger directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // -- reading: scrolling --
    ScrollDown,
    ScrollUp,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    ScrollTop,
    ScrollBottom,
    ScrollTablesLeft,
    ScrollTablesRight,
    // -- reading: links & navigation --
    LinkCycleNext,
    LinkCyclePrev,
    LinkHints,
    LinkHintsBackground,
    FollowLink,
    OpenBackgroundTab,
    Back,
    Forward,
    BackStackPicker,
    ReadingHistory,
    // -- tabs --
    NextTab,
    PrevTab,
    TabPicker,
    ReopenClosedTab,
    CloseTab,
    // -- views & tools --
    Toc,
    CycleTheme,
    /// PRD FR-ACC-5: flip the active tab between an article and its talk
    /// page.
    TalkToggle,
    Search,
    CommandLine,
    Help,
    Palette,
    YankUrl,
    YankMarkdown,
    ReadLater,
    Research,
    Library,
    BookmarkToggle,
    BookmarkPicker,
    Annotate,
    FindInPage,
    FindNext,
    FindPrev,
    SaveOffline,
    RandomArticle,
    RelatedPanel,
    LangPicker,
    /// PRD FR-ML-4: bare `:wiki`'s picker — Wikipedia + the four sister
    /// projects. No default keybinding (same as `LangPicker`, which also
    /// has none); reachable via `:wiki` or the command palette.
    WikiPicker,
    Home,
    Today,
    /// PRD §10 / Appendix B: open the `:info` article-attribution overlay
    /// (title, canonical URL, revid, license, history permalink).
    Info,
    /// PRD FR-ACC-1 / §5.9: start the OAuth 2.0 PKCE login (loopback flow).
    Login,
    /// PRD FR-ACC-9: local logout (delete stored tokens) + link to
    /// server-side grant revocation.
    Logout,
    /// PRD FR-ACC-2: toggle watch/unwatch on the article on screen.
    WatchToggle,
    /// PRD FR-ACC-2: open the watchlist pane (watched pages + activity feed).
    WatchlistOpen,
    /// PRD FR-ACC-3: open the notifications (Echo) pane.
    NotificationsOpen,
    /// PRD FR-ACC-4: open the logged-in user's own contributions.
    ContribsOpen,
    /// PRD FR-ACC-7: open the read-only preferences card.
    PrefsOpen,
    Quit,
    // -- UX-6 additions: these five already worked as keypresses but had no
    // registry row, so the palette and the generated `?` cheatsheet — both
    // built from `COMMANDS`/`READING_HELP_ORDER` — never mentioned them at
    // all. Registering them doesn't change their existing keybinding (each
    // one's `main::handle_key` arm is untouched); it only makes them
    // discoverable the same way every other action already is.
    /// PRD FR-NV-4/5: `K` peeks the focused link (footnote peek or link
    /// preview).
    Peek,
    /// PRD FR-PR-3: `zz` toggles incognito.
    IncognitoToggle,
    /// PRD FR-NV-3: `za` folds/unfolds the section under the cursor.
    FoldToggle,
    /// PRD FR-NV-3: `zM` folds every section.
    FoldAll,
    /// PRD FR-NV-3: `zR` unfolds every section.
    UnfoldAll,
    /// PRD FR-NV-4: `gK` jumps to the References/Notes section.
    JumpReferences,
    /// PRD FR-TB-4: `Ctrl-w v` splits the view. Unlike every other action
    /// here, its binding is a Control-chord *prefix* (`Ctrl-w` arms a latch,
    /// then `v` resolves it — see `App::pending_ctrl_w`), which the
    /// single-key/`prefix`-char `Chord` model this registry uses can't name
    /// directly (`Chord::prefix` is a plain char, never itself a
    /// Control-chord). So this action is deliberately left keyless in the
    /// vim table (`label_for` renders "—"), same as any other unbound
    /// action — its help string spells out the real chord instead.
    Split,
    /// PRD FR-PF-4: `:prefetch-log` — no default key, `:`-only until now.
    PrefetchLogOpen,
    /// PRD FR-PF-3: `:interests` — no default key, `:`-only until now.
    InterestsOpen,
    /// PRD FR-PC-3: `:stats` — no default key, `:`-only until now.
    StatsOpen,
    /// PRD FR-TB-5: `:sessions` — lists named sessions; no default key,
    /// `:`-only until now.
    SessionsList,
    // -- picker-generic (help only; not palette-invokable) --
    MoveDown,
    MoveUp,
    Select,
    Close,
}

/// Where a binding or action is meaningful. Resolution tries the specific
/// context first, then [`KeyContext::Global`], so a globally-bound key (help,
/// palette) reaches every view without being repeated in each table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyContext {
    /// Applies everywhere the keymap is consulted.
    Global,
    /// Reading an article (a document is on screen).
    Reading,
    /// The start page (no document — FR-DL-1).
    StartPage,
    /// Any read-only selectable list (TOC, tab/bookmark/history/... pickers).
    Picker,
    /// The search prompt (FR-SR-1).
    Search,
}

impl KeyContext {
    fn from_toml_name(name: &str) -> Option<Self> {
        Some(match name {
            "global" => KeyContext::Global,
            "reading" => KeyContext::Reading,
            "startpage" => KeyContext::StartPage,
            "picker" => KeyContext::Picker,
            "search" => KeyContext::Search,
            _ => return None,
        })
    }
}

/// The metadata one command carries — the registry's row type.
pub struct Meta {
    pub action: Action,
    /// Stable, hyphenated identifier used in `keymap.toml` and tests.
    pub name: &'static str,
    /// Human-facing label shown in the palette.
    pub display: &'static str,
    /// One-line help string shown in the palette and the help overlay.
    pub help: &'static str,
    /// The contexts this action applies in.
    pub contexts: &'static [KeyContext],
    /// Whether the command palette offers it (argument-free, context-invokable
    /// actions only — raw scroll steps and picker-internal moves are excluded).
    pub in_palette: bool,
}

/// The complete command table — the single source of truth (PRD §5.13).
pub const COMMANDS: &[Meta] = &[
    Meta {
        action: Action::ScrollDown,
        name: "scroll-down",
        display: "Scroll down",
        help: "scroll one line down",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::ScrollUp,
        name: "scroll-up",
        display: "Scroll up",
        help: "scroll one line up",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::HalfPageDown,
        name: "half-page-down",
        display: "Half page down",
        help: "scroll half a page down",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::HalfPageUp,
        name: "half-page-up",
        display: "Half page up",
        help: "scroll half a page up",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::PageDown,
        name: "page-down",
        display: "Page down",
        help: "scroll a full page down",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::ScrollTop,
        name: "scroll-top",
        display: "Go to top",
        help: "jump to the top of the article",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::ScrollBottom,
        name: "scroll-bottom",
        display: "Go to bottom",
        help: "jump to the bottom of the article",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::ScrollTablesLeft,
        name: "table-scroll-left",
        display: "Scroll tables left",
        help: "shift wide tables left",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::ScrollTablesRight,
        name: "table-scroll-right",
        display: "Scroll tables right",
        help: "shift wide tables right",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::LinkCycleNext,
        name: "link-cycle-next",
        display: "Next link",
        help: "focus the next link",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::LinkCyclePrev,
        name: "link-cycle-prev",
        display: "Previous link",
        help: "focus the previous link",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::LinkHints,
        name: "link-hints",
        display: "Link hints",
        help: "label visible links; type the label to follow",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::LinkHintsBackground,
        name: "link-hints-background",
        display: "Link hints (background tab)",
        help: "label visible links; follow into a background tab",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::FollowLink,
        name: "follow-link",
        display: "Follow link",
        help: "open the focused link",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::OpenBackgroundTab,
        name: "open-background-tab",
        display: "Open link in background tab",
        help: "open the focused link in a background tab",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::Back,
        name: "history-back",
        display: "Back",
        help: "go back in this tab's history",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::Forward,
        name: "history-forward",
        display: "Forward",
        help: "go forward in this tab's history",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::BackStackPicker,
        name: "back-stack-picker",
        display: "Back-stack picker",
        help: "browse this tab's history trail",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::ReadingHistory,
        name: "reading-history",
        display: "Reading history",
        help: "the persistent, cross-session reading history",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::NextTab,
        name: "tab-next",
        display: "Next tab",
        help: "switch to the next tab",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::PrevTab,
        name: "tab-prev",
        display: "Previous tab",
        help: "switch to the previous tab",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::TabPicker,
        name: "tab-picker",
        display: "Tab picker",
        help: "pick from the open tabs",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::ReopenClosedTab,
        name: "tab-reopen",
        display: "Reopen closed tab",
        help: "reopen the most recently closed tab",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::CloseTab,
        name: "tab-close",
        display: "Close tab",
        help: "close the current tab (quits on the last)",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::Toc,
        name: "toc",
        display: "Table of contents",
        help: "open the table of contents",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::CycleTheme,
        name: "cycle-theme",
        display: "Cycle theme",
        help: "cycle to the next color theme",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::TalkToggle,
        name: "talk-toggle",
        display: "Talk page",
        help: "flip between the article and its talk page",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::Search,
        name: "search",
        display: "Search",
        help: "search Wikipedia (typeahead + full-text)",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::CommandLine,
        name: "command-line",
        display: "Command line",
        help: "open the : ex-command line",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::Help,
        name: "help",
        display: "Help",
        help: "show this view's keybinding cheatsheet",
        contexts: &[KeyContext::Global],
        in_palette: true,
    },
    Meta {
        action: Action::Palette,
        name: "command-palette",
        display: "Command palette",
        help: "fuzzy-find any command",
        // UX-14: `main::palette_allowed` only opens the palette from
        // Reading (text-input modes have their own Ctrl-p, and the latches
        // documented there must gate it too) — `Global` here overclaimed
        // reach the runtime never honors.
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::YankUrl,
        name: "yank-url",
        display: "Yank URL",
        help: "copy the article's canonical URL",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::YankMarkdown,
        name: "yank-markdown",
        display: "Yank Markdown link",
        help: "copy the article as a Markdown link",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::ReadLater,
        name: "read-later",
        display: "Read later",
        help: "queue the focused link (or article) for later",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::Research,
        name: "research",
        display: "Research mode",
        // UX-3 fix: `r` reads as a plain one-key action here, but it's
        // actually a chord's fallback — `main::handle_key` arms `pending_r`
        // and waits for a second key (`rl` queues for later, Esc cancels,
        // anything else — including a timed-out non-char key — resolves to
        // this). Spelling that out in the help text itself (rather than
        // inventing a "prefix" column just for this one row) is the smaller
        // of the two fixes UX-3 allows for.
        help: "cite this page and its sources — r alone (rl instead queues the page for later)",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::Library,
        name: "library",
        display: "Library",
        help: "browse and export the saved bibliography",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::BookmarkToggle,
        name: "bookmark-toggle",
        display: "Toggle bookmark",
        help: "bookmark or un-bookmark this article",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::BookmarkPicker,
        name: "bookmark-picker",
        display: "Bookmarks",
        help: "open the bookmark picker",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::Annotate,
        name: "annotate",
        display: "Annotate bookmark",
        help: "write a note on this article's bookmark",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::FindInPage,
        name: "find-in-page",
        display: "Find in page",
        help: "incremental in-page search",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::FindNext,
        name: "find-next",
        display: "Find next",
        help: "jump to the next in-page match",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::FindPrev,
        name: "find-prev",
        display: "Find previous",
        help: "jump to the previous in-page match",
        contexts: &[KeyContext::Reading],
        in_palette: false,
    },
    Meta {
        action: Action::SaveOffline,
        name: "save-offline",
        display: "Save offline",
        help: "pin this article for offline reading",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::RandomArticle,
        name: "random-article",
        display: "Random article",
        help: "open a random article",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::RelatedPanel,
        name: "related",
        display: "Related articles",
        help: "articles similar to this one (morelike:)",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::LangPicker,
        name: "lang-picker",
        display: "Language editions",
        help: "switch to another language edition",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::WikiPicker,
        name: "wiki-picker",
        display: "Wiki",
        help: "switch to Wikipedia or a sister project",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::Home,
        name: "home",
        display: "Start page",
        help: "return to the start page",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::Today,
        name: "today",
        display: "On this day",
        help: "open the on-this-day panel",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::Info,
        name: "info",
        display: "Article info",
        help: "show this article's attribution: title, URL, revision, license, history",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::Login,
        name: "login",
        display: "Log in",
        help: "log in to Wikipedia via OAuth (watchlist, notifications, reading lists)",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::Logout,
        name: "logout",
        display: "Log out",
        help: "log out and delete stored tokens (revoke server-side via OAuthManageMyGrants)",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::WatchToggle,
        name: "watch-toggle",
        display: "Watch / unwatch",
        help: "toggle watching this article for edits (logged in)",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::WatchlistOpen,
        name: "watchlist",
        display: "Watchlist",
        help: "watched pages and recent changes to them (logged in)",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::NotificationsOpen,
        name: "notifications",
        display: "Notifications",
        help: "Echo alerts and messages (logged in)",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::ContribsOpen,
        name: "contribs",
        display: "Contributions",
        help: "your recent edits (or :contribs <username> for anyone's)",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::PrefsOpen,
        name: "prefs",
        display: "Preferences",
        help: "your account preferences, read-only (logged in)",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::Quit,
        name: "quit",
        display: "Quit",
        help: "quit wikitui (with confirmation)",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    // -- UX-6 additions (see the `Action` variants' own doc comments) --
    Meta {
        action: Action::Peek,
        name: "peek",
        display: "Peek",
        help: "preview the focused link or footnote without leaving the page",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::IncognitoToggle,
        name: "incognito-toggle",
        display: "Toggle incognito",
        help: "no history, no stats, no prefetch while on",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::FoldToggle,
        name: "fold-toggle",
        display: "Fold/unfold section",
        help: "fold or unfold the section under the cursor",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::FoldAll,
        name: "fold-all",
        display: "Fold all sections",
        help: "collapse every section",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::UnfoldAll,
        name: "unfold-all",
        display: "Unfold all sections",
        help: "expand every section",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::JumpReferences,
        name: "jump-references",
        display: "Jump to references",
        help: "jump to the References/Notes section",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::Split,
        name: "split",
        display: "Split view",
        help: "split the view (Ctrl-w v; Ctrl-w w switches panes, Ctrl-w c/o/q closes)",
        contexts: &[KeyContext::Reading],
        in_palette: true,
    },
    Meta {
        action: Action::PrefetchLogOpen,
        name: "prefetch-log",
        display: "Prefetch log",
        help: "the prefetch transparency/debug panel",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::InterestsOpen,
        name: "interests",
        display: "Interests",
        help: "the local interest-affinity model, in full",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::StatsOpen,
        name: "stats-open",
        display: "Reading stats",
        help: "articles read, time spent, streaks, top topics",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    Meta {
        action: Action::SessionsList,
        name: "sessions",
        display: "Named sessions",
        help: "list this profile's named sessions (:mksession/:session)",
        contexts: &[KeyContext::Reading, KeyContext::StartPage],
        in_palette: true,
    },
    // Picker-generic actions: help overlay only, never the palette.
    Meta {
        action: Action::MoveDown,
        name: "move-down",
        display: "Move down",
        help: "move the selection down",
        contexts: &[KeyContext::Picker],
        in_palette: false,
    },
    Meta {
        action: Action::MoveUp,
        name: "move-up",
        display: "Move up",
        help: "move the selection up",
        contexts: &[KeyContext::Picker],
        in_palette: false,
    },
    Meta {
        action: Action::Select,
        name: "select",
        display: "Select",
        help: "open / confirm the highlighted entry",
        contexts: &[KeyContext::Picker],
        in_palette: false,
    },
    Meta {
        action: Action::Close,
        name: "close",
        display: "Close",
        help: "close this view",
        contexts: &[KeyContext::Picker],
        in_palette: false,
    },
];

/// Look up a command's metadata.
pub fn meta(action: Action) -> &'static Meta {
    COMMANDS
        .iter()
        .find(|m| m.action == action)
        .expect("every Action has exactly one COMMANDS row")
}

impl Action {
    /// Resolve a stable command name (as written in `keymap.toml`) to its
    /// action. `None` for an unknown name.
    pub fn by_name(name: &str) -> Option<Action> {
        COMMANDS.iter().find(|m| m.name == name).map(|m| m.action)
    }

    pub fn display(self) -> &'static str {
        meta(self).display
    }

    pub fn help(self) -> &'static str {
        meta(self).help
    }

    /// Whether this action is meaningful in `ctx` (its own contexts, or a
    /// `Global` action which applies everywhere).
    pub fn applies_in(self, ctx: KeyContext) -> bool {
        let m = meta(self);
        m.contexts.contains(&ctx) || m.contexts.contains(&KeyContext::Global)
    }
}

/// A key or two-key chord: an optional leading prefix character (the
/// established `g`/`b`/`r` latches — `gg`, `gt`, `bb`, `rl`), an optional
/// Control modifier, and the terminating key code.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chord {
    pub prefix: Option<char>,
    pub ctrl: bool,
    pub code: ChordCode,
}

/// The subset of key codes a binding can name (Shift is folded into the
/// character case; `BackTab` is the Shift-Tab the terminal already reports).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChordCode {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Backspace,
}

impl Chord {
    pub fn key(code: ChordCode) -> Chord {
        Chord {
            prefix: None,
            ctrl: false,
            code,
        }
    }

    pub fn ch(c: char) -> Chord {
        Chord::key(ChordCode::Char(c))
    }

    pub fn ctrl_ch(c: char) -> Chord {
        Chord {
            prefix: None,
            ctrl: true,
            code: ChordCode::Char(c),
        }
    }

    pub fn ctrl_code(code: ChordCode) -> Chord {
        Chord {
            prefix: None,
            ctrl: true,
            code,
        }
    }

    pub fn prefixed(prefix: char, c: char) -> Chord {
        Chord {
            prefix: Some(prefix),
            ctrl: false,
            code: ChordCode::Char(c),
        }
    }

    /// Translate a pressed key into a single-key chord (never a prefixed one —
    /// the `g`/`b`/`r` latches are resolved by `handle_key`, not here). Space
    /// arrives as `Char(' ')`, matching how the vim table stores it.
    pub fn from_key(code: KeyCode, mods: KeyModifiers) -> Option<Chord> {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let code = match code {
            KeyCode::Char(c) => ChordCode::Char(c),
            KeyCode::Enter => ChordCode::Enter,
            KeyCode::Tab => ChordCode::Tab,
            KeyCode::BackTab => ChordCode::BackTab,
            KeyCode::Esc => ChordCode::Esc,
            KeyCode::Up => ChordCode::Up,
            KeyCode::Down => ChordCode::Down,
            KeyCode::Left => ChordCode::Left,
            KeyCode::Right => ChordCode::Right,
            KeyCode::Backspace => ChordCode::Backspace,
            _ => return None,
        };
        Some(Chord {
            prefix: None,
            ctrl,
            code,
        })
    }

    /// A human-readable label for the help overlay and the palette (`"gg"`,
    /// `"Ctrl-d"`, `"Space"`, `"Shift-Tab"`, `"↓"`).
    pub fn label(&self) -> String {
        let base = match &self.code {
            ChordCode::Char(' ') => "Space".to_string(),
            ChordCode::Char(c) => c.to_string(),
            ChordCode::Enter => "Enter".to_string(),
            ChordCode::Tab => "Tab".to_string(),
            ChordCode::BackTab => "Shift-Tab".to_string(),
            ChordCode::Esc => "Esc".to_string(),
            ChordCode::Up => "\u{2191}".to_string(),
            ChordCode::Down => "\u{2193}".to_string(),
            ChordCode::Left => "\u{2190}".to_string(),
            ChordCode::Right => "\u{2192}".to_string(),
            ChordCode::Backspace => "Backspace".to_string(),
        };
        let base = if self.ctrl {
            format!("Ctrl-{base}")
        } else {
            base
        };
        match self.prefix {
            Some(p) => format!("{p}{base}"),
            None => base,
        }
    }

    /// Parse a `keymap.toml` key string (`"J"`, `"Ctrl-e"`, `"gg"`,
    /// `"Shift-Tab"`, `"Space"`) into a chord. `None` for anything
    /// unrecognized (the loader warns and skips).
    pub fn parse(s: &str) -> Option<Chord> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // A Control modifier is written as a leading `Ctrl-`/`C-` segment.
        let (ctrl, rest) = if let Some(r) = s
            .strip_prefix("Ctrl-")
            .or_else(|| s.strip_prefix("ctrl-"))
            .or_else(|| s.strip_prefix("C-"))
        {
            (true, r)
        } else {
            (false, s)
        };

        // A two-character all-lowercase-prefix chord (`gg`, `gt`, `bb`, `rl`,
        // `zz`) — only the established latch characters may lead one. `z`
        // joined this list alongside the UX-6 registry additions for
        // `zz`/`za`/`zM`/`zR` (previously a `keymap.toml` had no way to name
        // them at all, since `Chord::parse` is also this grammar's entry
        // point for user rebindings).
        if !ctrl {
            let chars: Vec<char> = rest.chars().collect();
            if chars.len() == 2 && matches!(chars[0], 'g' | 'b' | 'r' | 'z') {
                return Some(Chord::prefixed(chars[0], chars[1]));
            }
        }

        let code = match rest {
            "Space" | "space" => ChordCode::Char(' '),
            "Enter" | "Return" | "CR" => ChordCode::Enter,
            "Tab" => ChordCode::Tab,
            "BackTab" | "Shift-Tab" | "S-Tab" => ChordCode::BackTab,
            "Esc" | "Escape" => ChordCode::Esc,
            "Up" => ChordCode::Up,
            "Down" => ChordCode::Down,
            "Left" => ChordCode::Left,
            "Right" => ChordCode::Right,
            "Backspace" | "BS" => ChordCode::Backspace,
            other => {
                let mut it = other.chars();
                let c = it.next()?;
                if it.next().is_some() {
                    return None; // multi-char that isn't a known named key
                }
                ChordCode::Char(c)
            }
        };
        Some(Chord {
            prefix: None,
            ctrl,
            code,
        })
    }
}

/// PRD FR-CS-3: parse a leader-key spelling — a single visible character
/// (`","`, `";"`, `"\\"`) or the word `Space`. Returns `None` for anything
/// that isn't exactly one usable key, so the loader can warn and keep the
/// default. Multi-character named keys (Enter/Tab/…) are deliberately not
/// accepted as a leader — a leader must be a plain typed character.
fn single_leader_char(s: &str) -> Option<char> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("space") {
        return Some(' ');
    }
    let mut it = s.chars();
    let c = it.next()?;
    if it.next().is_some() { None } else { Some(c) }
}

/// A configurable keymap: a `base` table (the built-in preset, mirroring
/// `handle_key`'s hardcoded arms) plus an `overrides` layer (the emacs
/// preset's deltas and any user `keymap.toml`). Runtime dispatch reads only
/// the overrides; the palette and help read the merged view.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    base: HashMap<KeyContext, HashMap<Chord, Action>>,
    overrides: HashMap<KeyContext, HashMap<Chord, Action>>,
    /// PRD FR-CS-3's leader key: the single key that opens the leader-prefixed
    /// binding space (`leader` below). `'\0'` means "no leader" (the `Default`
    /// derive's value — never produced by a real preset, which all set
    /// [`DEFAULT_LEADER`]).
    leader_key: char,
    /// PRD FR-CS-3: the leader-prefixed bindings — the *second* key (pressed
    /// after `leader_key`) mapped to its action. A built-in set (see
    /// [`Keymap::vim`]) that a `keymap.toml` `[leader]` section and the
    /// `:map`-family commands extend at runtime.
    leader: HashMap<Chord, Action>,
}

/// PRD FR-CS-3's default leader key. `,` rather than Space (Space is
/// `PageDown` in Reading) or any letter already bound there — an otherwise
/// unbound key, so turning it into a prefix steals nothing.
pub const DEFAULT_LEADER: char = ',';

impl Keymap {
    fn bind(
        table: &mut HashMap<KeyContext, HashMap<Chord, Action>>,
        ctx: KeyContext,
        chord: Chord,
        action: Action,
    ) {
        table.entry(ctx).or_default().insert(chord, action);
    }

    /// The built-in `vim` keymap — the exact set of bindings
    /// `main::handle_key` already implements (PRD Appendix B + everything
    /// shipped since). Asserted binding-for-binding by the regression tests.
    pub fn vim() -> Keymap {
        use Action::*;
        use ChordCode::*;
        use KeyContext::{Global, Picker, Reading};
        let mut base: HashMap<KeyContext, HashMap<Chord, Action>> = HashMap::new();
        let b = &mut base;

        // Global (works in Reading and every picker).
        Keymap::bind(b, Global, Chord::ch('?'), Help);
        Keymap::bind(b, Global, Chord::ctrl_ch('p'), Palette);

        // Reading — scrolling.
        Keymap::bind(b, Reading, Chord::ch('j'), ScrollDown);
        Keymap::bind(b, Reading, Chord::key(Down), ScrollDown);
        Keymap::bind(b, Reading, Chord::ch('k'), ScrollUp);
        Keymap::bind(b, Reading, Chord::key(Up), ScrollUp);
        Keymap::bind(b, Reading, Chord::ctrl_ch('d'), HalfPageDown);
        Keymap::bind(b, Reading, Chord::ctrl_ch('u'), HalfPageUp);
        Keymap::bind(b, Reading, Chord::ch(' '), PageDown);
        Keymap::bind(b, Reading, Chord::prefixed('g', 'g'), ScrollTop);
        Keymap::bind(b, Reading, Chord::ch('G'), ScrollBottom);
        Keymap::bind(b, Reading, Chord::ch(']'), ScrollTablesRight);
        Keymap::bind(b, Reading, Chord::ch('['), ScrollTablesLeft);

        // Reading — links & navigation.
        Keymap::bind(b, Reading, Chord::key(Tab), LinkCycleNext);
        Keymap::bind(b, Reading, Chord::key(BackTab), LinkCyclePrev);
        Keymap::bind(b, Reading, Chord::ch('f'), LinkHints);
        Keymap::bind(b, Reading, Chord::ch('F'), LinkHintsBackground);
        Keymap::bind(b, Reading, Chord::key(Enter), FollowLink);
        Keymap::bind(b, Reading, Chord::ctrl_code(Enter), OpenBackgroundTab);
        Keymap::bind(b, Reading, Chord::ch('H'), Back);
        Keymap::bind(b, Reading, Chord::ch('L'), Forward);
        Keymap::bind(b, Reading, Chord::prefixed('g', 'b'), BackStackPicker);
        Keymap::bind(b, Reading, Chord::ctrl_ch('h'), ReadingHistory);

        // Reading — tabs.
        Keymap::bind(b, Reading, Chord::prefixed('g', 't'), NextTab);
        Keymap::bind(b, Reading, Chord::prefixed('g', 'T'), PrevTab);
        Keymap::bind(b, Reading, Chord::prefixed('b', 'b'), TabPicker);
        Keymap::bind(b, Reading, Chord::ch('u'), ReopenClosedTab);
        Keymap::bind(b, Reading, Chord::ch('q'), CloseTab);

        // Reading — views & tools.
        Keymap::bind(b, Reading, Chord::ch('t'), Toc);
        // PRD Appendix B's "Article: T talk page" wins the bare `T` key;
        // cycle-theme (which Appendix B never actually binds a bare key to —
        // only `:theme <name>`) moves to Ctrl-t. See `main::toggle_talk_page`
        // for the conflict this resolves and why.
        Keymap::bind(b, Reading, Chord::ch('T'), TalkToggle);
        Keymap::bind(b, Reading, Chord::ctrl_ch('t'), CycleTheme);
        Keymap::bind(b, Reading, Chord::ch('/'), Search);
        Keymap::bind(b, Reading, Chord::ch(':'), CommandLine);
        Keymap::bind(b, Reading, Chord::ch('y'), YankUrl);
        Keymap::bind(b, Reading, Chord::ch('Y'), YankMarkdown);
        Keymap::bind(b, Reading, Chord::prefixed('r', 'l'), ReadLater);
        Keymap::bind(b, Reading, Chord::ch('r'), Research);
        Keymap::bind(b, Reading, Chord::ch('m'), BookmarkToggle);
        Keymap::bind(b, Reading, Chord::ch('B'), BookmarkPicker);
        Keymap::bind(b, Reading, Chord::prefixed('b', 'a'), Annotate);
        Keymap::bind(b, Reading, Chord::ch('R'), Library);
        Keymap::bind(b, Reading, Chord::ctrl_ch('f'), FindInPage);
        Keymap::bind(b, Reading, Chord::ch('n'), FindNext);
        Keymap::bind(b, Reading, Chord::ch('N'), FindPrev);
        Keymap::bind(b, Reading, Chord::ch('S'), SaveOffline);
        Keymap::bind(b, Reading, Chord::prefixed('g', 'r'), RandomArticle);
        Keymap::bind(b, Reading, Chord::prefixed('g', 'R'), RelatedPanel);
        Keymap::bind(b, Reading, Chord::prefixed('g', 'h'), Home);
        // PRD §10 / Appendix B's "Article: i article info/attribution".
        Keymap::bind(b, Reading, Chord::ch('i'), Info);
        // PRD FR-ACC-2 / Appendix B: watch/unwatch the article on screen.
        Keymap::bind(b, Reading, Chord::ch('w'), WatchToggle);
        // PRD FR-ACC-2: `gW` opens the watchlist pane — the `g`-prefix
        // panel-open convention `gr`/`gR`/`gb` already established, since
        // Appendix B documents no dedicated single-letter open key for it.
        Keymap::bind(b, Reading, Chord::prefixed('g', 'W'), WatchlistOpen);
        // UX-6: these five already worked as keypresses (`main::handle_key`'s
        // `K`/`z`-prefix/`g`-prefix arms) but had no registry row at all —
        // registering their existing bindings here is what makes the palette
        // and generated `?` cheatsheet finally mention them (see each
        // `Action` variant's own doc comment). `Action::Split` is the one
        // exception — see its doc comment for why it stays keyless here.
        Keymap::bind(b, Reading, Chord::ch('K'), Peek);
        Keymap::bind(b, Reading, Chord::prefixed('z', 'z'), IncognitoToggle);
        Keymap::bind(b, Reading, Chord::prefixed('z', 'a'), FoldToggle);
        Keymap::bind(b, Reading, Chord::prefixed('z', 'M'), FoldAll);
        Keymap::bind(b, Reading, Chord::prefixed('z', 'R'), UnfoldAll);
        Keymap::bind(b, Reading, Chord::prefixed('g', 'K'), JumpReferences);
        Keymap::bind(b, Reading, Chord::ch('Q'), Quit);

        // Picker-generic navigation.
        Keymap::bind(b, Picker, Chord::ch('j'), MoveDown);
        Keymap::bind(b, Picker, Chord::key(Down), MoveDown);
        Keymap::bind(b, Picker, Chord::ch('k'), MoveUp);
        Keymap::bind(b, Picker, Chord::key(Up), MoveUp);
        Keymap::bind(b, Picker, Chord::key(Enter), Select);
        Keymap::bind(b, Picker, Chord::key(Esc), Close);

        // PRD FR-CS-3 leader space (`,` then a key). A small built-in set of
        // discoverable shortcuts — every one dispatched through the same
        // `main::dispatch_action` path a runtime rebinding uses, so a
        // `keymap.toml` `[leader]` section or `:map`/`:unmap` can add to or
        // shadow it. Deliberately reuses actions that also have single-key
        // bindings: the leader is a *namespace*, not a place to hide
        // otherwise-unreachable actions.
        let mut leader: HashMap<Chord, Action> = HashMap::new();
        leader.insert(Chord::ch('t'), Toc);
        leader.insert(Chord::ch('s'), SaveOffline);
        leader.insert(Chord::ch('w'), WatchToggle);
        leader.insert(Chord::ch('r'), Research);
        leader.insert(Chord::ch('d'), Today);
        leader.insert(Chord::ch('h'), Home);

        Keymap {
            base,
            overrides: HashMap::new(),
            leader_key: DEFAULT_LEADER,
            leader,
        }
    }

    /// The built-in `emacs` preset. Additive over the shared vim base: it adds
    /// Control-chord equivalents for scrolling and search. Quit (`Q`) and
    /// tab-switch (`gt`/`gT`) come from the base — full emacs bindings are a
    /// documented seam (SP-10), not shipped here. Its deltas live in the
    /// override layer, so they take effect at runtime through the same path a
    /// user `keymap.toml` does.
    pub fn emacs() -> Keymap {
        use Action::*;
        use KeyContext::Reading;
        let mut km = Keymap::vim();
        km.apply_override(Reading, Chord::ctrl_ch('n'), ScrollDown);
        km.apply_override(Reading, Chord::ctrl_ch('v'), PageDown);
        km.apply_override(Reading, Chord::ctrl_ch('s'), Search);
        km
    }

    /// Build a preset by name (`"vim"` default, `"emacs"`); unknown names fall
    /// back to vim.
    pub fn preset(name: &str) -> Keymap {
        match name {
            "emacs" => Keymap::emacs(),
            _ => Keymap::vim(),
        }
    }

    /// Add or replace a binding in the override layer.
    pub fn apply_override(&mut self, ctx: KeyContext, chord: Chord, action: Action) {
        self.overrides.entry(ctx).or_default().insert(chord, action);
    }

    /// PRD FR-CS-3 (`:unmap`): remove a binding from the override layer,
    /// returning whether one was actually there. Only the override layer is
    /// touched — a `:unmap` can undo a `:map`/preset/`keymap.toml` override,
    /// but never deletes a hardcoded default binding (which lives in `base`
    /// and `main::handle_key`'s own arms, not here).
    pub fn remove_override(&mut self, ctx: KeyContext, chord: &Chord) -> bool {
        self.overrides
            .get_mut(&ctx)
            .and_then(|t| t.remove(chord))
            .is_some()
    }

    /// PRD FR-CS-3: the active leader key (the prefix that opens the leader
    /// binding space). `'\0'` when unset (no preset produces that).
    pub fn leader_key(&self) -> char {
        self.leader_key
    }

    /// PRD FR-CS-3: resolve the key pressed *after* the leader to its action.
    pub fn leader_action(&self, chord: &Chord) -> Option<Action> {
        self.leader.get(chord).copied()
    }

    /// PRD FR-CS-3: add or replace a leader binding (a `keymap.toml`
    /// `[leader]` entry, or `:map ,<key> <action>` at runtime).
    pub fn set_leader_binding(&mut self, chord: Chord, action: Action) {
        self.leader.insert(chord, action);
    }

    /// PRD FR-CS-3: remove a leader binding (`:unmap ,<key>`), returning
    /// whether one existed.
    pub fn remove_leader_binding(&mut self, chord: &Chord) -> bool {
        self.leader.remove(chord).is_some()
    }

    /// Resolve a chord to an action: override layer first, then the base
    /// table; the specific context first, then `Global`. This is the full,
    /// specified resolution the test suite locks the vim/emacs tables against
    /// — the runtime deliberately does *not* use it (that would reroute every
    /// default binding through the dispatcher and risk a behavior change);
    /// `handle_key` uses the narrower [`Keymap::runtime_action`] instead.
    #[allow(dead_code)]
    pub fn resolve(&self, ctx: KeyContext, chord: &Chord) -> Option<Action> {
        for layer in [&self.overrides, &self.base] {
            for c in [ctx, KeyContext::Global] {
                if let Some(a) = layer.get(&c).and_then(|t| t.get(chord)) {
                    return Some(*a);
                }
            }
        }
        None
    }

    /// Resolve only against the override layer — the vim default has none, so
    /// this returns `None` for every default binding and `handle_key`'s
    /// hardcoded arms stay the dispatch path (no behavior change). A preset or
    /// user rebinding populates the overrides and is what this surfaces.
    pub fn runtime_action(&self, ctx: KeyContext, chord: &Chord) -> Option<Action> {
        for c in [ctx, KeyContext::Global] {
            if let Some(a) = self.overrides.get(&c).and_then(|t| t.get(chord)) {
                return Some(*a);
            }
        }
        None
    }

    /// Every chord bound to `action` in `ctx` (or `Global`), base and
    /// override layers merged, sorted for a stable help/palette rendering.
    /// Empty when nothing is bound.
    pub fn bindings_for(&self, ctx: KeyContext, action: Action) -> Vec<Chord> {
        let mut out: Vec<Chord> = Vec::new();
        for layer in [&self.base, &self.overrides] {
            for c in [ctx, KeyContext::Global] {
                if let Some(t) = layer.get(&c) {
                    for (chord, a) in t {
                        if *a == action && !out.contains(chord) {
                            out.push(chord.clone());
                        }
                    }
                }
            }
        }
        out.sort_by_key(|c| c.label());
        out
    }

    /// A joined key label for `action` (`"j/↓"`), or `"—"` when unbound.
    pub fn label_for(&self, ctx: KeyContext, action: Action) -> String {
        let chords = self.bindings_for(ctx, action);
        if chords.is_empty() {
            "\u{2014}".to_string()
        } else {
            chords
                .iter()
                .map(Chord::label)
                .collect::<Vec<_>>()
                .join("/")
        }
    }

    /// Apply a user `keymap.toml`. Sections are context names
    /// (`[reading]`, `[global]`, ...); each `"<key>" = "<command-name>"`
    /// binding adds to the override layer. Returns human-readable warnings for
    /// every line it could not apply — the caller prints them; a bad file is
    /// never fatal (PRD §6.7: warn, never crash).
    pub fn apply_user_toml(&mut self, text: &str) -> Vec<String> {
        let mut warnings = Vec::new();
        let table: toml::Table = match toml::from_str(text) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("keymap.toml is not valid TOML: {e}"));
                return warnings;
            }
        };
        for (section, value) in &table {
            // PRD FR-CS-3: a top-level `leader-key = ","` sets the leader
            // prefix key itself (a single character).
            if section == "leader-key" {
                match value.as_str().and_then(single_leader_char) {
                    Some(c) => self.leader_key = c,
                    None => warnings.push(
                        "keymap.toml: leader-key must be a single-character string".to_string(),
                    ),
                }
                continue;
            }
            // PRD FR-CS-3: the `[leader]` table — `"<key>" = "<command>"`
            // bindings in the leader-prefixed space (the key pressed *after*
            // the leader).
            if section == "leader" {
                let Some(bindings) = value.as_table() else {
                    warnings
                        .push("keymap.toml: [leader] must be a table of key = command".to_string());
                    continue;
                };
                for (key, cmd) in bindings {
                    let Some(chord) = Chord::parse(key) else {
                        warnings.push(format!(
                            "keymap.toml: [leader] {key:?} is not a recognizable key — ignored"
                        ));
                        continue;
                    };
                    let Some(action) = cmd.as_str().and_then(Action::by_name) else {
                        warnings.push(format!(
                            "keymap.toml: [leader] {key} -> unknown command — ignored"
                        ));
                        continue;
                    };
                    self.set_leader_binding(chord, action);
                }
                continue;
            }
            let Some(ctx) = KeyContext::from_toml_name(section) else {
                warnings.push(format!(
                    "keymap.toml: unknown context [{section}] — ignored (try: global, reading, startpage, picker, search, leader)"
                ));
                continue;
            };
            let Some(bindings) = value.as_table() else {
                warnings.push(format!(
                    "keymap.toml: [{section}] must be a table of key = command"
                ));
                continue;
            };
            for (key, cmd) in bindings {
                let Some(chord) = Chord::parse(key) else {
                    warnings.push(format!(
                        "keymap.toml: [{section}] {key:?} is not a recognizable key — ignored"
                    ));
                    continue;
                };
                let Some(name) = cmd.as_str() else {
                    warnings.push(format!(
                        "keymap.toml: [{section}] {key} must map to a command name string"
                    ));
                    continue;
                };
                let Some(action) = Action::by_name(name) else {
                    warnings.push(format!(
                        "keymap.toml: [{section}] {key} -> unknown command {name:?} — ignored"
                    ));
                    continue;
                };
                self.apply_override(ctx, chord, action);
            }
        }
        warnings
    }
}

/// The actions the command palette offers in `ctx` (PRD FR-CS-1): every
/// palette-enabled command that applies there, filtered by the fuzzy `query`,
/// ranked best-match-first. Each row carries its key label so the palette can
/// show "command — key — help" (see `ui::draw_palette`).
pub fn palette_matches(keymap: &Keymap, ctx: KeyContext, query: &str) -> Vec<PaletteRow> {
    let mut rows: Vec<(f64, PaletteRow)> = COMMANDS
        .iter()
        .filter(|m| m.in_palette && m.action.applies_in(ctx))
        .filter_map(|m| {
            crate::fuzzy::fuzzy_score(m.display, query).map(|score| {
                (
                    score,
                    PaletteRow {
                        action: m.action,
                        display: m.action.display(),
                        help: m.action.help(),
                        key: keymap.label_for(ctx, m.action),
                    },
                )
            })
        })
        .collect();
    // Best score first; ties broken alphabetically for a stable order.
    rows.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.display.cmp(b.1.display))
    });
    rows.into_iter().map(|(_, r)| r).collect()
}

/// One palette row: the action to run, plus the strings the palette paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteRow {
    pub action: Action,
    pub display: &'static str,
    pub help: &'static str,
    pub key: String,
}

/// The ordered actions the Reading-view cheatsheet lists (PRD FR-CS-4). Order
/// is curated for readability; the *keys* and *help* come from the registry
/// and keymap, so the sheet can never drift from the actual bindings.
pub const READING_HELP_ORDER: &[Action] = &[
    Action::ScrollDown,
    Action::HalfPageDown,
    Action::PageDown,
    Action::ScrollTop,
    Action::ScrollBottom,
    Action::ScrollTablesRight,
    Action::LinkCycleNext,
    Action::LinkHints,
    Action::LinkHintsBackground,
    Action::FollowLink,
    Action::OpenBackgroundTab,
    Action::Peek,
    Action::Back,
    Action::Forward,
    Action::BackStackPicker,
    Action::JumpReferences,
    Action::ReadingHistory,
    Action::NextTab,
    Action::PrevTab,
    Action::TabPicker,
    Action::ReopenClosedTab,
    Action::CloseTab,
    Action::Split,
    Action::Toc,
    Action::FoldToggle,
    Action::FoldAll,
    Action::UnfoldAll,
    Action::CycleTheme,
    Action::TalkToggle,
    Action::IncognitoToggle,
    Action::Search,
    Action::FindInPage,
    Action::FindNext,
    Action::CommandLine,
    Action::Palette,
    Action::YankUrl,
    Action::YankMarkdown,
    Action::BookmarkToggle,
    Action::BookmarkPicker,
    Action::Annotate,
    Action::ReadLater,
    Action::Research,
    Action::Library,
    Action::SaveOffline,
    Action::RandomArticle,
    Action::RelatedPanel,
    Action::LangPicker,
    Action::WikiPicker,
    Action::Home,
    Action::Today,
    Action::Info,
    Action::Login,
    Action::Logout,
    Action::WatchToggle,
    Action::WatchlistOpen,
    Action::NotificationsOpen,
    Action::ContribsOpen,
    Action::PrefsOpen,
    Action::PrefetchLogOpen,
    Action::InterestsOpen,
    Action::StatsOpen,
    Action::SessionsList,
    Action::Help,
    Action::Quit,
];

/// One row of a generated cheatsheet: the key label and the help text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpRow {
    pub key: String,
    pub help: String,
}

/// Generate the Reading-view cheatsheet from the registry + `keymap` (PRD
/// FR-CS-4). Always accurate because both columns are read live.
pub fn reading_help(keymap: &Keymap) -> Vec<HelpRow> {
    READING_HELP_ORDER
        .iter()
        .map(|&action| HelpRow {
            key: keymap.label_for(KeyContext::Reading, action),
            help: action.help().to_string(),
        })
        .collect()
}

/// The generic picker-navigation cheatsheet rows, generated from the registry
/// and `keymap`. Per-picker extras (delete, filter, ...) are appended by the
/// caller (`ui`), which knows the concrete view.
pub fn picker_help(keymap: &Keymap) -> Vec<HelpRow> {
    [
        Action::MoveDown,
        Action::MoveUp,
        Action::Select,
        Action::Help,
        Action::Close,
    ]
    .iter()
    .map(|&action| HelpRow {
        key: keymap.label_for(KeyContext::Picker, action),
        help: action.help().to_string(),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRD FR-CS-3 (regression backbone): the vim keymap must reproduce the
    /// bindings `handle_key` implements. Every important one is asserted here
    /// so a refactor that silently drops or reroutes a key fails loudly.
    #[test]
    fn vim_keymap_reproduces_the_key_existing_bindings() {
        let km = Keymap::vim();
        use Action::*;
        use ChordCode::*;
        use KeyContext::{Picker, Reading};

        // Scrolling.
        assert_eq!(km.resolve(Reading, &Chord::ch('j')), Some(ScrollDown));
        assert_eq!(km.resolve(Reading, &Chord::key(Down)), Some(ScrollDown));
        assert_eq!(km.resolve(Reading, &Chord::ch('k')), Some(ScrollUp));
        assert_eq!(
            km.resolve(Reading, &Chord::ctrl_ch('d')),
            Some(HalfPageDown)
        );
        assert_eq!(km.resolve(Reading, &Chord::ctrl_ch('u')), Some(HalfPageUp));
        assert_eq!(km.resolve(Reading, &Chord::ch(' ')), Some(PageDown));
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('g', 'g')),
            Some(ScrollTop)
        );
        assert_eq!(km.resolve(Reading, &Chord::ch('G')), Some(ScrollBottom));

        // Links & navigation.
        assert_eq!(km.resolve(Reading, &Chord::key(Tab)), Some(LinkCycleNext));
        assert_eq!(
            km.resolve(Reading, &Chord::key(BackTab)),
            Some(LinkCyclePrev)
        );
        assert_eq!(km.resolve(Reading, &Chord::key(Enter)), Some(FollowLink));
        assert_eq!(km.resolve(Reading, &Chord::ch('f')), Some(LinkHints));
        assert_eq!(km.resolve(Reading, &Chord::ch('H')), Some(Back));
        assert_eq!(km.resolve(Reading, &Chord::ch('L')), Some(Forward));
        assert_eq!(
            km.resolve(Reading, &Chord::ctrl_ch('h')),
            Some(ReadingHistory)
        );

        // Tabs.
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('g', 't')),
            Some(NextTab)
        );
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('g', 'T')),
            Some(PrevTab)
        );
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('b', 'b')),
            Some(TabPicker)
        );

        // Views & tools.
        assert_eq!(km.resolve(Reading, &Chord::ch('t')), Some(Toc));
        assert_eq!(km.resolve(Reading, &Chord::ch('T')), Some(TalkToggle));
        assert_eq!(km.resolve(Reading, &Chord::ctrl_ch('t')), Some(CycleTheme));
        assert_eq!(km.resolve(Reading, &Chord::ch('/')), Some(Search));
        assert_eq!(km.resolve(Reading, &Chord::ch(':')), Some(CommandLine));
        assert_eq!(km.resolve(Reading, &Chord::ch('m')), Some(BookmarkToggle));
        assert_eq!(km.resolve(Reading, &Chord::ch('S')), Some(SaveOffline));
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('g', 'r')),
            Some(RandomArticle)
        );
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('r', 'l')),
            Some(ReadLater)
        );
        assert_eq!(km.resolve(Reading, &Chord::ctrl_ch('f')), Some(FindInPage));
        assert_eq!(km.resolve(Reading, &Chord::prefixed('g', 'h')), Some(Home));
        assert_eq!(km.resolve(Reading, &Chord::ch('q')), Some(CloseTab));
        assert_eq!(km.resolve(Reading, &Chord::ch('Q')), Some(Quit));
        assert_eq!(km.resolve(Reading, &Chord::ch('i')), Some(Info));

        // UX-6: these five previously worked as keypresses but had no
        // registry row at all (see each `Action` variant's own doc comment).
        assert_eq!(km.resolve(Reading, &Chord::ch('K')), Some(Peek));
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('z', 'z')),
            Some(IncognitoToggle)
        );
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('z', 'a')),
            Some(FoldToggle)
        );
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('z', 'M')),
            Some(FoldAll)
        );
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('z', 'R')),
            Some(UnfoldAll)
        );
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('g', 'K')),
            Some(JumpReferences)
        );
        // `Split` (Ctrl-w v) is deliberately unbound here — see its own doc
        // comment for why the Chord model can't name a Control-chord prefix.
        assert_eq!(km.label_for(Reading, Split), "\u{2014}");

        // Global reaches Reading and pickers.
        assert_eq!(km.resolve(Reading, &Chord::ch('?')), Some(Help));
        assert_eq!(km.resolve(Reading, &Chord::ctrl_ch('p')), Some(Palette));
        assert_eq!(km.resolve(Picker, &Chord::ch('?')), Some(Help));

        // Picker navigation.
        assert_eq!(km.resolve(Picker, &Chord::ch('j')), Some(MoveDown));
        assert_eq!(km.resolve(Picker, &Chord::key(Enter)), Some(Select));
        assert_eq!(km.resolve(Picker, &Chord::key(Esc)), Some(Close));
    }

    #[test]
    fn every_action_has_exactly_one_metadata_row() {
        // `meta()` would panic on a missing row; this exercises all of them
        // and asserts names are unique.
        let mut names = std::collections::HashSet::new();
        for m in COMMANDS {
            assert_eq!(meta(m.action).name, m.name);
            assert!(names.insert(m.name), "duplicate command name {}", m.name);
            assert!(!m.help.is_empty(), "{} has no help", m.name);
        }
    }

    /// UX-6: peek/incognito/fold/jump-references/split — and UX-5's
    /// argument-free `:`-only panels — previously had no registry row at
    /// all, so neither the palette (FR-CS-1) nor the generated `?`
    /// cheatsheet (FR-CS-4) ever mentioned them. Registering them (this
    /// chunk) is what makes both surface them now; this pins that they
    /// actually do, not just that the `Action` variant compiles.
    #[test]
    fn ux6_additions_are_reachable_from_the_palette_and_reading_help() {
        let km = Keymap::vim();
        for (query, action) in [
            ("Peek", Action::Peek),
            ("Toggle incognito", Action::IncognitoToggle),
            ("Fold/unfold", Action::FoldToggle),
            ("Fold all", Action::FoldAll),
            ("Unfold all", Action::UnfoldAll),
            ("Jump to references", Action::JumpReferences),
            ("Split view", Action::Split),
            ("Prefetch log", Action::PrefetchLogOpen),
            ("Interests", Action::InterestsOpen),
            ("Reading stats", Action::StatsOpen),
            ("Named sessions", Action::SessionsList),
        ] {
            let rows = palette_matches(&km, KeyContext::Reading, query);
            assert!(
                rows.iter().any(|r| r.action == action),
                "{query:?} should surface {action:?} in the palette: {rows:?}"
            );
        }

        let help = reading_help(&km);
        assert!(
            help.iter()
                .any(|r| r.key == "K" && r.help.contains("preview")),
            "Peek missing from reading help: {help:?}"
        );
        assert!(
            help.iter().any(|r| r.key == "zz"),
            "zz (incognito) missing from reading help: {help:?}"
        );
        assert!(
            help.iter().any(|r| r.key == "gK"),
            "gK (jump references) missing from reading help: {help:?}"
        );
        assert!(
            help.iter()
                .any(|r| r.help.contains("split") && r.key == "\u{2014}"),
            "split should appear, keyless, in reading help: {help:?}"
        );
    }

    /// UX-3: `r` reads as a plain one-key action in the generated help, but
    /// it is actually a two-keystroke chord (`main::handle_key`'s
    /// `pending_r` latch) — the help text itself must say so, since the
    /// registry's `Chord` model has no distinct "this is a prefix" column.
    #[test]
    fn research_help_documents_that_r_is_a_chord_not_a_plain_key() {
        let help = Action::Research.help();
        assert!(
            help.contains("rl"),
            "Research's help must mention the rl read-later chord: {help:?}"
        );
    }

    /// PRD FR-CS-3: a user keymap.toml adds/overrides bindings via the
    /// override layer, and those win over the base.
    #[test]
    fn user_keymap_toml_adds_and_overrides_bindings() {
        let mut km = Keymap::vim();
        let warnings = km.apply_user_toml(
            r#"
            [reading]
            J = "scroll-down"
            k = "scroll-bottom"

            [global]
            "Ctrl-e" = "cycle-theme"
            "#,
        );
        assert!(
            warnings.is_empty(),
            "clean file warns nothing: {warnings:?}"
        );
        use Action::*;
        use KeyContext::Reading;
        // Added binding.
        assert_eq!(
            km.runtime_action(Reading, &Chord::ch('J')),
            Some(ScrollDown)
        );
        assert_eq!(km.resolve(Reading, &Chord::ch('J')), Some(ScrollDown));
        // Overridden binding wins over the base `k = scroll-up`.
        assert_eq!(km.resolve(Reading, &Chord::ch('k')), Some(ScrollBottom));
        // Global override reaches Reading.
        assert_eq!(km.resolve(Reading, &Chord::ctrl_ch('e')), Some(CycleTheme));
    }

    /// PRD FR-CS-3 leader key: the default is `,`, its built-in bindings
    /// resolve, and an unbound leader key resolves to `None`.
    #[test]
    fn leader_key_defaults_to_comma_with_builtin_bindings() {
        let km = Keymap::vim();
        assert_eq!(km.leader_key(), ',');
        assert_eq!(km.leader_action(&Chord::ch('t')), Some(Action::Toc));
        assert_eq!(km.leader_action(&Chord::ch('w')), Some(Action::WatchToggle));
        assert_eq!(km.leader_action(&Chord::ch('X')), None);
    }

    /// PRD FR-CS-3: a `keymap.toml` `[leader]` table adds/overrides leader
    /// bindings and `leader-key` sets the prefix key itself.
    #[test]
    fn keymap_toml_configures_leader_key_and_bindings() {
        let mut km = Keymap::vim();
        let warnings = km.apply_user_toml(
            r#"
            leader-key = ";"

            [leader]
            g = "stats-open"
            t = "today"
            "#,
        );
        assert!(warnings.is_empty(), "clean leader config: {warnings:?}");
        assert_eq!(km.leader_key(), ';');
        // Added binding.
        assert_eq!(km.leader_action(&Chord::ch('g')), Some(Action::StatsOpen));
        // Overrode the built-in `,t -> Toc` to `today`.
        assert_eq!(km.leader_action(&Chord::ch('t')), Some(Action::Today));
    }

    /// PRD FR-CS-3 (`:map`/`:unmap` substrate): a runtime override is what
    /// `runtime_action` surfaces, and `remove_override` takes it back out
    /// without disturbing the base binding.
    #[test]
    fn override_add_and_remove_round_trips() {
        use KeyContext::Reading;
        let mut km = Keymap::vim();
        assert_eq!(km.runtime_action(Reading, &Chord::ch('J')), None);
        km.apply_override(Reading, Chord::ch('J'), Action::ScrollBottom);
        assert_eq!(
            km.runtime_action(Reading, &Chord::ch('J')),
            Some(Action::ScrollBottom)
        );
        assert!(km.remove_override(Reading, &Chord::ch('J')));
        assert_eq!(km.runtime_action(Reading, &Chord::ch('J')), None);
        // Removing something that was never overridden reports false.
        assert!(!km.remove_override(Reading, &Chord::ch('J')));
        // A leader binding round-trips the same way.
        km.set_leader_binding(Chord::ch('q'), Action::Quit);
        assert_eq!(km.leader_action(&Chord::ch('q')), Some(Action::Quit));
        assert!(km.remove_leader_binding(&Chord::ch('q')));
        assert!(!km.remove_leader_binding(&Chord::ch('q')));
    }

    #[test]
    fn keymap_toml_warns_and_skips_bad_lines_without_crashing() {
        let mut km = Keymap::vim();
        let warnings = km.apply_user_toml(
            r#"
            [bogus]
            x = "scroll-down"

            [reading]
            "???nope" = "scroll-down"
            j = "not-a-command"
            "#,
        );
        assert_eq!(warnings.len(), 3, "each bad line warns once: {warnings:?}");
        // The base binding is untouched by the rejected lines.
        assert_eq!(
            km.resolve(KeyContext::Reading, &Chord::ch('j')),
            Some(Action::ScrollDown)
        );
    }

    /// PRD FR-CS-3: the emacs preset resolves its covered keys; quit/tab come
    /// from the shared base.
    #[test]
    fn emacs_preset_resolves_its_covered_keys() {
        let km = Keymap::emacs();
        use Action::*;
        use KeyContext::Reading;
        // Deltas (the override layer — also live at runtime).
        assert_eq!(
            km.runtime_action(Reading, &Chord::ctrl_ch('n')),
            Some(ScrollDown)
        );
        assert_eq!(km.resolve(Reading, &Chord::ctrl_ch('n')), Some(ScrollDown));
        assert_eq!(km.resolve(Reading, &Chord::ctrl_ch('v')), Some(PageDown));
        assert_eq!(km.resolve(Reading, &Chord::ctrl_ch('s')), Some(Search));
        // Quit and tab-switch inherited from the base.
        assert_eq!(km.resolve(Reading, &Chord::ch('Q')), Some(Quit));
        assert_eq!(
            km.resolve(Reading, &Chord::prefixed('g', 't')),
            Some(NextTab)
        );
        // vim default has no overrides, so it never changes default behavior.
        assert_eq!(Keymap::vim().runtime_action(Reading, &Chord::ch('j')), None);
    }

    /// PRD FR-CS-1: the palette filters to the current context and fuzzy-ranks.
    #[test]
    fn palette_filters_by_context_and_fuzzy_query() {
        let km = Keymap::vim();
        // "toc" matches the table-of-contents command in Reading.
        let rows = palette_matches(&km, KeyContext::Reading, "toc");
        assert!(
            rows.iter().any(|r| r.action == Action::Toc),
            "toc present: {rows:?}"
        );
        assert_eq!(rows[0].action, Action::Toc, "best match ranks first");
        // The matched row shows its current key binding.
        assert_eq!(rows[0].key, "t");

        // A picker context offers no reading-only command like Toc.
        let picker = palette_matches(&km, KeyContext::Picker, "toc");
        assert!(!picker.iter().any(|r| r.action == Action::Toc));

        // Every returned row is palette-enabled and context-applicable.
        for r in palette_matches(&km, KeyContext::Reading, "") {
            assert!(meta(r.action).in_palette);
            assert!(r.action.applies_in(KeyContext::Reading));
        }
    }

    /// PRD FR-CS-4: the reading cheatsheet is generated from the registry +
    /// keymap and lists the right commands with their live keys.
    #[test]
    fn reading_help_is_generated_from_registry_and_keymap() {
        let km = Keymap::vim();
        let rows = reading_help(&km);
        // A representative binding: scroll-down shows j and the arrow.
        let scroll = rows
            .iter()
            .find(|r| r.help == Action::ScrollDown.help())
            .expect("scroll-down row present");
        assert!(scroll.key.contains('j'), "shows the j key: {}", scroll.key);
        // Remapping surfaces immediately (no hand-maintained drift).
        let mut km2 = Keymap::vim();
        km2.apply_override(KeyContext::Reading, Chord::ch('J'), Action::ScrollDown);
        let rows2 = reading_help(&km2);
        let scroll2 = rows2
            .iter()
            .find(|r| r.help == Action::ScrollDown.help())
            .unwrap();
        assert!(
            scroll2.key.contains('J'),
            "remap shows up in help: {}",
            scroll2.key
        );
    }

    #[test]
    fn chord_labels_and_parsing_round_trip_for_the_common_forms() {
        for (s, chord) in [
            ("j", Chord::ch('j')),
            ("Ctrl-d", Chord::ctrl_ch('d')),
            ("Space", Chord::ch(' ')),
            ("Tab", Chord::key(ChordCode::Tab)),
            ("gg", Chord::prefixed('g', 'g')),
            ("gT", Chord::prefixed('g', 'T')),
            ("Enter", Chord::key(ChordCode::Enter)),
        ] {
            assert_eq!(Chord::parse(s), Some(chord.clone()), "parse {s:?}");
        }
        assert_eq!(Chord::ctrl_ch('d').label(), "Ctrl-d");
        assert_eq!(Chord::prefixed('g', 'g').label(), "gg");
        assert_eq!(Chord::ch(' ').label(), "Space");
        assert_eq!(Chord::parse(""), None);
    }

    #[test]
    fn from_key_maps_crossterm_events_to_single_key_chords() {
        assert_eq!(
            Chord::from_key(KeyCode::Char('j'), KeyModifiers::NONE),
            Some(Chord::ch('j'))
        );
        assert_eq!(
            Chord::from_key(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(Chord::ctrl_ch('d'))
        );
        assert_eq!(
            Chord::from_key(KeyCode::Enter, KeyModifiers::CONTROL),
            Some(Chord::ctrl_code(ChordCode::Enter))
        );
    }
}
