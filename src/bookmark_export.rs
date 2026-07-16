//! `:bookmarks export md|html|json|netscape [path]` (PRD FR-BM-4): four
//! export formats over the saved bookmarks, every one grouped by tag
//! (untagged last) and every one ending in the §10 attribution footer —
//! title/URL/note/saved-date per entry, license + retrieval-date note once
//! per file, exactly as the saved-citation exports in `cite.rs` already do
//! for the research bibliography. `netscape` is the classic
//! `NETSCAPE-Bookmark-file-1` shape every browser's "import bookmarks"
//! understands, with one folder per tag — a bookmark carrying two tags
//! therefore appears once under *each* tag's `<DL>`, which is how that
//! format has always represented multi-membership (there's no such thing as
//! a bookmark existing in two folders "by reference" in this format; it's
//! duplicated by design, and every browser's own exporter does the same).

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::bookmarks::{Bookmark, display_date};

pub const FORMATS: [&str; 4] = ["md", "html", "json", "netscape"];

/// The file extension a format conventionally uses — `netscape`'s is
/// `.html` too, since that's the file extension every browser's own
/// "import bookmarks" file picker expects.
pub fn extension(format: &str) -> Option<&'static str> {
    match format {
        "md" => Some("md"),
        "html" => Some("html"),
        "json" => Some("json"),
        "netscape" => Some("html"),
        _ => None,
    }
}

/// Renders `bookmarks` in the named format, or `None` for an unrecognized
/// one (the `:bookmarks export` command already validates this at parse
/// time — see `command.rs` — so this is a defensive second check, not the
/// primary error path).
pub fn render(bookmarks: &[Bookmark], format: &str) -> Option<String> {
    match format {
        "md" => Some(render_markdown(bookmarks)),
        "html" => Some(render_html(bookmarks)),
        "json" => Some(render_json(bookmarks)),
        "netscape" => Some(render_netscape(bookmarks)),
        _ => None,
    }
}

/// `$XDG_DATA_HOME/wikitui/exports/` (PRD §6.4): the default export
/// directory when `:bookmarks export` isn't given an explicit path. Falls
/// back to the system temp directory on a platform with no resolvable data
/// dir — degrading gracefully rather than refusing to export at all.
pub fn default_export_dir() -> PathBuf {
    crate::paths::wikitui_data_dir()
        .map(|d| d.join("exports"))
        .unwrap_or_else(std::env::temp_dir)
}

/// The default timestamped path for a given format — `bookmarks-
/// <YYYYMMDD-HHMMSS>.<ext>` under [`default_export_dir`]. `None` for an
/// unrecognized format.
pub fn default_export_path(format: &str) -> Option<PathBuf> {
    let ext = extension(format)?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    Some(default_export_dir().join(format!("bookmarks-{ts}.{ext}")))
}

/// Groups bookmarks by tag (a bookmark with N tags appears in N groups, tag
/// names sorted for stable output), returning untagged bookmarks
/// separately so callers can place that section last (PRD FR-BM-4: "grouped
/// by tag (untagged last)").
fn group_by_tag(bookmarks: &[Bookmark]) -> (Vec<(String, Vec<&Bookmark>)>, Vec<&Bookmark>) {
    let mut by_tag: BTreeMap<String, Vec<&Bookmark>> = BTreeMap::new();
    let mut untagged = Vec::new();
    for b in bookmarks {
        if b.tags.is_empty() {
            untagged.push(b);
        } else {
            for tag in &b.tags {
                by_tag.entry(tag.clone()).or_default().push(b);
            }
        }
    }
    (by_tag.into_iter().collect(), untagged)
}

/// PRD §10's export attribution footer, shared by every format here — and,
/// via `crate::attribution`, with the saved-page export (PRD FR-OFF-7) so the
/// wording lives in exactly one place. `include_annotation_note = true`: a
/// bookmark carries the reader's own note/tags, which ShareAlike does not
/// govern (they annotate the article, they aren't it), so the footer says so.
fn attribution_note() -> String {
    crate::attribution::export_footer(&crate::research::today(), true)
}

/// The revision permalink for a bookmark, when its revid is known — unlike
/// `cite.rs`'s citations (which never fetch revision metadata), a bookmark
/// records `revid_at_bookmark` directly, so a real oldid permalink (not
/// just the live-article URL) is available here.
fn permalink(b: &Bookmark) -> Option<String> {
    b.revid_at_bookmark.map(|revid| {
        format!(
            "https://{}.wikipedia.org/w/index.php?title={}&oldid={revid}",
            b.lang,
            b.title.replace(' ', "_")
        )
    })
}

fn canonical_url(b: &Bookmark) -> String {
    crate::research::article_url(&b.title, &b.lang)
}

fn note_first_line(b: &Bookmark) -> Option<&str> {
    b.note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| n.lines().next().unwrap_or(n))
}

// ---- Markdown -----------------------------------------------------------

fn render_markdown(bookmarks: &[Bookmark]) -> String {
    let mut out = format!("# Bookmarks (exported {})\n", crate::research::today());
    if bookmarks.is_empty() {
        out.push_str("\n(no saved bookmarks)\n");
        return out;
    }

    let (by_tag, untagged) = group_by_tag(bookmarks);
    let md_entry = |b: &Bookmark| -> String {
        let title = crate::cite::escape_markdown(&b.title);
        let url = canonical_url(b);
        let date = display_date(&b.created_at);
        let mut line = format!("- [{title}]({url})");
        if let Some(note) = note_first_line(b) {
            line.push_str(&format!(" — {note}"));
        }
        line.push_str(&format!(" (saved {date}"));
        if let Some(link) = permalink(b) {
            line.push_str(&format!(", [permalink]({link})"));
        }
        line.push(')');
        line
    };

    for (tag, entries) in &by_tag {
        out.push_str(&format!("\n## #{tag}\n\n"));
        for b in entries {
            out.push_str(&md_entry(b));
            out.push('\n');
        }
    }
    if !untagged.is_empty() {
        out.push_str("\n## Untagged\n\n");
        for b in &untagged {
            out.push_str(&md_entry(b));
            out.push('\n');
        }
    }

    out.push_str("\n---\n\n*");
    out.push_str(&attribution_note());
    out.push_str("*\n");
    out
}

// ---- JSON -----------------------------------------------------------------

/// The saved records verbatim, pretty-printed — plus the attribution note
/// as a sibling field, so even the machine-readable export carries it (PRD
/// FR-BM-4: "EVERY export ends with the §10 attribution footer").
fn render_json(bookmarks: &[Bookmark]) -> String {
    #[derive(serde::Serialize)]
    struct Export<'a> {
        bookmarks: &'a [Bookmark],
        attribution: String,
    }
    let export = Export {
        bookmarks,
        attribution: attribution_note(),
    };
    serde_json::to_string_pretty(&export).unwrap_or_else(|_| "{}".to_string())
}

// ---- HTML (semantic, not Netscape) ---------------------------------------

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn render_html(bookmarks: &[Bookmark]) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
    out.push_str("<meta charset=\"utf-8\">\n<title>Bookmarks</title>\n</head>\n<body>\n");
    out.push_str(&format!(
        "<h1>Bookmarks</h1>\n<p>Exported {}.</p>\n",
        crate::research::today()
    ));

    if bookmarks.is_empty() {
        out.push_str("<p>(no saved bookmarks)</p>\n");
    } else {
        let (by_tag, untagged) = group_by_tag(bookmarks);
        let mut html_section = |heading: &str, entries: &[&Bookmark]| {
            out.push_str(&format!(
                "<section>\n<h2>{}</h2>\n<ul>\n",
                escape_html(heading)
            ));
            for b in entries {
                let url = escape_html(&canonical_url(b));
                let title = escape_html(&b.title);
                let date = display_date(&b.created_at);
                out.push_str("<li>");
                out.push_str(&format!("<a href=\"{url}\">{title}</a>"));
                if let Some(note) = note_first_line(b) {
                    out.push_str(&format!(" — {}", escape_html(note)));
                }
                out.push_str(&format!(" (saved {date}"));
                if let Some(link) = permalink(b) {
                    out.push_str(&format!(
                        ", <a href=\"{}\">permalink</a>",
                        escape_html(&link)
                    ));
                }
                out.push_str(")</li>\n");
            }
            out.push_str("</ul>\n</section>\n");
        };
        for (tag, entries) in &by_tag {
            html_section(&format!("#{tag}"), entries);
        }
        if !untagged.is_empty() {
            html_section("Untagged", &untagged);
        }
    }

    out.push_str(&format!(
        "<footer>{}</footer>\n</body>\n</html>\n",
        escape_html(&attribution_note())
    ));
    out
}

// ---- Netscape bookmark file -----------------------------------------------

/// `ADD_DATE` is Unix-epoch seconds in this format; falls back to `0` for a
/// timestamp that somehow doesn't parse (hand-edited file) rather than
/// failing the whole export over one bad date.
fn epoch_seconds(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

fn render_netscape(bookmarks: &[Bookmark]) -> String {
    let mut out = String::new();
    out.push_str("<!DOCTYPE NETSCAPE-Bookmark-file-1>\n");
    out.push_str("<!-- This is an automatically generated file. It will be read and overwritten. DO NOT EDIT! -->\n");
    out.push_str(&format!("<!-- {} -->\n", attribution_note()));
    out.push_str("<META HTTP-EQUIV=\"Content-Type\" CONTENT=\"text/html; charset=UTF-8\">\n");
    out.push_str("<TITLE>Bookmarks</TITLE>\n<H1>Bookmarks</H1>\n<DL><p>\n");

    let netscape_entry = |b: &Bookmark| -> String {
        format!(
            "    <DT><A HREF=\"{}\" ADD_DATE=\"{}\">{}</A>\n",
            escape_html(&canonical_url(b)),
            epoch_seconds(&b.created_at),
            escape_html(&b.title)
        )
    };

    let (by_tag, untagged) = group_by_tag(bookmarks);
    for (tag, entries) in &by_tag {
        out.push_str(&format!(
            "    <DT><H3>{}</H3>\n    <DL><p>\n",
            escape_html(tag)
        ));
        for b in entries {
            out.push_str("    ");
            out.push_str(&netscape_entry(b));
        }
        out.push_str("    </DL><p>\n");
    }
    for b in &untagged {
        out.push_str(&netscape_entry(b));
    }

    out.push_str("</DL><p>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bookmarks::now_ts;

    fn bookmark(title: &str, tags: &[&str], note: Option<&str>) -> Bookmark {
        Bookmark {
            title: title.to_string(),
            lang: "en".to_string(),
            wiki: String::new(),
            revid_at_bookmark: Some(42),
            section_anchor: None,
            created_at: "2026-07-13T10:00:00-07:00".to_string(),
            updated_at: now_ts(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            note: note.map(str::to_string),
        }
    }

    #[test]
    fn markdown_groups_by_tag_with_untagged_last_and_has_attribution() {
        let bookmarks = vec![
            bookmark("Enigma", &["crypto"], Some("great writeup")),
            bookmark("Random Article", &[], None),
        ];
        let md = render_markdown(&bookmarks);

        let crypto_pos = md.find("## #crypto").unwrap();
        let untagged_pos = md.find("## Untagged").unwrap();
        assert!(crypto_pos < untagged_pos, "untagged section must come last");
        assert!(md.contains("[Enigma](https://en.wikipedia.org/wiki/Enigma)"));
        assert!(md.contains("great writeup"));
        assert!(
            md.contains("CC BY-SA 4.0"),
            "attribution footer must be present"
        );
    }

    #[test]
    fn markdown_entry_with_no_note_omits_the_dash() {
        let mut b = bookmark("Plain", &[], None);
        b.revid_at_bookmark = None; // isolate this assertion from the permalink suffix
        let md = render_markdown(&[b]);
        assert!(md.contains("[Plain](https://en.wikipedia.org/wiki/Plain) (saved 2026-07-13)"));
        // The footer legitimately mentions "revision permalink" — check the
        // entry line itself carries no permalink suffix, not the whole doc.
        let entry_line = md.lines().find(|l| l.contains("[Plain]")).unwrap();
        assert!(!entry_line.contains("permalink"));
    }

    #[test]
    fn markdown_and_html_include_the_revision_permalink_when_known() {
        let bookmarks = vec![bookmark("Alan Turing", &[], None)];
        let md = render_markdown(&bookmarks);
        assert!(md.contains(
            "[permalink](https://en.wikipedia.org/w/index.php?title=Alan_Turing&oldid=42)"
        ));
        let html = render_html(&bookmarks);
        assert!(html.contains(
            "href=\"https://en.wikipedia.org/w/index.php?title=Alan_Turing&amp;oldid=42\""
        ));
    }

    #[test]
    fn a_bookmark_with_two_tags_appears_in_both_tag_groups() {
        let bookmarks = vec![bookmark("Enigma", &["crypto", "ww2"], None)];
        let md = render_markdown(&bookmarks);
        let crypto_section = md.split("## #crypto").nth(1).unwrap();
        let ww2_section = md.split("## #ww2").nth(1).unwrap();
        assert!(crypto_section.contains("Enigma"));
        assert!(ww2_section.contains("Enigma"));
    }

    #[test]
    fn empty_bookmarks_says_so_in_every_format() {
        assert!(render_markdown(&[]).contains("no saved bookmarks"));
        assert!(render_html(&[]).contains("no saved bookmarks"));
    }

    #[test]
    fn json_export_is_valid_pretty_json_with_attribution_and_verbatim_records() {
        let bookmarks = vec![bookmark("Enigma", &["crypto"], Some("note"))];
        let json = render_json(&bookmarks);
        assert!(json.contains("\n"), "pretty-printed, not a single line");

        let value: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");
        assert_eq!(value["bookmarks"][0]["title"], "Enigma");
        assert_eq!(value["bookmarks"][0]["tags"][0], "crypto");
        assert!(
            value["attribution"]
                .as_str()
                .unwrap()
                .contains("CC BY-SA 4.0")
        );
    }

    #[test]
    fn html_export_is_semantic_and_escapes_content() {
        let bookmarks = vec![bookmark("A & B <script>", &["x"], None)];
        let html = render_html(&bookmarks);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<footer>"));
        assert!(html.contains("CC BY-SA 4.0"));
        assert!(
            html.contains("A &amp; B &lt;script&gt;"),
            "hostile/special characters in a title must be escaped: {html}"
        );
        assert!(
            !html.contains("<script>A"),
            "must never emit an unescaped tag from content"
        );
    }

    #[test]
    fn netscape_export_has_the_classic_doctype_and_folders_per_tag() {
        let bookmarks = vec![
            bookmark("Enigma", &["crypto", "ww2"], None),
            bookmark("Plain", &[], None),
        ];
        let netscape = render_netscape(&bookmarks);
        assert!(netscape.starts_with("<!DOCTYPE NETSCAPE-Bookmark-file-1>"));
        assert!(netscape.contains("<DT><H3>crypto</H3>"));
        assert!(netscape.contains("<DT><H3>ww2</H3>"));
        assert!(netscape.contains("ADD_DATE="));
        assert!(
            netscape.contains("CC BY-SA 4.0"),
            "attribution present as a comment"
        );

        // The two-tag bookmark appears once per tag folder (documented
        // Netscape-format behavior — no cross-folder "by reference").
        assert_eq!(
            netscape
                .matches("HREF=\"https://en.wikipedia.org/wiki/Enigma\"")
                .count(),
            2
        );
        // The untagged bookmark sits outside any folder, directly under
        // the root <DL>.
        assert!(netscape.contains("Plain"));
    }

    #[test]
    fn epoch_seconds_falls_back_to_zero_on_a_bad_timestamp() {
        assert_eq!(epoch_seconds("not-a-date"), 0);
        assert!(epoch_seconds("2026-07-13T10:00:00-07:00") > 0);
    }

    #[test]
    fn extension_and_default_path_agree_on_known_formats() {
        for format in FORMATS {
            let ext = extension(format).unwrap();
            let path = default_export_path(format).unwrap();
            assert!(path.to_string_lossy().ends_with(&format!(".{ext}")));
        }
        assert_eq!(extension("bogus"), None);
        assert_eq!(default_export_path("bogus"), None);
        assert_eq!(render(&[], "bogus"), None);
    }

    #[test]
    fn permalink_uses_the_recorded_revid_when_known() {
        let with_revid = bookmark("Alan Turing", &[], None);
        assert_eq!(
            permalink(&with_revid).as_deref(),
            Some("https://en.wikipedia.org/w/index.php?title=Alan_Turing&oldid=42")
        );
        let mut without_revid = bookmark("Alan Turing", &[], None);
        without_revid.revid_at_bookmark = None;
        assert_eq!(permalink(&without_revid), None);
    }
}
