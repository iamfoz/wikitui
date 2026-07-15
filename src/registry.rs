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
        contexts: &[KeyContext::Global],
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
        help: "cite this page and its sources",
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

        // A two-character all-lowercase-prefix chord (`gg`, `gt`, `bb`, `rl`)
        // — only the established latch characters may lead one.
        if !ctrl {
            let chars: Vec<char> = rest.chars().collect();
            if chars.len() == 2 && matches!(chars[0], 'g' | 'b' | 'r') {
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

/// A configurable keymap: a `base` table (the built-in preset, mirroring
/// `handle_key`'s hardcoded arms) plus an `overrides` layer (the emacs
/// preset's deltas and any user `keymap.toml`). Runtime dispatch reads only
/// the overrides; the palette and help read the merged view.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    base: HashMap<KeyContext, HashMap<Chord, Action>>,
    overrides: HashMap<KeyContext, HashMap<Chord, Action>>,
}

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
        Keymap::bind(b, Reading, Chord::ch('Q'), Quit);

        // Picker-generic navigation.
        Keymap::bind(b, Picker, Chord::ch('j'), MoveDown);
        Keymap::bind(b, Picker, Chord::key(Down), MoveDown);
        Keymap::bind(b, Picker, Chord::ch('k'), MoveUp);
        Keymap::bind(b, Picker, Chord::key(Up), MoveUp);
        Keymap::bind(b, Picker, Chord::key(Enter), Select);
        Keymap::bind(b, Picker, Chord::key(Esc), Close);

        Keymap {
            base,
            overrides: HashMap::new(),
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
            let Some(ctx) = KeyContext::from_toml_name(section) else {
                warnings.push(format!(
                    "keymap.toml: unknown context [{section}] — ignored (try: global, reading, startpage, picker, search)"
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
    Action::Back,
    Action::Forward,
    Action::BackStackPicker,
    Action::ReadingHistory,
    Action::NextTab,
    Action::PrevTab,
    Action::TabPicker,
    Action::ReopenClosedTab,
    Action::CloseTab,
    Action::Toc,
    Action::CycleTheme,
    Action::TalkToggle,
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
