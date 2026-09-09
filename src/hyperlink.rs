//! OSC 8 hyperlinks (PRD FR-RD-2, SEC-2, §6.1's OSC-8 note): terminal-native
//! clickable links for terminals that support the escape (many modern ones
//! do — xterm ≥ 333, kitty, iTerm2, WezTerm, foot, VTE ≥ 0.50, tmux ≥ 3.0
//! passthrough).
//!
//! Emitted as a **post-render pass** (see `main::emit_hyperlinks`), not by
//! injecting escape bytes into a `ratatui::text::Span`'s content: ratatui has
//! no native OSC 8 support (§6.1 / upstream issue #1028), and its
//! `Buffer`/`Span` machinery measures a span's *content string* with
//! `unicode-width` to decide how many cells it occupies. Embedding raw OSC 8
//! bytes in that string would make ratatui itself miscount the line's width
//! (every printable byte of the escape framing — `]`, `8`, `;`, the URL's own
//! characters — reads as an ordinary 1-width character to a width function
//! that doesn't know OSC 8 exists), corrupting wrapping and clipping for
//! everything after it on the line. A post-render pass sidesteps this
//! entirely: ratatui draws the frame with its ordinary, correct width
//! accounting exactly as if hyperlinks didn't exist (no OSC 8 byte is ever
//! part of a `Span`'s text, a `LaidLine`'s text, or anything `layout.rs`
//! measures), and only *after* that frame is on screen does `main.rs`
//! overlay the invisible OSC 8 escapes directly onto the terminal at the
//! exact cell ranges the same layout geometry already reports for each
//! visible link (`Layout::link_at`'s companion range lookup) — a `MoveTo` to
//! the link's first cell, print the zero-width open sequence, `MoveTo` to one
//! past its last cell, print the zero-width close sequence. No width
//! computation anywhere in this codebase ever sees an OSC 8 byte.

/// The OSC 8 open sequence: everything up to (not including) the visible
/// link text. `\x1b\\` (ST) terminates rather than BEL — both are valid per
/// the spec; ST is the form most commonly recommended.
pub fn osc8_open(uri: &str) -> String {
    format!("\x1b]8;;{uri}\x1b\\")
}

/// The OSC 8 close sequence: an empty-URI open, which every implementation
/// treats as "end the current hyperlink."
pub const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

/// SEC-2 hygiene: only `https://`/`http://` URIs may ever become an OSC 8
/// target — no arbitrary scheme from article content (`javascript:`,
/// `data:`, `file:`, a bare relative path, ...) ever reaches a terminal
/// escape sequence. `None` for anything else; callers fall back to plain
/// styled text for a link this rejects (the pre-existing behavior, unchanged
/// for a scheme this module won't wrap).
pub fn sanitize_uri(uri: &str) -> Option<&str> {
    if uri.starts_with("https://") || uri.starts_with("http://") {
        Some(uri)
    } else {
        None
    }
}

/// `hyperlinks = auto|on|off` (PRD FR-RD-2's "config/detection flag").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyperlinkMode {
    /// Emit whenever [`active`]'s heuristic says the session looks like a
    /// real interactive terminal (the default).
    Auto,
    /// Always emit (for a terminal the reader knows supports it, even if the
    /// `auto` heuristic would guess wrong).
    On,
    /// Never emit — always the plain styled text path.
    Off,
}

impl HyperlinkMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// The environment [`active`] consults for the `auto` case.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HyperlinkEnv {
    pub is_tty: bool,
    /// PRD FR-ACS-6: `ACCESSIBLE=1` implies plain link text over OSC 8 even
    /// on a terminal that would otherwise support it — a screen-reader
    /// session has no use for an invisible escape sequence and every use for
    /// the printed URL (`[link: target]`, wired in `App`'s accessible/dump
    /// path, FR-ACS-1).
    pub accessible: bool,
}

/// Whether OSC 8 hyperlinks should be emitted right now. `On`/`Off` are
/// unconditional; `Auto` emits whenever stdout is a real terminal and
/// ACCESSIBLE isn't active.
///
/// Deliberately not a per-terminal capability denylist: an OSC 8 sequence
/// sent to a terminal that doesn't understand it is specified to be silently
/// ignored — the same "harmless when unsupported" property that already lets
/// `main::osc52_clipboard_sequence` skip capability detection entirely — so
/// `Auto` only needs to rule out the cases where emitting it would be
/// actively wrong: a non-tty (the bytes would land in a pipe/redirect's
/// otherwise-plain text, e.g. `wikitui --dump | grep ...`) or an accessible
/// session that wants the plain URL printed instead of an invisible escape a
/// screen reader can't announce.
pub fn active(mode: HyperlinkMode, env: HyperlinkEnv) -> bool {
    match mode {
        HyperlinkMode::Off => false,
        HyperlinkMode::On => true,
        HyperlinkMode::Auto => env.is_tty && !env.accessible,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_and_http_are_allowed() {
        assert_eq!(
            sanitize_uri("https://en.wikipedia.org/wiki/Alan_Turing"),
            Some("https://en.wikipedia.org/wiki/Alan_Turing")
        );
        assert_eq!(
            sanitize_uri("http://en.wikipedia.org/wiki/Alan_Turing"),
            Some("http://en.wikipedia.org/wiki/Alan_Turing")
        );
    }

    /// PRD SEC-2: no arbitrary scheme from content ever becomes an OSC 8
    /// target — these must all fall back to plain text, never a wrapped
    /// escape.
    #[test]
    fn dangerous_and_non_http_schemes_are_rejected() {
        for bad in [
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "vbscript:msgbox(1)",
            "//example.com/protocol-relative",
            "/wiki/Relative_path",
            "",
            "  https://leading-whitespace-not-stripped.example",
        ] {
            assert_eq!(sanitize_uri(bad), None, "must reject {bad:?}");
        }
    }

    #[test]
    fn osc8_open_frames_the_uri_between_escape_and_st() {
        let seq = osc8_open("https://en.wikipedia.org/wiki/Alan_Turing");
        assert_eq!(
            seq,
            "\x1b]8;;https://en.wikipedia.org/wiki/Alan_Turing\x1b\\"
        );
    }

    #[test]
    fn osc8_close_is_an_empty_uri_open() {
        assert_eq!(OSC8_CLOSE, "\x1b]8;;\x1b\\");
    }

    #[test]
    fn mode_parses_the_three_named_values_and_rejects_others() {
        assert_eq!(HyperlinkMode::parse("auto"), Some(HyperlinkMode::Auto));
        assert_eq!(HyperlinkMode::parse("on"), Some(HyperlinkMode::On));
        assert_eq!(HyperlinkMode::parse("off"), Some(HyperlinkMode::Off));
        assert_eq!(HyperlinkMode::parse("maybe"), None);
        assert_eq!(HyperlinkMode::parse(""), None);
    }

    #[test]
    fn on_and_off_ignore_the_environment_entirely() {
        for env in [
            HyperlinkEnv {
                is_tty: true,
                accessible: false,
            },
            HyperlinkEnv {
                is_tty: false,
                accessible: true,
            },
        ] {
            assert!(active(HyperlinkMode::On, env));
            assert!(!active(HyperlinkMode::Off, env));
        }
    }

    #[test]
    fn auto_requires_a_tty_and_respects_accessible() {
        assert!(active(
            HyperlinkMode::Auto,
            HyperlinkEnv {
                is_tty: true,
                accessible: false,
            }
        ));
        assert!(!active(
            HyperlinkMode::Auto,
            HyperlinkEnv {
                is_tty: false,
                accessible: false,
            }
        ));
        // PRD FR-ACS-6: ACCESSIBLE wins even on a real tty.
        assert!(!active(
            HyperlinkMode::Auto,
            HyperlinkEnv {
                is_tty: true,
                accessible: true,
            }
        ));
    }
}
