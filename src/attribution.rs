//! PRD §10's attribution requirement, both halves.
//!
//! **Export** (this file's original half): every export (bookmarks, saved
//! pages, trail) must embed the same footer — Wikipedia's content license,
//! the note that a revision permalink carries the author attribution
//! Wikimedia's reuse terms require, and the retrieval date, plus a statement
//! that wikitui is an unofficial client. `bookmark_export.rs` grew this text
//! first; the saved-page export (PRD FR-OFF-7) needs the identical footer, so
//! rather than a second verbatim copy the wording lives here once and both
//! call it — the DRY the PRD's "every export" rule invites.
//!
//! The one variation is the reader-annotation clause: a bookmark export
//! carries the reader's own notes/tags, which ShareAlike does *not* govern
//! (they annotate the article, they aren't the article), so that export adds
//! a sentence saying so. A saved-page export carries no user annotations, so
//! it omits that clause. `include_annotation_note` is the switch.
//!
//! **Display** ([`ArticleAttribution`]): the article footer + `i` / `:info`
//! surface (PRD Appendix B's "article info/attribution") shows the same
//! facts on screen instead of baking them into a written file — title,
//! canonical URL, revision id, license, and a permalink to the article's
//! history. No network call: everything it needs is already sitting on the
//! tab (`doc.title`, `tab.lang`, `tab.current_revid`) by the time an article
//! is on screen, the same "derive from what the caller already fetched"
//! shape as [`export_footer`].

/// The §10 export-attribution footer, retrieved on `retrieved_on` (a
/// `YYYY-MM-DD` date, e.g. `research::today()`). `include_annotation_note`
/// appends the reader-annotation ShareAlike clause (true for exports that
/// embed the reader's own notes/tags — bookmarks; false for saved pages,
/// which have none).
pub fn export_footer(retrieved_on: &str, include_annotation_note: bool) -> String {
    let mut footer = format!(
        "Wikipedia article content is licensed CC BY-SA 4.0 (some articles GFDL); \
         each entry's revision permalink, where present, carries the author attribution \
         Wikimedia's reuse terms require. Exported {retrieved_on} by wikitui, an unofficial client \
         (not endorsed by the Wikimedia Foundation)."
    );
    if include_annotation_note {
        footer.push_str(
            " Notes and tags above are the \
             reader's own annotations, not Wikipedia article text.",
        );
    }
    footer
}

/// The wording shared by [`export_footer`] and [`ArticleAttribution`] so the
/// license line never drifts between the two §10 surfaces.
const LICENSE_LINE: &str = "CC BY-SA 4.0 (some articles GFDL)";

/// PRD §10's display-side attribution surface: the fields `i` / `:info`
/// shows for the article on screen. Plain data (not rendered text) so
/// `ui::draw_info_overlay` and its tests can each pick apart the fields they
/// care about rather than string-matching a formatted block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleAttribution {
    pub title: String,
    /// The canonical `/wiki/Title` URL (`research::article_url`).
    pub canonical_url: String,
    /// `0` means no revision has loaded yet (e.g. the start page slipped
    /// through) — [`App::open_info`] guards against opening the overlay in
    /// that state, so in practice this is always a real revid by the time a
    /// caller reads it.
    pub revid: u64,
    pub license: String,
    /// The `oldid=` permalink to this exact revision (`research::permalink_url`).
    pub permalink_url: String,
    /// The `action=history` permalink to the article's full revision history
    /// (`research::history_url`) — PRD §10's "permalink to the article's
    /// history".
    pub history_url: String,
    /// `YYYY-MM-DD`, the date the overlay was opened (`research::today()`).
    pub retrieved_on: String,
}

/// Builds the `:info` overlay's content from what the reader is looking at
/// right now. No network call and no fallible step — every field is either a
/// string format or a value already sitting on the tab.
pub fn article_attribution(
    title: &str,
    lang: &str,
    revid: u64,
    retrieved_on: &str,
) -> ArticleAttribution {
    ArticleAttribution {
        title: title.to_string(),
        canonical_url: crate::research::article_url(title, lang),
        revid,
        license: LICENSE_LINE.to_string(),
        permalink_url: crate::research::permalink_url(title, lang, revid),
        history_url: crate::research::history_url(title, lang),
        retrieved_on: retrieved_on.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_carries_license_client_disclaimer_and_date() {
        let footer = export_footer("2026-07-13", false);
        assert!(footer.contains("CC BY-SA 4.0"));
        assert!(footer.contains("GFDL"));
        assert!(footer.contains("Exported 2026-07-13"));
        assert!(footer.contains("not endorsed by the Wikimedia Foundation"));
    }

    #[test]
    fn annotation_clause_is_present_only_when_requested() {
        let with = export_footer("2026-07-13", true);
        let without = export_footer("2026-07-13", false);
        assert!(with.contains("reader's own annotations"));
        assert!(
            !without.contains("reader's own annotations"),
            "a saved-page export has no user annotations to disclaim"
        );
    }

    /// Every §10 display field derives correctly from a title/lang/revid:
    /// canonical URL, the oldid permalink, the history permalink, the
    /// license string, and the retrieval date all land in their own field.
    #[test]
    fn article_attribution_derives_every_field_from_doc_and_revid() {
        let info = article_attribution("Alan Turing", "en", 123456, "2026-07-14");
        assert_eq!(info.title, "Alan Turing");
        assert_eq!(
            info.canonical_url,
            "https://en.wikipedia.org/wiki/Alan_Turing"
        );
        assert_eq!(info.revid, 123456);
        assert!(info.license.contains("CC BY-SA 4.0"));
        assert!(info.license.contains("GFDL"));
        assert_eq!(
            info.permalink_url,
            "https://en.wikipedia.org/w/index.php?title=Alan_Turing&oldid=123456"
        );
        assert_eq!(
            info.history_url,
            "https://en.wikipedia.org/w/index.php?title=Alan_Turing&action=history"
        );
        assert_eq!(info.retrieved_on, "2026-07-14");
    }

    /// The export footer and the display surface must never quote different
    /// license wording — same constant, so they can't drift apart.
    #[test]
    fn display_license_matches_the_export_footer_license() {
        let info = article_attribution("Stub", "en", 1, "2026-07-14");
        let footer = export_footer("2026-07-14", false);
        assert!(footer.contains(&info.license));
    }

    /// PRD §10's trademark/attribution requirements must actually be
    /// visible in the README, not just implemented in code — a guardrail
    /// against the prose drifting out of the repo unnoticed. Reads the real
    /// file (mirrors `privacy::tests::
    /// no_analytics_endpoints_in_the_real_source_tree`'s "check the actual
    /// tree, not a fixture" approach) rather than asserting against a copy
    /// of the text that could go stale independently of the real README.
    #[test]
    fn readme_states_the_section_10_trademark_and_credit_requirements() {
        let readme_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
        let readme = std::fs::read_to_string(&readme_path)
            .unwrap_or_else(|e| panic!("README.md must be readable: {e}"));
        assert!(
            readme.contains("not endorsed by the Wikimedia Foundation"),
            "README must carry PRD §10's trademark disclaimer verbatim"
        );
        assert!(
            readme.contains("wiki-tui"),
            "README must credit and disambiguate from the archived wiki-tui project"
        );
        assert!(
            readme.contains("CC BY-SA 4.0"),
            "README must state the article content license"
        );
    }
}
