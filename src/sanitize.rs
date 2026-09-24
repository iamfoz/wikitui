//! SEC-1 content sanitizer: the single choke point every remote-derived
//! string passes through before it can reach a `ratatui::Span`, stdout
//! (`--dump`), the OSC 52 clipboard payload, or a `cite.rs` export.
//! `ratatui` escapes nothing of its own accord — a raw ESC or C1 byte
//! embedded in a `Span`'s text reaches the terminal's control-sequence
//! parser verbatim (PRD §6.6 SEC-1, R-12) — so this module is the only
//! thing standing between attacker-influenced article/search text and the
//! terminal.
//!
//! Callers (`doc.rs`, `api.rs`, `target.rs`) each apply this at their own
//! single ingestion boundary rather than scattering calls through their
//! rendering code — see each module's `sanitize_document`/equivalent doc
//! comment for exactly which fields are covered.
//!
//! Two entry points, chosen by whether the field is conceptually one
//! line or may legitimately contain a line break:
//!
//! - [`sanitize_text`] keeps `\n`/`\t`: paragraph/list/blockquote span text
//!   can carry an intentional `\n` for a source `<br>`, and code-block text
//!   needs real line breaks to stay readable.
//! - [`sanitize_single_line`] strips `\n`/`\t` like any other unwanted
//!   control character: titles, table/infobox cells, image alt text,
//!   citation text/urls, and search/typeahead fields are all rendered as a
//!   single line, so a raw newline there is anomalous input, not a
//!   legitimate line break.
//!
//! What is stripped, and why:
//!
//! - **C0 controls** (`0x00..=0x1F`) other than the caller's allowed set,
//!   and **DEL** (`0x7F`): the byte range terminals act on as control codes
//!   (bell, backspace, cursor motion) even outside of a longer escape
//!   sequence.
//! - **ESC** (`0x1B`) is itself a C0 control and is called out separately
//!   in the PRD because it is the introducer for every 7-bit escape
//!   sequence (CSI/OSC/DCS/APC/PM). It falls out of the C0 rule above.
//! - **C1 controls** (`U+0080..=U+009F`): the 8-bit encodings of the same
//!   sequence introducers (CSI `0x9B`, OSC `0x9D`, DCS `0x90`, APC `0x9F`,
//!   PM `0x9E`, ST `0x9C`) — some terminals honor these without a leading
//!   ESC at all.
//!
//!   Together, stripping every C0 control and every C1 control byte removes
//!   every possible sequence *introducer*: nothing that survives
//!   sanitization can begin a control sequence on the terminal, 7-bit or
//!   8-bit. The parameter/text bytes that used to *follow* an introducer
//!   (e.g. the `0;31m` of a stripped CSI color change, or the `0;pwned` of
//!   a stripped OSC title-set) are ordinary printable characters and are
//!   left alone — they render as inert, if ugly, text instead of a live
//!   sequence.
//! - **Bidi override/isolate characters** (`U+202A..=U+202E`,
//!   `U+2066..=U+2069`): LRE/RLE/LRO/RLO/PDF and LRI/RLI/FSI/PDI can make
//!   rendered text lie about its own reading order — e.g. disguising a
//!   malicious link target as something else — independent of genuine RTL
//!   script rendering (which needs none of these; full bidi is out of
//!   scope per PRD FR-ML-7).
//! - **Dead zero-width characters**: ZWSP `U+200B`, WORD JOINER `U+2060`,
//!   and BOM/ZWNBSP `U+FEFF` do no useful work in a terminal cell grid and
//!   are stripped. **ZWJ `U+200D` is deliberately kept** — emoji ZWJ
//!   sequences (e.g. family/profession emoji) need it to form a single
//!   grapheme cluster (`layout.rs` measures width per grapheme cluster, so
//!   this is exactly where it would otherwise break). Combining marks
//!   (general categories Mn/Mc/Me) are untouched — IPA transcriptions and
//!   other diacritics depend on them (PRD FR-RD-10).
//! - **Unicode line/paragraph separators** (`U+2028`, `U+2029`): not
//!   control characters, but line-breaking characters most terminals don't
//!   expect outside of `\n`. Mapped to a space (never dropped outright) so
//!   two words on either side don't get jammed together.
//!
//! Cost model: a clean-input fast path (the overwhelming majority of real
//! article text) returns `Cow::Borrowed` with no allocation at all.

use std::borrow::Cow;

/// PRD SEC-3: bounds the layout/wrap cost of a single pathological span
/// (one giant, unbroken text node) independent of the whole-document
/// [`crate::doc::MAX_ARTICLE_HTML_BYTES`] cap — a hostile page could still
/// pack an enormous single text node inside that budget. Applied uniformly
/// to every string this module's callers cap.
pub const MAX_SPAN_CHARS: usize = 20_000;

fn is_c0_or_del(c: char) -> bool {
    (c as u32) < 0x20 || c == '\u{7F}'
}

fn is_c1(c: char) -> bool {
    matches!(c as u32, 0x80..=0x9F)
}

fn is_bidi_override_or_isolate(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Zero-width characters that do no useful rendering work in a terminal
/// cell grid. ZWJ (`U+200D`) is intentionally excluded — see module docs.
fn is_dead_zero_width(c: char) -> bool {
    matches!(c, '\u{200B}' | '\u{2060}' | '\u{FEFF}')
}

fn is_unicode_line_or_paragraph_separator(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}')
}

/// `true` if `c` must be removed or replaced by [`sanitize_with`] under the
/// given multi-line policy (`keep_newline_tab`: whether `\n`/`\t` are exempt
/// from the C0 strip).
fn is_dirty(c: char, keep_newline_tab: bool) -> bool {
    if keep_newline_tab && (c == '\n' || c == '\t') {
        return false;
    }
    is_c0_or_del(c)
        || is_c1(c)
        || is_bidi_override_or_isolate(c)
        || is_dead_zero_width(c)
        || is_unicode_line_or_paragraph_separator(c)
}

fn sanitize_with(s: &str, keep_newline_tab: bool) -> Cow<'_, str> {
    if !s.chars().any(|c| is_dirty(c, keep_newline_tab)) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if keep_newline_tab && (c == '\n' || c == '\t') {
            out.push(c);
        } else if is_unicode_line_or_paragraph_separator(c) {
            out.push(' ');
        } else if is_c0_or_del(c)
            || is_c1(c)
            || is_bidi_override_or_isolate(c)
            || is_dead_zero_width(c)
        {
            // Dropped: see the module doc comment for why each category is
            // unconditionally removed rather than escaped or replaced.
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Multi-line-safe sanitizer: keeps `\n`/`\t`. Use for paragraph/heading/
/// list/blockquote span text and code-block text, where a line break can be
/// legitimate content.
pub fn sanitize_text(s: &str) -> Cow<'_, str> {
    sanitize_with(s, true)
}

/// Single-line sanitizer: `\n`/`\t` are treated like any other C0 control
/// and stripped outright (not replaced with a space) — a raw newline in a
/// title, table cell, alt text, or citation field is anomalous input, never
/// legitimate content, so there is no word-joining concern to guard against
/// the way there is for genuine prose.
pub fn sanitize_single_line(s: &str) -> Cow<'_, str> {
    sanitize_with(s, false)
}

/// Truncates `s` to at most `max_chars` characters (on a char boundary,
/// never splitting a multi-byte or combined grapheme), appending a marker
/// so a truncated field doesn't silently read as complete. A no-op
/// (borrowed, no allocation) when `s` is already within budget.
pub fn cap_len(s: &str, max_chars: usize) -> Cow<'_, str> {
    if s.chars().count() <= max_chars {
        return Cow::Borrowed(s);
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("…[truncated]");
    Cow::Owned(out)
}

/// Sanitize (multi-line-safe) then cap length in one call — the common case
/// for span/code text.
pub fn sanitize_and_cap_multiline(s: &str, max_chars: usize) -> String {
    cap_len(&sanitize_text(s), max_chars).into_owned()
}

/// Sanitize (single-line) then cap length in one call — the common case for
/// titles, table/infobox cells, alt text, citations, and search/typeahead
/// fields.
pub fn sanitize_and_cap_single_line(s: &str, max_chars: usize) -> String {
    cap_len(&sanitize_single_line(s), max_chars).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_ascii_text_is_borrowed_not_allocated() {
        let s = "Alan Turing was a mathematician.";
        assert!(matches!(sanitize_text(s), Cow::Borrowed(_)));
        assert!(matches!(sanitize_single_line(s), Cow::Borrowed(_)));
    }

    #[test]
    fn strips_esc_and_csi_introducer_leaving_inert_text() {
        // A CSI color-change sequence: ESC '[' '3' '1' 'm'. Only the ESC
        // (the introducer) is a control character; '[','3','1','m' are
        // ordinary printable ASCII and survive as harmless text.
        let hostile = "before\x1b[31mafter";
        let clean = sanitize_text(hostile);
        assert!(!clean.contains('\x1b'), "{clean:?}");
        assert_eq!(clean, "before[31mafter");
    }

    #[test]
    fn strips_osc_title_set_sequence_introducer_and_terminator() {
        // ESC ] 0 ; pwned BEL — a classic OSC window-title-set attempt.
        // ESC and BEL (a C0 control) both get stripped; the ASCII payload
        // in between is left as inert text.
        let hostile = "\x1b]0;pwned\x07Article Title";
        let clean = sanitize_single_line(hostile);
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\x07'));
        assert_eq!(clean, "]0;pwnedArticle Title");
    }

    #[test]
    fn strips_c1_control_bytes() {
        // U+009B is the 8-bit CSI introducer.
        let hostile = "before\u{9B}31mafter";
        let clean = sanitize_text(hostile);
        assert!(!clean.chars().any(|c| (0x80..=0x9F).contains(&(c as u32))));
        assert_eq!(clean, "before31mafter");
    }

    #[test]
    fn strips_del_byte() {
        let hostile = "abc\u{7F}def";
        assert_eq!(sanitize_text(hostile), "abcdef");
    }

    #[test]
    fn strips_bidi_overrides() {
        // RLO around "evil" would make a terminal display it reversed.
        let hostile = "safe\u{202E}evil\u{202C}tail";
        let clean = sanitize_text(hostile);
        assert!(!clean.contains('\u{202E}'));
        assert!(!clean.contains('\u{202C}'));
        assert_eq!(clean, "safeeviltail");
    }

    #[test]
    fn strips_isolate_characters() {
        let hostile = "\u{2066}isolated\u{2069}";
        assert_eq!(sanitize_text(hostile), "isolated");
    }

    #[test]
    fn strips_zwsp_word_joiner_and_bom_but_keeps_zwj_emoji_intact() {
        // Family emoji: man + ZWJ + woman + ZWJ + girl. This whole run must
        // survive byte-for-byte — layout.rs measures width per grapheme
        // cluster, and breaking the ZWJ would split one visual glyph into
        // several mismatched ones.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(sanitize_text(family), family, "ZWJ emoji must be intact");

        let hostile = "a\u{200B}b\u{2060}c\u{FEFF}d";
        assert_eq!(
            sanitize_text(hostile),
            "abcd",
            "ZWSP/WORD JOINER/BOM must be stripped"
        );
    }

    #[test]
    fn keeps_combining_marks_for_ipa_and_diacritics() {
        // "n" + combining tilde (ñ built from combining marks) and an IPA
        // transcription with combining marks must render unchanged.
        let combining = "n\u{0303} \u{0283}\u{0361}\u{0288}"; // combining tilde; tie bars
        assert_eq!(sanitize_text(combining), combining);
    }

    #[test]
    fn maps_unicode_line_and_paragraph_separators_to_space() {
        assert_eq!(sanitize_text("a\u{2028}b"), "a b");
        assert_eq!(sanitize_text("a\u{2029}b"), "a b");
    }

    #[test]
    fn multiline_variant_keeps_newline_and_tab() {
        let s = "line one\nline\ttwo";
        assert!(matches!(sanitize_text(s), Cow::Borrowed(_)));
        assert_eq!(sanitize_text(s), s);
    }

    #[test]
    fn single_line_variant_strips_newline_and_tab() {
        let s = "line one\nline\ttwo";
        assert_eq!(sanitize_single_line(s), "line onelinetwo");
    }

    #[test]
    fn cap_len_is_a_no_op_under_budget() {
        let s = "short";
        assert!(matches!(cap_len(s, 100), Cow::Borrowed(_)));
    }

    #[test]
    fn cap_len_truncates_on_a_char_boundary_with_a_marker() {
        let s = "é".repeat(10); // 2 bytes each, so byte-slicing blindly would panic
        let capped = cap_len(&s, 3);
        assert!(capped.starts_with(&"é".repeat(3)));
        assert!(capped.ends_with("[truncated]"));
    }

    #[test]
    fn sanitize_and_cap_helpers_compose_both_steps() {
        let hostile = format!("\x1b[31m{}", "x".repeat(30));
        let capped = sanitize_and_cap_single_line(&hostile, 10);
        assert!(!capped.contains('\x1b'));
        assert!(capped.ends_with("[truncated]"));
    }
}
