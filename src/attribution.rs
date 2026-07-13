//! The shared §10 export-attribution footer. PRD §10 requires *every* export
//! (bookmarks, saved pages, trail) to embed the same footer: Wikipedia's
//! content license, the note that a revision permalink carries the author
//! attribution Wikimedia's reuse terms require, and the retrieval date, plus a
//! statement that wikitui is an unofficial client. `bookmark_export.rs` grew
//! this text first; the saved-page export (PRD FR-OFF-7) needs the identical
//! footer, so rather than a second verbatim copy the wording lives here once
//! and both call it — the DRY the PRD's "every export" rule invites.
//!
//! The one variation is the reader-annotation clause: a bookmark export
//! carries the reader's own notes/tags, which ShareAlike does *not* govern
//! (they annotate the article, they aren't the article), so that export adds
//! a sentence saying so. A saved-page export carries no user annotations, so
//! it omits that clause. `include_annotation_note` is the switch.

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
}
