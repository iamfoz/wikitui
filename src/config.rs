//! The configuration system (PRD §6.7): a versioned TOML file at
//! `$XDG_CONFIG_HOME/wikitui/config.toml`, layered under environment
//! variables and CLI flags. Precedence is fixed by the PRD and never
//! reordered here: **CLI flags > environment (`WIKITUI_*`) > config file >
//! built-in defaults**.
//!
//! Every resolved value remembers *where* it came from (`Source`) so
//! `wikitui config doctor` can print an honest provenance report instead of
//! just a final number. Nothing in this module ever aborts on bad input:
//! a missing file, a syntax error, an unknown key, or an out-of-range value
//! all degrade to "keep going with the default and say why" — per §6.7,
//! "unknown keys warn, never crash."
//!
//! One literal deviation from the brief this module implements: the spec
//! called for a top-level `wiki = "<name>"` selector alongside `[wiki.
//! <name>]` sections, but those collide under TOML's own rules — a scalar
//! key and a table can't share one name (confirmed against both `toml`
//! crate and Python's `tomllib`). The selector is `active_wiki` instead;
//! the `[wiki.<name>]` section shape (FR-ML-5's own wording) is unchanged.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::cite::CiteStyle;
use crate::target;
use crate::theme::Theme;

/// Schema version this build understands. Bump alongside a `MIGRATIONS`
/// entry when a key's meaning changes across releases.
pub const CONFIG_VERSION: u32 = 1;

/// Built-in fallback when no CLI flag, env var, or config file sets a
/// wiki's base URL (PRD §6.2 rule 1: per-wiki `{lang}.wikipedia.org`).
pub const DEFAULT_BASE_URL_TEMPLATE: &str = "https://{lang}.wikipedia.org";

/// The sane bounds for `measure` (FR-RD-9), shared with `:set measure=N`
/// (FR-PC-4) so the runtime override and the config loader agree.
pub const MEASURE_MIN: u16 = 40;
pub const MEASURE_MAX: u16 = 200;
/// The sane bounds for `reading_wpm` (FR-RD-11), shared with `:set
/// reading_wpm=N` so the runtime override and the config loader agree —
/// same split as `MEASURE_MIN`/`MEASURE_MAX`.
pub const READING_WPM_MIN: u32 = 50;
pub const READING_WPM_MAX: u32 = 2000;
/// The sane bounds for the FR-PC-1 spacing/typography knobs, shared with
/// `:set`/`:set-tab` (`command::validate_set_value`) so the runtime override
/// and the config loader agree — same split as `MEASURE_MIN`/`MEASURE_MAX`.
/// `margin` tops out well below `measure` itself (a margin that ate the
/// whole column would leave nothing to read); `paragraph_spacing`/
/// `line_spacing` top out at "airy, not empty page"; `word_spacing` is
/// binary (PRD FR-PC-1: "inter_word_spacing +1") since a cell grid has no
/// finer inter-word gradient worth exposing.
pub const MARGIN_MAX: u16 = 40;
pub const PARAGRAPH_SPACING_MAX: u8 = 4;
pub const LINE_SPACING_MAX: u8 = 2;
pub const WORD_SPACING_MAX: u8 = 1;
const DEFAULT_WIKI_NAME: &str = "wikipedia";

/// Where a resolved value came from, in the precedence order the PRD
/// fixes: CLI outranks env outranks file outranks the built-in default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Default,
    File,
    Env,
    Cli,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Source::Default => "default",
            Source::File => "config file",
            Source::Env => "environment",
            Source::Cli => "CLI flag",
        })
    }
}

/// A resolved value paired with the layer that won.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Valued<T> {
    pub value: T,
    pub source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueLevel {
    /// Surfaced, but the app still starts and a sane default stands in.
    Warning,
    /// The config file itself couldn't be parsed at all. Still never
    /// crashes (defaults are used for everything), but `doctor` exits 1.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub level: IssueLevel,
    pub message: String,
}

impl Issue {
    fn warning(message: impl Into<String>) -> Self {
        Self {
            level: IssueLevel::Warning,
            message: message.into(),
        }
    }
    fn error(message: impl Into<String>) -> Self {
        Self {
            level: IssueLevel::Error,
            message: message.into(),
        }
    }
}

/// CLI-flag overrides, outranking everything but a URL/lang-prefixed TITLE
/// argument's own embedded language (main.rs folds that in ahead of this).
/// A thin, clap-independent shape so resolution stays testable without
/// constructing a real `Cli`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOverrides {
    pub lang: Option<String>,
    pub theme: Option<String>,
    pub measure: Option<u16>,
    pub ambiguous_width: Option<u8>,
    pub cite_style: Option<String>,
    /// PRD FR-ML-4: set when the TITLE argument itself named a sister
    /// project (`wikt:Word`, an `en.wiktionary.org` URL — `target::parse`'s
    /// `project` field) — the whole session starts on that wiki, not just
    /// this one fetch, same as how the TITLE argument's own language prefix
    /// already overrides `--lang`/`lang` (`main`'s CLI-target handling).
    pub active_wiki: Option<String>,
}

/// Environment-variable overrides. Kept as raw strings (mirroring how
/// process env always arrives) so a garbled value gets the same
/// warn-and-fall-back treatment as a garbled file value, rather than
/// silently vanishing at `.parse().ok()` time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvOverrides {
    pub lang: Option<String>,
    pub theme: Option<String>,
    pub measure: Option<String>,
    pub ambiguous_width: Option<String>,
    pub cite_style: Option<String>,
    pub base_url: Option<String>,
    /// PRD FR-BM-3's "(config)" `readlater_auto_dequeue`.
    pub readlater_auto_dequeue: Option<String>,
    /// PRD FR-TH-7's `images` override of the theme default (`WIKITUI_IMAGES`).
    pub images: Option<String>,
    /// PRD FR-PF-6 kill switch via env (`WIKITUI_PREFETCH`).
    pub prefetch: Option<String>,
    /// PRD NF-NET-2 User-Agent contact channel via env (`WIKITUI_CONTACT`).
    pub contact: Option<String>,
    /// PRD FR-ACS-4's `WIKITUI_ANIMATIONS=none` no-motion override.
    pub animations: Option<String>,
    /// PRD FR-TH-3's `color_depth` override via `WIKITUI_COLOR_DEPTH`
    /// (`auto`/`truecolor`/`256`/`16`/`mono`) — for testing and user control
    /// over capability degradation, same env-first pattern as `animations`.
    pub color_depth: Option<String>,
}

impl EnvOverrides {
    /// Reads the supported `WIKITUI_*` variables from the real process
    /// environment. Isolated to this one call site so tests can construct
    /// `EnvOverrides` directly instead of mutating global env state (which
    /// would race across `cargo test`'s parallel threads).
    pub fn from_process_env() -> Self {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            lang: get("WIKITUI_LANG"),
            theme: get("WIKITUI_THEME"),
            measure: get("WIKITUI_MEASURE"),
            ambiguous_width: get("WIKITUI_AMBIGUOUS_WIDTH"),
            cite_style: get("WIKITUI_CITE_STYLE"),
            base_url: get("WIKITUI_BASE_URL"),
            readlater_auto_dequeue: get("WIKITUI_READLATER_AUTO_DEQUEUE"),
            images: get("WIKITUI_IMAGES"),
            prefetch: get("WIKITUI_PREFETCH"),
            contact: get("WIKITUI_CONTACT"),
            animations: get("WIKITUI_ANIMATIONS"),
            color_depth: get("WIKITUI_COLOR_DEPTH"),
        }
    }
}

/// Everything needed to re-run `resolve` later — the `:config reload` /
/// SIGHUP live-reload path (§6.7) re-reads the file with the exact same
/// overrides pinned at startup, so CLI/env still outrank whatever the file
/// says even after a reload.
#[derive(Debug, Clone, Default)]
pub struct ConfigContext {
    pub cli: CliOverrides,
    pub env: EnvOverrides,
    pub config_path: Option<PathBuf>,
}

/// The fully resolved, ready-to-use configuration.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub config_version: Valued<u32>,
    pub lang: Valued<String>,
    pub languages: Valued<Vec<String>>,
    pub theme: Valued<String>,
    /// PRD FR-CS-3's keymap preset selector (`keymap = "vim"|"emacs"`,
    /// default `vim`). The concrete keymap (preset + any `keymap.toml`
    /// overrides) is built by `main` from this — see `registry::Keymap`.
    pub keymap_preset: Valued<String>,
    pub measure: Valued<u16>,
    pub ambiguous_wide: Valued<bool>,
    pub cite_style: Valued<String>,
    pub cache_max_mb: Valued<u64>,
    pub cache_fresh_ttl_hours: Valued<u64>,
    /// PRD FR-OFF-2's force-refetch backstop: entries older than this are
    /// treated as absent on open, network-first, regardless of what a
    /// background revalidation might otherwise have decided.
    pub cache_force_refetch_days: Valued<u64>,
    /// PRD FR-PR-5's cache relocation: `[cache] dir` overrides the platform
    /// cache directory (which already honors `$XDG_CACHE_HOME` via the
    /// `directories` crate) so a reader can point the whole L2 cache at,
    /// say, a tmpfs mount. File-only (like the other `[cache]` keys) — no
    /// CLI flag or env var, matching how `max_mb`/`fresh_ttl_hours`/
    /// `force_refetch_days` are already file-only. `None` (the default)
    /// means "use the platform directory" — see `cache::resolve_pages_dir`.
    pub cache_dir: Valued<Option<PathBuf>>,
    pub active_wiki: Valued<String>,
    /// Always concrete (defaults to `DEFAULT_BASE_URL_TEMPLATE`); `{lang}`
    /// is substituted by `api::WikiClient` when present, else used
    /// verbatim (arbitrary MediaWiki sites, FR-ML-5, aren't per-language).
    pub base_url_template: Valued<String>,
    /// PRD FR-ML-5's feature-degradation matrix for `active_wiki`.
    pub wiki_capabilities: ResolvedWikiCapabilities,
    /// PRD FR-ML-4's `:wiki <name>` switch target list, resolved once at
    /// startup: Wikipedia, the four sister projects, and every
    /// `[wiki.<name>]` section — see [`resolve_wiki`]'s doc comment.
    pub wiki_registry: ResolvedWikiRegistry,
    /// PRD FR-BM-3's "(config)" read-later behavior: whether opening a
    /// queued entry removes it. Defaults to `true`.
    pub readlater_auto_dequeue: Valued<bool>,
    /// PRD FR-HS-4's retention window: `history::History::retention_prune`
    /// runs with this at startup. `0` (the default) means "keep forever."
    pub history_retention_days: Valued<u64>,
    /// PRD FR-PF-3 / FR-PR-2: whether the local interest-learning model
    /// updates from reading signals. Default `true` — it is a differentiator,
    /// fully local + inspectable (`:interests`) + incognito-exempt (incognito
    /// disables it regardless). File only, like `include_nonfree`/`history.*`.
    pub interest_learning: Valued<bool>,
    /// PRD FR-PF-3's exponential-decay half-life in days
    /// (`interest_half_life_days`, default 30): a topic's affinity halves per
    /// this many days untouched. File only.
    pub interest_half_life_days: Valued<f64>,
    /// PRD FR-TH-7's `images` key: overrides the active theme's `images`
    /// default when set (`Some(true)`/`Some(false)`); `None` (the default)
    /// means "follow the theme." Wired into `App::images_override`.
    pub images: Valued<Option<bool>>,
    /// PRD §10 licensing: whether non-free/fair-use images may be used.
    /// Default false. Today a policy flag with a documented seam (saved-page
    /// image persistence, which must exclude non-free, isn't built yet).
    pub include_nonfree: Valued<bool>,
    /// PRD FR-DL-8's `pro = true`: disables every easter egg (`:xyzzy`) and
    /// achievement toast outright. Default `false` — file only (like
    /// `include_nonfree`/`startpage`), a preference set once, not a CLI/env
    /// concern.
    pub pro: Valued<bool>,
    /// PRD FR-DL-1's `startpage = feed|blank|resume`, default `feed`. File
    /// only (like `include_nonfree`/`history.*`) — a preference set once,
    /// not worth a CLI flag or env var for a single run. Parsed into
    /// `startpage::StartPageConfig` by `App::start_page_for_launch`.
    pub startpage: Valued<String>,
    /// PRD FR-TB-5: an independent trigger for session auto-restore,
    /// alongside `startpage = resume` — a reader who prefers `startpage =
    /// blank`/`feed` for the "look" of that view can still opt into
    /// reopening their tabs by setting this instead of switching `startpage`.
    /// File only, default `false` (same scope as `startpage`/
    /// `include_nonfree` — a preference set once, not a CLI/env concern).
    pub restore_session: Valued<bool>,
    /// PRD FR-RD-11's reading-time WPM divisor, default 230. File only (like
    /// `startpage`/`include_nonfree`) — also settable at runtime via `:set
    /// reading_wpm=N` (`main::Command::Set`), same split as `measure`'s
    /// config-default-plus-runtime-override.
    pub reading_wpm: Valued<u32>,
    /// PRD §5.8 / FR-PF-1..6 prefetch settings (the `[prefetch]` table).
    pub prefetch: ResolvedPrefetch,
    /// PRD NF-NET-2 User-Agent contact channel (`[network] contact`).
    pub network_contact: Valued<String>,
    /// PRD FR-NV-9 (mouse), FR-ACS-4 (no-motion), FR-TH-4 (auto light/dark),
    /// FR-RD-2 (OSC 8 hyperlinks) — the terminal-integration settings this
    /// chunk adds, grouped the way `[prefetch]` groups its own table.
    pub terminal: ResolvedTerminal,
    /// PRD FR-PC-1's `[reading]` table (spacing/typography options).
    pub reading: ResolvedReading,
    /// PRD §5.9 / FR-ACC-1's `[auth]` table (OAuth client id + endpoints).
    pub auth: ResolvedAuth,
    /// PRD FR-BM-6's designated mirror tag: which bookmark tag `:sync`/
    /// `:mirror-watchlist` mirrors to the real watchlist. File only (a
    /// preference set once, like `startpage`/`include_nonfree`), default
    /// `"watched"`.
    pub watchlist_mirror_tag: Valued<String>,
    /// PRD FR-PC-2 / SEC-5: the user-configured TTS command (e.g.
    /// `"espeak-ng -s 160"`, macOS `"say"`). File only (no CLI/env — a
    /// preference set once, like `watchlist_mirror_tag`/`startpage`).
    /// `None` (unset, the default) means TTS is disabled; `main::
    /// start_tts_playback` reports a notice rather than silently doing
    /// nothing when invoked with nothing configured — there is no portable
    /// platform default to guess (unlike `$EDITOR`, there is no `$SPEECH`
    /// convention).
    pub tts_command: Valued<Option<String>>,
    /// PRD FR-CS-5's command-sequence macros (`[command.<name>] run =
    /// [...]`): every macro name mapped to its ordered step list. Each step
    /// is already checked against the registry/command grammar at load time
    /// (an unknown step warns via `issues` but does not drop the macro —
    /// see [`resolve_macros`]'s doc comment for why); a step that still
    /// turns out bad at run time is caught again there, per FR-CS-5's own
    /// per-step error policy.
    pub macros: std::collections::BTreeMap<String, Vec<String>>,
    /// Parse errors, unknown keys, and rejected values — never fatal, but
    /// `doctor` reports them and exits 1 if any is `IssueLevel::Error`.
    pub issues: Vec<Issue>,
    /// Set only when the file's `config_version` was below `CONFIG_VERSION`
    /// and a migration ran (today, a documented no-op — see `MIGRATIONS`).
    pub migration_summary: Option<String>,
    /// The file actually consulted, if any (for `doctor`'s report).
    pub config_path: Option<PathBuf>,
}

impl ResolvedConfig {
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|i| i.level == IssueLevel::Error)
    }
}

/// PRD FR-ML-5's per-wiki feature-degradation matrix, resolved for one
/// wiki (`active_wiki`, or one [`ResolvedWikiEntry`] in the registry).
/// Defaults follow whether the wiki is the built-in `wikipedia`: that one
/// gets every feature (today's unconditional pre-FR-ML-5 behavior,
/// unchanged); every other wiki — a sister project or a third-party
/// `[wiki.<name>]` site — defaults the three optional endpoints off, on the
/// grounds that Wikifeeds and PageAssessments in particular are close to
/// Wikipedia-only in practice (§6.2 rule 6 / Appendix A "Quality"). A
/// `[wiki.<name>]` section overrides any of the four explicitly, e.g.:
///
/// ```toml
/// [wiki.dewiktionary]
/// base_url = "https://de.wiktionary.org"
/// pageviews = true       # this deployment's PageViewInfo does work
/// ```
///
/// `parser` alone always defaults to `"auto"` regardless of which wiki —
/// the try-Parsoid-then-detect-and-fall-back behavior
/// (`api::WikiClient::fetch_article_html`) costs nothing extra once Parsoid
/// answers, so there is no reason to default it off the way the other three
/// are.
#[derive(Debug, Clone)]
pub struct ResolvedWikiCapabilities {
    /// `"auto"` (try Parsoid REST, fall back to legacy `action=parse` on an
    /// unsupported-shaped 404 — PRD §6.2 rule 3), `"parsoid"` (Parsoid only,
    /// no fallback — surfaces a misconfigured wiki's real error instead of
    /// masking it), or `"legacy"` (skip Parsoid entirely, straight to
    /// `action=parse` — saves a request on a wiki already known to lack it).
    pub parser: Valued<String>,
    /// Wikifeeds (`feed/featured`/`feed/onthisday`) — gates the FR-DL-1
    /// start-page feed and FR-PF-2 trending prefetch. `false` degrades the
    /// start page to `StartPageModel::offline_fallback`'s simpler view
    /// (recent history / saved pages) — the same view an offline reader
    /// already sees, not a new code path.
    pub wikifeeds: Valued<bool>,
    /// `prop=pageviews` — gates FR-PF-1's link-rank pageviews term. `false`
    /// degrades ranking to lead-position-only (the pageviews term is 0 for
    /// every candidate, the same math `rank_links` already does for an
    /// unseen title — no separate "degraded" code path there either).
    pub pageviews: Valued<bool>,
    /// `prop=pageassessments` — gates the FR-DL-3 quality badge. `false`
    /// means the lookup is never attempted, so no badge ever shows for this
    /// wiki (same visible result as a wiki that has the extension but no
    /// assessment for one particular title).
    pub pageassessments: Valued<bool>,
}

/// One entry in the FR-ML-4/5 wiki registry: a resolvable `:wiki <name>`
/// target's host template plus its own capability matrix.
#[derive(Debug, Clone)]
pub struct ResolvedWikiEntry {
    pub base_url_template: Valued<String>,
    pub capabilities: ResolvedWikiCapabilities,
}

/// Every wiki `:wiki <name>` can switch to, resolved once at startup
/// (`resolve_wiki`): the built-in `wikipedia` plus its four sister projects
/// (FR-ML-4), and every `[wiki.<name>]` section the config file defines
/// (FR-ML-5, arbitrary MediaWiki sites) — even ones that aren't the
/// currently active wiki, so a runtime switch never re-parses the config
/// file.
#[derive(Debug, Clone, Default)]
pub struct ResolvedWikiRegistry {
    pub entries: std::collections::BTreeMap<String, ResolvedWikiEntry>,
}

/// The resolved `[prefetch]` table (PRD §5.8, FR-PF-1/5/6). Each field carries
/// its provenance so `doctor` can show where a value came from.
#[derive(Debug, Clone)]
pub struct ResolvedPrefetch {
    /// FR-PF-6 kill switch. Default on (the product intent — prefetch is the
    /// differentiator); `main` still forces it off under `--incognito`.
    pub enabled: Valued<bool>,
    /// FR-PF-5 per-day byte budget in MB (default 20).
    pub daily_mb: Valued<u64>,
    /// FR-PF-5 per-hour background request budget (default 100).
    pub hourly_requests: Valued<u64>,
    /// FR-PF-5 `never|reduced|always` (default `reduced`).
    pub metered: Valued<String>,
    /// FR-PF-1 top-N article bodies per page (default 5).
    pub top_n: Valued<u64>,
    /// FR-PF-1 ranking weights (`w1·lead + w2·log(views) + w3·affinity`).
    pub weight_lead: Valued<f64>,
    pub weight_pageviews: Valued<f64>,
    pub weight_affinity: Valued<f64>,
}

/// The terminal-integration settings (PRD FR-NV-9, FR-ACS-4/6, FR-TH-4,
/// FR-RD-2). Each field carries its provenance like every other resolved
/// value, so `doctor` and `App::config_ctx`-driven reload agree on where a
/// value came from.
#[derive(Debug, Clone)]
pub struct ResolvedTerminal {
    /// FR-NV-9: opt-in mouse support. Default **off** — enabling mouse
    /// capture (`crossterm::event::EnableMouseCapture`) claims the terminal's
    /// own click/drag handling, which is also how a user's terminal emulator
    /// normally does text selection and copy. A reader who never asks for
    /// mouse support keeps that native selection/copy working exactly as
    /// before wikitui existed; `:set mouse=on` (or this config key) is the
    /// explicit, informed opt-in — never the default, per FR-ACS-3's
    /// keyboard-only-guarantee ethos ("mouse strictly optional" reads most
    /// safely as "and off unless asked for").
    pub mouse: Valued<bool>,
    /// FR-ACS-4: `full` (the default) or `none`. There is no actual
    /// smooth-scroll/spinner/blink in this codebase to disable today (see
    /// `main.rs`'s no-motion audit note) — this is a forward-compatible
    /// guard rail plus the config/env/ACCESSIBLE-precedence surface itself,
    /// which is real, wired, and tested regardless of whether anything
    /// currently animates.
    pub animations: Valued<String>,
    /// FR-TH-4: query OSC 11 at startup and auto-pick `theme_light`/
    /// `theme_dark`. Default **off** — an explicit `theme` always wins
    /// unless this is turned on, and even then only when `theme` itself was
    /// never explicitly set (see `main::resolve_auto_theme_pick`).
    pub auto_theme: Valued<bool>,
    /// FR-TH-4's light-background pick when `auto_theme` fires. Default
    /// `paper` — the PRD's own light theme.
    pub theme_light: Valued<String>,
    /// FR-TH-4's dark-background pick when `auto_theme` fires. Default
    /// `terminal` — the app's own overall default theme, so a dark terminal
    /// auto-resolves to exactly what an unconfigured install already shows.
    pub theme_dark: Valued<String>,
    /// FR-RD-2 / SEC-2: `auto` (the default), `on`, or `off` — see
    /// `hyperlink::HyperlinkMode`.
    pub hyperlinks: Valued<String>,
    /// FR-TH-3: `auto` (the default, `theme::detect_color_depth`),
    /// `truecolor`, `256`, `16`, or `mono` — an explicit value overrides
    /// auto-detection for testing or a terminal this build's heuristic
    /// misjudges. Resolved to a `theme::ColorDepth` by `main`, not here
    /// (this module has no reason to depend on `theme`'s degradation code,
    /// only to validate the string against the closed set).
    pub color_depth: Valued<String>,
}

/// PRD FR-PC-1's `[reading]` table: the "honestly marketed spacing options"
/// — measure/margin/text-align, paragraph and interline spacing, and the
/// one letter-spacing-adjacent knob a cell grid actually allows (extra
/// inter-word gap width). Each also has a runtime `:set`/`:set-tab`
/// override (`command::validate_set_value`), same config-default-plus-
/// runtime-override split as `measure`/`reading_wpm`.
#[derive(Debug, Clone)]
pub struct ResolvedReading {
    /// Extra left margin in cells, default 0. See `layout::LayoutOptions::
    /// margin`'s doc comment for how it interacts with centering.
    pub margin: Valued<u16>,
    /// `center` (default, FR-RD-9's original centered column) or `left`.
    pub text_align: Valued<String>,
    /// Blank rows between blocks, default 1 (today's pre-FR-PC-1 behavior).
    pub paragraph_spacing: Valued<u8>,
    /// Blank rows after every wrapped line, default 0. No literal "1.5" —
    /// see `layout::LayoutOptions::line_spacing`'s doc comment.
    pub line_spacing: Valued<u8>,
    /// Extra inter-word gap width in cells, default 0.
    pub word_spacing: Valued<u8>,
    /// PRD FR-RD-9 (v1.x): full justification, default **false** (ragged-right
    /// stays the default). Also `:set justify=on|off`. See
    /// `layout::LayoutOptions::justify`.
    pub justify: Valued<bool>,
    /// PRD FR-RD-9 (v1.x): soft (Knuth-Liang) hyphenation, default false. Also
    /// `:set hyphenate=on|off`. See `layout::LayoutOptions::hyphenate`.
    pub hyphenate: Valued<bool>,
}

/// The resolved `[auth]` table (PRD §5.9, FR-ACC-1, §6.2 rule 2): the OAuth
/// 2.0 client identity and endpoints. All file-only (an OAuth consumer is a
/// set-once registration detail, not a per-run CLI/env concern). The
/// endpoints default to Meta-Wiki's central OAuth (Appendix A) and are
/// overridable so a WMF endpoint move — or pointing at a test server — is a
/// config change, not a release.
#[derive(Debug, Clone)]
pub struct ResolvedAuth {
    /// `[auth] client_id` — the registered OAuth consumer's public client id.
    /// Empty by default: login is unavailable (and `:login` says so) until a
    /// consumer is registered and this is set. There is no secret — wikitui is
    /// a public PKCE client (§5.9).
    pub client_id: Valued<String>,
    /// `[auth] authorize_url` (default [`crate::auth::DEFAULT_AUTHORIZE_URL`]).
    pub authorize_url: Valued<String>,
    /// `[auth] token_url` (default [`crate::auth::DEFAULT_TOKEN_URL`]).
    pub token_url: Valued<String>,
}

/// A schema migration from `from` to `from + 1`, run over the raw table
/// before typed fields are extracted. Empty today (`CONFIG_VERSION` is 1,
/// the first shipped schema) — the table exists so bumping the version
/// later is "add an entry here", not "invent the mechanism then."
struct Migration {
    from: u32,
    #[allow(dead_code)] // exercised once a real migration lands
    apply: fn(&mut toml::Table),
}

const MIGRATIONS: &[Migration] = &[];

/// Resolves the config file's location: `--config` (highest), then
/// `WIKITUI_CONFIG`, then the platform config directory (`directories`
/// crate, matching `cache.rs`/`research.rs`). `None` only when none of the
/// above apply and the platform gives no home directory to fall back to —
/// config loading is then simply skipped, same as a missing file.
pub fn resolve_config_path(cli_path: Option<PathBuf>, env_var: Option<String>) -> Option<PathBuf> {
    if let Some(p) = cli_path {
        return Some(p);
    }
    if let Some(p) = env_var {
        return Some(PathBuf::from(p));
    }
    directories::ProjectDirs::from("", "", "wikitui").map(|d| d.config_dir().join("config.toml"))
}

/// PRD FR-CS-8's first-run sentinel: the absence of the config file. `None`
/// (no config directory at all — e.g. a headless box with no home) is treated
/// as "not a first run" since there is nowhere to write the defaults that
/// would end onboarding, so showing a tour we can't dismiss permanently would
/// be worse than skipping it.
pub fn first_run(config_path: Option<&Path>) -> bool {
    config_path.is_some_and(|p| !p.exists())
}

/// The commented-defaults config file written on first run (PRD FR-CS-8).
/// Every setting is shown commented at its default, so a reader can
/// uncomment and edit rather than hunt the docs — and, crucially, loading it
/// produces an empty table (all comments), so `resolve` reports zero issues
/// and every value keeps its built-in default.
pub const DEFAULT_CONFIG_TEMPLATE: &str = "\
# wikitui configuration. Every setting below is shown at its default,
# commented out. Uncomment and edit the ones you want to change.
# See also: keymap.toml (keybindings) and themes/*.toml in this directory.

# config_version = 1

# Wikipedia language edition and the fallback chain for search/open.
# lang = \"en\"
# languages = [\"en\"]

# Color theme: terminal | full | homebrew | night | paper | contrast.
# theme = \"terminal\"

# Keybinding preset: vim (default) | emacs. Per-key overrides go in keymap.toml.
# keymap = \"vim\"

# Typography (FR-RD-9/10): line measure in cells (40..=200), and how wide
# East-Asian-Ambiguous characters are (1 = narrow, 2 = wide).
# measure = 88
# ambiguous_width = 1

# Reading comfort / spacing options (FR-PC-1) — honestly \"spacing\", not
# letter-spacing (a cell grid can't do that). All also settable per-`:set`
# session-wide, or per-tab via `:set-tab` (measure/images/ambiguous_width
# too). text_align: center (default, floats the measure column in the
# middle of a wide terminal) | left (hugs the left margin instead).
# paragraph_spacing: blank rows between blocks, 0 (tight) .. 4 (airy),
# default 1. line_spacing: blank rows after every wrapped line, 0..=2,
# default 0 — there is no literal \"1.5\"; a true half-row isn't renderable,
# so `line_spacing = 1` is offered as the closest honest approximation.
# word_spacing: extra cells in every inter-word gap, 0 or 1, default 0.
# justify (FR-RD-9): stretch each wrapped line's gaps to a flush right edge,
# default false (ragged-right); the last line of a paragraph and lines too
# sparse to fill without an ugly whitespace river stay ragged. hyphenate
# (FR-RD-9): break long words at Knuth-Liang points (en-US patterns) with a
# trailing \"-\", default false. Both also via `:set justify=on|off` /
# `:set hyphenate=on|off`, or per-tab with `:set-tab`.
# [reading]
# margin = 0
# text_align = \"center\"
# paragraph_spacing = 1
# line_spacing = 0
# word_spacing = 0
# justify = false
# hyphenate = false

# Citation style for Research mode: apa | harvard | mla | chicago.
# cite_style = \"apa\"

# Start page (FR-DL-1): feed | blank | resume.
# startpage = \"feed\"

# Session auto-restore (FR-TB-5): reopen the last session's tabs on start.
# An alternative to `startpage = \"resume\"` for readers who want a
# different startpage look but still want their tabs back.
# restore_session = false

# Inline images (FR-TH-7): follow the theme unless set here.
# images = \"on\"

# Prefetch (FR-PF-*): the kill switch and byte/request budgets.
# [prefetch]
# enabled = true
# daily_mb = 20
# hourly_requests = 100

# Page cache (FR-OFF-*).
# [cache]
# max_mb = 500

# Mouse support (FR-NV-9): off by default so the terminal's own native text
# selection/copy keeps working untouched; `:set mouse=on` to enable scroll
# wheel, click-to-follow-link, TOC-entry, and tab-bar clicks. Every mouse
# action stays keyboard-reachable regardless (FR-ACS-3).
# mouse = false

# No-motion mode (FR-ACS-4): full | none. Also honors WIKITUI_ANIMATIONS and
# ACCESSIBLE=1 (which implies none unless overridden here).
# animations = \"full\"

# Auto light/dark (FR-TH-4): query the terminal's background color at
# startup and pick theme_light/theme_dark accordingly. Off by default; an
# explicit theme above always wins over this when both are set.
# auto_theme = false
# theme_light = \"paper\"
# theme_dark = \"terminal\"

# Color depth (FR-TH-3): auto | truecolor | 256 | 16 | mono. auto detects
# from $COLORTERM/$TERM; an explicit value overrides detection (also honors
# WIKITUI_COLOR_DEPTH). Every theme's colors are mapped down to this depth.
# color_depth = \"auto\"

# OSC 8 terminal hyperlinks (FR-RD-2): auto | on | off. ACCESSIBLE=1 implies
# off (plain link text) unless overridden here.
# hyperlinks = \"auto\"
";

/// Write [`DEFAULT_CONFIG_TEMPLATE`] to `config_path`, creating parent
/// directories as needed (PRD FR-CS-8). Returns `Ok(true)` when a file was
/// written, `Ok(false)` when there was nothing to do — no config path, or a
/// file already exists (never clobber a user's config). The write is
/// best-effort: any I/O error is returned for the caller to surface, never
/// panicked.
pub fn write_default_config(config_path: Option<&Path>) -> std::io::Result<bool> {
    let Some(path) = config_path else {
        return Ok(false);
    };
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DEFAULT_CONFIG_TEMPLATE)?;
    Ok(true)
}

/// Loads and resolves the full configuration. `config_path` is the
/// already-resolved file location (see `resolve_config_path`) — `None`
/// (or a nonexistent path) simply means "no file", not an error: a fresh
/// install with no config is the common case, not a broken one.
pub fn resolve(
    cli: &CliOverrides,
    env: &EnvOverrides,
    config_path: Option<&Path>,
) -> ResolvedConfig {
    let mut issues = Vec::new();
    let table = load_table(config_path, &mut issues);

    // PRD FR-TH-1: "a user theme's name joins Theme::NAMES-equivalent
    // resolution" — `theme`/`theme_light`/`theme_dark` below all validate
    // against the built-ins *plus* whatever `themes/*.toml` the config
    // directory holds. A cheap re-scan (parse errors and low-contrast
    // findings are silently discarded here — the real load, which warns on
    // both, happens once at startup in `main`; this is a name-membership
    // pre-check only) rather than threading a pre-loaded theme list through
    // `resolve`'s signature, which every existing caller (including every
    // test in this module) would otherwise have to grow a new parameter for.
    let user_theme_names: Vec<String> = crate::theme::user_theme_names(
        config_path
            .and_then(Path::parent)
            .map(|d| d.join("themes"))
            .as_deref(),
    );
    let theme_name_is_known = |s: &str| -> bool {
        Theme::by_name(s).is_some() || user_theme_names.iter().any(|n| n == s)
    };

    let known_top_level: BTreeSet<&str> = [
        "config_version",
        "lang",
        "languages",
        "theme",
        "keymap",
        "ambiguous_width",
        "measure",
        "cite_style",
        "cache",
        "active_wiki",
        "wiki",
        "readlater_auto_dequeue",
        "history",
        "images",
        "include_nonfree",
        "pro",
        "startpage",
        "restore_session",
        "reading_wpm",
        "prefetch",
        "network",
        "mouse",
        "animations",
        "auto_theme",
        "theme_light",
        "theme_dark",
        "hyperlinks",
        "color_depth",
        "reading",
        "watchlist_mirror_tag",
        "tts_command",
        "command",
    ]
    .into_iter()
    .collect();
    for key in table.keys() {
        if !known_top_level.contains(key.as_str()) {
            issues.push(Issue::warning(format!(
                "unknown config key '{key}' — ignored"
            )));
        }
    }

    let config_version = resolve_config_version(&table, &mut issues);
    if let Some(summary) = &config_version.1 {
        // Reported alongside issues, not folded into the version's own
        // `Valued` — it's a one-time migration note, not a value source.
        issues.push(Issue::warning(summary.clone()));
    }

    let lang = resolve_lang(cli, env, &table, &mut issues);
    let languages = resolve_languages(&table, &mut issues);
    let theme = resolve_string_field(
        "theme",
        cli.theme.as_deref(),
        env.theme.as_deref(),
        table.get("theme"),
        "terminal",
        |s| {
            if theme_name_is_known(s) {
                Ok(s.to_string())
            } else {
                Err(format!(
                    "unknown theme {s:?} — one of: {} (or a themes/*.toml name)",
                    Theme::NAMES.join(", ")
                ))
            }
        },
        &mut issues,
    );
    let keymap_preset = resolve_string_field(
        "keymap",
        None,
        None,
        table.get("keymap"),
        "vim",
        |s| match s {
            "vim" | "emacs" => Ok(s.to_string()),
            other => Err(format!(
                "unknown keymap preset {other:?} — one of: vim, emacs"
            )),
        },
        &mut issues,
    );
    let cite_style = resolve_string_field(
        "cite_style",
        cli.cite_style.as_deref(),
        env.cite_style.as_deref(),
        table.get("cite_style"),
        "apa",
        |s| {
            if CiteStyle::by_name(s).is_some() {
                Ok(s.to_string())
            } else {
                Err(format!(
                    "unknown citation style {s:?} — one of: {}",
                    CiteStyle::NAMES.join(", ")
                ))
            }
        },
        &mut issues,
    );
    let measure = resolve_measure(cli, env, &table, &mut issues);
    let ambiguous_wide = resolve_ambiguous_width(cli, env, &table, &mut issues);
    let (cache_max_mb, cache_fresh_ttl_hours, cache_force_refetch_days, cache_dir) =
        resolve_cache(&table, &mut issues);
    let (active_wiki, base_url_template, wiki_capabilities, wiki_registry) =
        resolve_wiki(cli, env, &table, &mut issues);
    let readlater_auto_dequeue = resolve_readlater_auto_dequeue(env, &table, &mut issues);
    let history_retention_days = resolve_history(&table, &mut issues);
    let interest_learning = resolve_interest_learning(&table, &mut issues);
    let interest_half_life_days = resolve_interest_half_life(&table, &mut issues);
    let images = resolve_images(env, &table, &mut issues);
    let include_nonfree = resolve_include_nonfree(&table, &mut issues);
    let pro = resolve_pro(&table, &mut issues);
    let startpage = resolve_startpage(&table, &mut issues);
    let restore_session = resolve_restore_session(&table, &mut issues);
    let reading_wpm = resolve_reading_wpm(&table, &mut issues);
    let prefetch = resolve_prefetch(env, &table, &mut issues);
    let network_contact = resolve_network_contact(env, &table, &mut issues);
    let terminal = resolve_terminal(env, &table, &user_theme_names, &mut issues);
    let reading = resolve_reading(&table, &mut issues);
    let auth = resolve_auth(&table, &mut issues);
    let watchlist_mirror_tag = resolve_watchlist_mirror_tag(&table, &mut issues);
    let tts_command = resolve_tts_command(&table, &mut issues);
    let macros = resolve_macros(&table, &mut issues);

    ResolvedConfig {
        config_version: Valued {
            value: config_version.0,
            source: if table.contains_key("config_version") {
                Source::File
            } else {
                Source::Default
            },
        },
        lang,
        languages,
        theme,
        keymap_preset,
        measure,
        ambiguous_wide,
        cite_style,
        cache_max_mb,
        cache_fresh_ttl_hours,
        cache_force_refetch_days,
        cache_dir,
        active_wiki,
        base_url_template,
        wiki_capabilities,
        wiki_registry,
        readlater_auto_dequeue,
        history_retention_days,
        interest_learning,
        interest_half_life_days,
        images,
        include_nonfree,
        pro,
        startpage,
        restore_session,
        reading_wpm,
        prefetch,
        network_contact,
        terminal,
        reading,
        auth,
        watchlist_mirror_tag,
        tts_command,
        macros,
        migration_summary: config_version.1,
        issues,
        config_path: config_path.map(Path::to_path_buf),
    }
}

/// Reads and parses the file into a table. A missing file is silent (the
/// common case); a syntax error is `IssueLevel::Error` (the whole file is
/// unusable, so every key below simply falls back to its default) with the
/// `toml` crate's own message, which already names the line/column.
fn load_table(config_path: Option<&Path>, issues: &mut Vec<Issue>) -> toml::Table {
    let Some(path) = config_path else {
        return toml::Table::new();
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return toml::Table::new(),
        Err(e) => {
            issues.push(Issue::warning(format!(
                "could not read config file {}: {e}",
                path.display()
            )));
            return toml::Table::new();
        }
    };
    match toml::from_str::<toml::Table>(&text) {
        Ok(table) => table,
        Err(e) => {
            issues.push(Issue::error(format!(
                "config file {} has a syntax error, using defaults:\n{e}",
                path.display()
            )));
            toml::Table::new()
        }
    }
}

fn resolve_config_version(table: &toml::Table, issues: &mut Vec<Issue>) -> (u32, Option<String>) {
    let Some(raw) = table.get("config_version") else {
        return (CONFIG_VERSION, None);
    };
    let Some(n) = raw.as_integer().filter(|n| *n >= 0) else {
        issues.push(Issue::warning(format!(
            "config_version must be a non-negative integer, got {raw}; assuming {CONFIG_VERSION}"
        )));
        return (CONFIG_VERSION, None);
    };
    let file_version = n as u32;
    match file_version.cmp(&CONFIG_VERSION) {
        std::cmp::Ordering::Equal => (file_version, None),
        std::cmp::Ordering::Less => {
            let applied = migrate(file_version);
            (
                CONFIG_VERSION,
                Some(format!(
                    "migrated config from version {file_version} to {CONFIG_VERSION} ({applied} step(s) applied)"
                )),
            )
        }
        std::cmp::Ordering::Greater => {
            issues.push(Issue::warning(format!(
                "config_version {file_version} is from a newer wikitui than this build ({CONFIG_VERSION}); some settings may be ignored"
            )));
            (file_version, None)
        }
    }
}

/// Runs every migration from `from_version` up to `CONFIG_VERSION`,
/// returning how many applied. A no-op today since `MIGRATIONS` is empty.
fn migrate(from_version: u32) -> usize {
    let mut table = toml::Table::new();
    let mut applied = 0;
    for v in from_version..CONFIG_VERSION {
        if let Some(m) = MIGRATIONS.iter().find(|m| m.from == v) {
            (m.apply)(&mut table);
            applied += 1;
        }
    }
    applied
}

fn resolve_lang(
    cli: &CliOverrides,
    env: &EnvOverrides,
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> Valued<String> {
    let file_lang = table.get("lang").and_then(toml::Value::as_str);
    let validate = |s: &str| -> Result<String, String> {
        if target::is_lang_code(s) {
            Ok(s.to_string())
        } else {
            Err(format!("{s:?} doesn't look like a language code"))
        }
    };
    let resolved = resolve_string_field(
        "lang",
        cli.lang.as_deref(),
        env.lang.as_deref(),
        None, // handled explicitly below so the `languages` fallback applies
        "__unset__",
        validate,
        issues,
    );
    if resolved.source != Source::Default {
        return resolved;
    }
    // No CLI/env override: the file's own `lang` key wins next.
    if let Some(raw) = file_lang {
        match validate(raw) {
            Ok(value) => {
                return Valued {
                    value,
                    source: Source::File,
                };
            }
            Err(reason) => issues.push(Issue::warning(format!(
                "lang: {reason} (from config file); trying 'languages'"
            ))),
        }
    }
    // FR-ML-2 groundwork: `languages`'s first (validated) entry stands in
    // for a default lang when `lang` itself isn't set.
    if let Some(raw) = table.get("languages").and_then(toml::Value::as_array) {
        for entry in raw {
            if let Some(s) = entry.as_str()
                && target::is_lang_code(s)
            {
                return Valued {
                    value: s.to_string(),
                    source: Source::File,
                };
            }
        }
    }
    Valued {
        value: "en".to_string(),
        source: Source::Default,
    }
}

fn resolve_languages(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<Vec<String>> {
    let Some(raw) = table.get("languages") else {
        return Valued {
            value: Vec::new(),
            source: Source::Default,
        };
    };
    let Some(array) = raw.as_array() else {
        issues.push(Issue::warning(
            "languages must be an array of language codes; ignoring",
        ));
        return Valued {
            value: Vec::new(),
            source: Source::Default,
        };
    };
    let mut codes = Vec::new();
    for entry in array {
        match entry.as_str() {
            Some(s) if target::is_lang_code(s) => codes.push(s.to_string()),
            other => issues.push(Issue::warning(format!(
                "languages: {other:?} doesn't look like a language code — skipped"
            ))),
        }
    }
    if codes.is_empty() {
        Valued {
            value: Vec::new(),
            source: Source::Default,
        }
    } else {
        Valued {
            value: codes,
            source: Source::File,
        }
    }
}

/// The shared shape behind `theme`/`cite_style` (and `lang`'s CLI/env
/// layers): try CLI, then env, then the file's raw value, each validated
/// the same way; the first *present* layer wins outright — a present but
/// invalid value falls back straight to the default (not the next layer
/// down), so "what does :config doctor say is active" always matches
/// "what did the highest-precedence layer that set anything actually say."
#[allow(clippy::too_many_arguments)]
fn resolve_string_field(
    field_name: &str,
    cli: Option<&str>,
    env: Option<&str>,
    file: Option<&toml::Value>,
    default: &str,
    validate: impl Fn(&str) -> Result<String, String>,
    issues: &mut Vec<Issue>,
) -> Valued<String> {
    let file_str = file.and_then(toml::Value::as_str);
    let file_invalid = file.is_some() && file_str.is_none();
    for (raw, source) in [
        (cli, Source::Cli),
        (env, Source::Env),
        (file_str, Source::File),
    ] {
        if let Some(raw) = raw {
            return match validate(raw) {
                Ok(value) => Valued { value, source },
                Err(reason) => {
                    issues.push(Issue::warning(format!(
                        "{field_name}: {reason} (from {source}); using default {default:?}"
                    )));
                    Valued {
                        value: default.to_string(),
                        source: Source::Default,
                    }
                }
            };
        }
    }
    if file_invalid {
        issues.push(Issue::warning(format!(
            "{field_name} must be a string; using default {default:?}"
        )));
    }
    Valued {
        value: default.to_string(),
        source: Source::Default,
    }
}

fn resolve_measure(
    cli: &CliOverrides,
    env: &EnvOverrides,
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> Valued<u16> {
    const MIN: i64 = MEASURE_MIN as i64;
    const MAX: i64 = MEASURE_MAX as i64;
    const DEFAULT: u16 = 88;

    let cli_raw = cli.measure.map(|m| m.to_string());
    let file_raw = table.get("measure");
    let file_invalid = file_raw.is_some_and(|v| v.as_integer().is_none());

    for (raw, source) in [
        (cli_raw.as_deref(), Source::Cli),
        (env.measure.as_deref(), Source::Env),
    ] {
        if let Some(raw) = raw {
            match raw.parse::<i64>() {
                Ok(n) => return clamp_measure(n, MIN, MAX, source, issues),
                Err(_) => issues.push(Issue::warning(format!(
                    "measure: {raw:?} (from {source}) is not an integer; using default {DEFAULT}"
                ))),
            }
        }
    }
    if let Some(n) = file_raw.and_then(toml::Value::as_integer) {
        return clamp_measure(n, MIN, MAX, Source::File, issues);
    }
    if file_invalid {
        issues.push(Issue::warning(format!(
            "measure must be an integer; using default {DEFAULT}"
        )));
    }
    Valued {
        value: DEFAULT,
        source: Source::Default,
    }
}

fn clamp_measure(
    n: i64,
    min: i64,
    max: i64,
    source: Source,
    issues: &mut Vec<Issue>,
) -> Valued<u16> {
    let clamped = n.clamp(min, max);
    if clamped != n {
        issues.push(Issue::warning(format!(
            "measure {n} (from {source}) is outside the sane {min}..={max} range; clamped to {clamped}"
        )));
    }
    Valued {
        value: clamped as u16,
        source,
    }
}

/// `1` -> `false`, `2` -> `true` (FR-RD-10); anything else is invalid.
fn validate_ambiguous_width(n: i64, source: Source, issues: &mut Vec<Issue>) -> Option<bool> {
    match n {
        1 => Some(false),
        2 => Some(true),
        other => {
            issues.push(Issue::warning(format!(
                "ambiguous_width must be 1 or 2 (from {source}), got {other}; using default 1"
            )));
            None
        }
    }
}

fn resolve_ambiguous_width(
    cli: &CliOverrides,
    env: &EnvOverrides,
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> Valued<bool> {
    let default = Valued {
        value: false,
        source: Source::Default,
    };

    if let Some(n) = cli.ambiguous_width {
        return match validate_ambiguous_width(i64::from(n), Source::Cli, issues) {
            Some(value) => Valued {
                value,
                source: Source::Cli,
            },
            None => default,
        };
    }
    if let Some(raw) = &env.ambiguous_width {
        return match raw.parse::<i64>() {
            Ok(n) => match validate_ambiguous_width(n, Source::Env, issues) {
                Some(value) => Valued {
                    value,
                    source: Source::Env,
                },
                None => default,
            },
            Err(_) => {
                issues.push(Issue::warning(format!(
                    "ambiguous_width: {raw:?} (from environment) is not an integer; using default 1"
                )));
                default
            }
        };
    }
    match table.get("ambiguous_width") {
        Some(v) => match v.as_integer() {
            Some(n) => match validate_ambiguous_width(n, Source::File, issues) {
                Some(value) => Valued {
                    value,
                    source: Source::File,
                },
                None => default,
            },
            None => {
                issues.push(Issue::warning(
                    "ambiguous_width must be an integer (1 or 2); using default 1".to_string(),
                ));
                default
            }
        },
        None => default,
    }
}

/// Loose boolean parsing for the env-var layer (process env only ever
/// carries strings, unlike TOML's native `bool`): the usual truthy/falsy
/// spellings, case-insensitively.
fn parse_bool_ish(raw: &str) -> Option<bool> {
    match raw.to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// PRD FR-BM-3's "(config)" `readlater_auto_dequeue`: file + env only (no
/// CLI flag — like the `cache.*` settings, this is a preference someone sets
/// once in `config.toml`, not something worth a flag for a single run).
fn resolve_readlater_auto_dequeue(
    env: &EnvOverrides,
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> Valued<bool> {
    const DEFAULT: bool = true;
    let default = Valued {
        value: DEFAULT,
        source: Source::Default,
    };

    if let Some(raw) = &env.readlater_auto_dequeue {
        return match parse_bool_ish(raw) {
            Some(value) => Valued {
                value,
                source: Source::Env,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "readlater_auto_dequeue: {raw:?} (from environment) is not a boolean; using default {DEFAULT}"
                )));
                default
            }
        };
    }
    match table.get("readlater_auto_dequeue") {
        Some(v) => match v.as_bool() {
            Some(value) => Valued {
                value,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "readlater_auto_dequeue must be a boolean; using default {DEFAULT}"
                )));
                default
            }
        },
        None => default,
    }
}

/// PRD FR-TH-7's `images` override of the active theme's default: env
/// (`WIKITUI_IMAGES`) then file. `None` (the default) means "follow the
/// theme"; `on`/`off`/`true`/`false` force it. Wired into
/// `App::images_override`.
fn resolve_images(
    env: &EnvOverrides,
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> Valued<Option<bool>> {
    let default = Valued {
        value: None,
        source: Source::Default,
    };
    if let Some(raw) = &env.images {
        return match parse_bool_ish(raw) {
            Some(value) => Valued {
                value: Some(value),
                source: Source::Env,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "images: {raw:?} (from environment) is not a boolean; following the theme"
                )));
                default
            }
        };
    }
    match table.get("images") {
        Some(v) => match v.as_bool() {
            Some(value) => Valued {
                value: Some(value),
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(
                    "images must be a boolean; following the theme".to_string(),
                ));
                default
            }
        },
        None => default,
    }
}

/// PRD §10 licensing: `include_nonfree` (file-only, default false). A policy
/// flag today — the saved-page image persistence that must honor it is a
/// later chunk (documented seam).
fn resolve_include_nonfree(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<bool> {
    const DEFAULT: bool = false;
    let default = Valued {
        value: DEFAULT,
        source: Source::Default,
    };
    match table.get("include_nonfree") {
        Some(v) => match v.as_bool() {
            Some(value) => Valued {
                value,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "include_nonfree must be a boolean; using default {DEFAULT}"
                )));
                default
            }
        },
        None => default,
    }
}

/// PRD FR-DL-8's `pro = true` (file-only, default false): disables every
/// easter egg and achievement toast. Mirrors `resolve_include_nonfree`'s
/// shape exactly (a plain policy bool with no CLI/env surface).
fn resolve_pro(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<bool> {
    const DEFAULT: bool = false;
    let default = Valued {
        value: DEFAULT,
        source: Source::Default,
    };
    match table.get("pro") {
        Some(v) => match v.as_bool() {
            Some(value) => Valued {
                value,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "pro must be a boolean; using default {DEFAULT}"
                )));
                default
            }
        },
        None => default,
    }
}

/// PRD FR-TB-5's `restore_session` (file-only, default false) — mirrors
/// `resolve_include_nonfree`'s shape exactly (a plain policy bool with no
/// CLI/env surface).
fn resolve_restore_session(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<bool> {
    const DEFAULT: bool = false;
    let default = Valued {
        value: DEFAULT,
        source: Source::Default,
    };
    match table.get("restore_session") {
        Some(v) => match v.as_bool() {
            Some(value) => Valued {
                value,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "restore_session must be a boolean; using default {DEFAULT}"
                )));
                default
            }
        },
        None => default,
    }
}

/// PRD FR-DL-1's `startpage`: `feed|blank|resume`, default `feed`. Mirrors
/// `resolve_metered`'s shape (a closed set of spellings, invalid falls back
/// straight to the default rather than partially applying).
fn resolve_startpage(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<String> {
    const DEFAULT: &str = "feed";
    let default = Valued {
        value: DEFAULT.to_string(),
        source: Source::Default,
    };
    match table.get("startpage") {
        None => default,
        Some(v) => match v.as_str() {
            Some(s) if crate::startpage::StartPageConfig::parse(s).is_some() => Valued {
                value: s.to_string(),
                source: Source::File,
            },
            _ => {
                issues.push(Issue::warning(format!(
                    "startpage must be one of feed|blank|resume; using default {DEFAULT}"
                )));
                default
            }
        },
    }
}

/// PRD FR-BM-6's designated mirror tag (file-only, default `"watched"`,
/// mirroring `resolve_startpage`'s shape). A leading `#` some users will type
/// out of habit is stripped, same convention as `bookmarks::parse_tags`;
/// anything left blank after that falls back to the default with a warning
/// — an empty tag can never match `Bookmark::tags`.
fn resolve_watchlist_mirror_tag(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<String> {
    const DEFAULT: &str = "watched";
    let default = Valued {
        value: DEFAULT.to_string(),
        source: Source::Default,
    };
    match table.get("watchlist_mirror_tag") {
        None => default,
        Some(v) => match v.as_str() {
            Some(s) => {
                let trimmed = s.strip_prefix('#').unwrap_or(s).trim();
                if trimmed.is_empty() {
                    issues.push(Issue::warning(format!(
                        "watchlist_mirror_tag must be a non-empty tag name; using default {DEFAULT:?}"
                    )));
                    default
                } else {
                    Valued {
                        value: trimmed.to_string(),
                        source: Source::File,
                    }
                }
            }
            None => {
                issues.push(Issue::warning(format!(
                    "watchlist_mirror_tag must be a string; using default {DEFAULT:?}"
                )));
                default
            }
        },
    }
}

/// PRD FR-PC-2 / SEC-5: the TTS command, file-only, unset by default. Never
/// falls back to a guessed platform default (unlike, say, `$EDITOR`'s own
/// convention) — an unset value means the feature stays off until the
/// reader opts in explicitly, and a set-but-blank string is treated the
/// same as unset (with a warning), since `tts::parse_command_line` would
/// otherwise have nothing to spawn.
fn resolve_tts_command(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<Option<String>> {
    let unset = Valued {
        value: None,
        source: Source::Default,
    };
    match table.get("tts_command") {
        None => unset,
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => Valued {
                value: Some(s.trim().to_string()),
                source: Source::File,
            },
            Some(_) => {
                issues.push(Issue::warning(
                    "tts_command must be a non-empty string; TTS stays disabled",
                ));
                unset
            }
            None => {
                issues.push(Issue::warning(
                    "tts_command must be a string; TTS stays disabled",
                ));
                unset
            }
        },
    }
}

/// PRD FR-CS-5: `[command.<name>] run = ["step1", "step2", ...]` — named
/// macros, resolved the same "table of named sections" shape `resolve_wiki`
/// already uses for `[wiki.<name>]`. Each step is checked against
/// `macros::step_names_a_known_command`'s grammar ("validate at load, warn
/// on unknown") — an unrecognized step *warns*, but the macro is still
/// registered with that step left in place: `main::run_macro`'s own
/// per-step error policy (report the failing step, run the rest) already
/// has to handle "this step doesn't resolve to anything" at run time
/// regardless (config can be hand-edited after this load, or reloaded via
/// `:config reload`), so dropping the whole macro here would just move the
/// same partial-failure outcome earlier without avoiding it — and would
/// silently discard the *valid* steps of an otherwise-working macro over
/// one typo.
fn resolve_macros(
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut macros = std::collections::BTreeMap::new();
    let Some(sections) = table.get("command").and_then(toml::Value::as_table) else {
        if table.get("command").is_some() {
            issues.push(Issue::warning(
                "command must be a table of [command.<name>] sections; ignoring",
            ));
        }
        return macros;
    };
    for (name, section) in sections {
        let Some(section_table) = section.as_table() else {
            issues.push(Issue::warning(format!(
                "command.{name} must be a table with a run key; ignoring"
            )));
            continue;
        };
        for key in section_table.keys() {
            if key != "run" {
                issues.push(Issue::warning(format!(
                    "unknown config key 'command.{name}.{key}' — ignored"
                )));
            }
        }
        let Some(run) = section_table.get("run") else {
            issues.push(Issue::warning(format!(
                "command.{name} has no run key; ignoring"
            )));
            continue;
        };
        let Some(steps) = run.as_array() else {
            issues.push(Issue::warning(format!(
                "command.{name}.run must be an array of strings; ignoring"
            )));
            continue;
        };
        let mut parsed_steps = Vec::with_capacity(steps.len());
        let mut malformed = false;
        for step in steps {
            match step.as_str() {
                Some(s) if !s.trim().is_empty() => parsed_steps.push(s.trim().to_string()),
                _ => {
                    issues.push(Issue::warning(format!(
                        "command.{name}.run must be an array of non-empty strings; ignoring the whole macro"
                    )));
                    malformed = true;
                    break;
                }
            }
        }
        if malformed {
            continue;
        }
        if parsed_steps.is_empty() {
            issues.push(Issue::warning(format!(
                "command.{name}.run is empty; ignoring"
            )));
            continue;
        }
        for step in &parsed_steps {
            if !crate::macros::step_names_a_known_command(step) {
                issues.push(Issue::warning(format!(
                    "command.{name}.run: unknown command/action {step:?} — will warn again if run"
                )));
            }
        }
        macros.insert(name.clone(), parsed_steps);
    }
    macros
}

/// PRD FR-RD-11's reading-time WPM divisor, default 230 (an average adult
/// silent-reading rate). File only — unlike `measure`, no CLI flag or env
/// var (a preference set once, matching `startpage`/`include_nonfree`'s own
/// scope); also settable at runtime via `:set reading_wpm=N`. Clamped to a
/// sane 50..=2000: below 50 turns any real article into an implausible
/// "N hour read", and above 2000 is faster than silent reading gets either
/// way the number stops being useful feedback, so the default is a better
/// fallback than an obviously-wrong outlier.
fn resolve_reading_wpm(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<u32> {
    const MIN: i64 = READING_WPM_MIN as i64;
    const MAX: i64 = READING_WPM_MAX as i64;
    const DEFAULT: u32 = 230;
    let default = Valued {
        value: DEFAULT,
        source: Source::Default,
    };
    match table.get("reading_wpm") {
        None => default,
        Some(v) => match v.as_integer() {
            Some(n) => {
                let clamped = n.clamp(MIN, MAX);
                if clamped != n {
                    issues.push(Issue::warning(format!(
                        "reading_wpm {n} is outside the sane {MIN}..={MAX} range; clamped to {clamped}"
                    )));
                }
                Valued {
                    value: clamped as u32,
                    source: Source::File,
                }
            }
            None => {
                issues.push(Issue::warning(format!(
                    "reading_wpm must be an integer; using default {DEFAULT}"
                )));
                default
            }
        },
    }
}

/// PRD §5.8 / FR-PF-1..6: the `[prefetch]` table. Budgets and weights are
/// preferences set once (file/env), like `cache.*` — no CLI flag. The runtime
/// kill switch is `:set prefetch=off` (FR-PF-6), separate from this resolved
/// default.
fn resolve_prefetch(
    env: &EnvOverrides,
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> ResolvedPrefetch {
    let pt: Option<&toml::Table> = table.get("prefetch").and_then(toml::Value::as_table);
    if table.contains_key("prefetch") && pt.is_none() {
        issues.push(Issue::warning(
            "prefetch must be a table (use [prefetch] with enabled/daily_mb/hourly_requests/metered/top_n/weight_lead/weight_pageviews/weight_affinity); ignoring",
        ));
    }
    if let Some(t) = pt {
        let known: BTreeSet<&str> = [
            "enabled",
            "daily_mb",
            "hourly_requests",
            "metered",
            "top_n",
            "weight_lead",
            "weight_pageviews",
            "weight_affinity",
        ]
        .into_iter()
        .collect();
        for key in t.keys() {
            if !known.contains(key.as_str()) {
                issues.push(Issue::warning(format!(
                    "unknown config key 'prefetch.{key}' — ignored"
                )));
            }
        }
    }
    let field = |k: &str| pt.and_then(|t| t.get(k));

    ResolvedPrefetch {
        enabled: resolve_prefetch_enabled(env, field("enabled"), issues),
        daily_mb: resolve_positive_int(field("daily_mb"), "prefetch.daily_mb", 20, issues),
        hourly_requests: resolve_positive_int(
            field("hourly_requests"),
            "prefetch.hourly_requests",
            100,
            issues,
        ),
        metered: resolve_metered(field("metered"), issues),
        top_n: resolve_positive_int(field("top_n"), "prefetch.top_n", 5, issues),
        weight_lead: resolve_weight(field("weight_lead"), "prefetch.weight_lead", 1.0, issues),
        weight_pageviews: resolve_weight(
            field("weight_pageviews"),
            "prefetch.weight_pageviews",
            1.0,
            issues,
        ),
        // PRD FR-PF-3 fills this once-zero seam: the interest model now
        // supplies a real w3 affinity term (`interest::InterestModel::
        // affinity_of_title`), so the default weight is live (1.0). It only
        // ever affects a link whose target the reader already read this
        // session — an unseen target's affinity is 0, so this default never
        // reshuffles ranking for a reader with no interest data (and is 0
        // whenever interest learning is off or incognito, since the affinity
        // map is then empty).
        weight_affinity: resolve_weight(
            field("weight_affinity"),
            "prefetch.weight_affinity",
            1.0,
            issues,
        ),
    }
}

/// FR-PF-6 `prefetch.enabled`: env (`WIKITUI_PREFETCH`) then file, default on.
fn resolve_prefetch_enabled(
    env: &EnvOverrides,
    raw: Option<&toml::Value>,
    issues: &mut Vec<Issue>,
) -> Valued<bool> {
    const DEFAULT: bool = true;
    let default = Valued {
        value: DEFAULT,
        source: Source::Default,
    };
    if let Some(raw) = &env.prefetch {
        return match parse_bool_ish(raw) {
            Some(value) => Valued {
                value,
                source: Source::Env,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "prefetch: {raw:?} (from environment) is not a boolean; using default {DEFAULT}"
                )));
                default
            }
        };
    }
    match raw {
        Some(v) => match v.as_bool() {
            Some(value) => Valued {
                value,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "prefetch.enabled must be a boolean; using default {DEFAULT}"
                )));
                default
            }
        },
        None => default,
    }
}

/// FR-PF-5 `prefetch.metered`: `never|reduced|always`, default `reduced`.
fn resolve_metered(raw: Option<&toml::Value>, issues: &mut Vec<Issue>) -> Valued<String> {
    const DEFAULT: &str = "reduced";
    let default = Valued {
        value: DEFAULT.to_string(),
        source: Source::Default,
    };
    match raw {
        None => default,
        Some(v) => match v.as_str() {
            Some(s) if matches!(s, "never" | "reduced" | "always") => Valued {
                value: s.to_string(),
                source: Source::File,
            },
            _ => {
                issues.push(Issue::warning(format!(
                    "prefetch.metered must be one of never|reduced|always; using default {DEFAULT}"
                )));
                default
            }
        },
    }
}

/// FR-PF-1 ranking weight: a non-negative float, `default` when absent or
/// invalid. Accepts integers too (TOML `1` as well as `1.0`).
fn resolve_weight(
    raw: Option<&toml::Value>,
    field_name: &str,
    default: f64,
    issues: &mut Vec<Issue>,
) -> Valued<f64> {
    let fallback = Valued {
        value: default,
        source: Source::Default,
    };
    match raw {
        None => fallback,
        Some(v) => {
            let parsed = v
                .as_float()
                .or_else(|| v.as_integer().map(|n| n as f64))
                .filter(|f| *f >= 0.0 && f.is_finite());
            match parsed {
                Some(value) => Valued {
                    value,
                    source: Source::File,
                },
                None => {
                    issues.push(Issue::warning(format!(
                        "{field_name} must be a non-negative number; using default {default}"
                    )));
                    fallback
                }
            }
        }
    }
}

/// PRD NF-NET-2 `[network] contact`: env (`WIKITUI_CONTACT`) then file, default
/// the project issue tracker. Feeds the User-Agent's `{contact}` token.
fn resolve_network_contact(
    env: &EnvOverrides,
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> Valued<String> {
    let default = Valued {
        value: crate::api::DEFAULT_CONTACT.to_string(),
        source: Source::Default,
    };
    if let Some(c) = &env.contact {
        return Valued {
            value: c.clone(),
            source: Source::Env,
        };
    }
    let nt: Option<&toml::Table> = table.get("network").and_then(toml::Value::as_table);
    if table.contains_key("network") && nt.is_none() {
        issues.push(Issue::warning(
            "network must be a table (use [network] with contact); ignoring",
        ));
    }
    if let Some(t) = nt {
        for key in t.keys() {
            if key != "contact" {
                issues.push(Issue::warning(format!(
                    "unknown config key 'network.{key}' — ignored"
                )));
            }
        }
        if let Some(v) = t.get("contact") {
            match v.as_str() {
                Some(s) if !s.trim().is_empty() => {
                    return Valued {
                        value: s.to_string(),
                        source: Source::File,
                    };
                }
                _ => issues.push(Issue::warning(
                    "network.contact must be a non-empty string; using default",
                )),
            }
        }
    }
    default
}

/// PRD FR-NV-9 / FR-ACS-4 / FR-TH-4 / FR-RD-2: resolves every terminal-
/// integration key. Each is file-only except `animations` (which also honors
/// `WIKITUI_ANIMATIONS` per FR-ACS-4's explicit wording) — mirroring
/// `startpage`/`include_nonfree`'s own "set once in config.toml" scope; `:set`
/// covers the in-session override for the ones that make sense to flip live
/// (mouse, animations, hyperlinks — not `auto_theme`, which only matters at
/// the one-shot startup query).
fn resolve_terminal(
    env: &EnvOverrides,
    table: &toml::Table,
    user_theme_names: &[String],
    issues: &mut Vec<Issue>,
) -> ResolvedTerminal {
    let theme_name_is_known = |s: &str| -> bool {
        Theme::by_name(s).is_some() || user_theme_names.iter().any(|n| n == s)
    };
    ResolvedTerminal {
        mouse: resolve_bool_field("mouse", None, table.get("mouse"), false, issues),
        animations: resolve_closed_string_field(
            "animations",
            env.animations.as_deref(),
            table.get("animations"),
            "full",
            &["full", "none"],
            issues,
        ),
        auto_theme: resolve_bool_field("auto_theme", None, table.get("auto_theme"), false, issues),
        theme_light: resolve_string_field(
            "theme_light",
            None,
            None,
            table.get("theme_light"),
            "paper",
            |s| {
                if theme_name_is_known(s) {
                    Ok(s.to_string())
                } else {
                    Err(format!(
                        "unknown theme {s:?} — one of: {} (or a themes/*.toml name)",
                        Theme::NAMES.join(", ")
                    ))
                }
            },
            issues,
        ),
        theme_dark: resolve_string_field(
            "theme_dark",
            None,
            None,
            table.get("theme_dark"),
            "terminal",
            |s| {
                if theme_name_is_known(s) {
                    Ok(s.to_string())
                } else {
                    Err(format!(
                        "unknown theme {s:?} — one of: {} (or a themes/*.toml name)",
                        Theme::NAMES.join(", ")
                    ))
                }
            },
            issues,
        ),
        hyperlinks: resolve_closed_string_field(
            "hyperlinks",
            None,
            table.get("hyperlinks"),
            "auto",
            &["auto", "on", "off"],
            issues,
        ),
        color_depth: resolve_closed_string_field(
            "color_depth",
            env.color_depth.as_deref(),
            table.get("color_depth"),
            "auto",
            &["auto", "truecolor", "256", "16", "mono"],
            issues,
        ),
    }
}

/// PRD FR-PC-1's `[reading]` table: the spacing/typography options. File
/// only (like `startpage`/`include_nonfree`) — a preference set once; the
/// runtime `:set`/`:set-tab` override layers on top at read time
/// (`App::layout_options`), same config-default-plus-runtime-override split
/// as `measure`/`reading_wpm`.
fn resolve_reading(table: &toml::Table, issues: &mut Vec<Issue>) -> ResolvedReading {
    let rt: Option<&toml::Table> = table.get("reading").and_then(toml::Value::as_table);
    if table.contains_key("reading") && rt.is_none() {
        issues.push(Issue::warning(
            "reading must be a table (use [reading] with margin/text_align/paragraph_spacing/line_spacing/word_spacing); ignoring",
        ));
    }
    if let Some(t) = rt {
        let known: BTreeSet<&str> = [
            "margin",
            "text_align",
            "paragraph_spacing",
            "line_spacing",
            "word_spacing",
            "justify",
            "hyphenate",
        ]
        .into_iter()
        .collect();
        for key in t.keys() {
            if !known.contains(key.as_str()) {
                issues.push(Issue::warning(format!(
                    "unknown config key 'reading.{key}' — ignored"
                )));
            }
        }
    }
    let field = |k: &str| rt.and_then(|t| t.get(k));

    ResolvedReading {
        margin: resolve_bounded_u16("reading.margin", field("margin"), 0, 0, MARGIN_MAX, issues),
        text_align: resolve_closed_string_field(
            "reading.text_align",
            None,
            field("text_align"),
            "center",
            &["center", "left"],
            issues,
        ),
        paragraph_spacing: resolve_bounded_u8(
            "reading.paragraph_spacing",
            field("paragraph_spacing"),
            1,
            0,
            PARAGRAPH_SPACING_MAX,
            issues,
        ),
        line_spacing: resolve_bounded_u8(
            "reading.line_spacing",
            field("line_spacing"),
            0,
            0,
            LINE_SPACING_MAX,
            issues,
        ),
        word_spacing: resolve_bounded_u8(
            "reading.word_spacing",
            field("word_spacing"),
            0,
            0,
            WORD_SPACING_MAX,
            issues,
        ),
        justify: resolve_bool_field("reading.justify", None, field("justify"), false, issues),
        hyphenate: resolve_bool_field("reading.hyphenate", None, field("hyphenate"), false, issues),
    }
}

/// PRD §5.9 / FR-ACC-1's `[auth]` table: the OAuth client id and endpoints,
/// all file-only (see `ResolvedAuth`). Unknown keys warn (never crash),
/// mirroring `resolve_reading`.
fn resolve_auth(table: &toml::Table, issues: &mut Vec<Issue>) -> ResolvedAuth {
    let at: Option<&toml::Table> = table.get("auth").and_then(toml::Value::as_table);
    if table.contains_key("auth") && at.is_none() {
        issues.push(Issue::warning(
            "auth must be a table (use [auth] with client_id/authorize_url/token_url); ignoring",
        ));
    }
    if let Some(t) = at {
        let known: BTreeSet<&str> = ["client_id", "authorize_url", "token_url"]
            .into_iter()
            .collect();
        for key in t.keys() {
            if !known.contains(key.as_str()) {
                issues.push(Issue::warning(format!(
                    "unknown config key 'auth.{key}' — ignored"
                )));
            }
        }
    }
    let field = |k: &str| at.and_then(|t| t.get(k));
    let accept_any = |s: &str| Ok(s.to_string());
    ResolvedAuth {
        client_id: resolve_string_field(
            "auth.client_id",
            None,
            None,
            field("client_id"),
            "",
            accept_any,
            issues,
        ),
        authorize_url: resolve_string_field(
            "auth.authorize_url",
            None,
            None,
            field("authorize_url"),
            crate::auth::DEFAULT_AUTHORIZE_URL,
            accept_any,
            issues,
        ),
        token_url: resolve_string_field(
            "auth.token_url",
            None,
            None,
            field("token_url"),
            crate::auth::DEFAULT_TOKEN_URL,
            accept_any,
            issues,
        ),
    }
}

/// A file-only `u16` field clamped to `[min, max]` with a warning if the
/// configured value fell outside that range — same "clamp, don't reject"
/// leniency as `resolve_measure`/`resolve_reading_wpm`, generalized (like
/// `resolve_weight` already is for the three `[prefetch]` weights) so
/// `reading.margin` doesn't need its own near-duplicate.
fn resolve_bounded_u16(
    field_name: &str,
    raw: Option<&toml::Value>,
    default: u16,
    min: u16,
    max: u16,
    issues: &mut Vec<Issue>,
) -> Valued<u16> {
    let fallback = Valued {
        value: default,
        source: Source::Default,
    };
    match raw {
        None => fallback,
        Some(v) => match v.as_integer() {
            Some(n) => {
                let clamped = n.clamp(min as i64, max as i64);
                if clamped != n {
                    issues.push(Issue::warning(format!(
                        "{field_name} {n} is outside the sane {min}..={max} range; clamped to {clamped}"
                    )));
                }
                Valued {
                    value: clamped as u16,
                    source: Source::File,
                }
            }
            None => {
                issues.push(Issue::warning(format!(
                    "{field_name} must be an integer; using default {default}"
                )));
                fallback
            }
        },
    }
}

/// The `u8` twin of [`resolve_bounded_u16`], for the three FR-PC-1 spacing
/// fields small enough to fit a byte.
fn resolve_bounded_u8(
    field_name: &str,
    raw: Option<&toml::Value>,
    default: u8,
    min: u8,
    max: u8,
    issues: &mut Vec<Issue>,
) -> Valued<u8> {
    let fallback = Valued {
        value: default,
        source: Source::Default,
    };
    match raw {
        None => fallback,
        Some(v) => match v.as_integer() {
            Some(n) => {
                let clamped = n.clamp(min as i64, max as i64);
                if clamped != n {
                    issues.push(Issue::warning(format!(
                        "{field_name} {n} is outside the sane {min}..={max} range; clamped to {clamped}"
                    )));
                }
                Valued {
                    value: clamped as u8,
                    source: Source::File,
                }
            }
            None => {
                issues.push(Issue::warning(format!(
                    "{field_name} must be an integer; using default {default}"
                )));
                fallback
            }
        },
    }
}

/// A plain `key = true|false` file setting with no CLI/env layer (today) —
/// shared shape for `mouse`/`auto_theme` so both validate identically.
fn resolve_bool_field(
    field_name: &str,
    env: Option<&str>,
    file: Option<&toml::Value>,
    default: bool,
    issues: &mut Vec<Issue>,
) -> Valued<bool> {
    let fallback = Valued {
        value: default,
        source: Source::Default,
    };
    if let Some(raw) = env {
        return match parse_bool_ish(raw) {
            Some(value) => Valued {
                value,
                source: Source::Env,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "{field_name}: {raw:?} (from environment) is not a boolean; using default {default}"
                )));
                fallback
            }
        };
    }
    match file {
        None => fallback,
        Some(v) => match v.as_bool() {
            Some(value) => Valued {
                value,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "{field_name} must be a boolean; using default {default}"
                )));
                fallback
            }
        },
    }
}

/// A string field restricted to a fixed, small set of spellings (mirrors
/// `resolve_metered`'s shape, generalized so `animations`/`hyperlinks` share
/// one implementation instead of two near-duplicates) — env then file, each
/// validated against `allowed`; an invalid or wrong-typed value falls
/// straight back to `default` rather than the next layer down, matching
/// `resolve_string_field`'s own "first present layer wins outright" rule.
fn resolve_closed_string_field(
    field_name: &str,
    env: Option<&str>,
    file: Option<&toml::Value>,
    default: &str,
    allowed: &[&str],
    issues: &mut Vec<Issue>,
) -> Valued<String> {
    let fallback = Valued {
        value: default.to_string(),
        source: Source::Default,
    };
    if let Some(raw) = env {
        return if allowed.contains(&raw) {
            Valued {
                value: raw.to_string(),
                source: Source::Env,
            }
        } else {
            issues.push(Issue::warning(format!(
                "{field_name}: {raw:?} (from environment) must be one of: {} — using default {default:?}",
                allowed.join(", ")
            )));
            fallback
        };
    }
    match file {
        None => fallback,
        Some(v) => match v.as_str() {
            Some(s) if allowed.contains(&s) => Valued {
                value: s.to_string(),
                source: Source::File,
            },
            Some(s) => {
                issues.push(Issue::warning(format!(
                    "{field_name}: {s:?} must be one of: {} — using default {default:?}",
                    allowed.join(", ")
                )));
                fallback
            }
            None => {
                issues.push(Issue::warning(format!(
                    "{field_name} must be a string; using default {default:?}"
                )));
                fallback
            }
        },
    }
}

/// PRD FR-ACS-6: `ACCESSIBLE=1` implies a bundle of accessibility-leaning
/// defaults — here, no-motion (`animations=none`) and plain link URLs
/// (`hyperlinks=off`) — **unless** the reader's own config file or
/// `WIKITUI_ANIMATIONS` already set that key explicitly, in which case the
/// explicit setting stands (§6.7's general precedence rule: something more
/// specific than a blanket environment standard always wins).
///
/// Applied as a post-processing step over `resolve`'s plain output — rather
/// than threading an `accessible: bool` through `resolve`'s own signature —
/// so this bundle's precedence logic is independently testable without
/// touching every existing `resolve()` call site and test (`ACCESSIBLE` is
/// itself an environment variable, but a "standard" one honored the same way
/// regardless of the `WIKITUI_*`/file/CLI layering `resolve` already
/// implements — see `main::accessible_active`, mirroring `NO_COLOR`'s own
/// always-on-when-set treatment).
pub fn apply_accessible_bundle(resolved: &mut ResolvedConfig, accessible: bool) {
    if !accessible {
        return;
    }
    if resolved.terminal.animations.source == Source::Default {
        resolved.terminal.animations = Valued {
            value: "none".to_string(),
            source: Source::Env,
        };
    }
    if resolved.terminal.hyperlinks.source == Source::Default {
        resolved.terminal.hyperlinks = Valued {
            value: "off".to_string(),
            source: Source::Env,
        };
    }
}

/// PRD FR-HS-4's `[history] retention_days`: file-only, like `cache.*` —
/// a preference set once in `config.toml`, not worth a CLI flag or env var
/// for a single run. `0` (default) means "keep forever"; the resolver
/// deliberately accepts `0` as a valid, meaningful file value (unlike
/// `resolve_positive_int`, which would reject it) since it's the
/// documented "don't prune" setting, not a mistake.
fn resolve_history(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<u64> {
    const DEFAULT_RETENTION_DAYS: u64 = 0;
    let default = Valued {
        value: DEFAULT_RETENTION_DAYS,
        source: Source::Default,
    };

    let Some(history_table) = table.get("history").and_then(toml::Value::as_table) else {
        if table.contains_key("history") {
            issues.push(Issue::warning(
                "history must be a table (use [history] with retention_days); ignoring",
            ));
        }
        return default;
    };

    let known: BTreeSet<&str> = ["retention_days"].into_iter().collect();
    for key in history_table.keys() {
        if !known.contains(key.as_str()) {
            issues.push(Issue::warning(format!(
                "unknown config key 'history.{key}' — ignored"
            )));
        }
    }

    match history_table.get("retention_days") {
        None => default,
        Some(v) => match v.as_integer().filter(|n| *n >= 0) {
            Some(n) => Valued {
                value: n as u64,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(
                    "history.retention_days must be a non-negative integer; using default 0 (keep forever)",
                ));
                default
            }
        },
    }
}

/// PRD FR-PF-3 / FR-PR-2: top-level `interest_learning` boolean, default
/// `true`. A single key (not a `[interest]` table) — like `include_nonfree`
/// it's one preference, not a family. Incognito overrides it to off at runtime
/// regardless of this value (see `App::interest_active`).
fn resolve_interest_learning(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<bool> {
    const DEFAULT: bool = true;
    let default = Valued {
        value: DEFAULT,
        source: Source::Default,
    };
    match table.get("interest_learning") {
        None => default,
        Some(v) => match v.as_bool() {
            Some(b) => Valued {
                value: b,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(
                    "interest_learning must be a boolean; using default true",
                ));
                default
            }
        },
    }
}

/// PRD FR-PF-3's `interest_half_life_days` (default 30). Must be positive — a
/// zero or negative half-life would disable decay entirely (or worse), so a
/// bad value falls back to the default rather than degrading the model.
fn resolve_interest_half_life(table: &toml::Table, issues: &mut Vec<Issue>) -> Valued<f64> {
    const DEFAULT: f64 = crate::interest::DEFAULT_HALF_LIFE_DAYS;
    let default = Valued {
        value: DEFAULT,
        source: Source::Default,
    };
    match table.get("interest_half_life_days") {
        None => default,
        Some(v) => {
            // Accept either an integer (30) or a float (30.0) spelling.
            let parsed = v.as_float().or_else(|| v.as_integer().map(|n| n as f64));
            match parsed {
                Some(d) if d > 0.0 => Valued {
                    value: d,
                    source: Source::File,
                },
                _ => {
                    issues.push(Issue::warning(format!(
                        "interest_half_life_days must be a positive number; using default {DEFAULT}"
                    )));
                    default
                }
            }
        }
    }
}

fn resolve_cache(
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> (
    Valued<u64>,
    Valued<u64>,
    Valued<u64>,
    Valued<Option<PathBuf>>,
) {
    let default_max_mb = crate::cache::DEFAULT_MAX_BYTES / (1024 * 1024);
    let default_ttl_hours = crate::cache::FRESH_TTL_SECS / 3600;
    let default_force_refetch_days = crate::cache::DEFAULT_FORCE_REFETCH_SECS / 86_400;
    let no_dir_override = Valued {
        value: None,
        source: Source::Default,
    };

    let Some(cache_table) = table.get("cache").and_then(toml::Value::as_table) else {
        if table.contains_key("cache") {
            issues.push(Issue::warning(
                "cache must be a table (use [cache] with max_mb/fresh_ttl_hours/force_refetch_days/dir); ignoring",
            ));
        }
        return (
            Valued {
                value: default_max_mb,
                source: Source::Default,
            },
            Valued {
                value: default_ttl_hours,
                source: Source::Default,
            },
            Valued {
                value: default_force_refetch_days,
                source: Source::Default,
            },
            no_dir_override,
        );
    };

    let known: BTreeSet<&str> = ["max_mb", "fresh_ttl_hours", "force_refetch_days", "dir"]
        .into_iter()
        .collect();
    for key in cache_table.keys() {
        if !known.contains(key.as_str()) {
            issues.push(Issue::warning(format!(
                "unknown config key 'cache.{key}' — ignored"
            )));
        }
    }

    let max_mb = resolve_positive_int(
        cache_table.get("max_mb"),
        "cache.max_mb",
        default_max_mb,
        issues,
    );
    let fresh_ttl_hours = resolve_positive_int(
        cache_table.get("fresh_ttl_hours"),
        "cache.fresh_ttl_hours",
        default_ttl_hours,
        issues,
    );
    let force_refetch_days = resolve_positive_int(
        cache_table.get("force_refetch_days"),
        "cache.force_refetch_days",
        default_force_refetch_days,
        issues,
    );
    let dir = match cache_table.get("dir") {
        None => no_dir_override,
        Some(toml::Value::String(s)) if !s.trim().is_empty() => Valued {
            value: Some(PathBuf::from(s)),
            source: Source::File,
        },
        Some(_) => {
            issues.push(Issue::warning(
                "cache.dir must be a non-empty string path — ignoring",
            ));
            no_dir_override
        }
    };
    (max_mb, fresh_ttl_hours, force_refetch_days, dir)
}

fn resolve_positive_int(
    raw: Option<&toml::Value>,
    field_name: &str,
    default: u64,
    issues: &mut Vec<Issue>,
) -> Valued<u64> {
    match raw {
        None => Valued {
            value: default,
            source: Source::Default,
        },
        Some(v) => match v.as_integer().filter(|n| *n > 0) {
            Some(n) => Valued {
                value: n as u64,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "{field_name} must be a positive integer; using default {default}"
                )));
                Valued {
                    value: default,
                    source: Source::Default,
                }
            }
        },
    }
}

/// PRD §6.2 rule 2 / FR-ML-4/5: which config keys a `[wiki.<name>]` section
/// may set. `base_url` is A2's original key; the other four are this
/// chunk's feature-degradation matrix (`ResolvedWikiCapabilities`).
const KNOWN_WIKI_SECTION_KEYS: [&str; 5] = [
    "base_url",
    "parser",
    "wikifeeds",
    "pageviews",
    "pageassessments",
];

/// Reads one `wiki.<name>.<key>` boolean, warning and falling back to
/// `default_value` on anything present but not a bool. `section` is the
/// whole `[wiki.<name>]` table (or `None` for a name with no section at
/// all, e.g. a sister project nobody configured) — shared by every one of
/// [`ResolvedWikiCapabilities`]'s three boolean fields so the "must be a
/// boolean" validation lives in exactly one place.
fn resolve_wiki_bool(
    section: Option<&toml::Table>,
    wiki_name: &str,
    key: &str,
    default_value: bool,
    issues: &mut Vec<Issue>,
) -> Valued<bool> {
    match section.and_then(|s| s.get(key)) {
        None => Valued {
            value: default_value,
            source: Source::Default,
        },
        Some(v) => match v.as_bool() {
            Some(value) => Valued {
                value,
                source: Source::File,
            },
            None => {
                issues.push(Issue::warning(format!(
                    "wiki.{wiki_name}.{key} must be a boolean; using default"
                )));
                Valued {
                    value: default_value,
                    source: Source::Default,
                }
            }
        },
    }
}

/// Resolves one wiki's full entry (host template + capability matrix) by
/// registry name — shared by every name `:wiki`/the picker can switch to, so
/// the resolution logic (built-in template, section override, capability
/// defaults) lives in exactly one place regardless of whether the name is
/// the active wiki or just one more registry candidate.
fn resolve_wiki_entry(
    name: &str,
    wiki_sections: Option<&toml::Table>,
    issues: &mut Vec<Issue>,
) -> ResolvedWikiEntry {
    let is_wikipedia = name == DEFAULT_WIKI_NAME;
    // PRD FR-ML-4: a known sister project gets its own `{lang}.<project>.org`
    // template even with no `[wiki.<name>]` section at all; an arbitrary
    // name (FR-ML-5's third-party sites) has no built-in template and must
    // supply `base_url` itself, else it falls back to the same
    // `DEFAULT_BASE_URL_TEMPLATE` the built-in wiki uses (matching this
    // function's pre-FR-ML-4 behavior for an active_wiki with no base_url).
    let mut base_url = match crate::sisters::by_name(name) {
        Some(project) => Valued {
            value: project.base_url_template(),
            source: if is_wikipedia {
                Source::Default
            } else {
                Source::File
            },
        },
        None => Valued {
            value: DEFAULT_BASE_URL_TEMPLATE.to_string(),
            source: Source::Default,
        },
    };
    let section = wiki_sections
        .and_then(|s| s.get(name))
        .and_then(toml::Value::as_table);
    if let Some(url) = section
        .and_then(|t| t.get("base_url"))
        .and_then(toml::Value::as_str)
    {
        base_url = Valued {
            value: url.to_string(),
            source: Source::File,
        };
    }

    let wikifeeds = resolve_wiki_bool(section, name, "wikifeeds", is_wikipedia, issues);
    let pageviews = resolve_wiki_bool(section, name, "pageviews", is_wikipedia, issues);
    let pageassessments = resolve_wiki_bool(section, name, "pageassessments", is_wikipedia, issues);
    let parser = match section.and_then(|t| t.get("parser")) {
        None => Valued {
            value: "auto".to_string(),
            source: Source::Default,
        },
        Some(v) => match v.as_str() {
            Some(s) if matches!(s, "auto" | "parsoid" | "legacy") => Valued {
                value: s.to_string(),
                source: Source::File,
            },
            _ => {
                issues.push(Issue::warning(format!(
                    "wiki.{name}.parser must be one of auto, parsoid, legacy; using 'auto'"
                )));
                Valued {
                    value: "auto".to_string(),
                    source: Source::Default,
                }
            }
        },
    };

    ResolvedWikiEntry {
        base_url_template: base_url,
        capabilities: ResolvedWikiCapabilities {
            parser,
            wikifeeds,
            pageviews,
            pageassessments,
        },
    }
}

/// PRD §6.2 rule 1/2, FR-ML-4/5: resolves the active wiki's name and host
/// template (`CliOverrides::active_wiki` > `WIKITUI_BASE_URL`/config file >
/// the built-in `wikipedia` default), its feature-degradation matrix, and
/// the full switch-target registry every `:wiki <name>` invocation consults
/// (`main::switch_wiki`) without re-reading the config file.
///
/// `active_wiki` may name the built-in `wikipedia`, one of the four sister
/// projects ([`crate::sisters`]) with no section required, or any
/// `[wiki.<name>]` section defined in the file — anything else warns and
/// falls back to `wikipedia`, exactly as before this chunk.
fn resolve_wiki(
    cli: &CliOverrides,
    env: &EnvOverrides,
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> (
    Valued<String>,
    Valued<String>,
    ResolvedWikiCapabilities,
    ResolvedWikiRegistry,
) {
    let wiki_sections = table.get("wiki").and_then(toml::Value::as_table);
    if table.get("wiki").is_some() && wiki_sections.is_none() {
        issues.push(Issue::warning(
            "wiki must be a table of [wiki.<name>] sections; ignoring",
        ));
    }
    if let Some(sections) = wiki_sections {
        for (name, section) in sections {
            let Some(section_table) = section.as_table() else {
                issues.push(Issue::warning(format!(
                    "wiki.{name} must be a table with a base_url key; ignoring"
                )));
                continue;
            };
            for key in section_table.keys() {
                if !KNOWN_WIKI_SECTION_KEYS.contains(&key.as_str()) {
                    issues.push(Issue::warning(format!(
                        "unknown config key 'wiki.{name}.{key}' — ignored"
                    )));
                }
            }
            if section_table.contains_key("base_url")
                && section_table
                    .get("base_url")
                    .and_then(toml::Value::as_str)
                    .is_none()
            {
                issues.push(Issue::warning(format!(
                    "wiki.{name}.base_url must be a string; ignoring that section"
                )));
            }
        }
    }

    let known_name = |name: &str| {
        name == DEFAULT_WIKI_NAME
            || crate::sisters::by_name(name).is_some()
            || wiki_sections.is_some_and(|s| s.contains_key(name))
    };

    let active_name = if let Some(name) = &cli.active_wiki {
        // PRD FR-ML-4: the TITLE argument's own sister-project URL/prefix
        // (`target::parse`'s `project` field) always names a known sister
        // project, so this branch never actually hits the warning path in
        // practice — kept for the same "never silently do something
        // surprising" reason the file-sourced branch below has one.
        if known_name(name) {
            Valued {
                value: name.clone(),
                source: Source::Cli,
            }
        } else {
            issues.push(Issue::warning(format!(
                "active_wiki {name:?} has no matching [wiki.{name}] section; using '{DEFAULT_WIKI_NAME}'"
            )));
            Valued {
                value: DEFAULT_WIKI_NAME.to_string(),
                source: Source::Default,
            }
        }
    } else {
        match table.get("active_wiki") {
            Some(v) => match v.as_str() {
                Some(name) => {
                    if known_name(name) {
                        Valued {
                            value: name.to_string(),
                            source: Source::File,
                        }
                    } else {
                        issues.push(Issue::warning(format!(
                            "active_wiki {name:?} has no matching [wiki.{name}] section; using '{DEFAULT_WIKI_NAME}'"
                        )));
                        Valued {
                            value: DEFAULT_WIKI_NAME.to_string(),
                            source: Source::Default,
                        }
                    }
                }
                None => {
                    issues.push(Issue::warning("active_wiki must be a string; ignoring"));
                    Valued {
                        value: DEFAULT_WIKI_NAME.to_string(),
                        source: Source::Default,
                    }
                }
            },
            None => Valued {
                value: DEFAULT_WIKI_NAME.to_string(),
                source: Source::Default,
            },
        }
    };

    // Every name the registry needs an entry for: the five built-ins plus
    // whatever custom sections the file defines (a name appearing in both,
    // e.g. an explicit `[wiki.wiktionary]` override, resolves once, its
    // section applied on top of the built-in template).
    let mut names: std::collections::BTreeSet<String> = crate::sisters::all_known_projects()
        .into_iter()
        .map(|p| p.name.to_string())
        .collect();
    if let Some(sections) = wiki_sections {
        names.extend(sections.keys().cloned());
    }
    let mut entries: std::collections::BTreeMap<String, ResolvedWikiEntry> = names
        .into_iter()
        .map(|name| {
            let entry = resolve_wiki_entry(&name, wiki_sections, issues);
            (name, entry)
        })
        .collect();

    // PRD §6.2 rule 2: `WIKITUI_BASE_URL` always wins for whichever wiki
    // actually starts active — the supported mock/third-party override —
    // but it repoints only that one registry entry, not every possible
    // `:wiki` target (a later `:wiki wiktionary` still resolves to the real
    // `wiktionary.org`, not the mock, unless a `[wiki.wiktionary]` section
    // itself points there).
    if let Some(url) = &env.base_url
        && let Some(active_entry) = entries.get_mut(&active_name.value)
    {
        active_entry.base_url_template = Valued {
            value: url.clone(),
            source: Source::Env,
        };
    }

    // `active_name.value` is always one of the built-ins or an existing
    // section name (the `known_name` check above guarantees it), so this
    // entry always exists; the fallback is defensive, never reachable.
    let active_entry = entries
        .get(&active_name.value)
        .cloned()
        .unwrap_or_else(|| resolve_wiki_entry(DEFAULT_WIKI_NAME, wiki_sections, issues));

    let base_url_template = active_entry.base_url_template;
    let wiki_capabilities = active_entry.capabilities;

    (
        active_name,
        base_url_template,
        wiki_capabilities,
        ResolvedWikiRegistry { entries },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Writes `contents` to a fresh temp file and returns its path — tests
    /// must never touch `$XDG_CONFIG_HOME` or a real user config, so every
    /// test injects an explicit path exactly the way `--config` does.
    fn temp_config(contents: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "wikitui-config-test-{}-{n}.toml",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// A path to a config file that does *not* exist yet (for first-run and
    /// write tests) — unique per call, cleaned up by the test.
    fn absent_config_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-config-absent-{}-{n}/config.toml",
            std::process::id()
        ))
    }

    /// PRD FR-CS-8: the config file's absence is the first-run sentinel;
    /// its presence (or no config path at all) is not.
    #[test]
    fn first_run_is_true_only_when_the_config_file_is_absent() {
        let path = absent_config_path();
        assert!(first_run(Some(&path)), "absent file is a first run");
        assert!(!first_run(None), "no config path is not a first run");

        let existing = temp_config("");
        assert!(
            !first_run(Some(&existing)),
            "present file is not a first run"
        );
        cleanup(&existing);
    }

    /// PRD FR-CS-8: dismissing onboarding writes commented defaults, and that
    /// file must load cleanly (all comments -> empty table -> zero issues,
    /// every value at its built-in default).
    #[test]
    fn write_default_config_writes_a_clean_loadable_file_once() {
        let path = absent_config_path();
        assert!(first_run(Some(&path)));

        let wrote = write_default_config(Some(&path)).expect("write ok");
        assert!(wrote, "a fresh path gets written");
        assert!(path.exists(), "the file now exists");
        assert!(!first_run(Some(&path)), "no longer a first run");

        // A second call is a no-op — never clobber an existing config.
        assert!(!write_default_config(Some(&path)).expect("second call ok"));

        // The written defaults load with no issues and keep every default.
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(
            resolved.issues.is_empty(),
            "clean load: {:?}",
            resolved.issues
        );
        assert_eq!(resolved.theme.value, "terminal");
        assert_eq!(resolved.keymap_preset.value, "vim");
        assert_eq!(resolved.measure.value, 88);

        let _ = std::fs::remove_file(&path);
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }

    #[test]
    fn write_default_config_is_a_noop_with_no_config_path() {
        assert!(!write_default_config(None).expect("no path is a no-op, not an error"));
    }

    /// PRD FR-CS-3: the keymap preset selector, validated like every other
    /// named field.
    #[test]
    fn keymap_preset_resolves_and_validates() {
        let resolved = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(resolved.keymap_preset.value, "vim");
        assert_eq!(resolved.keymap_preset.source, Source::Default);

        let path = temp_config("keymap = \"emacs\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.keymap_preset.value, "emacs");
        assert_eq!(resolved.keymap_preset.source, Source::File);
        cleanup(&path);

        // An unknown preset warns and falls back to the default.
        let path = temp_config("keymap = \"kakoune\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.keymap_preset.value, "vim");
        assert!(resolved.issues.iter().any(|i| i.message.contains("keymap")));
        cleanup(&path);
    }

    #[test]
    fn defaults_apply_with_no_file_no_env_no_cli() {
        let resolved = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(resolved.lang.value, "en");
        assert_eq!(resolved.lang.source, Source::Default);
        assert_eq!(resolved.theme.value, "terminal");
        assert_eq!(resolved.theme.source, Source::Default);
        assert_eq!(resolved.measure.value, 88);
        assert_eq!(resolved.measure.source, Source::Default);
        assert!(!resolved.ambiguous_wide.value);
        assert_eq!(resolved.cite_style.value, "apa");
        assert_eq!(resolved.base_url_template.value, DEFAULT_BASE_URL_TEMPLATE);
        assert!(resolved.issues.is_empty());
    }

    #[test]
    fn file_value_beats_default_for_lang_theme_measure() {
        let path = temp_config("lang = \"de\"\ntheme = \"paper\"\nmeasure = 60\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            resolved.lang,
            Valued {
                value: "de".into(),
                source: Source::File
            }
        );
        assert_eq!(
            resolved.theme,
            Valued {
                value: "paper".into(),
                source: Source::File
            }
        );
        assert_eq!(
            resolved.measure,
            Valued {
                value: 60,
                source: Source::File
            }
        );
        cleanup(&path);
    }

    #[test]
    fn env_beats_file_for_lang_theme_measure() {
        let path = temp_config("lang = \"de\"\ntheme = \"paper\"\nmeasure = 60\n");
        let env = EnvOverrides {
            lang: Some("fr".into()),
            theme: Some("night".into()),
            measure: Some("70".into()),
            ..Default::default()
        };
        let resolved = resolve(&CliOverrides::default(), &env, Some(&path));
        assert_eq!(
            resolved.lang,
            Valued {
                value: "fr".into(),
                source: Source::Env
            }
        );
        assert_eq!(
            resolved.theme,
            Valued {
                value: "night".into(),
                source: Source::Env
            }
        );
        assert_eq!(
            resolved.measure,
            Valued {
                value: 70,
                source: Source::Env
            }
        );
        cleanup(&path);
    }

    #[test]
    fn cli_beats_everything_for_lang_theme_measure() {
        let path = temp_config("lang = \"de\"\ntheme = \"paper\"\nmeasure = 60\n");
        let env = EnvOverrides {
            lang: Some("fr".into()),
            theme: Some("night".into()),
            measure: Some("70".into()),
            ..Default::default()
        };
        let cli = CliOverrides {
            lang: Some("ja".into()),
            theme: Some("contrast".into()),
            measure: Some(50),
            ..Default::default()
        };
        let resolved = resolve(&cli, &env, Some(&path));
        assert_eq!(
            resolved.lang,
            Valued {
                value: "ja".into(),
                source: Source::Cli
            }
        );
        assert_eq!(
            resolved.theme,
            Valued {
                value: "contrast".into(),
                source: Source::Cli
            }
        );
        assert_eq!(
            resolved.measure,
            Valued {
                value: 50,
                source: Source::Cli
            }
        );
        cleanup(&path);
    }

    #[test]
    fn full_precedence_chain_default_file_env_cli() {
        // measure only: exercise all four rungs one at a time.
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(default.measure.source, Source::Default);

        let path = temp_config("measure = 100\n");
        let file_only = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            file_only.measure,
            Valued {
                value: 100,
                source: Source::File
            }
        );

        let env = EnvOverrides {
            measure: Some("120".into()),
            ..Default::default()
        };
        let file_and_env = resolve(&CliOverrides::default(), &env, Some(&path));
        assert_eq!(
            file_and_env.measure,
            Valued {
                value: 120,
                source: Source::Env
            }
        );

        let cli = CliOverrides {
            measure: Some(140),
            ..Default::default()
        };
        let all_four = resolve(&cli, &env, Some(&path));
        assert_eq!(
            all_four.measure,
            Valued {
                value: 140,
                source: Source::Cli
            }
        );
        cleanup(&path);
    }

    #[test]
    fn unknown_keys_are_collected_as_warnings_not_crashes() {
        let path =
            temp_config("lang = \"de\"\nfrobnicate = true\n[cache]\nmax_mb = 100\nbogus = 1\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(!resolved.has_errors());
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("frobnicate"))
        );
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("cache.bogus"))
        );
        assert_eq!(
            resolved.lang.value, "de",
            "a sibling unknown key must not break valid keys"
        );
        cleanup(&path);
    }

    #[test]
    fn ambiguous_width_two_maps_to_ambiguous_wide_true() {
        let path = temp_config("ambiguous_width = 2\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(resolved.ambiguous_wide.value);
        assert_eq!(resolved.ambiguous_wide.source, Source::File);

        let path_one = temp_config("ambiguous_width = 1\n");
        let resolved_one = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path_one),
        );
        assert!(!resolved_one.ambiguous_wide.value);
        cleanup(&path);
        cleanup(&path_one);
    }

    #[test]
    fn readlater_auto_dequeue_defaults_true_and_honors_file_and_env() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert!(default.readlater_auto_dequeue.value);
        assert_eq!(default.readlater_auto_dequeue.source, Source::Default);

        let path = temp_config("readlater_auto_dequeue = false\n");
        let from_file = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(!from_file.readlater_auto_dequeue.value);
        assert_eq!(from_file.readlater_auto_dequeue.source, Source::File);
        cleanup(&path);

        let env = EnvOverrides {
            readlater_auto_dequeue: Some("false".to_string()),
            ..Default::default()
        };
        let from_env = resolve(&CliOverrides::default(), &env, None);
        assert!(!from_env.readlater_auto_dequeue.value);
        assert_eq!(from_env.readlater_auto_dequeue.source, Source::Env);
    }

    #[test]
    fn startpage_defaults_to_feed_and_honors_the_file() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(default.startpage.value, "feed");
        assert_eq!(default.startpage.source, Source::Default);

        for value in ["feed", "blank", "resume"] {
            let path = temp_config(&format!("startpage = \"{value}\"\n"));
            let r = resolve(
                &CliOverrides::default(),
                &EnvOverrides::default(),
                Some(&path),
            );
            assert_eq!(r.startpage.value, value);
            assert_eq!(r.startpage.source, Source::File);
            cleanup(&path);
        }
    }

    #[test]
    fn auth_defaults_to_meta_wiki_endpoints_and_no_client_id() {
        let d = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(d.auth.client_id.value, "");
        assert_eq!(d.auth.client_id.source, Source::Default);
        assert_eq!(
            d.auth.authorize_url.value,
            crate::auth::DEFAULT_AUTHORIZE_URL
        );
        assert_eq!(d.auth.token_url.value, crate::auth::DEFAULT_TOKEN_URL);
    }

    #[test]
    fn auth_reads_client_id_and_endpoint_overrides_from_the_file() {
        let path = temp_config(concat!(
            "[auth]\n",
            "client_id = \"my-consumer\"\n",
            "authorize_url = \"http://127.0.0.1:8943/w/rest.php/oauth2/authorize\"\n",
            "token_url = \"http://127.0.0.1:8943/w/rest.php/oauth2/access_token\"\n",
        ));
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.auth.client_id.value, "my-consumer");
        assert_eq!(r.auth.client_id.source, Source::File);
        assert_eq!(
            r.auth.authorize_url.value,
            "http://127.0.0.1:8943/w/rest.php/oauth2/authorize"
        );
        assert_eq!(
            r.auth.token_url.value,
            "http://127.0.0.1:8943/w/rest.php/oauth2/access_token"
        );
        cleanup(&path);
    }

    #[test]
    fn auth_unknown_key_warns_but_keeps_the_known_ones() {
        let path = temp_config("[auth]\nclient_id = \"x\"\nbogus = 1\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.auth.client_id.value, "x");
        assert!(
            r.issues.iter().any(|i| i.message.contains("auth.bogus")),
            "unknown auth key should warn: {:?}",
            r.issues
        );
        cleanup(&path);
    }

    #[test]
    fn watchlist_mirror_tag_defaults_to_watched_and_honors_the_file() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(default.watchlist_mirror_tag.value, "watched");
        assert_eq!(default.watchlist_mirror_tag.source, Source::Default);

        let path = temp_config("watchlist_mirror_tag = \"to-watch\"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.watchlist_mirror_tag.value, "to-watch");
        assert_eq!(r.watchlist_mirror_tag.source, Source::File);
        cleanup(&path);
    }

    #[test]
    fn watchlist_mirror_tag_strips_a_leading_hash_a_user_typed_out_of_habit() {
        let path = temp_config("watchlist_mirror_tag = \"#watched\"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.watchlist_mirror_tag.value, "watched");
        cleanup(&path);
    }

    #[test]
    fn watchlist_mirror_tag_rejects_an_empty_value_with_a_warning() {
        let path = temp_config("watchlist_mirror_tag = \"   \"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.watchlist_mirror_tag.value, "watched");
        assert_eq!(r.watchlist_mirror_tag.source, Source::Default);
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("watchlist_mirror_tag")),
        );
        cleanup(&path);
    }

    // ---- tts_command (PRD FR-PC-2 / SEC-5) -------------------------------

    #[test]
    fn tts_command_is_unset_by_default() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(default.tts_command.value, None);
        assert_eq!(default.tts_command.source, Source::Default);
    }

    #[test]
    fn tts_command_honors_the_file_value() {
        let path = temp_config("tts_command = \"espeak-ng -s 160\"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.tts_command.value, Some("espeak-ng -s 160".to_string()));
        assert_eq!(r.tts_command.source, Source::File);
        cleanup(&path);
    }

    #[test]
    fn tts_command_rejects_a_blank_value_and_stays_disabled() {
        let path = temp_config("tts_command = \"   \"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.tts_command.value, None);
        assert!(r.issues.iter().any(|i| i.message.contains("tts_command")));
        cleanup(&path);
    }

    #[test]
    fn tts_command_rejects_a_non_string_value() {
        let path = temp_config("tts_command = 160\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.tts_command.value, None);
        assert!(r.issues.iter().any(|i| i.message.contains("tts_command")));
        cleanup(&path);
    }

    // ---- [command.<name>] macros (PRD FR-CS-5) ---------------------------

    #[test]
    fn no_command_sections_resolves_to_an_empty_macro_table() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert!(default.macros.is_empty());
    }

    /// PRD FR-CS-5's own example: `[command.morning] run = ["open-feed",
    /// "tab-open-random-good"]`.
    #[test]
    fn a_valid_command_section_parses_its_run_steps_in_order() {
        let path =
            temp_config("[command.morning]\nrun = [\"open-feed\", \"tab-open-random-good\"]\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            r.macros.get("morning"),
            Some(&vec![
                "open-feed".to_string(),
                "tab-open-random-good".to_string()
            ])
        );
        assert!(
            r.issues.is_empty(),
            "a valid, fully-known macro warns nothing: {:?}",
            r.issues
        );
        cleanup(&path);
    }

    #[test]
    fn multiple_command_sections_each_resolve_independently() {
        let path = temp_config(
            "[command.demo]\nrun = [\":open Alan Turing\", \":toc\"]\n\n\
             [command.morning]\nrun = [\"open-feed\"]\n",
        );
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            r.macros.get("demo"),
            Some(&vec![":open Alan Turing".to_string(), ":toc".to_string()])
        );
        assert_eq!(
            r.macros.get("morning"),
            Some(&vec!["open-feed".to_string()])
        );
        cleanup(&path);
    }

    /// PRD FR-CS-5: "validate at load, warn on unknown" — an unrecognized
    /// step still keeps the macro registered (so its other, valid steps
    /// still run; see `resolve_macros`'s doc comment) but surfaces a warning
    /// naming the exact bad step.
    #[test]
    fn an_unknown_step_warns_but_the_macro_still_registers() {
        let path =
            temp_config("[command.demo]\nrun = [\"open-feed\", \"this-is-not-a-real-command\"]\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            r.macros.get("demo"),
            Some(&vec![
                "open-feed".to_string(),
                "this-is-not-a-real-command".to_string()
            ])
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("this-is-not-a-real-command")),
            "{:?}",
            r.issues
        );
        cleanup(&path);
    }

    #[test]
    fn a_self_referential_macro_step_is_accepted_at_load_time() {
        // A macro naming itself is only meaningfully caught at *run* time
        // (main::run_macro's seen-set) — at load time, a macro name is just
        // another string this loader has no registry of yet (macro names
        // aren't `registry::Action`s or `:` commands), so it is silently
        // accepted here, same as any other unrecognized bare word would be
        // if it happened to match a sibling macro's name. Documented so a
        // future reader doesn't mistake this for a missed validation.
        let path = temp_config("[command.a]\nrun = [\"a\"]\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.macros.get("a"), Some(&vec!["a".to_string()]));
        cleanup(&path);
    }

    #[test]
    fn a_run_key_that_is_not_an_array_of_strings_is_rejected_with_a_warning() {
        let path = temp_config("[command.demo]\nrun = \"open-feed\"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(!r.macros.contains_key("demo"));
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("command.demo.run"))
        );
        cleanup(&path);
    }

    #[test]
    fn a_command_section_with_no_run_key_is_rejected_with_a_warning() {
        let path = temp_config("[command.demo]\nnotrun = [\"open-feed\"]\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(!r.macros.contains_key("demo"));
        assert!(r.issues.iter().any(|i| i.message.contains("command.demo")));
        cleanup(&path);
    }

    #[test]
    fn command_as_a_non_table_is_rejected_with_a_warning() {
        let path = temp_config("command = \"oops\"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(r.macros.is_empty());
        assert!(r.issues.iter().any(|i| i.message.contains("command")));
        cleanup(&path);
    }

    #[test]
    fn startpage_rejects_an_unknown_value_and_falls_back_with_a_warning() {
        let path = temp_config("startpage = \"resurrect\"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            r.startpage.value, "feed",
            "invalid value falls back to the default"
        );
        assert_eq!(r.startpage.source, Source::Default);
        assert!(r.issues.iter().any(|i| i.message.contains("startpage")));
        cleanup(&path);
    }

    #[test]
    fn reading_wpm_defaults_to_230_and_honors_the_file() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(default.reading_wpm.value, 230);
        assert_eq!(default.reading_wpm.source, Source::Default);

        let path = temp_config("reading_wpm = 300\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.reading_wpm.value, 300);
        assert_eq!(r.reading_wpm.source, Source::File);
        cleanup(&path);
    }

    #[test]
    fn reading_wpm_out_of_range_is_clamped_not_rejected() {
        let low = temp_config("reading_wpm = 1\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&low),
        );
        assert_eq!(r.reading_wpm.value, READING_WPM_MIN);
        assert_eq!(r.reading_wpm.source, Source::File);
        assert!(r.issues.iter().any(|i| i.message.contains("reading_wpm")));
        cleanup(&low);

        let high = temp_config("reading_wpm = 999999\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&high),
        );
        assert_eq!(r.reading_wpm.value, READING_WPM_MAX);
        cleanup(&high);
    }

    #[test]
    fn reading_wpm_non_integer_falls_back_to_the_default_with_a_warning() {
        let path = temp_config("reading_wpm = \"fast\"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.reading_wpm.value, 230);
        assert_eq!(r.reading_wpm.source, Source::Default);
        assert!(r.issues.iter().any(|i| i.message.contains("reading_wpm")));
        cleanup(&path);
    }

    #[test]
    fn prefetch_defaults_match_the_prd() {
        let d = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert!(d.prefetch.enabled.value, "FR-PF-6: default on");
        assert_eq!(d.prefetch.daily_mb.value, 20, "FR-PF-5 default 20 MB");
        assert_eq!(d.prefetch.hourly_requests.value, 100, "FR-PF-5 default 100");
        assert_eq!(d.prefetch.metered.value, "reduced");
        assert_eq!(d.prefetch.top_n.value, 5, "FR-PF-1 default top-5");
        assert_eq!(d.prefetch.weight_lead.value, 1.0);
        assert_eq!(d.prefetch.weight_pageviews.value, 1.0);
        assert_eq!(
            d.prefetch.weight_affinity.value, 1.0,
            "FR-PF-3 wired the w3 affinity term: default weight is now live"
        );
        assert_eq!(d.network_contact.value, crate::api::DEFAULT_CONTACT);
    }

    #[test]
    fn interest_learning_defaults_on_and_half_life_defaults_to_thirty_days() {
        let d = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert!(
            d.interest_learning.value,
            "FR-PF-3: default on (differentiator)"
        );
        assert_eq!(d.interest_learning.source, Source::Default);
        assert_eq!(
            d.interest_half_life_days.value, 30.0,
            "FR-PF-3 default half-life"
        );
        assert_eq!(d.interest_half_life_days.source, Source::Default);
    }

    #[test]
    fn interest_options_honor_the_file_and_reject_bad_values() {
        let path = temp_config("interest_learning = false\ninterest_half_life_days = 14\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(!r.interest_learning.value);
        assert_eq!(r.interest_learning.source, Source::File);
        assert_eq!(
            r.interest_half_life_days.value, 14.0,
            "integer spelling accepted"
        );
        assert_eq!(r.interest_half_life_days.source, Source::File);
        cleanup(&path);

        let bad = temp_config("interest_half_life_days = 0\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&bad),
        );
        assert_eq!(
            r.interest_half_life_days.value, 30.0,
            "non-positive falls back"
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("interest_half_life_days"))
        );
        cleanup(&bad);
    }

    #[test]
    fn prefetch_table_is_honored_and_validated() {
        let path = temp_config(
            "[prefetch]\nenabled = false\ndaily_mb = 5\nhourly_requests = 40\nmetered = \"always\"\ntop_n = 8\nweight_lead = 2.0\nweight_pageviews = 0.5\nweight_affinity = 0.25\n",
        );
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(!r.prefetch.enabled.value);
        assert_eq!(r.prefetch.enabled.source, Source::File);
        assert_eq!(r.prefetch.daily_mb.value, 5);
        assert_eq!(r.prefetch.hourly_requests.value, 40);
        assert_eq!(r.prefetch.metered.value, "always");
        assert_eq!(r.prefetch.top_n.value, 8);
        assert_eq!(r.prefetch.weight_lead.value, 2.0);
        assert_eq!(r.prefetch.weight_pageviews.value, 0.5);
        assert_eq!(r.prefetch.weight_affinity.value, 0.25);
        cleanup(&path);
    }

    #[test]
    fn prefetch_rejects_bad_values_with_warnings_and_falls_back() {
        let path =
            temp_config("[prefetch]\nmetered = \"sometimes\"\ntop_n = 0\nweight_lead = -1.0\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            r.prefetch.metered.value, "reduced",
            "bad metered falls back"
        );
        assert_eq!(r.prefetch.top_n.value, 5, "0 top_n falls back");
        assert_eq!(
            r.prefetch.weight_lead.value, 1.0,
            "negative weight falls back"
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("prefetch.metered"))
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("prefetch.weight_lead"))
        );
        cleanup(&path);
    }

    /// PRD FR-PC-1: the `[reading]` table defaults, honoring the file, and
    /// clamping out-of-range values with a warning — same three-way check
    /// `prefetch_table_is_honored_and_validated`/`prefetch_rejects_bad_
    /// values_with_warnings_and_falls_back` run for `[prefetch]`.
    #[test]
    fn reading_table_defaults_are_the_pre_fr_pc_1_behavior() {
        let r = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(r.reading.margin.value, 0);
        assert_eq!(r.reading.text_align.value, "center");
        assert_eq!(r.reading.paragraph_spacing.value, 1);
        assert_eq!(r.reading.line_spacing.value, 0);
        assert_eq!(r.reading.word_spacing.value, 0);
        for v in [
            &r.reading.margin.source,
            &r.reading.paragraph_spacing.source,
            &r.reading.line_spacing.source,
            &r.reading.word_spacing.source,
        ] {
            assert_eq!(*v, Source::Default);
        }
    }

    #[test]
    fn reading_table_is_honored_from_the_file() {
        let path = temp_config(
            "[reading]\nmargin = 4\ntext_align = \"left\"\nparagraph_spacing = 2\nline_spacing = 1\nword_spacing = 1\n",
        );
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.reading.margin.value, 4);
        assert_eq!(r.reading.margin.source, Source::File);
        assert_eq!(r.reading.text_align.value, "left");
        assert_eq!(r.reading.paragraph_spacing.value, 2);
        assert_eq!(r.reading.line_spacing.value, 1);
        assert_eq!(r.reading.word_spacing.value, 1);
        cleanup(&path);
    }

    #[test]
    fn reading_table_clamps_out_of_range_values_with_a_warning() {
        let path = temp_config(
            "[reading]\nmargin = 9999\nparagraph_spacing = 9\nline_spacing = 9\nword_spacing = 9\ntext_align = \"justify\"\n",
        );
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.reading.margin.value, MARGIN_MAX);
        assert_eq!(r.reading.paragraph_spacing.value, PARAGRAPH_SPACING_MAX);
        assert_eq!(r.reading.line_spacing.value, LINE_SPACING_MAX);
        assert_eq!(r.reading.word_spacing.value, WORD_SPACING_MAX);
        assert_eq!(
            r.reading.text_align.value, "center",
            "an unknown text_align falls back to the default rather than clamping"
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("reading.margin"))
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("reading.paragraph_spacing"))
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("reading.line_spacing"))
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("reading.word_spacing"))
        );
        cleanup(&path);
    }

    #[test]
    fn reading_table_rejects_a_non_table_value_and_unknown_subkeys() {
        let path = temp_config("reading = \"nope\"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.reading.margin.value, 0, "falls back to defaults");
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("reading must be a table"))
        );
        cleanup(&path);

        let path2 = temp_config("[reading]\nbogus_key = 1\n");
        let r2 = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path2),
        );
        assert!(
            r2.issues
                .iter()
                .any(|i| i.message.contains("reading.bogus_key"))
        );
        cleanup(&path2);
    }

    #[test]
    fn prefetch_env_kill_switch_and_contact_are_honored() {
        let env = EnvOverrides {
            prefetch: Some("off".to_string()),
            contact: Some("mailto:ops@example.org".to_string()),
            ..Default::default()
        };
        let r = resolve(&CliOverrides::default(), &env, None);
        assert!(!r.prefetch.enabled.value, "WIKITUI_PREFETCH=off disables");
        assert_eq!(r.prefetch.enabled.source, Source::Env);
        assert_eq!(r.network_contact.value, "mailto:ops@example.org");
        assert_eq!(r.network_contact.source, Source::Env);
    }

    #[test]
    fn network_contact_from_file_is_honored() {
        let path = temp_config("[network]\ncontact = \"mailto:me@example.com\"\n");
        let r = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(r.network_contact.value, "mailto:me@example.com");
        assert_eq!(r.network_contact.source, Source::File);
        cleanup(&path);
    }

    #[test]
    fn images_defaults_to_follow_theme_and_honors_file_and_env() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(default.images.value, None, "default follows the theme");
        assert_eq!(default.images.source, Source::Default);

        let path = temp_config("images = false\n");
        let from_file = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(from_file.images.value, Some(false));
        assert_eq!(from_file.images.source, Source::File);
        cleanup(&path);

        let env = EnvOverrides {
            images: Some("on".to_string()),
            ..Default::default()
        };
        let from_env = resolve(&CliOverrides::default(), &env, None);
        assert_eq!(from_env.images.value, Some(true));
        assert_eq!(from_env.images.source, Source::Env);
    }

    #[test]
    fn include_nonfree_defaults_false_and_honors_file() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert!(!default.include_nonfree.value);
        assert_eq!(default.include_nonfree.source, Source::Default);

        let path = temp_config("include_nonfree = true\n");
        let from_file = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(from_file.include_nonfree.value);
        assert_eq!(from_file.include_nonfree.source, Source::File);
        cleanup(&path);
    }

    /// PRD FR-DL-8: `pro = true` (file-only, default false) disables every
    /// easter egg and achievement toast — same shape as `include_nonfree`.
    #[test]
    fn pro_defaults_false_and_honors_file() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert!(!default.pro.value);
        assert_eq!(default.pro.source, Source::Default);

        let path = temp_config("pro = true\n");
        let from_file = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(from_file.pro.value);
        assert_eq!(from_file.pro.source, Source::File);
        cleanup(&path);
    }

    #[test]
    fn pro_rejects_a_non_boolean_with_a_warning() {
        let path = temp_config("pro = \"sometimes\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(!resolved.pro.value, "a bad value falls back to the default");
        assert_eq!(resolved.pro.source, Source::Default);
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("pro must be a boolean")),
            "{:?}",
            resolved.issues
        );
        cleanup(&path);
    }

    /// PRD FR-TB-5: `restore_session` is an independent trigger alongside
    /// `startpage = resume` — file-only, default false, same shape as
    /// `include_nonfree`.
    #[test]
    fn restore_session_defaults_false_and_honors_file() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert!(!default.restore_session.value);
        assert_eq!(default.restore_session.source, Source::Default);

        let path = temp_config("restore_session = true\n");
        let from_file = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(from_file.restore_session.value);
        assert_eq!(from_file.restore_session.source, Source::File);
        cleanup(&path);
    }

    #[test]
    fn restore_session_rejects_a_non_boolean_with_a_warning() {
        let path = temp_config("restore_session = \"sometimes\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(
            !resolved.restore_session.value,
            "falls back to default false"
        );
        assert_eq!(resolved.restore_session.source, Source::Default);
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("restore_session"))
        );
        cleanup(&path);
    }

    #[test]
    fn readlater_auto_dequeue_rejects_a_non_boolean_with_a_warning() {
        let path = temp_config("readlater_auto_dequeue = \"sometimes\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(
            resolved.readlater_auto_dequeue.value,
            "falls back to the default"
        );
        assert_eq!(resolved.readlater_auto_dequeue.source, Source::Default);
        assert!(resolved.issues.iter().any(|i| {
            i.message
                .contains("readlater_auto_dequeue must be a boolean")
        }));
        cleanup(&path);
    }

    #[test]
    fn history_retention_days_defaults_to_zero_keep_forever() {
        let default = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        assert_eq!(default.history_retention_days.value, 0);
        assert_eq!(default.history_retention_days.source, Source::Default);
    }

    #[test]
    fn history_retention_days_honors_the_file() {
        let path = temp_config("[history]\nretention_days = 90\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.history_retention_days.value, 90);
        assert_eq!(resolved.history_retention_days.source, Source::File);
        cleanup(&path);
    }

    #[test]
    fn history_retention_days_accepts_zero_as_a_real_file_value() {
        // 0 is the meaningful "keep forever" setting, not an error — unlike
        // `resolve_positive_int`'s fields, it must round-trip from the file.
        let path = temp_config("[history]\nretention_days = 0\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.history_retention_days.value, 0);
        assert_eq!(resolved.history_retention_days.source, Source::File);
        cleanup(&path);
    }

    #[test]
    fn history_retention_days_rejects_negative_and_non_integer_values() {
        let path = temp_config("[history]\nretention_days = -5\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.history_retention_days.value, 0);
        assert_eq!(resolved.history_retention_days.source, Source::Default);
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("history.retention_days"))
        );
        cleanup(&path);
    }

    #[test]
    fn history_unknown_keys_and_non_table_shape_warn() {
        let path = temp_config("[history]\nretention_days = 30\nbogus = 1\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("history.bogus"))
        );
        cleanup(&path);

        let path = temp_config("history = 5\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.history_retention_days.value, 0);
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("history must be a table"))
        );
        cleanup(&path);
    }

    #[test]
    fn invalid_values_are_rejected_default_kept_with_warning() {
        let path = temp_config("theme = \"sepia\"\nambiguous_width = 3\nmeasure = \"wide\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            resolved.theme,
            Valued {
                value: "terminal".into(),
                source: Source::Default
            }
        );
        assert!(!resolved.ambiguous_wide.value);
        assert_eq!(resolved.ambiguous_wide.source, Source::Default);
        assert_eq!(
            resolved.measure,
            Valued {
                value: 88,
                source: Source::Default
            }
        );
        assert!(
            !resolved.has_errors(),
            "invalid values warn, they don't error"
        );
        assert!(resolved.issues.iter().any(|i| i.message.contains("sepia")));
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("ambiguous_width"))
        );
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("measure"))
        );
        cleanup(&path);
    }

    #[test]
    fn measure_out_of_range_is_clamped_not_rejected() {
        let path = temp_config("measure = 9999\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.measure.value, 200);
        assert_eq!(resolved.measure.source, Source::File);
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("clamped"))
        );

        let path_low = temp_config("measure = 3\n");
        let low = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path_low),
        );
        assert_eq!(low.measure.value, 40);
        cleanup(&path);
        cleanup(&path_low);
    }

    #[test]
    fn base_url_template_substitution_and_verbatim() {
        // {lang} present in a [wiki.*] section's base_url.
        let path = temp_config(
            "active_wiki = \"archwiki\"\n[wiki.archwiki]\nbase_url = \"https://wiki.archlinux.org\"\n",
        );
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.active_wiki.value, "archwiki");
        assert_eq!(
            resolved.base_url_template.value,
            "https://wiki.archlinux.org"
        );
        assert_eq!(resolved.base_url_template.source, Source::File);
        cleanup(&path);

        // env WIKITUI_BASE_URL wins over the file, verbatim (mock server).
        let env = EnvOverrides {
            base_url: Some("http://127.0.0.1:8943".into()),
            ..Default::default()
        };
        let resolved_env = resolve(&CliOverrides::default(), &env, None);
        assert_eq!(
            resolved_env.base_url_template.value,
            "http://127.0.0.1:8943"
        );
        assert_eq!(resolved_env.base_url_template.source, Source::Env);

        // A template containing {lang} is preserved for api.rs to expand.
        let env_lang = EnvOverrides {
            base_url: Some("https://{lang}.example.org".into()),
            ..Default::default()
        };
        let resolved_lang = resolve(&CliOverrides::default(), &env_lang, None);
        assert_eq!(
            resolved_lang.base_url_template.value,
            "https://{lang}.example.org"
        );
    }

    #[test]
    fn active_wiki_without_a_matching_section_falls_back_with_warning() {
        let path = temp_config("active_wiki = \"nonexistent\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.active_wiki.value, "wikipedia");
        assert_eq!(resolved.base_url_template.value, DEFAULT_BASE_URL_TEMPLATE);
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("nonexistent"))
        );
        cleanup(&path);
    }

    /// PRD FR-ML-4: the built-in `wikipedia` default gets every feature —
    /// today's unconditional pre-FR-ML-5 behavior, unchanged.
    #[test]
    fn wikipedia_default_capabilities_are_full() {
        let resolved = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        let caps = &resolved.wiki_capabilities;
        assert_eq!(caps.parser.value, "auto");
        assert!(caps.wikifeeds.value);
        assert!(caps.pageviews.value);
        assert!(caps.pageassessments.value);
        assert_eq!(caps.wikifeeds.source, Source::Default);
    }

    /// PRD FR-ML-4: a sister project resolves its own `{lang}.<project>.org`
    /// template with no `[wiki.<name>]` section at all, and defaults its
    /// three optional endpoints off (FR-ML-5's degradation matrix) — only
    /// `wikipedia` gets them for free.
    #[test]
    fn sister_project_resolves_its_domain_with_degraded_defaults() {
        let path = temp_config("active_wiki = \"wiktionary\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.active_wiki.value, "wiktionary");
        assert_eq!(
            resolved.base_url_template.value,
            "https://{lang}.wiktionary.org"
        );
        assert!(resolved.issues.is_empty(), "{:?}", resolved.issues);
        let caps = &resolved.wiki_capabilities;
        assert_eq!(caps.parser.value, "auto");
        assert!(!caps.wikifeeds.value);
        assert!(!caps.pageviews.value);
        assert!(!caps.pageassessments.value);
        cleanup(&path);
    }

    /// PRD FR-ML-5: a `[wiki.<name>]` section can override any one of the
    /// four capability keys independently, each landing at `Source::File`.
    #[test]
    fn wiki_section_overrides_individual_capabilities() {
        let path = temp_config(
            "active_wiki = \"archwiki\"\n\
             [wiki.archwiki]\n\
             base_url = \"https://wiki.archlinux.org\"\n\
             parser = \"legacy\"\n\
             pageviews = true\n",
        );
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        let caps = &resolved.wiki_capabilities;
        assert_eq!(caps.parser.value, "legacy");
        assert_eq!(caps.parser.source, Source::File);
        assert!(caps.pageviews.value);
        assert_eq!(caps.pageviews.source, Source::File);
        // Untouched keys keep the non-wikipedia default.
        assert!(!caps.wikifeeds.value);
        assert!(!caps.pageassessments.value);
        cleanup(&path);
    }

    /// An unrecognized `parser` value warns and falls back to `"auto"`
    /// rather than being silently ignored or crashing.
    #[test]
    fn wiki_section_rejects_an_unknown_parser_value() {
        let path = temp_config("active_wiki = \"archwiki\"\n[wiki.archwiki]\nparser = \"bogus\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.wiki_capabilities.parser.value, "auto");
        assert!(resolved.issues.iter().any(|i| i.message.contains("parser")));
        cleanup(&path);
    }

    /// A non-boolean `wikifeeds`/`pageviews`/`pageassessments` value warns
    /// and falls back to the name-based default instead of crashing.
    #[test]
    fn wiki_section_rejects_a_non_boolean_capability_value() {
        let path =
            temp_config("active_wiki = \"archwiki\"\n[wiki.archwiki]\nwikifeeds = \"yes\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(!resolved.wiki_capabilities.wikifeeds.value);
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("wikifeeds"))
        );
        cleanup(&path);
    }

    /// PRD FR-ML-4: `:wiki <name>`'s switch-target registry carries every
    /// built-in project plus every configured `[wiki.<name>]` section, even
    /// when it isn't the currently active wiki — a runtime switch never
    /// re-reads the config file.
    #[test]
    fn wiki_registry_carries_every_built_in_and_custom_wiki() {
        let path = temp_config(
            "active_wiki = \"wikipedia\"\n[wiki.archwiki]\nbase_url = \"https://wiki.archlinux.org\"\n",
        );
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        let names: Vec<&str> = resolved
            .wiki_registry
            .entries
            .keys()
            .map(String::as_str)
            .collect();
        for expected in [
            "wikipedia",
            "wiktionary",
            "wikivoyage",
            "wikiquote",
            "wikinews",
            "archwiki",
        ] {
            assert!(
                names.contains(&expected),
                "missing {expected:?} in {names:?}"
            );
        }
        let archwiki = &resolved.wiki_registry.entries["archwiki"];
        assert_eq!(
            archwiki.base_url_template.value,
            "https://wiki.archlinux.org"
        );
        let wikt = &resolved.wiki_registry.entries["wiktionary"];
        assert_eq!(
            wikt.base_url_template.value,
            "https://{lang}.wiktionary.org"
        );
        assert!(!wikt.capabilities.wikifeeds.value);
        cleanup(&path);
    }

    /// PRD FR-ML-4: `WIKITUI_BASE_URL` overrides only the *active* wiki's
    /// registry entry — a later `:wiki wiktionary` still resolves to the
    /// real domain, not the mock, unless a `[wiki.wiktionary]` section
    /// itself points there.
    #[test]
    fn env_base_url_only_overrides_the_active_registry_entry() {
        let env = EnvOverrides {
            base_url: Some("http://127.0.0.1:8943".into()),
            ..Default::default()
        };
        let resolved = resolve(&CliOverrides::default(), &env, None);
        assert_eq!(resolved.active_wiki.value, "wikipedia");
        assert_eq!(
            resolved.wiki_registry.entries["wikipedia"]
                .base_url_template
                .value,
            "http://127.0.0.1:8943"
        );
        assert_eq!(
            resolved.wiki_registry.entries["wiktionary"]
                .base_url_template
                .value,
            "https://{lang}.wiktionary.org"
        );
    }

    /// PRD FR-ML-4: `CliOverrides::active_wiki` (the TITLE argument's own
    /// sister-project URL/prefix) outranks the config file, at `Source::Cli`
    /// — the same precedence `lang`'s own CLI override already has.
    #[test]
    fn cli_active_wiki_override_wins_over_the_file() {
        let path = temp_config("active_wiki = \"wikiquote\"\n");
        let cli = CliOverrides {
            active_wiki: Some("wiktionary".to_string()),
            ..Default::default()
        };
        let resolved = resolve(&cli, &EnvOverrides::default(), Some(&path));
        assert_eq!(resolved.active_wiki.value, "wiktionary");
        assert_eq!(resolved.active_wiki.source, Source::Cli);
        assert_eq!(
            resolved.base_url_template.value,
            "https://{lang}.wiktionary.org"
        );
        cleanup(&path);
    }

    #[test]
    fn config_version_older_migrates_with_summary() {
        let path = temp_config("config_version = 0\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.config_version.value, CONFIG_VERSION);
        assert!(resolved.migration_summary.is_some());
        assert!(
            resolved
                .migration_summary
                .as_ref()
                .unwrap()
                .contains("migrated")
        );
        cleanup(&path);
    }

    #[test]
    fn config_version_newer_warns() {
        let path = temp_config("config_version = 99\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.config_version.value, 99);
        assert!(resolved.migration_summary.is_none());
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("newer wikitui"))
        );
        cleanup(&path);
    }

    #[test]
    fn config_version_absent_is_current_with_no_migration_noise() {
        let path = temp_config("lang = \"de\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.config_version.value, CONFIG_VERSION);
        assert_eq!(resolved.config_version.source, Source::Default);
        assert!(resolved.migration_summary.is_none());
        cleanup(&path);
    }

    #[test]
    fn syntax_error_is_reported_and_defaults_used_throughout() {
        let path = temp_config("this is not valid = = toml [[[\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(resolved.has_errors());
        assert!(resolved.issues.iter().any(|i| i.level == IssueLevel::Error));
        // Everything still resolves to a sane default; nothing panics.
        assert_eq!(resolved.lang.value, "en");
        assert_eq!(resolved.theme.value, "terminal");
        cleanup(&path);
    }

    #[test]
    fn missing_file_is_not_an_issue() {
        let path = std::env::temp_dir().join("wikitui-config-test-definitely-missing.toml");
        let _ = std::fs::remove_file(&path);
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(resolved.issues.is_empty());
        assert_eq!(resolved.lang.value, "en");
    }

    #[test]
    fn languages_first_entry_is_the_default_lang_groundwork() {
        let path = temp_config("languages = [\"de\", \"en\"]\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.lang.value, "de");
        assert_eq!(
            resolved.languages.value,
            vec!["de".to_string(), "en".to_string()]
        );
        cleanup(&path);
    }

    #[test]
    fn explicit_lang_key_beats_languages_first_entry() {
        let path = temp_config("lang = \"ja\"\nlanguages = [\"de\", \"en\"]\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.lang.value, "ja");
        cleanup(&path);
    }

    #[test]
    fn cache_settings_are_wired_from_the_file() {
        let path = temp_config("[cache]\nmax_mb = 250\nfresh_ttl_hours = 6\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            resolved.cache_max_mb,
            Valued {
                value: 250,
                source: Source::File
            }
        );
        assert_eq!(
            resolved.cache_fresh_ttl_hours,
            Valued {
                value: 6,
                source: Source::File
            }
        );
        assert_eq!(
            resolved.cache_force_refetch_days,
            Valued {
                value: 30,
                source: Source::Default
            },
            "unset in the file: falls back to the 30-day default"
        );
        cleanup(&path);
    }

    #[test]
    fn cache_force_refetch_days_is_wired_from_the_file() {
        let path = temp_config("[cache]\nforce_refetch_days = 7\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            resolved.cache_force_refetch_days,
            Valued {
                value: 7,
                source: Source::File
            }
        );
        cleanup(&path);
    }

    #[test]
    fn cache_dir_override_is_wired_from_the_file() {
        let path = temp_config("[cache]\ndir = \"/tmp/wikitui-custom-cache\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            resolved.cache_dir,
            Valued {
                value: Some(PathBuf::from("/tmp/wikitui-custom-cache")),
                source: Source::File,
            }
        );
        cleanup(&path);
    }

    #[test]
    fn cache_dir_defaults_to_none_when_unset() {
        let path = temp_config("[cache]\nmax_mb = 100\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            resolved.cache_dir,
            Valued {
                value: None,
                source: Source::Default,
            }
        );
        cleanup(&path);
    }

    #[test]
    fn cache_dir_of_the_wrong_type_warns_and_falls_back_to_none() {
        let path = temp_config("[cache]\ndir = 42\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.cache_dir.value, None);
        assert!(resolved.issues.iter().any(|i| {
            i.message
                .contains("cache.dir must be a non-empty string path")
        }));
        cleanup(&path);
    }

    #[test]
    fn resolve_config_path_prefers_cli_then_env_then_platform_default() {
        let cli_path = PathBuf::from("/tmp/from-cli.toml");
        assert_eq!(
            resolve_config_path(Some(cli_path.clone()), Some("/tmp/from-env.toml".into())),
            Some(cli_path)
        );
        assert_eq!(
            resolve_config_path(None, Some("/tmp/from-env.toml".into())),
            Some(PathBuf::from("/tmp/from-env.toml"))
        );
        // Platform-default branch just needs to not panic; its value
        // depends on the real environment, which tests must not assert on.
        let _ = resolve_config_path(None, None);
    }

    // -- Terminal integration (PRD FR-NV-9, FR-ACS-4/6, FR-TH-4, FR-RD-2) ----

    #[test]
    fn terminal_settings_default_when_unset() {
        let resolved = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        let t = resolved.terminal;
        assert_eq!(
            t.mouse,
            Valued {
                value: false,
                source: Source::Default
            },
            "PRD FR-NV-9: mouse defaults off so native terminal selection/copy stays untouched"
        );
        assert_eq!(t.animations.value, "full");
        assert_eq!(t.animations.source, Source::Default);
        assert!(!t.auto_theme.value);
        assert_eq!(t.theme_light.value, "paper");
        assert_eq!(t.theme_dark.value, "terminal");
        assert_eq!(t.hyperlinks.value, "auto");
    }

    #[test]
    fn mouse_and_auto_theme_read_from_the_config_file() {
        let path = temp_config("mouse = true\nauto_theme = true\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            resolved.terminal.mouse,
            Valued {
                value: true,
                source: Source::File
            }
        );
        assert_eq!(
            resolved.terminal.auto_theme,
            Valued {
                value: true,
                source: Source::File
            }
        );
        assert!(resolved.issues.is_empty(), "{:?}", resolved.issues);
        cleanup(&path);
    }

    #[test]
    fn mouse_rejects_a_non_boolean_and_falls_back() {
        let path = temp_config("mouse = \"yes\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(!resolved.terminal.mouse.value);
        assert_eq!(resolved.terminal.mouse.source, Source::Default);
        assert!(resolved.issues.iter().any(|i| i.message.contains("mouse")));
        cleanup(&path);
    }

    /// PRD FR-ACS-4: `WIKITUI_ANIMATIONS=none` beats a config file value —
    /// same env-over-file precedence every other overridable key follows.
    #[test]
    fn animations_env_beats_file() {
        let path = temp_config("animations = \"full\"\n");
        let env = EnvOverrides {
            animations: Some("none".to_string()),
            ..Default::default()
        };
        let resolved = resolve(&CliOverrides::default(), &env, Some(&path));
        assert_eq!(
            resolved.terminal.animations,
            Valued {
                value: "none".to_string(),
                source: Source::Env
            }
        );
        cleanup(&path);
    }

    #[test]
    fn animations_rejects_unknown_values() {
        let path = temp_config("animations = \"smooth\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.terminal.animations.value, "full");
        assert_eq!(resolved.terminal.animations.source, Source::Default);
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("animations"))
        );
        cleanup(&path);
    }

    #[test]
    fn hyperlinks_reads_and_validates_the_closed_set() {
        let path = temp_config("hyperlinks = \"off\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(
            resolved.terminal.hyperlinks,
            Valued {
                value: "off".to_string(),
                source: Source::File
            }
        );
        cleanup(&path);

        let path = temp_config("hyperlinks = \"sometimes\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.terminal.hyperlinks.value, "auto");
        assert_eq!(resolved.terminal.hyperlinks.source, Source::Default);
        cleanup(&path);
    }

    #[test]
    fn theme_light_and_dark_validate_against_known_theme_names() {
        let path = temp_config("theme_light = \"contrast\"\ntheme_dark = \"night\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.terminal.theme_light.value, "contrast");
        assert_eq!(resolved.terminal.theme_dark.value, "night");
        cleanup(&path);

        let path = temp_config("theme_light = \"sepia\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        // Invalid falls back to the default rather than an empty/garbage value.
        assert_eq!(resolved.terminal.theme_light.value, "paper");
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("theme_light"))
        );
        cleanup(&path);
    }

    /// PRD FR-TH-1: "a user theme's name joins Theme::NAMES-equivalent
    /// resolution" — `theme = "solar"` validates against a `themes/solar.toml`
    /// sitting next to `config.toml`, the same directory relationship
    /// `keymap.toml` already has with `config.toml` (`main::run`).
    #[test]
    fn theme_field_accepts_a_user_theme_name_from_the_themes_directory() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wikitui-config-themes-test-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        std::fs::write(
            dir.join("themes").join("solar.toml"),
            "[meta]\nname = \"solar\"\n[colors]\nbg = \"#222222\"\nfg = \"#eeeeee\"\nlink = \"#4488ff\"\n",
        )
        .unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, "theme = \"solar\"\n").unwrap();

        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&config_path),
        );
        assert_eq!(resolved.theme.value, "solar");
        assert_eq!(resolved.theme.source, Source::File);
        assert!(resolved.issues.is_empty(), "{:?}", resolved.issues);

        // theme_light/theme_dark share the exact same resolution.
        std::fs::write(
            &config_path,
            "theme_light = \"solar\"\ntheme_dark = \"solar\"\n",
        )
        .unwrap();
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&config_path),
        );
        assert_eq!(resolved.terminal.theme_light.value, "solar");
        assert_eq!(resolved.terminal.theme_dark.value, "solar");
        assert!(resolved.issues.is_empty(), "{:?}", resolved.issues);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// PRD FR-TH-3: `color_depth` is file/env-resolved against the closed
    /// `auto|truecolor|256|16|mono` set, same pattern as `hyperlinks`.
    #[test]
    fn color_depth_reads_from_file_env_and_validates_the_closed_set() {
        let path = temp_config("color_depth = \"256\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.terminal.color_depth.value, "256");
        assert_eq!(resolved.terminal.color_depth.source, Source::File);
        cleanup(&path);

        let path = temp_config("color_depth = \"bogus\"\n");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert_eq!(resolved.terminal.color_depth.value, "auto");
        assert!(
            resolved
                .issues
                .iter()
                .any(|i| i.message.contains("color_depth"))
        );
        cleanup(&path);

        let path = temp_config("color_depth = \"256\"\n");
        let env = EnvOverrides {
            color_depth: Some("mono".to_string()),
            ..EnvOverrides::default()
        };
        let resolved = resolve(&CliOverrides::default(), &env, Some(&path));
        assert_eq!(resolved.terminal.color_depth.value, "mono");
        assert_eq!(resolved.terminal.color_depth.source, Source::Env);
        cleanup(&path);
    }

    /// PRD FR-ACS-6: `ACCESSIBLE=1` implies the no-motion/plain-link bundle,
    /// but only for keys the reader didn't already pin explicitly.
    #[test]
    fn accessible_bundle_overrides_defaults_but_not_explicit_config() {
        // Nothing set explicitly: ACCESSIBLE flips both to their accessible
        // defaults, tagged as coming from the environment.
        let mut resolved = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        apply_accessible_bundle(&mut resolved, true);
        assert_eq!(
            resolved.terminal.animations,
            Valued {
                value: "none".to_string(),
                source: Source::Env
            }
        );
        assert_eq!(
            resolved.terminal.hyperlinks,
            Valued {
                value: "off".to_string(),
                source: Source::Env
            }
        );

        // An explicit config file value stands even under ACCESSIBLE.
        let path = temp_config("animations = \"full\"\nhyperlinks = \"on\"\n");
        let mut resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        apply_accessible_bundle(&mut resolved, true);
        assert_eq!(resolved.terminal.animations.value, "full");
        assert_eq!(resolved.terminal.animations.source, Source::File);
        assert_eq!(resolved.terminal.hyperlinks.value, "on");
        assert_eq!(resolved.terminal.hyperlinks.source, Source::File);
        cleanup(&path);
    }

    #[test]
    fn accessible_bundle_is_a_noop_when_accessible_is_false() {
        let mut resolved = resolve(&CliOverrides::default(), &EnvOverrides::default(), None);
        apply_accessible_bundle(&mut resolved, false);
        assert_eq!(resolved.terminal.animations.value, "full");
        assert_eq!(resolved.terminal.animations.source, Source::Default);
        assert_eq!(resolved.terminal.hyperlinks.value, "auto");
        assert_eq!(resolved.terminal.hyperlinks.source, Source::Default);
    }

    /// The first-run template's new terminal-integration lines must stay
    /// commented out (a clean load must produce zero issues and every
    /// default), mirroring `write_default_config_writes_a_clean_loadable_file_once`.
    #[test]
    fn default_config_template_terminal_keys_are_commented_and_load_clean() {
        assert!(DEFAULT_CONFIG_TEMPLATE.contains("# mouse = false"));
        assert!(DEFAULT_CONFIG_TEMPLATE.contains("# animations = "));
        assert!(DEFAULT_CONFIG_TEMPLATE.contains("# auto_theme = false"));
        assert!(DEFAULT_CONFIG_TEMPLATE.contains("# hyperlinks = "));

        let path = absent_config_path();
        write_default_config(Some(&path)).expect("write ok");
        let resolved = resolve(
            &CliOverrides::default(),
            &EnvOverrides::default(),
            Some(&path),
        );
        assert!(resolved.issues.is_empty(), "{:?}", resolved.issues);
        assert!(!resolved.terminal.mouse.value);
        assert_eq!(resolved.terminal.animations.value, "full");
        cleanup(&path);
    }
}
