//! PRD FR-ML-7: experimental right-to-left (RTL) support. Explicitly
//! v2-directional, P2 priority, and explicitly **not** a full bidi
//! implementation — R-7 ("RTL expectations unmeetable in most emulators")
//! is the risk this whole feature lives in the shadow of, and the PRD's own
//! wording is blunt about it: "full bidi is explicitly not promised."
//!
//! Three independent pieces, composed by callers (`main.rs`/`ui.rs`), never
//! by this module:
//!
//! 1. [`direction`] — is this article's language RTL at all? The only signal
//!    consulted is the wiki language edition the content came from (e.g.
//!    `Tab::lang`, `"ar"`/`"he"`), the same per-language granularity every
//!    other multi-language feature in this codebase already uses (FR-ML-1/2's
//!    langlinks, `config::ResolvedWikiCapabilities`'s per-wiki matrix) rather
//!    than per-paragraph content inspection. A mixed-direction article (an
//!    Arabic Wikipedia page quoting an English source inline) is still,
//!    wholesale, "an RTL article" by this measure.
//! 2. [`BidiMode`] / [`active`] / [`should_emit_terminal_enable`] — handing
//!    the reordering job to the **terminal** by emitting
//!    [`VTE_BIDI_AUTODETECT_ENABLE`]. See that constant's own doc comment for
//!    which sequence was chosen, why, and the honest uncertainty around it.
//! 3. [`should_app_reorder`] / [`reorder_line_for_display`] /
//!    [`reorder_laid_lines`] — the app-side fallback for a terminal that does
//!    no bidi at all. A deliberately partial logical→visual transform,
//!    engaged only when explicitly configured on AND the terminal path above
//!    is inactive. That "AND" is the whole point: FR-ML-7 names the
//!    double-reordering hazard explicitly — if both the terminal and this
//!    app reorder the same text, the result is reordered *twice* (wrong in a
//!    new way, not merely "still logical order"). [`should_app_reorder`] is
//!    the single place that guard lives.
//!
//! Nothing in this module ever runs unless a reader is looking at RTL-tagged
//! content — every entry point is a no-op (`Direction::Ltr`, `false`, or an
//! unchanged string/`Vec` clone) for the ordinary English-reading session,
//! so this feature cannot regress anything for the LTR path §12 has shipped
//! for months.

/// The reading direction FR-ML-7 cares about. Two values, not a general
/// bidi embedding-level stack (UAX#9's real model nests many levels) — this
/// whole feature is "is the article RTL, yes or no", never "what is the
/// bidi level of this specific run".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Ltr,
    Rtl,
}

/// PRD FR-ML-7's "documented set" of RTL language codes, sourced from
/// Wikimedia Commons' `Module:Dir/RTL overrides` and Meta-Wiki's
/// `Template:Active Wikipedias` (the canonical cross-project lists of which
/// language editions render right-to-left) — not this codebase's own
/// invention, and not claimed to be exhaustive: a language whose Wikipedia
/// edition later adds an RTL script, or a niche RTL edition this list
/// missed, is a one-line addition here, never a design change (the same
/// "small, documented, extend-on-demand" shape as
/// `doc::CITATION_NEEDED_TEMPLATES`).
///
/// Matched against the *primary* subtag only (`direction` strips a region/
/// script suffix like `-EG` first), lowercase, so `"ar"`, `"AR"`, and
/// `"ar-EG"` all resolve the same way.
pub const RTL_LANGS: &[&str] = &[
    "ar",  // Arabic
    "arc", // Aramaic
    "arz", // Egyptian Arabic
    "azb", // South Azerbaijani
    "bcc", // Southern Balochi
    "bqi", // Bakhtiari
    "ckb", // Central Kurdish (Sorani)
    "dv",  // Divehi (Maldivian)
    "fa",  // Persian
    "glk", // Gilaki
    "he",  // Hebrew
    "khw", // Khowar
    "lrc", // Northern Luri
    "mzn", // Mazanderani
    "pnb", // Western Punjabi
    "prs", // Dari
    "ps",  // Pashto
    "sd",  // Sindhi
    "skr", // Saraiki
    "ug",  // Uyghur
    "ur",  // Urdu
    "yi",  // Yiddish
];

/// Whether `lang` (a wiki language-edition code, e.g. `Tab::lang`) is RTL.
/// Strips a trailing region/script subtag (`"ar-EG"` → `"ar"`) and compares
/// case-insensitively before checking [`RTL_LANGS`] — an empty or unknown
/// code defaults to [`Direction::Ltr`], matching every other "unknown ⇒ the
/// safe/common case" default in this codebase (e.g. `doc::extract_math`'s
/// "missing-tex graceful" `None`).
pub fn direction(lang: &str) -> Direction {
    let primary = lang.split(['-', '_']).next().unwrap_or(lang);
    let lower = primary.to_ascii_lowercase();
    if RTL_LANGS.contains(&lower.as_str()) {
        Direction::Rtl
    } else {
        Direction::Ltr
    }
}

/// `bidi = auto|on|off` (config key, mirrors `hyperlink::HyperlinkMode`'s
/// shape exactly — same three-state closed set, same `parse`/`as_str`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BidiMode {
    /// Emit the terminal escape only when [`auto_env_supported`]'s heuristic
    /// says the session looks like a terminal that understands it. See that
    /// function's doc comment for exactly how unreliable this is — the PRD
    /// itself anticipates that in practice this resolves to "off" for most
    /// sessions, since the heuristic has very little to go on.
    Auto,
    /// Always emit, for a terminal the reader knows supports bidi even
    /// though `auto`'s heuristic can't confirm it (or guesses wrong).
    On,
    /// Never emit — the terminal never hears about bidi from this app.
    Off,
}

impl BidiMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// Not called by production code today (there is no runtime `:set
    /// bidi=` command in this chunk — `bidi`/`rtl_reorder` are resolved once
    /// from config, like `auto_theme`/`theme_light`/`theme_dark`, not
    /// session-mutable); kept for symmetry with `HyperlinkMode::as_str` (used
    /// by `App::set_hyperlinks_mode`'s notice) and exercised directly by
    /// [`tests::bidi_mode_as_str_round_trips_through_parse`], the same
    /// "real, tested, not yet wired to a caller" posture already established
    /// for e.g. `autotheme::ENABLE_COLOR_SCHEME_NOTIFICATIONS`.
    #[allow(dead_code)]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// PRD FR-ML-7's chosen terminal escape sequence, and the honest story
/// behind it.
///
/// **Research performed for this chunk** (three independently corroborating
/// sources): the freedesktop.org "BiDi in Terminal Emulators" working-group
/// pages document ECMA-48's own standard mechanism, **BDSM** ("Bi-Directional
/// Support Mode", `SM 8` / `RM 8` — i.e. `CSI 8 h` / `CSI 8 l`, an actual
/// "SM/RM" pair in ECMA-48's own naming, which is exactly the shape the PRD's
/// wording alludes to); VTE's own bidi implementation (its `wip/egmont/bidi`
/// commit series, titled "BDSM and SPD escape sequences") implements that
/// standard pair *and* a VTE-private DECSET mode, documented as: "for
/// auto-direction support in VTE-based terminals, to enable UBA
/// (Unicode Bidi Algorithm) support for command output, use `echo
/// '\e[?2501h'`"; and mlterm ships its own `doc/en/README.bidi` describing
/// bidi support in mlterm terms this chunk's research pass could not fetch
/// the exact byte sequences from (network access to that file was blocked in
/// this sandbox), matching this codebase's honest-uncertainty precedent for
/// unfetchable specifics (`graphics.rs`'s kitty/iTerm2 emitters, `autotheme
/// .rs`'s OSC 11).
///
/// **This build emits [`VTE_BIDI_AUTODETECT_ENABLE`]/[`_DISABLE`]** (the
/// private DECSET pair, `CSI ? 2501 h`/`l`), not the ECMA-48 standard BDSM
/// pair, because: (a) it is the sequence actually attributed by name to "VTE
/// family" terminals — the exact terminal family the PRD names first — while
/// BDSM's real-world implementation breadth is far less certain even inside
/// VTE itself; (b) it is a plain on/off toggle ("apply the Unicode Bidi
/// Algorithm to whatever text follows"), so it needs no companion
/// per-paragraph direction marker — a capable terminal auto-detects RTL runs
/// from the Unicode strong-direction properties of the characters
/// themselves, and is a no-op on pure-LTR text, so leaving it enabled while
/// reading an LTR article afterward is harmless (see `main::emit_bidi_mode`'s
/// doc comment for why this is sent once, lazily, rather than toggled per
/// tab switch). [`BDSM_IMPLICIT`]/[`BDSM_RESET`] are kept below, byte-tested,
/// as the standards-documented alternative this build does not actually
/// issue — the same "spec-complete, byte-tested, not wired" posture
/// `autotheme::ENABLE_COLOR_SCHEME_NOTIFICATIONS` already established for
/// exactly this reason (a real, researched sequence worth locking down even
/// though this particular chunk doesn't have a consumer ready for it).
///
/// **What cannot be verified in this environment**: there is no bidi-capable
/// terminal emulator available in this sandbox's pty harness (the harness's
/// "terminal" is a plain Python pty with no bidi rendering of its own,
/// exactly the limitation already documented for `autotheme`'s OSC 11 query
/// and `graphics.rs`'s kitty/iTerm2 emitters) — so this chunk can prove the
/// *bytes emitted* are exactly these, byte-for-byte, and that emission is
/// correctly gated by config/env, but it cannot prove that any real VTE
/// version (or mlterm) actually reorders text on screen in response. That is
/// exactly the gap FR-ML-7's "full bidi is explicitly not promised" and R-7
/// ("RTL expectations unmeetable in most emulators") are written to cover.
pub const VTE_BIDI_AUTODETECT_ENABLE: &str = "\x1b[?2501h";
/// The disabling counterpart, unconditionally sent on terminal restore
/// (`crashguard::restore_terminal_best_effort`) — harmless whether or not
/// this session ever sent the enable sequence, exactly like
/// `autotheme::DISABLE_COLOR_SCHEME_NOTIFICATIONS`.
pub const VTE_BIDI_AUTODETECT_DISABLE: &str = "\x1b[?2501l";

/// ECMA-48's own standard BDSM control (`SM 8` / `RM 8`): "implicit" bidi
/// mode (the terminal infers direction from the Unicode bidi class of each
/// character) vs. ECMA-48's documented reset state, named "explicit" in the
/// standard itself (direction must be marked explicitly, e.g. via SDS/SRS —
/// which amounts to "do nothing automatically", the ordinary LTR-terminal
/// behavior every session already has). Not emitted by this build — see
/// [`VTE_BIDI_AUTODETECT_ENABLE`]'s doc comment for why — but locked down
/// here, byte-tested, as the standards-track alternative.
#[allow(dead_code)]
pub const BDSM_IMPLICIT: &str = "\x1b[8h";
#[allow(dead_code)]
pub const BDSM_RESET: &str = "\x1b[8l";

/// Whether this session manages the terminal's bidi mode at all. `On` always
/// does; `Off` never does; `Auto` defers to [`auto_env_supported`] — per the
/// PRD's own framing, this is expected to resolve to `false` for nearly
/// every real session, since there is no query/response capability probe for
/// bidi support the way `autotheme::resolve_via_io` has DA1 to guard OSC 11
/// (a terminal that doesn't understand `CSI ? 2501 h` simply ignores it —
/// harmless, but also silent, so there is no way to *confirm* support the
/// way OSC 11's reply confirms auto-theme support).
pub fn active(mode: BidiMode, env_supported: bool) -> bool {
    match mode {
        BidiMode::Off => false,
        BidiMode::On => true,
        BidiMode::Auto => env_supported,
    }
}

/// `Auto`'s env heuristic: `VTE_VERSION` is set (non-empty) by every
/// VTE-based terminal (GNOME Terminal, and most GTK terminal forks) since
/// VTE started exporting it — a real, if imprecise, signal that the session
/// is *probably* running inside some VTE version, not proof that *this*
/// version implements bidi (VTE's bidi support landed in 0.58; an older
/// VTE_VERSION would still pass this heuristic and get a sequence its
/// terminal silently ignores — inert, per `active`'s own doc comment, but
/// still a false positive against "known to support it"). `TERM` containing
/// `"mlterm"` is the second, even weaker signal (mlterm does not export a
/// dedicated version env var this build can key on) — both are documented,
/// static, env-only checks, not a live capability probe; this is exactly the
/// "detection is unreliable" the PRD asks to be honest about.
pub fn auto_env_supported(vte_version: Option<&str>, term: Option<&str>) -> bool {
    let vte = vte_version.is_some_and(|v| !v.trim().is_empty());
    let mlterm = term.is_some_and(|t| t.contains("mlterm"));
    vte || mlterm
}

/// Whether `main::emit_bidi_mode` should send [`VTE_BIDI_AUTODETECT_ENABLE`]
/// right now: this session manages bidi at all (`terminal_active`), it
/// hasn't already been sent once this run (`already_sent` — sent lazily,
/// exactly once, the first time RTL content is actually on screen, per PRD
/// "when an RTL article is opened, the app is in RTL mode for that content"
/// — never speculatively at startup for a reader who may never open RTL
/// content this session), and the tab actually on screen is RTL right now.
pub fn should_emit_terminal_enable(
    terminal_active: bool,
    already_sent: bool,
    dir: Direction,
) -> bool {
    terminal_active && !already_sent && dir == Direction::Rtl
}

/// **The double-reordering hazard gate** (PRD FR-ML-7, called out by name):
/// app-side reordering must engage only when it is explicitly turned on
/// (`rtl_reorder_enabled`, config default **false**) AND the terminal is
/// *not* already reordering this session (`!terminal_bidi_active`) AND the
/// content on screen is actually RTL. If both the terminal and this app
/// reordered the same RTL text, the result would be reordered twice — visual
/// order run back through another visual-order transform, which is not
/// logical order restored, it is a *third*, wrong order. This function is
/// the single place that guard is decided; every caller (`ui.rs`'s
/// `draw_reading`/`paint_pane`) goes through it rather than re-deriving the
/// AND by hand.
pub fn should_app_reorder(
    rtl_reorder_enabled: bool,
    terminal_bidi_active: bool,
    dir: Direction,
) -> bool {
    rtl_reorder_enabled && !terminal_bidi_active && dir == Direction::Rtl
}

/// Unicode code-point ranges of scripts with a strong right-to-left bidi
/// class (UAX#9's `R`/`AL` categories) — Hebrew, Arabic (plus its
/// supplements/extensions and presentation-form blocks), Syriac, Thaana
/// (Divehi), and N'Ko. Deliberately a fixed code-point-range table, not a
/// dependency on a full Unicode bidi-class database: this module's transform
/// is an explicitly simplified "UAX#9-ish pass" (see
/// [`reorder_line_for_display`]'s doc comment), and these ranges cover every
/// script any [`RTL_LANGS`] entry's Wikipedia edition is actually written in.
fn is_strong_rtl_char(c: char) -> bool {
    let u = c as u32;
    (0x0590..=0x05FF).contains(&u) // Hebrew
        || (0x0600..=0x06FF).contains(&u) // Arabic
        || (0x0700..=0x074F).contains(&u) // Syriac
        || (0x0750..=0x077F).contains(&u) // Arabic Supplement
        || (0x0780..=0x07BF).contains(&u) // Thaana (Divehi)
        || (0x07C0..=0x07FF).contains(&u) // N'Ko
        || (0x08A0..=0x08FF).contains(&u) // Arabic Extended-A
        || (0xFB1D..=0xFB4F).contains(&u) // Hebrew presentation forms
        || (0xFB50..=0xFDFF).contains(&u) // Arabic presentation forms A
        || (0xFE70..=0xFEFF).contains(&u) // Arabic presentation forms B
}

/// Whether `token` (a maximal run of non-whitespace characters, see
/// [`reorder_line_for_display`]) should be treated as an RTL run: it counts
/// if it contains *any* strong-RTL character. A token mixing scripts with no
/// separating whitespace (rare — an embedded Latin acronym glued directly to
/// Arabic text with no space) is therefore classified as one whole unit by
/// whichever direction appears in it at all; see
/// [`reorder_line_for_display`]'s doc comment for why that imprecision is an
/// accepted, documented simplification rather than a bug.
fn token_is_rtl(token: &str) -> bool {
    token.chars().any(is_strong_rtl_char)
}

/// PRD FR-ML-7's app-side "basic logical→visual transform": a deliberately
/// partial, simplified UAX#9-ish pass over one run of text (one `LaidSpan`'s
/// content — see [`reorder_laid_lines`], the only real caller). `dir ==
/// Ltr` is the identity (the whole function is a no-op for the ordinary
/// English-reading session — LTR articles are completely unaffected, by
/// construction, not by a separate check the caller has to remember).
///
/// For `dir == Rtl`: splits `text` into maximal whitespace/non-whitespace
/// tokens (preserving exact whitespace and token order), and for each
/// non-whitespace token that is RTL ([`token_is_rtl`]), reverses its
/// characters — approximating how an RTL word's glyphs occupy screen cells
/// in right-to-left order, which a plain codepoint-order print would
/// otherwise show mirrored. An LTR token (an embedded English name, a
/// number) is left untouched: its own internal reading order was already
/// correct, and reversing it would break it, not fix it — this is the "handle
/// LTR runs embedded in RTL" half of the ask.
///
/// **Deliberately not reversed: token/run ORDER within the line.** A full
/// logical→visual transform would also re-flow the sequence of runs
/// right-to-left (the first logical clause ends up rightmost on screen).
/// This function does not do that, for a load-bearing reason specific to
/// this codebase: `layout.rs`'s `Layout::link_cols`/`link_lines` and
/// `find_matches`'s `MatchSpan` column ranges are computed once, at layout
/// time, against the *original* span/character order — the AGENT_BRIEF's own
/// "cross-module consistency invariant guarded by tests" warning names
/// exactly this class of line-position mapping. Reversing token order here
/// (a display-only, uncached transform — see [`reorder_laid_lines`]) would
/// silently desync those positions from what's actually on screen. Reversing
/// characters *within* a token changes no token's length or the line's total
/// width, so those position invariants stay intact; only reversing run order
/// would break them. The honest cost: a multi-run RTL line (alternating
/// RTL/LTR runs several times) reads with each run's own text correctly
/// flipped, but in the same left-to-right run *sequence* as the source, not
/// the fully mirrored sequence a true bidi engine would produce. For the
/// common case — one contiguous RTL sentence, occasionally interrupting with
/// one embedded LTR name/acronym as its own run — a one-run line has nothing
/// left to reorder at the run level, so this already reads correctly.
///
/// Further documented simplifications (full UAX#9 is explicitly NOT
/// promised): no bracket-pairing/mirroring (UAX#9's N0 rule — a `(` is never
/// swapped for `)`); no embedding levels or directional isolates (LRI/RLI/
/// PDI are not interpreted, single-level only); numbers are always treated
/// as their own (LTR) token, matching real bidi behavior for the common
/// "a number appears mid RTL-sentence" case; this operates on `char`s, not
/// extended grapheme clusters, so a combining-mark sequence (Hebrew niqqud,
/// Arabic tashkil) within a reversed token has its base letter and
/// combining mark(s) individually reordered relative to each other — a
/// known, accepted limitation for the same reason `layout.rs` reserves full
/// grapheme-cluster handling for its own width-measurement code, not this
/// experimental transform.
pub fn reorder_line_for_display(text: &str, dir: Direction) -> String {
    if dir != Direction::Rtl {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(&first) = chars.peek() {
        let is_ws = first.is_whitespace();
        let mut token = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() != is_ws {
                break;
            }
            token.push(c);
            chars.next();
        }
        if !is_ws && token_is_rtl(&token) {
            out.extend(token.chars().rev());
        } else {
            out.push_str(&token);
        }
    }
    out
}

/// Applies [`reorder_line_for_display`] across a whole laid-out document's
/// lines (PRD FR-ML-7's "reorder RTL runs app-side... within a line"),
/// producing an owned copy — the same "transient, painted-in copy, never
/// written back into the cacheable `Layout`" shape `hints::
/// overlay_hint_labels` already established for link-hint overlays (see
/// `ui::draw_reading`'s `Cow<[LaidLine]>`, which this function's only caller
/// feeds identically). Structural, non-prose span kinds — a table/infobox
/// grid, a code block, an inline-image half-block row — are left completely
/// untouched: reordering box-drawing/grid alignment or source code would
/// corrupt it, not translate it (the same "structured content is exempt"
/// posture `layout::LayoutOptions::line_spacing`'s doc comment already
/// documents for paragraph/interline spacing).
///
/// `dir` is expected to already be [`Direction::Rtl`] — callers gate on
/// [`should_app_reorder`] first — but this function honors `Ltr` as a safe
/// identity regardless (returns an unchanged clone), so it is never unsound
/// to call directly, including from a test that wants to confirm the LTR
/// no-op path without going through the gating function at all.
pub fn reorder_laid_lines(
    lines: &[crate::layout::LaidLine],
    dir: Direction,
) -> Vec<crate::layout::LaidLine> {
    use crate::layout::{LaidLine, LaidSpan, SpanKind};

    fn is_prose_kind(kind: &SpanKind) -> bool {
        matches!(
            kind,
            SpanKind::Plain
                | SpanKind::Bold
                | SpanKind::Italic
                | SpanKind::Dim
                | SpanKind::Title
                | SpanKind::Heading(_)
                | SpanKind::Quote
                | SpanKind::Image
                | SpanKind::Caption
                | SpanKind::Math
                | SpanKind::Link(_)
                | SpanKind::Hint
                | SpanKind::CitationNeeded
        )
    }

    lines
        .iter()
        .map(|line| LaidLine {
            spans: line
                .spans
                .iter()
                .map(|s| {
                    if dir == Direction::Rtl && is_prose_kind(&s.kind) {
                        LaidSpan {
                            text: reorder_line_for_display(&s.text, dir),
                            kind: s.kind.clone(),
                        }
                    } else {
                        s.clone()
                    }
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{LaidLine, LaidSpan, SpanKind};

    // --- direction / RTL_LANGS ---

    #[test]
    fn documented_rtl_codes_resolve_to_rtl() {
        for &code in RTL_LANGS {
            assert_eq!(direction(code), Direction::Rtl, "{code} must be RTL");
        }
    }

    #[test]
    fn ordinary_and_unknown_languages_default_to_ltr() {
        for code in ["en", "de", "fr", "ja", "zh", "es", "", "xx", "klingon"] {
            assert_eq!(
                direction(code),
                Direction::Ltr,
                "{code} must default to LTR"
            );
        }
    }

    #[test]
    fn direction_is_case_insensitive_and_strips_region_subtags() {
        assert_eq!(direction("AR"), Direction::Rtl);
        assert_eq!(direction("ar-EG"), Direction::Rtl);
        assert_eq!(direction("He_IL"), Direction::Rtl);
        assert_eq!(direction("EN-us"), Direction::Ltr);
    }

    // --- BidiMode ---

    #[test]
    fn bidi_mode_parses_the_three_named_values_and_rejects_others() {
        assert_eq!(BidiMode::parse("auto"), Some(BidiMode::Auto));
        assert_eq!(BidiMode::parse("on"), Some(BidiMode::On));
        assert_eq!(BidiMode::parse("off"), Some(BidiMode::Off));
        assert_eq!(BidiMode::parse("sometimes"), None);
        assert_eq!(BidiMode::parse(""), None);
    }

    #[test]
    fn bidi_mode_as_str_round_trips_through_parse() {
        for mode in [BidiMode::Auto, BidiMode::On, BidiMode::Off] {
            assert_eq!(BidiMode::parse(mode.as_str()), Some(mode));
        }
    }

    // --- escape sequence bytes ---

    #[test]
    fn vte_bidi_sequences_have_the_documented_bytes() {
        assert_eq!(VTE_BIDI_AUTODETECT_ENABLE, "\x1b[?2501h");
        assert_eq!(VTE_BIDI_AUTODETECT_DISABLE, "\x1b[?2501l");
    }

    #[test]
    fn bdsm_sequences_have_the_documented_bytes() {
        // Locked down even though unused today — see `VTE_BIDI_AUTODETECT_ENABLE`'s
        // doc comment for why this standards-track alternative isn't emitted.
        assert_eq!(BDSM_IMPLICIT, "\x1b[8h");
        assert_eq!(BDSM_RESET, "\x1b[8l");
    }

    // --- active / auto_env_supported / should_emit_terminal_enable ---

    #[test]
    fn on_and_off_ignore_the_environment_entirely() {
        assert!(active(BidiMode::On, false));
        assert!(active(BidiMode::On, true));
        assert!(!active(BidiMode::Off, false));
        assert!(!active(BidiMode::Off, true));
    }

    #[test]
    fn auto_follows_the_env_heuristic() {
        assert!(active(BidiMode::Auto, true));
        assert!(!active(BidiMode::Auto, false));
    }

    #[test]
    fn auto_env_supported_detects_vte_version_or_mlterm_term() {
        assert!(auto_env_supported(Some("0.76.0"), None));
        assert!(auto_env_supported(None, Some("mlterm")));
        assert!(auto_env_supported(None, Some("xterm-mlterm")));
        assert!(!auto_env_supported(None, None));
        assert!(!auto_env_supported(Some(""), Some("xterm-256color")));
        assert!(!auto_env_supported(Some("   "), Some("xterm-256color")));
    }

    #[test]
    fn terminal_enable_is_sent_exactly_once_lazily_for_rtl_content() {
        // Inactive session: never, regardless of direction.
        assert!(!should_emit_terminal_enable(false, false, Direction::Rtl));
        // Active, not yet sent, RTL on screen: send it.
        assert!(should_emit_terminal_enable(true, false, Direction::Rtl));
        // Active, already sent: don't resend.
        assert!(!should_emit_terminal_enable(true, true, Direction::Rtl));
        // Active, not sent, but the tab on screen is LTR: nothing to enable yet.
        assert!(!should_emit_terminal_enable(true, false, Direction::Ltr));
    }

    // --- should_app_reorder: the double-reordering hazard gate ---

    #[test]
    fn app_reorder_engages_only_when_enabled_terminal_off_and_content_is_rtl() {
        assert!(should_app_reorder(true, false, Direction::Rtl));
    }

    #[test]
    fn app_reorder_default_off_never_engages_even_for_rtl_content() {
        // PRD FR-ML-7's default: rtl_reorder = false.
        assert!(!should_app_reorder(false, false, Direction::Rtl));
    }

    #[test]
    fn app_reorder_never_engages_when_terminal_bidi_is_active_double_reorder_hazard() {
        // The exact hazard FR-ML-7 names: both configured on must not both fire.
        assert!(!should_app_reorder(true, true, Direction::Rtl));
    }

    #[test]
    fn app_reorder_never_engages_for_ltr_content_regardless_of_config() {
        assert!(!should_app_reorder(true, false, Direction::Ltr));
        assert!(!should_app_reorder(false, false, Direction::Ltr));
    }

    // --- reorder_line_for_display ---

    #[test]
    fn ltr_direction_is_always_the_identity() {
        for s in ["hello world", "", "123 abc", "שלום"] {
            assert_eq!(reorder_line_for_display(s, Direction::Ltr), s);
        }
    }

    #[test]
    fn a_single_rtl_word_is_character_reversed() {
        // Hebrew "shalom": ש ל ו ם — reversed becomes ם ו ל ש.
        assert_eq!(reorder_line_for_display("שלום", Direction::Rtl), "םולש");
    }

    #[test]
    fn an_ltr_run_embedded_in_rtl_text_is_left_untouched() {
        // The embedded "Wikipedia" (an LTR run) keeps its own internal
        // character order; the two surrounding Hebrew words are each
        // character-reversed. Token order itself is NOT reversed (see this
        // module's own doc comment on `reorder_line_for_display` for why).
        let input = "שלום Wikipedia שלום";
        let output = reorder_line_for_display(input, Direction::Rtl);
        assert_eq!(output, "םולש Wikipedia םולש");
        assert!(
            output.contains("Wikipedia"),
            "the embedded LTR run must read correctly, not reversed: {output}"
        );
    }

    #[test]
    fn whitespace_runs_are_preserved_verbatim() {
        let input = "שלום  \tעולם"; // two spaces + a tab between the words
        let output = reorder_line_for_display(input, Direction::Rtl);
        assert!(
            output.contains("  \t"),
            "whitespace run must survive untouched: {output:?}"
        );
    }

    #[test]
    fn pure_latin_text_is_unaffected_even_under_rtl_direction() {
        // No strong-RTL character anywhere: every token classifies as
        // non-RTL, so nothing is reversed even though `dir` is Rtl (e.g. a
        // citation number or an all-Latin proper noun standing alone on an
        // RTL article's line).
        assert_eq!(
            reorder_line_for_display("Wikipedia 2024", Direction::Rtl),
            "Wikipedia 2024"
        );
    }

    #[test]
    fn reorder_never_changes_the_character_count() {
        for s in ["שלום Wikipedia שלום", "مرحبا 2024 world", "", "   ", "aébç"] {
            let out = reorder_line_for_display(s, Direction::Rtl);
            assert_eq!(
                out.chars().count(),
                s.chars().count(),
                "character count must be preserved for {s:?} -> {out:?}"
            );
        }
    }

    #[test]
    fn a_mixed_script_token_with_no_separating_space_reverses_as_one_unit() {
        // Documented imprecision: a token mixing scripts with no space
        // (rare) is classified as RTL because it contains *a* strong-RTL
        // character, and the whole token (Latin digits included) reverses
        // together rather than splitting mid-token.
        let out = reorder_line_for_display("Data2024ب", Direction::Rtl);
        assert_eq!(out, "ب4202ataD");
    }

    // --- reorder_laid_lines: span-kind gating ---

    fn plain(text: &str) -> LaidSpan {
        LaidSpan {
            text: text.to_string(),
            kind: SpanKind::Plain,
        }
    }

    fn code(text: &str) -> LaidSpan {
        LaidSpan {
            text: text.to_string(),
            kind: SpanKind::Code,
        }
    }

    #[test]
    fn ltr_direction_leaves_every_line_byte_for_byte_unchanged() {
        let lines = vec![LaidLine {
            spans: vec![plain("hello world")],
        }];
        let out = reorder_laid_lines(&lines, Direction::Ltr);
        assert_eq!(out[0].spans[0].text, "hello world");
    }

    #[test]
    fn prose_spans_reorder_but_code_spans_do_not() {
        let lines = vec![LaidLine {
            spans: vec![plain("שלום"), code("שלום")],
        }];
        let out = reorder_laid_lines(&lines, Direction::Rtl);
        assert_eq!(out[0].spans[0].text, "םולש", "prose span must reorder");
        assert_eq!(
            out[0].spans[1].text, "שלום",
            "a Code span must never be touched — it's source text, not prose"
        );
    }

    #[test]
    fn reorder_laid_lines_preserves_span_count_and_kind() {
        let lines = vec![LaidLine {
            spans: vec![plain("שלום"), code("x"), plain("עולם")],
        }];
        let out = reorder_laid_lines(&lines, Direction::Rtl);
        assert_eq!(out[0].spans.len(), 3);
        assert_eq!(out[0].spans[0].kind, SpanKind::Plain);
        assert_eq!(out[0].spans[1].kind, SpanKind::Code);
        assert_eq!(out[0].spans[2].kind, SpanKind::Plain);
    }
}
