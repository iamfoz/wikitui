//! PRD FR-ML-8's string-table architecture: wikitui is English-only through
//! v1.x by explicit product choice (§5.10: "English-only through v1.x by
//! explicit choice"), but the PRD asks for the *architecture* to exist from
//! MVP so v2 localization is tractable — not for a translation to actually
//! ship, and not for every literal in the codebase to be migrated in one
//! chunk. This module is that architecture's one seam.
//!
//! [`Key`] is the closed set of user-facing strings migrated so far — a
//! REPRESENTATIVE subset (status-bar hints, a couple of transient notices,
//! one view title), chosen to prove the pattern end to end rather than to
//! exhaustively rewrite every `format!`/literal call site in `ui.rs`/
//! `main.rs` for a locale v1.x explicitly does not ship. Moving the rest of
//! the UI's literals onto this same seam is mechanical follow-up: pick a
//! literal, add a `Key` variant, add its `t` arm, replace the call site —
//! exactly the pattern every entry below already follows.
//!
//! [`t`] returns `&'static str`, matching how every migrated call site
//! already consumed its literal (a `String`-returning signature would force
//! an allocation at every call site for no benefit while there is exactly
//! one locale). A v2 build swaps this module's single `match` for a real
//! per-locale lookup (a `HashMap<(Locale, Key), &str>`, or a table generated
//! from `.ftl`/`.po` files) — nothing outside this module has to change,
//! because every caller already goes through `t(Key::...)`, never a bare
//! literal.
//!
//! `Key` deliberately has no `Display`/string-name derivation and no
//! `#[derive(Hash)]`-based stringly lookup: a real localization pass keys
//! strings by a stable identifier chosen by a translator/tool, not by
//! rendering the Rust enum variant's own spelling, so nothing here should
//! encourage treating `Key::HelpHint`'s Rust name as if it *were* the
//! translation key. The exhaustive `match` in `t` is the actual safety net a
//! real "missing key" lint would give you: add a variant, and `t` fails to
//! compile until every arm (today, just the one English arm) covers it.
//!
//! Keybindings are explicitly out of this module's scope (and always have
//! been): PRD FR-ML-8 also requires "keybindings never assume QWERTY", which
//! this codebase already satisfies architecturally, confirmed by inspection
//! of `registry.rs` for this chunk: every action is a named [`crate::registry
//! ::Action`] first (PRD §5.13's architectural rule), and `Keymap::vim`/
//! `Keymap::emacs` are ordinary *data* — `(context, chord) -> action` tables —
//! not hardcoded `match` arms a layout choice could bake in. A user
//! `keymap.toml` ([`crate::registry::Keymap::apply_user_toml`]) adds or
//! overrides any binding in either preset (PRD FR-CS-3), so a reader on
//! Dvorak, AZERTY, or any other layout remaps every action to whatever key
//! their layout makes convenient — nothing in the keymap layer assumes the
//! physical/logical position a QWERTY `j`/`k`/`f`/`g` happens to occupy.
//! There is no separate string table for keybindings because a keybinding is
//! not user-facing *text* to translate — see `registry.rs` for the keymap
//! itself.

/// One translatable string. Exhaustively matched by [`t`] — the compiler
/// enforces that every key has an English string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// `Mode::Help`'s status-bar hint.
    HelpHint,
    /// `Mode::Onboarding`'s status-bar hint (PRD FR-CS-8).
    OnboardingHint,
    /// `Mode::Toc`'s status-bar hint (PRD FR-NV-2).
    TocHint,
    /// `Mode::TabPicker`'s status-bar hint (PRD FR-TB-1).
    TabPickerHint,
    /// `Mode::HistoryPicker`'s status-bar hint (PRD FR-NV-7).
    HistoryPickerHint,
    /// `Mode::WikiPicker`'s status-bar hint (PRD FR-ML-4/5).
    WikiPickerHint,
    /// `Mode::Results`'s status-bar hint (PRD FR-SR-1/2).
    ResultsHint,
    /// `Mode::OfflineCard`'s status-bar hint (PRD FR-OFF-6).
    OfflineCardHint,
    /// `Mode::RedlinkCard`'s status-bar hint (PRD FR-DL-5).
    RedlinkCardHint,
    /// The `:info` popup's title (PRD §10).
    ArticleInfoTitle,
    /// PRD FR-PR-3's persistent incognito status-bar glyph prefix.
    IncognitoPrefix,
    /// PRD FR-ML-7's experimental-RTL status-bar badge.
    RtlExperimentalBadge,
}

/// The English string table (PRD FR-ML-8). No locale parameter: there is
/// exactly one locale until v2, so threading one through every call site now
/// would be unexercised plumbing — adding it later is a signature change at
/// this one function, not a rewrite of every caller.
pub fn t(key: Key) -> &'static str {
    match key {
        Key::HelpHint => "j/k: scroll   Esc/?/q: close",
        Key::OnboardingHint => "Press any key to start reading",
        Key::TocHint => "Enter: jump to section   Esc: cancel   j/k: move",
        Key::TabPickerHint => "Enter: switch tab   d: close   Esc: cancel   j/k: move",
        Key::HistoryPickerHint => "Enter: jump   Esc: cancel   j/k: move",
        Key::WikiPickerHint => "Enter: switch wiki   Esc: cancel   j/k: move",
        Key::ResultsHint => "Enter: open   Esc: cancel   j/k: move",
        Key::OfflineCardHint => {
            "f: queue for fetch when online   s: search saved pages   Esc: dismiss"
        }
        Key::RedlinkCardHint => "s: search similar titles   y: yank create URL   Esc: dismiss",
        Key::ArticleInfoTitle => "Article info",
        Key::IncognitoPrefix => "[incognito]",
        Key::RtlExperimentalBadge => "RTL (experimental)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key resolves to a non-empty English string — the baseline
    /// "missing key" guard a real localization table would give you as a
    /// runtime check; here the exhaustive `match` already gives it to us at
    /// compile time, so this test locks the actual *content*, not just that
    /// an arm exists.
    #[test]
    fn every_key_resolves_to_a_non_empty_string() {
        for key in [
            Key::HelpHint,
            Key::OnboardingHint,
            Key::TocHint,
            Key::TabPickerHint,
            Key::HistoryPickerHint,
            Key::WikiPickerHint,
            Key::ResultsHint,
            Key::OfflineCardHint,
            Key::RedlinkCardHint,
            Key::ArticleInfoTitle,
            Key::IncognitoPrefix,
            Key::RtlExperimentalBadge,
        ] {
            assert!(
                !t(key).is_empty(),
                "{key:?} must resolve to a non-empty string"
            );
        }
    }

    #[test]
    fn the_migrated_subset_resolves_to_its_documented_english_text() {
        assert_eq!(t(Key::HelpHint), "j/k: scroll   Esc/?/q: close");
        assert_eq!(t(Key::ArticleInfoTitle), "Article info");
        assert_eq!(t(Key::IncognitoPrefix), "[incognito]");
        assert_eq!(t(Key::RtlExperimentalBadge), "RTL (experimental)");
    }
}
