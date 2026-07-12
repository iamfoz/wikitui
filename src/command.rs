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
    /// `:q` / `:quit` — exit.
    Quit,
}

pub const USAGE: &str = "commands: open <title>, lang <code>, theme <name>, style <name>, library, research, toc, export [style], help, quit";

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
}
