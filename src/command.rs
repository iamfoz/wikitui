//! The `:` ex-style command line (PRD FR-CS-2, MVP slice): a small set of
//! named commands with short aliases, mapping onto features that already
//! exist. Argument completion and ranges are later phases; unknown
//! commands produce an error message listing what's available.

use crate::cite::CiteStyle;
use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `:open <title>` / `:o` — open an article (also accepts URLs and
    /// lang-prefixed titles, same grammar as the CLI TITLE argument).
    Open(String),
    /// `:lang <code>` — switch the language edition for searches and new
    /// articles.
    Lang(String),
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
    /// `:q` / `:quit` — exit.
    Quit,
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

pub const USAGE: &str = "commands: open <title>, lang <code>, theme <name>, style <name>, library, research, toc, export [style], tab close|new [title], tabs, bookmarks [export md|html|json|netscape [path]], readlater, history [clear today|all], config reload, help, quit";

pub fn parse(input: &str) -> Result<Command, String> {
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
        "lang" => {
            let code = require_arg("code")?;
            if crate::target::is_lang_code(&code) {
                Ok(Command::Lang(code))
            } else {
                Err(format!(
                    "{code:?} doesn't look like a language code (e.g. en, de, zh-yue)"
                ))
            }
        }
        "theme" => {
            let theme = require_arg("name")?;
            if Theme::by_name(&theme).is_some() {
                Ok(Command::Theme(theme))
            } else {
                Err(format!(
                    "unknown theme {theme:?} — one of: {}",
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
        "help" | "h" => Ok(Command::Help),
        "q" | "quit" => Ok(Command::Quit),
        "" => Err(USAGE.to_string()),
        other => Err(format!("unknown command {other:?} — {USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parse("lang de"), Ok(Command::Lang("de".to_string())));
        assert_eq!(
            parse("lang zh-yue"),
            Ok(Command::Lang("zh-yue".to_string()))
        );
        assert!(parse("lang DE!").is_err());
        assert!(parse("lang").is_err());
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
    fn history_clear_rejects_bad_input() {
        assert!(parse("history clear").is_err());
        assert!(parse("history clear yesterday").is_err());
        assert!(parse("history frobnicate").is_err());
    }
}
