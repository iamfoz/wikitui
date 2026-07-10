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
    }

    /// The night-red contrast claim from the PRD ("pure red/black ≈
    /// 5.25:1") only holds if the red channel is kept near full brightness,
    /// as the PRD requires ("held near full brightness... never dimming").
    /// This doesn't recompute WCAG luminance (that's the theme-lint feature,
    /// FR-TH-6, not yet built) — it just guards against someone quietly
    /// dimming the red and silently breaking that documented guarantee.
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
}
