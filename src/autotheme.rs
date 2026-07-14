//! Auto light/dark theme detection (PRD FR-TH-4): query the terminal's
//! background color via OSC 11, guarded by a DA1 (Primary Device Attributes)
//! query sent right after it, so a terminal that doesn't understand OSC 11
//! still answers *something* almost immediately — the DA1 reply's arrival is
//! what tells [`resolve_via_io`] it is safe to stop waiting for an OSC 11
//! reply that will never come, rather than hanging until a fixed timeout on
//! every unsupporting terminal. DEC private mode 2031 ("color scheme change
//! notifications") shares the same OSC 11 response grammar for its
//! unprompted push, so [`parse_osc11_response`] handles both.
//!
//! Everything up to [`resolve_via_io`] is a pure function of bytes in,
//! decision out, and is exhaustively unit-tested against canned terminal
//! replies (real and malformed). [`resolve_via_io`] adds the guarded-read
//! loop and is tested here too, but only against an in-memory mock — no
//! terminal emulator in this build's test/pty environment answers a real
//! OSC 11 query (the pty harness's "terminal" is a Python script that never
//! sends unsolicited replies), the same limitation `graphics.rs` documents
//! for the kitty/iTerm2 escape emitters it can format but not prove against a
//! real terminal. `main::query_terminal_bg` wraps this in a spawned-thread
//! timeout for the real `io::stdin()`/`io::stdout()` case — see its own doc
//! comment for why that boundary, not this module, owns the actual
//! wall-clock guarantee against a `read()` that blocks forever.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::theme::relative_luminance;

/// The OSC 11 query: "what is the background color?" A terminal that
/// understands it replies with the same escape, background color filled in.
pub const OSC11_QUERY: &str = "\x1b]11;?\x1b\\";

/// Primary Device Attributes query (DA1), sent immediately after
/// [`OSC11_QUERY`] so a terminal with no OSC 11 support still answers this
/// one almost instantly (PRD FR-TH-4: "guarded by DA1 so unsupporting
/// terminals don't hang").
pub const DA1_QUERY: &str = "\x1b[c";

/// DEC private mode 2031: "notify me when the color scheme changes" (PRD
/// FR-TH-4's live-switching half). Best-effort — ignored outright by any
/// terminal that doesn't recognize the mode number.
///
/// **Not sent by this build.** Enabling it would make a genuinely
/// OSC-11-capable terminal push an *unprompted* OSC 11 response later in the
/// session, on top of the ordinary crossterm key/mouse event stream this
/// build reads from the same `stdin` — and nothing in this chunk's event
/// loop consumes such a push (live re-theming mid-session is a documented
/// scope cut, not wired into `main::run`'s loop, which has no distinct
/// "raw terminal reply" event type to route it through without a much larger
/// change to how input is read). Turning the notifications on without
/// anything to consume them would only add the risk of stray bytes
/// eventually reaching crossterm's key parser, for zero benefit — so this
/// constant stays spec-complete and byte-tested (ready to wire once a
/// consumer exists) exactly like `graphics.rs`'s not-yet-wired kitty/iTerm2
/// escape emitters, rather than actually issued anywhere.
#[allow(dead_code)]
pub const ENABLE_COLOR_SCHEME_NOTIFICATIONS: &str = "\x1b[?2031h";
/// The disabling counterpart, sent unconditionally on terminal restore
/// (`crashguard::restore_terminal_best_effort`) exactly like
/// `DisableMouseCapture` — harmless whether or not this session ever sent
/// the enable sequence above.
pub const DISABLE_COLOR_SCHEME_NOTIFICATIONS: &str = "\x1b[?2031l";

/// Light or dark, once a background color has been classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgMode {
    Light,
    Dark,
}

/// WCAG relative luminance above this classifies as light; at/below, dark.
/// 0.5 sits at "medium gray" on the 0.0..1.0 luminance scale — comfortably on
/// the correct side of both a near-white "paper" background and a near-black
/// terminal default, which is all a light/dark split needs.
pub const LUMINANCE_LIGHT_THRESHOLD: f64 = 0.5;

/// Classifies a background color as light or dark by WCAG relative
/// luminance, reusing `theme::relative_luminance` (FR-TH-6's own contrast
/// math) rather than a second formula that could drift from it.
pub fn classify_luminance(rgb: (u8, u8, u8)) -> BgMode {
    if relative_luminance(rgb) > LUMINANCE_LIGHT_THRESHOLD {
        BgMode::Light
    } else {
        BgMode::Dark
    }
}

/// Parses one OSC 11 response — a query reply or a DEC 2031 push
/// notification, which share the same grammar:
/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB (BEL | ESC \)`. Each channel is a
/// terminal-chosen hex value (xterm's own convention allows 1-4 hex digits
/// per channel); only the leading two significant digits are read, which is
/// all [`classify_luminance`] needs. `None` on anything that doesn't match —
/// a truncated, reordered, or garbled response must never panic or
/// misclassify (PRD: "malformed -> no-switch").
pub fn parse_osc11_response(raw: &str) -> Option<(u8, u8, u8)> {
    let body = raw.strip_prefix("\x1b]11;rgb:")?;
    let end = body.find(['\x07', '\x1b']).unwrap_or(body.len());
    let body = &body[..end];
    let mut parts = body.split('/');
    let r = parse_channel(parts.next()?)?;
    let g = parse_channel(parts.next()?)?;
    let b = parse_channel(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

/// One channel's leading hex digits, downsampled to a byte. A channel is
/// required to have at least one hex digit; a bare 4-digit value like
/// `"e100"` reads its top byte (`0xe1`) the same way a 1-digit value like
/// `"f"` reads as `0xff`-scaled... except the simpler, honestly-documented
/// choice made here is to require at least 2 hex digits and read exactly
/// those as the byte, which is what every terminal actually emits in
/// practice (`rgb:ffff/0000/0000`, never a bare single digit).
fn parse_channel(hex: &str) -> Option<u8> {
    if hex.len() < 2 {
        return None;
    }
    u8::from_str_radix(&hex[..2], 16).ok()
}

/// What the guarded read loop has seen so far in the accumulated buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanOutcome {
    /// A complete, parseable OSC 11 response is in the buffer.
    Resolved(BgMode),
    /// A DA1 reply's own terminator (`c`, ending a `ESC [ ? ... c` CSI
    /// sequence) arrived with no valid OSC 11 response ahead of it: this
    /// terminal answered the "are you there" probe but not the color query,
    /// so it doesn't support OSC 11 — stop waiting rather than hang for the
    /// unsupporting-terminal's sake.
    Unsupported,
    /// Neither has arrived (in full) yet — keep reading, subject to the
    /// caller's own deadline.
    Pending,
}

/// Scans the bytes accumulated so far for either terminal reply. OSC 11 is
/// checked first: if it is present and parses, that wins outright regardless
/// of whether a DA1 reply is *also* in the buffer (a terminal that answers
/// both, in either order, still resolves correctly). The DA1 detector looks
/// for `ESC [` followed somewhere by a `c` — good enough to recognize "this
/// terminal answered a CSI query" without a full CSI-sequence parser, and
/// safe to conflate with any other CSI reply landing first: any answer at
/// all to the DA1 probe is equally good evidence "no OSC 11 is coming."
fn scan(buf: &[u8]) -> ScanOutcome {
    let text = String::from_utf8_lossy(buf);
    if let Some(pos) = text.find("\x1b]11;rgb:")
        && let Some(rgb) = parse_osc11_response(&text[pos..])
    {
        return ScanOutcome::Resolved(classify_luminance(rgb));
    }
    if let Some(csi) = text.find("\x1b[")
        && text[csi..].contains('c')
    {
        return ScanOutcome::Unsupported;
    }
    ScanOutcome::Pending
}

/// Sends the OSC 11 + DA1 query pair to `writer`, then reads from `reader`
/// until [`scan`] resolves one way or the other or `timeout` elapses since
/// the queries were sent — `None` on a timeout, an `Unsupported` verdict, an
/// I/O error, or EOF, every one of which means the same thing to the caller:
/// don't switch, keep whatever theme was already configured.
///
/// The deadline is only checked *between* calls to `reader.read`, so this
/// function alone cannot bound a `read` that blocks forever on its own —
/// that per-call read must itself be non-blocking or otherwise bounded for
/// the overall timeout to hold. That is deliberately not this function's
/// job: it is what makes `resolve_via_io` directly testable against a plain
/// in-memory mock (below) with no real time elapsing, while the real
/// `io::stdin()` case (`main::query_terminal_bg`) supplies the outer
/// wall-clock guarantee itself via a spawned thread.
pub fn resolve_via_io<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    timeout: Duration,
) -> Option<BgMode> {
    writer.write_all(OSC11_QUERY.as_bytes()).ok()?;
    writer.write_all(DA1_QUERY.as_bytes()).ok()?;
    writer.flush().ok()?;

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        match scan(&buf) {
            ScanOutcome::Resolved(mode) => return Some(mode),
            ScanOutcome::Unsupported => return None,
            ScanOutcome::Pending => {}
        }
        if Instant::now() >= deadline {
            return None;
        }
        match reader.read(&mut chunk) {
            Ok(0) => return None, // EOF: nothing more will ever arrive.
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_a_well_formed_bel_terminated_response() {
        // A dark terminal background (near-black).
        let raw = "\x1b]11;rgb:0000/0000/0000\x07";
        assert_eq!(parse_osc11_response(raw), Some((0x00, 0x00, 0x00)));
    }

    #[test]
    fn parses_a_well_formed_st_terminated_response() {
        // A light terminal background (near-white), ST-terminated instead
        // of BEL-terminated — xterm supports both.
        let raw = "\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        assert_eq!(parse_osc11_response(raw), Some((0xff, 0xff, 0xff)));
    }

    #[test]
    fn parses_paper_themes_own_cream_background() {
        // PRD Appendix C's `paper` bg (#f5f0e1) — round-tripped through the
        // same 4-hex-digit-per-channel convention a real terminal would use.
        let raw = "\x1b]11;rgb:f5f5/f0f0/e1e1\x07";
        assert_eq!(parse_osc11_response(raw), Some((0xf5, 0xf0, 0xe1)));
    }

    #[test]
    fn malformed_responses_never_panic_and_return_none() {
        for bad in [
            "",
            "not an escape sequence at all",
            "\x1b]11;rgb:garbage\x07",
            "\x1b]11;rgb:ffff/ffff\x07",           // missing a channel
            "\x1b]11;rgb:ffff/ffff/ffff/ffff\x07", // extra channel
            "\x1b]11;rgb:zz/zz/zz\x07",            // non-hex
            "\x1b]12;rgb:ffff/ffff/ffff\x07",      // OSC 12 (cursor color), not 11
        ] {
            assert_eq!(parse_osc11_response(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn classifies_known_light_and_dark_backgrounds() {
        assert_eq!(classify_luminance((0xff, 0xff, 0xff)), BgMode::Light);
        assert_eq!(classify_luminance((0x00, 0x00, 0x00)), BgMode::Dark);
        // PRD Appendix C's `paper` bg — must classify light.
        assert_eq!(classify_luminance((0xf5, 0xf0, 0xe1)), BgMode::Light);
        // A typical dark terminal default.
        assert_eq!(classify_luminance((0x10, 0x14, 0x18)), BgMode::Dark);
    }

    /// A mock `Read` that serves a canned OSC 11 reply, simulating an
    /// OSC-11-capable terminal. `resolve_via_io` takes its reader and writer
    /// as two separate parameters (the real `io::stdin()`/`io::stdout()`
    /// case is never one object either), so the query bytes this test writes
    /// go to a throwaway `Cursor` sink below — only the read side, this
    /// mock, is what's under test.
    struct RespondingTerminal {
        reply: Vec<u8>,
    }

    impl Read for RespondingTerminal {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.reply.is_empty() {
                return Ok(0); // EOF: nothing (more) to deliver.
            }
            let n = self.reply.len().min(buf.len());
            buf[..n].copy_from_slice(&self.reply[..n]);
            self.reply.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn resolve_via_io_classifies_a_responding_terminals_dark_background() {
        let mut term = RespondingTerminal {
            reply: b"\x1b]11;rgb:1010/1414/1818\x1b\\".to_vec(),
        };
        let mut sink = Cursor::new(Vec::new());
        let outcome = resolve_via_io(&mut term, &mut sink, Duration::from_millis(50));
        assert_eq!(outcome, Some(BgMode::Dark));
    }

    #[test]
    fn resolve_via_io_gives_up_on_a_silent_terminal_without_hanging() {
        // Never replies at all (EOF immediately) — must resolve to `None`
        // promptly rather than blocking until the timeout wall-clock
        // deadline, since `Ok(0)` is treated as "nothing more is coming."
        let mut reader = Cursor::new(Vec::<u8>::new());
        let mut writer = Cursor::new(Vec::new());
        let start = Instant::now();
        let outcome = resolve_via_io(&mut reader, &mut writer, Duration::from_millis(200));
        assert_eq!(outcome, None);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "an EOF reader must not wait out the full timeout"
        );
    }

    #[test]
    fn resolve_via_io_recognizes_a_da1_only_reply_as_unsupported() {
        // A terminal that answers DA1 (e.g. `ESC [ ? 6 c`) but not OSC 11 —
        // the documented "guarded by DA1" unsupported path.
        let mut reader = Cursor::new(b"\x1b[?6c".to_vec());
        let mut writer = Cursor::new(Vec::new());
        let outcome = resolve_via_io(&mut reader, &mut writer, Duration::from_millis(50));
        assert_eq!(outcome, None);
    }

    #[test]
    fn resolve_via_io_sends_both_queries() {
        let mut reader = Cursor::new(Vec::<u8>::new());
        let mut writer = Cursor::new(Vec::new());
        resolve_via_io(&mut reader, &mut writer, Duration::from_millis(10));
        let sent = writer.into_inner();
        let sent = String::from_utf8(sent).unwrap();
        assert!(sent.contains(OSC11_QUERY), "must send the OSC 11 query");
        assert!(sent.contains(DA1_QUERY), "must send the DA1 guard query");
    }

    /// PRD FR-TH-4's live-switching notification and a query reply share
    /// exactly one parser — a push notification is just an unprompted
    /// instance of the same grammar.
    #[test]
    fn a_dec_2031_push_notification_parses_with_the_same_function() {
        let push = "\x1b]11;rgb:0000/0000/0000\x07";
        assert_eq!(parse_osc11_response(push), Some((0, 0, 0)));
    }

    /// DEC mode 2031's exact escape framing, locked so a future edit can't
    /// silently corrupt it even though nothing sends the enable half yet
    /// (see that constant's own doc comment for why).
    #[test]
    fn dec_2031_sequences_have_the_documented_framing() {
        assert_eq!(ENABLE_COLOR_SCHEME_NOTIFICATIONS, "\x1b[?2031h");
        assert_eq!(DISABLE_COLOR_SCHEME_NOTIFICATIONS, "\x1b[?2031l");
    }
}
