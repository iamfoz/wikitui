//! Color themes (PRD §5.11, Appendix C, FR-TH-1/3/6/8). Six built-in
//! presets: `terminal` (the safe default, inherits the user's palette),
//! `full` (truecolor + images), `homebrew` (phosphor green on black),
//! `night` (red on black), `paper` (dark grey on cream), and `contrast`
//! (high-contrast accessibility) — plus user-authored theme files
//! (`themes/*.toml` in the config directory, [`load_user_themes`]) and a
//! best-effort base16 scheme importer ([`parse_base16_toml_map`] /
//! [`parse_base16_yaml_map`]).
//!
//! Three load-bearing pieces beyond the theme table itself:
//!  - [`SemanticSlots`] + [`expand_slots`]: a user theme file only has to
//!    name the colors it cares about (Appendix C's `[colors]` schema);
//!    everything [`Theme`] needs but the schema doesn't cover (the table/
//!    infobox/focus/status/selected chrome slots) is derived from what *was*
//!    given, documented at each derivation site below.
//!  - [`ColorDepth`] + [`adapt_color`]/[`Theme::adapt`]: FR-TH-3's capability
//!    degradation. A theme is always authored in truecolor; `adapt` maps
//!    every slot down to the terminal's actual depth at the one point a
//!    theme becomes "the active theme" (`App::set_theme`), so every
//!    `ui::colored`/`base_style` call downstream stays oblivious to depth
//!    entirely — same pattern as `no_color`, just realized once per theme
//!    change instead of once per span.
//!  - [`load_user_themes`]: parse errors and low-contrast findings warn and
//!    the offending file is skipped (never crashes the app) — see its own
//!    doc comment.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    /// `None` means "don't set" — used only by `terminal`, which must never
    /// touch the user's background (PRD: "never sets bg; the safe default
    /// everywhere").
    pub bg: Option<Color>,
    pub fg: Option<Color>,
    pub heading: Color,
    pub link: Color,
    pub link_visited: Color,
    pub quote: Color,
    pub code: Color,
    pub table: Color,
    pub infobox: Color,
    pub image: Color,
    pub dim: Color,
    /// The currently Tab-focused link (§ui::spans_to_rspans).
    pub focus_fg: Color,
    pub focus_bg: Color,
    pub status_fg: Color,
    pub status_bg: Color,
    pub selected_bg: Color,
    pub selected_fg: Color,
    /// Whether this theme renders images (vs. alt-text placeholders only) —
    /// only `full` does today; real image rendering is future work (FR-RD-8),
    /// so this presently only changes a hint in the placeholder text.
    pub images: bool,
    /// The `match` semantic slot (PRD FR-TH-1 / Appendix C's theme schema):
    /// shared by in-page find highlighting (FR-NV-6) and full-text search
    /// snippet highlighting (FR-SR-2's `<span class="searchmatch">` spans).
    /// Painted bold; the *current* find match additionally gets
    /// `Modifier::REVERSED` on top (ui.rs) rather than a second color slot,
    /// so it stands out without every theme needing to define one.
    pub match_fg: Color,
    /// Appendix C's `[colors] warning` slot. No UI surface consumes this yet
    /// (there is no in-app warning banner today) — carried on `Theme` so a
    /// full theme-file round trip (FR-TH-1) has somewhere to land every
    /// documented schema field, the same reasoning `table`/`infobox`/`image`
    /// already established for chrome slots the schema doesn't name.
    pub warning: Color,
    /// Appendix C's `[colors] error` slot — see `warning`'s own doc comment.
    pub error: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

impl Theme {
    pub fn terminal() -> Self {
        Self {
            name: "terminal",
            bg: None,
            fg: None,
            heading: Color::Reset,
            link: Color::Blue,
            link_visited: Color::Magenta,
            quote: Color::Gray,
            code: Color::Green,
            table: Color::Gray,
            infobox: Color::Yellow,
            image: Color::DarkGray,
            dim: Color::DarkGray,
            focus_fg: Color::Black,
            focus_bg: Color::Yellow,
            status_fg: Color::White,
            status_bg: Color::DarkGray,
            selected_bg: Color::Blue,
            selected_fg: Color::White,
            images: false,
            match_fg: Color::Yellow,
            warning: Color::Yellow,
            error: Color::Red,
        }
    }

    pub fn full() -> Self {
        Self {
            name: "full",
            bg: Some(rgb(0x101418)),
            fg: Some(rgb(0xd8dee9)),
            heading: Color::White,
            link: rgb(0x6fb3ff),
            link_visited: rgb(0x9d8cff),
            quote: rgb(0xa3be8c),
            code: rgb(0xa3be8c),
            table: rgb(0xd8dee9),
            infobox: rgb(0x2e3440),
            image: rgb(0x6b7280),
            dim: rgb(0x6b7280),
            focus_fg: rgb(0x101418),
            focus_bg: rgb(0x6fb3ff),
            status_fg: rgb(0xd8dee9),
            status_bg: rgb(0x2e3440),
            selected_bg: rgb(0x6fb3ff),
            selected_fg: rgb(0x101418),
            images: true,
            // The exact value from Appendix C's theme-file schema example
            // (`[colors] match = "#ebcb8b"`) — `full` is the truecolor
            // reference theme that example was written against.
            match_fg: rgb(0xebcb8b),
            // Also Appendix C's own schema-example values (`warning =
            // "#d08770"`, `error = "#bf616a"`) — `full` doubles as that
            // example's reference theme for these two slots as well.
            warning: rgb(0xd08770),
            error: rgb(0xbf616a),
        }
    }

    pub fn homebrew() -> Self {
        Self {
            name: "homebrew",
            bg: Some(rgb(0x000000)),
            fg: Some(rgb(0x33ff33)),
            heading: rgb(0x66ff66),
            link: rgb(0x99ff99),
            link_visited: rgb(0x66cc66),
            quote: rgb(0x33ff33),
            code: rgb(0x33ff33),
            table: rgb(0x33ff33),
            infobox: rgb(0x66ff66),
            image: rgb(0x229922),
            dim: rgb(0x229922),
            focus_fg: rgb(0x000000),
            focus_bg: rgb(0x33ff33),
            status_fg: rgb(0x000000),
            status_bg: rgb(0x33ff33),
            selected_bg: rgb(0x66ff66),
            selected_fg: rgb(0x000000),
            images: false,
            match_fg: rgb(0xccffcc),
            // `warning` stays in the green ramp — not urgent enough to break
            // the monochrome aesthetic; `error` is the one deliberate
            // non-green accent, since an error is exactly the case worth
            // spending phosphor-green purity on.
            warning: rgb(0x66ff66),
            error: rgb(0xff5555),
        }
    }

    pub fn night() -> Self {
        Self {
            name: "night",
            bg: Some(rgb(0x000000)),
            fg: Some(rgb(0xff2b2b)),
            heading: rgb(0xff5555),
            link: rgb(0xff8080),
            link_visited: rgb(0xcc5050),
            quote: rgb(0xff2b2b),
            code: rgb(0xff2b2b),
            table: rgb(0xff2b2b),
            infobox: rgb(0xff5555),
            image: rgb(0x992020),
            dim: rgb(0x992020),
            focus_fg: rgb(0x000000),
            focus_bg: rgb(0xff2b2b),
            status_fg: rgb(0x000000),
            status_bg: rgb(0xff2b2b),
            selected_bg: rgb(0xff5555),
            selected_fg: rgb(0x000000),
            images: false,
            // Amber, not red — the PRD's own "night-amber" variant accent
            // (Appendix C), reused here since search highlighting is a
            // different semantic axis than body-text hierarchy (which
            // stays red-only "via weight, never dimming" by design).
            match_fg: rgb(0xffb000),
            warning: rgb(0xffb000), // the same amber — a second red-family
            // hue for warning would blur into the theme's own body text.
            error: rgb(0xff2b2b), // error *is* this theme's dominant hue.
        }
    }

    pub fn paper() -> Self {
        Self {
            name: "paper",
            bg: Some(rgb(0xf5f0e1)),
            fg: Some(rgb(0x3a3a3a)),
            heading: rgb(0x1a1a1a),
            link: rgb(0x1a5276),
            link_visited: rgb(0x6c3483),
            quote: rgb(0x555555),
            code: rgb(0x1a5276),
            table: rgb(0x3a3a3a),
            infobox: rgb(0x1a1a1a),
            image: rgb(0x8a8a8a),
            dim: rgb(0x8a8a8a),
            focus_fg: rgb(0xf5f0e1),
            focus_bg: rgb(0x1a5276),
            status_fg: rgb(0xf5f0e1),
            status_bg: rgb(0x3a3a3a),
            selected_bg: rgb(0x1a5276),
            selected_fg: rgb(0xf5f0e1),
            images: false,
            match_fg: rgb(0xb8860b), // dark goldenrod, readable on the cream bg
            warning: rgb(0xb8860b),
            error: rgb(0xbf616a), // Appendix C's own schema-example error hex
        }
    }

    pub fn contrast() -> Self {
        Self {
            name: "contrast",
            bg: Some(Color::Black),
            fg: Some(Color::White),
            heading: Color::White,
            link: Color::LightCyan,
            link_visited: Color::LightMagenta,
            quote: Color::White,
            code: Color::White,
            table: Color::White,
            infobox: Color::White,
            image: Color::Gray,
            dim: Color::Gray,
            focus_fg: Color::Black,
            focus_bg: Color::LightCyan,
            status_fg: Color::Black,
            status_bg: Color::White,
            selected_bg: Color::LightCyan,
            selected_fg: Color::Black,
            images: false,
            // Named ANSI color, like every other `contrast` slot (kept out
            // of the RGB contrast lint below for the same reason `link` is).
            match_fg: Color::LightYellow,
            warning: Color::LightYellow,
            error: Color::LightRed,
        }
    }

    /// All built-in theme names, in the order `cycle` moves through them.
    pub const NAMES: [&'static str; 6] =
        ["terminal", "full", "homebrew", "night", "paper", "contrast"];

    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "terminal" => Some(Self::terminal()),
            "full" => Some(Self::full()),
            "homebrew" => Some(Self::homebrew()),
            "night" => Some(Self::night()),
            "paper" => Some(Self::paper()),
            "contrast" => Some(Self::contrast()),
            _ => None,
        }
    }

    /// The theme after this one in `NAMES`, wrapping around — used by the
    /// `Ctrl-T` keybinding (moved off bare `T` when FR-ACC-5's talk-page
    /// toggle claimed it, see `registry`'s default keymap) to cycle themes
    /// for live preview (PRD FR-TH-2's runtime-switching requirement; the
    /// `:theme <name>` command syntax arrives with the command system,
    /// FR-CS-2).
    pub fn next(&self) -> Self {
        let idx = Self::NAMES
            .iter()
            .position(|n| *n == self.name)
            .unwrap_or(0);
        let next_name = Self::NAMES[(idx + 1) % Self::NAMES.len()];
        Self::by_name(next_name).expect("NAMES only lists valid themes")
    }
}

/// `Some((r,g,b))` for a truecolor slot, `None` for anything defined in
/// terms of the terminal's 16-color palette (`Color::Black`, `Color::Gray`,
/// …) or left unset. FR-TH-6's linter only has real pixels to compare for
/// the former — a named ANSI color's actual RGB is up to the terminal, so
/// `contrast` (which uses named colors throughout) is correctly out of
/// scope for the numeric lint, not silently wrong.
fn as_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// WCAG 2.x relative luminance of one sRGB channel (0-255), the piecewise
/// gamma-decode both the 2.1 and 2.2 formulas use.
fn channel_luminance(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 2.x relative luminance of an sRGB triple, 0.0 (black) to 1.0
/// (white). `pub(crate)` rather than test-only: `autotheme`'s OSC 11
/// light/dark classification (PRD FR-TH-4) reuses this exact formula rather
/// than a second copy that could drift from FR-TH-6's own contrast lint.
pub(crate) fn relative_luminance((r, g, b): (u8, u8, u8)) -> f64 {
    0.2126 * channel_luminance(r) + 0.7152 * channel_luminance(g) + 0.0722 * channel_luminance(b)
}

/// WCAG 2.x contrast ratio between two sRGB colors, from 1:1 (identical) to
/// 21:1 (black on white) — the metric FR-TH-6's "warn below 4.5:1" and
/// `contrast`'s "≥ 7:1" claims (Appendix C) are both stated in.
pub fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (lighter, darker) = if la > lb { (la, lb) } else { (lb, la) };
    (lighter + 0.05) / (darker + 0.05)
}

/// One fg/bg-style pair FR-TH-6's linter checked for a built-in theme.
#[derive(Debug, Clone, Copy)]
pub struct ContrastCheck {
    pub theme: &'static str,
    /// "fg/bg", "link/bg", or "dim/bg".
    pub pair: &'static str,
    pub ratio: f64,
}

impl ContrastCheck {
    /// FR-TH-6's own threshold: below this, the pair warns.
    pub const AA_THRESHOLD: f64 = 4.5;
    /// The stricter bar Appendix C invokes for `contrast` (not checked here
    /// since `contrast` uses named, not RGB, colors) and for explaining
    /// why `night` — which clears `AA_THRESHOLD` — still isn't AAA.
    pub const AAA_THRESHOLD: f64 = 7.0;

    pub fn passes(&self) -> bool {
        self.ratio >= Self::AA_THRESHOLD
    }

    /// "AAA" (≥ 7:1), "AA" (≥ 4.5:1, FR-TH-6's own bar), or "FAIL".
    /// `night`'s fg/bg lands in "AA" by design (Appendix C: red held near
    /// full brightness makes pure red/black ~5.25:1 — passes AA, fails
    /// AAA); that is a documented tradeoff, not a lint finding to chase to
    /// AAA by dimming the red, which would undo the reason it's there.
    pub fn level(&self) -> &'static str {
        if self.ratio >= Self::AAA_THRESHOLD {
            "AAA"
        } else if self.ratio >= Self::AA_THRESHOLD {
            "AA"
        } else {
            "FAIL"
        }
    }
}

/// The fg/bg, link/bg, and dim/bg pairs for one named theme whose relevant
/// slots are RGB-defined — shared by [`builtin_contrast_report`] (FR-TH-6 on
/// the six built-ins) and [`user_contrast_report`] (the same lint extended to
/// user theme files), so the two can never check different things.
fn contrast_checks_for(name: &'static str, theme: &Theme) -> Vec<ContrastCheck> {
    let mut out = Vec::new();
    let Some(bg) = theme.bg.and_then(as_rgb) else {
        return out;
    };
    if let Some(fg) = theme.fg.and_then(as_rgb) {
        out.push(ContrastCheck {
            theme: name,
            pair: "fg/bg",
            ratio: contrast_ratio(fg, bg),
        });
    }
    if let Some(link) = as_rgb(theme.link) {
        out.push(ContrastCheck {
            theme: name,
            pair: "link/bg",
            ratio: contrast_ratio(link, bg),
        });
    }
    if let Some(dim) = as_rgb(theme.dim) {
        out.push(ContrastCheck {
            theme: name,
            pair: "dim/bg",
            ratio: contrast_ratio(dim, bg),
        });
    }
    out
}

/// FR-TH-6's contrast lint: fg/bg, link/bg, and dim/bg for every built-in
/// theme whose relevant slots are RGB-defined (`terminal` has no
/// bg/fg to check; `contrast` is defined in named ANSI colors, not RGB —
/// both are correctly absent from this report, not silently passing it).
pub fn builtin_contrast_report() -> Vec<ContrastCheck> {
    Theme::NAMES
        .iter()
        .flat_map(|&name| {
            let theme = Theme::by_name(name).expect("NAMES only lists valid themes");
            contrast_checks_for(name, &theme)
        })
        .collect()
}

/// FR-TH-6's contrast lint extended to user theme files (deliverable 3):
/// the same fg/bg-style checks `builtin_contrast_report` runs, over whatever
/// [`load_user_themes`] loaded. Callers that want to *warn* on a low-contrast
/// user theme should prefer the warnings `load_user_themes` itself already
/// produced (which respect `allow_low_contrast`) — this report is for
/// display (`wikitui config doctor`), which shows every ratio regardless.
pub fn user_contrast_report(user_themes: &[LoadedUserTheme]) -> Vec<ContrastCheck> {
    user_themes
        .iter()
        .flat_map(|t| contrast_checks_for(t.theme.name, &t.theme))
        .collect()
}

// ---------------------------------------------------------------------
// FR-TH-3: capability degradation (truecolor -> 256 -> 16 -> mono).
// ---------------------------------------------------------------------

/// The terminal color depths FR-TH-3 degrades across, brightest first.
/// `Mono` deliberately reuses the same "modifiers only, no color" behavior
/// `NO_COLOR` (FR-TH-5) already gives every span (`ui::colored`) — the two
/// are the same visual contract from two different triggers, not a second
/// code path to keep in sync. `NO_COLOR` itself is intentionally *not*
/// folded into detection here: it is a policy override owned by
/// `main::no_color_active` (FR-TH-5), not a capability signal, so combining
/// the two is the caller's job (`main` ANDs them together before this ever
/// reaches `App::color_depth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    Truecolor,
    TwoFiftySix,
    Sixteen,
    Mono,
}

impl ColorDepth {
    /// Parses an explicit `color_depth` config value (`"truecolor"`,
    /// `"256"`, `"16"`, `"mono"`) — `"auto"` is handled by
    /// [`resolve_color_depth`], not here, since it needs environment access
    /// this pure parse deliberately doesn't.
    pub fn parse(s: &str) -> Option<ColorDepth> {
        match s {
            "truecolor" => Some(ColorDepth::Truecolor),
            "256" => Some(ColorDepth::TwoFiftySix),
            "16" => Some(ColorDepth::Sixteen),
            "mono" => Some(ColorDepth::Mono),
            _ => None,
        }
    }

    /// A short label for `wikitui config doctor`'s capability report.
    pub fn label(self) -> &'static str {
        match self {
            ColorDepth::Truecolor => "truecolor",
            ColorDepth::TwoFiftySix => "256-color",
            ColorDepth::Sixteen => "16-color",
            ColorDepth::Mono => "mono",
        }
    }
}

/// FR-TH-3's `auto` detection logic, as a pure function of the two env
/// strings it reads — split out from [`detect_color_depth`] so tests can
/// exercise every branch directly instead of mutating real process env vars
/// (which `config::EnvOverrides`'s own doc comment already flags as a
/// `cargo test`-parallelism race; this module follows that same established
/// precedent rather than reintroducing the hazard).
///
/// `$COLORTERM=truecolor`/`24bit` -> truecolor; `$TERM` containing
/// `256color` -> 256; `$TERM=dumb` -> mono (a "dumb" terminal has no color
/// capability at all — a real capability signal, unlike `NO_COLOR`, so it
/// belongs here rather than in the caller's policy layer); anything else ->
/// 16, the safe assumption for an ordinary unlabeled terminal.
fn classify_color_depth(term: &str, colorterm: &str) -> ColorDepth {
    if term == "dumb" {
        return ColorDepth::Mono;
    }
    if colorterm == "truecolor" || colorterm == "24bit" {
        return ColorDepth::Truecolor;
    }
    if term.contains("256color") {
        return ColorDepth::TwoFiftySix;
    }
    ColorDepth::Sixteen
}

/// Reads `$TERM`/`$COLORTERM` from the real process environment and
/// classifies them via [`classify_color_depth`] — the production entry
/// point; see that function for the actual rule and why it, not this thin
/// wrapper, is what tests exercise directly.
pub fn detect_color_depth() -> ColorDepth {
    classify_color_depth(
        &std::env::var("TERM").unwrap_or_default(),
        &std::env::var("COLORTERM").unwrap_or_default(),
    )
}

/// Resolves the `color_depth` config value (`"auto"` or an explicit depth)
/// to a concrete [`ColorDepth`] — `"auto"` and any value that fails to parse
/// both fall back to [`detect_color_depth`] (config.rs already validates the
/// string against the closed allowed set, so the latter is a belt-and-braces
/// default, not an expected path).
pub fn resolve_color_depth(configured: &str) -> ColorDepth {
    if configured == "auto" {
        detect_color_depth()
    } else {
        ColorDepth::parse(configured).unwrap_or_else(detect_color_depth)
    }
}

/// The 6 RGB levels xterm's 256-color cube uses per channel (indices
/// 16..=231, `16 + 36*r + 6*g + b`).
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn nearest_cube_index(v: u8) -> usize {
    CUBE_LEVELS
        .iter()
        .enumerate()
        .min_by_key(|&(_, &level)| (i32::from(level) - i32::from(v)).abs())
        .map(|(i, _)| i)
        .expect("CUBE_LEVELS is non-empty")
}

fn squared_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> i64 {
    let d = |x: u8, y: u8| i64::from(x) - i64::from(y);
    let (dr, dg, db) = (d(a.0, b.0), d(a.1, b.1), d(a.2, b.2));
    dr * dr + dg * dg + db * db
}

/// Quantizes a truecolor RGB triple to the nearest xterm 256-color palette
/// index (FR-TH-3's computed 256-color approximation): the better of the
/// nearest 6x6x6 color-cube entry (indices 16..=231) and the nearest
/// grayscale-ramp entry (232..=255, 24 steps from level 8 to 238) — the
/// standard algorithm most terminal-color libraries use, since xterm's
/// palette has no direct RGB-to-index formula.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let (ri, gi, bi) = (
        nearest_cube_index(r),
        nearest_cube_index(g),
        nearest_cube_index(b),
    );
    let cube_color = (CUBE_LEVELS[ri], CUBE_LEVELS[gi], CUBE_LEVELS[bi]);
    let cube_index = 16 + 36 * ri + 6 * gi + bi;

    let gray_avg = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let gray_step = if gray_avg <= 8 {
        0u32
    } else if gray_avg >= 238 {
        23
    } else {
        (((gray_avg - 8) as f64) / 10.0).round() as u32
    }
    .min(23);
    let gray_level = (8 + 10 * gray_step) as u8;
    let gray_index = 232 + gray_step as u8;

    let target = (r, g, b);
    if squared_distance(target, (gray_level, gray_level, gray_level))
        <= squared_distance(target, cube_color)
    {
        gray_index
    } else {
        cube_index as u8
    }
}

/// The xterm default 16-color palette's reference RGB values — a fixed
/// table, not a live query (no terminal in this build's reach reliably
/// reports its actual customized palette), used only to pick the *nearest*
/// of the 16 named ANSI colors for FR-TH-3's 16-color degradation. A
/// terminal with a heavily recolored palette will see a merely-reasonable
/// approximation rather than a perfect one — an accepted, documented
/// limitation of degrading without querying the live terminal (the same
/// limitation `autotheme`'s OSC 11 query exists specifically to avoid for
/// light/dark classification, but doing that per-slot here would mean 17
/// OSC 4 round trips per theme change, which is not worth it for a fallback
/// path only low-capability terminals ever take).
const ANSI_16_REFERENCE: [(Color, (u8, u8, u8)); 16] = [
    (Color::Black, (0x00, 0x00, 0x00)),
    (Color::Red, (0xCD, 0x00, 0x00)),
    (Color::Green, (0x00, 0xCD, 0x00)),
    (Color::Yellow, (0xCD, 0xCD, 0x00)),
    (Color::Blue, (0x00, 0x00, 0xEE)),
    (Color::Magenta, (0xCD, 0x00, 0xCD)),
    (Color::Cyan, (0x00, 0xCD, 0xCD)),
    (Color::Gray, (0xE5, 0xE5, 0xE5)),
    (Color::DarkGray, (0x7F, 0x7F, 0x7F)),
    (Color::LightRed, (0xFF, 0x00, 0x00)),
    (Color::LightGreen, (0x00, 0xFF, 0x00)),
    (Color::LightYellow, (0xFF, 0xFF, 0x00)),
    (Color::LightBlue, (0x5C, 0x5C, 0xFF)),
    (Color::LightMagenta, (0xFF, 0x00, 0xFF)),
    (Color::LightCyan, (0x00, 0xFF, 0xFF)),
    (Color::White, (0xFF, 0xFF, 0xFF)),
];

/// Quantizes a truecolor RGB triple to the nearest of the 16 standard ANSI
/// colors (FR-TH-3's computed 16-color approximation), by Euclidean distance
/// against [`ANSI_16_REFERENCE`].
pub fn rgb_to_16(r: u8, g: u8, b: u8) -> Color {
    ANSI_16_REFERENCE
        .iter()
        .min_by_key(|(_, ref_rgb)| squared_distance((r, g, b), *ref_rgb))
        .map(|(color, _)| *color)
        .expect("ANSI_16_REFERENCE is non-empty")
}

/// Maps one theme color down to `depth`, honoring a declared per-slot
/// fallback (`[fallback] palette256`/`palette16` in a user theme file) ahead
/// of computed quantization when one is given (deliverable 2's "declared
/// fallbacks take precedence over computed quantization") — `declared` is
/// `None` for every built-in theme (which have none) and for any slot a user
/// theme didn't declare, in which case this falls through to
/// [`rgb_to_256`]/[`rgb_to_16`]. Named ANSI colors (`Color::Blue`, …) and
/// `Color::Reset` are already depth-appropriate at every depth above `Mono`
/// (ratatui encodes them as a plain 16-color SGR code regardless of
/// terminal capability) and pass straight through; only `Color::Rgb` needs
/// active quantization.
pub fn adapt_color(color: Color, depth: ColorDepth, declared: Option<Color>) -> Color {
    if depth == ColorDepth::Mono {
        return Color::Reset;
    }
    match color {
        Color::Rgb(r, g, b) => match depth {
            ColorDepth::Truecolor => color,
            ColorDepth::TwoFiftySix => {
                declared.unwrap_or_else(|| Color::Indexed(rgb_to_256(r, g, b)))
            }
            ColorDepth::Sixteen => declared.unwrap_or_else(|| rgb_to_16(r, g, b)),
            ColorDepth::Mono => unreachable!("handled above"),
        },
        _ => declared.unwrap_or(color),
    }
}

/// A user theme's declared 256-/16-color approximations (Appendix C's
/// `[fallback]` table), keyed by the same slot names as `[colors]`
/// (`"bg"`, `"accent"`, `"code_bg"`, …) — see [`Theme::adapt`] for exactly
/// which alias each `Theme` field looks under. Empty (the default) for
/// every built-in theme and for a user theme with no `[fallback]` section;
/// `adapt_color` simply computes an approximation in that case.
#[derive(Debug, Clone, Default)]
pub struct PaletteFallback {
    pub palette256: BTreeMap<String, u8>,
    pub palette16: BTreeMap<String, Color>,
}

impl PaletteFallback {
    /// The declared color for the first of `slots` (tried in order —
    /// callers pass a slot's own key plus any documented alias, e.g. `link`
    /// falling back to a declared `accent`) present in the table for `depth`,
    /// or `None` to let `adapt_color` compute one.
    fn resolve(&self, slots: &[&str], depth: ColorDepth) -> Option<Color> {
        match depth {
            ColorDepth::TwoFiftySix => slots
                .iter()
                .find_map(|s| self.palette256.get(*s))
                .map(|&i| Color::Indexed(i)),
            ColorDepth::Sixteen => slots.iter().find_map(|s| self.palette16.get(*s)).copied(),
            _ => None,
        }
    }
}

impl Theme {
    /// FR-TH-3: maps every color slot on this theme down to `depth`,
    /// preferring `fallback`'s declared per-slot approximation over computed
    /// quantization wherever one exists. `name`/`images` pass through
    /// unchanged — this only ever touches colors. The single choke point for
    /// degradation: `App::set_theme` calls this on every theme change (the
    /// initial one, `T`-cycling, `:theme`, `:set theme=`, and `:config
    /// reload`), so nothing downstream (`ui::colored`/`base_style`) ever
    /// needs to know `depth` exists.
    pub fn adapt(&self, depth: ColorDepth, fallback: Option<&PaletteFallback>) -> Theme {
        let d = |slots: &[&str], c: Color| -> Color {
            let declared = fallback.and_then(|f| f.resolve(slots, depth));
            adapt_color(c, depth, declared)
        };
        let od = |slots: &[&str], c: Option<Color>| -> Option<Color> { c.map(|c| d(slots, c)) };
        Theme {
            name: self.name,
            bg: od(&["bg"], self.bg),
            fg: od(&["fg"], self.fg),
            heading: d(&["heading"], self.heading),
            link: d(&["link", "accent"], self.link),
            link_visited: d(&["link_visited"], self.link_visited),
            quote: d(&["quote"], self.quote),
            code: d(&["code_bg", "code"], self.code),
            table: d(&["table"], self.table),
            infobox: d(&["infobox"], self.infobox),
            image: d(&["image"], self.image),
            dim: d(&["dim"], self.dim),
            focus_fg: d(&["focus_fg"], self.focus_fg),
            focus_bg: d(&["focus_bg", "accent", "link"], self.focus_bg),
            status_fg: d(&["status_fg"], self.status_fg),
            status_bg: d(&["status_bg"], self.status_bg),
            selected_bg: d(&["selected_bg", "accent", "link"], self.selected_bg),
            selected_fg: d(&["selected_fg"], self.selected_fg),
            images: self.images,
            match_fg: d(&["match"], self.match_fg),
            warning: d(&["warning"], self.warning),
            error: d(&["error"], self.error),
        }
    }
}

// ---------------------------------------------------------------------
// FR-TH-1: user theme files — semantic slots, auto-expansion, base16
// import, and the config-directory loader.
// ---------------------------------------------------------------------

/// Parses a hex color string into a truecolor `Color::Rgb`, accepting both
/// `"#rrggbb"` (Appendix C's own `[colors]` schema) and bare `"rrggbb"` (how
/// upstream base16 scheme files write every `base00`..`base0F` value) by
/// delegating to `ratatui::style::Color`'s own `FromStr` (which requires the
/// `#`) after normalizing the bare form — one hex parser instead of a second
/// one that could disagree with ratatui's about, say, case-sensitivity.
pub(crate) fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.trim();
    let with_hash = if s.starts_with('#') {
        s.to_string()
    } else {
        format!("#{s}")
    };
    match Color::from_str(&with_hash) {
        Ok(c @ Color::Rgb(..)) => Some(c),
        _ => None,
    }
}

/// The partial color inputs a theme file (native or base16) supplies before
/// [`expand_slots`] fills in everything it didn't. Field names match
/// Appendix C's `[colors]` schema (`code` here is that schema's `code_bg`,
/// `match_fg` is its `match` — renamed only because `match` is a Rust
/// keyword and `code` already means "the code slot" throughout `Theme`).
#[derive(Debug, Clone, Default)]
pub struct SemanticSlots {
    pub name: String,
    pub dark: bool,
    pub images: bool,
    pub allow_low_contrast: bool,
    pub bg: Option<Color>,
    pub fg: Option<Color>,
    pub accent: Option<Color>,
    pub link: Option<Color>,
    pub link_visited: Option<Color>,
    pub heading: Option<Color>,
    pub quote: Option<Color>,
    pub code: Option<Color>,
    pub match_fg: Option<Color>,
    pub warning: Option<Color>,
    pub error: Option<Color>,
    pub dim: Option<Color>,
}

/// Blends two truecolor `Color`s channel-by-channel, `t` fraction of the way
/// from `a` to `b` (`t=0.0` is `a`, `t=1.0` is `b`) — the mixing primitive
/// every derivation below uses instead of picking arbitrary fixed shades, so
/// a derived slot always relates back to colors the theme file actually
/// gave. Non-RGB inputs (this only ever runs on values already parsed from
/// hex, so this is a defensive fallback, not an expected path) are treated
/// as mid-gray.
fn mix(a: Color, b: Color, t: f64) -> Color {
    let as_triple = |c: Color| match c {
        Color::Rgb(r, g, b) => (f64::from(r), f64::from(g), f64::from(b)),
        _ => (128.0, 128.0, 128.0),
    };
    let (ar, ag, ab) = as_triple(a);
    let (br, bg, bb) = as_triple(b);
    let lerp = |x: f64, y: f64| (x + (y - x) * t).round().clamp(0.0, 255.0) as u8;
    Color::Rgb(lerp(ar, br), lerp(ag, bg), lerp(ab, bb))
}

/// Expands a theme file's given [`SemanticSlots`] into a complete [`Theme`]
/// (deliverable 1's "semantic slots auto-expanded to shades"), so a minimal
/// file — even one giving only `bg`/`fg`/`link` — still produces every color
/// `Theme` needs. The derivation chain, in dependency order:
///
///  - `link` defaults to `accent`, then to `fg`, then (only for a file
///    giving none of the three) to `full`'s own link blue — a fixed safety
///    net so this function never has to fail.
///  - `accent` (used only as a derivation source — `Theme` has no separate
///    accent field) defaults to `link`.
///  - `heading` defaults to `accent` — Wikipedia's own convention of
///    coloring headings like the link/accent hue.
///  - `dim` defaults to a blend 60% of the way from `fg` toward `bg` (or,
///    lacking a `bg`, toward a fixed mid-gray) — a muted shade of body text.
///  - `quote` defaults to a blend halfway between `fg` and `dim` — between
///    full body-text emphasis and the fully de-emphasized `dim`.
///  - `code` (the schema's `code_bg`) defaults to `quote` — matching how
///    `full`/`homebrew`/`night`/`contrast` all set the two identically.
///  - `link_visited` defaults to a blend 35% of the way from `link` toward
///    `bg` (or black) — a duller version of `link`, echoing how browsers
///    render visited links darker.
///  - `match` defaults to `warning`, then to Appendix C's own schema-example
///    goldenrod (`#ebcb8b`) — a color that reads as "highlight" in every
///    theme regardless of hue.
///  - `warning` defaults to `match`; `error` defaults to Appendix C's own
///    schema-example red (`#bf616a`).
///  - The chrome slots Appendix C's schema doesn't name at all — `table`
///    (defaults to `fg`), `infobox` (defaults to `heading`), `image`
///    (defaults to `dim`), `focus_bg`/`selected_bg` (default to `accent`),
///    `focus_fg`/`status_fg`/`selected_fg` (default to `bg`, or black),
///    `status_bg` (defaults to `fg`) — mirror the majority pattern the six
///    built-in themes already establish for each (see their own literals).
pub fn expand_slots(slots: &SemanticSlots) -> Theme {
    const SAFE_ACCENT: Color = rgb(0x6fb3ff);
    let black = Color::Rgb(0, 0, 0);

    let link = slots
        .link
        .or(slots.accent)
        .or(slots.fg)
        .unwrap_or(SAFE_ACCENT);
    let accent = slots.accent.or(slots.link).unwrap_or(link);
    let heading = slots.heading.unwrap_or(accent);
    let dim = slots.dim.unwrap_or_else(|| match slots.fg {
        Some(fg) => mix(fg, slots.bg.unwrap_or(Color::Rgb(0x60, 0x60, 0x60)), 0.6),
        None => Color::DarkGray,
    });
    let quote = slots.quote.unwrap_or_else(|| match slots.fg {
        Some(fg) => mix(fg, dim, 0.5),
        None => dim,
    });
    let code = slots.code.unwrap_or(quote);
    let link_visited = slots
        .link_visited
        .unwrap_or_else(|| mix(link, slots.bg.unwrap_or(black), 0.35));
    let match_fg = slots.match_fg.or(slots.warning).unwrap_or(rgb(0xebcb8b));
    let warning = slots.warning.unwrap_or(match_fg);
    let error = slots.error.unwrap_or(rgb(0xbf616a));

    let table = slots.fg.unwrap_or(link);
    let infobox = heading;
    let image = dim;
    let focus_bg = accent;
    let focus_fg = slots.bg.unwrap_or(Color::Black);
    let status_fg = slots.bg.unwrap_or(Color::Black);
    let status_bg = slots.fg.unwrap_or(link);
    let selected_bg = accent;
    let selected_fg = slots.bg.unwrap_or(Color::Black);

    Theme {
        name: Box::leak(slots.name.clone().into_boxed_str()),
        bg: slots.bg,
        fg: slots.fg,
        heading,
        link,
        link_visited,
        quote,
        code,
        table,
        infobox,
        image,
        dim,
        focus_fg,
        focus_bg,
        status_fg,
        status_bg,
        selected_bg,
        selected_fg,
        images: slots.images,
        match_fg,
        warning,
        error,
    }
}

/// Parses a native wikitui theme file (Appendix C's `[meta]`/`[colors]`/
/// `[fallback]` schema). `file_stem` names the theme when `[meta] name` is
/// absent. Returns a plain error string (never panics) on unparseable TOML —
/// the caller ([`load_user_themes`]) turns that into a "skip this file, warn"
/// outcome, never a crash.
pub fn parse_native_theme_toml(
    text: &str,
    file_stem: &str,
) -> Result<(SemanticSlots, PaletteFallback), String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| format!("not valid TOML: {e}"))?;

    let meta = table.get("meta").and_then(toml::Value::as_table);
    let name = meta
        .and_then(|m| m.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or(file_stem)
        .to_string();
    let dark = meta
        .and_then(|m| m.get("dark"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    let images = meta
        .and_then(|m| m.get("images"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let allow_low_contrast = meta
        .and_then(|m| m.get("allow_low_contrast"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);

    let colors = table.get("colors").and_then(toml::Value::as_table);
    let get = |key: &str| -> Option<Color> {
        colors
            .and_then(|c| c.get(key))
            .and_then(toml::Value::as_str)
            .and_then(parse_hex_color)
    };

    let slots = SemanticSlots {
        name,
        dark,
        images,
        allow_low_contrast,
        bg: get("bg"),
        fg: get("fg"),
        accent: get("accent"),
        link: get("link"),
        link_visited: get("link_visited"),
        heading: get("heading"),
        quote: get("quote"),
        code: get("code_bg"),
        match_fg: get("match"),
        warning: get("warning"),
        error: get("error"),
        dim: get("dim"),
    };
    let fallback = parse_fallback_table(table.get("fallback").and_then(toml::Value::as_table));
    Ok((slots, fallback))
}

fn parse_fallback_table(table: Option<&toml::Table>) -> PaletteFallback {
    let mut out = PaletteFallback::default();
    let Some(table) = table else {
        return out;
    };
    if let Some(p256) = table.get("palette256").and_then(toml::Value::as_table) {
        for (slot, value) in p256 {
            if let Some(index) = value.as_integer().and_then(|n| u8::try_from(n).ok()) {
                out.palette256.insert(slot.clone(), index);
            }
        }
    }
    if let Some(p16) = table.get("palette16").and_then(toml::Value::as_table) {
        for (slot, value) in p16 {
            if let Some(color) = value.as_str().and_then(|s| Color::from_str(s).ok()) {
                out.palette16.insert(slot.clone(), color);
            }
        }
    }
    out
}

/// Lowercases and hyphenates free-form prose (a base16 scheme title, a
/// wiki-tui config value) into a `:theme <name>`-typeable token: runs of
/// non-alphanumeric characters collapse to a single `-`, with no leading,
/// trailing, or doubled hyphens.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The base16 styling guide's own documented semantics
/// (<https://github.com/chriskempson/base16/blob/main/styling.md>), mapped
/// onto wikitui's slots — the assumptions this build makes when importing a
/// base16 scheme (FR-TH-1's "base16 scheme import").
///
///  - `base00` (default background) -> `bg`; `base05` (default foreground)
///    -> `fg` — the two grayscale slots the guide names for exactly this.
///  - `base0D` (guide: "Functions, Methods, Attribute IDs, **Headings**") ->
///    `link`/`accent`/`heading` — the guide explicitly names headings, and
///    base16-scheme authors overwhelmingly also treat 0D as their scheme's
///    primary accent, so reusing it for the link color too is standard
///    practice, not a stretch.
///  - `base0E` ("Keywords, Storage, Selector, ... Diff Changed") -> a
///    secondary accent with no dedicated "visited link" meaning in the
///    guide; reused here as `link_visited` since it is the next most
///    prominent hue after 0D in virtually every published scheme.
///  - `base0A` ("Classes, Markup Bold, **Search Text Background**") ->
///    `match` — the guide's own wording is a near-exact description of
///    wikitui's find-highlight slot.
///  - `base0B` ("Strings, Inherited Class, **Markup Code**") -> `code_bg`.
///  - `base0C` ("Support, Regular Expressions, Escape Characters, **Markup
///    Quotes**") -> `quote`.
///  - `base08` ("Variables, XML Tags, Markup Link Text, **Diff Deleted**")
///    -> `error` — red is the de facto error color in virtually every
///    scheme regardless of the guide's own wording.
///  - `base09` ("Integers, Boolean, Constants, ...") -> `warning` — the
///    guide has no dedicated warning slot; orange (0's conventional hue) is
///    this build's own convention for it, not a documented base16 meaning.
///  - `base03` ("Comments, Invisibles, Line Highlighting") -> `dim`.
///  - `base01`/`base02`/`base04`/`base06`/`base07` are not used: the
///    `expand_slots` derivation chain already produces every chrome slot
///    (selection/status backgrounds, etc.) this build needs from the slots
///    above, and reusing those four best-effort would mean two derivation
///    systems disagreeing with each other rather than one.
pub fn base16_to_slots(map: &BTreeMap<String, String>, name_hint: &str) -> SemanticSlots {
    let get = |key: &str| map.get(key).and_then(|v| parse_hex_color(v));
    // The scheme title (`scheme: Solarized Dark`) is free-form prose, but a
    // theme name is also a `:theme <name>` command argument — slugified so
    // it's actually typeable, rather than importing "Solarized Dark" (with
    // its space and mixed case) verbatim.
    let name = map
        .get("scheme")
        .map(|s| slugify(s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| name_hint.to_string());
    SemanticSlots {
        name,
        // base16 schemes are conventionally authored dark-on-light-text;
        // this build has no reliable per-scheme signal otherwise, so `dark`
        // defaults true (documented assumption — a light base16 scheme
        // imports fine, it just carries an inaccurate `dark` flag, which
        // nothing in this build currently branches on beyond FR-TH-4's
        // theme_light/theme_dark *names*, not their contents).
        dark: true,
        images: false,
        allow_low_contrast: false,
        bg: get("base00"),
        fg: get("base05"),
        accent: get("base0d"),
        link: get("base0d"),
        link_visited: get("base0e"),
        heading: get("base0d"),
        quote: get("base0c"),
        code: get("base0b"),
        match_fg: get("base0a"),
        warning: get("base09"),
        error: get("base08"),
        dim: get("base03"),
    }
}

/// Reads a base16 scheme expressed as flat TOML keys (`scheme`, `author`,
/// `base00`..`base0F` as top-level string values, no `[colors]` table) —
/// this build's TOML-native way of accepting base16 data (see
/// [`parse_base16_yaml_map`]'s doc comment for why upstream's actual YAML
/// format isn't parsed directly).
pub fn parse_base16_toml_map(text: &str) -> Result<BTreeMap<String, String>, String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| format!("not valid TOML: {e}"))?;
    let mut map = BTreeMap::new();
    for (key, value) in &table {
        if let Some(s) = value.as_str() {
            map.insert(key.to_lowercase(), s.to_string());
        }
    }
    Ok(map)
}

/// A hand-rolled reader for the flat `key: value` scalar mapping every
/// upstream base16 scheme YAML file actually is (`scheme:`, `author:`,
/// `base00:` .. `base0F:`, one per line, no nesting/lists/anchors/multi-line
/// strings) — deliberately **not** a general YAML parser. This build has no
/// YAML dependency, and adding one just to accept a 16-key flat color map
/// verbatim was judged disproportionate; a user (or a future contributor
/// with more appetite for a `serde_yaml` dependency) can trivially re-key an
/// upstream scheme into the TOML form [`parse_base16_toml_map`] reads
/// instead. Any line that isn't blank, a `#` comment, or a `key: value` pair
/// is a hard parse error (never a panic) — the caller warns and skips the
/// file, same as malformed TOML.
pub fn parse_base16_yaml_map(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for (lineno, raw_line) in text.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!(
                "line {}: {raw_line:?} is not a `key: value` pair",
                lineno + 1
            ));
        };
        let key = key.trim().trim_matches(['"', '\'']).to_lowercase();
        let value = value.trim().trim_matches(['"', '\'']).to_string();
        if !key.is_empty() && !value.is_empty() {
            map.insert(key, value);
        }
    }
    Ok(map)
}

/// One user theme, loaded and fully expanded — the [`load_user_themes`]
/// result the rest of the app consumes: `theme` for painting/`resolve_named`
/// lookups, `fallback` for `Theme::adapt`'s declared-approximation
/// precedence, `allow_low_contrast` so callers that re-check contrast at
/// load time (this module's own loader) know whether a low ratio should
/// warn.
#[derive(Debug, Clone)]
pub struct LoadedUserTheme {
    pub name: String,
    pub theme: Theme,
    pub fallback: PaletteFallback,
    pub allow_low_contrast: bool,
    /// The `[meta] dark` claim from the theme file (default `true`) —
    /// informational (`wikitui config doctor` reports it); this build
    /// doesn't branch behavior on it beyond that today (FR-TH-4's
    /// `theme_light`/`theme_dark` match by *name*, not by inspecting a
    /// candidate theme's contents), a documented seam for whenever
    /// auto-selecting *among* user themes by declared darkness becomes a
    /// real feature rather than always requiring an explicit name.
    pub dark: bool,
    pub source: PathBuf,
}

/// Loads every `themes/*.toml` and `themes/*.yaml`/`*.yml` file in `dir`
/// (FR-TH-1: `$XDG_CONFIG_HOME/wikitui/themes/`), returning the themes that
/// parsed alongside human-readable warnings for the ones that didn't — a
/// bad file is skipped, never a crash (deliverable 1's "parse errors warn,
/// never crash"), and neither is a file whose name collides with a built-in
/// theme (silently shadowing `paper` or `full` would be far more confusing
/// than refusing it with a clear reason).
///
/// Format detection: a `.toml` file is read as the native `[meta]`/
/// `[colors]`/`[fallback]` schema *unless* it has no `[colors]` table but
/// does have at least one `base0`-prefixed top-level key, in which case it's
/// treated as a base16 scheme expressed in TOML. A `.yaml`/`.yml` file is
/// always read as base16 (see [`parse_base16_yaml_map`]'s doc comment for
/// the format this build actually accepts).
///
/// FR-TH-6: every loaded theme is contrast-linted immediately (fg/bg,
/// link/bg, dim/bg — the same pairs [`builtin_contrast_report`] checks);
/// a ratio below `ContrastCheck::AA_THRESHOLD` warns unless `[meta]
/// allow_low_contrast = true`.
pub fn load_user_themes(dir: Option<&Path>) -> (Vec<LoadedUserTheme>, Vec<String>) {
    let mut loaded = Vec::new();
    let mut warnings = Vec::new();
    let Some(dir) = dir else {
        return (loaded, warnings);
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (loaded, warnings);
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        let is_toml = path.extension().is_some_and(|e| e == "toml");
        let is_yaml = path.extension().is_some_and(|e| e == "yaml" || e == "yml");
        if !is_toml && !is_yaml {
            continue;
        }
        let file_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("theme")
            .to_string();
        let Ok(text) = std::fs::read_to_string(&path) else {
            warnings.push(format!("{}: could not read file", path.display()));
            continue;
        };

        let parsed = if is_yaml {
            parse_base16_yaml_map(&text).map(|map| {
                (
                    base16_to_slots(&map, &file_stem),
                    PaletteFallback::default(),
                )
            })
        } else {
            let looks_like_base16 = !text.contains("[colors]")
                && ["base00", "base05", "base0d", "base0D"]
                    .iter()
                    .any(|k| text.to_lowercase().contains(&k.to_lowercase()));
            if looks_like_base16 {
                parse_base16_toml_map(&text).map(|map| {
                    (
                        base16_to_slots(&map, &file_stem),
                        PaletteFallback::default(),
                    )
                })
            } else {
                parse_native_theme_toml(&text, &file_stem)
            }
        };

        let (slots, fallback) = match parsed {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("{}: {e} — skipped", path.display()));
                continue;
            }
        };

        if Theme::by_name(&slots.name).is_some() {
            warnings.push(format!(
                "{}: theme name {:?} collides with a built-in theme — skipped",
                path.display(),
                slots.name
            ));
            continue;
        }

        let theme = expand_slots(&slots);
        let checks = contrast_checks_for(theme.name, &theme);
        if !slots.allow_low_contrast {
            for check in &checks {
                if !check.passes() {
                    warnings.push(format!(
                        "theme {:?}: {} contrast is {:.2}:1, below WCAG AA ({:.1}:1) — set [meta] allow_low_contrast = true to silence",
                        slots.name,
                        check.pair,
                        check.ratio,
                        ContrastCheck::AA_THRESHOLD
                    ));
                }
            }
        }

        loaded.push(LoadedUserTheme {
            name: slots.name.clone(),
            theme,
            fallback,
            allow_low_contrast: slots.allow_low_contrast,
            dark: slots.dark,
            source: path,
        });
    }
    (loaded, warnings)
}

/// Just the names `load_user_themes` would resolve, discarding warnings —
/// used only by config.rs's own `theme`/`theme_light`/`theme_dark`
/// validation (a name-membership pre-check at config-resolve time; the real
/// load, with full diagnostics, happens once at startup in `main`) so a
/// config file's `theme = "myname"` validates against user themes without
/// `config::resolve` needing to accept a pre-loaded theme list as a new
/// parameter.
pub fn user_theme_names(dir: Option<&Path>) -> Vec<String> {
    load_user_themes(dir)
        .0
        .into_iter()
        .map(|t| t.name)
        .collect()
}

/// Resolves a theme name against the built-ins first, then `user_themes` —
/// the "user theme names join `Theme::NAMES`-equivalent resolution"
/// requirement (FR-TH-1), used everywhere a theme is actually *applied*
/// (startup, `:theme`, `:set theme=`, `:config reload`). Deliberately a
/// free function rather than widening `Theme::by_name` itself: `by_name` is
/// exercised by tests and call sites that only ever mean "a built-in," and
/// its being built-ins-only, `'static`-parameter-free lookup is exactly what
/// lets those stay unchanged.
pub fn resolve_named(name: &str, user_themes: &[LoadedUserTheme]) -> Option<Theme> {
    Theme::by_name(name).or_else(|| user_themes.iter().find(|t| t.name == name).map(|t| t.theme))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_name_resolves_and_round_trips() {
        for name in Theme::NAMES {
            let theme = Theme::by_name(name).unwrap_or_else(|| panic!("{name} should resolve"));
            assert_eq!(theme.name, name);
        }
    }

    #[test]
    fn unknown_theme_name_is_rejected() {
        assert!(Theme::by_name("nonexistent").is_none());
    }

    #[test]
    fn cycle_visits_every_theme_once_and_wraps() {
        let mut theme = Theme::terminal();
        let mut seen = vec![theme.name];
        for _ in 0..Theme::NAMES.len() - 1 {
            theme = theme.next();
            seen.push(theme.name);
        }
        assert_eq!(seen, Theme::NAMES.to_vec());
        // One more step should wrap back to the start.
        assert_eq!(theme.next().name, "terminal");
    }

    #[test]
    fn terminal_theme_never_sets_background_or_foreground() {
        let theme = Theme::terminal();
        assert_eq!(
            theme.bg, None,
            "terminal must inherit the user's background"
        );
        assert_eq!(
            theme.fg, None,
            "terminal must inherit the user's foreground"
        );
    }

    /// Locks the exact hex values from PRD Appendix C so a typo doesn't
    /// silently ship a slightly-wrong color.
    #[test]
    fn matches_prd_appendix_c_hex_values() {
        assert_eq!(Theme::full().bg, Some(rgb(0x101418)));
        assert_eq!(Theme::full().fg, Some(rgb(0xd8dee9)));
        assert_eq!(Theme::full().link, rgb(0x6fb3ff));
        assert_eq!(Theme::full().link_visited, rgb(0x9d8cff));
        assert_eq!(Theme::full().infobox, rgb(0x2e3440));

        assert_eq!(Theme::homebrew().bg, Some(rgb(0x000000)));
        assert_eq!(Theme::homebrew().fg, Some(rgb(0x33ff33)));
        assert_eq!(Theme::homebrew().heading, rgb(0x66ff66));
        assert_eq!(Theme::homebrew().link, rgb(0x99ff99));

        assert_eq!(Theme::night().bg, Some(rgb(0x000000)));
        assert_eq!(Theme::night().fg, Some(rgb(0xff2b2b)));
        assert_eq!(Theme::night().heading, rgb(0xff5555));
        assert_eq!(Theme::night().link, rgb(0xff8080));

        assert_eq!(Theme::paper().bg, Some(rgb(0xf5f0e1)));
        assert_eq!(Theme::paper().fg, Some(rgb(0x3a3a3a)));
        assert_eq!(Theme::paper().heading, rgb(0x1a1a1a));
        assert_eq!(Theme::paper().link, rgb(0x1a5276));

        // Appendix C's theme-file schema excerpt literally writes
        // `match = "#ebcb8b"` under `[colors]` — `full` is the truecolor
        // reference theme, so it gets that exact value.
        assert_eq!(Theme::full().match_fg, rgb(0xebcb8b));
    }

    /// Every built-in theme must give `match_fg` a color that's actually
    /// distinct from its body/link/heading colors — otherwise a highlighted
    /// find match would be invisible against its own surroundings.
    #[test]
    fn match_color_is_distinct_from_body_link_and_heading_in_every_theme() {
        for name in Theme::NAMES {
            let theme = Theme::by_name(name).unwrap();
            assert_ne!(
                theme.match_fg, theme.link,
                "{name}: match color collides with link color"
            );
            assert_ne!(
                theme.match_fg, theme.heading,
                "{name}: match color collides with heading color"
            );
            if let Some(fg) = theme.fg {
                assert_ne!(
                    theme.match_fg, fg,
                    "{name}: match color collides with body fg"
                );
            }
        }
    }

    /// The night-red contrast claim from the PRD ("pure red/black ≈
    /// 5.25:1") only holds if the red channel is kept near full brightness,
    /// as the PRD requires ("held near full brightness... never dimming").
    /// `builtin_contrast_report` (FR-TH-6) recomputes the real ratio from
    /// whatever `fg` currently is; this test guards the brightness
    /// precondition directly so a future edit can't quietly dim the red
    /// and drop the ratio below the AA line that claim depends on.
    #[test]
    fn night_theme_keeps_red_near_full_brightness() {
        let Color::Rgb(r, _, _) = Theme::night().fg.unwrap() else {
            panic!("night fg should be Rgb")
        };
        assert!(
            r >= 0xf0,
            "night theme's red channel must stay near full brightness for its AA contrast claim"
        );
    }

    #[test]
    fn images_flag_only_set_for_full_theme() {
        assert!(Theme::full().images);
        for name in Theme::NAMES.iter().filter(|n| **n != "full") {
            assert!(
                !Theme::by_name(name).unwrap().images,
                "{name} should not claim image support yet"
            );
        }
    }

    /// Locks the doctor's headline example (PRD §6.7 verification target):
    /// paper's fg/bg (#3a3a3a on #f5f0e1) clears FR-TH-6's 4.5:1 bar.
    #[test]
    fn known_good_pair_passes_contrast_lint() {
        let ratio = contrast_ratio((0x3a, 0x3a, 0x3a), (0xf5, 0xf0, 0xe1));
        assert!(ratio >= ContrastCheck::AA_THRESHOLD, "ratio was {ratio}");
    }

    /// A deliberately bad pair — two nearly-identical mid-grays — must fail
    /// the same lint, so the "known good" test above isn't just tautology.
    #[test]
    fn deliberately_similar_pair_fails_contrast_lint() {
        let ratio = contrast_ratio((0x77, 0x77, 0x77), (0x88, 0x88, 0x88));
        assert!(ratio < ContrastCheck::AA_THRESHOLD, "ratio was {ratio}");
    }

    #[test]
    fn contrast_ratio_is_symmetric_and_bottoms_out_at_one() {
        let a = (0x10, 0x20, 0x30);
        let b = (0xe0, 0xd0, 0xc0);
        assert_eq!(contrast_ratio(a, b), contrast_ratio(b, a));
        assert!((contrast_ratio(a, a) - 1.0).abs() < 1e-9);
    }

    /// `terminal` never sets bg/fg and `contrast` is defined in named ANSI
    /// colors, not RGB — both are meaningfully absent from the report
    /// rather than silently reported as passing.
    #[test]
    fn contrast_report_only_covers_rgb_defined_themes() {
        let report = builtin_contrast_report();
        let themes_covered: std::collections::BTreeSet<&str> =
            report.iter().map(|c| c.theme).collect();
        assert_eq!(
            themes_covered,
            ["full", "homebrew", "night", "paper"].into_iter().collect()
        );
        // Each covered theme reports exactly its three pairs.
        for name in ["full", "homebrew", "night", "paper"] {
            let pairs: std::collections::BTreeSet<&str> = report
                .iter()
                .filter(|c| c.theme == name)
                .map(|c| c.pair)
                .collect();
            assert_eq!(pairs, ["fg/bg", "link/bg", "dim/bg"].into_iter().collect());
        }
    }

    /// Locks the two known findings this feature surfaced: `night`'s fg/bg
    /// clears FR-TH-6's bar but only into AA (Appendix C's documented
    /// tradeoff, not a bug), while its `dim` — deliberately low-contrast,
    /// being the de-emphasized slot — genuinely fails the same bar. Both
    /// are expected; neither should be "fixed" by editing the theme.
    #[test]
    fn night_fg_is_aa_only_and_night_dim_fails() {
        let report = builtin_contrast_report();
        let find = |pair| {
            report
                .iter()
                .find(|c| c.theme == "night" && c.pair == pair)
                .unwrap()
        };
        let fg = find("fg/bg");
        assert!(fg.passes(), "night fg/bg ratio was {}", fg.ratio);
        assert_eq!(fg.level(), "AA");
        let dim = find("dim/bg");
        assert!(
            !dim.passes(),
            "expected night dim/bg to fail, was {}",
            dim.ratio
        );
        assert_eq!(dim.level(), "FAIL");
    }

    // -- FR-TH-3: capability degradation --

    #[test]
    fn color_depth_parses_the_closed_set_and_rejects_junk() {
        assert_eq!(ColorDepth::parse("truecolor"), Some(ColorDepth::Truecolor));
        assert_eq!(ColorDepth::parse("256"), Some(ColorDepth::TwoFiftySix));
        assert_eq!(ColorDepth::parse("16"), Some(ColorDepth::Sixteen));
        assert_eq!(ColorDepth::parse("mono"), Some(ColorDepth::Mono));
        assert_eq!(ColorDepth::parse("auto"), None, "auto needs env access");
        assert_eq!(ColorDepth::parse("bogus"), None);
    }

    /// Exercises every branch of FR-TH-3's `auto` detection as a pure
    /// function of the two env strings, avoiding real env-var mutation (see
    /// `classify_color_depth`'s own doc comment for why).
    #[test]
    fn classify_color_depth_covers_every_branch() {
        assert_eq!(classify_color_depth("dumb", ""), ColorDepth::Mono);
        assert_eq!(classify_color_depth("dumb", "truecolor"), ColorDepth::Mono);
        assert_eq!(
            classify_color_depth("xterm-256color", "truecolor"),
            ColorDepth::Truecolor
        );
        assert_eq!(
            classify_color_depth("screen", "24bit"),
            ColorDepth::Truecolor
        );
        assert_eq!(
            classify_color_depth("xterm-256color", ""),
            ColorDepth::TwoFiftySix
        );
        assert_eq!(classify_color_depth("xterm", ""), ColorDepth::Sixteen);
        assert_eq!(classify_color_depth("", ""), ColorDepth::Sixteen);
    }

    #[test]
    fn resolve_color_depth_treats_explicit_values_as_an_override() {
        assert_eq!(resolve_color_depth("mono"), ColorDepth::Mono);
        assert_eq!(resolve_color_depth("16"), ColorDepth::Sixteen);
        assert_eq!(resolve_color_depth("256"), ColorDepth::TwoFiftySix);
        assert_eq!(resolve_color_depth("truecolor"), ColorDepth::Truecolor);
    }

    /// Pure red round-trips to itself at truecolor, to xterm256's
    /// well-known "red" cube index 196, and to the nearest of the 16
    /// standard ANSI colors — which, at exactly (255,0,0), is *bright* red
    /// (0,0,0 distance) rather than the dimmer standard "red" (205,0,0) —
    /// matching how real terminal-color-quantization tools round pure red.
    #[test]
    fn pure_red_quantizes_to_the_expected_index_and_ansi16_slot() {
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        assert_eq!(rgb_to_16(255, 0, 0), Color::LightRed);
    }

    /// A known mid-grey (#808080) lands on xterm256's grayscale ramp at the
    /// well-known "grey58" index 244, not the RGB cube (its nearest cube
    /// entry, index 102, is a worse match).
    #[test]
    fn mid_grey_quantizes_to_the_grayscale_ramp_not_the_cube() {
        assert_eq!(rgb_to_256(0x80, 0x80, 0x80), 244);
    }

    /// Pure black and pure white — the boundary cases of both palettes.
    #[test]
    fn black_and_white_quantize_to_their_own_boundary_slots() {
        assert_eq!(rgb_to_256(0, 0, 0), 16); // the cube's own (0,0,0) corner
        assert_eq!(rgb_to_256(255, 255, 255), 231); // the cube's (5,5,5) corner
        assert_eq!(rgb_to_16(0, 0, 0), Color::Black);
        assert_eq!(rgb_to_16(255, 255, 255), Color::White);
    }

    #[test]
    fn adapt_color_passes_truecolor_through_unchanged() {
        let c = Color::Rgb(0x6f, 0xb3, 0xff);
        assert_eq!(adapt_color(c, ColorDepth::Truecolor, None), c);
    }

    #[test]
    fn adapt_color_at_mono_strips_to_reset_regardless_of_input() {
        assert_eq!(
            adapt_color(Color::Rgb(255, 0, 0), ColorDepth::Mono, None),
            Color::Reset
        );
        assert_eq!(
            adapt_color(Color::Blue, ColorDepth::Mono, None),
            Color::Reset
        );
    }

    #[test]
    fn adapt_color_named_ansi_colors_pass_through_at_every_non_mono_depth() {
        for depth in [
            ColorDepth::Truecolor,
            ColorDepth::TwoFiftySix,
            ColorDepth::Sixteen,
        ] {
            assert_eq!(adapt_color(Color::Blue, depth, None), Color::Blue);
        }
    }

    #[test]
    fn adapt_color_computes_quantization_when_no_fallback_declared() {
        let c = Color::Rgb(255, 0, 0);
        assert_eq!(
            adapt_color(c, ColorDepth::TwoFiftySix, None),
            Color::Indexed(196)
        );
        assert_eq!(adapt_color(c, ColorDepth::Sixteen, None), Color::LightRed);
    }

    /// Deliverable 2: a declared per-slot fallback wins over computed
    /// quantization at 256 and 16 depth.
    #[test]
    fn declared_fallback_takes_precedence_over_computed_quantization() {
        let c = Color::Rgb(255, 0, 0); // would otherwise compute to 196 / LightRed
        assert_eq!(
            adapt_color(c, ColorDepth::TwoFiftySix, Some(Color::Indexed(9))),
            Color::Indexed(9)
        );
        assert_eq!(
            adapt_color(c, ColorDepth::Sixteen, Some(Color::Red)),
            Color::Red
        );
    }

    #[test]
    fn theme_adapt_preserves_name_and_images_and_degrades_every_color() {
        let full = Theme::full();
        let mono = full.adapt(ColorDepth::Mono, None);
        assert_eq!(mono.name, "full");
        assert_eq!(mono.images, full.images);
        assert_eq!(mono.bg, Some(Color::Reset));
        assert_eq!(mono.fg, Some(Color::Reset));
        assert_eq!(mono.link, Color::Reset);
        assert_eq!(mono.heading, Color::Reset);

        let sixteen = full.adapt(ColorDepth::Sixteen, None);
        assert_ne!(sixteen.link, full.link, "should have quantized");
        assert!(!matches!(sixteen.link, Color::Rgb(..)));

        let two56 = full.adapt(ColorDepth::TwoFiftySix, None);
        assert!(matches!(two56.link, Color::Indexed(_)));
    }

    /// A theme's own declared `[fallback]` (via `PaletteFallback`) takes
    /// precedence over `Theme::adapt`'s computed quantization, end to end.
    #[test]
    fn theme_adapt_honors_declared_palette_fallback() {
        let full = Theme::full();
        let mut fallback = PaletteFallback::default();
        fallback.palette256.insert("bg".to_string(), 234);
        fallback.palette16.insert("bg".to_string(), Color::Black);
        let two56 = full.adapt(ColorDepth::TwoFiftySix, Some(&fallback));
        assert_eq!(two56.bg, Some(Color::Indexed(234)));
        let sixteen = full.adapt(ColorDepth::Sixteen, Some(&fallback));
        assert_eq!(sixteen.bg, Some(Color::Black));
    }

    // -- FR-TH-1: user theme files --

    const MINIMAL_THEME_TOML: &str = r##"
        [meta]
        name = "solar"

        [colors]
        bg = "#222222"
        fg = "#eeeeee"
        link = "#4488ff"
    "##;

    const FULL_THEME_TOML: &str = r##"
        [meta]
        name = "mytheme"
        dark = true
        images = false

        [colors]
        bg = "#101418"
        fg = "#d8dee9"
        accent = "#6fb3ff"
        link = "#6fb3ff"
        link_visited = "#9d8cff"
        heading = "#eceff4"
        quote = "#a3be8c"
        code_bg = "#161b22"
        match = "#ebcb8b"
        warning = "#d08770"
        error = "#bf616a"
        dim = "#4c566a"

        [fallback]
        palette256 = { bg = 234, fg = 253, accent = 111 }
        palette16  = { bg = "black", fg = "white", accent = "blue" }
    "##;

    #[test]
    fn parses_a_full_theme_file_into_every_declared_slot() {
        let (slots, fallback) = parse_native_theme_toml(FULL_THEME_TOML, "mytheme").unwrap();
        assert_eq!(slots.name, "mytheme");
        assert!(slots.dark);
        assert!(!slots.images);
        assert_eq!(slots.bg, Some(Color::Rgb(0x10, 0x14, 0x18)));
        assert_eq!(slots.fg, Some(Color::Rgb(0xd8, 0xde, 0xe9)));
        assert_eq!(slots.accent, Some(Color::Rgb(0x6f, 0xb3, 0xff)));
        assert_eq!(slots.match_fg, Some(Color::Rgb(0xeb, 0xcb, 0x8b)));
        assert_eq!(slots.warning, Some(Color::Rgb(0xd0, 0x87, 0x70)));
        assert_eq!(slots.error, Some(Color::Rgb(0xbf, 0x61, 0x6a)));
        assert_eq!(slots.dim, Some(Color::Rgb(0x4c, 0x56, 0x6a)));

        let theme = expand_slots(&slots);
        assert_eq!(theme.name, "mytheme");
        assert_eq!(theme.warning, Color::Rgb(0xd0, 0x87, 0x70));
        assert_eq!(theme.error, Color::Rgb(0xbf, 0x61, 0x6a));

        assert_eq!(fallback.palette256.get("bg"), Some(&234));
        assert_eq!(fallback.palette16.get("accent"), Some(&Color::Blue));
    }

    /// Deliverable 1's headline case: a minimal file naming only bg/fg/link
    /// still produces a complete, usable `Theme` via derivation — nothing
    /// panics and every slot ends up with *some* color.
    #[test]
    fn minimal_theme_file_derives_every_missing_slot() {
        let (slots, _) = parse_native_theme_toml(MINIMAL_THEME_TOML, "solar").unwrap();
        assert_eq!(slots.link, Some(Color::Rgb(0x44, 0x88, 0xff)));
        assert_eq!(slots.heading, None, "not given — must be derived");

        let theme = expand_slots(&slots);
        assert_eq!(theme.name, "solar");
        assert_eq!(theme.bg, Some(Color::Rgb(0x22, 0x22, 0x22)));
        assert_eq!(theme.link, Color::Rgb(0x44, 0x88, 0xff));
        // Derived, not left as some sentinel/default-looking value:
        assert_eq!(
            theme.heading, theme.link,
            "heading derives from accent/link"
        );
        assert_ne!(theme.dim, theme.fg.unwrap(), "dim must differ from body fg");
        assert_eq!(theme.table, theme.fg.unwrap());
    }

    #[test]
    fn bad_theme_toml_is_a_parse_error_not_a_panic() {
        let result = parse_native_theme_toml("this is not [ valid toml", "broken");
        assert!(result.is_err());
    }

    #[test]
    fn parse_hex_color_accepts_hash_and_bare_forms() {
        assert_eq!(parse_hex_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_hex_color("ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_hex_color("not-a-color"), None);
    }

    // -- FR-TH-1: base16 import --

    const BASE16_SOLARIZED_DARK: &str = "\
scheme: Solarized Dark
author: Ethan Schoonover
base00: 002b36
base01: 073642
base02: 586e75
base03: 657b83
base04: 839496
base05: 93a1a1
base06: eee8d5
base07: fdf6e3
base08: dc322f
base09: cb4b16
base0A: b58900
base0B: 859900
base0C: 2aa198
base0D: 268bd2
base0E: 6c71c4
base0F: d33682
";

    #[test]
    fn base16_yaml_map_parses_the_flat_scalar_scheme() {
        let map = parse_base16_yaml_map(BASE16_SOLARIZED_DARK).unwrap();
        assert_eq!(
            map.get("scheme").map(String::as_str),
            Some("Solarized Dark")
        );
        assert_eq!(map.get("base00").map(String::as_str), Some("002b36"));
        assert_eq!(map.get("base0d").map(String::as_str), Some("268bd2"));
    }

    /// The documented base16 -> wikitui slot mapping: base00 -> bg, base05
    /// -> fg, base0D -> link/heading, base0A -> match, base08 -> error.
    #[test]
    fn base16_scheme_maps_onto_the_documented_semantic_slots() {
        let map = parse_base16_yaml_map(BASE16_SOLARIZED_DARK).unwrap();
        let slots = base16_to_slots(&map, "solarized-fallback-name");
        assert_eq!(slots.name, "solarized-dark", "slugified for :theme use");
        assert_eq!(slots.bg, Some(Color::Rgb(0x00, 0x2b, 0x36)));
        assert_eq!(slots.fg, Some(Color::Rgb(0x93, 0xa1, 0xa1)));
        assert_eq!(slots.link, Some(Color::Rgb(0x26, 0x8b, 0xd2)));
        assert_eq!(slots.heading, slots.link);
        assert_eq!(slots.match_fg, Some(Color::Rgb(0xb5, 0x89, 0x00)));
        assert_eq!(slots.error, Some(Color::Rgb(0xdc, 0x32, 0x2f)));
        assert_eq!(slots.dim, Some(Color::Rgb(0x65, 0x7b, 0x83)));

        let theme = expand_slots(&slots);
        assert_eq!(theme.bg, slots.bg);
        assert_eq!(theme.link, slots.link.unwrap());
    }

    #[test]
    fn base16_yaml_map_rejects_a_line_that_is_not_key_value() {
        assert!(parse_base16_yaml_map("scheme: ok\nthis has no colon at all").is_err());
    }

    #[test]
    fn base16_toml_map_reads_flat_base0x_keys() {
        let text = "scheme = \"Test\"\nbase00 = \"111111\"\nbase0d = \"222222\"\n";
        let map = parse_base16_toml_map(text).unwrap();
        assert_eq!(map.get("base00").map(String::as_str), Some("111111"));
        let slots = base16_to_slots(&map, "test");
        assert_eq!(slots.bg, Some(Color::Rgb(0x11, 0x11, 0x11)));
    }

    // -- FR-TH-1 / FR-TH-6: the config-directory loader --

    fn write_theme_file(dir: &Path, filename: &str, contents: &str) {
        std::fs::write(dir.join(filename), contents).unwrap();
    }

    /// A base16 scheme expressed as flat TOML (`themes/*.toml` with no
    /// `[colors]` table but `base0x` keys) is auto-detected and imported the
    /// same way a `.yaml` file would be.
    #[test]
    fn load_user_themes_detects_a_base16_scheme_written_as_toml() {
        let tmp = std::env::temp_dir().join(format!(
            "wikitui-theme-test-{}-{}",
            std::process::id(),
            "base16-toml"
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        write_theme_file(
            &tmp,
            "gruvbox.toml",
            "scheme = \"Gruvbox\"\nbase00 = \"282828\"\nbase05 = \"ebdbb2\"\nbase0d = \"458588\"\n",
        );
        let (loaded, warnings) = load_user_themes(Some(&tmp));
        assert_eq!(loaded.len(), 1, "warnings: {warnings:?}");
        let gruvbox = &loaded[0];
        assert_eq!(gruvbox.name, "gruvbox");
        assert_eq!(gruvbox.theme.bg, Some(Color::Rgb(0x28, 0x28, 0x28)));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_user_themes_parses_good_files_and_skips_and_warns_on_bad_ones() {
        let tmp = std::env::temp_dir().join(format!(
            "wikitui-theme-test-{}-{}",
            std::process::id(),
            "good-and-bad"
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        write_theme_file(&tmp, "solar.toml", MINIMAL_THEME_TOML);
        write_theme_file(&tmp, "broken.toml", "not [ toml at all");
        write_theme_file(&tmp, "solarized.yaml", BASE16_SOLARIZED_DARK);

        let (loaded, warnings) = load_user_themes(Some(&tmp));
        assert_eq!(loaded.len(), 2, "solar + solarized parsed: {loaded:#?}");
        assert!(loaded.iter().any(|t| t.name == "solar"));
        assert!(loaded.iter().any(|t| t.name == "solarized-dark"));
        assert!(
            warnings.iter().any(|w| w.contains("broken.toml")),
            "broken.toml should warn: {warnings:?}"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_user_themes_skips_a_name_colliding_with_a_builtin() {
        let tmp = std::env::temp_dir().join(format!(
            "wikitui-theme-test-{}-{}",
            std::process::id(),
            "collision"
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        write_theme_file(
            &tmp,
            "paper2.toml",
            "[meta]\nname = \"paper\"\n[colors]\nbg = \"#ffffff\"\nfg = \"#000000\"\nlink = \"#0000ff\"\n",
        );
        let (loaded, warnings) = load_user_themes(Some(&tmp));
        assert!(loaded.is_empty());
        assert!(warnings.iter().any(|w| w.contains("collides")));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_user_themes_warns_on_low_contrast_unless_allowed() {
        let tmp = std::env::temp_dir().join(format!(
            "wikitui-theme-test-{}-{}",
            std::process::id(),
            "contrast"
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        // Near-identical mid-greys: fails WCAG AA (mirrors theme.rs's own
        // `deliberately_similar_pair_fails_contrast_lint`).
        write_theme_file(
            &tmp,
            "murky.toml",
            "[meta]\nname = \"murky\"\n[colors]\nbg = \"#888888\"\nfg = \"#777777\"\nlink = \"#4488ff\"\n",
        );
        let (loaded, warnings) = load_user_themes(Some(&tmp));
        assert_eq!(loaded.len(), 1);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("murky") && w.contains("contrast")),
            "low-contrast theme should warn: {warnings:?}"
        );

        write_theme_file(
            &tmp,
            "murky2.toml",
            "[meta]\nname = \"murky2\"\nallow_low_contrast = true\n[colors]\nbg = \"#888888\"\nfg = \"#777777\"\nlink = \"#4488ff\"\n",
        );
        let (_, warnings2) = load_user_themes(Some(&tmp));
        assert!(
            !warnings2.iter().any(|w| w.contains("murky2")),
            "allow_low_contrast should silence the warning: {warnings2:?}"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn resolve_named_checks_builtins_first_then_user_themes() {
        let (loaded, _) = {
            let tmp = std::env::temp_dir().join(format!(
                "wikitui-theme-test-{}-{}",
                std::process::id(),
                "resolve-named"
            ));
            std::fs::create_dir_all(&tmp).unwrap();
            write_theme_file(&tmp, "solar.toml", MINIMAL_THEME_TOML);
            let result = load_user_themes(Some(&tmp));
            std::fs::remove_dir_all(&tmp).ok();
            result
        };
        assert_eq!(resolve_named("paper", &loaded).unwrap().name, "paper");
        assert_eq!(resolve_named("solar", &loaded).unwrap().name, "solar");
        assert!(resolve_named("nonexistent", &loaded).is_none());
    }

    #[test]
    fn user_contrast_report_covers_loaded_user_themes() {
        let user_themes = load_user_themes(None).0;
        assert!(user_contrast_report(&user_themes).is_empty());
    }
}
