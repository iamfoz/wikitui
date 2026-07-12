//! Built-in color themes (PRD §5.11, Appendix C). Six named presets:
//! `terminal` (the safe default, inherits the user's palette), `full`
//! (truecolor + images), `homebrew` (phosphor green on black), `night` (red
//! on black), `paper` (dark grey on cream), and `contrast` (high-contrast
//! accessibility). Truecolor (`Color::Rgb`) values only for now — the
//! 256-/16-color fallback table promised by FR-TH-3 is follow-up work.

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
    /// `T` keybinding to cycle themes for live preview (PRD FR-TH-2's
    /// runtime-switching requirement; the `:theme <name>` command syntax
    /// arrives with the command system, FR-CS-2).
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

fn relative_luminance((r, g, b): (u8, u8, u8)) -> f64 {
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

/// FR-TH-6's contrast lint: fg/bg, link/bg, and dim/bg for every built-in
/// theme whose relevant slots are RGB-defined (`terminal` has no
/// bg/fg to check; `contrast` is defined in named ANSI colors, not RGB —
/// both are correctly absent from this report, not silently passing it).
pub fn builtin_contrast_report() -> Vec<ContrastCheck> {
    let mut out = Vec::new();
    for name in Theme::NAMES {
        let theme = Theme::by_name(name).expect("NAMES only lists valid themes");
        let Some(bg) = theme.bg.and_then(as_rgb) else {
            continue;
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
    }
    out
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
}
