//! The `:` ex-style command line (PRD FR-CS-2, MVP slice): a small set of
//! named commands with short aliases, mapping onto features that already
//! exist. Argument completion and ranges are later phases; unknown
//! commands produce an error message listing what's available.

use crate::cite::CiteStyle;
use crate::saved::Tier;
use crate::session;
use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `:open <title>` / `:o` — open an article (also accepts URLs and
    /// lang-prefixed titles, same grammar as the CLI TITLE argument).
    Open(String),
    /// `:lang` (bare) — opens the language-switcher picker for the article
    /// on screen (PRD FR-ML-1). `:lang <code>` disambiguates (PRD FR-ML-2):
    /// if the current article has a cached langlink for `code`, switches
    /// straight to it (a real navigation — see `main::set_or_switch_lang`);
    /// otherwise sets `code` as the default language for new
    /// searches/opens, this command's pre-FR-ML-1 meaning.
    Lang(Option<String>),
    /// `:theme <name>` — switch the color theme.
    Theme(String),
    /// `:style <name>` — switch the citation style.
    Style(String),
    /// `:library` / `:lib` — open the saved bibliography.
    Library,
    /// `:research` — open the citation picker for the current article.
    Research,
    /// `:toc` — open the table of contents.
    Toc,
    /// `:export [style]` — export the bibliography (optionally switching
    /// style first).
    Export(Option<String>),
    /// `:help` / `:h` — the help overlay.
    Help,
    /// `:config reload` — re-read the config file and live-apply
    /// theme/measure/ambiguous_wide (PRD §6.7). Network/storage settings
    /// are documented as restart-only, so this deliberately leaves them
    /// alone; the same reload also fires on SIGHUP.
    ConfigReload,
    /// `:tab close` — close the active tab (PRD FR-TB-1). Closing the last
    /// tab quits.
    TabClose,
    /// `:tab new [title]` — open a fresh tab, switching to it; loads `title`
    /// if given, else the welcome screen.
    TabNew(Option<String>),
    /// `:tabs` — open the fuzzy tab picker (same view as `bb`).
    Tabs,
    /// `:bookmarks` — open the bookmark picker (same view as `B`).
    Bookmarks,
    /// `:bookmarks export <format> [path]` (PRD FR-BM-4): `format` is one of
    /// `bookmark_export::FORMATS`; `path` overrides the default timestamped
    /// location under the data dir's `exports/`.
    BookmarksExport {
        format: String,
        path: Option<String>,
    },
    /// `:readlater` — open the read-later queue view.
    ReadLater,
    /// `:history` (PRD FR-HS-1) — bare form opens the fuzzy reading-history
    /// picker (same view as Ctrl-h), mirroring `:bookmarks`' "bare opens the
    /// picker" convention.
    History,
    /// `:history clear today|all` (PRD FR-HS-4): `today` removes visits
    /// opened since local midnight, `all` empties the whole history.
    /// Clearing a single article stays picker-only (`d`), not an
    /// ex-command argument.
    HistoryClear(HistoryClearScope),
    /// `:save …` (PRD FR-OFF-4..7): pin the current article at a tier, run a
    /// bulk save, or export a saved/current page. See [`SaveSpec`].
    Save(SaveSpec),
    /// `:saved` — open the saved-pages browser (PRD §5.7 / FR-OFF-4).
    Saved,
    /// `:fetch-queue` — drain the offline "queue for fetch when online" list
    /// (PRD FR-OFF-6): fetch every queued title into the cache now.
    FetchQueue,
    /// `:set <key>=<value>` — session-global runtime render override. Handles
    /// `images=on|off` (PRD FR-TH-7), `prefetch=on|off` (FR-PF-6 kill
    /// switch), and the FR-PC-1 typography/spacing knobs (`text_align`,
    /// `margin`, `paragraph_spacing`, `line_spacing`, `word_spacing`)
    /// alongside `measure`/`ambiguous_width`/`reading_wpm`.
    Set { key: String, value: String },
    /// `:set-tab <key>=<value>` / `:set-tab <key>=` (PRD FR-PC-4): a
    /// tab-local override for one of `TAB_SCOPED_KEYS`, applying only to the
    /// active tab. `value: None` is the `key=` (empty right-hand side)
    /// spelling that clears the override, falling back to whatever `:set`
    /// (or config) says session-globally.
    SetTab { key: String, value: Option<String> },
    /// `:prefetch-log` (PRD FR-PF-4): open the transparency/debug panel of
    /// recent prefetch actions, their reasons, status, and budget state.
    PrefetchLog,
    /// `:interests` (PRD FR-PF-3 / FR-PF-4): open the interest-model inspector
    /// — top topic affinities, decay half-life, on/off state, morelike seeds.
    Interests,
    /// `:not-interested` (PRD FR-PF-3): strongly down-weight the current
    /// article's topics in the interest model. Same action as the keybinding.
    NotInterested,
    /// `:stats` (PRD FR-PC-3): open the local reading-stats view (articles
    /// read, time, streaks, topic distribution).
    Stats,
    /// `:start` (PRD FR-DL-1) — return the active tab to the start page
    /// (same action as the `gh` "home" keybinding).
    Start,
    /// `:today` (PRD FR-DL-2) — open the on-this-day panel.
    Today,
    /// `:random` / `:random good` (PRD FR-SR-5) — open a random article, or
    /// one filtered to assessment ≥ GA. Same action as `gr` (Appendix B);
    /// the `good` variant has no dedicated keybinding, `:random good` only.
    Random(RandomSpec),
    /// `:related` (PRD FR-SR-6) — open the Related panel for the current
    /// article (`morelike:{title}`). Same action as `gR`.
    Related,
    /// `:talk` (PRD FR-ACC-5) — flip the active tab between an article and
    /// its talk page. Same action as `T`.
    Talk,
    /// `:info` (PRD §10 / Appendix B) — open the article-attribution
    /// overlay for the current article (title, canonical URL, revid,
    /// license, history permalink). Same action as `i`.
    Info,
    /// `:login` (PRD FR-ACC-1, §5.9) — start the OAuth 2.0 PKCE login. Bare
    /// `:login` runs the loopback flow (opens the browser, captures the
    /// redirect on a local listener); `:login paste` runs the manual
    /// code-paste fallback for when loopback URIs are rejected or no browser
    /// is reachable.
    Login(LoginMode),
    /// `:logout` (PRD FR-ACC-9) — revoke locally (delete stored tokens) and
    /// link to `Special:OAuthManageMyGrants` for server-side revocation.
    Logout,
    /// `:enable-editing` (PRD FR-ACC-8) — opt into typo-fix editing: flips the
    /// `editing_enabled` config gate on for the session AND re-runs the OAuth
    /// flow requesting the extra `editpage` grant (the *separate* opt-in the
    /// ordinary read-only login never asks for). Both halves are required
    /// before any `:edit` is permitted.
    EnableEditing,
    /// `:edit [summary]` (PRD FR-ACC-8) — fix a typo in the focused sentence
    /// of the current MAIN-namespace article: fetch its wikitext, locate the
    /// sentence, open `$EDITOR`, preview the diff, and save (minor, with a
    /// conflict-detecting base revid) on confirmation. Gated by the editing
    /// double opt-in; refused on non-main-namespace pages. The optional
    /// argument is appended to the automatic "Typo fix via wikitui" summary.
    Edit(Option<String>),
    /// `:watchlist` (PRD FR-ACC-2, logged in only) — the watched-pages list
    /// plus the "what changed" activity feed. Same view as `gW`.
    Watchlist,
    /// `:notifications` (PRD FR-ACC-3, logged in only) — the Echo alerts/
    /// messages pane.
    Notifications,
    /// `:contribs [username]` (PRD FR-ACC-4) — recent edits; the logged-in
    /// user's own by default, or any given username (works logged out too).
    Contribs(Option<String>),
    /// `:prefs` (PRD FR-ACC-7, logged in only) — the read-only preferences
    /// card.
    Prefs,
    /// `:sync` (PRD FR-BM-5/6, logged in only, opt-in): a two-way Reading
    /// List reconcile (`action=readinglists`) plus applying the watchlist-
    /// mirror tag (`action=watch`) — two distinct backends, reported
    /// separately, never conflated into one count (see `main::cmd_sync`).
    Sync,
    /// `:mirror-watchlist` (PRD FR-BM-6 alone): applies just the watch-
    /// mirror half of `:sync` — e.g. right after tagging/untagging a
    /// bookmark with the configured `watchlist_mirror_tag`, without
    /// touching Reading List sync.
    MirrorWatchlist,
    /// `:vsplit` / `:vsp` (PRD FR-TB-4, also `Ctrl-w v`) — split the content
    /// area into two side-by-side panes showing the current article.
    VSplit,
    /// `:only` / `:close` (PRD FR-TB-4, also `Ctrl-w c`) — collapse an active
    /// split back to a single pane.
    Only,
    /// `:bilingual` / `:bi` (PRD FR-ML-3) — open the current article in a split
    /// alongside the same article in another language (via langlinks).
    Bilingual,
    /// `:search-offline` (PRD FR-SR-7): toggles `App::force_offline_search`
    /// — while on, the search box goes straight to the local saved/cached
    /// index instead of the API, even while online. Independent of the
    /// automatic offline/API-failure fallback, which applies regardless of
    /// this toggle.
    SearchOffline,
    /// `:wiki` (bare) — opens the wiki-switcher picker (PRD FR-ML-4):
    /// Wikipedia and the four sister projects. `:wiki <name>` switches
    /// directly, without the picker — `name` may be a sister project or any
    /// configured `[wiki.<name>]` site (FR-ML-5); which names actually
    /// resolve is checked at execution time (`main::switch_wiki`), the same
    /// split `:lang <code>` uses for its own deeper validation.
    Wiki(Option<String>),
    /// `:trail [all|days N]` (PRD FR-HS-3): opens the wander-graph tree
    /// view. Bare `:trail` uses [`TrailScope::Session`] (this run's own
    /// history) — "session-scope is the natural wander graph."
    Trail(TrailScope),
    /// `:trail dag [all|days N]` (PRD FR-HS-3 v2): the same scope grammar as
    /// [`Command::Trail`], but opens the true-DAG view
    /// (`trail::dag_from_graph`) instead of the tree — a node with more than
    /// one referrer shows every parent, not one plus an "also from" note.
    /// The tree stays the default; this is the opt-in alternate view.
    TrailDag(TrailScope),
    /// `:trail export md|dot|mermaid [path]` (PRD FR-HS-3): exports the
    /// trail — always the session scope, regardless of what scope a
    /// currently-open `:trail` view is showing (see `App::export_trail`'s
    /// doc comment for why). `format` is one of `trail_export::FORMATS`;
    /// `path` overrides the default timestamped location under the data
    /// dir's `exports/`.
    TrailExport {
        format: String,
        path: Option<String>,
    },
    /// `:mksession <name>` (PRD FR-TB-5): saves the current tab set as a
    /// named session (`session::save`), alongside the continuous
    /// auto-restore snapshot. `name` is validated at parse time — same
    /// "fail fast, before any I/O" pattern `:lang <code>`'s shape check
    /// already uses.
    MkSession(String),
    /// `:session <name>` — switches to a named session at runtime,
    /// replacing every currently open tab (see `main::switch_to_named_
    /// session`'s doc comment for why replace, not merge, was chosen).
    SessionSwitch(String),
    /// `:sessions` (bare) — lists every saved named session by name.
    Sessions,
    /// `:tts` / `:speak` (PRD FR-PC-2): play/stop TTS playback. `:speak` is
    /// a full alias for the whole command (both its bare and `stop` forms),
    /// not just a synonym for the bare spelling — neither reads more
    /// "canonical" than the other, and Appendix B pins no default keybinding
    /// for this v1.x feature.
    Tts(TtsSpec),
    /// `:run <name>` (PRD FR-CS-5) — explicitly runs a configured macro.
    /// Bare `:<macro-name>` is the shorthand (resolved in `main`'s command
    /// dispatch, after the ordinary command grammar reports it unknown —
    /// see that call site's doc comment for why macro names can't be
    /// validated inside this parser itself).
    RunMacro(String),
    /// `:game ...` (PRD FR-DL-6) — the wiki-walk game; see [`GameSpec`].
    Game(GameSpec),
    /// `:xyzzy` (PRD FR-DL-8) — the classic. "Nothing happens." unless
    /// `pro = true`, in which case it's treated exactly like any other
    /// unrecognized command (`main::execute_command`'s doc comment explains
    /// why that check lives at execution time, not here).
    Xyzzy,
    /// `:q` / `:quit` — exit.
    Quit,
}

/// `:game`'s three forms (PRD FR-DL-6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameSpec {
    /// Bare `:game` or `:game daily`: today's deterministic daily puzzle
    /// (`game::daily_puzzle`) — see that function's doc comment for why bare
    /// `:game` doesn't instead draw a fresh client-side-random pair.
    Daily,
    /// `:game <start> <goal>`: two whitespace-separated tokens (underscores
    /// stand in for spaces in a multi-word title, the same URL-ish
    /// convention the mock fixtures and `internal_title_from_href` already
    /// use) — already underscore-normalized to real titles here.
    Pair(String, String),
    /// `:game share`: (re)shows and clipboard-yanks the current/just-
    /// finished game's shareable result card (`game::share_card`).
    Share,
}

/// `:tts`/`:speak`'s two forms (PRD FR-PC-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsSpec {
    /// Bare `:tts`/`:speak`: play from the reading cursor onward.
    Play,
    /// `:tts stop`/`:speak stop`: halt playback.
    Stop,
}

/// `:trail`'s scope argument (PRD FR-HS-3). Kept separate from
/// `history::ClearRange` — this describes which visits to *include*, not a
/// deletion range, and (like `HistoryClearScope::Today`) resolving `Days`
/// into an actual cutoff happens at execution time (`App::open_trail`), not
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailScope {
    /// Bare `:trail`: this run's own session (`App::session_started_at`
    /// onward) — the PRD's "natural wander graph."
    Session,
    /// `:trail all`: the entire reading history, unbounded.
    All,
    /// `:trail days <N>`: every visit in the last `N` days.
    Days(u32),
}

/// `:login`'s two forms (PRD §5.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMode {
    /// Bare `:login`: browser + loopback-listener capture (the primary path).
    Loopback,
    /// `:login paste`: the manual code-paste fallback (§5.9; SP-2-unverified
    /// for OAuth 2.0).
    Paste,
}

/// `:random`'s two documented forms (PRD FR-SR-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RandomSpec {
    /// Bare `:random` (and `gr`): any article in the main namespace.
    Any,
    /// `:random good`: filtered to assessment ≥ GA, batching 10 candidates
    /// against one `pageassessments` query.
    Good,
}

/// `:history clear`'s two documented scopes (PRD FR-HS-4). Kept separate
/// from `history::ClearRange` — that type's `Since`/`Before` cutoffs are
/// unix timestamps, which parsing has no business computing; resolving
/// `Today` into an actual cutoff happens at execution time
/// (`App::clear_history`), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryClearScope {
    Today,
    All,
}

/// What `:save` was asked to do (PRD FR-OFF-4..7). Kept as parsed data so
/// `main.rs` performs the network/storage work with the handles it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveSpec {
    /// `:save` / `:save t0|t1|t2` (and the `S` key) — pin the current article
    /// at the given depth. `S` and a bare `:save` default to `Tier::T0`.
    Current(Tier),
    /// `:save tag <name>` — bulk-save every bookmark carrying `<name>`.
    Tag(String),
    /// `:save category <Cat>` — bulk-save a category's members (depth 1).
    Category(String),
    /// `:save tabs` — bulk-save every open tab's article.
    Tabs,
    /// `:save export md|txt|html [path]` — export a saved (or the current)
    /// page with the §10 attribution footer.
    Export {
        format: String,
        path: Option<String>,
    },
}

/// The saved-page export formats (`saved_export::FORMATS`), duplicated as a
/// parse-time constant so `command` doesn't depend on the render module.
const SAVE_EXPORT_FORMATS: [&str; 3] = ["md", "txt", "html"];

/// PRD FR-PC-4: the `:set` keys `:set-tab` also accepts — every render/
/// typography option that feeds `layout::LayoutOptions` (plus `images`,
/// which feeds the image box map alongside it). Deliberately excludes the
/// session-only keys (`theme`, `prefetch`, `mouse`, `animations`,
/// `hyperlinks`, `reading_wpm`) — none of those describe "how this one
/// article reads," so a per-tab override for them would have no coherent
/// meaning. Shared by the parser (this list gates which keys `:set-tab`
/// accepts) and quoted verbatim in its own error message.
pub const TAB_SCOPED_KEYS: [&str; 10] = [
    "measure",
    "images",
    "ambiguous_width",
    "text_align",
    "margin",
    "paragraph_spacing",
    "line_spacing",
    "word_spacing",
    "justify",
    "hyphenate",
];

/// Validates and normalizes one `:set`/`:set-tab` `key=value` pair's value,
/// shared by both commands so the bounds/spelling checks live in exactly one
/// place. `theme_name_is_known` is threaded through rather than closed over
/// so this stays a plain function callable from either parse arm.
fn validate_set_value(
    key: &str,
    value: &str,
    theme_name_is_known: &impl Fn(&str) -> bool,
) -> Result<String, String> {
    let on_off = |value: &str| -> Result<String, String> {
        if value == "on" || value == "off" {
            Ok(value.to_string())
        } else {
            Err(format!("{key} must be on or off (got {value:?})"))
        }
    };
    match key {
        // PRD FR-TH-7.
        "images" => on_off(value),
        // PRD FR-PF-6 kill switch.
        "prefetch" => on_off(value),
        // PRD FR-TB-4: sync-scroll the two panes of a split.
        "scrollbind" => on_off(value),
        // PRD FR-DL-4: citation-needed highlighting, default off.
        "show-cn" => on_off(value),
        // PRD FR-RD-9 (v1.x): full justification / soft hyphenation, both
        // default off. `:set`-session-wide or `:set-tab`-per-tab.
        "justify" => on_off(value),
        "hyphenate" => on_off(value),
        // PRD FR-TH-2: live theme switch, same validation as `:theme <name>`
        // and the config loader.
        "theme" => {
            if theme_name_is_known(value) {
                Ok(value.to_string())
            } else {
                Err(format!(
                    "unknown theme {value:?} — one of: {} (or a themes/*.toml name)",
                    Theme::NAMES.join(", ")
                ))
            }
        }
        // PRD FR-RD-9 / FR-PC-4: line measure, same 40..=200 bounds the
        // config loader enforces.
        "measure" => match value.parse::<u16>() {
            Ok(n) if (crate::config::MEASURE_MIN..=crate::config::MEASURE_MAX).contains(&n) => {
                Ok(n.to_string())
            }
            Ok(n) => Err(format!(
                "measure must be {}..={} (got {n})",
                crate::config::MEASURE_MIN,
                crate::config::MEASURE_MAX
            )),
            Err(_) => Err(format!("measure must be an integer (got {value:?})")),
        },
        // PRD FR-RD-10: East-Asian-Ambiguous width, 1 (narrow) or 2 (wide).
        "ambiguous_width" => {
            if value == "1" || value == "2" {
                Ok(value.to_string())
            } else {
                Err(format!("ambiguous_width must be 1 or 2 (got {value:?})"))
            }
        }
        // PRD FR-RD-11: reading-time WPM divisor, same
        // config-default-plus-runtime-override split as `measure`.
        "reading_wpm" => match value.parse::<u32>() {
            Ok(n)
                if (crate::config::READING_WPM_MIN..=crate::config::READING_WPM_MAX)
                    .contains(&n) =>
            {
                Ok(n.to_string())
            }
            Ok(n) => Err(format!(
                "reading_wpm must be {}..={} (got {n})",
                crate::config::READING_WPM_MIN,
                crate::config::READING_WPM_MAX
            )),
            Err(_) => Err(format!("reading_wpm must be an integer (got {value:?})")),
        },
        // PRD FR-NV-9: opt-in mouse capture.
        "mouse" => on_off(value),
        // PRD FR-ACS-4: no-motion mode.
        "animations" => {
            if value == "full" || value == "none" {
                Ok(value.to_string())
            } else {
                Err(format!("animations must be full or none (got {value:?})"))
            }
        }
        // PRD FR-RD-2 / SEC-2: OSC 8 hyperlink emission.
        "hyperlinks" => {
            if crate::hyperlink::HyperlinkMode::parse(value).is_some() {
                Ok(value.to_string())
            } else {
                Err(format!(
                    "hyperlinks must be auto, on, or off (got {value:?})"
                ))
            }
        }
        // PRD FR-PC-1: `center` (default) or `left` — see `layout::TextAlign`.
        "text_align" => {
            if crate::layout::TextAlign::parse(value).is_some() {
                Ok(value.to_string())
            } else {
                Err(format!("text_align must be center or left (got {value:?})"))
            }
        }
        // PRD FR-PC-1: extra left margin in cells.
        "margin" => match value.parse::<u16>() {
            Ok(n) if n <= crate::config::MARGIN_MAX => Ok(n.to_string()),
            Ok(n) => Err(format!(
                "margin must be 0..={} (got {n})",
                crate::config::MARGIN_MAX
            )),
            Err(_) => Err(format!("margin must be an integer (got {value:?})")),
        },
        // PRD FR-PC-1: blank rows between blocks (0 tight, 1 default, 2+ airy).
        "paragraph_spacing" => match value.parse::<u8>() {
            Ok(n) if n <= crate::config::PARAGRAPH_SPACING_MAX => Ok(n.to_string()),
            Ok(n) => Err(format!(
                "paragraph_spacing must be 0..={} (got {n})",
                crate::config::PARAGRAPH_SPACING_MAX
            )),
            Err(_) => Err(format!(
                "paragraph_spacing must be an integer (got {value:?})"
            )),
        },
        // PRD FR-PC-1: blank rows after every wrapped line — no literal
        // "1.5"; see `layout::LayoutOptions::line_spacing`'s doc comment.
        "line_spacing" => match value.parse::<u8>() {
            Ok(n) if n <= crate::config::LINE_SPACING_MAX => Ok(n.to_string()),
            Ok(n) => Err(format!(
                "line_spacing must be 0..={} (got {n})",
                crate::config::LINE_SPACING_MAX
            )),
            Err(_) => Err(format!("line_spacing must be an integer (got {value:?})")),
        },
        // PRD FR-PC-1: extra inter-word gap width, the only "spacing" a cell
        // grid honestly allows.
        "word_spacing" => match value.parse::<u8>() {
            Ok(n) if n <= crate::config::WORD_SPACING_MAX => Ok(n.to_string()),
            Ok(n) => Err(format!(
                "word_spacing must be 0..={} (got {n})",
                crate::config::WORD_SPACING_MAX
            )),
            Err(_) => Err(format!("word_spacing must be an integer (got {value:?})")),
        },
        other => Err(format!(
            "unknown :set key {other:?} — try: theme, images=on|off, prefetch=on|off, scrollbind=on|off, show-cn=on|off, measure=N, ambiguous_width=1|2, reading_wpm=N, mouse=on|off, animations=full|none, hyperlinks=auto|on|off, text_align=center|left, margin=N, paragraph_spacing=N, line_spacing=N, word_spacing=N, justify=on|off, hyphenate=on|off"
        )),
    }
}

pub const USAGE: &str = "commands: open <title>, lang [<code>], theme <name>, style <name>, library, research, toc, export [style], tab close|new [title], tabs, bookmarks [export md|html|json|netscape [path]], readlater, history [clear today|all], save [t0|t1|t2|tag <t>|category <c>|tabs|export md|txt|html [path]], saved, fetch-queue, prefetch-log, interests, not-interested, stats, start, today, random [good], related, talk, info, set theme=<name>|images=on|off|prefetch=on|off|show-cn=on|off|measure=N|ambiguous_width=1|2|reading_wpm=N|text_align=center|left|margin=N|paragraph_spacing=N|line_spacing=N|word_spacing=N|justify=on|off|hyphenate=on|off, set-tab measure=N|images=on|off|ambiguous_width=1|2|text_align=center|left|margin=N|paragraph_spacing=N|line_spacing=N|word_spacing=N|justify=on|off|hyphenate=on|off (or set-tab key= to reset), config reload, vsplit, only, bilingual, wiki [<name>], set scrollbind, set show-cn, watchlist, notifications, contribs [username], prefs, enable-editing, edit [summary], sync, mirror-watchlist, search-offline, trail [all|days N|export md|dot|mermaid [path]], mksession <name>, session <name>, sessions, tts [stop], speak [stop], run <macro>, game [daily|share|<start> <goal>], xyzzy, help, quit";

/// Parses one `:` command line. `user_theme_names` are accepted alongside
/// the six built-ins for `:theme <name>` and `:set theme=<name>` (PRD
/// FR-TH-1's "a user theme's name joins `Theme::NAMES`-equivalent
/// resolution") — exactly the same names `theme::resolve_named` accepts at
/// theme-*application* time; this is that requirement's *validation* half.
/// Pass an empty slice when no user themes are loaded (or none are relevant,
/// as in every test below that only exercises built-in names).
pub fn parse_with_user_themes(input: &str, user_theme_names: &[String]) -> Result<Command, String> {
    let theme_name_is_known = |s: &str| -> bool {
        Theme::by_name(s).is_some() || user_theme_names.iter().any(|n| n == s)
    };
    let input = input.trim();
    let (name, arg) = match input.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (input, ""),
    };

    let require_arg = |what: &str| -> Result<String, String> {
        if arg.is_empty() {
            Err(format!("usage: :{name} <{what}>"))
        } else {
            Ok(arg.to_string())
        }
    };

    match name {
        "open" | "o" => Ok(Command::Open(require_arg("title")?)),
        // Bare `:lang` opens the switcher picker (PRD FR-ML-1); `:lang
        // <code>` keeps the code-shape validation the picker's own Enter
        // key bypasses (it already knows the code is real, from a langlink).
        "lang" => {
            if arg.is_empty() {
                Ok(Command::Lang(None))
            } else if crate::target::is_lang_code(arg) {
                Ok(Command::Lang(Some(arg.to_string())))
            } else {
                Err(format!(
                    "{arg:?} doesn't look like a language code (e.g. en, de, zh-yue)"
                ))
            }
        }
        // Bare `:wiki` opens the picker (PRD FR-ML-4); `:wiki <name>`
        // switches directly. Unlike `:lang`, there's no shape check here —
        // a wiki name has no fixed format the way a language code does, so
        // "known or not" is entirely `main::switch_wiki`'s call.
        // PRD FR-SR-7: no argument — a bare toggle, same shape as `:set
        // scrollbind`'s on/off flip, except this one needs no explicit
        // on/off spelling since there's only one thing to flip.
        "search-offline" | "searchoffline" => Ok(Command::SearchOffline),
        "wiki" => {
            if arg.is_empty() {
                Ok(Command::Wiki(None))
            } else {
                Ok(Command::Wiki(Some(arg.to_string())))
            }
        }
        // `:trail` (PRD FR-HS-3): bare opens the session-scoped tree view;
        // `all`/`days <N>` widen the scope; `dag [all|days N]` opens the v2
        // true-DAG view instead of the tree, over the same scope grammar;
        // `export md|dot|mermaid [path]` writes it out — same two-level
        // `sub`/`rest` split as `:bookmarks export`/`:save export`.
        "trail" => {
            let (sub, rest) = match arg.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (arg, ""),
            };
            match sub {
                "" => Ok(Command::Trail(TrailScope::Session)),
                "all" => Ok(Command::Trail(TrailScope::All)),
                "days" => {
                    if rest.is_empty() {
                        return Err("usage: :trail days <N>".to_string());
                    }
                    match rest.parse::<u32>() {
                        Ok(0) => Err("trail days must be at least 1".to_string()),
                        Ok(n) => Ok(Command::Trail(TrailScope::Days(n))),
                        Err(_) => Err(format!("trail days must be an integer (got {rest:?})")),
                    }
                }
                // PRD FR-HS-3 v2: `dag` takes the identical scope grammar
                // `:trail` itself does, one level deeper (`dag`, `dag all`,
                // `dag days <N>`) — never composed with `export` (a DAG
                // export doesn't exist; DOT/Mermaid already render the full
                // graph regardless of which view is on screen).
                "dag" => {
                    let (dag_sub, dag_rest) = match rest.split_once(char::is_whitespace) {
                        Some((s, r)) => (s, r.trim()),
                        None => (rest, ""),
                    };
                    match dag_sub {
                        "" => Ok(Command::TrailDag(TrailScope::Session)),
                        "all" => Ok(Command::TrailDag(TrailScope::All)),
                        "days" => {
                            if dag_rest.is_empty() {
                                return Err("usage: :trail dag days <N>".to_string());
                            }
                            match dag_rest.parse::<u32>() {
                                Ok(0) => Err("trail days must be at least 1".to_string()),
                                Ok(n) => Ok(Command::TrailDag(TrailScope::Days(n))),
                                Err(_) => {
                                    Err(format!("trail days must be an integer (got {dag_rest:?})"))
                                }
                            }
                        }
                        other => Err(format!(
                            "unknown trail dag subcommand {other:?} — try: trail dag, trail dag all, trail dag days <N>"
                        )),
                    }
                }
                "export" => {
                    let (format, path) = match rest.split_once(char::is_whitespace) {
                        Some((f, p)) => (f, Some(p.trim()).filter(|p| !p.is_empty())),
                        None => (rest, None),
                    };
                    if format.is_empty() {
                        return Err("usage: :trail export md|dot|mermaid [path]".to_string());
                    }
                    if !crate::trail_export::FORMATS.contains(&format) {
                        return Err(format!(
                            "unknown export format {format:?} — one of: {}",
                            crate::trail_export::FORMATS.join(", ")
                        ));
                    }
                    Ok(Command::TrailExport {
                        format: format.to_string(),
                        path: path.map(str::to_string),
                    })
                }
                other => Err(format!(
                    "unknown trail subcommand {other:?} — try: trail, trail all, trail days <N>, trail dag, trail export md|dot|mermaid [path]"
                )),
            }
        }
        "theme" => {
            let theme = require_arg("name")?;
            if theme_name_is_known(&theme) {
                Ok(Command::Theme(theme))
            } else {
                Err(format!(
                    "unknown theme {theme:?} — one of: {} (or a themes/*.toml name)",
                    Theme::NAMES.join(", ")
                ))
            }
        }
        "style" => {
            let style = require_arg("name")?;
            if CiteStyle::by_name(&style).is_some() {
                Ok(Command::Style(style))
            } else {
                Err(format!(
                    "unknown citation style {style:?} — one of: {}",
                    CiteStyle::NAMES.join(", ")
                ))
            }
        }
        "config" => match arg {
            "reload" => Ok(Command::ConfigReload),
            "" => Err("usage: :config reload".to_string()),
            other => Err(format!(
                "unknown config subcommand {other:?} — try: config reload"
            )),
        },
        // `:tab close` / `:tab new [title]` (PRD FR-TB-1). `close!` is
        // accepted as an alias for `close` — the exclamation-force idiom from
        // Appendix B / FR-CS-2, harmless here since a tab close needs no
        // confirmation.
        "tab" => {
            let (sub, rest) = match arg.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (arg, ""),
            };
            match sub {
                "close" | "close!" => Ok(Command::TabClose),
                "new" => Ok(Command::TabNew(
                    (!rest.is_empty()).then(|| rest.to_string()),
                )),
                "" => Err("usage: :tab close | :tab new [title]".to_string()),
                other => Err(format!(
                    "unknown tab subcommand {other:?} — try: tab close, tab new [title]"
                )),
            }
        }
        "tabs" => Ok(Command::Tabs),
        // `:bookmarks` alone opens the picker; `:bookmarks export
        // md|html|json|netscape [path]` exports it (PRD FR-BM-4).
        "bookmarks" => {
            let (sub, rest) = match arg.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (arg, ""),
            };
            match sub {
                "" => Ok(Command::Bookmarks),
                "export" => {
                    let (format, path) = match rest.split_once(char::is_whitespace) {
                        Some((f, p)) => (f, Some(p.trim()).filter(|p| !p.is_empty())),
                        None => (rest, None),
                    };
                    if format.is_empty() {
                        return Err(
                            "usage: :bookmarks export md|html|json|netscape [path]".to_string()
                        );
                    }
                    if !crate::bookmark_export::FORMATS.contains(&format) {
                        return Err(format!(
                            "unknown export format {format:?} — one of: {}",
                            crate::bookmark_export::FORMATS.join(", ")
                        ));
                    }
                    Ok(Command::BookmarksExport {
                        format: format.to_string(),
                        path: path.map(str::to_string),
                    })
                }
                other => Err(format!(
                    "unknown bookmarks subcommand {other:?} — try: bookmarks, bookmarks export md|html|json|netscape [path]"
                )),
            }
        }
        "readlater" => Ok(Command::ReadLater),
        // `:history` alone opens the picker; `:history clear today|all`
        // clears it (PRD FR-HS-1/4).
        "history" => {
            let (sub, rest) = match arg.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (arg, ""),
            };
            match sub {
                "" => Ok(Command::History),
                "clear" => match rest {
                    "all" => Ok(Command::HistoryClear(HistoryClearScope::All)),
                    "today" => Ok(Command::HistoryClear(HistoryClearScope::Today)),
                    "" => Err("usage: :history clear today|all".to_string()),
                    other => Err(format!(
                        "unknown history clear scope {other:?} — one of: today, all"
                    )),
                },
                other => Err(format!(
                    "unknown history subcommand {other:?} — try: history, history clear today|all"
                )),
            }
        }
        // `:save` (PRD FR-OFF-4..7). Bare / `t0`/`t1`/`t2` pin the current
        // article; `tag`/`category`/`tabs` bulk-save; `export` writes a file.
        "save" => {
            let (sub, rest) = match arg.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (arg, ""),
            };
            match sub {
                "" => Ok(Command::Save(SaveSpec::Current(Tier::T0))),
                "t0" | "t1" | "t2" => {
                    // A tier alone is the whole argument; reject trailing junk.
                    if !rest.is_empty() {
                        return Err(format!("usage: :save {sub}"));
                    }
                    Ok(Command::Save(SaveSpec::Current(Tier::parse(sub).unwrap())))
                }
                "tag" => {
                    if rest.is_empty() {
                        Err("usage: :save tag <tagname>".to_string())
                    } else {
                        Ok(Command::Save(SaveSpec::Tag(rest.to_string())))
                    }
                }
                "category" | "cat" => {
                    if rest.is_empty() {
                        Err("usage: :save category <Category>".to_string())
                    } else {
                        Ok(Command::Save(SaveSpec::Category(rest.to_string())))
                    }
                }
                "tabs" => Ok(Command::Save(SaveSpec::Tabs)),
                "export" => {
                    let (format, path) = match rest.split_once(char::is_whitespace) {
                        Some((f, p)) => (f, Some(p.trim()).filter(|p| !p.is_empty())),
                        None => (rest, None),
                    };
                    if format.is_empty() {
                        return Err("usage: :save export md|txt|html [path]".to_string());
                    }
                    if !SAVE_EXPORT_FORMATS.contains(&format) {
                        return Err(format!(
                            "unknown export format {format:?} — one of: {}",
                            SAVE_EXPORT_FORMATS.join(", ")
                        ));
                    }
                    Ok(Command::Save(SaveSpec::Export {
                        format: format.to_string(),
                        path: path.map(str::to_string),
                    }))
                }
                other => Err(format!(
                    "unknown save subcommand {other:?} — try: save [t0|t1|t2], save tag <t>, save category <c>, save tabs, save export md|txt|html [path]"
                )),
            }
        }
        "saved" => Ok(Command::Saved),
        "fetch-queue" | "fetchqueue" => Ok(Command::FetchQueue),
        "library" | "lib" => Ok(Command::Library),
        "research" => Ok(Command::Research),
        "toc" => Ok(Command::Toc),
        "export" => {
            if arg.is_empty() {
                Ok(Command::Export(None))
            } else if CiteStyle::by_name(arg).is_some() {
                Ok(Command::Export(Some(arg.to_string())))
            } else {
                Err(format!(
                    "unknown citation style {arg:?} — one of: {}",
                    CiteStyle::NAMES.join(", ")
                ))
            }
        }
        // `:set <key>=<value>` — session-global render/behavior overrides
        // (PRD FR-TH-7, FR-PF-6, FR-PC-1). Values are validated here so
        // execution (`main::execute_command`) is a pure apply.
        // PRD FR-TB-4: vim-style side-by-side panes and their close command.
        "vsplit" | "vsp" | "vs" => Ok(Command::VSplit),
        "only" | "close" => Ok(Command::Only),
        // PRD FR-ML-3: the bilingual side-by-side view.
        "bilingual" | "bi" => Ok(Command::Bilingual),
        "set" => {
            let assignment = require_arg("key=value")?;
            // PRD FR-TB-4 / FR-DL-4: vim-style boolean toggles without `=` —
            // `:set scrollbind` / `:set noscrollbind` and `:set show-cn` /
            // `:set noshow-cn` (the spelling the PRD uses), normalized to
            // the ordinary `key=value` shape.
            if !assignment.contains('=') {
                return match assignment.trim() {
                    "scrollbind" => Ok(Command::Set {
                        key: "scrollbind".to_string(),
                        value: "on".to_string(),
                    }),
                    "noscrollbind" => Ok(Command::Set {
                        key: "scrollbind".to_string(),
                        value: "off".to_string(),
                    }),
                    "show-cn" => Ok(Command::Set {
                        key: "show-cn".to_string(),
                        value: "on".to_string(),
                    }),
                    "noshow-cn" => Ok(Command::Set {
                        key: "show-cn".to_string(),
                        value: "off".to_string(),
                    }),
                    other => Err(format!(
                        "usage: :set key=value (e.g. images=on) — or :set scrollbind/show-cn (got {other:?})"
                    )),
                };
            }
            let (key, value) = assignment
                .split_once('=')
                .ok_or_else(|| "usage: :set images=on|off".to_string())?;
            let (key, value) = (key.trim(), value.trim());
            Ok(Command::Set {
                key: key.to_string(),
                value: validate_set_value(key, value, &theme_name_is_known)?,
            })
        }
        // `:set-tab <key>=<value>` / `:set-tab <key>=` (PRD FR-PC-4): the
        // same value grammar/bounds as `:set`, but restricted to
        // `TAB_SCOPED_KEYS` — the render/typography options that actually
        // feed a per-article layout, never the session-only ones (`theme`,
        // `prefetch`, `mouse`, `animations`, `hyperlinks`, `reading_wpm`).
        // An empty right-hand side (`key=`) clears the override instead of
        // setting one — `require_arg` still demands the `=` itself, so bare
        // `:set-tab measure` (no `=` at all) stays an error, matching `:set`.
        "set-tab" | "settab" => {
            let assignment = require_arg("key=value (or key= to reset)")?;
            let (key, value) = assignment
                .split_once('=')
                .ok_or_else(|| "usage: :set-tab measure=60 (or measure= to reset)".to_string())?;
            let (key, value) = (key.trim(), value.trim());
            if !TAB_SCOPED_KEYS.contains(&key) {
                return Err(format!(
                    "{key} is not a per-tab setting — try: {}",
                    TAB_SCOPED_KEYS.join(", ")
                ));
            }
            if value.is_empty() {
                Ok(Command::SetTab {
                    key: key.to_string(),
                    value: None,
                })
            } else {
                Ok(Command::SetTab {
                    key: key.to_string(),
                    value: Some(validate_set_value(key, value, &theme_name_is_known)?),
                })
            }
        }
        "prefetch-log" | "prefetchlog" => Ok(Command::PrefetchLog),
        // PRD FR-PF-3: the interest model inspector + its explicit signals.
        "interests" | "interest" => Ok(Command::Interests),
        "not-interested" | "notinterested" => Ok(Command::NotInterested),
        // PRD FR-PC-3: the reading-stats view.
        "stats" => Ok(Command::Stats),
        // PRD FR-DL-1/2.
        "start" => Ok(Command::Start),
        "today" => Ok(Command::Today),
        // PRD FR-SR-5: bare `:random` is any article; `:random good` filters
        // by assessment ≥ GA.
        "random" => match arg {
            "" => Ok(Command::Random(RandomSpec::Any)),
            "good" => Ok(Command::Random(RandomSpec::Good)),
            other => Err(format!(
                "unknown :random argument {other:?} — try: random, random good"
            )),
        },
        // PRD FR-SR-6.
        "related" => Ok(Command::Related),
        // PRD FR-ACC-5.
        "talk" => Ok(Command::Talk),
        // PRD §10 / Appendix B.
        "info" => Ok(Command::Info),
        // PRD FR-ACC-1 / §5.9: bare `:login` is the loopback flow, `:login
        // paste` the manual code-paste fallback.
        "login" => match arg {
            "" => Ok(Command::Login(LoginMode::Loopback)),
            "paste" => Ok(Command::Login(LoginMode::Paste)),
            other => Err(format!(
                "unknown :login argument {other:?} — try: login, login paste"
            )),
        },
        // PRD FR-ACC-9.
        "logout" => Ok(Command::Logout),
        // PRD FR-ACC-8: opt into editing (config gate + editpage re-auth).
        "enable-editing" | "enableediting" => Ok(Command::EnableEditing),
        // PRD FR-ACC-8: the gated typo-fix flow; the optional argument is a
        // free-text summary note appended to the automatic prefix.
        "edit" => Ok(Command::Edit((!arg.is_empty()).then(|| arg.to_string()))),
        // PRD FR-ACC-2.
        "watchlist" => Ok(Command::Watchlist),
        // PRD FR-ACC-3.
        "notifications" => Ok(Command::Notifications),
        // PRD FR-ACC-4: bare form defaults to the logged-in user (resolved at
        // execution time, since parsing has no session to consult); a given
        // username is used verbatim.
        "contribs" => Ok(Command::Contribs(
            (!arg.is_empty()).then(|| arg.to_string()),
        )),
        // PRD FR-ACC-7.
        "prefs" => Ok(Command::Prefs),
        // PRD FR-BM-5/6.
        "sync" => Ok(Command::Sync),
        "mirror-watchlist" | "mirrorwatchlist" => Ok(Command::MirrorWatchlist),
        // PRD FR-TB-5: named sessions. `:mksession`/`:session` both take a
        // name validated up front (`session::is_valid_session_name`) — the
        // same "letters, digits, - and _" shape the file path resolver
        // itself enforces, checked here too so a bad name reports a clear
        // parse error instead of a later, more confusing I/O failure.
        "mksession" => {
            let name = require_arg("name")?;
            if session::is_valid_session_name(&name) {
                Ok(Command::MkSession(name))
            } else {
                Err(format!(
                    "{name:?} is not a usable session name — letters, digits, - and _ only"
                ))
            }
        }
        "session" => {
            let name = require_arg("name")?;
            if session::is_valid_session_name(&name) {
                Ok(Command::SessionSwitch(name))
            } else {
                Err(format!(
                    "{name:?} is not a usable session name — letters, digits, - and _ only"
                ))
            }
        }
        "sessions" => Ok(Command::Sessions),
        // PRD FR-PC-2: `:tts`/`:speak` (bare) plays from the cursor; `stop`
        // halts playback.
        "tts" | "speak" => match arg {
            "" => Ok(Command::Tts(TtsSpec::Play)),
            "stop" => Ok(Command::Tts(TtsSpec::Stop)),
            other => Err(format!(
                "unknown :tts argument {other:?} — try: tts, tts stop"
            )),
        },
        // PRD FR-CS-5: `:run <macro-name>`.
        "run" => Ok(Command::RunMacro(require_arg("macro name")?)),
        // PRD FR-DL-6: `:game`/`:game daily` (today's puzzle), `:game
        // <start> <goal>` (exactly two whitespace-separated titles,
        // underscores standing in for spaces), or `:game share`.
        "game" => {
            let arg = arg.trim();
            if arg.is_empty() || arg.eq_ignore_ascii_case("daily") {
                Ok(Command::Game(GameSpec::Daily))
            } else if arg.eq_ignore_ascii_case("share") {
                Ok(Command::Game(GameSpec::Share))
            } else {
                let mut parts = arg.split_whitespace();
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(start), Some(goal), None) => Ok(Command::Game(GameSpec::Pair(
                        start.replace('_', " "),
                        goal.replace('_', " "),
                    ))),
                    _ => Err(format!(
                        "usage: game <start> <goal> (underscores for multi-word titles), or game daily, or game share (got {arg:?})"
                    )),
                }
            }
        }
        // PRD FR-DL-8: the classic easter egg.
        "xyzzy" => Ok(Command::Xyzzy),
        "help" | "h" => Ok(Command::Help),
        "q" | "quit" => Ok(Command::Quit),
        "" => Err(USAGE.to_string()),
        other => Err(format!("unknown command {other:?} — {USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test below exercises built-in theme names only, so this local
    /// shorthand (no user theme list to thread through 100+ call sites)
    /// stands in for `parse_with_user_themes(input, &[])` — the dedicated
    /// `..._with_user_themes` tests further down cover the non-empty case.
    fn parse(input: &str) -> Result<Command, String> {
        parse_with_user_themes(input, &[])
    }

    #[test]
    fn split_commands_parse_with_their_aliases() {
        assert_eq!(parse("vsplit"), Ok(Command::VSplit));
        assert_eq!(parse("vsp"), Ok(Command::VSplit));
        assert_eq!(parse("vs"), Ok(Command::VSplit));
        assert_eq!(parse("only"), Ok(Command::Only));
        assert_eq!(parse("close"), Ok(Command::Only));
        assert_eq!(parse("bilingual"), Ok(Command::Bilingual));
        assert_eq!(parse("bi"), Ok(Command::Bilingual));
    }

    #[test]
    fn set_scrollbind_toggle_parses_with_and_without_equals() {
        assert_eq!(
            parse("set scrollbind"),
            Ok(Command::Set {
                key: "scrollbind".to_string(),
                value: "on".to_string()
            })
        );
        assert_eq!(
            parse("set noscrollbind"),
            Ok(Command::Set {
                key: "scrollbind".to_string(),
                value: "off".to_string()
            })
        );
        assert_eq!(
            parse("set scrollbind=off"),
            Ok(Command::Set {
                key: "scrollbind".to_string(),
                value: "off".to_string()
            })
        );
        assert!(
            parse("set scrollbind=maybe").is_err(),
            "scrollbind is on|off only"
        );
    }

    // ---- FR-DL-4: `:set show-cn` -------------------------------------------

    #[test]
    fn set_show_cn_toggle_parses_with_and_without_equals() {
        assert_eq!(
            parse("set show-cn"),
            Ok(Command::Set {
                key: "show-cn".to_string(),
                value: "on".to_string()
            })
        );
        assert_eq!(
            parse("set noshow-cn"),
            Ok(Command::Set {
                key: "show-cn".to_string(),
                value: "off".to_string()
            })
        );
        assert_eq!(
            parse("set show-cn=off"),
            Ok(Command::Set {
                key: "show-cn".to_string(),
                value: "off".to_string()
            })
        );
        assert!(
            parse("set show-cn=maybe").is_err(),
            "show-cn is on|off only"
        );
    }

    // ---- FR-DL-6: `:game` ---------------------------------------------------

    #[test]
    fn bare_game_and_game_daily_both_parse_as_daily() {
        assert_eq!(parse("game"), Ok(Command::Game(GameSpec::Daily)));
        assert_eq!(parse("game daily"), Ok(Command::Game(GameSpec::Daily)));
        assert_eq!(parse("game DAILY"), Ok(Command::Game(GameSpec::Daily)));
    }

    #[test]
    fn game_share_parses_as_share() {
        assert_eq!(parse("game share"), Ok(Command::Game(GameSpec::Share)));
    }

    #[test]
    fn game_with_two_titles_underscore_normalizes_both() {
        assert_eq!(
            parse("game Alan_Turing Computer_science"),
            Ok(Command::Game(GameSpec::Pair(
                "Alan Turing".to_string(),
                "Computer science".to_string()
            )))
        );
    }

    #[test]
    fn game_with_the_wrong_number_of_titles_is_an_error() {
        assert!(
            parse("game Alan_Turing").is_err(),
            "one title alone (not \"daily\"/\"share\") is not a valid pair"
        );
        assert!(
            parse("game Alan_Turing Computer_science Extra").is_err(),
            "three tokens is not a start/goal pair"
        );
    }

    // ---- FR-DL-8: `:xyzzy` ---------------------------------------------------

    #[test]
    fn xyzzy_parses() {
        assert_eq!(parse("xyzzy"), Ok(Command::Xyzzy));
    }

    #[test]
    fn open_takes_the_rest_of_the_line_as_the_title() {
        assert_eq!(
            parse("open Alan Turing"),
            Ok(Command::Open("Alan Turing".to_string()))
        );
        assert_eq!(parse("o Turing"), Ok(Command::Open("Turing".to_string())));
        assert!(parse("open").is_err(), "missing title is an error");
    }

    #[test]
    fn lang_validates_the_code_shape() {
        assert_eq!(parse("lang de"), Ok(Command::Lang(Some("de".to_string()))));
        assert_eq!(
            parse("lang zh-yue"),
            Ok(Command::Lang(Some("zh-yue".to_string())))
        );
        assert!(parse("lang DE!").is_err());
    }

    /// PRD FR-ML-1: bare `:lang` (no code) is the switcher-picker trigger,
    /// not an error — the pre-FR-ML-1 behavior required an argument.
    #[test]
    fn bare_lang_opens_the_picker() {
        assert_eq!(parse("lang"), Ok(Command::Lang(None)));
        assert_eq!(parse("lang   "), Ok(Command::Lang(None)));
    }

    /// PRD FR-ML-4: bare `:wiki` opens the picker; `:wiki <name>` switches
    /// directly to any name at all — unlike `:lang`, there is no shape
    /// validation here (a wiki name has no fixed format), so an unknown name
    /// still parses; whether it resolves is `main::switch_wiki`'s job.
    #[test]
    fn bare_wiki_opens_the_picker_and_named_wiki_switches_directly() {
        assert_eq!(parse("wiki"), Ok(Command::Wiki(None)));
        assert_eq!(parse("wiki   "), Ok(Command::Wiki(None)));
        assert_eq!(
            parse("wiki wiktionary"),
            Ok(Command::Wiki(Some("wiktionary".to_string())))
        );
        assert_eq!(
            parse("wiki archwiki"),
            Ok(Command::Wiki(Some("archwiki".to_string())))
        );
    }

    /// PRD FR-SR-7: `:search-offline` (and its no-hyphen alias) parses as a
    /// bare toggle command, taking no argument.
    #[test]
    fn search_offline_parses_bare_and_its_alias() {
        assert_eq!(parse("search-offline"), Ok(Command::SearchOffline));
        assert_eq!(parse("searchoffline"), Ok(Command::SearchOffline));
    }

    #[test]
    fn theme_and_style_validate_against_known_names() {
        assert_eq!(
            parse("theme paper"),
            Ok(Command::Theme("paper".to_string()))
        );
        assert!(parse("theme sepia").is_err());
        assert_eq!(parse("style mla"), Ok(Command::Style("mla".to_string())));
        assert!(parse("style vancouver").is_err());
    }

    /// PRD FR-TH-1: `:theme <name>` and `:set theme=<name>` both accept a
    /// loaded user theme's name, not just the six built-ins.
    #[test]
    fn theme_and_set_theme_accept_a_user_theme_name() {
        let user_themes = vec!["solar".to_string()];
        assert_eq!(
            parse_with_user_themes("theme solar", &user_themes),
            Ok(Command::Theme("solar".to_string()))
        );
        assert_eq!(
            parse_with_user_themes("set theme=solar", &user_themes),
            Ok(Command::Set {
                key: "theme".to_string(),
                value: "solar".to_string(),
            })
        );
        // Still rejected without the user theme list.
        assert!(parse_with_user_themes("theme solar", &[]).is_err());
        // Still rejected regardless of the list, if truly unknown.
        assert!(parse_with_user_themes("theme nonexistent", &user_themes).is_err());
    }

    #[test]
    fn export_style_argument_is_optional_but_validated() {
        assert_eq!(parse("export"), Ok(Command::Export(None)));
        assert_eq!(
            parse("export harvard"),
            Ok(Command::Export(Some("harvard".to_string())))
        );
        assert!(parse("export vancouver").is_err());
    }

    #[test]
    fn bare_commands_and_aliases_resolve() {
        assert_eq!(parse("library"), Ok(Command::Library));
        assert_eq!(parse("lib"), Ok(Command::Library));
        assert_eq!(parse("research"), Ok(Command::Research));
        assert_eq!(parse("toc"), Ok(Command::Toc));
        assert_eq!(parse("q"), Ok(Command::Quit));
        assert_eq!(parse("quit"), Ok(Command::Quit));
        assert_eq!(parse("help"), Ok(Command::Help));
    }

    #[test]
    fn config_reload_parses_and_rejects_other_subcommands() {
        assert_eq!(parse("config reload"), Ok(Command::ConfigReload));
        assert!(parse("config").is_err());
        assert!(parse("config bogus").is_err());
    }

    #[test]
    fn tab_subcommands_parse() {
        assert_eq!(parse("tab close"), Ok(Command::TabClose));
        assert_eq!(parse("tab close!"), Ok(Command::TabClose));
        assert_eq!(parse("tab new"), Ok(Command::TabNew(None)));
        assert_eq!(
            parse("tab new Alan Turing"),
            Ok(Command::TabNew(Some("Alan Turing".to_string())))
        );
        assert_eq!(parse("tabs"), Ok(Command::Tabs));
        assert!(parse("tab").is_err());
        assert!(parse("tab frobnicate").is_err());
    }

    #[test]
    fn unknown_and_empty_input_report_usage() {
        let err = parse("frobnicate").unwrap_err();
        assert!(err.contains("unknown command"));
        assert!(err.contains("open <title>"));
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(
            parse("  open   Alan Turing  "),
            Ok(Command::Open("Alan Turing".to_string()))
        );
    }

    #[test]
    fn bookmarks_bare_opens_the_picker() {
        assert_eq!(parse("bookmarks"), Ok(Command::Bookmarks));
        assert_eq!(parse("readlater"), Ok(Command::ReadLater));
    }

    #[test]
    fn bookmarks_export_parses_format_and_optional_path() {
        assert_eq!(
            parse("bookmarks export md"),
            Ok(Command::BookmarksExport {
                format: "md".to_string(),
                path: None
            })
        );
        assert_eq!(
            parse("bookmarks export netscape /tmp/out.html"),
            Ok(Command::BookmarksExport {
                format: "netscape".to_string(),
                path: Some("/tmp/out.html".to_string())
            })
        );
        for format in ["md", "html", "json", "netscape"] {
            assert!(parse(&format!("bookmarks export {format}")).is_ok());
        }
    }

    #[test]
    fn bookmarks_export_rejects_bad_input() {
        assert!(parse("bookmarks export").is_err());
        assert!(parse("bookmarks export carrier-pigeon").is_err());
        assert!(parse("bookmarks frobnicate").is_err());
    }

    #[test]
    fn history_bare_opens_the_picker() {
        assert_eq!(parse("history"), Ok(Command::History));
    }

    #[test]
    fn history_clear_parses_both_scopes() {
        assert_eq!(
            parse("history clear all"),
            Ok(Command::HistoryClear(HistoryClearScope::All))
        );
        assert_eq!(
            parse("history clear today"),
            Ok(Command::HistoryClear(HistoryClearScope::Today))
        );
    }

    #[test]
    fn set_images_parses_on_and_off_and_rejects_others() {
        assert_eq!(
            parse("set images=on"),
            Ok(Command::Set {
                key: "images".to_string(),
                value: "on".to_string()
            })
        );
        assert_eq!(
            parse("set images=off"),
            Ok(Command::Set {
                key: "images".to_string(),
                value: "off".to_string()
            })
        );
        // Surrounding whitespace around the assignment is tolerated.
        assert_eq!(
            parse("set  images = off "),
            Ok(Command::Set {
                key: "images".to_string(),
                value: "off".to_string()
            })
        );
        assert!(parse("set images=maybe").is_err());
        assert!(parse("set width=90").is_err(), "width is not wired yet");
        assert!(parse("set images").is_err(), "needs key=value");
        assert!(parse("set").is_err());
    }

    /// PRD FR-PC-4 seed: `:set` now takes a small typed set beyond images.
    #[test]
    fn set_theme_measure_and_ambiguous_width_parse_and_validate() {
        assert_eq!(
            parse("set theme=paper"),
            Ok(Command::Set {
                key: "theme".to_string(),
                value: "paper".to_string()
            })
        );
        assert!(parse("set theme=sepia").is_err(), "unknown theme rejected");

        assert_eq!(
            parse("set measure=60"),
            Ok(Command::Set {
                key: "measure".to_string(),
                value: "60".to_string()
            })
        );
        // Bounds match the config loader (40..=200).
        assert!(parse("set measure=10").is_err(), "below the floor");
        assert!(parse("set measure=999").is_err(), "above the ceiling");
        assert!(parse("set measure=wide").is_err(), "non-integer");

        assert_eq!(
            parse("set ambiguous_width=2"),
            Ok(Command::Set {
                key: "ambiguous_width".to_string(),
                value: "2".to_string()
            })
        );
        assert_eq!(
            parse("set ambiguous_width=1"),
            Ok(Command::Set {
                key: "ambiguous_width".to_string(),
                value: "1".to_string()
            })
        );
        assert!(parse("set ambiguous_width=3").is_err());
    }

    /// PRD FR-RD-11: `:set reading_wpm=N`, same bounds-shared-with-the-config-
    /// loader pattern as `measure`.
    #[test]
    fn set_reading_wpm_parses_and_validates() {
        assert_eq!(
            parse("set reading_wpm=300"),
            Ok(Command::Set {
                key: "reading_wpm".to_string(),
                value: "300".to_string()
            })
        );
        assert!(parse("set reading_wpm=1").is_err(), "below the floor");
        assert!(
            parse("set reading_wpm=999999").is_err(),
            "above the ceiling"
        );
        assert!(parse("set reading_wpm=fast").is_err(), "non-integer");
    }

    #[test]
    fn set_prefetch_toggles_the_kill_switch() {
        assert_eq!(
            parse("set prefetch=off"),
            Ok(Command::Set {
                key: "prefetch".to_string(),
                value: "off".to_string()
            })
        );
        assert_eq!(
            parse("set prefetch=on"),
            Ok(Command::Set {
                key: "prefetch".to_string(),
                value: "on".to_string()
            })
        );
        assert!(parse("set prefetch=maybe").is_err());
    }

    /// PRD FR-NV-9: `:set mouse=on|off`, same on/off grammar as `images`.
    #[test]
    fn set_mouse_toggles_and_rejects_other_values() {
        assert_eq!(
            parse("set mouse=on"),
            Ok(Command::Set {
                key: "mouse".to_string(),
                value: "on".to_string()
            })
        );
        assert_eq!(
            parse("set mouse=off"),
            Ok(Command::Set {
                key: "mouse".to_string(),
                value: "off".to_string()
            })
        );
        assert!(parse("set mouse=maybe").is_err());
    }

    /// PRD FR-ACS-4: `:set animations=full|none`.
    #[test]
    fn set_animations_parses_full_and_none() {
        assert_eq!(
            parse("set animations=none"),
            Ok(Command::Set {
                key: "animations".to_string(),
                value: "none".to_string()
            })
        );
        assert_eq!(
            parse("set animations=full"),
            Ok(Command::Set {
                key: "animations".to_string(),
                value: "full".to_string()
            })
        );
        assert!(parse("set animations=smooth").is_err());
    }

    /// PRD FR-RD-2 / SEC-2: `:set hyperlinks=auto|on|off`.
    #[test]
    fn set_hyperlinks_parses_the_closed_set() {
        for value in ["auto", "on", "off"] {
            assert_eq!(
                parse(&format!("set hyperlinks={value}")),
                Ok(Command::Set {
                    key: "hyperlinks".to_string(),
                    value: value.to_string()
                })
            );
        }
        assert!(parse("set hyperlinks=sometimes").is_err());
    }

    /// PRD FR-PC-1: `:set`'s new typography/spacing keys, same
    /// bounds-shared-with-the-config-loader pattern as `measure`.
    #[test]
    fn set_spacing_keys_parse_and_validate_bounds() {
        assert_eq!(
            parse("set text_align=left"),
            Ok(Command::Set {
                key: "text_align".to_string(),
                value: "left".to_string()
            })
        );
        assert_eq!(
            parse("set text_align=center"),
            Ok(Command::Set {
                key: "text_align".to_string(),
                value: "center".to_string()
            })
        );
        assert!(parse("set text_align=justify").is_err());

        assert_eq!(
            parse("set margin=10"),
            Ok(Command::Set {
                key: "margin".to_string(),
                value: "10".to_string()
            })
        );
        assert!(
            parse(&format!(
                "set margin={}",
                crate::config::MARGIN_MAX as u32 + 1
            ))
            .is_err()
        );
        assert!(parse("set margin=wide").is_err());

        assert_eq!(
            parse("set paragraph_spacing=2"),
            Ok(Command::Set {
                key: "paragraph_spacing".to_string(),
                value: "2".to_string()
            })
        );
        assert!(
            parse(&format!(
                "set paragraph_spacing={}",
                crate::config::PARAGRAPH_SPACING_MAX as u32 + 1
            ))
            .is_err()
        );

        assert_eq!(
            parse("set line_spacing=1"),
            Ok(Command::Set {
                key: "line_spacing".to_string(),
                value: "1".to_string()
            })
        );
        assert!(
            parse(&format!(
                "set line_spacing={}",
                crate::config::LINE_SPACING_MAX as u32 + 1
            ))
            .is_err()
        );

        assert_eq!(
            parse("set word_spacing=1"),
            Ok(Command::Set {
                key: "word_spacing".to_string(),
                value: "1".to_string()
            })
        );
        assert!(
            parse(&format!(
                "set word_spacing={}",
                crate::config::WORD_SPACING_MAX as u32 + 1
            ))
            .is_err()
        );
    }

    /// PRD FR-PC-4: `:set-tab` accepts exactly `TAB_SCOPED_KEYS`, rejects the
    /// session-only keys with a clear message, and treats an empty
    /// right-hand side as a reset (`value: None`) rather than an error.
    #[test]
    fn set_tab_scopes_to_render_options_and_supports_reset() {
        assert_eq!(
            parse("set-tab measure=60"),
            Ok(Command::SetTab {
                key: "measure".to_string(),
                value: Some("60".to_string())
            })
        );
        assert_eq!(
            parse("set-tab measure="),
            Ok(Command::SetTab {
                key: "measure".to_string(),
                value: None
            }),
            "an empty right-hand side resets the override"
        );
        assert_eq!(
            parse("settab images=on"),
            Ok(Command::SetTab {
                key: "images".to_string(),
                value: Some("on".to_string())
            }),
            "settab is accepted as an alias"
        );
        // Every key in TAB_SCOPED_KEYS round-trips through validation.
        for key in TAB_SCOPED_KEYS {
            let value = match key {
                "images" | "justify" | "hyphenate" => "on",
                "ambiguous_width" => "2",
                "text_align" => "left",
                "measure" => "60",
                _ => "1",
            };
            assert!(
                parse(&format!("set-tab {key}={value}")).is_ok(),
                "{key} should be a valid :set-tab key"
            );
        }
        // Session-only keys are rejected with a clear message, not silently
        // accepted or confused with a per-tab override.
        for key in [
            "theme",
            "prefetch",
            "mouse",
            "animations",
            "hyperlinks",
            "reading_wpm",
        ] {
            let err = parse(&format!("set-tab {key}=on")).unwrap_err();
            assert!(err.contains("not a per-tab setting"), "{key}: {err}");
        }
        assert!(parse("set-tab measure").is_err(), "needs key=value");
        assert!(parse("set-tab").is_err());
    }

    #[test]
    fn prefetch_log_parses() {
        assert_eq!(parse("prefetch-log"), Ok(Command::PrefetchLog));
        assert_eq!(parse("prefetchlog"), Ok(Command::PrefetchLog));
    }

    #[test]
    fn interest_and_stats_commands_parse() {
        assert_eq!(parse("interests"), Ok(Command::Interests));
        assert_eq!(parse("interest"), Ok(Command::Interests));
        assert_eq!(parse("not-interested"), Ok(Command::NotInterested));
        assert_eq!(parse("notinterested"), Ok(Command::NotInterested));
        assert_eq!(parse("stats"), Ok(Command::Stats));
    }

    #[test]
    fn start_and_today_parse() {
        assert_eq!(parse("start"), Ok(Command::Start));
        assert_eq!(parse("today"), Ok(Command::Today));
    }

    #[test]
    fn history_clear_rejects_bad_input() {
        assert!(parse("history clear").is_err());
        assert!(parse("history clear yesterday").is_err());
        assert!(parse("history frobnicate").is_err());
    }

    #[test]
    fn save_bare_and_tiers_parse() {
        assert_eq!(
            parse("save"),
            Ok(Command::Save(SaveSpec::Current(Tier::T0)))
        );
        assert_eq!(
            parse("save t0"),
            Ok(Command::Save(SaveSpec::Current(Tier::T0)))
        );
        assert_eq!(
            parse("save t1"),
            Ok(Command::Save(SaveSpec::Current(Tier::T1)))
        );
        assert_eq!(
            parse("save t2"),
            Ok(Command::Save(SaveSpec::Current(Tier::T2)))
        );
        assert!(parse("save t3").is_err());
        assert!(parse("save t1 extra").is_err(), "a tier takes no argument");
    }

    #[test]
    fn save_bulk_forms_parse() {
        assert_eq!(
            parse("save tag crypto"),
            Ok(Command::Save(SaveSpec::Tag("crypto".to_string())))
        );
        assert_eq!(
            parse("save category Physics"),
            Ok(Command::Save(SaveSpec::Category("Physics".to_string())))
        );
        assert_eq!(
            parse("save cat Category:Physics"),
            Ok(Command::Save(SaveSpec::Category(
                "Category:Physics".to_string()
            )))
        );
        assert_eq!(parse("save tabs"), Ok(Command::Save(SaveSpec::Tabs)));
        assert!(parse("save tag").is_err());
        assert!(parse("save category").is_err());
        assert!(parse("save frobnicate").is_err());
    }

    #[test]
    fn save_export_parses_format_and_optional_path() {
        assert_eq!(
            parse("save export md"),
            Ok(Command::Save(SaveSpec::Export {
                format: "md".to_string(),
                path: None
            }))
        );
        assert_eq!(
            parse("save export html /tmp/out.html"),
            Ok(Command::Save(SaveSpec::Export {
                format: "html".to_string(),
                path: Some("/tmp/out.html".to_string())
            }))
        );
        for format in ["md", "txt", "html"] {
            assert!(parse(&format!("save export {format}")).is_ok());
        }
        assert!(parse("save export").is_err());
        assert!(parse("save export pdf").is_err());
    }

    #[test]
    fn saved_and_fetch_queue_parse() {
        assert_eq!(parse("saved"), Ok(Command::Saved));
        assert_eq!(parse("fetch-queue"), Ok(Command::FetchQueue));
        assert_eq!(parse("fetchqueue"), Ok(Command::FetchQueue));
    }

    #[test]
    fn random_parses_bare_and_good_and_rejects_other_arguments() {
        assert_eq!(parse("random"), Ok(Command::Random(RandomSpec::Any)));
        assert_eq!(parse("random good"), Ok(Command::Random(RandomSpec::Good)));
        assert!(parse("random bad").is_err());
        assert!(parse("random good extra").is_err());
    }

    #[test]
    fn related_parses_bare() {
        assert_eq!(parse("related"), Ok(Command::Related));
    }

    #[test]
    fn login_parses_loopback_and_paste_and_logout() {
        assert_eq!(parse("login"), Ok(Command::Login(LoginMode::Loopback)));
        assert_eq!(parse("login paste"), Ok(Command::Login(LoginMode::Paste)));
        assert!(parse("login bogus").is_err());
        assert_eq!(parse("logout"), Ok(Command::Logout));
    }

    #[test]
    fn editing_commands_parse() {
        // PRD FR-ACC-8.
        assert_eq!(parse("enable-editing"), Ok(Command::EnableEditing));
        assert_eq!(parse("enableediting"), Ok(Command::EnableEditing));
        assert_eq!(parse("edit"), Ok(Command::Edit(None)));
        assert_eq!(
            parse("edit fixed teh->the"),
            Ok(Command::Edit(Some("fixed teh->the".to_string())))
        );
    }

    #[test]
    fn talk_parses_bare() {
        assert_eq!(parse("talk"), Ok(Command::Talk));
    }

    // ---- PRD FR-ACC-2/3/4/7: watchlist / notifications / contribs / prefs --

    #[test]
    fn watchlist_notifications_and_prefs_parse_bare() {
        assert_eq!(parse("watchlist"), Ok(Command::Watchlist));
        assert_eq!(parse("notifications"), Ok(Command::Notifications));
        assert_eq!(parse("prefs"), Ok(Command::Prefs));
    }

    // ---- PRD FR-BM-5/6: sync / mirror-watchlist -----------------------------

    #[test]
    fn sync_and_mirror_watchlist_parse_bare() {
        assert_eq!(parse("sync"), Ok(Command::Sync));
        assert_eq!(parse("mirror-watchlist"), Ok(Command::MirrorWatchlist));
        assert_eq!(parse("mirrorwatchlist"), Ok(Command::MirrorWatchlist));
    }

    #[test]
    fn contribs_parses_bare_as_the_logged_in_user() {
        // Resolving "bare = the logged-in user" needs a session, which
        // parsing doesn't have — `None` here just means "not specified,"
        // resolved at execution time (`main::open_contribs`).
        assert_eq!(parse("contribs"), Ok(Command::Contribs(None)));
    }

    #[test]
    fn contribs_parses_an_explicit_username() {
        assert_eq!(
            parse("contribs OtherEditor"),
            Ok(Command::Contribs(Some("OtherEditor".to_string())))
        );
        // A username may itself contain spaces (real MediaWiki usernames
        // can) — everything after the command name is the username, not
        // just its first word.
        assert_eq!(
            parse("contribs Jane Q. Editor"),
            Ok(Command::Contribs(Some("Jane Q. Editor".to_string())))
        );
    }

    #[test]
    fn info_parses_bare() {
        assert_eq!(parse("info"), Ok(Command::Info));
    }

    // ---- PRD FR-HS-3: :trail --------------------------------------------------

    #[test]
    fn trail_bare_opens_the_session_scoped_view() {
        assert_eq!(parse("trail"), Ok(Command::Trail(TrailScope::Session)));
        assert_eq!(parse("trail   "), Ok(Command::Trail(TrailScope::Session)));
    }

    #[test]
    fn trail_all_and_days_widen_the_scope() {
        assert_eq!(parse("trail all"), Ok(Command::Trail(TrailScope::All)));
        assert_eq!(
            parse("trail days 7"),
            Ok(Command::Trail(TrailScope::Days(7)))
        );
        assert!(parse("trail days").is_err());
        assert!(parse("trail days 0").is_err(), "0 days is meaningless");
        assert!(parse("trail days soon").is_err());
        assert!(parse("trail frobnicate").is_err());
    }

    /// PRD FR-HS-3 v2: `:trail dag [all|days N]` mirrors `:trail`'s own
    /// scope grammar, one level deeper.
    #[test]
    fn trail_dag_mirrors_the_scope_grammar_of_plain_trail() {
        assert_eq!(
            parse("trail dag"),
            Ok(Command::TrailDag(TrailScope::Session))
        );
        assert_eq!(
            parse("trail dag all"),
            Ok(Command::TrailDag(TrailScope::All))
        );
        assert_eq!(
            parse("trail dag days 7"),
            Ok(Command::TrailDag(TrailScope::Days(7)))
        );
        assert!(parse("trail dag days").is_err());
        assert!(parse("trail dag days 0").is_err());
        assert!(parse("trail dag frobnicate").is_err());
    }

    #[test]
    fn trail_export_parses_format_and_optional_path() {
        assert_eq!(
            parse("trail export md"),
            Ok(Command::TrailExport {
                format: "md".to_string(),
                path: None
            })
        );
        assert_eq!(
            parse("trail export dot /tmp/out.dot"),
            Ok(Command::TrailExport {
                format: "dot".to_string(),
                path: Some("/tmp/out.dot".to_string())
            })
        );
        for format in ["md", "dot", "mermaid"] {
            assert!(parse(&format!("trail export {format}")).is_ok());
        }
        assert!(parse("trail export").is_err());
        assert!(parse("trail export pdf").is_err());
    }

    // ---- PRD FR-TB-5: named sessions ---------------------------------------

    #[test]
    fn mksession_and_session_parse_a_valid_name() {
        assert_eq!(
            parse("mksession research"),
            Ok(Command::MkSession("research".to_string()))
        );
        assert_eq!(
            parse("session research"),
            Ok(Command::SessionSwitch("research".to_string()))
        );
        assert_eq!(
            parse("mksession ww2-research_2"),
            Ok(Command::MkSession("ww2-research_2".to_string()))
        );
    }

    #[test]
    fn mksession_and_session_require_a_name() {
        assert!(parse("mksession").is_err());
        assert!(parse("session").is_err());
    }

    #[test]
    fn mksession_and_session_reject_an_unsafe_name() {
        for cmd in ["mksession", "session"] {
            assert!(parse(&format!("{cmd} has space")).is_err());
            assert!(parse(&format!("{cmd} ../escape")).is_err());
        }
    }

    #[test]
    fn sessions_parses_bare() {
        assert_eq!(parse("sessions"), Ok(Command::Sessions));
    }

    // ---- PRD FR-PC-2: :tts / :speak ----------------------------------------

    #[test]
    fn tts_and_speak_parse_play_and_stop() {
        assert_eq!(parse("tts"), Ok(Command::Tts(TtsSpec::Play)));
        assert_eq!(parse("tts stop"), Ok(Command::Tts(TtsSpec::Stop)));
        assert_eq!(parse("speak"), Ok(Command::Tts(TtsSpec::Play)));
        assert_eq!(parse("speak stop"), Ok(Command::Tts(TtsSpec::Stop)));
        assert!(parse("tts pause").is_err());
    }

    // ---- PRD FR-CS-5: :run <macro> ------------------------------------------

    #[test]
    fn run_parses_a_macro_name() {
        assert_eq!(
            parse("run morning"),
            Ok(Command::RunMacro("morning".to_string()))
        );
        assert!(parse("run").is_err());
    }
}
