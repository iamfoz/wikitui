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
    pub measure: Valued<u16>,
    pub ambiguous_wide: Valued<bool>,
    pub cite_style: Valued<String>,
    pub cache_max_mb: Valued<u64>,
    pub cache_fresh_ttl_hours: Valued<u64>,
    /// PRD FR-OFF-2's force-refetch backstop: entries older than this are
    /// treated as absent on open, network-first, regardless of what a
    /// background revalidation might otherwise have decided.
    pub cache_force_refetch_days: Valued<u64>,
    pub active_wiki: Valued<String>,
    /// Always concrete (defaults to `DEFAULT_BASE_URL_TEMPLATE`); `{lang}`
    /// is substituted by `api::WikiClient` when present, else used
    /// verbatim (arbitrary MediaWiki sites, FR-ML-5, aren't per-language).
    pub base_url_template: Valued<String>,
    /// PRD FR-BM-3's "(config)" read-later behavior: whether opening a
    /// queued entry removes it. Defaults to `true`.
    pub readlater_auto_dequeue: Valued<bool>,
    /// PRD FR-HS-4's retention window: `history::History::retention_prune`
    /// runs with this at startup. `0` (the default) means "keep forever."
    pub history_retention_days: Valued<u64>,
    /// PRD FR-TH-7's `images` key: overrides the active theme's `images`
    /// default when set (`Some(true)`/`Some(false)`); `None` (the default)
    /// means "follow the theme." Wired into `App::images_override`.
    pub images: Valued<Option<bool>>,
    /// PRD §10 licensing: whether non-free/fair-use images may be used.
    /// Default false. Today a policy flag with a documented seam (saved-page
    /// image persistence, which must exclude non-free, isn't built yet).
    pub include_nonfree: Valued<bool>,
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

    let known_top_level: BTreeSet<&str> = [
        "config_version",
        "lang",
        "languages",
        "theme",
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
            if Theme::by_name(s).is_some() {
                Ok(s.to_string())
            } else {
                Err(format!(
                    "unknown theme {s:?} — one of: {}",
                    Theme::NAMES.join(", ")
                ))
            }
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
    let (cache_max_mb, cache_fresh_ttl_hours, cache_force_refetch_days) =
        resolve_cache(&table, &mut issues);
    let (active_wiki, base_url_template) = resolve_wiki(env, &table, &mut issues);
    let readlater_auto_dequeue = resolve_readlater_auto_dequeue(env, &table, &mut issues);
    let history_retention_days = resolve_history(&table, &mut issues);
    let images = resolve_images(env, &table, &mut issues);
    let include_nonfree = resolve_include_nonfree(&table, &mut issues);

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
        measure,
        ambiguous_wide,
        cite_style,
        cache_max_mb,
        cache_fresh_ttl_hours,
        cache_force_refetch_days,
        active_wiki,
        base_url_template,
        readlater_auto_dequeue,
        history_retention_days,
        images,
        include_nonfree,
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
    const MIN: i64 = 40;
    const MAX: i64 = 200;
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

fn resolve_cache(
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> (Valued<u64>, Valued<u64>, Valued<u64>) {
    let default_max_mb = crate::cache::DEFAULT_MAX_BYTES / (1024 * 1024);
    let default_ttl_hours = crate::cache::FRESH_TTL_SECS / 3600;
    let default_force_refetch_days = crate::cache::DEFAULT_FORCE_REFETCH_SECS / 86_400;

    let Some(cache_table) = table.get("cache").and_then(toml::Value::as_table) else {
        if table.contains_key("cache") {
            issues.push(Issue::warning(
                "cache must be a table (use [cache] with max_mb/fresh_ttl_hours/force_refetch_days); ignoring",
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
        );
    };

    let known: BTreeSet<&str> = ["max_mb", "fresh_ttl_hours", "force_refetch_days"]
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
    (max_mb, fresh_ttl_hours, force_refetch_days)
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

fn resolve_wiki(
    env: &EnvOverrides,
    table: &toml::Table,
    issues: &mut Vec<Issue>,
) -> (Valued<String>, Valued<String>) {
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
                if key != "base_url" {
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

    let active_name = match table.get("active_wiki") {
        Some(v) => match v.as_str() {
            Some(name) => {
                let known = name == DEFAULT_WIKI_NAME
                    || wiki_sections.is_some_and(|s| s.contains_key(name));
                if known {
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
    };

    let mut base_url = Valued {
        value: DEFAULT_BASE_URL_TEMPLATE.to_string(),
        source: Source::Default,
    };
    if let Some(sections) = wiki_sections
        && let Some(section) = sections.get(&active_name.value)
        && let Some(url) = section
            .as_table()
            .and_then(|t| t.get("base_url"))
            .and_then(toml::Value::as_str)
    {
        base_url = Valued {
            value: url.to_string(),
            source: Source::File,
        };
    }
    if let Some(url) = &env.base_url {
        base_url = Valued {
            value: url.clone(),
            source: Source::Env,
        };
    }

    (active_name, base_url)
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
}
