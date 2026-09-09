//! Terminal graphics protocol detection and escape emission (PRD FR-RD-8,
//! §6.3). The PRD's negotiation order is
//! `kitty → iTerm2 → sixel → Unicode half-blocks → alt-text placeholder`;
//! [`detect_protocol`] implements that as a pure function over a snapshot of
//! the relevant environment ([`GraphicsEnv`]) so it is exhaustively
//! table-testable without a real terminal.
//!
//! Only the **half-block** path (`GraphicsProtocol::HalfBlock`) is actually
//! painted by this build (see `image.rs` / `ui.rs`): it needs no terminal
//! cooperation at all — it is just fg/bg-colored `▀` cells — so it is the one
//! path this environment can and does verify end to end. The kitty and iTerm2
//! escape *emitters* below are implemented to spec with byte-framing unit
//! tests, but their runtime use is gated behind detection and they are
//! **unverified against a real kitty/iTerm2 terminal** in this environment
//! (there is none). Sixel emission is deliberately left as a documented stub
//! (`sixel_escape`) — full sixel color-quantization encoding is a larger body
//! of work than this chunk covers; detection scaffolds it behind a force flag
//! so the seam exists.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

/// The graphics capability wikitui will use for one image, highest-fidelity
/// first (PRD FR-RD-8's negotiation chain). `None` is the honest terminal
/// state: alt-text placeholder, never a broken escape (PRD R-5 — "halfblock
/// fallback always works").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsProtocol {
    /// kitty graphics protocol (APC `_G`, base64, chunked).
    Kitty,
    /// iTerm2 inline-image protocol (OSC 1337 `File=`).
    ITerm2,
    /// DEC sixel. Requires a runtime DA1 capability query to confirm; this
    /// build only selects it when explicitly forced (see [`GraphicsEnv`]).
    Sixel,
    /// Unicode upper-half-block (`▀`) cells with truecolor fg/bg — works on
    /// any truecolor terminal with no negotiation. wikitui's always-available
    /// path.
    HalfBlock,
    /// No inline graphics: render the alt-text placeholder.
    None,
}

/// A snapshot of the environment inputs [`detect_protocol`] consults. Built
/// once from the real process environment (plus the app's `no_color` and the
/// active theme/config `images` decision) and passed by value so detection
/// stays a pure, order-independent function of its inputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphicsEnv {
    /// `$TERM` (e.g. `xterm-kitty`, `xterm-256color`).
    pub term: String,
    /// `$TERM_PROGRAM` (e.g. `iTerm.app`, `WezTerm`).
    pub term_program: String,
    /// Whether `$KITTY_WINDOW_ID` is set (kitty exports it even under a
    /// generic `$TERM`).
    pub kitty_window_id: bool,
    /// `$COLORTERM` (`truecolor`/`24bit` gate the half-block path).
    pub colorterm: String,
    /// Whether stdout is a real terminal. Non-tty (pipe, `--dump`, CI) always
    /// resolves to alt text (PRD FR-TH-5: "non-TTY output is plain text").
    pub is_tty: bool,
    /// PRD FR-TH-5's `NO_COLOR`: images are colored output, so honoring it
    /// means no inline graphics.
    pub no_color: bool,
    /// The active theme/config/`:set` images decision (`App::images_enabled`).
    /// A text theme (`images = false`) never renders images regardless of
    /// terminal capability (PRD FR-TH-7).
    pub images_enabled: bool,
    /// Force sixel selection without a DA1 round trip (config/env escape
    /// hatch, off by default). Sixel otherwise needs a runtime query this
    /// build does not perform, so it is never auto-selected.
    pub force_sixel: bool,
}

impl GraphicsEnv {
    /// Reads the terminal-capability variables from the real process
    /// environment. The dynamic bits (`no_color`, `images_enabled`) are set
    /// by the caller, not here, so this stays a cheap one-shot startup read.
    pub fn from_process_env(is_tty: bool) -> Self {
        let get = |k: &str| std::env::var(k).unwrap_or_default();
        Self {
            term: get("TERM"),
            term_program: get("TERM_PROGRAM"),
            kitty_window_id: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            colorterm: get("COLORTERM"),
            is_tty,
            no_color: false,
            images_enabled: false,
            force_sixel: std::env::var("WIKITUI_FORCE_SIXEL")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }
    }
}

/// The documented capability table (PRD FR-RD-8 / §6.3), evaluated in the
/// PRD's negotiation order. The three "off" gates come first because they
/// override any terminal capability:
///   1. images disabled (text theme / `:set images=off`), `NO_COLOR`, or a
///      non-tty → alt text.
///   2. kitty: `$TERM = xterm-kitty` (or contains `kitty`) or
///      `$KITTY_WINDOW_ID` present.
///   3. iTerm2: `$TERM_PROGRAM` is `iTerm.app` or `WezTerm` (WezTerm speaks
///      the iTerm2 inline-image protocol).
///   4. sixel: only when forced (a real DA1 query is out of this chunk).
///   5. half-block: any truecolor terminal (`$COLORTERM` = `truecolor`/`24bit`).
///   6. otherwise `None` — a 16/256-color terminal with no graphics gets the
///      alt-text placeholder rather than a mangled render.
pub fn detect_protocol(env: &GraphicsEnv) -> GraphicsProtocol {
    if !env.images_enabled || env.no_color || !env.is_tty {
        return GraphicsProtocol::None;
    }
    let term = env.term.to_ascii_lowercase();
    if term == "xterm-kitty" || term.contains("kitty") || env.kitty_window_id {
        return GraphicsProtocol::Kitty;
    }
    if env.term_program == "iTerm.app" || env.term_program == "WezTerm" {
        return GraphicsProtocol::ITerm2;
    }
    if env.force_sixel {
        return GraphicsProtocol::Sixel;
    }
    let colorterm = env.colorterm.to_ascii_lowercase();
    if colorterm == "truecolor" || colorterm == "24bit" {
        return GraphicsProtocol::HalfBlock;
    }
    GraphicsProtocol::None
}

// The escape emitters below are spec-complete and unit-tested on their byte
// framing, but not yet wired into the (half-block-only) paint path — their
// use is gated behind `detect_protocol`, and positioned graphics emission is
// deferred (see the module doc). They stay in the shipped binary as
// ready-to-wire scaffolding, hence the `dead_code` allowances.

/// The kitty graphics protocol's maximum base64 payload per APC escape (the
/// documented 4096-byte cap; base64 is ASCII so bytes == chars here).
#[allow(dead_code)]
const KITTY_CHUNK: usize = 4096;

/// Emit a PNG as a kitty-graphics APC (`_G`) transmit-and-display sequence
/// (PRD FR-RD-8's kitty path), base64-encoded and chunked at [`KITTY_CHUNK`].
/// The control keys ride only the first chunk (`a=T` transmit+display,
/// `f=100` PNG); every chunk carries `m=1` except the last (`m=0`), per the
/// kitty spec's multi-chunk framing. Gated behind [`detect_protocol`] at the
/// call site and **unverified against a real kitty terminal here** — the unit
/// tests lock the byte framing only.
#[allow(dead_code)]
pub fn kitty_escape(png: &[u8]) -> String {
    let payload = BASE64.encode(png);
    let mut out = String::new();
    // A zero-byte image still needs one terminating chunk so the parser sees
    // a complete transmission rather than a dangling escape.
    let chunks: Vec<&str> = if payload.is_empty() {
        vec![""]
    } else {
        (0..payload.len())
            .step_by(KITTY_CHUNK)
            .map(|start| &payload[start..(start + KITTY_CHUNK).min(payload.len())])
            .collect()
    };
    let last = chunks.len() - 1;
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i != last);
        out.push_str("\x1b_G");
        if i == 0 {
            out.push_str(&format!("a=T,f=100,m={more}"));
        } else {
            out.push_str(&format!("m={more}"));
        }
        out.push(';');
        out.push_str(chunk);
        out.push_str("\x1b\\");
    }
    out
}

/// Emit a PNG as an iTerm2 inline-image OSC 1337 `File=` sequence (PRD
/// FR-RD-8's iTerm2 path). `inline=1` displays it in place; `size` is the raw
/// byte length; the base64 payload follows the `:` separator, terminated by
/// BEL. WezTerm implements the same protocol. Gated behind [`detect_protocol`]
/// and **unverified against a real iTerm2/WezTerm terminal here** — the unit
/// test locks the byte framing only.
#[allow(dead_code)]
pub fn iterm2_escape(png: &[u8]) -> String {
    let payload = BASE64.encode(png);
    format!("\x1b]1337;File=inline=1;size={}:{}\x07", png.len(), payload)
}

/// Sixel emission is intentionally unimplemented in this chunk (PRD FR-RD-8).
/// Full sixel encoding needs color quantization to a sixel palette plus band
/// packing — materially more work than the half-block path this chunk
/// commits to. The seam exists (`GraphicsProtocol::Sixel` is a detectable,
/// force-selectable state); returning `None` here makes the paint path fall
/// back to half-blocks/placeholder rather than emit a broken sequence.
#[allow(dead_code)]
pub fn sixel_escape(_rgba: &[u8], _width: u32, _height: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> GraphicsEnv {
        GraphicsEnv {
            is_tty: true,
            images_enabled: true,
            colorterm: "truecolor".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn off_gates_win_over_any_terminal_capability() {
        // A kitty terminal still gets alt text when images are disabled,
        // NO_COLOR is set, or output is not a tty.
        let base = GraphicsEnv {
            term: "xterm-kitty".to_string(),
            ..env()
        };
        assert_eq!(
            detect_protocol(&GraphicsEnv {
                images_enabled: false,
                ..base.clone()
            }),
            GraphicsProtocol::None
        );
        assert_eq!(
            detect_protocol(&GraphicsEnv {
                no_color: true,
                ..base.clone()
            }),
            GraphicsProtocol::None
        );
        assert_eq!(
            detect_protocol(&GraphicsEnv {
                is_tty: false,
                ..base
            }),
            GraphicsProtocol::None
        );
    }

    #[test]
    fn kitty_detected_by_term_or_window_id() {
        assert_eq!(
            detect_protocol(&GraphicsEnv {
                term: "xterm-kitty".to_string(),
                ..env()
            }),
            GraphicsProtocol::Kitty
        );
        assert_eq!(
            detect_protocol(&GraphicsEnv {
                term: "xterm-256color".to_string(),
                kitty_window_id: true,
                ..env()
            }),
            GraphicsProtocol::Kitty
        );
    }

    #[test]
    fn iterm2_detected_for_iterm_and_wezterm() {
        for prog in ["iTerm.app", "WezTerm"] {
            assert_eq!(
                detect_protocol(&GraphicsEnv {
                    term_program: prog.to_string(),
                    ..env()
                }),
                GraphicsProtocol::ITerm2
            );
        }
    }

    #[test]
    fn sixel_only_when_forced() {
        assert_eq!(
            detect_protocol(&GraphicsEnv {
                term: "xterm-256color".to_string(),
                colorterm: String::new(),
                force_sixel: true,
                ..env()
            }),
            GraphicsProtocol::Sixel
        );
        // Without the force flag a plain 256-color terminal is not sixel.
        assert_ne!(
            detect_protocol(&GraphicsEnv {
                term: "xterm-256color".to_string(),
                colorterm: String::new(),
                ..env()
            }),
            GraphicsProtocol::Sixel
        );
    }

    #[test]
    fn halfblock_for_truecolor_else_none() {
        assert_eq!(
            detect_protocol(&GraphicsEnv {
                term: "xterm-256color".to_string(),
                colorterm: "truecolor".to_string(),
                ..env()
            }),
            GraphicsProtocol::HalfBlock
        );
        assert_eq!(
            detect_protocol(&GraphicsEnv {
                term: "xterm-256color".to_string(),
                colorterm: "24bit".to_string(),
                ..env()
            }),
            GraphicsProtocol::HalfBlock
        );
        // A non-truecolor terminal with no higher protocol falls to None.
        assert_eq!(
            detect_protocol(&GraphicsEnv {
                term: "xterm-256color".to_string(),
                colorterm: String::new(),
                ..env()
            }),
            GraphicsProtocol::None
        );
    }

    #[test]
    fn kitty_escape_frames_and_round_trips_a_small_payload() {
        let png = b"\x89PNG\r\n\x1a\nHELLO";
        let esc = kitty_escape(png);
        assert!(esc.starts_with("\x1b_Ga=T,f=100,m=0;"));
        assert!(esc.ends_with("\x1b\\"));
        // Single chunk: no trailing continuation escapes.
        assert_eq!(esc.matches("\x1b_G").count(), 1);
        let payload = esc
            .trim_start_matches("\x1b_Ga=T,f=100,m=0;")
            .trim_end_matches("\x1b\\");
        assert_eq!(BASE64.decode(payload).unwrap(), png);
    }

    #[test]
    fn kitty_escape_chunks_large_payloads_with_continuation_flags() {
        // A payload whose base64 exceeds one chunk must split, with m=1 on
        // every chunk but the last (m=0), and the control keys only on the
        // first.
        let png: Vec<u8> = (0..7000u32).map(|i| (i % 256) as u8).collect();
        let esc = kitty_escape(&png);
        let chunk_count = esc.matches("\x1b_G").count();
        assert!(
            chunk_count >= 2,
            "expected multiple chunks, got {chunk_count}"
        );
        assert!(esc.contains("a=T,f=100,m=1"));
        assert!(esc.contains("\x1b_Gm=1;"));
        assert!(esc.contains("m=0;"));
        // Reassemble every chunk's payload and confirm it decodes to the PNG.
        let mut payload = String::new();
        for piece in esc.split("\x1b_G").skip(1) {
            let body = piece.trim_end_matches("\x1b\\");
            let after_semi = body.split_once(';').map(|(_, p)| p).unwrap_or("");
            payload.push_str(after_semi);
        }
        assert_eq!(BASE64.decode(payload).unwrap(), png);
    }

    #[test]
    fn iterm2_escape_frames_and_round_trips() {
        let png = b"\x89PNG\r\n\x1a\nWORLD";
        let esc = iterm2_escape(png);
        assert!(esc.starts_with("\x1b]1337;File="));
        assert!(esc.contains("inline=1"));
        assert!(esc.contains(&format!("size={}", png.len())));
        assert!(esc.ends_with('\x07'));
        let payload = esc
            .rsplit_once(':')
            .map(|(_, p)| p.trim_end_matches('\x07'))
            .unwrap();
        assert_eq!(BASE64.decode(payload).unwrap(), png);
    }

    #[test]
    fn sixel_is_a_documented_stub() {
        assert!(sixel_escape(&[0, 0, 0, 255], 1, 1).is_none());
    }
}
