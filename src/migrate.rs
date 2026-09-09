//! wiki-tui migration (PRD FR-TH-8, goal G3): a best-effort importer for
//! wiki-tui's (github.com/Builditluc/wiki-tui) `config.toml` — its `[theme]`
//! table onto wikitui's own semantic-slot theme schema (Appendix C), and its
//! `[keybindings]` table onto wikitui's command registry (`registry::
//! Action`) — writing a `themes/*.toml` and a `keymap.toml` into wikitui's
//! own config directory. wiki-tui's maintainer archived the project,
//! stranding its userbase; this is the documented on-ramp for them (goal G3:
//! "become the reference client for the orphaned wiki-tui niche").
//!
//! **This build cannot fetch wiki-tui's current schema from its repository
//! or documentation** (no live network access to non-Wikipedia hosts in this
//! environment), so the key names below are this build's best-effort
//! recollection of its config shape, not a verified spec. Every slot/action
//! tries several plausible spellings (documented alongside each table) to
//! hedge against that uncertainty, and — critically — an unrecognized key is
//! always a warning, never a fatal error: a real wiki-tui user's config may
//! use different names than the ones assumed here, in which case this
//! importer degrades to "derive everything from what little it did
//! recognize" rather than refusing to run. The summary this module returns
//! is meant to be read: it tells the user exactly what carried over and
//! what didn't, so a manual touch-up is an expected, not a failure, outcome.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::registry::{self, Action, Chord, KeyContext};
use crate::theme::{self, SemanticSlots};

/// wikitui semantic slot -> the wiki-tui `[theme]` key names this build
/// guesses might hold that color, tried in order. wiki-tui's actual key
/// names are unverified (see module doc comment) — `background`/
/// `foreground`/`title`/`highlight`* are the shape a cursive/ratatui-based
/// single-pane reader's theme table plausibly takes, not a confirmed spec.
const THEME_SLOT_ALIASES: &[(&str, &[&str])] = &[
    ("bg", &["background", "bg"]),
    ("fg", &["foreground", "fg", "text"]),
    // wiki-tui's "title" color is the closest analogue to an accent/link
    // hue in a reader with no separate link-vs-heading distinction; reused
    // for both, then let `expand_slots`' own derivation chain take it from
    // there (accent -> link -> heading).
    ("accent", &["title", "accent", "heading"]),
    (
        "link_visited",
        &["highlight_inactive", "visited", "visited_link"],
    ),
    ("match", &["search_match", "highlight_text", "match"]),
    ("dim", &["disabled", "dim"]),
];

/// wikitui registry action name -> the wiki-tui `[keybindings]` key names
/// this build guesses might bind it, tried in order. Same unverified-schema
/// caveat as `THEME_SLOT_ALIASES`.
const KEYBIND_ALIASES: &[(&str, &[&str])] = &[
    ("scroll-up", &["up", "scroll_up", "move_up"]),
    ("scroll-down", &["down", "scroll_down", "move_down"]),
    ("table-scroll-left", &["left", "scroll_left"]),
    ("table-scroll-right", &["right", "scroll_right"]),
    ("half-page-up", &["page_up", "scroll_page_up"]),
    ("half-page-down", &["page_down", "scroll_page_down"]),
    ("scroll-top", &["top", "scroll_top", "go_to_top"]),
    (
        "scroll-bottom",
        &["bottom", "scroll_bottom", "go_to_bottom"],
    ),
    (
        "follow-link",
        &["select", "toggle", "confirm", "open_link", "enter"],
    ),
    ("history-back", &["back", "go_back"]),
    ("history-forward", &["forward", "go_forward"]),
    ("search", &["search", "open_search", "open_search_bar"]),
    (
        "toc",
        &["toc", "open_toc", "toggle_toc", "table_of_contents"],
    ),
    ("help", &["help", "toggle_help", "open_help"]),
    ("quit", &["quit", "exit"]),
];

/// The section name `registry::Keymap::apply_user_toml` expects for each
/// context — the inverse of `KeyContext::from_toml_name` (private to
/// `registry`, so this is a second small table rather than a visibility
/// change to a module this chunk doesn't otherwise touch).
fn context_toml_name(ctx: KeyContext) -> &'static str {
    match ctx {
        KeyContext::Global => "global",
        KeyContext::Reading => "reading",
        KeyContext::StartPage => "startpage",
        KeyContext::Picker => "picker",
        KeyContext::Search => "search",
    }
}

/// One resolved keybinding: which wikitui keymap section/chord it writes,
/// and the wiki-tui key string it came from (kept for the summary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedBinding {
    pub context: &'static str,
    pub key: String,
    pub action_name: &'static str,
    pub from_wikitui_key: String,
}

/// The full result of importing one wiki-tui `config.toml`'s text —
/// filesystem-free and directly testable; [`import_to_config_dir`] is the
/// thin I/O wrapper around this.
#[derive(Debug, Clone, Default)]
pub struct ImportResult {
    pub theme_name: String,
    pub slots: SemanticSlots,
    /// (wikitui slot, wiki-tui key it came from).
    pub theme_mapped: Vec<(String, String)>,
    /// wiki-tui `[theme]` keys present but not recognized by any alias.
    pub theme_unmapped: Vec<String>,
    pub keymap_bindings: Vec<MappedBinding>,
    /// wiki-tui `[keybindings]` keys present but not recognized by any alias.
    pub keymap_unmapped: Vec<String>,
    /// A recognized action whose wiki-tui key string didn't parse as a
    /// wikitui chord (e.g. an unrecognized named key) — recorded, not
    /// written.
    pub keymap_bad_keys: Vec<String>,
}

/// Parses wiki-tui's `[theme]` table into wikitui's [`SemanticSlots`] via
/// `THEME_SLOT_ALIASES`, and its `[keybindings]` table into wikitui
/// `keymap.toml` bindings via `KEYBIND_ALIASES` — the pure core of the
/// importer (deliverable 4). Never fails: a config with neither table still
/// returns a result (an all-derived theme, an empty keymap) rather than an
/// error, matching "unknown keys warned, never fatal."
pub fn import_from_text(wiki_tui_toml: &str, name_hint: &str) -> Result<ImportResult, String> {
    let table: toml::Table =
        toml::from_str(wiki_tui_toml).map_err(|e| format!("not valid TOML: {e}"))?;

    let mut result = ImportResult {
        theme_name: name_hint.to_string(),
        ..ImportResult::default()
    };

    if let Some(theme_table) = table.get("theme").and_then(toml::Value::as_table) {
        let mut claimed: BTreeSet<&str> = BTreeSet::new();
        let get_str = |k: &str| theme_table.get(k).and_then(toml::Value::as_str);
        for (slot, aliases) in THEME_SLOT_ALIASES {
            for alias in *aliases {
                if let Some(raw) = get_str(alias) {
                    claimed.insert(alias);
                    if let Some(color) = theme::parse_hex_color(raw) {
                        set_slot(&mut result.slots, slot, color);
                        result
                            .theme_mapped
                            .push(((*slot).to_string(), (*alias).to_string()));
                        break;
                    }
                }
            }
        }
        for key in theme_table.keys() {
            if !claimed.contains(key.as_str()) {
                result.theme_unmapped.push(key.clone());
            }
        }
    }

    if let Some(kb_table) = table.get("keybindings").and_then(toml::Value::as_table) {
        let mut claimed: BTreeSet<&str> = BTreeSet::new();
        for (action_name, aliases) in KEYBIND_ALIASES {
            let Some(action) = Action::by_name(action_name) else {
                continue; // defensive: a typo in KEYBIND_ALIASES, never a user-facing panic
            };
            for alias in *aliases {
                let Some(raw) = kb_table.get(*alias).and_then(toml::Value::as_str) else {
                    continue;
                };
                claimed.insert(alias);
                match Chord::parse(raw) {
                    Some(_chord) => {
                        let ctx = registry::meta(action)
                            .contexts
                            .first()
                            .copied()
                            .unwrap_or(KeyContext::Reading);
                        result.keymap_bindings.push(MappedBinding {
                            context: context_toml_name(ctx),
                            key: raw.to_string(),
                            action_name,
                            from_wikitui_key: (*alias).to_string(),
                        });
                    }
                    None => result
                        .keymap_bad_keys
                        .push(format!("{alias} = {raw:?} (unrecognized key syntax)")),
                }
                break;
            }
        }
        for key in kb_table.keys() {
            if !claimed.contains(key.as_str()) {
                result.keymap_unmapped.push(key.clone());
            }
        }
    }

    Ok(result)
}

fn set_slot(slots: &mut SemanticSlots, slot: &str, color: ratatui::style::Color) {
    match slot {
        "bg" => slots.bg = Some(color),
        "fg" => slots.fg = Some(color),
        "accent" => {
            slots.accent = Some(color);
            slots.link.get_or_insert(color);
            slots.heading.get_or_insert(color);
        }
        "link_visited" => slots.link_visited = Some(color),
        "match" => slots.match_fg = Some(color),
        "dim" => slots.dim = Some(color),
        _ => {}
    }
}

/// Serializes `slots` into wikitui's native theme-file TOML (Appendix C's
/// `[meta]`/`[colors]` schema) — only the slots that were actually set are
/// written; everything else is left for `theme::expand_slots`' own
/// derivation to fill in the next time this file loads, exactly as it would
/// for a hand-written minimal theme file.
pub fn render_theme_toml(name: &str, slots: &SemanticSlots) -> String {
    let mut out = format!(
        "# Imported from a wiki-tui config (PRD FR-TH-8).\n# Review the colors below — derived slots are filled in automatically\n# when this file loads (see the [colors] schema in the wikitui docs).\n[meta]\nname = \"{name}\"\n\n[colors]\n"
    );
    let mut push = |key: &str, c: Option<ratatui::style::Color>| {
        if let Some(c) = c {
            out.push_str(&format!("{key} = \"{c}\"\n"));
        }
    };
    push("bg", slots.bg);
    push("fg", slots.fg);
    push("accent", slots.accent);
    push("link", slots.link);
    push("link_visited", slots.link_visited);
    push("heading", slots.heading);
    push("quote", slots.quote);
    push("code_bg", slots.code);
    push("match", slots.match_fg);
    push("warning", slots.warning);
    push("error", slots.error);
    push("dim", slots.dim);
    out
}

/// Serializes the mapped keybindings into a `keymap.toml`
/// (`registry::Keymap::apply_user_toml`'s own format: `[context]` sections,
/// `"key" = "action-name"` entries), grouped by context in the fixed order
/// `global, reading, startpage, picker, search` for stable, readable output.
pub fn render_keymap_toml(bindings: &[MappedBinding]) -> String {
    let mut out = String::from(
        "# Imported from a wiki-tui config (PRD FR-TH-8).\n# Unmapped wiki-tui bindings are listed in the import summary, not here.\n",
    );
    for ctx in ["global", "reading", "startpage", "picker", "search"] {
        let rows: Vec<&MappedBinding> = bindings.iter().filter(|b| b.context == ctx).collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("\n[{ctx}]\n"));
        for row in rows {
            out.push_str(&format!("{:?} = {:?}\n", row.key, row.action_name));
        }
    }
    out
}

/// Everything [`import_to_config_dir`] actually wrote (or chose not to, to
/// avoid clobbering an existing file), plus the same mapping summary
/// [`import_from_text`] produced — what the CLI subcommand prints.
#[derive(Debug, Clone)]
pub struct Written {
    pub result: ImportResult,
    pub theme_path: PathBuf,
    pub keymap_path: PathBuf,
    /// True when `keymap_path` is an alternate name because `keymap.toml`
    /// already existed — the caller should tell the user to merge by hand.
    pub keymap_renamed: bool,
}

/// Resolves `path` to a wiki-tui `config.toml`: used directly if it's a
/// file, or looked for as `config.toml` inside it if it's a directory —
/// the "reads a wiki-tui config dir/file" shape FR-TH-8 asks for.
fn resolve_source_file(path: &Path) -> Result<PathBuf, String> {
    if path.is_dir() {
        let candidate = path.join("config.toml");
        if candidate.is_file() {
            Ok(candidate)
        } else {
            Err(format!(
                "{} is a directory with no config.toml in it",
                path.display()
            ))
        }
    } else if path.is_file() {
        Ok(path.to_path_buf())
    } else {
        Err(format!("{} does not exist", path.display()))
    }
}

/// A path that doesn't exist yet: `path` itself if it's free, else
/// `<stem>-1.<ext>`, `<stem>-2.<ext>`, … — never clobbers an existing file
/// (this build's own config writer, `config::write_default_config`, applies
/// the same "never clobber a user's config" rule for the same reason).
fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("theme")
        .to_string();
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    for n in 1..1000 {
        let candidate = dir.join(format!("{stem}-{n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

/// The full `wikitui import-wiki-tui <path>` implementation: reads
/// `wiki_tui_path` (a file or a directory containing `config.toml`),
/// converts it, and writes `themes/<name>.toml` + `keymap.toml` into
/// `wikitui_config_dir` (creating both directories as needed) — never
/// overwriting an existing `keymap.toml` (writes `keymap.from-wiki-tui.toml`
/// instead, flagged in the returned [`Written::keymap_renamed`], so a
/// reader's own customizations are never silently clobbered).
pub fn import_to_config_dir(
    wiki_tui_path: &Path,
    wikitui_config_dir: &Path,
) -> Result<Written, String> {
    let source = resolve_source_file(wiki_tui_path)?;
    let text = std::fs::read_to_string(&source)
        .map_err(|e| format!("could not read {}: {e}", source.display()))?;
    let name_hint = "wiki-tui-import";
    let result = import_from_text(&text, name_hint)?;

    let themes_dir = wikitui_config_dir.join("themes");
    std::fs::create_dir_all(&themes_dir)
        .map_err(|e| format!("could not create {}: {e}", themes_dir.display()))?;
    let theme_path = unique_path(themes_dir.join(format!("{}.toml", result.theme_name)));
    std::fs::write(
        &theme_path,
        render_theme_toml(&result.theme_name, &result.slots),
    )
    .map_err(|e| format!("could not write {}: {e}", theme_path.display()))?;

    std::fs::create_dir_all(wikitui_config_dir)
        .map_err(|e| format!("could not create {}: {e}", wikitui_config_dir.display()))?;
    let default_keymap_path = wikitui_config_dir.join("keymap.toml");
    let keymap_renamed = default_keymap_path.exists();
    let keymap_path = if keymap_renamed {
        wikitui_config_dir.join("keymap.from-wiki-tui.toml")
    } else {
        default_keymap_path
    };
    std::fs::write(&keymap_path, render_keymap_toml(&result.keymap_bindings))
        .map_err(|e| format!("could not write {}: {e}", keymap_path.display()))?;

    Ok(Written {
        result,
        theme_path,
        keymap_path,
        keymap_renamed,
    })
}

/// Formats [`Written`] into the human-readable summary `main` prints —
/// what mapped, what didn't, and where the files landed (deliverable 4's
/// "printing a summary of what mapped and what didn't").
pub fn summarize(written: &Written) -> String {
    let r = &written.result;
    let mut out = String::new();
    out.push_str(&format!(
        "Imported theme -> {}\n",
        written.theme_path.display()
    ));
    for (slot, key) in &r.theme_mapped {
        out.push_str(&format!("  [theme] {key} -> {slot}\n"));
    }
    for key in &r.theme_unmapped {
        out.push_str(&format!(
            "  [theme] {key}: not recognized — not imported (derive manually if needed)\n"
        ));
    }
    if written.keymap_renamed {
        out.push_str(&format!(
            "Imported keymap -> {} (keymap.toml already existed — merge by hand)\n",
            written.keymap_path.display()
        ));
    } else {
        out.push_str(&format!(
            "Imported keymap -> {}\n",
            written.keymap_path.display()
        ));
    }
    for b in &r.keymap_bindings {
        out.push_str(&format!(
            "  [keybindings] {} = {:?} -> {} ({})\n",
            b.from_wikitui_key, b.key, b.action_name, b.context
        ));
    }
    for key in &r.keymap_unmapped {
        out.push_str(&format!(
            "  [keybindings] {key}: not recognized — not imported\n"
        ));
    }
    for bad in &r.keymap_bad_keys {
        out.push_str(&format!("  [keybindings] {bad} — not imported\n"));
    }
    out.push_str(&format!(
        "\nRun `:theme {}` to try it; `wikitui config doctor` re-checks its contrast.\n",
        written.result.theme_name
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    const SAMPLE_WIKI_TUI_CONFIG: &str = r##"
        [settings]
        language = "en"

        [theme]
        background = "#1e1e1e"
        foreground = "#c8ccd4"
        title = "#61afef"
        search_match = "#e5c07b"
        disabled = "#5c6370"
        some_unknown_theme_key = "#ff00ff"

        [keybindings]
        up = "k"
        down = "j"
        select = "Enter"
        back = "Backspace"
        search = "/"
        toggle_help = "?"
        quit = "q"
        some_unknown_action = "z"
    "##;

    #[test]
    fn maps_known_theme_keys_and_warns_on_unmapped() {
        let result = import_from_text(SAMPLE_WIKI_TUI_CONFIG, "wiki-tui-import").unwrap();
        assert_eq!(result.slots.bg, Some(Color::Rgb(0x1e, 0x1e, 0x1e)));
        assert_eq!(result.slots.fg, Some(Color::Rgb(0xc8, 0xcc, 0xd4)));
        assert_eq!(result.slots.accent, Some(Color::Rgb(0x61, 0xaf, 0xef)));
        assert_eq!(result.slots.link, Some(Color::Rgb(0x61, 0xaf, 0xef)));
        assert_eq!(result.slots.match_fg, Some(Color::Rgb(0xe5, 0xc0, 0x7b)));
        assert_eq!(result.slots.dim, Some(Color::Rgb(0x5c, 0x63, 0x70)));
        assert!(
            result
                .theme_unmapped
                .contains(&"some_unknown_theme_key".to_string())
        );
        assert!(!result.theme_mapped.is_empty());
    }

    #[test]
    fn maps_known_keybindings_and_warns_on_unmapped() {
        let result = import_from_text(SAMPLE_WIKI_TUI_CONFIG, "wiki-tui-import").unwrap();
        let find = |action: &str| {
            result
                .keymap_bindings
                .iter()
                .find(|b| b.action_name == action)
        };
        assert_eq!(find("scroll-up").unwrap().key, "k");
        assert_eq!(find("scroll-down").unwrap().key, "j");
        assert_eq!(find("follow-link").unwrap().key, "Enter");
        assert_eq!(find("history-back").unwrap().key, "Backspace");
        assert_eq!(find("search").unwrap().key, "/");
        assert_eq!(find("help").unwrap().key, "?");
        assert_eq!(find("quit").unwrap().key, "q");
        assert!(
            result
                .keymap_unmapped
                .contains(&"some_unknown_action".to_string())
        );
    }

    #[test]
    fn a_config_with_neither_table_still_imports_cleanly() {
        let result = import_from_text("[settings]\nlanguage = \"en\"\n", "empty").unwrap();
        assert!(result.theme_mapped.is_empty());
        assert!(result.keymap_bindings.is_empty());
    }

    #[test]
    fn bad_toml_is_an_error_not_a_panic() {
        assert!(import_from_text("not [ valid toml", "x").is_err());
    }

    #[test]
    fn rendered_theme_toml_round_trips_through_the_native_parser() {
        let result = import_from_text(SAMPLE_WIKI_TUI_CONFIG, "wiki-tui-import").unwrap();
        let rendered = render_theme_toml(&result.theme_name, &result.slots);
        let (slots, _) = crate::theme::parse_native_theme_toml(&rendered, "fallback").unwrap();
        assert_eq!(slots.bg, result.slots.bg);
        assert_eq!(slots.link, result.slots.link);
        // A full Theme still builds from it (derivation fills the rest).
        let theme = crate::theme::expand_slots(&slots);
        assert_eq!(theme.bg, result.slots.bg);
    }

    #[test]
    fn rendered_keymap_toml_round_trips_through_the_registry_parser() {
        let result = import_from_text(SAMPLE_WIKI_TUI_CONFIG, "wiki-tui-import").unwrap();
        let rendered = render_keymap_toml(&result.keymap_bindings);
        let mut km = registry::Keymap::vim();
        let warnings = km.apply_user_toml(&rendered);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(
            km.runtime_action(KeyContext::Reading, &Chord::ch('k')),
            Some(Action::ScrollUp)
        );
        assert_eq!(
            km.runtime_action(KeyContext::Reading, &Chord::ch('q')),
            Some(Action::Quit)
        );
    }

    #[test]
    fn import_to_config_dir_writes_theme_and_keymap_and_never_clobbers() {
        let tmp = std::env::temp_dir().join(format!(
            "wikitui-migrate-test-{}-{}",
            std::process::id(),
            "write"
        ));
        let source_dir = tmp.join("wiki-tui-src");
        let config_dir = tmp.join("wikitui-config");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), SAMPLE_WIKI_TUI_CONFIG).unwrap();

        let written = import_to_config_dir(&source_dir, &config_dir).unwrap();
        assert!(written.theme_path.exists());
        assert!(written.keymap_path.exists());
        assert!(!written.keymap_renamed);

        // A pre-existing keymap.toml must never be overwritten.
        let written2 = import_to_config_dir(&source_dir, &config_dir).unwrap();
        assert!(written2.keymap_renamed);
        assert_ne!(written2.keymap_path, written.keymap_path);
        assert!(written.keymap_path.exists(), "original must survive");

        let summary = summarize(&written);
        assert!(summary.contains("Imported theme"));
        assert!(summary.contains("Imported keymap"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn import_to_config_dir_reports_a_missing_source_clearly() {
        let tmp = std::env::temp_dir().join(format!(
            "wikitui-migrate-test-{}-{}",
            std::process::id(),
            "missing"
        ));
        let result = import_to_config_dir(&tmp.join("nope"), &tmp.join("cfg"));
        assert!(result.is_err());
    }
}
