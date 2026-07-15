//! Talk-page title derivation (PRD FR-ACC-5): an article's talk page is
//! just another ordinary page titled `Talk:{title}` — fetched, cached, and
//! rendered through the exact same load path as any article
//! (`main::follow_internal_link`/`open_title`). There is no separate
//! "discussion view"; the document model, layout, folding, search, and
//! offline caching all apply unchanged.
//!
//! **v1.0 seam**: the `Talk:` namespace name is hardcoded here rather than
//! resolved from the wiki's `siteinfo` (which would give the localized
//! name — e.g. dewiki's `Diskussion:`). This is correct for enwiki and the
//! mock server; a non-English wiki's talk pages won't round-trip through
//! this module until a later chunk reads the real namespace name. FR-ML-5's
//! per-wiki degradation matrix already documents this class of
//! simplification as an accepted v1.0 gap, not an oversight.

/// The (hardcoded, v1.0) talk-namespace prefix — see the module doc comment.
pub const TALK_PREFIX: &str = "Talk:";

/// The talk-page title for `article_title` — the Article → Talk direction
/// of the `T` / `:talk` toggle.
pub fn to_talk(article_title: &str) -> String {
    format!("{TALK_PREFIX}{article_title}")
}

/// The article title `title` is the talk page of, or `None` if `title`
/// isn't already a talk page — the Talk → Article direction of the toggle.
pub fn from_talk(title: &str) -> Option<&str> {
    title.strip_prefix(TALK_PREFIX)
}

/// What the talk toggle should open next, given whatever title is
/// currently on screen: strip `Talk:` if it's already there, add it if not.
/// A pure function of the current title (mirrors `app::resolve_hint_action`'s
/// shape) so the toggle logic is testable without touching the tab/network
/// machinery — `main::toggle_talk_page` is the thin, untestable-by-itself
/// wrapper that feeds this into the ordinary link-follow path.
pub fn toggle_target(current_title: &str) -> String {
    match from_talk(current_title) {
        Some(article) => article.to_string(),
        None => to_talk(current_title),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_talk_prepends_the_prefix() {
        assert_eq!(to_talk("Alan Turing"), "Talk:Alan Turing");
    }

    #[test]
    fn from_talk_strips_the_prefix_only_when_present() {
        assert_eq!(from_talk("Talk:Alan Turing"), Some("Alan Turing"));
        assert_eq!(from_talk("Alan Turing"), None);
    }

    #[test]
    fn from_talk_does_not_match_a_title_that_merely_contains_the_word() {
        // "Talk:" must be a genuine namespace prefix, not a substring match —
        // a real article titled e.g. "Talking Heads" must never be treated
        // as somebody else's talk page.
        assert_eq!(from_talk("Talking Heads"), None);
    }

    #[test]
    fn toggle_target_flips_article_to_talk_and_back() {
        assert_eq!(toggle_target("Alan Turing"), "Talk:Alan Turing");
        assert_eq!(toggle_target("Talk:Alan Turing"), "Alan Turing");
    }

    #[test]
    fn toggle_target_round_trips() {
        let article = "Enigma machine";
        let talk = toggle_target(article);
        let back = toggle_target(&talk);
        assert_eq!(back, article);
    }
}
